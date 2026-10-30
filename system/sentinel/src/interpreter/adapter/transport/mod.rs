//! Capability transport calls and connection cleanup, independent of SRDL rules.

use std::{future::Future, time::Duration};

use robonix_atlas::{client::AtlasClient, pb};
use serde_json::Value;
use tokio::time::{Instant, timeout};

mod grpc;
mod mcp;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// A logical capability response with its local acquisition time.
#[derive(Debug)]
pub struct Response {
    /// JSON data decoded from the protocol response without SRDL selection.
    pub value: Value,
    /// Monotonic time immediately after receiving the call response.
    pub acquired_at: Instant,
}

/// Failures in transport selection, connection, invocation or cleanup.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no supported transport for capability '{0}'")]
    UnsupportedTransport(String),
    #[error("Atlas connection failed: {0}")]
    AtlasConnect(#[source] anyhow::Error),
    #[error("Atlas channel cleanup failed: {0}")]
    AtlasDisconnect(#[source] anyhow::Error),
    #[error("{0} timed out")]
    TimedOut(&'static str),
    #[error("MCP acquisition failed: {0}")]
    Mcp(#[source] anyhow::Error),
    #[error("gRPC acquisition failed: {0}")]
    Grpc(#[source] anyhow::Error),
}

/// Invoke an argument-free capability and release its Atlas channel.
///
/// The cancellation future stops acquisition but allows cleanup to finish. Callers
/// must keep polling this function until it returns; None means acquisition was cancelled.
/// Connection and cleanup have fixed timeouts; the caller controls the overall acquisition deadline.
pub async fn acquire(
    atlas: &AtlasClient,
    provider: &pb::CapabilityProvider,
    capability: &str,
    consumer_id: &str,
    cancelled: impl Future<Output = ()>,
) -> Option<Result<Response, Error>> {
    let transport = match select_transport(provider, capability) {
        Ok(transport) => transport,
        Err(error) => return Some(Err(error)),
    };
    let mut atlas = atlas.clone();
    // Once sent, Connect must finish so cancellation cannot discard a known channel ID.
    let connection = timeout(
        CONTROL_TIMEOUT,
        atlas.connect_capability(consumer_id, &provider.id, capability, transport),
    )
    .await;
    let (channel_id, endpoint, _) = match connection {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => return Some(Err(Error::AtlasConnect(error))),
        Err(_) => return Some(Err(Error::TimedOut("Atlas connection"))),
    };
    let result = match transport {
        pb::Transport::Mcp => mcp::acquire(&endpoint, capability, cancelled)
            .await
            .map(|result| result.map_err(Error::Mcp)),
        pb::Transport::Grpc => tokio::select! {
            biased;
            _ = cancelled => None,
            result = grpc::acquire(&endpoint, capability) => Some(result.map_err(Error::Grpc)),
        },
        _ => unreachable!(),
    };
    // Cleanup runs after success, failure or cancellation; it must not race cancellation.
    let cleanup = match timeout(CONTROL_TIMEOUT, atlas.disconnect_capability(&channel_id)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(Error::AtlasDisconnect(error)),
        Err(_) => Err(Error::TimedOut("Atlas channel cleanup")),
    };
    match (result, cleanup) {
        (Some(Ok(value)), Ok(())) => Some(Ok(value)),
        (Some(Err(error)), cleanup) => {
            // Preserve the invocation failure and report any additional cleanup failure separately.
            if let Err(cleanup) = cleanup {
                robonix_scribe::warn!("{cleanup}");
            }
            Some(Err(error))
        }
        (Some(Ok(_)), Err(error)) => Some(Err(error)),
        (None, Err(error)) => {
            robonix_scribe::warn!("{error}");
            None
        }
        (None, Ok(())) => None,
    }
}

/// Prefer typed gRPC for known contracts, then MCP; only exact contract matches qualify.
/// A gRPC advertisement alone does not provide the message types needed for arbitrary calls.
fn select_transport(
    provider: &pb::CapabilityProvider,
    capability: &str,
) -> Result<pb::Transport, Error> {
    let offers = |transport| {
        provider
            .capabilities
            .iter()
            .any(|cap| cap.contract_id == capability && cap.transport() == transport)
    };
    if grpc::supports(capability) && offers(pb::Transport::Grpc) {
        return Ok(pb::Transport::Grpc);
    }
    if offers(pb::Transport::Mcp) {
        return Ok(pb::Transport::Mcp);
    }
    Err(Error::UnsupportedTransport(capability.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_supported_grpc_then_mcp_and_rejects_unknown_grpc() {
        use pb::Transport::{Grpc, Mcp};
        let soma = "robonix/system/soma/get_health";
        for (contract, transports, expected) in [
            (soma, vec![Mcp, Grpc], Some(Grpc)),
            (soma, vec![Mcp], Some(Mcp)),
            ("sensor/value", vec![Grpc], None),
            ("sensor/value", vec![Grpc, Mcp], Some(Mcp)),
        ] {
            let provider = pb::CapabilityProvider {
                capabilities: transports
                    .into_iter()
                    .map(|transport| pb::Capability {
                        contract_id: contract.into(),
                        transport: transport as i32,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            };
            let result = select_transport(&provider, contract);
            match expected {
                Some(transport) => assert_eq!(result.unwrap(), transport),
                None => assert!(matches!(result, Err(Error::UnsupportedTransport(_)))),
            }
            assert!(select_transport(&provider, "unrelated/contract").is_err());
        }
    }
}
