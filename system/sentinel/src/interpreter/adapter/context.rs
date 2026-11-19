// SPDX-License-Identifier: MulanPSL-2.0

//! Typed bindings to the Executor's context snapshot, with no freshness deadline.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use serde_json::Value;

use super::InputValue;
use crate::{
    interpreter::evaluator::Error as CheckError,
    srdl::ast::{ContextClause, InputType, Rule},
};

/// A field traversal, type, or enum membership failure in the call context.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("field '{0}' requires an object")]
    ExpectedObject(String),
    #[error("missing field '{0}'")]
    MissingField(String),
    #[error("context value is not an allowed enum member")]
    InvalidEnum,
    #[error("expected {expected:?}, found {actual}")]
    TypeMismatch {
        /// The type declared by the context binding.
        expected: InputType,
        /// The JSON type at the resolved path.
        actual: &'static str,
    },
}

/// Resolve the context bindings needed by this check.
pub(super) fn bind(
    rule: &Rule,
    context: &Value,
    names: Option<&BTreeSet<&str>>,
) -> Result<BTreeMap<String, Arc<InputValue>>, CheckError> {
    rule.contexts
        .iter()
        .filter(|binding| names.is_none_or(|names| names.contains(binding.name.0.as_str())))
        .map(|binding| {
            let value = resolve(binding, context).map_err(|source| CheckError::Context {
                rule: rule.name.0.clone(),
                binding: binding.name.0.clone(),
                source,
            })?;
            Ok((binding.name.0.clone(), Arc::new(value)))
        })
        .collect()
}

fn resolve(binding: &ContextClause, mut value: &Value) -> Result<InputValue, Error> {
    let optional = matches!(binding.data_type, InputType::Option(_));
    for field in &binding.path {
        if optional && value.is_null() {
            break;
        }
        let object = value
            .as_object()
            .ok_or_else(|| Error::ExpectedObject(field.0.clone()))?;
        value = match object.get(&field.0) {
            Some(value) => value,
            None if optional => &Value::Null,
            None => return Err(Error::MissingField(field.0.clone())),
        };
    }
    let value = InputValue::from_json(value, binding.data_type.clone()).map_err(|actual| {
        Error::TypeMismatch {
            expected: binding.data_type.clone(),
            actual,
        }
    })?;
    if !value.matches_enum(&binding.allowed_values) {
        return Err(Error::InvalidEnum);
    }
    Ok(value)
}
