use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use futures::{StreamExt, TryStreamExt, stream};
use robonix_atlas::service::{AtlasRegistry, AtlasService};
use tokio::{
    net::TcpListener,
    task::JoinHandle,
    time::{sleep, timeout},
};

use super::*;
use crate::{
    interpreter::{
        CheckContext, LoadError, SrdlInterpreter,
        enforcer::Effect,
        evaluator::{Error, RuleResult, expression},
    },
    srdl::{ast::TriggerTiming, parse},
};

fn check_context<'a>(
    plan_id: &'a str,
    call_id: &'a str,
    capability: &'a str,
    values: &'a Value,
) -> CheckContext<'a> {
    CheckContext {
        plan_id,
        call_id,
        capability,
        consumer_id: "sentinel-test",
        values,
    }
}

/// Runs real Atlas discovery over a local gRPC connection.
struct AtlasFixture {
    /// Registry used to register and transition providers during tests.
    registry: Arc<AtlasRegistry>,
    /// Adapter connected to the local Atlas service.
    adapter: InputAdapter,
    /// Server task aborted when the fixture leaves scope.
    servers: Vec<JoinHandle<()>>,
    /// MCP invocations counted independently of the number of selected fields.
    calls: Arc<AtomicUsize>,
}

impl Drop for AtlasFixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
    }
}

impl AtlasFixture {
    fn interpreter(&self, source: &str) -> SrdlInterpreter {
        SrdlInterpreter::load(
            source,
            self.adapter.atlas.clone(),
            strategy::StrategyConfig::default(),
        )
        .unwrap()
    }

    fn interpreter_with_strategies(
        &self,
        source: &str,
        strategies: strategy::Strategies,
    ) -> Result<SrdlInterpreter, LoadError> {
        SrdlInterpreter::load_with_strategies(source, self.adapter.atlas.clone(), strategies)
    }

    async fn wait_for_cleanup(&self) {
        // MCP shutdown and Atlas disconnect have separate timeout budgets.
        timeout(Duration::from_secs(15), async {
            while !self.channels_empty().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    async fn collect<'a>(&self, rule: &'a Rule) -> Result<Vec<AcquiredInput<'a>>, InputError> {
        let names = rule
            .inputs
            .iter()
            .flat_map(|input| input.fields())
            .map(|field| field.name().0.as_str())
            .collect();
        let inputs = self
            .adapter
            .fetch_group(
                [(rule, &names)],
                "sentinel-test",
                Instant::now() + Duration::from_secs(5),
                Arc::new(Value::Null),
            )
            .pop()
            .unwrap();
        timeout(Duration::from_secs(5), inputs.try_collect())
            .await
            .unwrap()
    }

    async fn new() -> Self {
        let registry = Arc::new(AtlasRegistry::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let incoming = stream::unfold(listener, |listener| async {
            Some((listener.accept().await.map(|(socket, _)| socket), listener))
        });
        let service = AtlasService::new(registry.clone());
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(pb::atlas_server::AtlasServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });
        let atlas = AtlasClient::connect(endpoint).await.unwrap();
        Self {
            registry,
            adapter: InputAdapter::new(atlas),
            servers: vec![server],
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn register(&self, id: &str, active: bool) {
        self.registry
            .register(id, pb::Kind::Service, "sensor", "", "")
            .await
            .unwrap();
        for contract in ["robonix/lifecycle/driver", "sensor/value"] {
            let service = "robonix.contracts.RobonixLifecycleDriver";
            let params = pb::TransportParams {
                kind: Some(pb::transport_params::Kind::Grpc(pb::GrpcParams {
                    proto_file: "robonix_contracts.proto".into(),
                    service_name: service.into(),
                    method: format!("/{service}/Driver"),
                })),
            };
            self.registry
                .declare(
                    id,
                    contract,
                    pb::Transport::Grpc,
                    &format!("{id}.localhost:51000"),
                    params,
                    "",
                )
                .await
                .unwrap();
        }
        if active {
            self.activate(id).await;
        }
    }

    async fn activate(&self, id: &str) {
        for state in [
            pb::LifecycleState::StateInactive,
            pb::LifecycleState::StateActive,
        ] {
            self.registry
                .set_lifecycle_state(id, state, "")
                .await
                .unwrap();
        }
    }

    async fn serve_mcp(&mut self) -> Arc<tokio::sync::Notify> {
        use rmcp::transport::streamable_http_server::{
            StreamableHttpService, session::local::LocalSessionManager,
        };
        let entered = Arc::new(tokio::sync::Notify::new());
        let handler = McpHandler {
            entered: entered.clone(),
            calls: self.calls.clone(),
        };
        let service: StreamableHttpService<McpHandler, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(handler.clone()),
                Default::default(),
                Default::default(),
            );
        let router = axum::Router::new().nest_service("/mcp", service);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        self.servers.push(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
        self.register("mcp", true).await;
        for name in ["value", "scalar", "error", "slow", "timed", "recovering"] {
            self.registry
                .declare(
                    "mcp",
                    &format!("sensor/{name}"),
                    pb::Transport::Mcp,
                    &endpoint,
                    pb::TransportParams {
                        kind: Some(pb::transport_params::Kind::Mcp(pb::McpParams {
                            input_schema_json: "{}".into(),
                        })),
                    },
                    "",
                )
                .await
                .unwrap();
        }
        entered
    }

    async fn channels_empty(&self) -> bool {
        let dump: serde_json::Value =
            serde_json::from_str(&self.registry.inspect_json().await.unwrap()).unwrap();
        dump["channels"].as_object().unwrap().is_empty()
    }
}

/// Provides deterministic MCP responses and signals when a slow call starts.
#[derive(Clone)]
struct McpHandler {
    /// Notification allowing tests to cancel an in-flight call without sleeps.
    entered: Arc<tokio::sync::Notify>,
    /// Counts calls to verify that a field group consumes one response.
    calls: Arc<AtomicUsize>,
}

impl rmcp::ServerHandler for McpHandler {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        _: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        use rmcp::model::{CallToolResult, Content};
        let count = self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(request.arguments.unwrap_or_default().is_empty());
        Ok(match request.name.as_ref() {
            "value" => CallToolResult::structured(serde_json::json!({
                "reading":42, "ratio":1.5, "ready":true, "label":"42", "nil":null, "items":[]
            })),
            "scalar" => CallToolResult::success(vec![Content::text("42")]),
            "timed" => {
                sleep(Duration::from_millis(30)).await;
                CallToolResult::structured(serde_json::json!({"ready":true}))
            }
            "recovering" => CallToolResult::structured(serde_json::json!({"ready":count == 2})),
            "error" => CallToolResult::error(vec![Content::text("sensor unavailable")]),
            "slow" => {
                self.entered.notify_one();
                std::future::pending().await
            }
            _ => unreachable!(),
        })
    }
}

#[tokio::test]
async fn while_checks_use_measured_intervals_and_reacquire_until_recovery() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let interpreter = fixture.interpreter(
        r#"
        trigger r while "robot/move";
        rule r { input from "sensor/timed" select boolean ready = ready;
            require ready; enforce stop; }
    "#,
    );
    let values = Value::Null;
    let context = check_context("plan", "call", "robot/move", &values);
    let checks = interpreter.on_execution(context, &());
    tokio::pin!(checks);
    let started = Instant::now();
    assert_eq!(checks.next().await.unwrap().effect, Effect::Allow);
    let elapsed = started.elapsed();
    assert!(timeout(elapsed * 2, checks.next()).await.is_err());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(checks.next().await.unwrap().effect, Effect::Allow);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);

