//! Enforcement decisions and structured feedback for completed rule checks.

use serde::{Deserialize, Serialize};

use super::evaluator::{self, RuleResult};
use crate::srdl::ast::{EnforceBehavior, Rule, TriggerTiming};

/// Capability-call decision returned by SRDL enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Deny,
}

/// An enforcement failure, separate from an input or requirement evaluation failure.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("enforce {0:?} is not supported")]
    UnsupportedBehavior(EnforceBehavior),
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
    /// The optional SRDL report text, included only when a requirement was violated.
    pub message: Option<String>,
}

/// Combined decision and rule feedback for one capability call.
#[derive(Debug)]
pub struct EnforcementResult {
    /// Before checks may deny admission; after checks always allow and report their results.
    pub effect: Effect,
    /// All supplied rule results, in the caller's declaration order.
    pub reports: Vec<RuleReport>,
}

impl EnforcementResult {
    /// Pool failures deny admission but cannot change a completed execution's result.
    pub(super) fn failure(message: &'static str, timing: TriggerTiming) -> Self {
        Self {
            effect: if timing == TriggerTiming::Before {
                Effect::Deny
            } else {
                Effect::Allow
            },
            reports: vec![RuleReport {
                rule: "sentinel".into(),
                evaluation: Err(evaluator::Error::Pool(message)),
                enforcement: None,
                message: None,
                timing,
            }],
        }
    }
}

/// Interprets enforce declarations without controlling execution or sending feedback.
pub struct Enforcer;

impl Enforcer {
    /// Consume all matched results in declaration order; an empty set allows the call.
    pub fn enforce<'ast>(
        results: impl IntoIterator<Item = (&'ast Rule, Result<RuleResult, evaluator::Error>)>,
        timing: TriggerTiming,
    ) -> EnforcementResult {
        let mut effect = Effect::Allow;
        let mut reports = Vec::new();
        for (rule, evaluation) in results {
            let (enforcement, message) = match &evaluation {
                Ok(result) if !result.passed() => {
                    let decision = if timing == TriggerTiming::After {
                        None
                    } else {
                        Some(match rule.enforce.behavior {
                            EnforceBehavior::Allow => Ok(Effect::Allow),
                            EnforceBehavior::Deny => Ok(Effect::Deny),
                            behavior @ EnforceBehavior::Pend => {
                                Err(Error::UnsupportedBehavior(behavior))
                            }
                        })
                    };
                    (decision, rule.enforce.report.clone())
                }
                _ => (None, None),
            };
            if timing == TriggerTiming::Before
                && (evaluation.is_err() || matches!(enforcement, Some(Ok(Effect::Deny) | Err(_))))
            {
                effect = Effect::Deny;
            }
            // Keep later feedback even after a denial so the caller sees every matched rule.
            reports.push(RuleReport {
                timing,
                rule: rule.name.0.clone(),
                evaluation,
                enforcement,
                message,
            });
        }
        EnforcementResult { effect, reports }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srdl::parse;

    #[test]
    fn applies_enforce_and_report_only_to_violations() {
        for (behavior, expected) in [
            ("allow", Effect::Allow),
            ("deny", Effect::Deny),
            ("pend", Effect::Deny),
        ] {
            let rule = parse(&format!(
                r#"rule r {{ require true; require false;
                enforce {behavior} report "unsafe"; }}"#
            ))
            .unwrap()
            .rules
            .remove(0);
            for passed in [true, false] {
                let result = RuleResult {
                    requirements: vec![true, passed],
                };
                let mut output = Enforcer::enforce([(&rule, Ok(result))], TriggerTiming::Before);
                assert_eq!(output.effect, if passed { Effect::Allow } else { expected });
                let report = output.reports.remove(0);
                assert_eq!(report.rule, "r");
                let evaluation = report.evaluation.unwrap();
                assert_eq!(evaluation.requirements, [true, passed]);
                assert_eq!(
                    evaluation.failed_requirements(),
                    if passed { vec![] } else { vec![2] }
                );
                let after = Enforcer::enforce(
                    [(
                        &rule,
                        Ok(RuleResult {
                            requirements: vec![true, passed],
                        }),
                    )],
                    TriggerTiming::After,
                );
                assert_eq!(after.effect, Effect::Allow);
                assert_eq!(after.reports[0].timing, TriggerTiming::After);
                assert!(after.reports[0].enforcement.is_none());
                assert_eq!(
                    after.reports[0].message.as_deref(),
                    if passed { None } else { Some("unsafe") }
                );
                if passed {
                    assert!(report.enforcement.is_none());
                    assert!(report.message.is_none());
                } else {
                    assert_eq!(report.message.as_deref(), Some("unsafe"));
                    if behavior == "pend" {
                        assert_eq!(
                            report.enforcement,
                            Some(Err(Error::UnsupportedBehavior(EnforceBehavior::Pend)))
                        );
                    } else {
                        assert_eq!(report.enforcement, Some(Ok(expected)));
                    }
                }
            }
        }
    }

    #[test]
    fn preserves_evaluation_failures_without_applying_enforce() {
        let rule = parse(r#"rule r { require false; enforce allow report "violation"; }"#)
            .unwrap()
            .rules
            .remove(0);
        let failure = evaluator::Error::Expired {
            rule: "r".into(),
            input: "reading".into(),
        };
        let mut output = Enforcer::enforce([(&rule, Err(failure))], TriggerTiming::Before);
        assert_eq!(output.effect, Effect::Deny);
        let report = output.reports.remove(0);
        assert!(
            matches!(report.evaluation, Err(evaluator::Error::Expired { input, .. }) if input == "reading")
        );
        assert!(report.enforcement.is_none());
        assert!(report.message.is_none());
        let after = Enforcer::enforce(
            [(&rule, Err(evaluator::Error::Timeout { rule: "r".into() }))],
            TriggerTiming::After,
        );
        assert_eq!(after.effect, Effect::Allow);
        assert!(after.reports[0].evaluation.is_err());
        assert!(after.reports[0].enforcement.is_none());
        let failure = EnforcementResult::failure("worker stopped", TriggerTiming::After);
        assert_eq!(failure.effect, Effect::Allow);
        assert!(failure.reports[0].evaluation.is_err());
    }

    #[test]
    fn combines_denials_without_losing_feedback_or_changing_order() {
        let program = parse(
            r#"
            rule allow_first { require false; enforce allow; }
            rule deny { require false; enforce deny report "blocked"; }
            rule allow_last { require false; enforce allow report "continue"; }
        "#,
        )
        .unwrap();
        let output = Enforcer::enforce(
            program.rules.iter().map(|rule| {
                (
                    rule,
                    Ok(RuleResult {
                        requirements: vec![false],
                    }),
                )
            }),
            TriggerTiming::Before,
        );
        assert_eq!(output.effect, Effect::Deny);
        assert_eq!(
            output
                .reports
                .iter()
                .map(|report| report.rule.as_str())
                .collect::<Vec<_>>(),
            ["allow_first", "deny", "allow_last"]
        );
        assert_eq!(
            output
                .reports
                .iter()
                .map(|report| report.message.as_deref())
                .collect::<Vec<_>>(),
            [None, Some("blocked"), Some("continue")]
        );
        assert_eq!(output.reports[0].enforcement, Some(Ok(Effect::Allow)));
        assert_eq!(output.reports[2].enforcement, Some(Ok(Effect::Allow)));
        let empty = Enforcer::enforce([], TriggerTiming::Before);
        assert_eq!(empty.effect, Effect::Allow);
        assert!(empty.reports.is_empty());
    }
}
