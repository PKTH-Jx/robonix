//! Ordered hook execution and accumulated capability-call feedback.

mod builder;
pub mod hooks;

pub use builder::{EnforcementBuilder, HookCall};
pub use hooks::{Hook, HookRuntime};

use serde::{Deserialize, Serialize};

use super::evaluator::{self, RuleResult};
use crate::srdl::ast::{Program, TriggerTiming};

/// Capability-call decision returned by SRDL enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Deny,
}

/// An enforcement failure, separate from an input or requirement evaluation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Runtime(String),
}

/// One rule's check and violation response, retained for feedback to the caller.
#[derive(Debug)]
pub struct RuleReport {
    /// The event that caused this rule to be checked.
    pub timing: TriggerTiming,
    /// The SRDL rule that produced this feedback.
    pub rule: String,
    /// Predicate values in clause order, or the failure that prevented calculation.
    pub evaluation: Result<RuleResult, evaluator::Error>,
    /// The violation decision or enforcement failure; absent when enforce was not applied.
    pub enforcement: Option<Result<Effect, Error>>,
}

/// Combined decision and rule feedback for one capability call.
#[derive(Debug)]
pub struct EnforcementResult {
    /// Identifies a committed exclusion admission for an eventual completion notification.
    pub admission_id: Option<u64>,
    /// Before checks may deny admission; after checks always allow and report their results.
    pub effect: Effect,
    /// Messages accumulated by report hooks in execution order.
    pub messages: Vec<String>,
    /// Capability calls recorded by hooks, including runtime execution outcomes.
    pub calls: Vec<HookCall>,
    /// All supplied rule results, in the caller's declaration order.
    pub reports: Vec<RuleReport>,
}

/// Hooks bound once when a rule set loads, in rule declaration order.
pub struct Enforcer {
    /// Immutable implementations reused by independent invocation builders.
    rules: Vec<RuleHooks>,
}

/// Bound responses for one rule, independent of an invocation's mutable builder.
struct RuleHooks {
    /// SRDL name included in evaluation and enforcement feedback.
    name: String,
    /// Implementations bound in the enforce clause's declaration order.
    hooks: Vec<Box<dyn Hook>>,
}

impl Enforcer {
    /// Keep bindings aligned with source rule indices returned by the evaluator.
    pub(super) fn new(program: &Program) -> Self {
        let rules = program
            .rules
            .iter()
            .map(|rule| RuleHooks {
                name: rule.name.0.clone(),
                hooks: hooks::bind(&rule.enforce),
            })
            .collect();
        Self { rules }
    }

    /// Consume source-ordered results; only violated requirements run their bound hooks.
    /// Evaluation errors deny before admission without invoking hooks; calls remain queued.
    pub(super) fn enforce(
        &self,
        results: impl IntoIterator<Item = (usize, Result<RuleResult, evaluator::Error>)>,
        timing: TriggerTiming,
        builder: &mut EnforcementBuilder,
    ) {
        for (index, evaluation) in results {
            let rule = &self.rules[index];
            let mut enforcement = None;
            match &evaluation {
                Ok(result) if !result.passed() => {
                    for hook in &rule.hooks {
                        hook.execute(builder);
                    }
                    enforcement = Some(Ok(builder.effect()));
                }
                Err(_) if timing == TriggerTiming::Before => builder.deny(),
                _ => {}
            }
            builder.reports.push(RuleReport {
                timing,
                rule: rule.name.clone(),
                evaluation,
                enforcement,
            });
        }
    }
}

#[cfg(test)]
mod tests;
