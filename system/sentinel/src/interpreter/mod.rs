//! SRDL interpretation and enforcement decisions.

pub mod adapter;
pub mod enforcer;
pub mod evaluator;
mod logging;
pub mod trigger_matcher;
pub mod type_checker;

pub use evaluator::CallChecks;

use std::sync::atomic::AtomicU64;

use robonix_atlas::client::AtlasClient;
use robonix_scribe::Level;
use serde_json::Value;

use self::{
    adapter::InputAdapter,
    enforcer::EnforcementResult,
    evaluator::RuleEvaluator,
    type_checker::{TypeChecker, TypeError},
};
use crate::srdl;

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
}

impl SrdlInterpreter {
    /// Parse and check the entire source before any provider discovery or acquisition.
    /// Requires an active Tokio runtime to start the pool worker during construction.
    pub fn new(source: &str, client: AtlasClient) -> Result<Self, LoadError> {
        let program = srdl::parse(source)?;
        TypeChecker::check(&program)?;
        let adapter = InputAdapter::new(client);
        Ok(Self {
            evaluator: RuleEvaluator::new(adapter, std::sync::Arc::new(program)),
        })
    }

    /// Match once and retain postconditions until completion or owner cancellation.
    pub async fn begin(
        &self,
        plan_id: &str,
        call_id: &str,
        capability: &str,
        consumer_id: &str,
        context: &Value,
    ) -> (EnforcementResult, Option<CallChecks>) {
        self.evaluator
            .begin(plan_id, call_id, capability, consumer_id, context)
            .await
    }

    /// Keep precondition and postcondition reports correlated without logging bound strings.
    fn log_decision(check_id: u64, decision: &EnforcementResult) {
        for report in &decision.reports {
            match &report.evaluation {
                Ok(result) => logging::log(
                    Level::Info,
                    format!(
                        "[sentinel] check_id={check_id} rule={:?} ensure={} result={} requirements={:?} enforcement={:?} report={:?}",
                        report.rule,
                        report.ensure,
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
                        "[sentinel] check_id={check_id} rule={:?} ensure={} result=error error={:?}",
                        report.rule,
                        report.ensure,
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
