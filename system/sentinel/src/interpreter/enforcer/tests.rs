use super::*;
use crate::srdl::parse;
use std::sync::Mutex;

/// Records completed runtime operations and can simulate a dispatch failure.
#[derive(Default)]
struct Runtime {
    /// Capability IDs appended only after execution has yielded.
    calls: Mutex<Vec<String>>,
    /// Whether execution should fail after recording the attempted call.
    fail: bool,
}

#[tonic::async_trait]
impl HookRuntime for Runtime {
    async fn call(&self, capability: &str) -> Result<(), Error> {
        tokio::task::yield_now().await;
        self.calls.lock().unwrap().push(capability.to_owned());
        if self.fail {
            Err(Error::Runtime("dispatch failed".into()))
        } else {
            Ok(())
        }
    }
}

async fn enforce(
    enforcer: &Enforcer,
    results: impl IntoIterator<Item = (usize, Result<RuleResult, evaluator::Error>)>,
    timing: TriggerTiming,
    runtime: &dyn HookRuntime,
) -> EnforcementResult {
    let mut builder = EnforcementBuilder::default();
    enforcer.enforce(results, timing, &mut builder);
    runtime.execute(&mut builder, timing).await;
    builder.finish(None)
}

#[tokio::test]
async fn hooks_accumulate_feedback_and_never_reverse_denial() {
    let program = parse(
        r#"
        rule first { require false; enforce allow, deny, report "blocked",
            call "robot/stop", report "stopped", allow, call "robot/release"; }
        rule last { require false; enforce allow, report "observed"; }
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
    );
    assert!(runtime.calls.lock().unwrap().is_empty());
    assert_eq!(builder.calls().len(), 2);
    assert!(builder.calls()[0].result.is_none());
    runtime.execute(&mut builder, TriggerTiming::Before).await;
    let result = builder.finish(None);
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
        parse(r#"rule r { require false; enforce call "robot/stop", report "violation"; }"#)
            .unwrap();
    let enforcer = Enforcer::new(&program);
    let runtime = Runtime::default();
    for timing in [TriggerTiming::Before, TriggerTiming::After] {
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
                if failed_evaluation && timing == TriggerTiming::Before {
                    Effect::Deny
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
async fn hook_failures_deny_before_calls_and_preserve_reports_at_both_timings() {
    let program =
        parse(r#"rule r { require false; enforce call "robot/stop", report "unsafe", call "robot/reset", allow; }"#)
            .unwrap();
    let enforcer = Enforcer::new(&program);
    let runtime = Runtime {
        fail: true,
        ..Default::default()
    };
    for timing in [TriggerTiming::Before, TriggerTiming::After] {
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
        assert_eq!(
            result.effect,
            if timing == TriggerTiming::Before {
                Effect::Deny
            } else {
                Effect::Allow
            }
        );
        assert_eq!(result.messages, ["unsafe"]);
        assert_eq!(
            result.reports[0].enforcement,
            Some(Err(Error::Runtime("dispatch failed".into())))
        );
    }
    assert_eq!(runtime.calls.lock().unwrap().len(), 4);
}

#[test]
fn check_errors_preserve_feedback_and_only_deny_before_admission() {
    for timing in [TriggerTiming::Before, TriggerTiming::After] {
        let mut builder = EnforcementBuilder::default();
        builder.report("retained feedback");
        builder.error(evaluator::Error::Pool("check failed"), timing);
        let result = builder.finish(None);
        assert_eq!(result.messages, ["retained feedback"]);
        assert_eq!(
            result.effect,
            if timing == TriggerTiming::Before {
                Effect::Deny
            } else {
                Effect::Allow
            }
        );
        assert!(matches!(
            result.reports[0].evaluation,
            Err(evaluator::Error::Pool("check failed"))
        ));
    }
}

#[test]
#[should_panic(expected = "recorded capability calls must complete before finalization")]
fn unfinished_calls_cannot_be_finalized() {
    let mut builder = EnforcementBuilder::default();
    builder.call("robot/stop");
    builder.finish(None);
}
