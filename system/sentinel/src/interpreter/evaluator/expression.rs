//! Pure expression evaluation over already acquired basic input values.

use std::{collections::BTreeMap, sync::Arc};

use crate::interpreter::adapter::InputValue;
use crate::srdl::ast::{BinaryOperator, Expression, InputType, Literal, UnaryOperator};

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
pub(super) fn evaluate_condition(
    expression: &Expression,
    inputs: &BTreeMap<String, Arc<InputValue>>,
) -> Result<bool, Error> {
    boolean(evaluate(expression, inputs)?)
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
