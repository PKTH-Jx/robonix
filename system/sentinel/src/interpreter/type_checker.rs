//! Static checks for trigger references and binding types, without resolving source schemas.

use std::collections::BTreeMap;

use super::enforcer::hooks;

use crate::srdl::{
    ast::*,
    visitor::{Visitor, walk_program, walk_rule},
};

/// A static type error located by rule and condition.
#[derive(Debug, PartialEq, Eq)]
pub struct TypeError {
    /// The rule containing the invalid declaration or expression.
    pub rule: String,
    /// The one-based condition index; None for declarations.
    pub requirement: Option<usize>,
    /// The failed type constraint.
    pub kind: TypeErrorKind,
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rule '{}'", self.rule)?;
        if let Some(requirement) = self.requirement {
            write!(f, ", require #{requirement}")?;
        }
        write!(f, ": {}", self.kind)
    }
}

impl std::error::Error for TypeError {}

/// Invalid rule references, declarations, or operations on the three basic binding types.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TypeErrorKind {
    #[error("duplicate rule '{0}'")]
    DuplicateRule(String),
    #[error("trigger references unknown rule '{0}'")]
    UnknownRule(String),
    #[error("hook '{hook}' is not valid for {timing:?} triggers")]
    InvalidHookTiming {
        /// Built-in response whose timing constraint was violated.
        hook: &'static str,
        /// Trigger lifecycle event incompatible with the hook.
        timing: TriggerTiming,
    },
    #[error("{0}")]
    InvalidHook(String),
    #[error("duplicate binding '{0}'")]
    DuplicateBinding(String),
    #[error("unknown binding '{0}'")]
    UnknownBinding(String),
    #[error("require must be boolean, found {0:?}")]
    NonBooleanRequire(InputType),
    #[error("unary operator {operator:?} does not accept {operand:?}")]
    InvalidUnary {
        /// The prefix operation being checked.
        operator: UnaryOperator,
        /// The inferred operand type.
        operand: InputType,
    },
    #[error("binary operator {operator:?} does not accept {left:?} and {right:?}")]
    InvalidBinary {
        /// The operation being checked.
        operator: BinaryOperator,
        /// The inferred left operand type.
        left: InputType,
        /// The inferred right operand type.
        right: InputType,
    },
    #[error("cannot type-check {0} without type information")]
    UnsupportedExpression(&'static str),
}

/// Infers expression types using one shared input and context scope per rule.
#[derive(Default)]
pub struct TypeChecker {
    /// Input and context aliases with their declared types in the current rule.
    bindings: BTreeMap<String, InputType>,
    /// The rule name used in diagnostics.
    rule: String,
    /// The current condition index within this rule.
    requirement: Option<usize>,
}

impl TypeChecker {
    /// Check a complete program, returning the first static type error.
    pub fn check(program: &Program) -> Result<(), TypeError> {
        Self::default().visit_program(program)
    }

    fn error(&self, kind: TypeErrorKind) -> TypeError {
        TypeError {
            rule: self.rule.clone(),
            requirement: self.requirement,
            kind,
        }
    }

    fn declare(&mut self, name: &Identifier, data_type: InputType) -> Result<(), TypeError> {
        if self.bindings.insert(name.0.clone(), data_type).is_some() {
            return Err(self.error(TypeErrorKind::DuplicateBinding(name.0.clone())));
        }
        Ok(())
    }