    fixture.calls.store(0, Ordering::SeqCst);
    let interpreter = fixture.interpreter(
        r#"
        trigger r while "robot/move" interval 1;
        rule r { input from "sensor/recovering" select boolean ready = ready;
            require ready; enforce report "unsafe"; }
    "#,
    );
    let checks = interpreter.on_execution(context, &());
    tokio::pin!(checks);
    for messages in [1, 0, 0, 1] {
        let result = checks.next().await.unwrap();
        assert_eq!(result.messages.len(), messages);
        assert_eq!(result.effect, Effect::Allow);
    }
    let independent = interpreter.on_execution(context, &());
    tokio::pin!(independent);
    assert_eq!(independent.next().await.unwrap().messages, ["unsafe"]);

    let unavailable = fixture.interpreter(
        r#"
        trigger r while "robot/move";
        rule r { input from "sensor/missing" select boolean ready = ready;
            require ready; enforce report "unsafe"; }
    "#,
    );
    let checks = unavailable.on_execution(context, &());
    tokio::pin!(checks);
    let result = checks.next().await.unwrap();
    assert_eq!(result.effect, Effect::Stop);
    assert!(result.reports[0].evaluation.is_err());
    assert!(result.messages.is_empty());
    assert!(checks.next().await.is_none());
}

#[tokio::test]
async fn returns_a_prepared_dataset_without_evaluating_predicates_or_fetching_unused_inputs() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let program = parse(
        r#"rule r {
        context number limit = arguments.limit;
        input from "sensor/value" select number x = reading maxage 1;
        input from "sensor/slow" select number later;
        require x / 0 > limit;
        enforce deny;
    }"#,
    )
    .unwrap();
    let overall = Instant::now() + Duration::from_secs(5);
    let dataset = fixture
        .adapter
        .acquire_group(
            [&program.rules[0]],
            "sentinel-test",
            Arc::new(serde_json::json!({"arguments":{"limit":10}})),
            0,
            overall,
        )
        .pop()
        .unwrap();
    let prepared = timeout(Duration::from_secs(5), dataset)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(prepared.values.len(), 2);
    assert_eq!(*prepared.values["x"], InputValue::Number(42.0));
    assert_eq!(*prepared.values["limit"], InputValue::Number(10.0));
    assert_eq!(prepared.deadline.overall, overall);
    assert_eq!(prepared.deadline.input.as_deref(), Some("x"));
    prepared
        .deadline
        .check(&program.rules[0], Instant::now())
        .unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(fixture.channels_empty().await);
}

#[tokio::test]
async fn executes_matching_rules_and_preserves_decisions_and_failures() {
    use crate::interpreter::{LoadError, evaluator};

    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let source = r#"
        trigger [violation, passing] before "robot/move";
        trigger ignored before "robot/stop";
        rule passing {
            context string user = identity.user_id;
            input from "sensor/value" select [number x = reading, boolean ready = ready];
            require user = "alice" && ready && x > 0;
            enforce [deny, report "unused"];
        }
        rule violation {
            input from "sensor/value" select number x = reading;
            require x < 0;
            enforce [deny, report "blocked"];
        }
        rule ignored {
            context string missing = unknown.field;
            require missing = "unused";
            enforce deny;
        }
    "#;
    let interpreter = fixture.interpreter(source);
    let context = serde_json::json!({"identity":{"user_id":"alice"}});
    let result = timeout(
        Duration::from_secs(5),
        interpreter.on_arrival(check_context("plan", "move", "robot/move", &context), &()),
    )
    .await
    .unwrap();
    assert_eq!(result.effect, Effect::Deny);
    assert_eq!(
        result
            .reports
            .iter()
            .map(|report| report.rule.as_str())
            .collect::<Vec<_>>(),
        ["passing", "violation"]
    );
    assert!(result.reports[0].evaluation.as_ref().unwrap().passed());
    assert!(result.reports[0].enforcement.is_none());
    assert_eq!(
        result.reports[1]
            .evaluation
            .as_ref()
            .unwrap()
            .failed_requirements(),
        [1]
    );
    assert_eq!(result.reports[1].enforcement, Some(Ok(Effect::Deny)));
    assert_eq!(result.messages, ["blocked"]);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    let unmatched = interpreter
        .on_arrival(
            check_context("plan", "unmatched", "unmatched", &Value::Null),
            &(),
        )
        .await;
    assert_eq!(unmatched.effect, Effect::Allow);
    assert!(unmatched.reports.is_empty());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);

    let failed = fixture.interpreter(
        r#"trigger failure before always;
        rule failure {
        input from "sensor/missing" select number x;
        require x > 0; enforce [allow, report "unused"];
    }"#,
    );
    let result = failed
        .on_arrival(check_context("plan", "failure", "any", &Value::Null), &())
        .await;
    assert_eq!(result.effect, Effect::Deny);
    assert!(matches!(
        &result.reports[0].evaluation,
        Err(evaluator::Error::Input(InputError {
            kind: InputErrorKind::NoActiveProvider,
            ..
        }))
    ));
    assert!(result.reports[0].enforcement.is_none());
    assert!(result.messages.is_empty());
    assert!(fixture.channels_empty().await);

    let demo = fixture.interpreter(include_str!(
        "../../../../../examples/webots/sentinel-rules.v1.srdl"
    ));
    for (path, effect) in [
        ("protected.txt", Effect::Deny),
        ("notes.txt", Effect::Allow),
    ] {
        let result = demo
            .on_arrival(
                check_context(
                    "demo",
                    path,
                    "robonix/system/executor/builtin/write_file",
                    &serde_json::json!({"arguments":{"path":path}}),
                ),
                &(),
            )
            .await;
        assert_eq!(result.effect, effect);
    }

    assert!(matches!(
        SrdlInterpreter::load(
            "invalid",
            fixture.adapter.atlas.clone(),
            strategy::StrategyConfig::default()
        ),
        Err(LoadError::Parse(_))
    ));
    assert!(matches!(
        SrdlInterpreter::load(
            r#"trigger invalid before "never/matches";
        rule invalid { require unknown; enforce deny;
    }"#,
            fixture.adapter.atlas.clone(),
            strategy::StrategyConfig::default(),
        ),
        Err(LoadError::Type(_))
    ));
}

