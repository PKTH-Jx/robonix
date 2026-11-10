//! Static checks over declared binding types, without resolving source field schemas.

use std::collections::BTreeMap;

use crate::srdl::{
    ast::*,
    visitor::{Visitor, walk_rule},
};

/// A static type error located by rule and the affected precondition or postcondition.
#[derive(Debug, PartialEq, Eq)]
pub struct TypeError {
    /// The rule containing the invalid declaration or expression.
    pub rule: String,
    /// The one-based precondition index; None for bindings and postconditions.
    pub requirement: Option<usize>,
    /// The one-based postcondition index; None for bindings and preconditions.
    pub ensure: Option<usize>,
    /// The failed type constraint.
    pub kind: TypeErrorKind,
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rule '{}'", self.rule)?;
        if let Some(requirement) = self.requirement {
            write!(f, ", require #{requirement}")?;
        }
        if let Some(ensure) = self.ensure {
            write!(f, ", ensure #{ensure}")?;
        }
        write!(f, ": {}", self.kind)
    }
}

impl std::error::Error for TypeError {}

/// Constraints enforceable using the three basic binding types.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TypeErrorKind {
    #[error("duplicate binding '{0}'")]
    DuplicateBinding(String),
    #[error("unknown binding '{0}'")]
    UnknownBinding(String),
    #[error("require must be boolean, found {0:?}")]
    NonBooleanRequire(InputType),
    #[error("ensure must be boolean, found {0:?}")]
    NonBooleanEnsure(InputType),
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
    /// The current precondition index, cleared when checking postconditions.
    requirement: Option<usize>,
    /// The current postcondition index, cleared when checking preconditions.
    ensure: Option<usize>,
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
            ensure: self.ensure,
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

    fn visit_rule(&mut self, rule: &Rule) -> Result<(), TypeError> {
        self.bindings.clear();
        self.rule.clone_from(&rule.name.0);
        self.requirement = None;
        self.ensure = None;
        walk_rule(self, rule)
    }

    fn visit_input_field(&mut self, input: &InputField) -> Result<(), TypeError> {
        self.declare(&input.name, input.data_type)
    }

    fn visit_context(&mut self, context: &ContextClause) -> Result<(), TypeError> {
        self.declare(&context.name, context.data_type)
    }

    fn visit_require(&mut self, require: &RequireClause) -> Result<(), TypeError> {
        self.ensure = None;
        self.requirement = Some(self.requirement.unwrap_or(0) + 1);
        let actual = self.infer_expression(&require.expression)?;
        if actual != InputType::Boolean {
            return Err(self.error(TypeErrorKind::NonBooleanRequire(actual)));
        }
        Ok(())
    }

    fn visit_ensure(&mut self, ensure: &EnsureClause) -> Result<(), TypeError> {
        self.requirement = None;
        self.ensure = Some(self.ensure.unwrap_or(0) + 1);
        let actual = self.infer_expression(&ensure.expression)?;
        if actual != InputType::Boolean {
            return Err(self.error(TypeErrorKind::NonBooleanEnsure(actual)));
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
            trigger always; enforce deny;
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
    fn checks_postcondition_types_and_bindings_independent_of_source_order() {
        let source = |expression| {
            format!(
                r#"rule typed {{
                ensure {expression};
                require true;
                trigger always;
                context boolean ready = arguments.ready;
                input from "source" select number x, string label;
                enforce deny;
            }}"#
            )
        };
        for expression in ["ready", "x > 0 && label = \"held\"", "false"] {
            TypeChecker::check(&parse(&source(expression)).unwrap()).unwrap();
        }
        for (expression, expected) in [
            ("x", TypeErrorKind::NonBooleanEnsure(InputType::Number)),
            ("label", TypeErrorKind::NonBooleanEnsure(InputType::String)),
            ("missing", TypeErrorKind::UnknownBinding("missing".into())),
            (
                "ready + x > 0",
                TypeErrorKind::InvalidBinary {
                    operator: BinaryOperator::Add,
                    left: InputType::Boolean,
                    right: InputType::Number,
                },
            ),
        ] {
            let error = TypeChecker::check(&parse(&source(expression)).unwrap()).unwrap_err();
            assert_eq!(error.kind, expected, "{expression}");
            assert_eq!(error.requirement, None);
            assert_eq!(error.ensure, Some(1));
            assert!(error.to_string().starts_with("rule 'typed', ensure #1:"));
        }
    }

    #[test]
    fn numbers_postconditions_separately_and_resets_between_rules() {
        let program = parse(
            "rule first { trigger always; require true; ensure true; enforce deny; }
             rule second { trigger always; require true; require false;
                 ensure true; ensure missing; enforce deny; }",
        )
        .unwrap();
        let error = TypeChecker::check(&program).unwrap_err();
        assert_eq!(error.requirement, None);
        assert_eq!(error.ensure, Some(2));
        assert_eq!(
            error.to_string(),
            "rule 'second', ensure #2: unknown binding 'missing'"
        );
    }

    #[test]
    fn isolates_rule_scopes_and_reports_requirement_number() {
        let first = r#"rule first { trigger always;
            input from "source" select number x = value maxage 1;
            require x > 0; ensure true; enforce deny; }"#;
        let second = "rule second { trigger always; require true; require x > 0; enforce deny; }";
        let program = parse(&format!("{first} {second}")).unwrap();
        assert_eq!(
            TypeChecker::check(&program).unwrap_err(),
            TypeError {
                rule: "second".into(),
                requirement: Some(2),
                ensure: None,
                kind: TypeErrorKind::UnknownBinding("x".into()),
            }
        );
        let second = r#"rule second { trigger always;
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
                "rule r {{ require {expression}; trigger always; {declarations} enforce deny; }}"
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
                "rule r {{ trigger always; {declarations} require true; enforce deny; }}"
            ))
            .unwrap();
            let error = TypeChecker::check(&program).unwrap_err();
            assert_eq!(error.kind, TypeErrorKind::DuplicateBinding("user".into()));
            assert_eq!(error.requirement, None);
        }
        let program = parse(r#"rule first { trigger always; context string user = identity.user_id; require true; enforce deny; }
            rule second { trigger always; require user = "alice"; enforce deny; }"#).unwrap();
        assert_eq!(
            TypeChecker::check(&program).unwrap_err().kind,
            TypeErrorKind::UnknownBinding("user".into())
        );
    }

    #[test]
    fn rejects_duplicate_inputs_in_constructed_ast() {
        let mut program = parse(
            r#"rule duplicate { trigger always;
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
                ensure: None,
                kind: TypeErrorKind::DuplicateBinding("x".into()),
            }
        );
    }
}
