//! Field traversal and unique array selection over capability response JSON.

use serde_json::Value;

use crate::srdl::ast::{Literal, SelectStep};

/// A select failure located within the declared path.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("select step {step}: {kind}")]
pub struct SelectError {
    /// The one-based position of the failing field or filter operation.
    pub step: usize,
    /// The structural or matching failure at that position.
    pub kind: SelectErrorKind,
}

/// Reasons a select path cannot resolve to one JSON value.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SelectErrorKind {
    #[error("field access requires an object")]
    ExpectedObject,
    #[error("missing field '{0}'")]
    MissingField(String),
    #[error("filter requires an array")]
    ExpectedArray,
    #[error("filter matched no elements")]
    NoMatch,
    #[error("filter matched multiple elements")]
    MultipleMatches,
}

/// Resolve a path without changing the response or coercing its values.
/// Filters skip elements lacking a matching field and require exactly one match.
pub(super) fn select<'json>(
    mut value: &'json Value,
    steps: &[SelectStep],
) -> Result<&'json Value, SelectError> {
    use SelectErrorKind as K;

    for (index, step) in steps.iter().enumerate() {
        let error = |kind| SelectError {
            step: index + 1,
            kind,
        };
        value = match step {
            SelectStep::Field(field) => value
                .as_object()
                .ok_or_else(|| error(K::ExpectedObject))?
                .get(&field.0)
                .ok_or_else(|| error(K::MissingField(field.0.clone())))?,
            SelectStep::Filter {
                field,
                value: expected,
            } => {
                let array = value.as_array().ok_or_else(|| error(K::ExpectedArray))?;
                let mut matches = array.iter().filter(|element| {
                    let Some(actual) = element.get(field.0.as_str()) else {
                        return false;
                    };
                    match expected {
                        Literal::Number(number) => actual.as_f64() == Some(*number),
                        Literal::String(text) => actual.as_str() == Some(text.as_str()),
                        Literal::Boolean(boolean) => actual.as_bool() == Some(*boolean),
                    }
                });
                let selected = matches.next().ok_or_else(|| error(K::NoMatch))?;
                // Reject ambiguity rather than making the result depend on array order.
                if matches.next().is_some() {
                    return Err(error(K::MultipleMatches));
                }
                selected
            }
        };
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::srdl::parse;

    fn steps(path: &str) -> Vec<SelectStep> {
        parse(&format!(
            r#"rule r {{
            input from "source" select number x = {path} maxage 1;
            require true; enforce deny; }}"#
        ))
        .unwrap()
        .rules
        .remove(0)
        .inputs
        .remove(0)
        .fields
        .remove(0)
        .select
    }

    #[test]
    fn selects_fields_and_typed_filters_including_nested_arrays() {
        let response = json!({
            "snapshot": { "items": [null, {},
                {"id": "left", "ready": true, "sequence": 1, "value": 2.5,
                 "samples": [{"id": -1.5, "value": "ok"}]},
                {"id": "right", "ready": false, "sequence": "1", "value": 3}
            ]},
            "require": [{"true": false, "maxage": null}]
        });
        for (path, expected) in [
            (r#"snapshot.items[id = "left"].value"#, json!(2.5)),
            ("snapshot.items[ready = true].value", json!(2.5)),
            ("snapshot.items[ready = false].value", json!(3)),
            ("snapshot.items[sequence = 1.0].value", json!(2.5)),
            (r#"snapshot.items[sequence = "1"].value"#, json!(3)),
            (
                r#"snapshot.items[id = "left"].samples[id = -1.5].value"#,
                json!("ok"),
            ),
            ("require[true = false].maxage", Value::Null),
        ] {
            assert_eq!(
                select(&response, &steps(path)).unwrap(),
                &expected,
                "{path}"
            );
        }
    }

    #[test]
    fn reports_structure_missing_and_ambiguous_matches_at_the_failing_step() {
        let response = json!({"items": [{"id": 1}, {"id": 1.0}], "nil": null});
        for (path, step, kind) in [
            (
                "missing",
                1,
                SelectErrorKind::MissingField("missing".into()),
            ),
            ("items.value", 2, SelectErrorKind::ExpectedObject),
            ("nil.value", 2, SelectErrorKind::ExpectedObject),
            ("nil[id = 1]", 2, SelectErrorKind::ExpectedArray),
            ("items[id = 2]", 2, SelectErrorKind::NoMatch),
            (r#"items[id = "1"]"#, 2, SelectErrorKind::NoMatch),
            ("items[id = true]", 2, SelectErrorKind::NoMatch),
            ("items[id = 1]", 2, SelectErrorKind::MultipleMatches),
        ] {
            assert_eq!(
                select(&response, &steps(path)).unwrap_err(),
                SelectError { step, kind },
                "{path}"
            );
        }
    }
}
