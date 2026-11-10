//! Runtime trigger matching of SRDL rules.

use std::collections::BTreeSet;

use crate::srdl::ast::*;

/// Selects immutable rule indices for one capability invocation.
pub struct TriggerMatcher;

impl TriggerMatcher {
    pub fn match_indices(program: &Program, capability: &str) -> Vec<usize> {
        let matched: BTreeSet<_> = program
            .triggers
            .iter()
            .filter(|trigger| {
                trigger.capabilities.iter().any(|target| match target {
                    TriggerTarget::Always => true,
                    TriggerTarget::Capability(name) => name == capability,
                })
            })
            .flat_map(|trigger| trigger.rules.iter().map(|name| name.0.as_str()))
            .collect();
        program
            .rules
            .iter()
            .enumerate()
            .filter_map(|(index, rule)| matched.contains(rule.name.0.as_str()).then_some(index))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srdl::parse;

    #[test]
    fn matches_once_in_order_and_keeps_calls_independent() {
        let program = parse(
            r#"
            trigger [alternatives, exact] @ ["robot/move", "robot/turn", "robot/move"];
            trigger alternatives @ ["robot/stop", "robot/move"];
            rule exact { require true; enforce deny; }
            rule unrelated { require true; enforce deny; }
            trigger unrelated @ "robot/move/other";
            rule alternatives { require true; enforce deny; }
            rule unconditional { require true; enforce deny; }
            rule unused { require true; enforce deny; }
            trigger unconditional @ always;
        "#,
        )
        .unwrap();
        for (capability, expected) in [
            ("robot/move", vec![0, 2, 3]),
            ("robot/stop", vec![2, 3]),
            ("robot/turn", vec![0, 2, 3]),
            ("robot/move/other", vec![1, 3]),
            ("robot/move", vec![0, 2, 3]),
        ] {
            assert_eq!(
                TriggerMatcher::match_indices(&program, capability),
                expected
            );
        }
        let program =
            parse(r#"rule r { require true; enforce deny; } trigger r @ "robot/move";"#).unwrap();
        assert!(TriggerMatcher::match_indices(&program, "robot/stop").is_empty());
        assert!(TriggerMatcher::match_indices(&parse("").unwrap(), "robot/move").is_empty());
    }
}
