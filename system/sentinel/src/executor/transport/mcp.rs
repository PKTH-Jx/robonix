//! MCP response acquisition without SRDL selection or type coercion.

use anyhow::{Context, Result, bail};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    transport::StreamableHttpClientTransport,
};
use serde_json::Value;
use tokio::time::Instant;

use super::{CONTROL_TIMEOUT, Response};

type McpClient = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// Call the contract's leaf-named MCP tool with empty arguments and close the session.
/// Timestamp the call response before decoding and cleanup; neither updates acquisition time.
pub(super) async fn acquire(endpoint: &str, capability: &str) -> Result<Response> {
    let mut client = connect(endpoint).await?;
    // Match Executor's mapping from contract IDs to MCP tool names.
    let name = capability
        .rsplit('/')
        .next()
        .context("missing MCP tool name")?;
    let result = async {
        let response = client
            .call_tool(
                CallToolRequestParams::new(name.to_owned()).with_arguments(Default::default()),
            )
            .await
            .context("MCP call_tool")?;
        let acquired_at = Instant::now();
        Ok(Response {
            value: decode(response)?,
            acquired_at,
        })
    }
    .await;
    let closed = client.close_with_timeout(CONTROL_TIMEOUT).await;
    match closed {
        Ok(Some(_)) => result,
        failure => {
            if let Err(error) = &result {
                robonix_scribe::warn!("{error:#}");
            }
            bail!("MCP session cleanup failed: {failure:?}")
        }
    }
}

/// Initialize MCP over a stdio child process or streamable HTTP.
/// HTTP endpoints follow Executor's default /mcp convention; stdio arguments are whitespace-separated.
async fn connect(endpoint: &str) -> Result<McpClient> {
    if let Some(command) = endpoint.strip_prefix("stdio://") {
        let mut parts = command.split_whitespace();
        let program = parts.next().context("empty MCP stdio command")?;
        let mut command = tokio::process::Command::new(program);
        command.args(parts).kill_on_drop(true);
        return ().serve(rmcp::transport::TokioChildProcess::new(command)?)
            .await.context("MCP stdio initialization");
    }
    let base = if endpoint.contains("://") {
        endpoint.to_owned()
    } else {
        format!("http://{endpoint}")
    };
    let uri = if base.trim_end_matches('/').ends_with("/mcp") {
        base
    } else {
        format!("{}/mcp", base.trim_end_matches('/'))
    };
    ().serve(StreamableHttpClientTransport::from_uri(uri))
        .await
        .context("MCP HTTP initialization")
}

/// Prefer structured JSON; otherwise require exactly one text block containing valid JSON.
/// Reject tool failures and ambiguous content instead of silently discarding response blocks.
fn decode(response: CallToolResult) -> Result<Value> {
    if response.is_error == Some(true) {
        bail!("MCP tool reported failure: {:?}", response.content);
    }
    if let Some(value) = response.structured_content {
        return Ok(value);
    }
    let [content] = response.content.as_slice() else {
        bail!("MCP input requires structured content or exactly one JSON text block");
    };
    let text = content
        .as_text()
        .context("MCP input content must be JSON text")?;
    serde_json::from_str(&text.text).context("invalid MCP response JSON")
}

#[cfg(test)]
mod tests {
    use rmcp::model::Content;
    use serde_json::json;

    use super::*;

    #[test]
    fn decodes_structured_and_json_text_responses_without_coercion() {
        let value = json!({"reading": 42, "ready": true});
        for response in [
            CallToolResult::structured(value.clone()),
            CallToolResult::success(vec![Content::text(value.to_string())]),
        ] {
            assert_eq!(decode(response).unwrap(), value);
        }
        for response in [
            CallToolResult::error(vec![Content::text("unavailable")]),
            CallToolResult::success(vec![Content::text("not JSON")]),
            CallToolResult::success(vec![]),
            CallToolResult::success(vec![Content::text("1"), Content::text("2")]),
        ] {
            assert!(decode(response).is_err());
        }
    }
}