    fn infer_expression(&self, expression: &Expression) -> Result<InputType, TypeError> {
        use BinaryOperator as B;
        use InputType as T;
        match expression {
            Expression::Literal(Literal::Number(_)) => Ok(T::Number),
            Expression::Literal(Literal::String(_)) => Ok(T::String),
            Expression::Literal(Literal::Boolean(_)) => Ok(T::Boolean),
            Expression::Identifier(Identifier(name)) => self
                .bindings
                .get(name)
                .copied()
                .ok_or_else(|| self.error(TypeErrorKind::UnknownBinding(name.clone()))),
            Expression::Unary { operator, operand } => {
                let operand_type = self.infer_expression(operand)?;
                match (operator, operand_type) {
                    (UnaryOperator::Negate, T::Number) => Ok(T::Number),
                    (UnaryOperator::Not, T::Boolean) => Ok(T::Boolean),
                    _ => Err(self.error(TypeErrorKind::InvalidUnary {
                        operator: *operator,
                        operand: operand_type,
                    })),
                }
            }
            Expression::Binary {
                operator,
                left,
                right,
            } => {
                let left_type = self.infer_expression(left)?;
                let right_type = self.infer_expression(right)?;
                match operator {
                    B::Add | B::Subtract | B::Multiply | B::Divide | B::Remainder
                        if left_type == T::Number && right_type == T::Number =>
                    {
                        Ok(T::Number)
                    }
                    B::Lt | B::Lte | B::Gt | B::Gte
                        if left_type == T::Number && right_type == T::Number =>
                    {
                        Ok(T::Boolean)
                    }
                    B::And | B::Or if left_type == T::Boolean && right_type == T::Boolean => {
                        Ok(T::Boolean)
                    }
                    B::Eq | B::Neq if left_type == right_type => Ok(T::Boolean),
                    _ => Err(self.error(TypeErrorKind::InvalidBinary {
                        operator: *operator,
                        left: left_type,
                        right: right_type,
                    })),
                }
            }
            Expression::Field { .. } => {
                Err(self.error(TypeErrorKind::UnsupportedExpression("field access")))
            }
            Expression::Call { .. } => {
                Err(self.error(TypeErrorKind::UnsupportedExpression("function calls")))
            }
        }
    }
}

impl<'ast> Visitor<'ast> for TypeChecker {
    type Error = TypeError;

    fn visit_program(&mut self, program: &Program) -> Result<(), TypeError> {
        self.requirement = None;
        let mut names = BTreeMap::new();
        for rule in &program.rules {
            if names.insert(&rule.name.0, rule).is_some() {
                self.rule.clone_from(&rule.name.0);
                return Err(self.error(TypeErrorKind::DuplicateRule(rule.name.0.clone())));
            }
        }
        // Resolve after collecting all definitions so triggers may precede their rules.
        for trigger in &program.triggers {
            for name in &trigger.rules {
                let Some(rule) = names.get(&name.0) else {
                    self.rule.clone_from(&name.0);
                    return Err(self.error(TypeErrorKind::UnknownRule(name.0.clone())));
                };
                for hook in hooks::bind(&rule.enforce) {
                    if !hook.timings().contains(&trigger.timing) {
                        self.rule.clone_from(&name.0);
                        return Err(self.error(TypeErrorKind::InvalidHookTiming {
                            hook: hook.name(),
                            timing: trigger.timing,
                        }));
                    }
                }
            }
        }
        walk_program(self, program)
    }

    fn visit_rule(&mut self, rule: &Rule) -> Result<(), TypeError> {
        self.bindings.clear();
        self.rule.clone_from(&rule.name.0);
        self.requirement = None;
        walk_rule(self, rule)
    }

    fn visit_enforce(&mut self, enforce: &EnforceClause) -> Result<(), TypeError> {
        self.requirement = None;
        for hook in hooks::bind(enforce) {
            hook.validate()
                .map_err(|error| self.error(TypeErrorKind::InvalidHook(error.to_string())))?;
        }
        Ok(())
    }

    fn visit_input_field(&mut self, input: &InputField) -> Result<(), TypeError> {
        self.declare(&input.name, input.data_type)
    }

    fn visit_context(&mut self, context: &ContextClause) -> Result<(), TypeError> {
        self.declare(&context.name, context.data_type)
    }

    fn visit_require(&mut self, require: &RequireClause) -> Result<(), TypeError> {
        self.requirement = Some(self.requirement.unwrap_or(0) + 1);
        let actual = self.infer_expression(&require.expression)?;
        if actual != InputType::Boolean {
            return Err(self.error(TypeErrorKind::NonBooleanRequire(actual)));
        }
        Ok(())
    }

