// SPDX-License-Identifier: MulanPSL-2.0

//! SRDL policy loading and invocation-bound checks with trusted call context.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::pb::executor::RtdlEvent;
use crate::pb::pilot::{CapabilityCall, CapabilityCallResult, rtdl_node_state::RtdlNodeStateEnum};
use crate::{active_plans::PlanRuntime, dispatch, rtdl_events::NodeEventContext};
use anyhow::{Context, Result};
use robonix_atlas::client::{self as atlas_client, AtlasClient};
use robonix_scribe::warn;
use robonix_sentinel::interpreter::{
    CheckContext, SrdlInterpreter,
    enforcer::{Effect, EnforcementResult, Error as HookError, HookRuntime},
};
use serde_json::{Map, Value};
use tokio::sync::{mpsc::Sender, watch};
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};

use crate::pb::contracts::robonix_system_keystone_get_profile_client::RobonixSystemKeystoneGetProfileClient;
use crate::pb::contracts::robonix_system_sentinel_list_rules_server::RobonixSystemSentinelListRules;
use crate::pb::contracts::robonix_system_sentinel_replace_rules_server::RobonixSystemSentinelReplaceRules;
use crate::pb::keystone::{GetProfileRequest, User};
use crate::pb::sentinel::{
    ListRulesRequest, ListRulesResponse, ReplaceRulesRequest, ReplaceRulesResponse,
};

const KEYSTONE_GET_PROFILE: &str = "robonix/system/keystone/get_profile";
const ATLAS_CONTROL_TIMEOUT: Duration = Duration::from_millis(250);
const ADMIN_RPC_TIMEOUT: Duration = Duration::from_secs(2);
pub const SENTINEL_PROVIDER_ID: &str = "sentinel";
pub const SENTINEL_NAMESPACE: &str = "robonix/system/sentinel";

/// Identity established by Executor's configured session validation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SentinelIdentity {
    /// Whether a session identity has been verified, rather than supplied by a caller.
    pub known: bool,
    /// Verified Keystone user ID; unavailable identities expose no user ID to SRDL.
    pub user_id: String,
    /// Roles returned by Keystone for the verified identity.
    pub roles: Vec<String>,
}

/// Immutable SRDL policy and shared Atlas acquisition resources for all active plans.
#[derive(Clone)]
pub struct SentinelRuntime {
    /// Shared interpreter and queues for the complete capability lifecycle.
    interpreter: Arc<SrdlInterpreter>,
    /// Discovery connection cloned for capability hooks without holding shared locks.
    atlas: AtlasClient,
}

/// Reserved endpoints for SRDL rule reading and dynamic loading.
#[derive(Clone)]
pub struct SentinelServiceImpl;

impl SentinelRuntime {
    /// Load a policy snapshot; later file changes do not affect active plans.
    pub fn load(path: &Path, atlas: AtlasClient) -> Result<Self> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("read Sentinel SRDL '{}'", path.display()))?;
        Self::new(&source, atlas)
            .with_context(|| format!("load Sentinel SRDL '{}'", path.display()))
    }

    /// Validate SRDL and start shared check queues; requires an active Tokio runtime.
    pub fn new(source: &str, atlas: AtlasClient) -> Result<Self> {
        Ok(Self {
            interpreter: Arc::new(SrdlInterpreter::load(source, atlas.clone())?),
            atlas,
        })
    }

    /// Bind checks and corrective calls to this node's verified identity and plan controls.
    pub(crate) fn hooks<'a>(
        &'a self,
        node: &'a NodeEventContext,
        identity: &'a SentinelIdentity,
        runtime: &'a PlanRuntime,
        provider_id: &'a str,
    ) -> ExecutorHooks<'a> {
        ExecutorHooks {
            interpreter: &self.interpreter,
            atlas: &self.atlas,
            runtime,
            provider_id,
            identity,
            node,
            depth: 0,
            completed: None,
            stopped: None,
        }
    }
}