#[tokio::test]
async fn acquires_typed_field_groups_once_and_cleans_channels() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    for (source, fields, expected) in [
        (
            "value",
            "number velocity = reading maxage 0.1, boolean ready = ready maxage 2,
                string label = label maxage 1, number ratio = ratio maxage 1",
            vec![
                ("velocity", InputValue::Number(42.0), 0.1),
                ("ready", InputValue::Boolean(true), 2.0),
                ("label", InputValue::String("42".into()), 1.0),
                ("ratio", InputValue::Number(1.5), 1.0),
            ],
        ),
        (
            "scalar",
            "number battery maxage 1",
            vec![("battery", InputValue::Number(42.0), 1.0)],
        ),
    ] {
        let program = parse(&format!(
            r#"rule r {{
                input from "sensor/{source}" select [{fields}];
                require true; enforce deny; }}"#
        ))
        .unwrap();
        let before = fixture.calls.load(Ordering::SeqCst);
        let started = Instant::now();
        let acquired = fixture.collect(&program.rules[0]).await.unwrap();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), before + 1);
        assert_eq!(acquired.len(), 1);
        assert_eq!(acquired[0].fields.len(), expected.len());
        for (selected, (name, expected, maxage)) in acquired[0].fields.iter().zip(expected) {
            assert_eq!(
                (selected.field.name().0.as_str(), selected.field.maxage()),
                (name, Some(maxage))
            );
            assert_eq!(selected.value, expected);
        }
        assert!(acquired[0].acquired_at >= started && acquired[0].acquired_at <= Instant::now());
        assert!(std::ptr::eq(acquired[0].input, &program.rules[0].inputs[0]));
        assert!(fixture.channels_empty().await);
    }
    let program = parse(
        r#"rule r {
            input from "sensor/value" select [number velocity = reading maxage 1,
                boolean ready = missing maxage 2];
            require true; enforce deny; }"#,
    )
    .unwrap();
    let error = fixture.collect(&program.rules[0]).await.unwrap_err();
    assert_eq!(error.input, "ready");
    assert!(
        matches!(error.kind, InputErrorKind::Selection(select::SelectError {
        step: 1, kind: select::SelectErrorKind::MissingField(field),
    }) if field == "missing")
    );
    assert!(fixture.channels_empty().await);

    let failed = rule(
        r#"input from "sensor/error" select number x = reading maxage 1;"#,
        "require x > 0;",
    );
    let error = fixture.collect(&failed).await.unwrap_err();
    assert_eq!((&*error.rule, &*error.input), ("r", "x"));
    assert!(matches!(
        error.kind,
        InputErrorKind::Acquisition(transport::Error::Mcp(_))
    ));
    assert!(fixture.channels_empty().await);
}

#[tokio::test]
async fn rejects_type_mismatches_without_coercion_and_preserves_error_context() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    for (field, expected, actual) in [
        ("number x = ready", InputType::Number, "boolean"),
        ("number x = label", InputType::Number, "string"),
        ("boolean x = reading", InputType::Boolean, "number"),
        ("string x = reading", InputType::String, "number"),
        ("number x = nil", InputType::Number, "null"),
        ("number x = items", InputType::Number, "array"),
        ("number x", InputType::Number, "object"),
    ] {
        let program = parse(&format!(
            r#"rule typed {{
            input from "sensor/value" select {field} maxage 1;
            require true; enforce deny; }}"#
        ))
        .unwrap();
        let error = fixture.collect(&program.rules[0]).await.unwrap_err();
        assert_eq!(
            (&*error.rule, &*error.input, &*error.capability),
            ("typed", "x", "sensor/value")
        );
        assert!(
            matches!(
                error.kind,
                InputErrorKind::TypeMismatch { expected: found_expected, actual: found_actual }
                    if found_expected == expected && found_actual == actual
            ),
            "{field}"
        );
        assert!(fixture.channels_empty().await);
    }
}

#[tokio::test]
async fn interpreter_checks_rules_concurrently_and_cancellation_releases_channels() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let source = (0..MAX_CONCURRENT_BINDINGS + 1)
        .map(|index| {
            format!(
                r#"trigger r{index} before always;
        rule r{index} {{
        input from "sensor/slow" select number x;
        require x > 0; enforce deny;
    }}"#
            )
        })
        .collect::<String>();
    let interpreter = fixture.interpreter(&source);
    let mut check =
        Box::pin(interpreter.on_arrival(check_context("plan", "cancel", "any", &Value::Null), &()));
    timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = async {
                while fixture.calls.load(Ordering::SeqCst) < MAX_CONCURRENT_BINDINGS {
                    tokio::task::yield_now().await;
                }
            } => {},
            result = &mut check => panic!("slow checks completed unexpectedly: {:?}", result.effect),
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        MAX_CONCURRENT_BINDINGS
    );
    assert!(!fixture.channels_empty().await);
    drop(check);
    fixture.wait_for_cleanup().await;
}

#[tokio::test]
async fn pool_checks_events_independently_with_fresh_inputs() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let source = r#"
        trigger [contextual, measured] after ["robot/pick", "robot/place"];
        trigger measured before "robot/pick";
        rule measured {
            input from "sensor/value" select number x = reading maxage 1;
            require x = 42; enforce allow;
        }
        rule contextual {
            context boolean ready = arguments.ready;
            require ready; enforce [allow, report "not ready"];
        }
    "#;
    let interpreter = fixture.interpreter(source);
    let values = serde_json::json!({"arguments":{"ready":false}});
    let context = check_context("p", "same", "robot/pick", &values);
    // Completion checks acquire inputs independently of arrival checks.
    let after = interpreter.on_complete(context, &(), None, true).await;
    assert_eq!(after.effect, Effect::Allow);
    assert_eq!(after.reports.len(), 2);
    assert_eq!(after.messages, ["not ready"]);
    assert!(after.reports.iter().all(|r| {
        r.timing == TriggerTiming::After
            && r.enforcement
                .as_ref()
                .is_none_or(|effect| matches!(effect, Ok(Effect::Allow)))
    }));
    let (before, after) = tokio::join!(
        interpreter.on_arrival(context, &()),
        interpreter.on_complete(context, &(), None, true),
    );
    assert_eq!(before.effect, Effect::Allow);
    assert_eq!(before.reports.len(), 1);
    assert_eq!(after.reports.len(), 2);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
    let repeated = interpreter.on_arrival(context, &()).await;
    assert_eq!(repeated.effect, Effect::Allow);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 4);
    let unmatched = interpreter
        .on_arrival(
            CheckContext {
                call_id: "place",
                capability: "robot/place",
                ..context
            },
            &(),
        )
        .await;
    assert!(unmatched.reports.is_empty());
}

