use futures::stream;
use tokio::time::sleep;

use super::*;
use crate::{
    interpreter::adapter::{InputErrorKind, SelectedField},
    srdl::parse,
};

fn rule(inputs: &str, requirements: &str) -> Rule {
    parse(&format!(
        "rule r {{ trigger always; {inputs} {requirements} enforce pend; }}"
    ))
    .unwrap()
    .rules
    .remove(0)
}

fn batch(
    rule: &Rule,
    index: usize,
    values: Vec<InputValue>,
    acquired_at: Instant,
) -> AcquiredInput<'_> {
    let input = &rule.inputs[index];
    assert_eq!(input.fields.len(), values.len());
    AcquiredInput {
        input,
        fields: input
            .fields
            .iter()
            .zip(values)
            .map(|(field, value)| SelectedField { field, value })
            .collect(),
        acquired_at,
    }
}

const TWO_INPUTS: &str = r#"
    input from "a" select number a maxage 1;
    input from "b" select number b maxage 2;
"#;

#[tokio::test(start_paused = true)]
async fn unbounded_inputs_keep_receipt_checks_without_expiry_deadlines() {
    let rule = rule(
        r#"input from "a" select number a;
        input from "b" select number b;"#,
        "require a = b;",
    );
    assert!(matches!(
        evaluate_inputs(0, &Value::Null, &rule, stream::pending()).await,
        Err(Error::FirstInputTimeout { .. })
    ));
    let first = batch(&rule, 0, vec![InputValue::Number(1.0)], Instant::now());
    sleep(Duration::from_secs(60)).await;
    let inputs = stream::iter([Ok(first)]).chain(stream::once(async {
        sleep(Duration::from_secs(60)).await;
        Ok(batch(
            &rule,
            1,
            vec![InputValue::Number(1.0)],
            Instant::now(),
        ))
    }));
    assert!(
        evaluate_inputs(0, &Value::Null, &rule, inputs)
            .await
            .unwrap()
            .passed()
    );
    let first = batch(&rule, 0, vec![InputValue::Number(1.0)], Instant::now());
    assert!(matches!(
        evaluate_inputs(0, &Value::Null, &rule, stream::iter([Ok(first)])).await,
        Err(Error::IncompleteInputs { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn evaluates_context_alone_and_with_delayed_inputs_without_expiring_context() {
    let context =
        serde_json::json!({"identity":{"user_id":"alice","ready":true},"arguments":{"limit":2}});
    let declarations = r#"context string user = identity.user_id;
        context boolean ready = identity.ready;
        context number limit = arguments.limit;"#;
    let invalid_expression = rule(declarations, "require limit / 0 > 0;");
    assert!(matches!(
        evaluate_inputs(0, &context, &invalid_expression, stream::pending()).await,
        Err(Error::Expression {
            source: expression::Error::DivisionByZero,
            ..
        })
    ));
    let context_only = rule(
        declarations,
        "require user = \"alice\" && ready && limit + limit = 4; require false;",
    );
    let result = evaluate_inputs(0, &context, &context_only, stream::empty())
        .await
        .unwrap();
    assert_eq!(result.requirements, [true, false]);

    let slow = rule(
        &format!(
            r#"{declarations}
        input from "first" select number a maxage 120;
        input from "second" select number b maxage 1;"#
        ),
        "require ready; require a + b < limit;",
    );
    // Context bindings do not satisfy the first input deadline.
    assert!(matches!(
        evaluate_inputs(0, &context, &slow, stream::pending()).await,
        Err(Error::FirstInputTimeout { .. })
    ));
    let first = batch(&slow, 0, vec![InputValue::Number(0.0)], Instant::now());
    let inputs = stream::iter([Ok(first)]).chain(stream::once(async {
        sleep(Duration::from_secs(60)).await;
        Ok(batch(
            &slow,
            1,
            vec![InputValue::Number(1.0)],
            Instant::now(),
        ))
    }));
    assert!(
        evaluate_inputs(0, &context, &slow, inputs)
            .await
            .unwrap()
            .passed()
    );
}

#[tokio::test]
async fn validates_context_paths_and_types_before_acquiring_inputs() {
    let rule = rule(
        "context string user = identity.user_id;",
        "require user = \"alice\";",
    );
    for (context, expected) in [
        (Value::Null, ContextError::ExpectedObject("identity".into())),
        (
            serde_json::json!({"identity":{}}),
            ContextError::MissingField("user_id".into()),
        ),
        (
            serde_json::json!({"identity":null}),
            ContextError::ExpectedObject("user_id".into()),
        ),
        (
            serde_json::json!({"identity":{"user_id":42}}),
            ContextError::TypeMismatch {
                expected: crate::srdl::ast::InputType::String,
                actual: "number",
            },
        ),
    ] {
        match evaluate_inputs(0, &context, &rule, stream::pending())
            .await
            .unwrap_err()
        {
            Error::Context {
                rule,
                binding,
                source,
            } => {
                assert_eq!(rule, "r");
                assert_eq!(binding, "user");
                assert_eq!(source, expected);
            }
            error => panic!("unexpected error: {error}"),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn evaluates_out_of_order_inputs_and_deduplicates_dependencies() {
    let rule = rule(
        TWO_INPUTS,
        "require a + a = b; require b > 0; require false;",
    );
    let now = Instant::now();
    let inputs = stream::iter([
        Ok(batch(&rule, 1, vec![InputValue::Number(4.0)], now)),
        Ok(batch(&rule, 0, vec![InputValue::Number(2.0)], now)),
    ]);
    let result = evaluate_inputs(0, &Value::Null, &rule, inputs)
        .await
        .unwrap();
    assert_eq!(result.requirements, [true, true, false]);
    assert!(!result.passed());
    assert_eq!(result.failed_requirements(), [3]);
    let constant = self::rule("", "require true;");
    assert!(
        evaluate_inputs(0, &Value::Null, &constant, stream::empty())
            .await
            .unwrap()
            .passed()
    );
}

#[tokio::test(start_paused = true)]
async fn evaluates_ready_requirements_without_waiting_for_unrelated_inputs() {
    let rule = rule(TWO_INPUTS, "require a > 0; require b / 0 > 0;");
    let inputs = stream::iter([Ok(batch(
        &rule,
        1,
        vec![InputValue::Number(1.0)],
        Instant::now(),
    ))])
    .chain(stream::pending());
    assert!(matches!(
        evaluate_inputs(0, &Value::Null, &rule, inputs).await,
        Err(Error::Expression {
            requirement: 2,
            source: expression::Error::DivisionByZero,
            ..
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn distinguishes_timeouts_incomplete_streams_and_acquisition_errors() {
    let rule = rule(TWO_INPUTS, "require false;");
    assert!(matches!(
        evaluate_inputs(0, &Value::Null, &rule, stream::pending()).await,
        Err(Error::FirstInputTimeout { .. })
    ));
    assert!(matches!(
        evaluate_inputs(0, &Value::Null, &rule, stream::empty()).await,
        Err(Error::IncompleteInputs { .. })
    ));
    let error = InputError {
        rule: "r".into(),
        input: "a".into(),
        capability: "a".into(),
        kind: InputErrorKind::NoActiveProvider,
    };
    assert!(matches!(
        evaluate_inputs(0, &Value::Null, &rule, stream::iter([Err(error)])).await,
        Err(Error::Input(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn keeps_completed_requirements_fresh_and_accepts_the_maxage_boundary() {
    for delay in [
        Duration::from_secs(1),
        Duration::from_secs(1) + Duration::from_nanos(1),
    ] {
        let rule = rule(TWO_INPUTS, "require a > 0; require b > 0;");
        let first = batch(&rule, 0, vec![InputValue::Number(1.0)], Instant::now());
        let inputs = stream::iter([Ok(first)]).chain(stream::once(async {
            sleep(delay).await;
            Ok(batch(
                &rule,
                1,
                vec![InputValue::Number(1.0)],
                Instant::now(),
            ))
        }));
        let result = evaluate_inputs(0, &Value::Null, &rule, inputs).await;
        if delay == Duration::from_secs(1) {
            assert!(result.unwrap().passed());
        } else {
            assert!(matches!(result, Err(Error::Expired { input, .. }) if input == "a"));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn checks_unused_bounded_fields_alongside_unbounded_inputs() {
    let rule = rule(
        r#"
        input from "a" select number unused maxage 1, number a;
        input from "b" select number b;
    "#,
        "require a > 0; require b > 0;",
    );
    let first = batch(
        &rule,
        0,
        vec![InputValue::Number(0.0), InputValue::Number(1.0)],
        Instant::now(),
    );
    let inputs = stream::iter([Ok(first)]).chain(stream::pending());
    assert!(
        matches!(evaluate_inputs(0, &Value::Null, &rule, inputs).await, Err(Error::Expired { input, .. }) if input == "unused")
    );
}

#[tokio::test(start_paused = true)]
async fn rejects_stale_future_and_invalid_input_metadata() {
    let mut rule = rule(TWO_INPUTS, "require a > 0;");
    let now = Instant::now();
    let inputs = stream::once(async {
        sleep(Duration::from_secs(2)).await;
        Ok(batch(&rule, 0, vec![InputValue::Number(1.0)], now))
    });
    assert!(
        matches!(evaluate_inputs(0, &Value::Null, &rule, inputs).await, Err(Error::Expired { input, .. }) if input == "a")
    );
    let future = batch(
        &rule,
        0,
        vec![InputValue::Number(1.0)],
        Instant::now() + Duration::from_secs(1),
    );
    assert!(matches!(
        evaluate_inputs(0, &Value::Null, &rule, stream::iter([Ok(future)])).await,
        Err(Error::FutureTimestamp { .. })
    ));
    for maxage in [0.0, f64::NAN, f64::INFINITY, 1e308] {
        rule.inputs[0].fields[0].maxage = Some(maxage);
        assert!(matches!(
            evaluate_inputs(0, &Value::Null, &rule, stream::pending()).await,
            Err(Error::InvalidMaxage { .. })
        ));
    }
}
