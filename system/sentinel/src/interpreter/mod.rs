//! SRDL interpretation and enforcement decisions.

pub mod adapter;
pub mod enforcer;
pub mod evaluator;
pub mod trigger_matcher;
pub mod type_checker;
pub mod visitor;

use futures::future::join_all;
use robonix_atlas::client::AtlasClient;
use serde_json::Value;

use crate::srdl::{self, ast::Program};
use adapter::InputAdapter;
use enforcer::{EnforcementResult, Enforcer};
use evaluator::RuleEvaluator;
use trigger_matcher::TriggerMatcher;
use type_checker::{TypeChecker, TypeError};

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
    /// Rules validated once at construction, shared by concurrent checks through borrowing.
    program: Program,
    /// Evaluates matched rules using the Adapter's shared acquisition concurrency limit.
    evaluator: RuleEvaluator,
}

impl SrdlInterpreter {
    /// Parse and check the entire source before any provider discovery or acquisition.
    pub fn new(source: &str, client: AtlasClient) -> Result<Self, LoadError> {
        let program = srdl::parse(source)?;
        TypeChecker::check(&program)?;
        let adapter = InputAdapter::new(client);
        Ok(Self {
            program,
            evaluator: RuleEvaluator::new(adapter),
        })
    }

    /// Return decisions and feedback without dispatching the target call or contacting Pilot.
    /// Context is the Executor's trusted call snapshot. Dropping this future cancels acquisition.
    pub async fn check(
        &self,
        capability: &str,
        consumer_id: &str,
        context: &Value,
    ) -> EnforcementResult {
        // join_all keeps declaration order even when input acquisitions finish out of order.
        let results = join_all(
            TriggerMatcher::match_rules(&self.program, capability)
                .into_iter()
                .map(|rule| async move {
                    (
                        rule,
                        self.evaluator.evaluate(rule, consumer_id, context).await,
                    )
                }),
        )
        .await;
        Enforcer::enforce(results)
    }
}