/// Holds capability execution open so another check can exercise the same pool.
#[derive(Default)]
struct BlockingHook {
    /// Signals that requirement evaluation has already reached hook execution.
    entered: tokio::sync::Notify,
    /// Releases the hook after the unrelated check completes.
    release: tokio::sync::Notify,
}

#[tonic::async_trait]
impl crate::interpreter::enforcer::HookRuntime for BlockingHook {
    async fn call(&self, _: &str) -> Result<(), crate::interpreter::enforcer::Error> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[tokio::test]
async fn slow_hooks_release_the_evaluator_pool_before_execution() {
    let fixture = AtlasFixture::new().await;
    let interpreter = fixture.interpreter(
        r#"
        trigger slow before A;
        trigger fast before B;
        rule slow { require false; enforce call "robot/stop"; }
        rule fast { require false; enforce deny; }
    "#,
    );
    let runtime = BlockingHook::default();
    let context = check_context("p", "slow", "A", &Value::Null);
    let slow = interpreter.on_arrival(context, &runtime);
    let fast = async {
        runtime.entered.notified().await;
        let decision = timeout(
            Duration::from_secs(1),
            interpreter.on_arrival(check_context("p", "fast", "B", &Value::Null), &()),
        )
        .await
        .unwrap();
        assert_eq!(decision.effect, Effect::Deny);
        runtime.release.notify_one();
    };
    let (slow, ()) = tokio::join!(slow, fast);
    assert_eq!(slow.effect, Effect::Allow);
}

#[tokio::test]
async fn admission_conflicts_during_hooks_preserve_completed_call_feedback() {
    let fixture = AtlasFixture::new().await;
    let interpreter = fixture.interpreter(
        r#"exclude mutual A B; trigger guard before A;
        rule guard { require false; enforce [call "robot/stop", report "stop requested"]; }"#,
    );
    let runtime = BlockingHook::default();
    let context = check_context("p", "a", "A", &Value::Null);
    let first = interpreter.on_arrival(context, &runtime);
    let conflict = async {
        runtime.entered.notified().await;
        let blocked = interpreter
            .on_arrival(check_context("q", "b", "B", &Value::Null), &())
            .await;
        assert_eq!(blocked.effect, Effect::Deny);
        runtime.release.notify_one();
    };
    let (result, ()) = timeout(Duration::from_secs(1), async {
        tokio::join!(first, conflict)
    })
    .await
    .unwrap();
    assert_eq!(result.effect, Effect::Deny);
    assert!(result.admission_id.is_none());
    assert_eq!(result.messages, ["stop requested"]);
    assert_eq!(result.calls[0].result, Some(Ok(())));
    assert_eq!(result.reports.len(), 2);
    assert!(matches!(
        result.reports[1].evaluation,
        Err(crate::interpreter::evaluator::Error::Exclusion { .. })
    ));
    let admitted = interpreter
        .on_arrival(check_context("q", "b", "B", &Value::Null), &())
        .await;
    assert_eq!(admitted.effect, Effect::Allow);
    interpreter
        .on_complete(
            CheckContext {
                plan_id: "q",
                call_id: "b",
                capability: "B",
                ..context
            },
            &(),
            admitted.admission_id,
            false,
        )
        .await;
}

#[tokio::test]
async fn completion_releases_exclusions_and_checks_after_rules_only_on_success() {
    let fixture = AtlasFixture::new().await;
    let source = r#"exclude mutual A B;
        trigger report after A;
        rule report { require false; enforce report "observed violation"; }"#;
    let interpreter = fixture.interpreter(source);
    let context = check_context("p", "a", "A", &Value::Null);
    let other = CheckContext {
        plan_id: "q",
        call_id: "b",
        capability: "B",
        ..context
    };
    for succeeded in [true, false] {
        let a = interpreter.on_arrival(context, &()).await;
        assert_eq!(a.effect, Effect::Allow);
        assert!(a.reports.is_empty());
        assert!(a.admission_id.is_some());
        let blocked = interpreter.on_arrival(other, &()).await;
        assert_eq!(blocked.effect, Effect::Deny);
        assert!(
            blocked.reports[0]
                .evaluation
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("replan required")
        );
        let after = interpreter
            .on_complete(context, &(), a.admission_id, succeeded)
            .await;
        assert_eq!(after.effect, Effect::Allow);
        assert_eq!(after.messages.len(), usize::from(succeeded));
        if succeeded {
            assert_eq!(after.messages, ["observed violation"]);
        } else {
            assert!(after.reports.is_empty());
        }
        let b = interpreter.on_arrival(other, &()).await;
        assert_eq!(b.effect, Effect::Allow);
        interpreter
            .on_complete(other, &(), b.admission_id, false)
            .await;
    }
}

#[tokio::test]
async fn conflicting_arrival_invalidates_an_in_flight_rule_check() {
    let mut fixture = AtlasFixture::new().await;
    let entered = fixture.serve_mcp().await;
    let source = r#"exclude mutual A B; trigger slow before A;
        rule slow { input from "sensor/slow" select boolean ready; require ready; enforce deny; }"#;
    let interpreter = fixture.interpreter(source);
    let mut first =
        Box::pin(interpreter.on_arrival(check_context("p", "a", "A", &Value::Null), &()));
    timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = entered.notified() => {},
            _ = &mut first => panic!("slow check finished before the conflicting arrival"),
        }
    })
    .await
    .unwrap();
    let second = interpreter
        .on_arrival(check_context("q", "b", "B", &Value::Null), &())
        .await;
    assert_eq!(second.effect, Effect::Deny);
    let first = timeout(Duration::from_secs(7), first).await.unwrap();
    assert_eq!(first.effect, Effect::Deny);
    assert!(
        first
            .reports
            .iter()
            .any(|report| matches!(report.evaluation, Err(Error::Exclusion { .. })))
    );
    let retry = interpreter
        .on_arrival(check_context("q", "b", "B", &Value::Null), &())
        .await;
    assert_eq!(retry.effect, Effect::Allow);
    interpreter
        .on_complete(
            check_context("q", "b", "B", &Value::Null),
            &(),
            retry.admission_id,
            false,
        )
        .await;
}

/// Allows checks to finish cleanup after their runtime signals cancellation.
#[derive(Default)]
struct CancellableRuntime {
    /// Wakes an active lifecycle check without dropping its future.
    cancelled: tokio::sync::Notify,
}

