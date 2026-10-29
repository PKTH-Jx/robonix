//! Input discovery and acquisition scheduling for SRDL rules.

use futures::{Stream, StreamExt, stream};
use robonix_atlas::{client::AtlasClient, pb};
use serde_json::Value;
use std::sync::Arc;
use tokio::{
    sync::{Semaphore, oneshot},
    time::Instant,
};

use crate::srdl::ast::{InputClause, InputField, InputType, Rule};

const MAX_CONCURRENT_BINDINGS: usize = 16;

mod select;
pub(super) mod transport;

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
    /// Shared by all rule streams and clones of this Adapter.
    bindings: Arc<Semaphore>,
}

impl InputAdapter {
    pub fn new(atlas: AtlasClient) -> Self {
        Self {
            atlas,
            bindings: Arc::new(Semaphore::new(MAX_CONCURRENT_BINDINGS)),
        }
    }

    /// Acquire each input in one binding-and-call task, yielding completion order.
    ///
    /// Dropping the stream cancels pending acquisition. Workers retain their shared
    /// concurrency permits until Atlas channel cleanup completes.
    /// Results contain type-checked selected values; maxage is not evaluated here yet.
    pub fn acquire<'ast>(
        &'ast self,
        rule: &'ast Rule,
        consumer_id: &'ast str,
    ) -> impl Stream<Item = Result<AcquiredInput<'ast>, InputError>> + 'ast {
        stream::iter(&rule.inputs)
            .map(move |input| async move {
                let (send, receive) = oneshot::channel();
                let adapter = self.clone();
                let capability = input.capability.clone();
                let consumer_id = consumer_id.to_owned();
                // A worker can finish asynchronous cleanup after its receiver is dropped.
                tokio::spawn(async move {
                    adapter.acquire_one(&capability, &consumer_id, send).await;
                });
                let result = match receive.await {
                    Ok(result) => result,
                    Err(error) => Err(InputErrorKind::WorkerStopped(error)),
                };
                let error = |alias, kind| InputError {
                    rule: rule.name.0.clone(),
                    input: alias,
                    capability: input.capability.clone(),
                    kind,
                };
                let response = result.map_err(|kind| {
                    error(
                        input
                            .fields
                            .iter()
                            .map(|field| field.name.0.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        kind,
                    )
                })?;
                let fields = input
                    .fields
                    .iter()
                    .map(|field| {
                        select_field(&response.value, field)
                            .map_err(|kind| error(field.name.0.clone(), kind))
                    })
                    .collect::<Result<Vec<_>, InputError>>()?;
                Ok(AcquiredInput {
                    input,
                    fields,
                    acquired_at: response.acquired_at,
                })
            })
            .buffer_unordered(MAX_CONCURRENT_BINDINGS)
    }

    /// Own the concurrency permit through discovery, invocation and asynchronous cleanup.
    /// Closing the receiver cancels discovery or signals transport cancellation;
    /// the worker remains alive until any established channel has been released or cleanup fails.
    async fn acquire_one(
        &self,
        capability: &str,
        consumer_id: &str,
        mut send: oneshot::Sender<Result<transport::Response, InputErrorKind>>,
    ) {
        let _permit = tokio::select! {
            _ = send.closed() => return,
            permit = self.bindings.acquire() => permit.expect("the private binding semaphore is never closed"),
        };
        let provider = tokio::select! {
            _ = send.closed() => return,
            result = self.discover(capability) => result,
        };
        let provider = match provider {
            Ok(provider) => provider,
            Err(error) => {
                let _ = send.send(Err(error));
                return;
            }
        };
        if let Some(result) = transport::acquire(
            &self.atlas,
            &provider,
            capability,
            consumer_id,
            send.closed(),
        )
        .await
        {
            let _ = send.send(result.map_err(InputErrorKind::Acquisition));
        }
    }

    /// Require one active provider across all transports, without caching the result.
    /// This does not acquire a permit because acquisition workers already hold one.
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

fn select_field<'ast>(
    response: &Value,
    field: &'ast InputField,
) -> Result<SelectedField<'ast>, InputErrorKind> {
    let value = select::select(response, &field.select).map_err(InputErrorKind::Selection)?;
    let selected = match (field.data_type, value) {
        (InputType::Boolean, Value::Bool(boolean)) => Some(InputValue::Boolean(*boolean)),
        (InputType::Number, Value::Number(number)) => number.as_f64().map(InputValue::Number),
        (InputType::String, Value::String(text)) => Some(InputValue::String(text.clone())),
        _ => None,
    }
    .ok_or_else(|| InputErrorKind::TypeMismatch {
        expected: field.data_type,
        actual: match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        },
    })?;
    Ok(SelectedField {
        field,
        value: selected,
    })
}

#[cfg(test)]
mod tests;
