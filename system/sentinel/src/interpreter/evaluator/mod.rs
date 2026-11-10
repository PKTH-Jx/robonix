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
    enforcer::{Effect, EnforcementResult},
    logging,
};
use crate::srdl::ast::{Program, Rule};

const CAPACITY: usize = 256;
type CallKey = (String, String);

const RULE_TIMEOUT: Duration = Duration::from_secs(5);

/// Predicate results for one active clause group, in source declaration order.
#[derive(Debug, PartialEq, Eq)]
pub struct RuleResult {
    /// True means the corresponding require or ensure expression holds.
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
    #[error("rule '{rule}', {clause} #{requirement}: {source}")]
    Expression {
        /// The rule whose requirement could not be calculated.
        rule: String,
        /// The condition keyword identifying preconditions or postconditions.
        clause: &'static str,
        /// The one-based index within that clause group.
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

/// Invocation lifecycle notifications serialized by the pool worker.
enum Command {
    /// Match rules, check `require`, and retain `ensure` clauses for successful completion.
    Begin {
        /// Plan and call IDs locating this invocation's rule entries.
        key: CallKey,
        /// Generation distinguishing this invocation from later reuse of its IDs.
        check_id: u64,
        /// Capability contract ID used for the single trigger-matching pass.
        capability: String,
        /// Atlas consumer identity used to acquire rule inputs.
        consumer: Arc<str>,
        /// Trusted arguments and identity shared by precondition and postcondition preparation.
        context: Arc<Value>,
        /// Owner-drop notification that cancels active and retained checks.
        cancel: watch::Receiver<()>,
        /// Returns the combined precondition decision before capability dispatch.
        reply: oneshot::Sender<EnforcementResult>,
        /// Admission slot retained until the invocation's cancellation watcher is retired.
        permit: OwnedSemaphorePermit,
    },
    /// Report successful capability execution and activate the retained `ensure` checks.
    Complete {
        /// Plan and call IDs identifying the retained invocation.
        key: CallKey,
        /// Rejects completion notifications belonging to an earlier reuse of those IDs.
        check_id: u64,
        /// Returns the combined postcondition decision before execution success is accepted.
        reply: oneshot::Sender<EnforcementResult>,
    },
}

/// Retains the matched postconditions and cancels pool work when its owner drops it.
pub struct CallChecks {
    /// Invocation identity supplied by Executor.
    key: CallKey,
    /// Generation that binds completion to the original precondition check.
    check_id: u64,
    /// Bounded delivery of the successful completion notification.
    sender: mpsc::Sender<Command>,
    /// Dropping this sender cancels outstanding acquisition and pending entries.
    _cancel: watch::Sender<()>,
}

impl CallChecks {
    /// Check the retained postconditions using new observations after successful execution.
    pub async fn ensure(self) -> EnforcementResult {
        let (reply, receive) = oneshot::channel();
        if self
            .sender
            .send(Command::Complete {
                key: self.key.clone(),
                check_id: self.check_id,
                reply,
            })
            .await
            .is_err()
        {
            return EnforcementResult::failure("Sentinel check pool stopped", true);
        }
        receive.await.unwrap_or_else(|_| {
            EnforcementResult::failure("Sentinel postcondition check stopped", true)
        })
    }
}

/// Invocation admission and atomic predicate calculation through a shared pool worker.
pub struct RuleEvaluator {
    /// Delivers invocation notifications to the worker started during construction.
    sender: mpsc::Sender<Command>,
    /// Backpressure bounds active calls and their retained postconditions.
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

    /// Wait for the combined precondition decision and return ownership of retained checks.
    /// Dropping that ownership cancels work independently of command-channel capacity.
    pub(in crate::interpreter) async fn begin(
        &self,
        plan_id: &str,
        call_id: &str,
        capability: &str,
        consumer: &str,
        context: &Value,
    ) -> (EnforcementResult, Option<CallChecks>) {
        // Admission precedes enqueueing so retained calls and cancellation watchers share a bound.
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("private admission semaphore stays open");
        let key = (plan_id.to_owned(), call_id.to_owned());
        let check_id = NEXT_CHECK_ID.fetch_add(1, Ordering::Relaxed);
        let (cancel, cancelled) = watch::channel(());
        let handle = CallChecks {
            key: key.clone(),
            check_id,
            sender: self.sender.clone(),
            _cancel: cancel,
        };
        let (reply, receive) = oneshot::channel();
        let command = Command::Begin {
            key,
            check_id,
            capability: capability.to_owned(),
            consumer: Arc::from(consumer),
            context: Arc::new(context.clone()),
            cancel: cancelled,
            reply,
            permit,
        };
        if self.sender.send(command).await.is_err() {
            return (
                EnforcementResult::failure("Sentinel check pool stopped", false),
                None,
            );
        }
        let result = receive.await.unwrap_or_else(|_| {
            EnforcementResult::failure("Sentinel precondition check stopped", false)
        });
        let handle = (result.effect == Effect::Allow).then_some(handle);
        (result, handle)
    }

    /// Evaluate one clause group synchronously against this snapshot.
    pub(super) fn evaluate(
        check_id: u64,
        rule: &Rule,
        ensure: bool,
        prepared: &Prepared,
    ) -> Result<RuleResult, Error> {
        let clause = if ensure { "ensure" } else { "require" };
        let expressions = rule
            .requires
            .iter()
            .filter(|_| !ensure)
            .map(|c| &c.expression)
            .chain(
                rule.ensures
                    .iter()
                    .filter(|_| ensure)
                    .map(|c| &c.expression),
            );
        let mut requirements = Vec::new();
        for (index, expression) in expressions.enumerate() {
            let result =
                expression::evaluate_condition(expression, &prepared.values).map_err(|source| {
                    Error::Expression {
                        rule: rule.name.0.clone(),
                        clause,
                        requirement: index + 1,
                        source,
                    }
                })?;
            logging::log(
                Level::Info,
                format!(
                    "[sentinel] check_id={check_id} rule={:?} {clause}={} result={result}",
                    rule.name.0,
                    index + 1
                ),
            );
            requirements.push(result);
        }
        Ok(RuleResult { requirements })
    }
}
