//! Dependency-driven rule evaluation with synchronous expressions and input freshness checks.

pub mod expression;

mod context;
mod dependencies;
mod freshness;

pub use context::Error as ContextError;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures::{Stream, StreamExt};
use robonix_scribe::Level;
use serde_json::Value;
use tokio::time::{Instant, sleep_until};

use super::adapter::{AcquiredInput, InputAdapter, InputError, InputValue};
use super::logging;
use crate::srdl::ast::Rule;
use dependencies::DependencyPlan;
use freshness::Freshness;

const FIRST_INPUT_TIMEOUT: Duration = Duration::from_secs(5);

/// Completed requirement values, in source declaration order.
#[derive(Debug, PartialEq, Eq)]
pub struct RuleResult {
    /// True means the corresponding requirement holds for the acquired inputs.
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
        /// The one-based requirement index.
        requirement: usize,
        /// The arithmetic, value or expression failure.
        #[source]
        source: expression::Error,
    },
    #[error("rule '{rule}': first input timed out")]
    FirstInputTimeout {
        /// The rule still waiting for its first input result.
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

/// Evaluates one applicable rule while the Adapter acquires its input groups.
pub struct RuleEvaluator {
    /// Input acquisition shared across evaluations, including its concurrency limit.
    adapter: InputAdapter,
}

impl RuleEvaluator {
    pub(super) fn new(adapter: InputAdapter) -> Self {
        Self { adapter }
    }

    /// Calculate a rule result without matching triggers or executing enforce.
    /// Any failure drops the input stream so the Adapter can cancel and clean up.
    /// Context must be the Executor's trusted snapshot for this call, not provider data.
    pub(super) async fn evaluate(
        &self,
        check_id: u64,
        rule: &Rule,
        consumer_id: &str,
        context: &Value,
    ) -> Result<RuleResult, Error> {
        evaluate_inputs(
            check_id,
            context,
            rule,
            self.adapter.acquire(rule, consumer_id),
        )
        .await
    }
}

async fn evaluate_inputs<'ast>(
    check_id: u64,
    context: &Value,
    rule: &'ast Rule,
    inputs: impl Stream<Item = Result<AcquiredInput<'ast>, InputError>>,
) -> Result<RuleResult, Error> {
    // SrdlInterpreter validates the immutable program before any rule is evaluated.
    let mut values = context::bind(rule, context)?;
    let mut plan = DependencyPlan::new(rule);
    // Context is ready at the start and never participates in input acquisition deadlines.
    for (name, value) in &values {
        logging::log(
            Level::Info,
            format!(
                "[sentinel] check_id={check_id} rule={:?} context={name:?} value={:?}",
                rule.name.0,
                logging::value(value)
            ),
        );
        plan.arrived(name);
    }
    let mut freshness = Freshness::new(rule, Instant::now(), FIRST_INPUT_TIMEOUT)?;
    evaluate_ready(check_id, rule, &mut plan, &values, &freshness)?;
    tokio::pin!(inputs);
    loop {
        freshness.check(Instant::now())?;
        let deadline = freshness.next_deadline();
        let item = tokio::select! {
            item = inputs.next() => item,
            _ = async {
                match deadline {
                    Some(deadline) => sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => continue,
        };
        // A deadline and an input can become ready together; expiry must not depend on select order.
        freshness.check(Instant::now())?;
        let Some(item) = item else {
            if !freshness.complete() {
                return Err(Error::IncompleteInputs {
                    rule: rule.name.0.clone(),
                });
            }
            let requirements = plan
                .requirements
                .into_iter()
                .map(|state| state.result)
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| Error::IncompleteInputs {
                    rule: rule.name.0.clone(),
                })?;
            return Ok(RuleResult { requirements });
        };
        let acquired = item?;
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
            if let std::collections::btree_map::Entry::Vacant(entry) = values.entry(name.to_owned())
            {
                freshness.record(name, acquired.acquired_at, Instant::now())?;
                entry.insert(Arc::new(selected.value));
                plan.arrived(name);
            }
        }
        freshness.check(Instant::now())?;
        evaluate_ready(check_id, rule, &mut plan, &values, &freshness)?;
    }
}

fn evaluate_ready(
    check_id: u64,
    rule: &Rule,
    plan: &mut DependencyPlan<'_>,
    values: &BTreeMap<String, Arc<InputValue>>,
    freshness: &Freshness<'_>,
) -> Result<(), Error> {
    for (index, state) in plan.requirements.iter_mut().enumerate() {
        if state.remaining == 0 && state.result.is_none() {
            freshness.check(Instant::now())?;
            let result = expression::evaluate_requirement(&rule.requires[index], values).map_err(
                |source| Error::Expression {
                    rule: rule.name.0.clone(),
                    requirement: index + 1,
                    source,
                },
            )?;
            state.result = Some(result);
            logging::log(
                Level::Info,
                format!(
                    "[sentinel] check_id={check_id} rule={:?} require={} result={result}",
                    rule.name.0,
                    index + 1
                ),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
