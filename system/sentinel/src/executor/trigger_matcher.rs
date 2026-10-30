//! Runtime trigger matching of SRDL rules.

use std::convert::Infallible;

use super::visitor::Visitor;
use crate::srdl::ast::*;

/// Collects rules whose triggers match a capability call, in declaration order.
pub struct TriggerMatcher<'ast> {
    /// The target capability contract ID used for exact matching.
    capability: String,
    /// Matching rules, with each rule included at most once.
    enabled_rules: Vec<&'ast Rule>,
}

impl<'ast> TriggerMatcher<'ast> {
    /// Match a program against one call without binding inputs or checking requirements.
    pub fn match_rules(program: &'ast Program, capability: &str) -> Vec<&'ast Rule> {
        let mut matcher = Self {
            capability: capability.to_owned(),
            enabled_rules: Vec::new(),
        };
        matcher.visit_program(program).unwrap();
        matcher.enabled_rules
    }
}

impl<'ast> Visitor<'ast> for TriggerMatcher<'ast> {
    type Error = Infallible;

    fn visit_rule(&mut self, rule: &'ast Rule) -> Result<(), Self::Error> {
        // Matching only needs triggers; visiting inputs and requirements is unnecessary.
        if rule.triggers.iter().any(|trigger| match trigger {
            TriggerClause::Always => true,
            TriggerClause::Capability(name) => name == &self.capability,
        }) {
            self.enabled_rules.push(rule);
        }
        Ok(())
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
            ("robot/move", vec!["exact", "alternatives", "unconditional"]),
            ("robot/stop", vec!["alternatives", "unconditional"]),
            ("robot/move", vec!["exact", "alternatives", "unconditional"]),
        ] {
            let matched = TriggerMatcher::match_rules(&program, capability);
            assert_eq!(
                matched
                    .iter()
                    .map(|rule| rule.name.0.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
        let program =
            parse(r#"rule r { trigger capability "robot/move"; require true; enforce deny; }"#)
                .unwrap();
        assert!(TriggerMatcher::match_rules(&program, "robot/stop").is_empty());
        assert!(TriggerMatcher::match_rules(&parse("").unwrap(), "robot/move").is_empty());
    }
}
