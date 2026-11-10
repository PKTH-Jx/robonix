//! Input discovery and acquisition scheduling for SRDL rules.

mod context;
mod dependencies;
mod scheduler;
mod select;
pub(super) mod transport;

pub use context::Error as ContextError;
pub(super) use scheduler::Deadline;

use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use futures::{FutureExt, Stream, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use robonix_atlas::{client::AtlasClient, pb};
use robonix_scribe::Level;
use serde_json::Value;
use tokio::{
    sync::{oneshot, watch},
    time::{Instant, sleep_until},
};

use self::scheduler::{Request, Scheduler};
use super::{evaluator::Error, logging};
use crate::srdl::ast::{InputClause, InputField, InputType, Rule};

const MAX_CONCURRENT_BINDINGS: usize = 16;

/// A basic SRDL value shared by acquired inputs and expression results.
#[derive(Debug, Clone, PartialEq)]
pub enum InputValue {
    /// A boolean.
    Boolean(bool),
    /// A number.
    Number(f64),
    /// A string.
    String(String),
}

impl InputValue {
    /// Decode a basic value without coercion; errors identify the actual JSON type.
    pub(super) fn from_json(value: &Value, expected: InputType) -> Result<Self, &'static str> {
        let selected = match (expected, value) {
            (InputType::Boolean, Value::Bool(boolean)) => Some(Self::Boolean(*boolean)),
            (InputType::Number, Value::Number(number)) => number
                .as_f64()
                .filter(|value| value.is_finite())
                .map(Self::Number),
            (InputType::String, Value::String(text)) => Some(Self::String(text.clone())),
            _ => None,
        };
        selected.ok_or(match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        })
    }

    pub fn data_type(&self) -> InputType {
        match self {
            Self::Boolean(_) => InputType::Boolean,
            Self::Number(_) => InputType::Number,
            Self::String(_) => InputType::String,
        }
    }
}

/// Type-checked fields from one capability response sharing its acquisition time.
#[derive(Debug)]
pub struct AcquiredInput<'ast> {
    /// The declaration associated with this response.
    pub input: &'ast InputClause,
    /// Values selected independently from the same response.
    pub fields: Vec<SelectedField<'ast>>,
    /// Monotonic time immediately after receiving the capability response.
    pub acquired_at: Instant,
}

/// One selected value paired with its field-level type and freshness requirements.
#[derive(Debug)]
pub struct SelectedField<'ast> {
    /// The field declaration referenced by requirement expressions.
    pub field: &'ast InputField,
    /// The selected input value.
    pub value: InputValue,
}

impl<'ast> SelectedField<'ast> {
    fn select(response: &Value, field: &'ast InputField) -> Result<Self, InputErrorKind> {
        let value = select::select(response, &field.select).map_err(InputErrorKind::Selection)?;
        let value = InputValue::from_json(value, field.data_type).map_err(|actual| {
            InputErrorKind::TypeMismatch {
                expected: field.data_type,
                actual,
            }
        })?;
        Ok(Self { field, value })
    }
}

/// Freshly acquired bindings retained while a complete check waits in FIFO order.
pub(in crate::interpreter) struct Prepared {
    /// All values used by this evaluation, fixed until it finishes.
    pub(in crate::interpreter) values: BTreeMap<String, Arc<InputValue>>,
    /// Input deadlines checked again after queueing and expression calculation.
    pub(in crate::interpreter) deadline: Deadline,
}

/// A discovery, acquisition, selection or type failure associated with one rule input.
#[derive(Debug, thiserror::Error)]
#[error("rule '{rule}', input '{input}' from '{capability}': {kind}")]
pub struct InputError {
    /// The rule containing the input that could not be acquired.
    pub rule: String,
    /// The failed field alias, or all aliases in a group whose acquisition failed.
    pub input: String,
    /// The requested capability contract ID.
    pub capability: String,
    /// The discovery, acquisition, cleanup, selection or type failure.
    #[source]
    pub kind: InputErrorKind,
}

