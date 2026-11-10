//! Typed gRPC acquisition for the supported system snapshot contracts.

use anyhow::{Context, Result, bail};
use tokio::time::Instant;
use tonic::transport::Endpoint;

use super::{CONTROL_TIMEOUT, Response};

#[allow(dead_code, clippy::all)]
pub(super) mod pb {
    include!(concat!(env!("OUT_DIR"), "/contract_proto_modules.rs"));
}

const SOMA: &str = "robonix/system/soma/get_health";
const VITALS: &str = "robonix/system/vitals/get";
const SCENE: &str = "robonix/system/scene/get_robot_context";

/// Identify contracts with compiled request, response and client types in this module.
pub(super) fn supports(capability: &str) -> bool {
    matches!(capability, SOMA | VITALS | SCENE)
}

/// Call one supported snapshot RPC with an empty request, preserving its response structure.
/// Source timestamps, TTL and stale flags remain data; this layer does not interpret them.
pub(super) async fn acquire(
    endpoint: &str,
    capability: &str,
    received: impl FnOnce(Instant),
) -> Result<Response> {
    use pb::contracts::{
        robonix_system_scene_get_robot_context_client::RobonixSystemSceneGetRobotContextClient,
        robonix_system_soma_get_health_client::RobonixSystemSomaGetHealthClient,
        robonix_system_vitals_get_client::RobonixSystemVitalsGetClient,
    };
    if !supports(capability) {
        bail!("unsupported gRPC capability '{capability}'");
    }
    let endpoint = endpoint.trim();
    let endpoint = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_owned()
    } else {
        format!("http://{endpoint}")
    };
    let channel = Endpoint::from_shared(endpoint)?
        .connect_timeout(CONTROL_TIMEOUT)
        .connect()
        .await
        .context("connect to gRPC input source")?;
    match capability {
        SOMA => decode(
            RobonixSystemSomaGetHealthClient::new(channel)
                .get_health(pb::soma::GetHealthRequest {})
                .await
                .context("Soma GetHealth")?,
            received,
        ),
        VITALS => decode(
            RobonixSystemVitalsGetClient::new(channel)
                .get_vitals(pb::vitals::GetVitalsRequest {})
                .await
                .context("Vitals Get")?,
            received,
        ),
        SCENE => decode(
            RobonixSystemSceneGetRobotContextClient::new(channel)
                .get_robot_context(pb::semantic_map::GetRobotContextRequest {})
                .await
                .context("Scene GetRobotContext")?,
            received,
        ),
        _ => unreachable!(),
    }
}

