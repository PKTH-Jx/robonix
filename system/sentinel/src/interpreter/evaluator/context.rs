//! Typed bindings to the Executor's context snapshot, with no freshness deadline.

use std::{collections::BTreeMap, sync::Arc};

use serde_json::Value;

use super::super::adapter::InputValue;
use crate::srdl::ast::{ContextClause, InputType, Rule};

/// A field traversal or basic type failure in the supplied call context.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("field '{0}' requires an object")]
    ExpectedObject(String),
    #[error("missing field '{0}'")]
    MissingField(String),
    #[error("expected {expected:?}, found {actual}")]
    TypeMismatch {
        /// The type declared by the context binding.
        expected: InputType,
        /// The JSON type at the resolved path.
        actual: &'static str,
    },
}

pub(super) fn bind(
    rule: &Rule,
    context: &Value,
) -> Result<BTreeMap<String, Arc<InputValue>>, super::Error> {
    rule.contexts
        .iter()
        .map(|binding| {
            let value = resolve(binding, context).map_err(|source| super::Error::Context {
                rule: rule.name.0.clone(),
                binding: binding.name.0.clone(),
                source,
            })?;
            Ok((binding.name.0.clone(), Arc::new(value)))
        })
        .collect()
}

fn resolve(binding: &ContextClause, mut value: &Value) -> Result<InputValue, Error> {
    for field in &binding.path {
        value = value
            .as_object()
            .ok_or_else(|| Error::ExpectedObject(field.0.clone()))?
            .get(&field.0)
            .ok_or_else(|| Error::MissingField(field.0.clone()))?;
    }
    InputValue::from_json(value, binding.data_type).map_err(|actual| Error::TypeMismatch {
        expected: binding.data_type,
        actual,
    })
}