/// Failures encountered while acquiring and validating an input.
#[derive(Debug, thiserror::Error)]
pub enum InputErrorKind {
    #[error("input capability contract ID must not be empty")]
    EmptyCapability,
    #[error("Atlas query failed: {0}")]
    AtlasQuery(#[source] anyhow::Error),
    #[error("no active provider offers this capability")]
    NoActiveProvider,
    #[error("multiple active providers offer this capability: {0:?}")]
    MultipleActiveProviders(Vec<String>),
    #[error(transparent)]
    Acquisition(#[from] transport::Error),
    #[error(transparent)]
    Selection(#[from] select::SelectError),
    #[error("input requires {expected:?}, found {actual}")]
    TypeMismatch {
        /// The basic type declared for this field.
        expected: InputType,
        /// The JSON type of the selected response value.
        actual: &'static str,
    },
    #[error("input acquisition worker stopped: {0}")]
    WorkerStopped(#[source] oneshot::error::RecvError),
}

/// Binds and acquires inputs using shared Atlas connectivity and a concurrency limit.
#[derive(Clone)]
pub struct InputAdapter {
    /// Clones share the underlying Atlas connection without serializing queries.
    atlas: AtlasClient,
    /// Shared queue metadata; locks never cover discovery or transport awaits.
    scheduling: Arc<Mutex<Scheduler>>,
}

/// Selected responses for one rule attempt; dropping it cancels queued and active inputs.
struct Inputs<'ast> {
    /// Receivers select fields against the immutable AST after acquisition completes.
    responses: FuturesUnordered<BoxFuture<'ast, Result<AcquiredInput<'ast>, InputError>>>,
    /// Adapter whose queue owns this attempt's remaining work.
    adapter: InputAdapter,
    /// Registration sequence, independent of invocation ID reuse and retries.
    id: u64,
    /// Receipt notifications share the scheduler's deadline with snapshot assembly.
    deadline: watch::Receiver<Deadline>,
}

impl<'ast> Inputs<'ast> {
    async fn receive(
        rule: &'ast Rule,
        input: &'ast InputClause,
        fields: Vec<&'ast InputField>,
        receive: oneshot::Receiver<Result<transport::Response, InputErrorKind>>,
    ) -> Result<AcquiredInput<'ast>, InputError> {
        let error = |alias, kind| InputError {
            rule: rule.name.0.clone(),
            input: alias,
            capability: input.capability.clone(),
            kind,
        };
        let response = receive
            .await
            .unwrap_or_else(|error| Err(InputErrorKind::WorkerStopped(error)))
            .map_err(|kind| {
                error(
                    fields
                        .iter()
                        .map(|field| field.name.0.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    kind,
                )
            })?;
        let fields = fields
            .into_iter()
            .map(|field| {
                SelectedField::select(&response.value, field)
                    .map_err(|kind| error(field.name.0.clone(), kind))
            })
            .collect::<Result<Vec<_>, InputError>>()?;
        Ok(AcquiredInput {
            input,
            fields,
            acquired_at: response.acquired_at,
        })
    }
}

impl<'ast> Stream for Inputs<'ast> {
    type Item = Result<AcquiredInput<'ast>, InputError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.responses).poll_next(cx)
    }
}

impl Drop for Inputs<'_> {
    fn drop(&mut self) {
        self.adapter.scheduling.lock().unwrap().cancel(self.id);
        // Close active receivers before releasing slots to other rules.
        self.responses.clear();
        self.adapter.dispatch();
    }
}

impl InputAdapter {
    /// Create a new adapter with shared Atlas connectivity and a concurrency limit.
    pub fn new(atlas: AtlasClient) -> Self {
        Self {
            atlas,
            scheduling: Arc::new(Mutex::new(Scheduler::default())),
        }
    }

