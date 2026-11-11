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
    enforcer::{Effect, EnforcementBuilder, EnforcementResult, Enforcer, HookRuntime},
    evaluator::RuleEvaluator,
    exclusion::ExclusionProcessor,
    type_checker::{TypeChecker, TypeError},
};
use crate::srdl::{self, ast::TriggerTiming};

static NEXT_CHECK_ID: AtomicU64 = AtomicU64::new(1);

/// Trusted invocation data for SRDL rule evaluation, exclusions, and enforcement.
pub struct CheckContext<'a> {
    /// Plan containing the capability invocation.
    pub plan_id: &'a str,
    /// Call identity used for cancellation and feedback correlation.
    pub call_id: &'a str,
    /// Contract whose requirements are being checked.
    pub capability: &'a str,
    /// Atlas consumer used for acquiring rule inputs.
    pub consumer_id: &'a str,
    /// Immutable arguments and verified identity supplied by the embedding runtime.
    pub values: &'a Value,
    /// Lifecycle event that selected these rules.
    pub timing: TriggerTiming,
}

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
    /// Bound built-ins executed in the submitting task after the pool finishes evaluation.
    enforcer: Enforcer,
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
            enforcer: Enforcer::new(&program),
            evaluator: RuleEvaluator::new(adapter, program),
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
        self.check_with_runtime(
            &CheckContext {
                plan_id,
                call_id,
                capability,
                consumer_id,
                values: context,
                timing,
            },
            &(),
        )
        .await
    }

    /// Evaluate requirements and run hooks with operations supplied by the embedding runtime.
    pub async fn check_with_runtime(
        &self,
        context: &CheckContext<'_>,
        runtime: &dyn HookRuntime,
    ) -> EnforcementResult {
        let CheckContext {
            plan_id,
            call_id,
            capability,
            consumer_id,
            values,
            timing,
        } = *context;
        let mut builder = EnforcementBuilder::default();
        // 1. Reserve exclusion admission before any asynchronous work.
        let admission = if timing == TriggerTiming::Before {
            match self.exclusions.begin(plan_id, call_id, capability) {
                Ok(admission) => admission,
                Err(error) => {
                    builder.error(error, timing);
                    return builder.finish(None);
                }
            }
        } else {
            None
        };
        // 2. Evaluate matched rules with cancellation support.
        let evaluated = tokio::select! {
            biased;
            _ = runtime.cancelled() => Err(evaluator::Error::Pool("Sentinel check cancelled")),
            result = self.evaluator.check(plan_id, call_id, capability, consumer_id, values, timing) => result,
        };
        let check_id = match evaluated {
            Ok(evaluation) => {
                // 3. Accumulate violated rules' hooks in definition order.
                self.enforcer
                    .enforce(evaluation.rules, timing, &mut builder);
                // 4. Execute every recorded call, including after denial.
                runtime.execute(&mut builder, timing).await;
                Some(evaluation.check_id)
            }
            Err(error) => {
                builder.error(error, timing);
                None
            }
        };
        // 5. Recheck exclusions and commit admission using the accumulated decision.
        let admission_id = if let Some(admission) = admission {
            match admission.commit(builder.effect() == Effect::Allow) {
                Ok(id) => id,
                Err(error) => {
                    builder.error(error, timing);
                    None
                }
            }
        } else {
            None
        };
        // 6. Finalize once, then log and return the completed result.
        let decision = builder.finish(admission_id);
        if let Some(check_id) = check_id {
            Self::log_decision(check_id, &decision);
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
                        "[sentinel] check_id={check_id} rule={:?} timing={:?} result={} requirements={:?} enforcement={:?}",
                        report.rule,
                        report.timing,
                        if result.passed() {
                            "passed"
                        } else {
                            "violated"
                        },
                        result.requirements,
                        report
                            .enforcement
                            .as_ref()
                            .map(|result| result.as_ref().map_err(|_| "hook execution failed"))
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
                "[sentinel] check_id={check_id} decision={:?} messages={:?}",
                decision.effect, decision.messages
            ),
        );
    }
}
