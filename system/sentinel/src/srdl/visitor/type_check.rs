//! Static checks over declared input types, without resolving capability response schemas.

use std::collections::BTreeMap;

use super::{Visitor, walk_rule};
use crate::srdl::ast::*;

/// A static type error located by rule and, when applicable, requirement number.
#[derive(Debug, PartialEq, Eq)]
pub struct TypeError {
    /// The rule containing the invalid declaration or expression.
    pub rule: String,
    /// The one-based requirement number; None identifies an input declaration error.
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

/// Constraints enforceable using the three basic input types.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TypeErrorKind {
    #[error("duplicate input '{0}'")]
    DuplicateInput(String),
    #[error("unknown input '{0}'")]
    UnknownInput(String),
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

/// Infers expression types using an independent input scope for each rule.
#[derive(Default)]
pub struct TypeChecker {
    /// The input names and their declared types in the current rule.
    inputs: BTreeMap<String, InputType>,
    /// The rule name used in diagnostics.
    rule: String,
    /// The requirement currently being checked, if declarations are complete.
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
}

impl Visitor for TypeChecker {
    type Error = TypeError;
    type ExpressionOutput = InputType;

    fn visit_rule(&mut self, rule: &Rule) -> Result<(), TypeError> {
        self.inputs.clear();
        self.rule.clone_from(&rule.name.0);
        self.requirement = None;
        walk_rule(self, rule)
    }

    fn visit_input(&mut self, input: &InputClause) -> Result<(), TypeError> {
        if self
            .inputs
            .insert(input.name.0.clone(), input.data_type)
            .is_some()
        {
            return Err(self.error(TypeErrorKind::DuplicateInput(input.name.0.clone())));
        }
        Ok(())
    }

    fn visit_require(&mut self, require: &RequireClause) -> Result<(), TypeError> {
        self.requirement = Some(self.requirement.unwrap_or(0) + 1);
        let actual = self.visit_expression(&require.expression)?;
        if actual != InputType::Boolean {
            return Err(self.error(TypeErrorKind::NonBooleanRequire(actual)));
        }
        Ok(())
    }

