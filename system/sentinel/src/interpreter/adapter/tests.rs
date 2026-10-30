use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::TryStreamExt;
use robonix_atlas::service::{AtlasRegistry, AtlasService};
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};

use super::*;
use crate::srdl::parse;

fn rule(capability: &str) -> Rule {
    parse(&format!(
        r#"rule r {{ trigger always;
            input from "{capability}" select number x = reading maxage 1;
            require x > 0; enforce deny; }}"#
    ))
    .unwrap()
    .rules
    .remove(0)
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
        for name in ["value", "scalar", "error", "slow"] {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(request.arguments.unwrap_or_default().is_empty());
        Ok(match request.name.as_ref() {
            "value" => CallToolResult::structured(serde_json::json!({
                "reading":42, "ratio":1.5, "ready":true, "label":"42", "nil":null, "items":[]
            })),
            "scalar" => CallToolResult::success(vec![Content::text("42")]),
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
async fn executes_matching_rules_and_preserves_decisions_and_failures() {
    use crate::{
        Effect,
        interpreter::{LoadError, SrdlInterpreter, evaluator},
    };

    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let source = r#"
        rule passing {
            trigger capability "robot/move";
            context string user = identity.user_id;
            input from "sensor/value" select number x = reading, boolean ready = ready;
            require user = "alice" && ready && x > 0;
            enforce deny report "unused";
        }
        rule violation {
            trigger capability "robot/move";
            input from "sensor/value" select number x = reading;
            require x < 0;
            enforce deny report "blocked";
        }
        rule ignored {
            trigger capability "robot/stop";
            context string missing = unknown.field;
            require missing = "unused";
            enforce deny;
        }
    "#;
    let interpreter = SrdlInterpreter::new(source, fixture.adapter.atlas.clone()).unwrap();
    let context = serde_json::json!({"identity":{"user_id":"alice"}});
    let result = timeout(
        Duration::from_secs(5),
        interpreter.check("robot/move", "consumer", &context),
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
    assert!(result.reports[0].message.is_none());
    assert_eq!(
        result.reports[1]
            .evaluation
            .as_ref()
            .unwrap()
            .failed_requirements(),
        [1]
    );
    assert_eq!(result.reports[1].enforcement, Some(Ok(Effect::Deny)));
    assert_eq!(result.reports[1].message.as_deref(), Some("blocked"));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    let unmatched = interpreter
        .check("unmatched", "consumer", &Value::Null)
        .await;
    assert_eq!(unmatched.effect, Effect::Allow);
    assert!(unmatched.reports.is_empty());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);

    let failed = SrdlInterpreter::new(
        r#"rule failure {
        trigger always;
        input from "sensor/missing" select number x;
        require x > 0; enforce allow report "unused";
    }"#,
        fixture.adapter.atlas.clone(),
    )
    .unwrap();
    let result = failed.check("any", "consumer", &Value::Null).await;
    assert_eq!(result.effect, Effect::Deny);
    assert!(matches!(
        &result.reports[0].evaluation,
        Err(evaluator::Error::Input(InputError {
            kind: InputErrorKind::NoActiveProvider,
            ..
        }))
    ));
    assert!(result.reports[0].enforcement.is_none());
    assert!(result.reports[0].message.is_none());
    assert!(fixture.channels_empty().await);

    assert!(matches!(
        SrdlInterpreter::new("invalid", fixture.adapter.atlas.clone()),
        Err(LoadError::Parse(_))
    ));
    assert!(matches!(
        SrdlInterpreter::new(
            r#"rule invalid {
        trigger capability "never/matches"; require unknown; enforce deny;
    }"#,
            fixture.adapter.atlas.clone()
        ),
        Err(LoadError::Type(_))
    ));
}

#[tokio::test]
async fn acquires_field_groups_once_and_supports_scalar_roots() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    for (source, fields, expected) in [
        (
            "value",
            "number velocity = reading maxage 0.1, boolean ready = ready maxage 2,
                string label = label maxage 1, number ratio = ratio maxage 1",
            vec![
                ("velocity", serde_json::json!(42), 0.1),
                ("ready", serde_json::json!(true), 2.0),
                ("label", serde_json::json!("42"), 1.0),
                ("ratio", serde_json::json!(1.5), 1.0),
            ],
        ),
        (
            "scalar",
            "number battery maxage 1",
            vec![("battery", serde_json::json!(42), 1.0)],
        ),
    ] {
        let program = parse(&format!(
            r#"rule r {{ trigger always;
                input from "sensor/{source}" select {fields};
                require true; enforce deny; }}"#
        ))
        .unwrap();
        let before = fixture.calls.load(Ordering::SeqCst);
        let acquired: Vec<_> = timeout(
            Duration::from_secs(5),
            fixture
                .adapter
                .acquire(&program.rules[0], "sentinel-test")
                .try_collect(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), before + 1);
        assert_eq!(acquired.len(), 1);
        assert_eq!(acquired[0].fields.len(), expected.len());
        for (selected, (name, expected, maxage)) in acquired[0].fields.iter().zip(expected) {
            assert_eq!(
                (selected.field.name.0.as_str(), selected.field.maxage),
                (name, Some(maxage))
            );
            match (&selected.value, &expected) {
                (InputValue::Number(actual), serde_json::Value::Number(expected)) => {
                    assert_eq!(Some(*actual), expected.as_f64())
                }
                (InputValue::Boolean(actual), serde_json::Value::Bool(expected)) => {
                    assert_eq!(actual, expected)
                }
                (InputValue::String(actual), serde_json::Value::String(expected)) => {
                    assert_eq!(actual, expected)
                }
                _ => panic!(
                    "unexpected typed value {:?}, expected {expected}",
                    selected.value
                ),
            }
        }
        assert!(acquired[0].acquired_at <= Instant::now());
        assert!(fixture.channels_empty().await);
    }
    let program = parse(
        r#"rule r { trigger always;
            input from "sensor/value" select number velocity = reading maxage 1,
                boolean ready = missing maxage 2;
            require true; enforce deny; }"#,
    )
    .unwrap();
    let result: Result<Vec<_>, _> = timeout(
        Duration::from_secs(5),
        fixture
            .adapter
            .acquire(&program.rules[0], "sentinel-test")
            .try_collect(),
    )
    .await
    .unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.input, "ready");
    assert!(matches!(error.kind, InputErrorKind::Selection(_)));
    assert!(fixture.channels_empty().await);
}

