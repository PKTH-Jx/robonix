use super::*;
use crate::srdl::ast::TriggerTiming;
use robonix_atlas::{
    client::AtlasClient,
    service::{AtlasRegistry, serve_atlas},
};
use tokio::{
    sync::Semaphore,
    time::{Duration, timeout},
};

async fn adapter() -> (InputAdapter, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let server = tokio::spawn(serve_atlas(Arc::new(AtlasRegistry::default()), address));
    let atlas =
        AtlasClient::connect_with_retry(format!("http://{address}"), 50, Duration::from_millis(10))
            .await
            .unwrap();
    (InputAdapter::new(atlas), server)
}

fn snapshot(rule: &crate::srdl::ast::Rule, overall: Instant) -> Prepared {
    let mut deadline = Deadline::new(overall);
    let mut values = BTreeMap::new();
    for field in rule.inputs.iter().flat_map(|input| &input.fields) {
        deadline.record(
            Instant::now(),
            field.maxage.map(Duration::from_secs_f64),
            Arc::from(field.name.0.as_str()),
        );
        values.insert(
            field.name.0.clone(),
            Arc::new(crate::interpreter::adapter::InputValue::Number(1.0)),
        );
    }
    Prepared { values, deadline }
}

#[tokio::test]
async fn queued_expiry_retries_before_evaluating_predicates() {
    let (adapter, server) = adapter().await;
    tokio::time::pause();
    let program = crate::srdl::parse(
        r#"trigger a before always;
        rule a {
        input from "sensor/a" select number x maxage 1;
        require x / 0 > 0; enforce deny; }"#,
    )
    .unwrap();
    let mut pool = Pool::new(&program, &adapter);
    let key = ("plan".into(), "call".into(), TriggerTiming::Before);
    let (_owner, cancel) = watch::channel(());
    let (reply, receive) = oneshot::channel();
    pool.check(Check {
        key: key.clone(),
        check_id: 1,
        capability: "robot/pick".into(),
        consumer: "test".into(),
        context: Arc::new(Value::Null),
        cancel,
        reply,
        permit: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
    });
    pool.acquiring.clear();
    let overall = pool.calls[&key].deadline;
    let stale = snapshot(&program.rules[0], overall);
    tokio::time::advance(Duration::from_secs(2)).await;
    pool.evaluate(ReadyCheck {
        key: (key.clone(), 0),
        check_id: 1,
        data: Ok(stale),
    });
    assert_eq!(pool.rules[&(key.clone(), 0)].retries, 1);
    assert!(pool.calls[&key].results.is_empty());
    pool.acquiring.clear();
    pool.evaluate(ReadyCheck {
        key: (key.clone(), 0),
        check_id: 1,
        data: Ok(snapshot(&program.rules[0], overall)),
    });
    let result = receive.await.unwrap().unwrap();
    assert!(matches!(
        result.rules[&0],
        Err(Error::Expression {
            source: super::super::expression::Error::DivisionByZero,
            ..
        })
    ));
    assert!(pool.calls.is_empty());
    server.abort();
}

#[tokio::test]
async fn retries_expired_results_at_the_combined_decision_and_bounds_attempts() {
    let (adapter, server) = adapter().await;
    tokio::time::pause();
    let program = crate::srdl::parse(
        r#"
        trigger [a, b] before always;
        trigger [a, b] after always;
        rule a { input from "sensor/a" select number x maxage 1;
            require x > 0; enforce deny; }
        rule b { input from "sensor/b" select number x;
            require x > 0; enforce deny; }
    "#,
    )
    .unwrap();
    let mut pool = Pool::new(&program, &adapter);
    let key = ("plan".into(), "call".into(), TriggerTiming::Before);
    let (_owner, cancel) = watch::channel(());
    let (reply, receive) = oneshot::channel();
    pool.check(Check {
        key: key.clone(),
        check_id: 1,
        capability: "robot/pick".into(),
        consumer: "test".into(),
        context: Arc::new(Value::Null),
        cancel,
        reply,
        permit: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
    });
    // Supply timestamped snapshots directly so network latency cannot affect expiry.
    pool.acquiring.clear();
    let deadline = pool.calls[&key].deadline;
    pool.evaluate(ReadyCheck {
        key: (key.clone(), 0),
        check_id: 1,
        data: Ok(snapshot(&program.rules[0], deadline)),
    });
    tokio::time::advance(Duration::from_secs(2)).await;
    pool.evaluate(ReadyCheck {
        key: (key.clone(), 1),
        check_id: 1,
        data: Ok(snapshot(&program.rules[1], deadline)),
    });
    assert_eq!(pool.rules[&(key.clone(), 0)].retries, 1);
    assert_eq!(pool.calls[&key].results.len(), 1);
    assert_eq!(pool.calls[&key].deadline, deadline);
    pool.acquiring.clear();
    pool.evaluate(ReadyCheck {
        key: (key.clone(), 0),
        check_id: 1,
        data: Ok(snapshot(&program.rules[0], deadline)),
    });
    assert!(
        receive
            .await
            .unwrap()
            .unwrap()
            .rules
            .values()
            .all(|result| result.as_ref().unwrap().passed())
    );
    assert!(pool.rules.is_empty());
    assert!(pool.calls.is_empty());

    let key = ("plan".into(), "call".into(), TriggerTiming::After);
    let (_after_owner, cancel) = watch::channel(());
    let (reply, receive) = oneshot::channel();
    pool.check(Check {
        key: key.clone(),
        check_id: 2,
        capability: "robot/pick".into(),
        consumer: "test".into(),
        context: Arc::new(Value::Null),
        cancel,
        reply,
        permit: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
    });
    assert!(pool.calls[&key].deadline > deadline);
    assert_eq!(pool.rules[&(key.clone(), 0)].retries, 0);
    pool.acquiring.clear();
    let deadline = pool.calls[&key].deadline;
    pool.evaluate(ReadyCheck {
        key: (key.clone(), 1),
        check_id: 2,
        data: Ok(snapshot(&program.rules[1], deadline)),
    });
    for attempt in 0..=MAX_FRESHNESS_RETRIES {
        pool.evaluate(ReadyCheck {
            key: (key.clone(), 0),
            check_id: 2,
            data: Err(Error::Expired {
                rule: "a".into(),
                input: "x".into(),
            }),
        });
        pool.acquiring.clear();
        if attempt < MAX_FRESHNESS_RETRIES {
            assert_eq!(pool.rules[&(key.clone(), 0)].retries, attempt + 1);
            assert_eq!(pool.calls[&key].deadline, deadline);
        }
    }
    let result = receive.await.unwrap().unwrap();
    assert!(matches!(result.rules[&0], Err(Error::Expired { .. })));
    assert!(pool.calls.is_empty());
    assert!(pool.rules.is_empty());
    server.abort();
}