/// Timestamp receipt before JSON conversion so decoding cannot refresh the data's age.
/// Retain snapshot wrappers and arrays; absent optional messages serialize as null.
fn decode<T: serde::Serialize>(
    response: tonic::Response<T>,
    received: impl FnOnce(Instant),
) -> Result<Response> {
    let acquired_at = tokio::time::Instant::now();
    received(acquired_at);
    let value = serde_json::to_value(response.into_inner()).context("serialize gRPC response")?;
    Ok(Response { value, acquired_at })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use futures::stream;
    use tokio::{
        net::TcpListener,
        time::{Instant, timeout},
    };
    use tonic::{Request, Status};

    use super::*;
    use pb::contracts::{
        robonix_system_scene_get_robot_context_server::{
            RobonixSystemSceneGetRobotContext, RobonixSystemSceneGetRobotContextServer,
        },
        robonix_system_soma_get_health_server::{
            RobonixSystemSomaGetHealth, RobonixSystemSomaGetHealthServer,
        },
        robonix_system_vitals_get_server::{RobonixSystemVitalsGet, RobonixSystemVitalsGetServer},
    };

    /// Supplies typed snapshots over the same generated interfaces as real providers.
    #[derive(Clone)]
    struct Snapshots {
        /// Switches Scene from a successful response to an explicit RPC failure.
        fail_scene: Arc<AtomicBool>,
    }

    #[tonic::async_trait]
    impl RobonixSystemSomaGetHealth for Snapshots {
        async fn get_health(
            &self,
            _: Request<pb::soma::GetHealthRequest>,
        ) -> Result<tonic::Response<pb::soma::GetHealthResponse>, Status> {
            Ok(tonic::Response::new(pb::soma::GetHealthResponse {
                snapshot: Some(pb::soma::SomaHealthSnapshot {
                    actuators: vec![pb::soma::ActuatorState {
                        component_id: "left_wheel".into(),
                        velocity: Some(pb::soma::Scalar {
                            value: 2.5,
                            unit: "m/s".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            }))
        }
    }

    #[tonic::async_trait]
    impl RobonixSystemVitalsGet for Snapshots {
        async fn get_vitals(
            &self,
            _: Request<pb::vitals::GetVitalsRequest>,
        ) -> Result<tonic::Response<pb::vitals::GetVitalsResponse>, Status> {
            Ok(tonic::Response::new(pb::vitals::GetVitalsResponse {
                snapshot: Some(pb::vitals::VitalsSnapshot {
                    ts_ns: 123,
                    power: Some(pb::vitals::PowerState {
                        battery_percent: 75.0,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            }))
        }
    }

    #[tonic::async_trait]
    impl RobonixSystemSceneGetRobotContext for Snapshots {
        async fn get_robot_context(
            &self,
            _: Request<pb::semantic_map::GetRobotContextRequest>,
        ) -> Result<tonic::Response<pb::semantic_map::GetRobotContextResponse>, Status> {
            if self.fail_scene.load(Ordering::SeqCst) {
                return Err(Status::unavailable("scene unavailable"));
            }
            Ok(tonic::Response::new(
                pb::semantic_map::GetRobotContextResponse {
                    x: 1.5,
                    stale: true,
                    containing_area_ids: vec!["room".into()],
                    ..Default::default()
                },
            ))
        }
    }

    #[tokio::test]
    async fn acquires_three_snapshot_interfaces_without_restructuring_responses() {
        let fail_scene = Arc::new(AtomicBool::new(false));
        let source = Snapshots {
            fail_scene: fail_scene.clone(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let incoming = stream::unfold(listener, |listener| async {
            Some((listener.accept().await.map(|(socket, _)| socket), listener))
        });
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(RobonixSystemSomaGetHealthServer::new(source.clone()))
                .add_service(RobonixSystemVitalsGetServer::new(source.clone()))
                .add_service(RobonixSystemSceneGetRobotContextServer::new(source))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });
        let mut snapshots = Vec::new();
        for capability in [SOMA, VITALS, SCENE] {
            let started = Instant::now();
            let response = timeout(CONTROL_TIMEOUT, acquire(&endpoint, capability, |_| {}))
                .await
                .unwrap()
                .unwrap();
            assert!(response.acquired_at >= started && response.acquired_at <= Instant::now());
            snapshots.push(response.value);
        }
        let actuator = &snapshots[0]["snapshot"]["actuators"][0];
        assert_eq!(actuator["component_id"], "left_wheel");
        assert_eq!(actuator["velocity"]["value"], 2.5);
        assert_eq!(actuator["velocity"]["unit"], "m/s");
        assert_eq!(snapshots[1]["snapshot"]["ts_ns"], 123);
        assert_eq!(snapshots[1]["snapshot"]["power"]["battery_percent"], 75.0);
        assert_eq!(snapshots[2]["x"], 1.5);
        assert_eq!(snapshots[2]["stale"], true);
        assert_eq!(
            snapshots[2]["containing_area_ids"],
            serde_json::json!(["room"])
        );
        fail_scene.store(true, Ordering::SeqCst);
        let error = timeout(CONTROL_TIMEOUT, acquire(&endpoint, SCENE, |_| {}))
            .await
            .unwrap()
            .unwrap_err();
        assert!(format!("{error:#}").contains("scene unavailable"));
        assert!(
            acquire("not an endpoint", "unknown/capability", |_| {})
                .await
                .unwrap_err()
                .to_string()
                .contains("unsupported gRPC capability")
        );
        server.abort();
    }
}
