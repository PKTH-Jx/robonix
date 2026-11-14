//! Runtime trigger matching of SRDL rules.

use crate::srdl::ast::*;

/// Selects immutable rule indices for one capability invocation.
pub struct TriggerMatcher;

impl TriggerMatcher {
    pub(super) fn matches(
        trigger: &TriggerClause,
        capability: &str,
        timing: TriggerTiming,
    ) -> bool {
        trigger.timing == timing
            && trigger.capabilities.iter().any(|target| match target {
                TriggerTarget::Always => true,
                TriggerTarget::Capability(name) => name == capability,
            })
    }

    /// Return the indices of all rules triggered by a capability at a given timing, in declaration order.
    pub fn match_indices(program: &Program, capability: &str, timing: TriggerTiming) -> Vec<usize> {
        program
            .rules
            .iter()
            .enumerate()
            .filter_map(|(index, rule)| {
                program
                    .triggers
                    .iter()
                    .any(|trigger| {
                        Self::matches(trigger, capability, timing)
                            && trigger.rules.contains(&rule.name)
                    })
                    .then_some(index)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srdl::parse;

    #[test]
    fn matches_in_source_order_and_keeps_calls_independent() {
        let program = parse(
            r#"
            trigger [alternatives, exact] before ["robot/move", "robot/turn"];
            trigger alternatives before "robot/stop";
            rule exact { require true; enforce allow; }
            rule unrelated { require true; enforce allow; }
            trigger unrelated before "robot/move/other";
            rule alternatives { require true; enforce allow; }
            rule unconditional { require true; enforce allow; }
            rule unused { require true; enforce allow; }
            trigger unconditional before always;
            trigger [unused, alternatives] after ["robot/move", "robot/stop"];
        "#,
        )
        .unwrap();
        crate::interpreter::type_checker::TypeChecker::check(&program).unwrap();
        for (capability, expected) in [
            ("robot/move", vec![0, 2, 3]),
            ("robot/stop", vec![2, 3]),
            ("robot/turn", vec![0, 2, 3]),
            ("robot/move/other", vec![1, 3]),
            ("robot/move", vec![0, 2, 3]),
        ] {
            assert_eq!(
                TriggerMatcher::match_indices(&program, capability, TriggerTiming::Before),
                expected
            );
        }
        assert_eq!(
            TriggerMatcher::match_indices(&program, "robot/move", TriggerTiming::After),
            [2, 4]
        );
        assert!(
            TriggerMatcher::match_indices(&program, "robot/turn", TriggerTiming::After).is_empty()
        );
        let program =
            parse(r#"rule r { require true; enforce deny; } trigger r before "robot/move";"#)
                .unwrap();
        assert!(
            TriggerMatcher::match_indices(&program, "robot/stop", TriggerTiming::Before).is_empty()
        );
        assert!(
            TriggerMatcher::match_indices(&parse("").unwrap(), "robot/move", TriggerTiming::Before)
                .is_empty()
        );
    }
}
