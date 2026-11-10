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
        enforcer::Effect,
        evaluator::{Error, RuleEvaluator, RuleResult, expression},
    },
    srdl::parse,
};

fn input_rule(capability: &str) -> Rule {
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
    fn acquire<'a>(&self, rule: &'a Rule) -> Inputs<'a> {
        let names = rule
            .inputs
            .iter()
            .flat_map(|input| &input.fields)
            .map(|field| field.name.0.as_str())
            .collect();
        self.adapter
            .fetch_group(
                [(rule, &names)],
                "sentinel-test",
                Instant::now() + Duration::from_secs(5),
            )
            .pop()
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
async fn returns_a_prepared_dataset_without_evaluating_predicates_or_fetching_postconditions() {
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let program = parse(
        r#"rule r {
        trigger always;
        context number limit = arguments.limit;
        input from "sensor/value" select number x = reading maxage 1;
        input from "sensor/slow" select number later;
        require x / 0 > limit;
        ensure later > 0;
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
            false,
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
    use crate::interpreter::{LoadError, SrdlInterpreter, evaluator};

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
    let (result, _) = timeout(
        Duration::from_secs(5),
        interpreter.begin("plan", "move", "robot/move", "consumer", &context),
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
    let (unmatched, _) = interpreter
        .begin("plan", "unmatched", "unmatched", "consumer", &Value::Null)
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
    let (result, _) = failed
        .begin("plan", "failure", "any", "consumer", &Value::Null)
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
    assert!(result.reports[0].message.is_none());
    assert!(fixture.channels_empty().await);

    let demo = SrdlInterpreter::new(
        include_str!("../../../../../examples/webots/sentinel-rules.v1.srdl"),
        fixture.adapter.atlas.clone(),
    )
    .unwrap();
    for (path, effect) in [
        ("protected.txt", Effect::Deny),
        ("notes.txt", Effect::Allow),
    ] {
        let (result, _) = demo
            .begin(
                "demo",
                path,
                "robonix/system/executor/builtin/write_file",
                "consumer",
                &serde_json::json!({"arguments":{"path":path}}),
            )
            .await;
        assert_eq!(result.effect, effect);
    }

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
async fn acquires_typed_field_groups_once_and_cleans_channels() {
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
        let started = Instant::now();
        let acquired: Vec<_> = timeout(
            Duration::from_secs(5),
            fixture.acquire(&program.rules[0]).try_collect(),
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
        assert!(acquired[0].acquired_at >= started && acquired[0].acquired_at <= Instant::now());
        assert!(std::ptr::eq(acquired[0].input, &program.rules[0].inputs[0]));
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
        fixture.acquire(&program.rules[0]).try_collect(),
    )
    .await
    .unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.input, "ready");
    assert!(
        matches!(error.kind, InputErrorKind::Selection(select::SelectError {
        step: 1, kind: select::SelectErrorKind::MissingField(field),
    }) if field == "missing")
    );
    assert!(fixture.channels_empty().await);

    let failed = input_rule("sensor/error");
    let result: Result<Vec<_>, _> = timeout(
        Duration::from_secs(5),
        fixture.acquire(&failed).try_collect(),
    )
    .await
    .unwrap();
    let error = result.unwrap_err();
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
            r#"rule typed {{ trigger always;
            input from "sensor/value" select {field} maxage 1;
            require true; enforce deny; }}"#
        ))
        .unwrap();
        let result: Result<Vec<_>, _> = timeout(
            Duration::from_secs(5),
            fixture.acquire(&program.rules[0]).try_collect(),
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
    let source = (0..MAX_CONCURRENT_BINDINGS + 1)
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
    let mut check =
        Box::pin(interpreter.begin("plan", "cancel", "any", "sentinel-test", &Value::Null));
    timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = async {
                while fixture.calls.load(Ordering::SeqCst) < MAX_CONCURRENT_BINDINGS {
                    tokio::task::yield_now().await;
                }
            } => {},
            result = &mut check => panic!("slow checks completed unexpectedly: {:?}", result.0),
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
    // MCP shutdown and Atlas disconnect have separate timeout budgets.
    timeout(Duration::from_secs(15), async {
        while !fixture.channels_empty().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn pool_retains_multiple_rules_and_reacquires_postcondition_inputs() {
    use crate::interpreter::SrdlInterpreter;
    let mut fixture = AtlasFixture::new().await;
    fixture.serve_mcp().await;
    let source = r#"
        rule measured {
            trigger capability "robot/pick";
            input from "sensor/value" select number x = reading maxage 1;
            require x = 42; ensure x = 42; enforce deny;
        }
        rule contextual {
            trigger capability "robot/pick";
            context boolean ready = arguments.ready;
            require true; ensure ready; enforce deny;
        }
    "#;
    let interpreter = SrdlInterpreter::new(source, fixture.adapter.atlas.clone()).unwrap();
    let yes = serde_json::json!({"arguments":{"ready":true}});
    let no = serde_json::json!({"arguments":{"ready":false}});
    let (before, first) = interpreter
        .begin("p", "same", "robot/pick", "test", &yes)
        .await;
    assert_eq!(before.effect, Effect::Allow);
    assert_eq!(before.reports.len(), 2);
    let (duplicate, handle) = interpreter
        .begin("p", "same", "robot/pick", "test", &no)
        .await;
    assert_eq!(duplicate.effect, Effect::Deny);
    assert!(handle.is_none());
    let (before, second) = interpreter
        .begin("q", "same", "robot/pick", "test", &no)
        .await;
    assert_eq!(before.effect, Effect::Allow);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    let after = second.unwrap().ensure().await;
    assert_eq!(after.effect, Effect::Deny);
    assert!(after.reports.iter().all(|report| report.ensure));
    assert_eq!(
        after.reports[1]
            .evaluation
            .as_ref()
            .unwrap()
            .failed_requirements(),
        [1]
    );
    assert_eq!(first.unwrap().ensure().await.effect, Effect::Allow);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 4);
    let (_, reused) = interpreter
        .begin("p", "same", "robot/pick", "test", &yes)
        .await;
    assert!(reused.is_some());
}

#[tokio::test]
async fn pending_postconditions_wait_for_completion_and_cancel_with_their_owner() {
    use crate::interpreter::SrdlInterpreter;
    let mut fixture = AtlasFixture::new().await;
    let entered = fixture.serve_mcp().await;
    let source = r#"rule hold {
        trigger always;
        input from "sensor/slow" select boolean ready;
        require true; ensure ready; enforce deny;
    }"#;
    let interpreter = SrdlInterpreter::new(source, fixture.adapter.atlas.clone()).unwrap();
    let (before, checks) = interpreter
        .begin("p", "c", "robot/pick", "test", &Value::Null)
        .await;
    assert_eq!(before.effect, Effect::Allow);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let checking = tokio::spawn(checks.unwrap().ensure());
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    checking.abort();
    assert!(checking.await.unwrap_err().is_cancelled());
    timeout(Duration::from_secs(10), async {
        while !fixture.channels_empty().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (_, checks) = interpreter
        .begin("p", "c", "robot/pick", "test", &Value::Null)
        .await;
    assert!(checks.is_some());
    drop(checks);
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
    RuleEvaluator::evaluate(check_id, rule, false, &prepared)
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
                    .maxage
                    .and_then(|age| Duration::try_from_secs_f64(age).ok());
                deadline.send_modify(|deadline| {
                    deadline.record(
                        acquired.acquired_at,
                        age,
                        Arc::from(selected.field.name.0.as_str()),
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
        rule.inputs[0].fields[0].maxage = Some(maxage);
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