#[tonic::async_trait]
impl crate::interpreter::enforcer::HookRuntime for CancellableRuntime {
    async fn call(&self, _: &str) -> Result<(), crate::interpreter::enforcer::Error> {
        unreachable!("cancellation rules have no corrective calls")
    }

    async fn cancelled(&self) {
        self.cancelled.notified().await;
    }
}

#[tokio::test]
async fn cancelled_lifecycle_checks_release_inputs_and_exclusions() {
    let mut fixture = AtlasFixture::new().await;
    let entered = fixture.serve_mcp().await;
    for (name, timing) in [
        ("before", TriggerTiming::Before),
        ("after", TriggerTiming::After),
    ] {
        let source = format!(
            r#"exclude mutual A B; trigger hold {name} A;
            rule hold {{ input from "sensor/slow" select boolean ready;
                require ready; enforce allow; }}"#
        );
        let interpreter = fixture.interpreter(&source);
        let runtime = CancellableRuntime::default();
        let context = check_context("p", "a", "A", &Value::Null);
        let admission = if timing == TriggerTiming::After {
            interpreter.on_arrival(context, &()).await.admission_id
        } else {
            None
        };
        let mut checking = Box::pin(async {
            match timing {
                TriggerTiming::Before => interpreter.on_arrival(context, &runtime).await,
                TriggerTiming::After => {
                    interpreter
                        .on_complete(context, &runtime, admission, true)
                        .await
                }
                TriggerTiming::While => unreachable!(),
            }
        });
        timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = entered.notified() => {},
                _ = &mut checking => panic!("slow check completed unexpectedly"),
            }
        })
        .await
        .unwrap();
        runtime.cancelled.notify_one();
        let result = checking.await;
        assert_eq!(
            result.effect,
            if timing == TriggerTiming::Before {
                Effect::Deny
            } else {
                Effect::Allow
            }
        );
        assert!(result.admission_id.is_none());
        assert!(result.reports[0].evaluation.is_err());
        fixture.wait_for_cleanup().await;
        let other = CheckContext {
            call_id: "b",
            capability: "B",
            ..context
        };
        let next = interpreter.on_arrival(other, &()).await;
        assert_eq!(next.effect, Effect::Allow);
        interpreter
            .on_complete(other, &(), next.admission_id, false)
            .await;
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn next_arrival_prunes_an_abandoned_check_before_reusing_its_call_id() {
    let mut fixture = AtlasFixture::new().await;
    let entered = fixture.serve_mcp().await;
    let interpreter = fixture.interpreter(
        r#"
        exclude mutual A B;
        trigger hold before A;
        rule hold { input from "sensor/slow" select boolean ready; require ready; enforce deny; }
    "#,
    );
    let mut arrival =
        Box::pin(interpreter.on_arrival(check_context("p", "a", "A", &Value::Null), &()));
    tokio::select! {
        result = &mut arrival => panic!("slow arrival finished: {result:?}"),
        result = timeout(Duration::from_secs(5), entered.notified()) => result.unwrap(),
    }
    drop(arrival);
    let other = check_context("p", "a", "B", &Value::Null);
    let decision = interpreter.on_arrival(other, &()).await;
    assert_eq!(decision.effect, Effect::Allow);
    interpreter
        .on_complete(other, &(), decision.admission_id, false)
        .await;
    fixture.wait_for_cleanup().await;
}

async fn evaluate_inputs<'a>(
    check_id: u64,
    context: &Value,
    rule: &'a Rule,
    inputs: impl Stream<Item = Result<AcquiredInput<'a>, InputError>>,
) -> Result<RuleResult, Error> {
    let prepared = prepare_inputs(
        check_id,
        context,
        rule,
        None,
        Instant::now() + RULE_TIMEOUT,
        inputs,
    )
    .await?;
    crate::interpreter::evaluator::evaluate(check_id, rule, &prepared)
}

/// Simulate Adapter receipt notifications while retaining the real snapshot acquisition path.
async fn prepare_inputs<'a>(
    check_id: u64,
    context: &Value,
    rule: &'a Rule,
    names: Option<&BTreeSet<&str>>,
    overall: Instant,
    inputs: impl Stream<Item = Result<AcquiredInput<'a>, InputError>>,
) -> Result<Prepared, Error> {
    let (deadline, receive) = watch::channel(Deadline::new(overall));
    let inputs = inputs.map(move |item| {
        if let Ok(acquired) = &item {
            for selected in &acquired.fields {
                let age = selected
                    .field
                    .maxage()
                    .and_then(|age| Duration::try_from_secs_f64(age).ok());
                deadline.send_modify(|deadline| {
                    deadline.record(
                        acquired.acquired_at,
                        age,
                        Arc::from(selected.field.name().0.as_str()),
                    );
                });
            }
        }
        item
    });
    InputAdapter::prepare(check_id, context, rule, names, receive, inputs).await
}

