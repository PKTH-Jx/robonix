// SPDX-License-Identifier: MulanPSL-2.0

//! SRDL policy loading and invocation-bound checks with trusted call context.

use std::{path::Path, sync::Arc, time::Duration};

use crate::pb::pilot::CapabilityCall;
use crate::{active_plans::PlanRuntime, dispatch, rtdl_events::NodeEventContext};
use anyhow::{Context, Result};
use robonix_atlas::client::{self as atlas_client, AtlasClient};
use robonix_scribe::warn;
use robonix_sentinel::{
    interpreter::{
        CheckContext, SrdlInterpreter,
        enforcer::{Effect, EnforcementResult, Error as HookError, HookRuntime},
    },
    srdl::ast::TriggerTiming,
};
use serde_json::{Map, Value};
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
    /// Shared interpreter with one pool for independent before and after checks.
    interpreter: Arc<SrdlInterpreter>,
    /// Discovery connection cloned for capability hooks without holding shared locks.
    atlas: AtlasClient,
}

/// Reserved endpoints for SRDL rule reading and dynamic loading.
#[derive(Clone)]
pub struct SentinelServiceImpl;

impl SentinelRuntime {
    pub fn load(path: &Path, atlas: AtlasClient) -> Result<Self> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("read Sentinel SRDL '{}'", path.display()))?;
        Self::new(&source, atlas)
            .with_context(|| format!("load Sentinel SRDL '{}'", path.display()))
    }

    pub fn new(source: &str, atlas: AtlasClient) -> Result<Self> {
        Ok(Self {
            interpreter: Arc::new(SrdlInterpreter::new(source, atlas.clone())?),
            atlas,
        })
    }

    pub async fn check(
        &self,
        node: &NodeEventContext,
        call: &CapabilityCall,
        identity: &SentinelIdentity,
        timing: TriggerTiming,
        runtime: &PlanRuntime,
        provider_id: &str,
    ) -> Result<EnforcementResult, serde_json::Error> {
        ExecutorHooks {
            sentinel: self,
            runtime,
            provider_id,
            identity,
            node,
            depth: 0,
        }
        .check(call, timing)
        .await
    }

    /// Release admission for a completed call independently of after-rule checking.
    pub fn finish(&self, plan_id: &str, call: &CapabilityCall, admission_id: Option<u64>) {
        if let Some(id) = admission_id {
            self.interpreter.finish(plan_id, &call.call_id, id);
        }
    }

    fn context(args_json: &str, identity: &SentinelIdentity) -> Result<Value, serde_json::Error> {
        let arguments: Map<String, Value> = serde_json::from_str(args_json)?;
        Ok(serde_json::json!({
            "arguments": arguments,
            "identity": {
                "known": identity.known,
                "user_id": if identity.known && !identity.user_id.is_empty() { Some(&identity.user_id) } else { None },
                "roles": if identity.known { identity.roles.clone() } else { Vec::new() },
            },
        }))
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

/// Invocation-scoped Executor access for hooks, including nested policy checks.
struct ExecutorHooks<'a> {
    /// Shared rule set and discovery connection.
    sentinel: &'a SentinelRuntime,
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
}

impl ExecutorHooks<'_> {
    async fn check(
        &self,
        call: &CapabilityCall,
        timing: TriggerTiming,
    ) -> Result<EnforcementResult, serde_json::Error> {
        let values = SentinelRuntime::context(&call.args_json, self.identity)?;
        Ok(self
            .sentinel
            .interpreter
            .check_with_runtime(
                &CheckContext {
                    plan_id: &self.node.plan_id,
                    call_id: &call.call_id,
                    capability: &call.contract_id,
                    consumer_id: SENTINEL_PROVIDER_ID,
                    values: &values,
                    timing,
                },
                self,
            )
            .await)
    }
}