#[tokio::test]
async fn cancelled_reuse_ignores_old_work_and_releases_capacity_after_checks() {
    let (adapter, server) = adapter().await;
    let program = crate::srdl::parse(
        r#"trigger a before always;
        rule a { input from "sensor/a" select number x; require x > 0; enforce deny; }"#,
    )
    .unwrap();
    let mut pool = Pool::new(&program, &adapter);
    let permits = Arc::new(Semaphore::new(2));
    let key = ("plan".to_owned(), "call".to_owned(), TriggerTiming::Before);
    let (owner, cancel) = watch::channel(());
    let (reply, old_receive) = oneshot::channel();
    pool.check(Check {
        key: key.clone(),
        check_id: 1,
        capability: "robot/pick".into(),
        consumer: "test".into(),
        context: Arc::new(Value::Null),
        cancel,
        reply,
        permit: permits.clone().try_acquire_owned().unwrap(),
    });
    pool.acquiring.clear();
    drop(owner);
    // Reuse happens before the cancellation stream has been polled.
    let (_owner, cancel) = watch::channel(());
    let (reply, receive) = oneshot::channel();
    pool.check(Check {
        key: key.clone(),
        check_id: 2,
        capability: "robot/pick".into(),
        consumer: "test".into(),
        context: Arc::new(Value::Null),
        cancel,
        reply,
        permit: permits.clone().try_acquire_owned().unwrap(),
    });
    pool.acquiring.clear();
    assert!(old_receive.await.is_err());
    let deadline = pool.calls[&key].deadline;
    pool.evaluate(ReadyCheck {
        key: (key.clone(), 0),
        check_id: 1,
        data: Ok(snapshot(&program.rules[0], deadline)),
    });
    assert!(pool.calls[&key].results.is_empty());
    pool.evaluate(ReadyCheck {
        key: (key.clone(), 0),
        check_id: 2,
        data: Ok(snapshot(&program.rules[0], deadline)),
    });
    assert!(pool.calls.is_empty());
    assert!(pool.rules.is_empty());
    // Check completion releases capacity even while its submitting owner is alive.
    timeout(Duration::from_secs(1), async {
        while let Some(event) = pool.cancelled.next().await {
            assert!(event.is_none());
        }
    })
    .await
    .unwrap();
    assert_eq!(permits.available_permits(), 2);
    assert!(
        receive
            .await
            .unwrap()
            .unwrap()
            .rules
            .values()
            .all(|result| result.as_ref().unwrap().passed())
    );
    server.abort();
}

mod expression {
    use crate::interpreter::{
        adapter::InputValue,
        evaluator::expression::{Error, evaluate, evaluate_condition},
    };
    use crate::srdl::{
        ast::{BinaryOperator, Expression, InputType, Literal, UnaryOperator},
        parse,
    };
    use std::{collections::BTreeMap, sync::Arc};

    fn condition(source: &str) -> Expression {
        parse(&format!("rule r {{ require {source}; enforce deny; }}"))
            .unwrap()
            .rules
            .remove(0)
            .requires
            .remove(0)
            .expression
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
                evaluate(&condition(source), &inputs).unwrap(),
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
                evaluate_condition(&condition(source), &inputs).unwrap(),
                expected
            );
        }
        assert_eq!(
            evaluate_condition(&condition("true && (1 / 0 > 0)"), &inputs),
            Err(Error::DivisionByZero)
        );
        assert_eq!(
            evaluate_condition(&condition("false || missing"), &inputs),
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
                evaluate(&condition(source), &inputs),
                Err(expected),
                "{source}"
            );
        }
        inputs.insert("x".into(), Arc::new(InputValue::Number(f64::NAN)));
        assert_eq!(
            evaluate_condition(&condition("x != x"), &inputs),
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
        assert_eq!(evaluate_condition(&condition("false"), &inputs), Ok(false));
        assert_eq!(evaluate_condition(&condition("ready"), &inputs), Ok(true));
        for (source, actual) in [
            ("x + 1", InputType::Number),
            ("label", InputType::String),
            ("0 || ready", InputType::Number),
        ] {
            assert_eq!(
                evaluate_condition(&condition(source), &inputs),
                Err(Error::ExpectedBoolean(actual))
            );
        }
    }
}
