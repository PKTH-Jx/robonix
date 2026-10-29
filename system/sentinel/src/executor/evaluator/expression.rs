//! Pure expression evaluation over already acquired basic input values.

use std::{collections::BTreeMap, sync::Arc};

use crate::executor::adapter::InputValue;
use crate::srdl::ast::{
    BinaryOperator, Expression, InputType, Literal, RequireClause, UnaryOperator,
};

/// Failures in expression calculation, distinct from a false requirement result.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("input '{0}' is unavailable")]
    UnknownInput(String),
    #[error("expected boolean, found {0:?}")]
    ExpectedBoolean(InputType),
    #[error("numeric values and results must be finite")]
    NonFiniteNumber,
    #[error("division by zero")]
    DivisionByZero,
    #[error("remainder by zero")]
    RemainderByZero,
    #[error("unary operator {operator:?} does not accept {operand:?}")]
    InvalidUnary {
        /// The prefix operation being evaluated.
        operator: UnaryOperator,
        /// The actual operand type.
        operand: InputType,
    },
    #[error("binary operator {operator:?} does not accept {left:?} and {right:?}")]
    InvalidBinary {
        /// The operation being evaluated.
        operator: BinaryOperator,
        /// The actual left operand type.
        left: InputType,
        /// The actual right operand type.
        right: InputType,
    },
    #[error("cannot evaluate {0} without type information")]
    UnsupportedExpression(&'static str),
}

/// Calculate a safety condition; false is a violation, while Err means calculation failed.
pub fn evaluate_requirement(
    require: &RequireClause,
    inputs: &BTreeMap<String, Arc<InputValue>>,
) -> Result<bool, Error> {
    boolean(evaluate(&require.expression, inputs)?)
}

/// Recursively calculate an expression without mutating inputs or performing I/O.
/// Logical operators evaluate only the operand needed to determine the result.
/// Static validation is a separate pass; short-circuited branches are not visited.
pub fn evaluate(
    expression: &Expression,
    inputs: &BTreeMap<String, Arc<InputValue>>,
) -> Result<InputValue, Error> {
    match expression {
        Expression::Literal(Literal::Number(value)) => number(*value),
        Expression::Literal(Literal::Boolean(value)) => Ok(InputValue::Boolean(*value)),
        Expression::Literal(Literal::String(value)) => Ok(InputValue::String(value.clone())),
        Expression::Identifier(name) => {
            let value = inputs
                .get(&name.0)
                .ok_or_else(|| Error::UnknownInput(name.0.clone()))?;
            match value.as_ref() {
                InputValue::Number(value) => number(*value),
                value => Ok(value.clone()),
            }
        }
        Expression::Unary { operator, operand } => {
            let value = evaluate(operand, inputs)?;
            match (operator, value) {
                (UnaryOperator::Not, InputValue::Boolean(value)) => Ok(InputValue::Boolean(!value)),
                (UnaryOperator::Negate, InputValue::Number(value)) => number(-value),
                (_, value) => Err(Error::InvalidUnary {
                    operator: *operator,
                    operand: value.data_type(),
                }),
            }
        }
        Expression::Binary {
            operator,
            left,
            right,
        } => {
            let left = evaluate(left, inputs)?;
            if matches!(operator, BinaryOperator::And | BinaryOperator::Or) {
                let left = boolean(left)?;
                if (*operator == BinaryOperator::And && !left)
                    || (*operator == BinaryOperator::Or && left)
                {
                    return Ok(InputValue::Boolean(left));
                }
                return boolean(evaluate(right, inputs)?).map(InputValue::Boolean);
            }
            binary(*operator, left, evaluate(right, inputs)?)
        }
        Expression::Field { .. } => Err(Error::UnsupportedExpression("field access")),
        Expression::Call { .. } => Err(Error::UnsupportedExpression("function calls")),
    }
}

fn boolean(value: InputValue) -> Result<bool, Error> {
    match value {
        InputValue::Boolean(value) => Ok(value),
        value => Err(Error::ExpectedBoolean(value.data_type())),
    }
}

fn number(value: f64) -> Result<InputValue, Error> {
    if !value.is_finite() {
        return Err(Error::NonFiniteNumber);
    }
    Ok(InputValue::Number(value))
}

