use super::*;
use crate::srdl::parse;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Records completed runtime operations and can simulate a dispatch failure.
#[derive(Default)]
struct Runtime {
    /// Capability IDs appended only after execution has yielded.
    calls: Mutex<Vec<String>>,
    /// Whether execution should fail after recording the attempted call.
    fail: bool,
    /// Whether stop was signalled before a corrective call began.
    stopped: AtomicBool,
    /// Requires stop to precede deferred calls in cancellation-order tests.
    require_stop: bool,
}

#[tonic::async_trait]
impl HookRuntime for Runtime {
    async fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    async fn call(&self, capability: &str) -> Result<(), Error> {
        if self.require_stop {
            assert!(self.stopped.load(Ordering::SeqCst));
        }
        tokio::task::yield_now().await;
        self.calls.lock().unwrap().push(capability.to_owned());
        if self.fail {
            Err(Error::Runtime("dispatch failed".into()))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn stop_precedes_corrective_calls_and_survives_allow() {
    let program =
        parse(r#"rule r { require false; enforce [call "release", stop, allow]; }"#).unwrap();
    let runtime = Runtime {
        require_stop: true,
        ..Default::default()
    };
    let result = enforce(
        &Enforcer::new(&program),
        [(
            0,
            Ok(RuleResult {
                requirements: vec![false],
            }),
        )],
        TriggerTiming::While,
        &runtime,
    )
    .await;
    assert_eq!(result.effect, Effect::Stop);
    assert_eq!(*runtime.calls.lock().unwrap(), ["release"]);
    let result = enforce(
        &Enforcer::new(&program),
        [(0, Err(evaluator::Error::Pool("unavailable")))],
        TriggerTiming::While,
        &runtime,
    )
    .await;
    assert_eq!(result.effect, Effect::Stop);
    assert!(result.calls.is_empty());
}

#[tokio::test]
async fn while_hooks_run_once_until_all_requirements_recover() {
    let program =
        parse(r#"rule r { require false; enforce [report "unsafe", call "correct"]; }"#).unwrap();
    let enforcer = Enforcer::new(&program);
    let runtime = Runtime::default();
    let mut violated = Default::default();
    for (requirements, messages) in [
        (vec![false, true], 1),
        (vec![false, false], 0),
        (vec![true, false], 0),
        (vec![true, true], 0),
        (vec![false, true], 1),
    ] {
        let mut builder = EnforcementBuilder::default();
        enforcer.enforce(
            [(0, Ok(RuleResult { requirements }))],
            TriggerTiming::While,
            &mut builder,
            &mut violated,
        );
        runtime.execute(&mut builder, TriggerTiming::While).await;
        assert_eq!(builder.finish().messages.len(), messages);
    }
    assert_eq!(*runtime.calls.lock().unwrap(), ["correct", "correct"]);
}

async fn enforce(
    enforcer: &Enforcer,
    results: impl IntoIterator<Item = (usize, Result<RuleResult, evaluator::Error>)>,
    timing: TriggerTiming,
    runtime: &dyn HookRuntime,
) -> EnforcementResult {
    let mut builder = EnforcementBuilder::default();
    enforcer.enforce(results, timing, &mut builder, &mut Default::default());
    runtime.execute(&mut builder, timing).await;
    builder.finish()
}

#[tokio::test]
async fn hooks_accumulate_feedback_and_never_reverse_denial() {
    let program = parse(
        r#"
        rule first { require false; enforce [allow, deny, report "blocked",
            call "robot/stop", report "stopped", allow, call "robot/release"]; }
        rule last { require false; enforce [allow, report "observed"]; }
    "#,
    )
    .unwrap();
    let enforcer = Enforcer::new(&program);
    let runtime = Runtime::default();
    let mut builder = EnforcementBuilder::default();
    enforcer.enforce(
        (0..2).map(|index| {
            (
                index,
                Ok(RuleResult {
                    requirements: vec![false],
                }),
            )
        }),
        TriggerTiming::Before,
        &mut builder,
        &mut Default::default(),
    );
    assert!(runtime.calls.lock().unwrap().is_empty());
    assert_eq!(builder.calls().len(), 2);
    assert!(builder.calls()[0].result.is_none());
    runtime.execute(&mut builder, TriggerTiming::Before).await;
    let result = builder.finish();
    assert_eq!(result.effect, Effect::Deny);
    assert_eq!(result.messages, ["blocked", "stopped", "observed"]);
    assert_eq!(
        *runtime.calls.lock().unwrap(),
        ["robot/stop", "robot/release"]
    );
    assert_eq!(
        result
            .reports
            .iter()
            .map(|r| r.rule.as_str())
            .collect::<Vec<_>>(),
        ["first", "last"]
    );
    assert_eq!(result.reports[1].enforcement, Some(Ok(Effect::Deny)));
    let empty = enforce(&enforcer, [], TriggerTiming::Before, &runtime).await;
    assert_eq!(empty.effect, Effect::Allow);
    assert!(empty.messages.is_empty());
}

#[tokio::test]
async fn passed_requirements_and_evaluation_failures_do_not_invoke_hooks() {
    let program =
        parse(r#"rule r { require false; enforce [call "robot/stop", report "violation"]; }"#)
            .unwrap();
    let enforcer = Enforcer::new(&program);
    let runtime = Runtime::default();
    for (timing, failure_effect) in [
        (TriggerTiming::Before, Effect::Deny),
        (TriggerTiming::While, Effect::Stop),
        (TriggerTiming::After, Effect::Allow),
    ] {
        for failed_evaluation in [false, true] {
            let evaluation = if failed_evaluation {
                Err(evaluator::Error::Timeout { rule: "r".into() })
            } else {
                Ok(RuleResult {
                    requirements: vec![true],
                })
            };
            let result = enforce(&enforcer, [(0, evaluation)], timing, &runtime).await;
            assert_eq!(
                result.effect,
                if failed_evaluation {
                    failure_effect
                } else {
                    Effect::Allow
                }
            );
            assert!(result.messages.is_empty());
            assert!(result.reports[0].enforcement.is_none());
        }
    }
    assert!(runtime.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn hook_failures_apply_timing_effects_and_preserve_reports() {
    let program =
        parse(r#"rule r { require false; enforce [call "robot/stop", report "unsafe", call "robot/reset", allow]; }"#)
            .unwrap();
    let enforcer = Enforcer::new(&program);
    let runtime = Runtime {
        fail: true,
        ..Default::default()
    };
    for (timing, failure_effect) in [
        (TriggerTiming::Before, Effect::Deny),
        (TriggerTiming::While, Effect::Stop),
        (TriggerTiming::After, Effect::Allow),
    ] {
        let result = enforce(
            &enforcer,
            [(
                0,
                Ok(RuleResult {
                    requirements: vec![false],
                }),
            )],
            timing,
            &runtime,
        )
        .await;
        assert_eq!(result.effect, failure_effect);
        assert_eq!(result.messages, ["unsafe"]);
        assert_eq!(
            result.reports[0].enforcement,
            Some(Err(Error::Runtime("dispatch failed".into())))
        );
    }
    assert_eq!(runtime.calls.lock().unwrap().len(), 6);
}

#[test]
fn check_errors_preserve_feedback_and_apply_timing_effects() {
    for (timing, failure_effect) in [
        (TriggerTiming::Before, Effect::Deny),
        (TriggerTiming::While, Effect::Stop),
        (TriggerTiming::After, Effect::Allow),
    ] {
        let mut builder = EnforcementBuilder::default();
        builder.report("retained feedback");
        builder.error(evaluator::Error::Pool("check failed"), timing);
        let result = builder.finish();
        assert_eq!(result.messages, ["retained feedback"]);
        assert_eq!(result.effect, failure_effect);
        assert!(matches!(
            result.reports[0].evaluation,
            Err(evaluator::Error::Pool("check failed"))
        ));
    }
}

#[test]
fn exclusion_outcomes_preserve_admission_feedback() {
    let mut builder = EnforcementBuilder::default();
    builder.report("retained feedback");
    builder.exclusion(Ok(Some(7)));
    let result = builder.finish();
    assert_eq!(result.effect, Effect::Allow);
    assert_eq!(result.admission_id, Some(7));
    assert_eq!(result.messages, ["retained feedback"]);

    let mut builder = EnforcementBuilder::default();
    builder.report("retained feedback");
    builder.exclusion(Err(evaluator::Error::Exclusion {
        capability: "B".into(),
        conflicting: "A".into(),
    }));
    let result = builder.finish();
    assert_eq!(result.effect, Effect::Deny);
    assert!(result.admission_id.is_none());
    assert_eq!(result.messages, ["retained feedback"]);
    assert_eq!(result.reports[0].timing, TriggerTiming::Before);
    assert!(
        matches!(&result.reports[0].evaluation, Err(evaluator::Error::Exclusion { capability, conflicting }) if capability == "B" && conflicting == "A")
    );
}

#[test]
#[should_panic(expected = "recorded capability calls must complete before finalization")]
fn unfinished_calls_cannot_be_finalized() {
    let mut builder = EnforcementBuilder::default();
    builder.call("robot/stop");
    builder.finish();
}