    /// Return one complete dataset per rule, with only the active conditions' bindings.
    /// Register peers together before any fetch can occupy their shared slots.
    pub(super) fn acquire_group<'ast>(
        &self,
        rules: impl IntoIterator<Item = &'ast Rule>,
        consumer: &str,
        context: Arc<Value>,
        check_id: u64,
        deadline: Instant,
    ) -> Vec<BoxFuture<'ast, Result<Prepared, Error>>> {
        let selected: Vec<_> = rules
            .into_iter()
            .map(|rule| (rule, dependencies::References::collect(rule)))
            .collect();
        let inputs = self.fetch_group(
            selected.iter().map(|(rule, names)| (*rule, names)),
            consumer,
            deadline,
        );
        selected
            .into_iter()
            .zip(inputs)
            .map(|((rule, names), inputs)| {
                let context = context.clone();
                async move {
                    let deadlines = inputs.deadline.clone();
                    Self::prepare(check_id, &context, rule, Some(&names), deadlines, inputs).await
                }
                .boxed()
            })
            .collect()
    }

    /// Acquire a complete snapshot without evaluating predicates as individual inputs arrive.
    async fn prepare<'ast>(
        check_id: u64,
        context: &Value,
        rule: &'ast Rule,
        names: Option<&BTreeSet<&str>>,
        mut deadlines: watch::Receiver<Deadline>,
        inputs: impl Stream<Item = Result<AcquiredInput<'ast>, InputError>>,
    ) -> Result<Prepared, Error> {
        let mut values = context::bind(rule, context, names)?;
        for (name, value) in &values {
            logging::log(
                Level::Info,
                format!(
                    "[sentinel] check_id={check_id} rule={:?} context={name:?} value={:?}",
                    rule.name.0,
                    logging::value(value)
                ),
            );
        }
        let mut expected = values.len();
        for field in rule
            .inputs
            .iter()
            .flat_map(|input| &input.fields)
            .filter(|field| names.is_none_or(|names| names.contains(field.name.0.as_str())))
        {
            if let Some(age) = field.maxage {
                let valid = age > 0.0
                    && Duration::try_from_secs_f64(age)
                        .ok()
                        .and_then(|age| age.checked_add(Duration::from_nanos(1)))
                        .and_then(|age| Instant::now().checked_add(age))
                        .is_some();
                if !valid {
                    return Err(Error::InvalidMaxage {
                        rule: rule.name.0.clone(),
                        input: field.name.0.clone(),
                    });
                }
            }
            expected += 1;
        }
        tokio::pin!(inputs);
        loop {
            let deadline = deadlines.borrow_and_update().clone();
            deadline.check(rule, Instant::now())?;
            if values.len() == expected {
                return Ok(Prepared { values, deadline });
            }
            let item = tokio::select! {
                item = inputs.next() => item,
                Ok(()) = deadlines.changed() => continue,
                _ = sleep_until(deadline.at) => continue,
            };
            // Input arrival and deadline expiry may wake the task in the same poll.
            deadlines.borrow().check(rule, Instant::now())?;
            let acquired = item.ok_or_else(|| Error::IncompleteInputs {
                rule: rule.name.0.clone(),
            })??;
            for selected in acquired.fields {
                let name = selected.field.name.0.as_str();
                logging::log(
                    Level::Info,
                    format!(
                        "[sentinel] check_id={check_id} rule={:?} input={name:?} source={:?} value={:?} age_s={:?} maxage_s={:?}",
                        rule.name.0,
                        acquired.input.capability,
                        logging::value(&selected.value),
                        Instant::now()
                            .checked_duration_since(acquired.acquired_at)
                            .map(|age| age.as_secs_f64()),
                        selected.field.maxage
                    ),
                );
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    values.entry(name.to_owned())
                {
                    if acquired.acquired_at > Instant::now() {
                        return Err(Error::FutureTimestamp {
                            rule: rule.name.0.clone(),
                            input: name.into(),
                        });
                    }
                    entry.insert(Arc::new(selected.value));
                }
            }
        }
    }

    /// Each input clause shares one response among its selected fields.
    fn fetch_group<'ast, 'names>(
        &self,
        rules: impl IntoIterator<Item = (&'ast Rule, &'names BTreeSet<&'ast str>)>,
        consumer: &str,
        deadline: Instant,
    ) -> Vec<Inputs<'ast>>
    where
        'ast: 'names,
    {
        let mut streams = Vec::new();
        let mut scheduling = self.scheduling.lock().unwrap();
        for (rule, names) in rules {
            let mut requests = Vec::new();
            let responses = FuturesUnordered::new();
            for input in &rule.inputs {
                let fields: Vec<_> = input
                    .fields
                    .iter()
                    .filter(|field| names.contains(field.name.0.as_str()))
                    .collect();
                if fields.is_empty() {
                    continue;
                }
                let bounded = fields
                    .iter()
                    .filter_map(|field| {
                        Some((
                            Duration::try_from_secs_f64(field.maxage?).ok()?,
                            field.name.0.as_str(),
                        ))
                    })
                    .min();
                let (reply, receive) = oneshot::channel();
                requests.push(Request {
                    capability: input.capability.clone(),
                    consumer: consumer.to_owned(),
                    reply,
                    maxage: bounded.map(|(age, _)| age),
                    input: Arc::from(bounded.map_or(fields[0].name.0.as_str(), |(_, name)| name)),
                });
                responses.push(Inputs::receive(rule, input, fields, receive).boxed());
            }
            let (id, deadline) = scheduling.insert(requests, deadline);
            streams.push(Inputs {
                responses,
                adapter: self.clone(),
                id,
                deadline,
            });
        }
        drop(scheduling);
        self.dispatch();
        streams
    }

    /// Only queue bookkeeping is serialized; a worker owns its slot through cleanup.
    fn dispatch(&self) {
        let mut scheduling = self.scheduling.lock().unwrap();
        while scheduling.active < MAX_CONCURRENT_BINDINGS {
            let Some((id, request)) = scheduling.next(Instant::now()) else {
                break;
            };
            let adapter = self.clone();
            tokio::spawn(adapter.acquire_one(id, request));
        }
    }

    /// Own the concurrency slot through discovery, invocation and asynchronous cleanup.
    /// Closing the receiver cancels discovery or signals transport cancellation;
    /// the worker remains alive until any established channel has been released or cleanup fails.
    fn acquire_one(self, id: u64, mut request: Request) -> BoxFuture<'static, ()> {
        async move {
            let result = async {
                let provider = tokio::select! {
                    biased;
                    _ = request.reply.closed() => return None,
                    result = self.discover(&request.capability) => result,
                };
                let provider = match provider {
                    Ok(provider) => provider,
                    Err(error) => return Some(Err(error)),
                };
                transport::acquire(
                    &self.atlas,
                    &provider,
                    &request.capability,
                    &request.consumer,
                    request.reply.closed(),
                    |acquired_at| {
                        self.scheduling.lock().unwrap().received(
                            id,
                            acquired_at,
                            request.maxage,
                            request.input.clone(),
                        );
                    },
                )
                .await
                .map(|result| result.map_err(InputErrorKind::Acquisition))
            }
            .await;
            self.scheduling.lock().unwrap().complete();
            if let Some(result) = result {
                let _ = request.reply.send(result);
            }
            self.dispatch();
        }
        .boxed()
    }

    /// Require one active provider across all transports, without caching the result.
    /// The caller already holds an acquisition slot.
    async fn discover(&self, capability: &str) -> Result<pb::CapabilityProvider, InputErrorKind> {
        // An empty Atlas contract filter queries all capabilities instead of one source.
        if capability.is_empty() {
            return Err(InputErrorKind::EmptyCapability);
        }
        let mut atlas = self.atlas.clone();
        // Query preserves lifecycle state; flatten_capabilities would discard it.
        let providers = atlas
            .query_capabilities("", capability, pb::Transport::Unspecified)
            .await
            .map_err(InputErrorKind::AtlasQuery)?;
        let mut active: Vec<_> = providers
            .into_iter()
            .filter(|provider| provider.state() == pb::LifecycleState::StateActive)
            .collect();
        match active.len() {
            0 => Err(InputErrorKind::NoActiveProvider),
            1 => Ok(active.pop().unwrap()),
            _ => {
                let mut ids: Vec<_> = active.into_iter().map(|provider| provider.id).collect();
                ids.sort();
                Err(InputErrorKind::MultipleActiveProviders(ids))
            }
        }
    }
}

#[cfg(test)]
mod tests;
