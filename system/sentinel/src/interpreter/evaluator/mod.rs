//! Invocation admission and atomic SRDL condition evaluation.

pub mod expression;
mod pool;

pub use super::adapter::ContextError;

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use robonix_scribe::Level;
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};

use self::pool::Pool;
use super::{
    NEXT_CHECK_ID,
    adapter::{InputAdapter, InputError, Prepared},
    enforcer::EnforcementResult,
    logging,
};
use crate::srdl::ast::{Program, Rule, TriggerTiming};

const CAPACITY: usize = 256;
type CallKey = (String, String, TriggerTiming);

const RULE_TIMEOUT: Duration = Duration::from_secs(5);

/// Predicate results for one triggered rule, in source declaration order.
#[derive(Debug, PartialEq, Eq)]
pub struct RuleResult {
    /// True means the corresponding require expression holds.
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
            .filter_map(|(index, value)| (!value).then_some(index + 1))
            .collect()
    }
}

/// Failures that prevent a rule result, distinct from a false requirement.
#[derive(Debug, thiserror::Error)]
pub enum Error {
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

/// One independently owned before or after check submitted to the pool.
struct Check {
    /// Plan, call, and timing distinguish concurrent lifecycle events.
    key: CallKey,
    /// Generation rejecting late work when the same key is reused.
    check_id: u64,
    /// Contract used to select rules for this event.
    capability: String,
    /// Atlas consumer identity used to acquire inputs.
    consumer: Arc<str>,
    /// Trusted call arguments and identity for this event's conditions.
    context: Arc<Value>,
    /// Dropping the submitting future cancels acquisition and releases metadata.
    cancel: watch::Receiver<()>,
    /// Delivers this event's combined enforcement result and reports.
    reply: oneshot::Sender<EnforcementResult>,
    /// Bounds active checks until their cancellation watcher is retired.
    permit: OwnedSemaphorePermit,
}

/// Invocation admission and atomic predicate calculation through a shared pool worker.
pub struct RuleEvaluator {
    /// Delivers independent checks to the worker started during construction.
    sender: mpsc::Sender<Check>,
    /// Backpressure bounds active checks and their cancellation watchers.
    permits: Arc<Semaphore>,
}

impl RuleEvaluator {
    pub(in crate::interpreter) fn new(adapter: InputAdapter, program: Arc<Program>) -> Self {
        let (sender, receive) = mpsc::channel(CAPACITY);
        tokio::spawn(async move {
            Pool::new(&program, &adapter).run(receive).await;
        });
        Self {
            sender,
            permits: Arc::new(Semaphore::new(CAPACITY)),
        }
    }

    /// Both timings share acquisition, evaluation, timeout and cancellation behavior.
    pub(in crate::interpreter) async fn check(
        &self,
        plan_id: &str,
        call_id: &str,
        capability: &str,
        consumer: &str,
        context: &Value,
        timing: TriggerTiming,
    ) -> EnforcementResult {
        let permit = self
            .permits
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
            consumer: Arc::from(consumer),
            context: Arc::new(context.clone()),
            cancel,
            reply,
            permit,
        };
        if self.sender.send(check).await.is_err() {
            return EnforcementResult::failure("Sentinel check pool stopped", timing);
        }
        receive
            .await
            .unwrap_or_else(|_| EnforcementResult::failure("Sentinel rule check stopped", timing))
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
                    "[sentinel] check_id={check_id} rule={:?} require={} result={result}",
                    rule.name.0,
                    index + 1
                ),
            );
            requirements.push(result);
        }
        Ok(RuleResult { requirements })
    }
}
