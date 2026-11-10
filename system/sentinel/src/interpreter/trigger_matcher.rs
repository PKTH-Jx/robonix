//! Runtime trigger matching of SRDL rules.

use crate::srdl::ast::*;

/// Selects immutable rule indices for one capability invocation.
pub struct TriggerMatcher;

impl TriggerMatcher {
    pub fn match_indices(program: &Program, capability: &str) -> Vec<usize> {
        program
            .rules
            .iter()
            .enumerate()
            .filter_map(|(index, rule)| {
                rule.triggers
                    .iter()
                    .any(|trigger| match trigger {
                        TriggerClause::Always => true,
                        TriggerClause::Capability(name) => name == capability,
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
    fn matches_once_in_order_and_keeps_calls_independent() {
        let program = parse(
            r#"
            rule exact { trigger capability "robot/move"; require true; enforce deny; }
            rule unrelated { trigger capability "robot/move/other"; require true; enforce deny; }
            rule alternatives {
                trigger capability "robot/stop";
                trigger capability "robot/move";
                trigger capability "robot/move";
                trigger always; require true; enforce deny;
            }
            rule unconditional { trigger always; require true; enforce deny; }
        "#,
        )
        .unwrap();
        for (capability, expected) in [
            ("robot/move", vec![0, 2, 3]),
            ("robot/stop", vec![2, 3]),
            ("robot/move", vec![0, 2, 3]),
        ] {
            assert_eq!(
                TriggerMatcher::match_indices(&program, capability),
                expected
            );
        }
        let program =
            parse(r#"rule r { trigger capability "robot/move"; require true; enforce deny; }"#)
                .unwrap();
        assert!(TriggerMatcher::match_indices(&program, "robot/stop").is_empty());
        assert!(TriggerMatcher::match_indices(&parse("").unwrap(), "robot/move").is_empty());
    }
}