    fn visit_expression(&mut self, expression: &Expression) -> Result<(), TypeError> {
        self.infer_expression(expression).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srdl::parse;

    fn check(expression: &str) -> Result<(), TypeError> {
        TypeChecker::check(
            &parse(&format!(
                r#"rule typed {{
            require {expression};
            input from "source" select number x = value maxage 1,
                number y = value maxage 1, string label = label maxage 1,
                boolean ready = ready maxage 1;
            enforce deny;
        }}"#
            ))
            .unwrap(),
        )
    }

    #[test]
    fn checks_basic_types_operators_and_source_order_independence() {
        TypeChecker::check(&parse(include_str!("../../test.srdl")).unwrap()).unwrap();
        TypeChecker::check(&parse("").unwrap()).unwrap();
        for expression in ["!ready", "-x < y", "x < y && (ready || label = \"active\")"] {
            check(expression).unwrap();
        }
        for operator in ["+", "-", "*", "/", "%", "<", "<=", ">", ">="] {
            let expression = format!("x {operator} y");
            check(&format!("({expression}) = ({expression})")).unwrap();
            assert!(matches!(
                check(&format!("x {operator} ready")).unwrap_err().kind,
                TypeErrorKind::InvalidBinary {
                    left: InputType::Number,
                    right: InputType::Boolean,
                    ..
                }
            ));
        }
        for operator in ["<", "<=", ">", ">="] {
            assert!(matches!(
                check(&format!("label {operator} \"other\""))
                    .unwrap_err()
                    .kind,
                TypeErrorKind::InvalidBinary { .. }
            ));
        }
        for operator in ["=", "!="] {
            for (left, right) in [("x", "y"), ("label", "\"active\""), ("ready", "false")] {
                check(&format!("{left} {operator} {right}")).unwrap();
            }
            assert!(matches!(
                check(&format!("x {operator} label")).unwrap_err().kind,
                TypeErrorKind::InvalidBinary { .. }
            ));
        }
        for operator in ["&&", "||"] {
            check(&format!("ready {operator} true")).unwrap();
            assert!(matches!(
                check(&format!("ready {operator} x")).unwrap_err().kind,
                TypeErrorKind::InvalidBinary { .. }
            ));
        }
    }

    #[test]
    fn resolves_trigger_references_and_rejects_ambiguous_rule_names() {
        let definitions =
            "rule a { require true; enforce deny; } rule b { require true; enforce deny; }";
        let program = parse(&format!(
            "trigger [b, a, a] before [capA, capB]; {definitions} trigger b before always;"
        ))
        .unwrap();
        TypeChecker::check(&program).unwrap();
        for (source, name, kind) in [
            (
                "trigger missing before capA;".to_owned(),
                "missing",
                TypeErrorKind::UnknownRule("missing".into()),
            ),
            (
                format!("{definitions} trigger [a, missing] before capA;"),
                "missing",
                TypeErrorKind::UnknownRule("missing".into()),
            ),
            (
                format!("{definitions} rule a {{ require true; enforce deny; }}"),
                "a",
                TypeErrorKind::DuplicateRule("a".into()),
            ),
        ] {
            assert_eq!(
                TypeChecker::check(&parse(&source).unwrap()).unwrap_err(),
                TypeError {
                    rule: name.into(),
                    requirement: None,
                    kind,
                }
            );
        }
    }

    #[test]
    fn rejects_invalid_unary_non_boolean_and_untyped_expressions() {
        for (expression, expected) in [
            (
                "!x",
                TypeErrorKind::InvalidUnary {
                    operator: UnaryOperator::Not,
                    operand: InputType::Number,
                },
            ),
            (
                "-ready",
                TypeErrorKind::InvalidUnary {
                    operator: UnaryOperator::Negate,
                    operand: InputType::Boolean,
                },
            ),
            ("x + 1", TypeErrorKind::NonBooleanRequire(InputType::Number)),
            ("label", TypeErrorKind::NonBooleanRequire(InputType::String)),
            (
                "ready()",
                TypeErrorKind::UnsupportedExpression("function calls"),
            ),
            (
                "x.value > 0",
                TypeErrorKind::UnsupportedExpression("field access"),
            ),
            (
                "true || missing",
                TypeErrorKind::UnknownBinding("missing".into()),
            ),
        ] {
            assert_eq!(
                check(expression).unwrap_err().kind,
                expected,
                "{expression}"
            );
        }
        assert_eq!(
            check("ready()").unwrap_err().to_string(),
            "rule 'typed', require #1: cannot type-check function calls without type information"
        );
    }

    #[test]
    fn checks_after_rules_with_the_same_condition_types_and_scope() {
        for (condition, valid) in [
            ("ready", true),
            ("ready + 1 > 0", false),
            ("missing", false),
            ("1", false),
        ] {
            let program = parse(&format!(
                "trigger r after capA; rule r {{ context boolean ready = arguments.ready; require {condition}; enforce allow; }}"
            )).unwrap();
            assert_eq!(TypeChecker::check(&program).is_ok(), valid, "{condition}");
        }
    }

    #[test]
    fn validates_hook_arguments_and_every_trigger_binding() {
        for source in [
            "trigger r after A; rule r { require true; enforce deny; }",
            "trigger r before A; trigger r after B; rule r { require true; enforce deny; }",
            "rule r { require true; enforce deny; } trigger [r] after always;",
            "trigger r after A; rule r { require true; enforce allow, report \"bad timing\", deny; }",
        ] {
            let error = TypeChecker::check(&parse(source).unwrap()).unwrap_err();
            assert_eq!(error.rule, "r");
            assert_eq!(error.requirement, None);
            assert_eq!(
                error.kind,
                TypeErrorKind::InvalidHookTiming {
                    hook: "deny",
                    timing: TriggerTiming::After,
                }
            );
        }
        for behavior in [
            "allow",
            r#"call "robot/stop""#,
            r#"report "observed", call "robot/stop", allow"#,
        ] {
            TypeChecker::check(&parse(&format!(
                "trigger r before A; trigger r after B; rule r {{ require false; enforce {behavior}; }}"
            )).unwrap()).unwrap();
        }
        for behavior in [r#"call """#, r#"call "  ""#] {
            assert!(matches!(
                TypeChecker::check(
                    &parse(&format!("rule r {{ require false; enforce {behavior}; }}")).unwrap()
                )
                .unwrap_err()
                .kind,
                TypeErrorKind::InvalidHook(_)
            ));
        }
    }

    #[test]
    fn isolates_rule_scopes_and_reports_requirement_number() {
        let first = r#"rule first {
            input from "source" select number x = value maxage 1;
            require x > 0; enforce deny; }"#;
        let second = "rule second { require true; require x > 0; enforce deny; }";
        let program = parse(&format!("{first} {second}")).unwrap();
        assert_eq!(
            TypeChecker::check(&program).unwrap_err(),
            TypeError {
                rule: "second".into(),
                requirement: Some(2),
                kind: TypeErrorKind::UnknownBinding("x".into()),
            }
        );
        let second = r#"rule second {
            input from "source" select boolean x = value maxage 1;
            require x; enforce deny; }"#;
        TypeChecker::check(&parse(&format!("{first} {second}")).unwrap()).unwrap();
    }

    #[test]
    fn checks_context_types_and_a_shared_scope_independent_of_source_order() {
        let declarations = r#"context string user = identity.user_id;
            context boolean ready = identity.ready;
            context number limit = arguments.limit;
            input from "source" select number value maxage 1;"#;
        for (expression, valid) in [
            ("user = \"alice\" && ready && value < limit", true),
            ("user + 1 > 0", false),
            ("ready = limit", false),
            ("limit", false),
        ] {
            let program = parse(&format!(
                "rule r {{ require {expression}; {declarations} enforce deny; }}"
            ))
            .unwrap();
            let result = TypeChecker::check(&program);
            assert_eq!(result.is_ok(), valid, "{expression}");
            if let Err(error) = result {
                assert_eq!(error.requirement, Some(1));
            }
        }
        for declarations in [
            "context string user = identity.user_id; context boolean user = identity.ready;",
            r#"context string user = identity.user_id; input from "source" select number user maxage 1;"#,
            r#"input from "source" select number user maxage 1; context string user = identity.user_id;"#,
        ] {
            let program = parse(&format!(
                "rule r {{ {declarations} require true; enforce deny; }}"
            ))
            .unwrap();
            let error = TypeChecker::check(&program).unwrap_err();
            assert_eq!(error.kind, TypeErrorKind::DuplicateBinding("user".into()));
            assert_eq!(error.requirement, None);
        }
        let program = parse(
            r#"rule first { context string user = identity.user_id; require true; enforce deny; }
            rule second { require user = "alice"; enforce deny; }"#,
        )
        .unwrap();
        assert_eq!(
            TypeChecker::check(&program).unwrap_err().kind,
            TypeErrorKind::UnknownBinding("user".into())
        );
    }

    #[test]
    fn rejects_duplicate_inputs_in_constructed_ast() {
        let mut program = parse(
            r#"rule duplicate {
            input from "source" select number x = value maxage 1, string y = value maxage 1;
            require true; enforce deny; }"#,
        )
        .unwrap();
        program.rules[0].inputs[0].fields[1].name = Identifier("x".into());
        assert_eq!(
            TypeChecker::check(&program).unwrap_err(),
            TypeError {
                rule: "duplicate".into(),
                requirement: None,
                kind: TypeErrorKind::DuplicateBinding("x".into()),
            }
        );
    }
}
