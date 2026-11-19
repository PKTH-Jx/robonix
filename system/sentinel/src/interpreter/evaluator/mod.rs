//! Shared rule-check submission and atomic SRDL condition evaluation.

pub mod expression;

pub use super::adapter::ContextError;

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use futures::{
    FutureExt, StreamExt,
    future::{AbortHandle, Abortable, BoxFuture},
    stream::FuturesUnordered,
};
use robonix_scribe::Level;
use serde_json::Value;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    time::Instant,
};

use super::{
    NEXT_CHECK_ID,
    adapter::{Deadline, InputAdapter, InputError, Prepared},
    logging,
    trigger_matcher::TriggerMatcher,
};
use crate::srdl::ast::{Program, Rule, TriggerTiming};

const CAPACITY: usize = 256;
type CallKey = (String, String, TriggerTiming);

const RULE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FRESHNESS_RETRIES: usize = 2;
type RuleKey = (CallKey, usize);

/// Completed requirement checks handed back before any hook executes.
pub(super) struct Evaluation {
    /// Correlates input evaluation with the later enforcement log.
    pub check_id: u64,
    /// Source indices keep rule results in declaration order.
    pub rules: BTreeMap<usize, Result<RuleResult, Error>>,
}

/// Predicate results for one triggered rule, in source declaration order.
#[derive(Debug, PartialEq, Eq)]
pub struct RuleResult {
    /// True passes and false violates a requirement.
    pub requirements: Vec<bool>,
}

impl RuleResult {
    pub fn passed(&self) -> bool {
        self.requirements.iter().all(|value| *value)
    }

    /// Return one-based indices of violated requirements, without applying enforce.
    pub fn failed_requirements(&self) -> Vec<usize> {
        self.requirements
            .iter()
            .enumerate()
            .filter_map(|(index, value)| (!*value).then_some(index + 1))
            .collect()
    }
}

/// Failures that prevent a rule result, distinct from a false requirement.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("capability '{capability}' conflicts with '{conflicting}'; replan required")]
    Exclusion {
        /// Contract whose admission was rejected.
        capability: String,
        /// Contract responsible for the exclusion conflict.
        conflicting: String,
    },
    #[error("{0}")]
    Pool(&'static str),
    #[error(transparent)]
    Input(#[from] InputError),
    #[error("rule '{rule}', context '{binding}': {source}")]
    Context {
        /// The rule containing the context binding that failed.
        rule: String,
        /// The alias whose value could not be extracted or type checked.
        binding: String,
        /// The failed field traversal or basic type constraint.
        #[source]
        source: ContextError,
    },
    #[error("rule '{rule}', require #{requirement}: {source}")]
    Expression {
        /// The rule whose requirement could not be calculated.
        rule: String,
        /// The one-based condition index in the rule.
        requirement: usize,
        /// The arithmetic, value or expression failure.
        #[source]
        source: expression::Error,
    },
    #[error("rule '{rule}': overall check timed out")]
    Timeout {
        /// The rule that exhausted its fixed activation budget.
        rule: String,
    },
    #[error("rule '{rule}', input '{input}': data exceeded maxage")]
    Expired {
        /// The rule whose inputs are no longer valid.
        rule: String,
        /// The expired field alias.
        input: String,
    },
    #[error("rule '{rule}', input '{input}': maxage is not a positive representable duration")]
    InvalidMaxage {
        /// The rule containing the invalid freshness constraint.
        rule: String,
        /// The field whose duration or deadline cannot be represented.
        input: String,
    },
    #[error("rule '{rule}', input '{input}': acquisition time is in the future")]
    FutureTimestamp {
        /// The rule receiving inconsistent monotonic time metadata.
        rule: String,
        /// The field whose acquisition time exceeds the current time.
        input: String,
    },
    #[error("rule '{rule}': input stream ended with missing fields or unresolved requirements")]
    IncompleteInputs {
        /// The rule that could not finish all declared inputs and requirements.
        rule: String,
    },
}

/// One independently owned lifecycle check submitted to the shared pool.
pub(super) struct Check {
    /// Plan, call, and timing distinguish concurrent lifecycle events.
    key: CallKey,
    /// Generation rejecting late work when the same key is reused.
    check_id: u64,
    /// Contract used to select rules for this event.
    capability: String,
    /// An explicit subset for independently scheduled while rules; None matches all triggers.
    rules: Option<Vec<usize>>,
    /// Atlas consumer identity used to acquire inputs.
    consumer: String,
    /// Trusted call arguments and identity for this event's conditions.
    context: Arc<Value>,
    /// Dropping the submitting future cancels acquisition and releases metadata.
    cancel: watch::Receiver<()>,
    /// Delivers requirement results without retaining the pool during hook execution.
    reply: oneshot::Sender<Result<Evaluation, Error>>,
    /// Bounds active checks until their cancellation watcher is retired.
    permit: OwnedSemaphorePermit,
}

/// Acquisition, freshness, and result owned by one rule in an active invocation.
#[derive(Default)]
struct RuleEntry {
    /// Cancels acquisition while preserving the Adapter's connection cleanup.
    acquisition: Option<AbortHandle>,
    /// Retained deadlines keep early results valid through combined enforcement.
    freshness: Option<Deadline>,
    /// Bounds reacquisition of the active clause group when its snapshot expires.
    retries: usize,
    /// Evaluated requirements, absent while input acquisition or evaluation is pending.
    result: Option<Result<RuleResult, Error>>,
}

/// Metadata and combined results for one active lifecycle check.
struct Invocation {
    /// Distinguishes late responses from a later reuse of the same call key.
    check_id: u64,
    /// Atlas identity used for acquiring rule inputs.
    consumer: String,
    /// Trusted arguments and identity for this check.
    context: Arc<Value>,
    /// Selected rules and their complete state, ordered by source index.
    rules: BTreeMap<usize, RuleEntry>,
    /// Delivers this check's requirement results exactly once.
    reply: oneshot::Sender<Result<Evaluation, Error>>,
    /// Detects owner drop before a queued cancellation notification is processed.
    owner: watch::Receiver<()>,
    /// Retires the cancellation watcher together with the invocation.
    cancellation: AbortHandle,
    /// Fixed budget for the current condition group, preserved across retries.
    deadline: Instant,
}

/// A completed acquisition waiting for its atomic evaluation turn.
struct ReadyCheck {
    /// Identifies the rule within its invocation.
    key: RuleKey,
    /// Rejects events left over from an earlier invocation.
    check_id: u64,
    /// A fixed observation set, or the failure that prevented acquisition.
    data: Result<Prepared, Error>,
}

/// Shared execution queue for lifecycle checks; complete snapshots enter FIFO evaluation.
pub struct RuleEvaluator<'a> {
    /// Policy that remains immutable for the worker's lifetime.
    program: &'a Program,
    /// Shared Adapter used by independently polled acquisition streams.
    adapter: &'a InputAdapter,
    /// Combined results and cancellation metadata for active checks.
    calls: HashMap<CallKey, Invocation>,
    /// Acquisition completions, each carrying its original invocation generation.
    acquiring: FuturesUnordered<BoxFuture<'a, Option<ReadyCheck>>>,
    /// Owner-drop notifications that cannot be lost to a full command channel.
    cancelled: FuturesUnordered<BoxFuture<'a, Option<(CallKey, u64)>>>,
    /// Complete snapshots scheduled in arrival order.
    ready: VecDeque<ReadyCheck>,
}