/// Invocation-scoped Executor access for hooks, including nested policy checks.
pub(crate) struct ExecutorHooks<'a> {
    /// Shared rule set used directly for lifecycle checks.
    interpreter: &'a SrdlInterpreter,
    /// Discovery connection for corrective capability calls.
    atlas: &'a AtlasClient,
    /// Tracks cancellation and active asynchronous capability executions.
    runtime: &'a PlanRuntime,
    /// Executor identity used for discovery and in-process dispatch.
    provider_id: &'a str,
    /// Verified identity inherited by calls made by enforcement hooks.
    identity: &'a SentinelIdentity,
    /// Plan and operation receiving progress from hook execution.
    node: &'a NodeEventContext,
    /// Bounds recursive hook execution when rules trigger other capability calls.
    depth: usize,
    /// Ends monitoring after capability completion without cancelling the plan.
    completed: Option<&'a watch::Receiver<bool>>,
    /// Shares stop feedback across a capability and its corrective calls.
    stopped: Option<&'a AtomicBool>,
}

impl ExecutorHooks<'_> {
    /// Reserve exclusions and apply before rules using trusted call context.
    /// Pass an allowed result's admission ID to `on_complete` for every execution outcome.
    pub(crate) async fn on_arrival(
        &self,
        call: &CapabilityCall,
    ) -> Result<EnforcementResult, serde_json::Error> {
        let values = self.values(&call.args_json)?;
        Ok(self
            .interpreter
            .on_arrival(self.context(call, &values), self)
            .await)
    }

    /// Execute the capability and drain its while checks before returning.
    pub(crate) async fn on_execution(
        &self,
        call: &CapabilityCall,
        tx: Option<&Sender<Result<RtdlEvent, Status>>>,
    ) -> (CapabilityCallResult, u32, String) {
        let (completed, completion) = watch::channel(false);
        let requested_stop = AtomicBool::new(false);
        // A corrective call's stop must remain visible in its caller's feedback.
        let stopped = self.stopped.unwrap_or(&requested_stop);
        let hooks = ExecutorHooks {
            completed: Some(&completion),
            stopped: Some(stopped),
            ..*self
        };
        let mut atlas = self.atlas.clone();
        let monitor = hooks.monitor(call, tx);
        let execute = dispatch::run_until_terminal(
            call,
            self.provider_id,
            &mut atlas,
            self.runtime,
            self.node,
        );
        tokio::pin!(monitor, execute);
        let ((mut result, mut state), detail) = tokio::select! {
            biased;
            result = &mut execute => {
                // Let active checks and hooks finish their cleanup before releasing exclusions.
                if !self.runtime.is_cancelled(&self.node.plan_id).await
                    && !stopped.load(Ordering::Acquire) {
                    completed.send_replace(true);
                }
                (result, monitor.await)
            }
            detail = &mut monitor => (execute.await, detail),
        };
        // Plan cancellation also owns a result that races with successful execution.
        if self.runtime.is_cancelled(&self.node.plan_id).await {
            state = RtdlNodeStateEnum::Canceled as u32;
            result.success = false;
            if stopped.load(Ordering::Acquire) && !detail.is_empty() {
                result.error = format!("Sentinel stopped plan: {detail}");
            }
        }
        (result, state, detail)
    }

    /// Release admission for every outcome, then apply after rules only on success.
    pub(crate) async fn on_complete(
        &self,
        call: &CapabilityCall,
        admission_id: Option<u64>,
        succeeded: bool,
    ) -> Result<EnforcementResult, serde_json::Error> {
        let values = if succeeded {
            self.values(&call.args_json)
        } else {
            Ok(Value::Null)
        };
        // Release admission even if context construction prevents postcondition evaluation.
        let result = self
            .interpreter
            .on_complete(
                self.context(call, values.as_ref().unwrap_or(&Value::Null)),
                self,
                admission_id,
                succeeded && values.is_ok(),
            )
            .await;
        values.map(|_| result)
    }

    /// Run invocation-local checks and retain their latest enforcement feedback.
    async fn monitor(
        &self,
        call: &CapabilityCall,
        tx: Option<&Sender<Result<RtdlEvent, Status>>>,
    ) -> String {
        let mut detail = String::new();
        let values = match self.values(&call.args_json) {
            Ok(values) => values,
            Err(error) => {
                self.stop().await;
                return serde_json::json!({"effect":"stop", "context_error":error.to_string()})
                    .to_string();
            }
        };
        let checks = self
            .interpreter
            .on_execution(self.context(call, &values), self);
        tokio::pin!(checks);
        while let Some(result) = checks.next().await {
            // Execution ending can cancel an in-flight check; discard that check's error.
            if self.completed.is_some_and(|completed| *completed.borrow())
                || (self.runtime.is_cancelled(&self.node.plan_id).await
                    && !self
                        .stopped
                        .is_some_and(|stopped| stopped.load(Ordering::Acquire)))
            {
                return detail;
            }
            if result.effect == Effect::Stop
                || result
                    .reports
                    .iter()
                    .any(|report| report.enforcement.is_some())
            {
                detail = feedback(&result).to_string();
                if let Some(tx) = tx {
                    // A slow stream consumer must not delay the next safety check.
                    match tx.try_send(Ok(crate::rtdl_events::node_state(
                        self.node,
                        RtdlNodeStateEnum::Running as u32,
                        detail.clone(),
                        None,
                    ))) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            warn!(
                                "[executor] while progress channel full; retaining latest feedback for the terminal event"
                            );
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return detail,
                    }
                }
            }
        }
        detail
    }

    fn values(&self, args_json: &str) -> Result<Value, serde_json::Error> {
        let arguments: Map<String, Value> = serde_json::from_str(args_json)?;
        Ok(serde_json::json!({
            "arguments": arguments,
            "identity": {
                "known": self.identity.known,
                "user_id": if self.identity.known && !self.identity.user_id.is_empty() { Some(&self.identity.user_id) } else { None },
                "roles": if self.identity.known { self.identity.roles.clone() } else { Vec::new() },
            },
        }))
    }

    fn context<'a>(&'a self, call: &'a CapabilityCall, values: &'a Value) -> CheckContext<'a> {
        CheckContext {
            plan_id: &self.node.plan_id,
            call_id: &call.call_id,
            capability: &call.contract_id,
            consumer_id: SENTINEL_PROVIDER_ID,
            values,
        }
    }
}