    fn visit_expression(&mut self, expression: &Expression) -> Result<InputType, TypeError> {
        use BinaryOperator as B;
        use InputType as T;
        match expression {
            Expression::Literal(Literal::Number(_)) => Ok(T::Number),
            Expression::Literal(Literal::String(_)) => Ok(T::String),
            Expression::Literal(Literal::Boolean(_)) => Ok(T::Boolean),
            Expression::Identifier(Identifier(name)) => self
                .inputs
                .get(name)
                .copied()
                .ok_or_else(|| self.error(TypeErrorKind::UnknownInput(name.clone()))),
            Expression::Unary { operator, operand } => {
                let operand_type = self.visit_expression(operand)?;
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
                let left_type = self.visit_expression(left)?;
                let right_type = self.visit_expression(right)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srdl::parse;

    fn check_requirement(expression: &str) -> Result<(), TypeError> {
        let program = parse(&format!(
            r#"
            rule typed {{
                require {expression};
                input number x from "source" select value maxage 1;
                input number y from "source" select value maxage 1;
                input string label from "source" select label maxage 1;
                input boolean ready from "source" select ready maxage 1;
                trigger always;
                enforce deny;
            }}
        "#
        ))
        .unwrap();
        TypeChecker::check(&program)
    }

    #[test]
    fn accepts_basic_types_and_source_order_independence() {
        for expression in [
            "true",
            "ready",
            "!ready",
            "ready = false",
            "label != \"blocked\"",
            "(x + y * 2 - 1) / 2 % 3 <= -y",
            "x < y && (ready || label = \"active\")",
        ] {
            assert!(check_requirement(expression).is_ok(), "{expression}");
        }
        TypeChecker::check(&parse(include_str!("../../../test.srdl")).unwrap()).unwrap();
        TypeChecker::check(&parse("").unwrap()).unwrap();
    }

    #[test]
    fn checks_each_binary_operator_without_coercion() {
        for operator in ["+", "-", "*", "/", "%"] {
            check_requirement(&format!("x {operator} y = 1")).unwrap();
            assert!(matches!(
                check_requirement(&format!("x {operator} ready = 1"))
                    .unwrap_err()
                    .kind,
                TypeErrorKind::InvalidBinary {
                    left: InputType::Number,
                    right: InputType::Boolean,
                    ..
                }
            ));
        }
        for operator in ["<", "<=", ">", ">="] {
            check_requirement(&format!("x {operator} y")).unwrap();
            assert!(matches!(
                check_requirement(&format!("label {operator} \"other\""))
                    .unwrap_err()
                    .kind,
                TypeErrorKind::InvalidBinary { .. }
            ));
        }
        for operator in ["=", "!="] {
            for operands in [("x", "y"), ("label", "\"active\""), ("ready", "false")] {
                check_requirement(&format!("{} {operator} {}", operands.0, operands.1)).unwrap();
            }
            assert!(matches!(
                check_requirement(&format!("x {operator} label"))
                    .unwrap_err()
                    .kind,
                TypeErrorKind::InvalidBinary { .. }
            ));
        }
        for operator in ["&&", "||"] {
            check_requirement(&format!("ready {operator} true")).unwrap();
            assert!(matches!(
                check_requirement(&format!("ready {operator} x"))
                    .unwrap_err()
                    .kind,
                TypeErrorKind::InvalidBinary { .. }
            ));
        }
    }

    #[test]
    fn rejects_invalid_unary_operators_and_non_boolean_requirements() {
        assert_eq!(
            check_requirement("!x").unwrap_err().kind,
            TypeErrorKind::InvalidUnary {
                operator: UnaryOperator::Not,
                operand: InputType::Number,
            }
        );
        assert_eq!(
            check_requirement("-ready").unwrap_err().kind,
            TypeErrorKind::InvalidUnary {
                operator: UnaryOperator::Negate,
                operand: InputType::Boolean,
            }
        );
        for (expression, actual) in [("x + 1", InputType::Number), ("label", InputType::String)] {
            assert_eq!(
                check_requirement(expression).unwrap_err().kind,
                TypeErrorKind::NonBooleanRequire(actual)
            );
        }
    }

    #[test]
    fn reports_rule_and_requirement_without_leaking_input_scopes() {
        let program = parse(
            r#"
            rule first {
                trigger always;
                input number x from "source" select value maxage 1;
                require x > 0;
                enforce deny;
            }
            rule second {
                trigger always;
                require true;
                require x > 0;
                enforce deny;
            }
        "#,
        )
        .unwrap();
        assert_eq!(
            TypeChecker::check(&program).unwrap_err(),
            TypeError {
                rule: "second".into(),
                requirement: Some(2),
                kind: TypeErrorKind::UnknownInput("x".into()),
            }
        );
        let program = parse(
            r#"
            rule first {
                trigger always;
                input number x from "source" select value maxage 1;
                require x > 0; enforce deny;
            }
            rule second {
                trigger always;
                input boolean x from "source" select value maxage 1;
                require x; enforce deny;
            }
        "#,
        )
        .unwrap();
        TypeChecker::check(&program).unwrap();
    }

    #[test]
    fn rejects_untyped_function_and_field_expressions() {
        assert_eq!(
            check_requirement("ready()").unwrap_err().to_string(),
            "rule 'typed', require #1: cannot type-check function calls without type information"
        );
        assert_eq!(
            check_requirement("ready()").unwrap_err().kind,
            TypeErrorKind::UnsupportedExpression("function calls")
        );
        assert_eq!(
            check_requirement("x.value > 0").unwrap_err().kind,
            TypeErrorKind::UnsupportedExpression("field access")
        );
        assert_eq!(
            check_requirement("true || missing").unwrap_err().kind,
            TypeErrorKind::UnknownInput("missing".into())
        );
    }

    #[test]
    fn rejects_duplicate_inputs_in_a_constructed_ast() {
        let mut program = parse(
            r#"rule duplicate {
            trigger always;
            input number x from "source" select value maxage 1;
            input string y from "source" select value maxage 1;
            require true; enforce deny;
        }"#,
        )
        .unwrap();
        program.rules[0].inputs[1].name = Identifier("x".into());
        assert_eq!(
            TypeChecker::check(&program).unwrap_err(),
            TypeError {
                rule: "duplicate".into(),
                requirement: None,
                kind: TypeErrorKind::DuplicateInput("x".into()),
            }
        );
    }
}