fn binary(
    operator: BinaryOperator,
    left: InputValue,
    right: InputValue,
) -> Result<InputValue, Error> {
    use BinaryOperator as B;
    use InputValue::{Boolean, Number};

    match (operator, &left, &right) {
        (B::Eq | B::Neq, _, _) if left.data_type() == right.data_type() => {
            Ok(Boolean(if operator == B::Eq {
                left == right
            } else {
                left != right
            }))
        }
        (B::Add, Number(left), Number(right)) => number(left + right),
        (B::Subtract, Number(left), Number(right)) => number(left - right),
        (B::Multiply, Number(left), Number(right)) => number(left * right),
        (B::Divide, Number(_), Number(right)) if *right == 0.0 => Err(Error::DivisionByZero),
        (B::Divide, Number(left), Number(right)) => number(left / right),
        (B::Remainder, Number(_), Number(right)) if *right == 0.0 => Err(Error::RemainderByZero),
        (B::Remainder, Number(left), Number(right)) => number(left % right),
        (B::Lt, Number(left), Number(right)) => Ok(Boolean(left < right)),
        (B::Lte, Number(left), Number(right)) => Ok(Boolean(left <= right)),
        (B::Gt, Number(left), Number(right)) => Ok(Boolean(left > right)),
        (B::Gte, Number(left), Number(right)) => Ok(Boolean(left >= right)),
        _ => Err(Error::InvalidBinary {
            operator,
            left: left.data_type(),
            right: right.data_type(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srdl::parse;

    fn requirement(source: &str) -> RequireClause {
        parse(&format!(
            "rule r {{ trigger always; require {source}; enforce deny; }}"
        ))
        .unwrap()
        .rules
        .remove(0)
        .requires
        .remove(0)
    }

    fn inputs() -> BTreeMap<String, Arc<InputValue>> {
        use InputValue::*;
        [
            ("x", Number(7.0)),
            ("y", Number(2.0)),
            ("ready", Boolean(true)),
            ("label", String("safe".into())),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), Arc::new(value)))
        .collect()
    }

    #[test]
    fn evaluates_basic_values_operators_and_precedence() {
        use InputValue::*;
        let inputs = inputs();
        for (source, expected) in [
            ("true", Boolean(true)),
            ("false", Boolean(false)),
            (r#""盘子\n""#, String("盘子\n".into())),
            ("label", String("safe".into())),
            ("ready", Boolean(true)),
            ("x + y * 2", Number(11.0)),
            ("(x + y) * 2", Number(18.0)),
            ("x - y - y", Number(3.0)),
            ("x / y", Number(3.5)),
            ("x % y", Number(1.0)),
            ("-x", Number(-7.0)),
            ("!ready", Boolean(false)),
            ("x > y", Boolean(true)),
            ("x >= 7", Boolean(true)),
            ("y < x", Boolean(true)),
            ("y <= 2", Boolean(true)),
            ("x = 7.0", Boolean(true)),
            ("ready = false", Boolean(false)),
            (r#"label != "blocked""#, Boolean(true)),
            (r#"label = "safe""#, Boolean(true)),
            ("ready && x > y", Boolean(true)),
            ("false || ready", Boolean(true)),
        ] {
            assert_eq!(
                evaluate(&requirement(source).expression, &inputs).unwrap(),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn short_circuits_only_unneeded_operands() {
        let inputs = inputs();
        for (source, expected) in [
            ("false && (1 / 0 > 0)", false),
            ("true || missing", true),
            ("!ready && missing", false),
            ("true && ready", true),
        ] {
            assert_eq!(
                evaluate_requirement(&requirement(source), &inputs).unwrap(),
                expected
            );
        }
        assert_eq!(
            evaluate_requirement(&requirement("true && (1 / 0 > 0)"), &inputs),
            Err(Error::DivisionByZero)
        );
        assert_eq!(
            evaluate_requirement(&requirement("false || missing"), &inputs),
            Err(Error::UnknownInput("missing".into()))
        );
    }

    #[test]
    fn reports_calculation_failures_without_coercion() {
        let mut inputs = inputs();
        for (source, expected) in [
            ("missing", Error::UnknownInput("missing".into())),
            ("1 / -0", Error::DivisionByZero),
            ("1 % 0", Error::RemainderByZero),
            ("1e308 * 1e308", Error::NonFiniteNumber),
            (
                "!x",
                Error::InvalidUnary {
                    operator: UnaryOperator::Not,
                    operand: InputType::Number,
                },
            ),
            (
                "-ready",
                Error::InvalidUnary {
                    operator: UnaryOperator::Negate,
                    operand: InputType::Boolean,
                },
            ),
            (
                "x + ready",
                Error::InvalidBinary {
                    operator: BinaryOperator::Add,
                    left: InputType::Number,
                    right: InputType::Boolean,
                },
            ),
            (
                "x = label",
                Error::InvalidBinary {
                    operator: BinaryOperator::Eq,
                    left: InputType::Number,
                    right: InputType::String,
                },
            ),
            (
                r#"label < "z""#,
                Error::InvalidBinary {
                    operator: BinaryOperator::Lt,
                    left: InputType::String,
                    right: InputType::String,
                },
            ),
            ("ready()", Error::UnsupportedExpression("function calls")),
            ("label.value", Error::UnsupportedExpression("field access")),
        ] {
            assert_eq!(
                evaluate(&requirement(source).expression, &inputs),
                Err(expected),
                "{source}"
            );
        }
        inputs.insert("x".into(), Arc::new(InputValue::Number(f64::NAN)));
        assert_eq!(
            evaluate_requirement(&requirement("x != x"), &inputs),
            Err(Error::NonFiniteNumber)
        );
        assert_eq!(
            evaluate(
                &Expression::Literal(Literal::Number(f64::INFINITY)),
                &inputs
            ),
            Err(Error::NonFiniteNumber)
        );
    }

    #[test]
    fn requires_boolean_results_and_preserves_false_as_a_violation() {
        let inputs = inputs();
        assert_eq!(
            evaluate_requirement(&requirement("false"), &inputs),
            Ok(false)
        );
        assert_eq!(
            evaluate_requirement(&requirement("ready"), &inputs),
            Ok(true)
        );
        for (source, actual) in [
            ("x + 1", InputType::Number),
            ("label", InputType::String),
            ("0 || ready", InputType::Number),
        ] {
            assert_eq!(
                evaluate_requirement(&requirement(source), &inputs),
                Err(Error::ExpectedBoolean(actual))
            );
        }
    }
}
