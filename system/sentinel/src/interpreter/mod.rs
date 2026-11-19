//! SRDL interpretation and enforcement decisions.

pub mod adapter;
pub mod enforcer;
pub mod evaluator;
mod exclusion;
mod logging;
pub mod trigger_matcher;
pub mod type_checker;

use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use futures::{Stream, stream};
use tokio::{
    sync::{Semaphore, mpsc},
    time::Instant,
};

use robonix_atlas::client::AtlasClient;
use robonix_scribe::Level;
use serde_json::Value;

use self::{
    adapter::{
        InputAdapter,
        strategy::{Strategies, StrategyConfig},
    },
    enforcer::{Effect, EnforcementBuilder, EnforcementResult, Enforcer, HookRuntime},
    evaluator::RuleEvaluator,
    exclusion::ExclusionChecker,
    trigger_matcher::TriggerMatcher,
    type_checker::{TypeChecker, TypeError},
};
use crate::srdl::{
    self,
    ast::{Program, TriggerTiming},
};

static NEXT_CHECK_ID: AtomicU64 = AtomicU64::new(1);

/// Trusted invocation data for SRDL rule evaluation, exclusions, and enforcement.
#[derive(Clone, Copy)]
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
}

/// A rule or strategy configuration failure that prevents executable checks.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Parse(#[from] srdl::Error),
    #[error(transparent)]
    Type(#[from] TypeError),
    #[error("strategy initialization failed: {0:#}")]
    Strategy(#[from] anyhow::Error),
}

/// Interprets an immutable, statically checked SRDL program for capability-call decisions.
pub struct SrdlInterpreter {
    /// Submission channel and capacity shared with the RuleEvaluator worker.
    evaluator: (mpsc::Sender<evaluator::Check>, Arc<Semaphore>),
    /// Capability lifetime restrictions, independent of the rule pool.
    exclusions: ExclusionChecker,
    /// Bound built-ins executed in the submitting task after the pool finishes evaluation.
    enforcer: Enforcer,
    /// Parsed triggers shared with the evaluator for invocation-local scheduling.
    program: Arc<srdl::ast::Program>,
}

impl SrdlInterpreter {
    /// Parse and check the entire source before any provider discovery or acquisition.
    /// Requires an active Tokio runtime to start the pool worker during construction.
    pub fn load(
        source: &str,
        client: AtlasClient,
        strategy_config: StrategyConfig,
    ) -> Result<Self, LoadError> {
        let program = Arc::new(srdl::parse(source)?);
        TypeChecker::check(&program)?;
        let strategies = Strategies::initialize(strategy_config)?;
        strategies.validate(&program)?;
        let adapter = InputAdapter::new(client).with_strategies(strategies);
        Ok(Self {
            exclusions: ExclusionChecker::new(&program.excludes),
            evaluator: RuleEvaluator::start(adapter, program.clone()),
            enforcer: Enforcer::new(&program),
            program,
        })
    }

    /// Load SRDL with strategy implementations supplied by the embedding runtime.
    pub fn load_with_strategies(
        source: &str,
        client: AtlasClient,
        strategies: Strategies,
    ) -> Result<Self, LoadError> {
        let program = Arc::new(srdl::parse(source)?);
        TypeChecker::check(&program)?;
        strategies.validate(&program)?;
        let adapter = InputAdapter::new(client).with_strategies(strategies);
        Ok(Self {
            exclusions: ExclusionChecker::new(&program.excludes),
            evaluator: RuleEvaluator::start(adapter, program.clone()),
            enforcer: Enforcer::new(&program),
            program,
        })
    }

    /// Admit an arriving capability after exclusion and before-rule checks.
    pub async fn on_arrival(
        &self,
        context: CheckContext<'_>,
        runtime: &dyn HookRuntime,
    ) -> EnforcementResult {
        let mut builder = EnforcementBuilder::default();
        // Reserve before asynchronous work can admit a conflicting call.
        let admission = self.exclusions.begin(
            context.plan_id,
            context.call_id,
            context.capability,
            &mut builder,
        );
        if builder.effect() != Effect::Allow {
            return builder.finish();
        }
        let (check_id, _) = self
            .evaluate(
                &context,
                runtime,
                TriggerTiming::Before,
                None,
                &mut BTreeSet::new(),
                &mut builder,
            )
            .await;
        runtime.execute(&mut builder, TriggerTiming::Before).await;

        // Corrective calls may have changed admission; commit only the final decision.
        // Commit also releases denied or cancelled reservations explicitly.
        if let Some(id) = admission {
            self.exclusions
                .commit(context.plan_id, context.call_id, id, &mut builder);
        }
        let decision = builder.finish();
        if let Some(check_id) = check_id {
            Self::log_decision(check_id, &decision);
        }
        decision
    }

    /// Yield interval checks throughout one capability invocation, ending on stop or cancellation.
    /// Each stream owns its schedule and recovery history; await active hooks before dropping it.
    pub fn on_execution<'a>(
        &'a self,
        context: CheckContext<'a>,
        runtime: &'a dyn HookRuntime,
    ) -> impl Stream<Item = EnforcementResult> + Send + 'a {
        let checks = WhileChecks::new(&self.program, context.capability);
        stream::unfold(Some(checks), move |checks| async move {
            let mut checks = checks?;
            let next = checks.rules.iter_mut().min_by_key(|rule| rule.next);
            let ready = async {
                match next.as_ref() {
                    Some(rule) => tokio::time::sleep_until(rule.next).await,
                    // Unmatched invocations sleep until execution ends their monitoring.
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                biased;
                _ = runtime.cancelled() => {
                    let mut builder = EnforcementBuilder::default();
                    builder.error(evaluator::Error::Pool("Sentinel while check cancelled"), TriggerTiming::While);
                    runtime.execute(&mut builder, TriggerTiming::While).await;
                    return Some((builder.finish(), None));
                }
                () = ready => {}
            }
            let rule = next.unwrap();
            let started = Instant::now();
            // Await one due rule at a time so checks for this invocation never overlap.
            let mut builder = EnforcementBuilder::default();
            let (check_id, elapsed) = self
                .evaluate(
                    &context,
                    runtime,
                    TriggerTiming::While,
                    Some(vec![rule.index]),
                    &mut checks.violated,
                    &mut builder,
                )
                .await;
            // A stop must reach execution before a corrective call can delay it.
            runtime.execute(&mut builder, TriggerTiming::While).await;

            // Acquisition/evaluation cost sets a start-to-start interval for this rule.
            rule.samples = rule.samples.saturating_add(1);
            rule.average += (elapsed.as_secs_f64() - rule.average) / rule.samples as f64;
            let delay = Duration::try_from_secs_f64(rule.interval * rule.average.max(1e-9)).ok();
            match delay.and_then(|delay| started.checked_add(delay)) {
                Some(next) => rule.next = next,
                None => {
                    builder.error(
                        evaluator::Error::Pool("Sentinel while interval cannot be represented"),
                        TriggerTiming::While,
                    );
                    runtime.stop().await;
                }
            }
            let decision = builder.finish();
            if let Some(check_id) = check_id {
                Self::log_decision(check_id, &decision);
            }
            let checks = (decision.effect != Effect::Stop).then_some(checks);
            Some((decision, checks))
        })
    }

    /// Release all admissions owned by a failed plan task, including its corrective calls.
    pub fn release_failed_task(&self, plan_id: &str, task: tokio::task::Id) {
        self.exclusions.finish_task(plan_id, task);
    }

    /// Release a completed capability's admission, then check after rules only on success.
    pub async fn on_complete(
        &self,
        context: CheckContext<'_>,
        runtime: &dyn HookRuntime,
        admission_id: Option<u64>,
        succeeded: bool,
    ) -> EnforcementResult {
        // Release even failed or cancelled work before acquiring any postcondition inputs.
        if let Some(id) = admission_id {
            self.exclusions.finish(context.plan_id, context.call_id, id);
        }
        if !succeeded {
            return EnforcementBuilder::default().finish();
        }
        let mut builder = EnforcementBuilder::default();
        let (check_id, _) = self
            .evaluate(
                &context,
                runtime,
                TriggerTiming::After,
                None,
                &mut BTreeSet::new(),
                &mut builder,
            )
            .await;
        runtime.execute(&mut builder, TriggerTiming::After).await;
        let decision = builder.finish();
        if let Some(check_id) = check_id {
            Self::log_decision(check_id, &decision);
        }
        decision
    }

    /// Acquire and enforce predicates, leaving corrective calls and admission to the caller.
    async fn evaluate(
        &self,
        context: &CheckContext<'_>,
        runtime: &dyn HookRuntime,
        timing: TriggerTiming,
        rules: Option<Vec<usize>>,
        violated: &mut BTreeSet<usize>,
        builder: &mut EnforcementBuilder,
    ) -> (Option<u64>, Duration) {
        let started = tokio::time::Instant::now();
        let evaluated = tokio::select! {
            biased;
            _ = runtime.cancelled() => Err(evaluator::Error::Pool("Sentinel check cancelled")),
            result = RuleEvaluator::check(&self.evaluator, context.plan_id, context.call_id, context.capability,
                context.consumer_id, context.values, timing, rules) => result,
        };
        let check_id = match evaluated {
            Ok(evaluation) => {
                self.enforcer
                    .enforce(evaluation.rules, timing, builder, violated);
                Some(evaluation.check_id)
            }
            Err(error) => {
                builder.error(error, timing);
                None
            }
        };
        // The measured cost excludes corrective calls, which execute after this stage.
        (check_id, started.elapsed())
    }

    /// Correlate lifecycle reports without logging bound strings.
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

/// Schedule and violation history owned by one capability invocation.
struct WhileChecks {
    /// One independently timed check per rule selected by a unique while trigger.
    rules: Vec<WhileRule>,
    /// Violated rules whose corrective hooks have already run, bounded by the loaded rule count.
    violated: BTreeSet<usize>,
}

/// Timing measurements for one repeatedly evaluated rule.
struct WhileRule {
    /// Source rule index passed to the existing evaluator.
    index: usize,
    /// Factor applied to this rule's average check duration.
    interval: f64,
    /// Earliest next start; checks for one invocation never overlap.
    next: Instant,
    /// Arithmetic mean of completed acquisition and evaluation durations, in seconds.
    average: f64,
    /// Number of measurements contributing to the mean.
    samples: u64,
}

impl WhileChecks {
    /// Build fresh timing and recovery history from the statically checked trigger associations.
    fn new(program: &Program, capability: &str) -> Self {
        let rules = program
            .rules
            .iter()
            .enumerate()
            .filter_map(|(index, rule)| {
                // Type checking guarantees at most one matching trigger for each rule and timing.
                program
                    .triggers
                    .iter()
                    .find(|trigger| {
                        TriggerMatcher::matches(trigger, capability, TriggerTiming::While)
                            && trigger.rules.contains(&rule.name)
                    })
                    .map(|trigger| WhileRule {
                        index,
                        interval: trigger.interval,
                        next: Instant::now(),
                        average: 0.0,
                        samples: 0,
                    })
            })
            .collect();
        Self {
            rules,
            violated: BTreeSet::new(),
        }
    }
}