#[tonic::async_trait]
impl HookRuntime for ExecutorHooks<'_> {
    async fn cancelled(&self) {
        self.runtime.wait_cancelled(&self.node.plan_id).await;
    }

    async fn call(&self, capability: &str) -> Result<(), HookError> {
        if self.depth >= 8 {
            return Err(HookError::Runtime(
                "capability hook nesting exceeds 8 calls".into(),
            ));
        }
        let mut atlas = self.sentinel.atlas.clone();
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
            .check(&call, TriggerTiming::Before)
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
        let (result, _) = dispatch::run_until_terminal(
            &call,
            self.provider_id,
            &mut atlas,
            self.runtime,
            self.node,
        )
        .await;
        self.sentinel
            .finish(&self.node.plan_id, &call, before.admission_id);
        if !result.success {
            return Err(HookError::Runtime(format!(
                "hook capability '{capability}' failed: {}",
                result.error
            )));
        }
        let after = hooks
            .check(&call, TriggerTiming::After)
            .await
            .map_err(|error| HookError::Runtime(error.to_string()))?;
        if let Some(error) = check_error(&after) {
            return Err(HookError::Runtime(format!(
                "hook capability after check failed: {error}"
            )));
        }
        Ok(())
    }
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

    async fn check(
        runtime: &SentinelRuntime,
        plan_id: &str,
        call: &CapabilityCall,
        identity: &SentinelIdentity,
        timing: TriggerTiming,
    ) -> Result<EnforcementResult, serde_json::Error> {
        runtime
            .check(
                &NodeEventContext {
                    plan_id: plan_id.into(),
                    node_index: 0,
                    node_kind: 2,
                    op_id: "test".into(),
                    description: "check hook execution".into(),
                },
                call,
                identity,
                timing,
                &PlanRuntime::default(),
                "executor",
            )
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
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
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
        let node = NodeEventContext {
            plan_id: "plan".into(),
            node_index: 0,
            node_kind: 2,
            op_id: "test".into(),
            description: "cancel discovery".into(),
        };
        let hooks = ExecutorHooks {
            sentinel: &sentinel,
            runtime: &runtime,
            provider_id: "executor",
            identity: &identity,
            node: &node,
            depth: 0,
        };
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
            call_id: "call".into(),
            provider_id: "remote".into(),
            contract_id: "target".into(),
            args_json: "{}".into(),
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
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let server = tokio::spawn(serve_atlas(Arc::new(AtlasRegistry::default()), address));
        let atlas = AtlasClient::connect_with_retry(
            format!("http://{address}"),
            50,
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        let path = std::env::temp_dir().join(format!("sentinel-{}.srdl", uuid::Uuid::new_v4()));
        assert!(SentinelRuntime::load(&path, atlas.clone()).is_err());
        std::fs::write(
            &path,
            r#"trigger guard before "move";
            rule guard {
            context string user = identity.user_id;
            context boolean permitted = arguments.permitted;
            require user = "alice"; require permitted;
            enforce deny, report "blocked";
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
            let args = serde_json::json!({"permitted":permitted,"identity":{"user_id":"alice"}})
                .to_string();
            let call = CapabilityCall {
                call_id: "move".into(),
                provider_id: "robot".into(),
                contract_id: "move".into(),
                args_json: args,
            };
            let decision = check(&runtime, "plan", &call, &identity, TriggerTiming::Before)
                .await
                .unwrap();
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
        let mut call = CapabilityCall {
            call_id: "other".into(),
            provider_id: "robot".into(),
            contract_id: "other".into(),
            args_json: "{}".into(),
        };
        let unmatched = check(&runtime, "plan", &call, &anonymous, TriggerTiming::Before)
            .await
            .unwrap();
        assert_eq!(unmatched.effect, Effect::Allow);
        assert!(unmatched.reports.is_empty());
        for invalid in ["{", "[]", "null"] {
            call.args_json = invalid.into();
            assert!(
                check(&runtime, "plan", &call, &anonymous, TriggerTiming::Before)
                    .await
                    .is_err()
            );
        }
        std::fs::remove_file(path).unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn recorded_calls_execute_with_nested_policy_checks_and_bounded_recursion() {
        use robonix_atlas::{client::mcp_params, pb::Transport};
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let server = tokio::spawn(serve_atlas(Arc::new(AtlasRegistry::default()), address));
        let mut atlas = AtlasClient::connect_with_retry(
            format!("http://{address}"),
            50,
            Duration::from_millis(10),
        )
        .await
        .unwrap();
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
                    "trigger stop before \"{capability}\"; rule stop {{ require false; enforce deny, report \"nested denial\"; }}"
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
                "trigger guard before target; rule guard {{ require false; enforce call \"{called}\", report \"requested\"; }} {extra}"
            );
            let runtime = SentinelRuntime::new(&source, atlas.clone()).unwrap();
            let target = CapabilityCall {
                call_id: "target".into(),
                provider_id: "executor".into(),
                contract_id: "target".into(),
                args_json: "{}".into(),
            };
            let result = check(
                &runtime,
                "plan",
                &target,
                &SentinelIdentity::default(),
                TriggerTiming::Before,
            )
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