#[tonic::async_trait]
impl HookRuntime for ExecutorHooks<'_> {
    async fn stop(&self) {
        if self.runtime.is_cancelled(&self.node.plan_id).await
            || self.completed.is_some_and(|completed| *completed.borrow())
        {
            return;
        }
        if let Some(stopped) = self.stopped {
            stopped.store(true, Ordering::Release);
        }
        let mut atlas = self.atlas.clone();
        // Waiting for this plan to finish here would wait on the check making this request.
        self.runtime
            .cancel_plan_control(&self.node.plan_id, 0, self.provider_id, &mut atlas)
            .await;
    }

    async fn cancelled(&self) {
        let completed = async {
            match self.completed {
                Some(completed) => {
                    let mut completed = completed.clone();
                    let _ = completed.wait_for(|completed| *completed).await;
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = self.runtime.wait_cancelled(&self.node.plan_id) => {},
            () = completed => {},
        }
    }

    async fn call(&self, capability: &str) -> Result<(), HookError> {
        if self.depth >= 8 {
            return Err(HookError::Runtime(
                "capability hook nesting exceeds 8 calls".into(),
            ));
        }
        let mut atlas = self.atlas.clone();
        let providers = tokio::select! {
            biased;
            _ = self.cancelled() => {
                return Err(HookError::Runtime("capability hook cancelled".into()));
            }
            result = tokio::time::timeout(
                ATLAS_CONTROL_TIMEOUT,
                atlas.query_capabilities("", capability, robonix_atlas::pb::Transport::Mcp),
            ) => result
                .map_err(|_| HookError::Runtime("resolve hook capability timed out".into()))?
                .map_err(|error| HookError::Runtime(format!("resolve hook capability: {error:#}")))?,
        };
        let providers: Vec<_> = providers
            .into_iter()
            .filter(|provider| !provider.capabilities.is_empty())
            .collect();
        if providers.len() != 1 {
            return Err(HookError::Runtime(format!(
                "hook capability '{capability}' requires exactly one provider, found {}",
                providers.len()
            )));
        }
        let call = CapabilityCall {
            call_id: format!("sentinel-{}", uuid::Uuid::new_v4()),
            provider_id: providers[0].id.clone(),
            contract_id: capability.to_owned(),
            args_json: "{}".into(),
        };
        let hooks = ExecutorHooks {
            depth: self.depth + 1,
            ..*self
        };
        let before = hooks
            .on_arrival(&call)
            .await
            .map_err(|error| HookError::Runtime(error.to_string()))?;
        if before.effect == Effect::Deny {
            return Err(HookError::Runtime(format!(
                "hook capability '{capability}' denied: {}",
                check_error(&before).unwrap_or_else(|| {
                    if before.messages.is_empty() {
                        "requirements were not satisfied".into()
                    } else {
                        before.messages.join("\n")
                    }
                })
            )));
        }
        let (result, _, _) = hooks.on_execution(&call, None).await;
        // Nested corrective calls follow the same explicit completion path as plan calls.
        let after = hooks
            .on_complete(&call, before.admission_id, result.success)
            .await
            .map_err(|error| HookError::Runtime(error.to_string()))?;
        if !result.success {
            return Err(HookError::Runtime(format!(
                "hook capability '{capability}' failed: {}",
                result.error
            )));
        }
        if let Some(error) = check_error(&after) {
            return Err(HookError::Runtime(format!(
                "hook capability after check failed: {error}"
            )));
        }
        Ok(())
    }
}

/// Serialize requirement results, reports and evaluation or enforcement failures.
pub fn feedback(result: &EnforcementResult) -> Value {
    let reports: Vec<_> = result
        .reports
        .iter()
        .map(|report| {
            let (requirements, violations, evaluation_error) = match &report.evaluation {
                Ok(value) => (Some(&value.requirements), value.failed_requirements(), None),
                Err(error) => (None, Vec::new(), Some(error.to_string())),
            };
            let (enforcement, enforcement_error) = match &report.enforcement {
                Some(Ok(effect)) => (Some(effect), None),
                Some(Err(error)) => (None, Some(error.to_string())),
                None => (None, None),
            };
            serde_json::json!({"rule":report.rule, "timing":report.timing, "requirements":requirements,
            "violations":violations, "enforcement":enforcement,
            "evaluation_error":evaluation_error, "enforcement_error":enforcement_error})
        })
        .collect();
    let calls: Vec<_> = result.calls.iter().map(|call| serde_json::json!({
        "capability":call.capability,
        "rule":result.reports[call.rule].rule,
        "success":call.result.as_ref().map(Result::is_ok),
        "error":call.result.as_ref().and_then(|result| result.as_ref().err()).map(ToString::to_string),
    })).collect();
    serde_json::json!({"effect":result.effect, "messages":result.messages, "calls":calls, "reports":reports})
}

fn check_error(result: &EnforcementResult) -> Option<String> {
    result.reports.iter().find_map(|report| {
        report
            .evaluation
            .as_ref()
            .err()
            .map(ToString::to_string)
            .or_else(|| {
                report
                    .enforcement
                    .as_ref()
                    .and_then(|result| result.as_ref().err())
                    .map(ToString::to_string)
            })
    })
}

pub(crate) async fn resolve_session_identity(
    atlas: &AtlasClient,
    consumer_id: &str,
    token: &str,
) -> Result<SentinelIdentity, Status> {
    let user = resolve_session_user(atlas, consumer_id, token).await?;
    Ok(SentinelIdentity {
        known: true,
        user_id: user.user_id,
        roles: user.roles,
    })
}

async fn resolve_session_user(
    atlas: &AtlasClient,
    consumer_id: &str,
    token: &str,
) -> Result<User, Status> {
    if token.is_empty() {
        return Err(Status::unauthenticated("login session is required"));
    }
    let mut atlas = atlas.clone();
    let (channel_id, _, channel) = tokio::time::timeout(
        ATLAS_CONTROL_TIMEOUT,
        atlas_client::connect_to_capability(&mut atlas, consumer_id, KEYSTONE_GET_PROFILE),
    )
    .await
    .map_err(|_| Status::deadline_exceeded("resolve Keystone timed out"))?
    .map_err(|error| Status::unavailable(format!("resolve Keystone: {error:#}")))?;
    let response = match tokio::time::timeout(
        ADMIN_RPC_TIMEOUT,
        RobonixSystemKeystoneGetProfileClient::new(channel).get_profile(GetProfileRequest {
            session_token: token.to_owned(),
        }),
    )
    .await
    {
        Ok(result) => result.map(|response| response.into_inner()),
        Err(_) => Err(Status::deadline_exceeded(
            "Keystone profile lookup timed out",
        )),
    };
    disconnect_bounded(&mut atlas, &channel_id).await;
    response?
        .user
        .ok_or_else(|| Status::internal("Keystone returned an empty profile"))
}

async fn disconnect_bounded(atlas: &mut AtlasClient, channel_id: &str) {
    match tokio::time::timeout(
        ATLAS_CONTROL_TIMEOUT,
        atlas.disconnect_capability(channel_id),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!("disconnect Atlas channel '{channel_id}' failed: {error:#}"),
        Err(_) => warn!("disconnect Atlas channel '{channel_id}' timed out"),
    }
}

#[tonic::async_trait]
impl RobonixSystemSentinelListRules for SentinelServiceImpl {
    async fn list_rules(
        &self,
        _: Request<ListRulesRequest>,
    ) -> Result<Response<ListRulesResponse>, Status> {
        Err(Status::unimplemented(
            "SRDL rule reading is not implemented yet",
        ))
    }
}

#[tonic::async_trait]
impl RobonixSystemSentinelReplaceRules for SentinelServiceImpl {
    async fn replace_rules(
        &self,
        _: Request<ReplaceRulesRequest>,
    ) -> Result<Response<ReplaceRulesResponse>, Status> {
        Err(Status::unimplemented(
            "SRDL dynamic rule loading is not implemented yet",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use robonix_atlas::service::{AtlasRegistry, serve_atlas};
    use robonix_sentinel::interpreter::enforcer::Effect;

    fn reserve_address() -> std::net::SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    async fn start_atlas() -> (AtlasClient, tokio::task::JoinHandle<Result<()>>) {
        let address = reserve_address();
        let server = tokio::spawn(serve_atlas(Arc::new(AtlasRegistry::default()), address));
        let atlas = AtlasClient::connect_with_retry(
            format!("http://{address}"),
            50,
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        (atlas, server)
    }

    fn node(plan_id: &str) -> NodeEventContext {
        NodeEventContext {
            plan_id: plan_id.into(),
            node_index: 0,
            node_kind: 2,
            op_id: "test".into(),
            description: "check hook execution".into(),
        }
    }

    fn call(capability: &str) -> CapabilityCall {
        CapabilityCall {
            call_id: capability.into(),
            provider_id: "executor".into(),
            contract_id: capability.into(),
            args_json: "{}".into(),
        }
    }

    #[tokio::test]
    async fn monitoring_cancels_only_on_stop_despite_full_progress_channel() {
        let (atlas, server) = start_atlas().await;
        for (source, command, stopped) in [
            (
                r#"
            trigger [report, halt] while "robonix/system/executor/builtin/run_command";
            rule report { require false; enforce report "unsafe"; }
            rule halt { require false; enforce stop; }
        "#,
                "sleep 30",
                true,
            ),
            ("", "printf complete", false),
        ] {
            let sentinel = SentinelRuntime::new(source, atlas.clone()).unwrap();
            let runtime = PlanRuntime::default();
            runtime.register_plan("plan").await.unwrap();
            let node = node("plan");
            let call = CapabilityCall {
                args_json: serde_json::json!({"command":command}).to_string(),
                ..call("robonix/system/executor/builtin/run_command")
            };
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            tx.try_send(Ok(RtdlEvent::default())).unwrap();
            let (result, state, detail) = tokio::time::timeout(
                Duration::from_secs(3),
                sentinel
                    .hooks(&node, &SentinelIdentity::default(), &runtime, "executor")
                    .on_execution(&call, Some(&tx)),
            )
            .await
            .unwrap();
            assert_eq!(result.success, !stopped);
            assert_eq!(
                state,
                if stopped {
                    RtdlNodeStateEnum::Canceled
                } else {
                    RtdlNodeStateEnum::Succeeded
                } as u32
            );
            assert_eq!(runtime.is_cancelled("plan").await, stopped);
            if stopped {
                assert_eq!(
                    serde_json::from_str::<Value>(&detail).unwrap()["effect"],
                    "stop"
                );
            } else {
                assert!(detail.is_empty());
            }
            assert!(rx.try_recv().is_ok());
        }
        server.abort();
    }

    async fn check(
        runtime: &SentinelRuntime,
        call: &CapabilityCall,
        identity: &SentinelIdentity,
    ) -> Result<EnforcementResult, serde_json::Error> {
        runtime
            .hooks(&node("plan"), identity, &PlanRuntime::default(), "executor")
            .on_arrival(call)
            .await
    }

    /// Accepts discovery requests without replying so cancellation can be tested in flight.
    #[derive(Clone)]
    struct StalledAtlas {
        /// Signals when a request has reached the server.
        requested: Arc<tokio::sync::Notify>,
    }

    impl tonic::server::NamedService for StalledAtlas {
        const NAME: &'static str = "robonix.atlas.Atlas";
    }

    impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>> for StalledAtlas {
        type Response = tonic::codegen::http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;

        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: tonic::codegen::http::Request<tonic::body::Body>) -> Self::Future {
            self.requested.notify_one();
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn stalled_discovery_times_out_and_plan_cancellation_stops_both_discovery_steps() {
        let address = reserve_address();
        let requested = Arc::new(tokio::sync::Notify::new());
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(StalledAtlas {
                    requested: requested.clone(),
                })
                .serve(address),
        );
        let mut atlas = AtlasClient::connect_with_retry(
            format!("http://{address}"),
            50,
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        let sentinel = SentinelRuntime::new("", atlas.clone()).unwrap();
        let identity = SentinelIdentity::default();
        let runtime = PlanRuntime::default();
        let node = node("plan");
        let hooks = sentinel.hooks(&node, &identity, &runtime, "executor");
        let error = hooks.call("missing").await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
        requested.notified().await;

        runtime.register_plan(&node.plan_id).await.unwrap();
        let cancel = async {
            requested.notified().await;
            runtime
                .cancel_plan_control(&node.plan_id, 0, "executor", &mut atlas)
                .await;
        };
        let check = hooks.call("missing");
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(check, cancel)
        })
        .await
        .unwrap();
        assert!(outcome.unwrap_err().to_string().contains("cancelled"));

        runtime.register_plan("dispatch").await.unwrap();
        let node = NodeEventContext {
            plan_id: "dispatch".into(),
            ..node
        };
        let call = CapabilityCall {
            provider_id: "remote".into(),
            ..call("target")
        };
        let mut dispatch_atlas = atlas.clone();
        let dispatch =
            dispatch::run_until_terminal(&call, "executor", &mut dispatch_atlas, &runtime, &node);
        let cancel = async {
            requested.notified().await;
            runtime
                .cancel_plan_control(&node.plan_id, 0, "executor", &mut atlas)
                .await;
        };
        let ((outcome, _), ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(dispatch, cancel)
        })
        .await
        .unwrap();
        assert!(!outcome.success);
        assert!(outcome.error.contains("cancelled"));
        server.abort();
    }

    #[tokio::test]
    async fn loaded_rules_use_trusted_context_and_preserve_failure_feedback() {
        let (atlas, server) = start_atlas().await;
        let path = std::env::temp_dir().join(format!("sentinel-{}.srdl", uuid::Uuid::new_v4()));
        assert!(SentinelRuntime::load(&path, atlas.clone()).is_err());
        std::fs::write(
            &path,
            r#"trigger guard before "move";
            rule guard {
            context string user = identity.user_id;
            context boolean permitted = arguments.permitted;
            require user = "alice"; require permitted;
            enforce [deny, report "blocked"];
        }"#,
        )
        .unwrap();
        let runtime = SentinelRuntime::load(&path, atlas).unwrap();
        std::fs::write(&path, "changed after loading").unwrap();
        for (known, user, permitted, effect) in [
            (true, "alice", true, Effect::Allow),
            (true, "alice", false, Effect::Deny),
            (true, "bob", true, Effect::Deny),
            (false, "alice", true, Effect::Deny),
        ] {
            let identity = SentinelIdentity {
                known,
                user_id: user.into(),
                roles: Vec::new(),
            };
            let call = CapabilityCall {
                args_json:
                    serde_json::json!({"permitted":permitted,"identity":{"user_id":"alice"}})
                        .to_string(),
                ..call("move")
            };
            let decision = check(&runtime, &call, &identity).await.unwrap();
            assert_eq!(decision.effect, effect);
            let detail = feedback(&decision);
            let report = &detail["reports"][0];
            if !known {
                assert!(report["evaluation_error"].is_string());
                assert_eq!(detail["messages"], serde_json::json!([]));
            } else if effect == Effect::Deny {
                assert_eq!(detail["messages"], serde_json::json!(["blocked"]));
                assert!(!report["violations"].as_array().unwrap().is_empty());
            }
        }
        let anonymous = SentinelIdentity::default();
        let mut call = call("other");
        let unmatched = check(&runtime, &call, &anonymous).await.unwrap();
        assert_eq!(unmatched.effect, Effect::Allow);
        assert!(unmatched.reports.is_empty());
        for invalid in ["{", "[]", "null"] {
            call.args_json = invalid.into();
            assert!(check(&runtime, &call, &anonymous).await.is_err());
        }
        std::fs::remove_file(path).unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn recorded_calls_execute_with_nested_policy_checks_and_bounded_recursion() {
        use robonix_atlas::{client::mcp_params, pb::Transport};
        let (mut atlas, server) = start_atlas().await;
        let capability = "robonix/system/executor/builtin/get_all_plans";
        atlas
            .register_service("executor", "robonix/system/executor", "")
            .await
            .unwrap();
        atlas
            .declare_capability(
                "executor",
                capability,
                Transport::Mcp,
                "internal://executor/builtin",
                mcp_params("{}"),
            )
            .await
            .unwrap();

        for (called, extra, expected_error) in [
            (capability, String::new(), None),
            ("missing", String::new(), Some("found 0")),
            (
                capability,
                format!(
                    "trigger stop before \"{capability}\"; rule stop {{ require false; enforce [deny, report \"nested denial\"]; }}"
                ),
                Some("nested denial"),
            ),
            (
                capability,
                format!(
                    "trigger repeat before \"{capability}\"; rule repeat {{ require false; enforce call \"{capability}\"; }}"
                ),
                Some("nesting exceeds"),
            ),
            (
                capability,
                format!(
                    "trigger post after \"{capability}\"; rule post {{ require false; enforce call \"missing\"; }}"
                ),
                Some("after check failed"),
            ),
        ] {
            let source = format!(
                "trigger guard before target; rule guard {{ require false; enforce [call \"{called}\", report \"requested\"]; }} {extra}"
            );
            let runtime = SentinelRuntime::new(&source, atlas.clone()).unwrap();
            let result = check(&runtime, &call("target"), &SentinelIdentity::default())
                .await
                .unwrap();
            assert_eq!(result.messages, ["requested"]);
            assert_eq!(result.calls.len(), 1);
            let outcome = result.calls[0].result.as_ref().unwrap();
            match expected_error {
                Some(message) => {
                    assert_eq!(result.effect, Effect::Deny);
                    assert!(outcome.as_ref().unwrap_err().to_string().contains(message));
                }
                None => {
                    assert_eq!(result.effect, Effect::Allow);
                    assert_eq!(outcome, &Ok(()));
                }
            }
        }
        server.abort();
    }
}
