//! SRDL interpretation and enforcement decisions.

pub mod adapter;
pub mod enforcer;
pub mod evaluator;
mod exclusion;
mod logging;
pub mod trigger_matcher;
pub mod type_checker;

use std::sync::atomic::AtomicU64;

use robonix_atlas::client::AtlasClient;
use robonix_scribe::Level;
use serde_json::Value;

use self::{
    adapter::InputAdapter,
    enforcer::{Effect, EnforcementResult},
    evaluator::RuleEvaluator,
    exclusion::ExclusionProcessor,
    type_checker::{TypeChecker, TypeError},
};
use crate::srdl::{self, ast::TriggerTiming};

static NEXT_CHECK_ID: AtomicU64 = AtomicU64::new(1);

/// A source or static type failure that prevents a rule set from becoming executable.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Parse(#[from] srdl::Error),
    #[error(transparent)]
    Type(#[from] TypeError),
}

/// Interprets an immutable, statically checked SRDL program for capability-call decisions.
pub struct SrdlInterpreter {
    /// Evaluates matched rules using the Adapter's shared acquisition concurrency limit.
    evaluator: RuleEvaluator,
    /// Capability lifetime restrictions, independent of the rule pool.
    exclusions: ExclusionProcessor,
}

impl SrdlInterpreter {
    /// Parse and check the entire source before any provider discovery or acquisition.
    /// Requires an active Tokio runtime to start the pool worker during construction.
    pub fn new(source: &str, client: AtlasClient) -> Result<Self, LoadError> {
        let program = srdl::parse(source)?;
        TypeChecker::check(&program)?;
        let adapter = InputAdapter::new(client);
        Ok(Self {
            exclusions: ExclusionProcessor::new(&program.excludes),
            evaluator: RuleEvaluator::new(adapter, std::sync::Arc::new(program)),
        })
    }

    /// Rules are independent per event; before checks also commit exclusion admission.
    pub async fn check(
        &self,
        plan_id: &str,
        call_id: &str,
        capability: &str,
        consumer_id: &str,
        context: &Value,
        timing: TriggerTiming,
    ) -> EnforcementResult {
        // Check exclusions first
        let admission = if timing == TriggerTiming::Before {
            match self.exclusions.begin(plan_id, call_id, capability) {
                Ok(admission) => admission,
                Err(error) => return EnforcementResult::error(error, timing),
            }
        } else {
            None
        };
        let mut decision = self
            .evaluator
            .check(plan_id, call_id, capability, consumer_id, context, timing)
            .await;
        // If the rule evaluation passed, enforce the exclusion admission decision.
        if let Some(admission) = admission {
            match admission.enforce(decision.effect == Effect::Allow) {
                Ok(id) => decision.admission_id = id,
                Err(error) => {
                    decision.effect = Effect::Deny;
                    decision
                        .reports
                        .extend(EnforcementResult::error(error, timing).reports);
                }
            }
        }
        decision
    }

    /// Release restrictions when the caller reports completion, including failure or cancellation.
    pub fn finish(&self, plan_id: &str, call_id: &str, admission_id: u64) {
        self.exclusions.finish(plan_id, call_id, admission_id);
    }

    /// Keep precondition and postcondition reports correlated without logging bound strings.
    fn log_decision(check_id: u64, decision: &EnforcementResult) {
        for report in &decision.reports {
            match &report.evaluation {
                Ok(result) => logging::log(
                    Level::Info,
                    format!(
                        "[sentinel] check_id={check_id} rule={:?} timing={:?} result={} requirements={:?} enforcement={:?} report={:?}",
                        report.rule,
                        report.timing,
                        if result.passed() {
                            "passed"
                        } else {
                            "violated"
                        },
                        result.requirements,
                        report.enforcement,
                        report.message
                    ),
                ),
                Err(error) => logging::log(
                    Level::Warn,
                    format!(
                        "[sentinel] check_id={check_id} rule={:?} timing={:?} result=error error={:?}",
                        report.rule,
                        report.timing,
                        logging::error(error)
                    ),
                ),
            }
        }
        logging::log(
            Level::Info,
            format!(
                "[sentinel] check_id={check_id} decision={:?}",
                decision.effect
            ),
        );
    }
}