#[tokio::test]
async fn acquires_mcp_json_and_cleans_channels_on_success_and_failure() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    for (name, field) in [
        ("value", "reading"),
        ("value", "missing"),
        ("error", "reading"),
    ] {
        let mut rule = rule(&format!("sensor/{name}"));
        rule.inputs[0].fields[0].select = vec![crate::srdl::ast::SelectStep::Field(
            crate::srdl::ast::Identifier(field.into()),
        )];
        let started = Instant::now();
        let result: Result<Vec<_>, _> = timeout(
            Duration::from_secs(5),
            fixture
                .adapter
                .acquire(&rule, "sentinel-test")
                .try_collect(),
        )
        .await
        .unwrap();
        match (name, field) {
            ("value", "reading") => {
                let values = result.unwrap();
                assert!(matches!(
                    values[0].fields[0].value,
                    InputValue::Number(42.0)
                ));
                assert!(
                    values[0].acquired_at >= started && values[0].acquired_at <= Instant::now()
                );
                assert!(std::ptr::eq(values[0].input, &rule.inputs[0]));
            }
            ("value", "missing") => {
                let error = result.unwrap_err();
                assert_eq!((&*error.rule, &*error.input), ("r", "x"));
                assert!(
                    matches!(error.kind, InputErrorKind::Selection(select::SelectError {
                        step: 1, kind: select::SelectErrorKind::MissingField(field),
                    }) if field == "missing")
                );
            }
            _ => {
                let error = result.unwrap_err();
                assert_eq!(error.rule, "r");
                assert_eq!(error.input, "x");
                assert!(matches!(
                    error.kind,
                    InputErrorKind::Acquisition(transport::Error::Mcp(_))
                ));
            }
        }
        assert!(fixture.channels_empty().await);
    }
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
            r#"rule typed {{ trigger always;
            input from "sensor/value" select {field} maxage 1;
            require true; enforce deny; }}"#
        ))
        .unwrap();
        let result: Result<Vec<_>, _> = timeout(
            Duration::from_secs(5),
            fixture
                .adapter
                .acquire(&program.rules[0], "sentinel-test")
                .try_collect(),
        )
        .await
        .unwrap();
        let error = result.unwrap_err();
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
    let source = (0..2)
        .map(|index| {
            format!(
                r#"rule r{index} {{
        trigger always;
        input from "sensor/slow" select number x;
        require x > 0; enforce deny;
    }}"#
            )
        })
        .collect::<String>();
    let interpreter =
        crate::interpreter::SrdlInterpreter::new(&source, fixture.adapter.atlas.clone()).unwrap();
    let mut check = Box::pin(interpreter.check("any", "sentinel-test", &Value::Null));
    timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = async {
                while fixture.calls.load(Ordering::SeqCst) < 2 {
                    tokio::task::yield_now().await;
                }
            } => {},
            result = &mut check => panic!("slow checks completed unexpectedly: {result:?}"),
        }
    })
    .await
    .unwrap();
    assert!(!fixture.channels_empty().await);
    drop(check);
    timeout(Duration::from_secs(5), async {
        while !fixture.channels_empty().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