impl<'a> RuleEvaluator<'a> {
    /// Start the shared queue and retain its capacity until cancellation watchers retire.
    pub(in crate::interpreter) fn start(
        adapter: InputAdapter,
        program: Arc<Program>,
    ) -> (mpsc::Sender<Check>, Arc<Semaphore>) {
        let (sender, receive) = mpsc::channel(CAPACITY);
        tokio::spawn(async move {
            RuleEvaluator::new(&program, &adapter).run(receive).await;
        });
        (sender, Arc::new(Semaphore::new(CAPACITY)))
    }

    /// Lifecycle checks share acquisition, evaluation, timeout and cancellation behavior.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::interpreter) async fn check(
        queue: &(mpsc::Sender<Check>, Arc<Semaphore>),
        plan_id: &str,
        call_id: &str,
        capability: &str,
        consumer: &str,
        context: &Value,
        timing: TriggerTiming,
        rules: Option<Vec<usize>>,
    ) -> Result<Evaluation, Error> {
        let (sender, permits) = queue;
        let permit = permits
            .clone()
            .acquire_owned()
            .await
            .expect("private check semaphore stays open");
        let check_id = NEXT_CHECK_ID.fetch_add(1, Ordering::Relaxed);
        // Keep cancellation ownership local so no state survives the check future.
        let (_owner, cancel) = watch::channel(());
        let (reply, receive) = oneshot::channel();
        let check = Check {
            key: (plan_id.to_owned(), call_id.to_owned(), timing),
            check_id,
            capability: capability.to_owned(),
            rules,
            consumer: consumer.to_owned(),
            context: Arc::new(context.clone()),
            cancel,
            reply,
            permit,
        };
        if sender.send(check).await.is_err() {
            return Err(Error::Pool("Sentinel check pool stopped"));
        }
        receive
            .await
            .unwrap_or_else(|_| Err(Error::Pool("Sentinel rule check stopped")))
    }

    fn new(program: &'a Program, adapter: &'a InputAdapter) -> Self {
        Self {
            program,
            adapter,
            calls: HashMap::new(),
            acquiring: FuturesUnordered::new(),
            cancelled: FuturesUnordered::new(),
            ready: VecDeque::new(),
        }
    }

    /// Multiplex work without holding a lock across acquisition or completion waits.
    /// Cancellation has a separate stream so a full command queue cannot prevent cleanup.
    async fn run(mut self, mut checks: mpsc::Receiver<Check>) {
        loop {
            tokio::select! {
                // Process check requests in arrival order
                check = checks.recv() => match check {
                    Some(check) => self.register(check),
                    None => break,
                },
                // Process acquisition completions
                Some(result) = self.acquiring.next(), if !self.acquiring.is_empty() => {
                    if let Some(result) = result
                        && self.current(&result.key.0, result.check_id) {
                        self.rule(&result.key.0, result.key.1).acquisition = None;
                        self.ready.push_back(result);
                    }
                },
                // Process cancellation notifications
                Some(cancelled) = self.cancelled.next(), if !self.cancelled.is_empty() => {
                    if let Some((key, check_id)) = cancelled
                        && self.current(&key, check_id) { self.remove(&key); }
                },
                // Evaluate ready snapshots
                _ = std::future::ready(()), if !self.ready.is_empty() => {
                    let ready = self.ready.pop_front().unwrap();
                    self.evaluate(ready);
                },
            }
        }
        // Abort handles do not cancel on drop; dropping the futures also cancels input streams.
    }

    /// Returns true if the call key is still active and its check ID matches the current invocation.
    fn current(&self, key: &CallKey, check_id: u64) -> bool {
        self.calls
            .get(key)
            .is_some_and(|call| call.check_id == check_id)
    }

    fn rule(&mut self, key: &CallKey, index: usize) -> &mut RuleEntry {
        let call = self.calls.get_mut(key).unwrap();
        call.rules.get_mut(&index).unwrap()
    }

    /// Register all trigger timings through the same matching and acquisition path.
    fn register(&mut self, check: Check) {
        let Check {
            key,
            check_id,
            capability,
            rules,
            consumer,
            context,
            cancel,
            reply,
            permit,
        } = check;
        let rules = rules
            .unwrap_or_else(|| TriggerMatcher::match_indices(self.program, &capability, key.2));
        logging::log(
            robonix_scribe::Level::Info,
            format!(
                "[sentinel] check_id={check_id} plan_id={:?} call_id={:?} timing={:?} capability={capability:?} consumer={consumer:?} triggered_rules={:?}",
                key.0,
                key.1,
                key.2,
                rules
                    .iter()
                    .map(|index| self.program.rules[*index].name.0.as_str())
                    .collect::<Vec<_>>()
            ),
        );
        if rules.is_empty() {
            let _ = reply.send(Ok(Evaluation {
                check_id,
                rules: BTreeMap::new(),
            }));
            return;
        }
        if self
            .calls
            .get(&key)
            .is_some_and(|call| call.owner.has_changed().is_err())
        {
            self.remove(&key);
        }
        if self.calls.contains_key(&key) {
            let _ = reply.send(Err(Error::Pool(
                "Capability event already has active Sentinel checks",
            )));
            return;
        }
        let (cancellation, registration) = AbortHandle::new_pair();
        self.calls.insert(
            key.clone(),
            Invocation {
                check_id,
                consumer,
                context,
                rules: rules
                    .iter()
                    .map(|index| (*index, RuleEntry::default()))
                    .collect(),
                reply,
                owner: cancel.clone(),
                cancellation,
                deadline: Instant::now() + RULE_TIMEOUT,
            },
        );
        let cancelled_key = key.clone();
        self.cancelled.push(
            async move {
                // Capacity is released only when even an aborted watcher is retired.
                let _permit = permit;
                Abortable::new(
                    async move {
                        let mut cancel = cancel;
                        let _ = cancel.changed().await;
                        (cancelled_key, check_id)
                    },
                    registration,
                )
                .await
                .ok()
            }
            .boxed(),
        );
        self.acquire(&key, rules);
    }

    /// Register peers together before fetching, then retain one atomic snapshot per rule.
    fn acquire(&mut self, key: &CallKey, indices: Vec<usize>) {
        let call = &self.calls[key];
        let check_id = call.check_id;
        let deadline = call.deadline;
        let datasets = self.adapter.acquire_group(
            indices.iter().map(|index| &self.program.rules[*index]),
            &call.consumer,
            call.context.clone(),
            check_id,
            deadline,
        );
        for (index, dataset) in indices.into_iter().zip(datasets) {
            let (abort, registration) = AbortHandle::new_pair();
            let entry = self.rule(key, index);
            entry.freshness = None;
            entry.result = None;
            entry.acquisition = Some(abort);
            let key = (key.clone(), index);
            self.acquiring.push(
                async move {
                    Abortable::new(
                        async move {
                            let data = dataset.await;
                            ReadyCheck {
                                key,
                                check_id,
                                data,
                            }
                        },
                        registration,
                    )
                    .await
                    .ok()
                }
                .boxed(),
            );
        }
    }

    /// Only expiry can retry; the original timeout and attempt limit remain authoritative.
    fn retry(&mut self, key: &RuleKey, result: &Result<RuleResult, Error>) -> bool {
        let call = self.calls.get_mut(&key.0).unwrap();
        if !matches!(result, Err(Error::Expired { .. })) || Instant::now() >= call.deadline {
            return false;
        }
        let entry = call.rules.get_mut(&key.1).unwrap();
        if entry.retries >= MAX_FRESHNESS_RETRIES {
            return false;
        }
        entry.retries += 1;
        logging::log(
            robonix_scribe::Level::Info,
            format!(
                "[sentinel] check_id={} rule={:?} freshness_retry={}",
                call.check_id, self.program.rules[key.1].name.0, entry.retries,
            ),
        );
        true
    }

    /// Run one complete clause group without yielding between its predicates.
    fn evaluate(&mut self, ready: ReadyCheck) {
        if !self.current(&ready.key.0, ready.check_id) {
            return;
        }
        let rule = &self.program.rules[ready.key.1];
        let entry = self.rule(&ready.key.0, ready.key.1);
        let mut result = match ready.data {
            Ok(prepared) => {
                let result = prepared
                    .deadline
                    .check(rule, Instant::now())
                    .and_then(|()| {
                        let result = evaluate(ready.check_id, rule, &prepared)?;
                        prepared.deadline.check(rule, Instant::now())?;
                        Ok(result)
                    });
                entry.freshness = Some(prepared.deadline);
                result
            }
            Err(error) => Err(error),
        };
        if Instant::now() >= self.calls[&ready.key.0].deadline {
            result = Err(Error::Timeout {
                rule: self.program.rules[ready.key.1].name.0.clone(),
            });
        }
        if self.retry(&ready.key, &result) {
            self.acquire(&ready.key.0, vec![ready.key.1]);
            return;
        }
        let call = self.calls.get_mut(&ready.key.0).unwrap();
        call.rules.get_mut(&ready.key.1).unwrap().result = Some(result);
        if call.rules.values().all(|entry| entry.result.is_some()) {
            self.finish_check(&ready.key.0);
        }
    }

    /// Recheck freshness before returning combined results or scheduling retries.
    fn finish_check(&mut self, key: &CallKey) {
        let indices: Vec<_> = self.calls[key].rules.keys().copied().collect();
        let mut retries = Vec::new();
        for index in indices {
            let rule = &self.program.rules[index];
            let entry = self.rule(key, index);
            let result = entry.result.take().unwrap();
            let passed = result.as_ref().is_ok_and(RuleResult::passed);
            // Earlier snapshots must still be fresh when the last rule finishes.
            let result = result.and_then(|result| {
                if let Some(freshness) = &entry.freshness {
                    freshness.check(rule, Instant::now())?;
                }
                Ok(result)
            });
            if passed && self.retry(&(key.clone(), index), &result) {
                retries.push(index);
            } else {
                self.rule(key, index).result = Some(result);
            }
        }
        if !retries.is_empty() {
            self.acquire(key, retries);
            return;
        }
        let call = self.remove(key);
        let _ = call.reply.send(Ok(Evaluation {
            check_id: call.check_id,
            rules: call
                .rules
                .into_iter()
                .map(|(index, entry)| (index, entry.result.unwrap()))
                .collect(),
        }));
    }

    /// Abort acquisition and retire the watcher even if its caller never polls the reply.
    fn remove(&mut self, key: &CallKey) -> Invocation {
        let call = self.calls.remove(key).unwrap();
        call.cancellation.abort();
        for entry in call.rules.values() {
            if let Some(acquisition) = &entry.acquisition {
                acquisition.abort();
            }
        }
        self.ready
            .retain(|ready| &ready.key.0 != key || ready.check_id != call.check_id);
        call
    }
}

/// Evaluate all conditions synchronously against this snapshot.
pub(super) fn evaluate(
    check_id: u64,
    rule: &Rule,
    prepared: &Prepared,
) -> Result<RuleResult, Error> {
    let mut requirements = Vec::new();
    for (index, condition) in rule.requires.iter().enumerate() {
        let result = expression::evaluate_condition(&condition.expression, &prepared.values)
            .map_err(|source| Error::Expression {
                rule: rule.name.0.clone(),
                requirement: index + 1,
                source,
            })?;
        logging::log(
            Level::Info,
            format!(
                "[sentinel] check_id={check_id} rule={:?} require={} result={result:?}",
                rule.name.0,
                index + 1
            ),
        );
        requirements.push(result);
    }
    Ok(RuleResult { requirements })
}

#[cfg(test)]
mod tests;
