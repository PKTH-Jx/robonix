// SPDX-License-Identifier: MulanPSL-2.0

//! Static SRDL policy loading, trusted call context and dispatch checks.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use robonix_atlas::client::{self as atlas_client, AtlasClient};
use robonix_scribe::warn;
use robonix_sentinel::interpreter::{SrdlInterpreter, enforcer::EnforcementResult};
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
    /// Shared interpreter; each check owns its context and rule evaluation state.
    interpreter: Arc<SrdlInterpreter>,
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
            interpreter: Arc::new(SrdlInterpreter::new(source, atlas)?),
        })
    }

    pub async fn check(
        &self,
        consumer_id: &str,
        contract_id: &str,
        args_json: &str,
        identity: &SentinelIdentity,
    ) -> Result<EnforcementResult, serde_json::Error> {
        let arguments: Map<String, Value> = serde_json::from_str(args_json)?;
        let context = serde_json::json!({
            "arguments": arguments,
            "identity": {
                "known": identity.known,
                "user_id": if identity.known && !identity.user_id.is_empty() { Some(&identity.user_id) } else { None },
                "roles": if identity.known { identity.roles.clone() } else { Vec::new() },
            },
        });
        Ok(self
            .interpreter
            .check(contract_id, consumer_id, &context)
            .await)
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
            serde_json::json!({"rule":report.rule, "requirements":requirements,
            "violations":violations, "enforcement":enforcement, "report":report.message,
            "evaluation_error":evaluation_error, "enforcement_error":enforcement_error})
        })
        .collect();
    serde_json::json!({"effect":result.effect, "reports":reports})
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
    use robonix_sentinel::Effect;

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
            r#"rule guard {
            trigger capability "move";
            context string user = identity.user_id;
            context boolean permitted = arguments.permitted;
            require user = "alice"; require permitted;
            enforce deny report "blocked";
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
            let decision = runtime
                .check("sentinel", "move", &args, &identity)
                .await
                .unwrap();
            assert_eq!(decision.effect, effect);
            let report = &feedback(&decision)["reports"][0];
            if !known {
                assert!(report["evaluation_error"].is_string());
                assert!(report["report"].is_null());
            } else if effect == Effect::Deny {
                assert_eq!(report["report"], "blocked");
                assert!(!report["violations"].as_array().unwrap().is_empty());
            }
        }
        let anonymous = SentinelIdentity::default();
        let unmatched = runtime
            .check("sentinel", "other", "{}", &anonymous)
            .await
            .unwrap();
        assert_eq!(unmatched.effect, Effect::Allow);
        assert!(unmatched.reports.is_empty());
        for invalid in ["{", "[]", "null"] {
            assert!(
                runtime
                    .check("sentinel", "move", invalid, &anonymous)
                    .await
                    .is_err()
            );
        }
        std::fs::remove_file(path).unwrap();
        server.abort();
    }
}