fn rule(inputs: &str, requirements: &str) -> Rule {
    parse(&format!(
        "rule r {{ {inputs} {requirements} enforce allow; }}"
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
    assert_eq!(input.selects.len(), values.len());
    AcquiredInput {
        input,
        fields: input
            .selects
            .iter()
            .zip(values)
            .map(|(field, value)| FieldValue {
                field: InputFieldRef::Select(field),
                value,
            })
            .collect(),
        acquired_at,
    }
}

const RULE_TIMEOUT: Duration = Duration::from_secs(5);

const TWO_INPUTS: &str = r#"
    input from "a" select number a maxage 1;
    input from "b" select number b maxage 2;
"#;

#[tokio::test(start_paused = true)]
async fn unbounded_inputs_still_obey_the_overall_timeout() {
    let rule = rule(
        r#"input from "a" select number a;
        input from "b" select number b;"#,
        "require a = b;",
    );
    assert!(matches!(
        evaluate_inputs(0, &Value::Null, &rule, stream::pending()).await,
        Err(Error::Timeout { .. })
    ));
    let first = batch(&rule, 0, vec![InputValue::Number(1.0)], Instant::now());
    let started = Instant::now();
    assert!(matches!(
        evaluate_inputs(
            0,
            &Value::Null,
            &rule,
            stream::iter([Ok(first)]).chain(stream::pending())
        )
        .await,
        Err(Error::Timeout { .. })
    ));
    assert_eq!(Instant::now() - started, RULE_TIMEOUT);
    let first = batch(&rule, 0, vec![InputValue::Number(1.0)], Instant::now());
    sleep(Duration::from_secs(1)).await;
    let inputs = stream::iter([Ok(first)]).chain(stream::once(async {
        sleep(Duration::from_secs(1)).await;
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
    // Context bindings do not remove the overall input deadline.
    assert!(matches!(
        evaluate_inputs(0, &context, &slow, stream::pending()).await,
        Err(Error::Timeout { .. })
    ));
    let first = batch(&slow, 0, vec![InputValue::Number(0.0)], Instant::now());
    let inputs = stream::iter([Ok(first)]).chain(stream::once(async {
        sleep(Duration::from_secs(1)).await;
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
async fn waits_for_all_inputs_before_evaluating_any_predicate() {
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
        Err(Error::Expired { input, .. }) if input == "b"
    ));
}

#[tokio::test(start_paused = true)]
async fn later_receipts_do_not_relax_the_earliest_snapshot_expiry() {
    let rule = rule(TWO_INPUTS, "require a = b;");
    let now = Instant::now();
    let inputs = stream::iter([
        Ok(batch(
            &rule,
            1,
            vec![InputValue::Number(1.0)],
            now - Duration::from_millis(1500),
        )),
        Ok(batch(&rule, 0, vec![InputValue::Number(1.0)], now)),
    ]);
    let prepared = prepare_inputs(0, &Value::Null, &rule, None, now + RULE_TIMEOUT, inputs)
        .await
        .unwrap();
    sleep(Duration::from_millis(750)).await;
    assert!(
        matches!(prepared.deadline.check(&rule, Instant::now()), Err(Error::Expired { input, .. }) if input == "b")
    );
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
        rule.inputs[0].selects[0].maxage = Some(maxage);
        assert!(matches!(
            evaluate_inputs(0, &Value::Null, &rule, stream::pending()).await,
            Err(Error::InvalidMaxage { .. })
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn overall_timeout_precedes_simultaneous_expiry_and_input_arrival() {
    let rule = rule(
        r#"input from "a" select number a maxage 1;
        input from "b" select number b;"#,
        "require a = b;",
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    let first = batch(&rule, 0, vec![InputValue::Number(1.0)], Instant::now());
    let inputs = stream::iter([Ok(first)]).chain(stream::once(async {
        sleep(Duration::from_secs(1)).await;
        Ok(batch(
            &rule,
            1,
            vec![InputValue::Number(1.0)],
            Instant::now(),
        ))
    }));
    assert!(matches!(
        prepare_inputs(0, &Value::Null, &rule, None, deadline, inputs).await,
        Err(Error::Timeout { .. })
    ));
}

/// Test transformation that asserts its input and returns configured JSON.
struct TestStrategy {
    /// JSON required from the preceding step.
    expected: Value,
    /// JSON emitted to the following step.
    output: Value,
    /// Rendered configuration expected for this invocation.
    arguments: Vec<String>,
    /// Semantic output description passed through the pipeline.
    description: &'static str,
}

impl strategy::Strategy for TestStrategy {
    fn process<'a>(
        &'a self,
        input: Value,
        arguments: &'a [String],
        fields: &'a [crate::srdl::ast::YieldField],
    ) -> BoxFuture<'a, anyhow::Result<Value>> {
        Box::pin(async move {
            anyhow::ensure!(input == self.expected, "unexpected pipeline input: {input}");
            assert_eq!(arguments, self.arguments);
            assert_eq!(fields[0].description, self.description);
            Ok(self.output.clone())
        })
    }
}

#[tokio::test]
async fn loading_checks_rules_and_strategy_configuration_before_acquisition() {
    let fixture = AtlasFixture::new().await;
    let source = r#"rule r {
        input from "sensor/scalar" yield number n = "Count" using Summarize("Interpret");
        require n > 0; enforce deny;
    } trigger r before "test";"#;
    let error = SrdlInterpreter::load(
        source,
        fixture.adapter.atlas.clone(),
        strategy::StrategyConfig::default(),
    )
    .err()
    .unwrap();
    assert!(matches!(error, LoadError::Strategy(_)));
    assert!(
        error
            .to_string()
            .contains("strategy 'Summarize' is not configured")
    );
    let invalid = strategy::StrategyConfig {
        summarize: Some(strategy::SummarizeConfig {
            api_base: "https://example.invalid/v1".into(),
            model: String::new(),
            api_key: "test-private-key".into(),
        }),
    };
    assert!(matches!(
        SrdlInterpreter::load(
            "rule r { require missing; enforce deny; }",
            fixture.adapter.atlas.clone(),
            invalid.clone()
        ),
        Err(LoadError::Type(_))
    ));
    let error = SrdlInterpreter::load(source, fixture.adapter.atlas.clone(), invalid)
        .err()
        .unwrap();
    assert!(matches!(error, LoadError::Strategy(_)));
    assert!(error.to_string().contains("requires a model"));
    assert!(!error.to_string().contains("test-private-key"));
    let valid = strategy::StrategyConfig {
        summarize: Some(strategy::SummarizeConfig {
            api_base: "https://example.invalid/v1".into(),
            model: "test-model".into(),
            api_key: "test-key".into(),
        }),
    };
    for args in ["", r#""a", "b""#] {
        let invalid_call = source.replace(r#""Interpret""#, args);
        let error =
            SrdlInterpreter::load(&invalid_call, fixture.adapter.atlas.clone(), valid.clone())
                .err()
                .unwrap();
        assert!(error.to_string().contains("exactly one hint"), "{error}");
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.channels_empty().await);
}

#[tokio::test]
async fn runs_independent_yield_pipelines_with_context_and_validates_outputs() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let mut strategies = strategy::Strategies::default();
    strategies.register(
        "First",
        Arc::new(TestStrategy {
            expected: serde_json::json!({"0":42}),
            output: serde_json::json!({"intermediate":7}),
            arguments: vec!["target pan {literal}".into()],
            description: "Count",
        }),
    );
    strategies.register(
        "Second",
        Arc::new(TestStrategy {
            expected: serde_json::json!({"intermediate":7}),
            output: serde_json::json!({"n":7,"extra":true}),
            arguments: vec![],
            description: "Count",
        }),
    );
    strategies.register(
        "Independent",
        Arc::new(TestStrategy {
            expected: serde_json::json!({"0":42}),
            output: serde_json::json!({"ready":true}),
            arguments: vec![],
            description: "Ready",
        }),
    );
    let interpreter = fixture
        .interpreter_with_strategies(
            r#"
        rule r {
            context string target = arguments.target;
            input from ["sensor/scalar"] select [number raw = 0],
                yield [number n = "Count"] using [First("target {target} {{literal}}"), Second()],
                yield [boolean ready = "Ready"] using [Independent()];
            require raw = 42 && n = 7 && ready; enforce deny;
        }
        trigger r before "test";
    "#,
            strategies,
        )
        .unwrap();
    let result = interpreter
        .on_arrival(
            check_context(
                "pipeline",
                "call",
                "test",
                &serde_json::json!({"arguments":{"target":"pan"}}),
            ),
            &(),
        )
        .await;
    assert_eq!(result.effect, Effect::Allow, "{result:?}");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    fixture.wait_for_cleanup().await;

    let source = r#"rule r { input from "sensor/scalar"
        yield number n = "Count" using Wrong();
        require n > 0; enforce deny; } trigger r before "test";"#;
    let mut strategies = strategy::Strategies::default();
    assert!(matches!(
        fixture.interpreter_with_strategies(source, strategies.clone()),
        Err(LoadError::Strategy(_))
    ));
    strategies.register(
        "Wrong",
        Arc::new(TestStrategy {
            expected: serde_json::json!(42),
            output: serde_json::json!({"n":"wrong"}),
            arguments: vec![],
            description: "Count",
        }),
    );
    let interpreter = fixture
        .interpreter_with_strategies(source, strategies)
        .unwrap();
    let result = interpreter
        .on_arrival(check_context("failure", "wrong", "test", &Value::Null), &())
        .await;
    assert_eq!(result.effect, Effect::Deny);
    assert!(matches!(
        &result.reports[0].evaluation,
        Err(Error::Input(InputError {
            kind: InputErrorKind::TypeMismatch { .. },
            ..
        }))
    ));
    fixture.wait_for_cleanup().await;
}

#[tokio::test]
async fn acquires_numbered_sources_in_declaration_order() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let program = parse(
        r#"rule r {
        input from ["sensor/value", "sensor/scalar"]
            select [number reading = 0.reading, number scalar = 1];
        require reading = scalar; enforce deny;
    }"#,
    )
    .unwrap();
    let acquired = fixture.collect(&program.rules[0]).await.unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    assert!(
        acquired[0]
            .fields
            .iter()
            .all(|field| field.value == InputValue::Number(42.0))
    );
    fixture.wait_for_cleanup().await;
}

/// Suspends processing until Adapter drops the pipeline at its deadline.
struct SuspendedStrategy {
    /// Signals that the strategy is waiting rather than acquiring transport data.
    entered: Arc<tokio::sync::Notify>,
    /// Counts processing cleanup on cancellation.
    dropped: Arc<AtomicUsize>,
}

/// Records cancellation of an in-flight strategy invocation.
struct ProcessingGuard {
    /// Counter updated when processing completes or is cancelled.
    dropped: Arc<AtomicUsize>,
}

impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl strategy::Strategy for SuspendedStrategy {
    fn process<'a>(
        &'a self,
        _: Value,
        _: &'a [String],
        _: &'a [crate::srdl::ast::YieldField],
    ) -> BoxFuture<'a, anyhow::Result<Value>> {
        Box::pin(async move {
            let _guard = ProcessingGuard {
                dropped: self.dropped.clone(),
            };
            self.entered.notify_one();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn deadlines_cancel_suspended_strategy_processing() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    for (maxage, expired) in [("maxage 0.1", true), ("", false)] {
        let entered = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut strategies = strategy::Strategies::default();
        strategies.register(
            "Wait",
            Arc::new(SuspendedStrategy {
                entered: entered.clone(),
                dropped: dropped.clone(),
            }),
        );
        let adapter = fixture.adapter.clone().with_strategies(strategies);
        let program = parse(&format!(
            r#"rule r {{
            input from "sensor/scalar" yield [number n = "Count" {maxage}] using [Wait()];
            require n > 0; enforce deny;
        }}"#
        ))
        .unwrap();
        let check = adapter
            .acquire_group(
                [&program.rules[0]],
                "sentinel-test",
                Arc::new(Value::Null),
                0,
                Instant::now() + Duration::from_secs(1),
            )
            .pop()
            .unwrap();
        tokio::pin!(check);
        tokio::select! {
            _ = entered.notified() => {},
            result = &mut check => panic!("check ended before strategy started: {:?}", result.err()),
        }
        let result = timeout(Duration::from_secs(2), check).await.unwrap();
        assert!(if expired {
            matches!(result, Err(Error::Expired { .. }))
        } else {
            matches!(result, Err(Error::Timeout { .. }))
        });
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        fixture.wait_for_cleanup().await;
    }
}

#[tokio::test]
async fn summarize_calls_model_and_feeds_validated_yield_inputs() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let model_calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let (calls, waiting) = (model_calls.clone(), entered.clone());
    let router = axum::Router::new().route("/v1/chat/completions", axum::routing::post(
        move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let (calls, waiting) = (calls.clone(), waiting.clone());
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(headers["authorization"], "Bearer test-api-key");
                let request: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(request["model"], "test-model");
                assert_eq!(request["stream"], false);
                let prompt: Value = serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
                assert_eq!(prompt["input"], 42);
                let mut expected = serde_json::json!({"name":"n", "type":"number", "description":"Observed count"});
                if prompt["yield_fields"][0]["type"] == "option<number>" {
                    expected["type"] = serde_json::json!("option<number>");
                    expected["allowed_values"] = serde_json::json!([42.0]);
                }
                assert_eq!(prompt["yield_fields"], serde_json::json!([expected]));
                let hint = prompt["hint"].as_str().unwrap();
                if hint == "wait" {
                    waiting.notify_one();
                    std::future::pending::<()>().await;
                }
                let (status, response) = match hint {
                    "http-error" => (axum::http::StatusCode::BAD_GATEWAY,
                        serde_json::json!({"error":"private-upstream-detail"})),
                    hint => {
                        let content = match hint {
                            "invalid-json" => "not JSON",
                            "non-object" => "[]",
                            "missing" => "{}",
                            "extra" => r#"{"n":42,"extra":null}"#,
                            "wrong-type" => r#"{"n":"42"}"#,
                            "unknown" => r#"{"n":null}"#,
                            _ => r#"{"n":42}"#,
                        };
                        (axum::http::StatusCode::OK, serde_json::json!({"choices":[{
                            "finish_reason": if hint == "incomplete" { "length" } else { "stop" },
                            "message": {"content":content, "refusal": if hint == "refusal" { Some("refused") } else { None }},
                        }]}))
                    }
                };
                (status, [(axum::http::header::CONTENT_TYPE, "application/json")], response.to_string())
            }
        }
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1/", listener.local_addr().unwrap());
    fixture.servers.push(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));
    let model = Arc::new(strategy::Summarize::new(&base, "test-model", "test-api-key").unwrap());
    let config = strategy::StrategyConfig {
        summarize: Some(strategy::SummarizeConfig {
            api_base: base,
            model: "test-model".into(),
            api_key: "test-api-key".into(),
        }),
    };
    let source = r#"
        rule r {
            context string hint = arguments.hint;
            input from "sensor/scalar"
                yield number n = "Observed count" using Summarize("{hint}");
            require n = 42; enforce deny;
        }
        trigger r before "test";
    "#;
    let interpreter = SrdlInterpreter::load(source, fixture.adapter.atlas.clone(), config).unwrap();
    for hint in [
        "good",
        "http-error",
        "invalid-json",
        "non-object",
        "missing",
        "extra",
        "wrong-type",
        "unknown",
        "incomplete",
        "refusal",
    ] {
        let result = interpreter
            .on_arrival(
                check_context(
                    "summarize",
                    hint,
                    "test",
                    &serde_json::json!({"arguments":{"hint":hint}}),
                ),
                &(),
            )
            .await;
        assert_eq!(
            result.effect,
            if hint == "good" {
                Effect::Allow
            } else {
                Effect::Deny
            },
            "{hint}: {result:?}"
        );
        assert_eq!(
            result.reports[0].evaluation.is_ok(),
            hint == "good",
            "{hint}"
        );
        let feedback = format!("{result:?}");
        assert!(
            !feedback.contains("test-api-key") && !feedback.contains("private-upstream-detail")
        );
    }
    fixture.wait_for_cleanup().await;

    use strategy::Strategy;
    let optional_program =
        parse(&source.replace("yield number n", "yield option<enum number { 42 }> n")).unwrap();
    let optional_fields = &optional_program.rules[0].inputs[0].yields[0].fields;
    let unknown = vec!["unknown".to_owned()];
    let output = model
        .process(serde_json::json!(42), &unknown, optional_fields)
        .await
        .unwrap();
    assert_eq!(
        FieldValue::from_json(&output, InputFieldRef::Yield(&optional_fields[0]))
            .unwrap()
            .value,
        InputValue::None(InputType::Number)
    );

    let program = parse(source).unwrap();
    let fields = &program.rules[0].inputs[0].yields[0].fields;
    let before = model_calls.load(Ordering::SeqCst);
    assert!(
        model
            .process(serde_json::json!(42), &[], fields)
            .await
            .is_err()
    );
    assert_eq!(model_calls.load(Ordering::SeqCst), before);
    let hint = vec!["wait".to_owned()];
    timeout(Duration::from_secs(1), async {
        tokio::select! {
            _ = entered.notified() => {},
            result = model.process(serde_json::json!(42), &hint, fields) => panic!("request finished before cancellation: {result:?}"),
        }
    }).await.unwrap();
    let hint = vec!["good".to_owned()];
    assert_eq!(
        model
            .process(serde_json::json!(42), &hint, fields)
            .await
            .unwrap(),
        serde_json::json!({"n":42})
    );
}

#[test]
fn validates_optional_selections_and_enum_membership() {
    let number_none = InputValue::None(InputType::Number);
    for (declaration, response, expected) in [
        (
            "option<number> n = missing",
            serde_json::json!({}),
            number_none.clone(),
        ),
        (
            "option<number> n = value",
            serde_json::json!({"value":null}),
            number_none.clone(),
        ),
        (
            "option<number> n = object.value",
            serde_json::json!({"object":null}),
            number_none.clone(),
        ),
        (
            r#"option<number> n = items[id = "target"].value"#,
            serde_json::json!({"items":[]}),
            number_none,
        ),
        (
            "enum number { 1, 2 } n = value",
            serde_json::json!({"value":2}),
            InputValue::Number(2.0),
        ),
        (
            r#"option<enum string { "A", "B" }> n = value"#,
            serde_json::json!({"value":"B"}),
            InputValue::Some(Box::new(InputValue::String("B".into()))),
        ),
    ] {
        let program = parse(&format!(
            r#"rule r {{ input from "source" select {declaration}; require true; enforce deny; }}"#
        ))
        .unwrap();
        let field = InputFieldRef::Select(&program.rules[0].inputs[0].selects[0]);
        assert_eq!(
            FieldValue::from_json(&response, field).unwrap().value,
            expected
        );
    }
    for (declaration, response) in [
        ("option<number> n = value", serde_json::json!({"value":"2"})),
        (
            "option<number> n = object.value",
            serde_json::json!({"object":3}),
        ),
        (
            r#"option<number> n = items[id = "target"].value"#,
            serde_json::json!({"items":[{"id":"target","value":1},{"id":"target","value":2}]}),
        ),
        (
            "enum number { 1, 2 } n = value",
            serde_json::json!({"value":3}),
        ),
        (
            r#"option<enum string { "A", "B" }> n = value"#,
            serde_json::json!({"value":"unknown"}),
        ),
    ] {
        let program = parse(&format!(
            r#"rule r {{ input from "source" select {declaration}; require true; enforce deny; }}"#
        ))
        .unwrap();
        let field = InputFieldRef::Select(&program.rules[0].inputs[0].selects[0]);
        assert!(
            FieldValue::from_json(&response, field).is_err(),
            "{declaration}: {response}"
        );
    }
    let program = parse(r#"rule r { input from "source" yield option<enum number { 1, 2 }> n = "Level" using Test(); require true; enforce deny; }"#).unwrap();
    let field = InputFieldRef::Yield(&program.rules[0].inputs[0].yields[0].fields[0]);
    assert_eq!(
        FieldValue::from_json(&serde_json::json!({"n":null}), field)
            .unwrap()
            .value,
        InputValue::None(InputType::Number)
    );
    assert!(FieldValue::from_json(&serde_json::json!({}), field).is_err());
    assert!(FieldValue::from_json(&serde_json::json!({"n":3}), field).is_err());
}

#[tokio::test]
async fn optional_inputs_use_fallback_results_and_match_scopes_values() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let interpreter = fixture.interpreter(
        r#"
        rule r {
            context boolean healthy = arguments.healthy;
            input from "sensor/value" select [
                option<number> absent = missing,
                option<enum string { "42" }> label = label,
                enum number { 42 } number = reading
            ];
            require healthy;
            require absent? > 0 | true;
            require healthy && match label { some(text) => text = "42", none => false };
            require match absent { some(number) => 1 / number > 0, none => true };
            require number = 42;
            enforce deny;
        }
        trigger r before "test";
    "#,
    );
    for healthy in [true, false] {
        let result = interpreter
            .on_arrival(
                check_context(
                    "optional",
                    if healthy { "good" } else { "bad" },
                    "test",
                    &serde_json::json!({"arguments":{"healthy":healthy}}),
                ),
                &(),
            )
            .await;
        assert_eq!(
            result.effect,
            if healthy { Effect::Allow } else { Effect::Deny }
        );
        let rule = result.reports[0].evaluation.as_ref().unwrap();
        assert_eq!(rule.requirements, [healthy, true, healthy, true, true]);
    }
    fixture.wait_for_cleanup().await;
}
