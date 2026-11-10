//! MCP response acquisition without SRDL selection or type coercion.

use std::future::Future;

use anyhow::{Context, Result, bail};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use tokio::time::Instant;

use super::{CONTROL_TIMEOUT, Response};

type McpClient = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// Call the contract's leaf-named MCP tool with empty arguments and close the session.
/// Timestamp the call response before decoding and cleanup; neither updates acquisition time.
pub(super) async fn acquire(
    endpoint: &str,
    capability: &str,
    cancelled: impl Future<Output = ()>,
    received: impl FnOnce(Instant),
) -> Option<Result<Response>> {
    // Finish initialization before cancellation so its transport remains owned through cleanup.
    let mut client = match connect(endpoint).await {
        Ok(client) => client,
        Err(error) => return Some(Err(error)),
    };
    // Match Executor's mapping from contract IDs to MCP tool names.
    let name = capability.rsplit('/').next().unwrap_or_default();
    let result = tokio::select! {
        biased;
        _ = cancelled => None,
        result = async {
            let response = client
                .call_tool(
                    CallToolRequestParams::new(name.to_owned()).with_arguments(Default::default()),
                )
                .await
                .context("MCP call_tool")?;
            let acquired_at = Instant::now();
            received(acquired_at);
            Ok(Response {
                value: decode(response)?,
                acquired_at,
            })
        } => Some(result),
    };
    // rmcp's close_with_timeout detaches unfinished cleanup. Await close instead;
    // HTTP request deadlines and rmcp's bounded session deletion bound this wait.
    let closed = client.close().await;
    match closed {
        Ok(_) => result,
        failure => {
            // Cancellation may have closed the receiver, so returning this error is insufficient.
            robonix_scribe::warn!("MCP session cleanup failed: {failure:?}");
            if let Some(Err(error)) = &result {
                robonix_scribe::warn!("{error:#}");
            }
            Some(Err(anyhow::anyhow!(
                "MCP session cleanup failed: {failure:?}"
            )))
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
        return tokio::time::timeout(
            CONTROL_TIMEOUT,
            ().serve(rmcp::transport::TokioChildProcess::new(command)?),
        )
        .await
        .context("MCP stdio initialization timed out")?
        .context("MCP stdio initialization");
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
    let client = reqwest::Client::builder()
        .timeout(CONTROL_TIMEOUT)
        .pool_max_idle_per_host(0)
        .build()
        .context("build MCP HTTP client")?;
    let transport = StreamableHttpClientTransport::with_client(
        client,
        StreamableHttpClientTransportConfig::with_uri(uri),
    );
    ().serve(transport).await.context("MCP HTTP initialization")
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
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use rmcp::model::Content;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::{Notify, oneshot},
        task::JoinSet,
        time::timeout,
    };

    use super::*;

    // A raw server lets the test observe socket closure without returning HTTP headers.
    async fn serve_request(
        mut socket: TcpStream,
        entered: Arc<Notify>,
        live: Arc<AtomicUsize>,
        cleanup: Option<Arc<Notify>>,
    ) {
        let mut request = Vec::new();
        let header_end = loop {
            let byte = socket.read_u8().await.unwrap();
            request.push(byte);
            if request.ends_with(b"\r\n\r\n") {
                break request.len();
            }
        };
        let headers = String::from_utf8_lossy(&request);
        let method = headers.split_whitespace().next().unwrap().to_owned();
        let length = headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        request.resize(header_end + length, 0);
        socket.read_exact(&mut request[header_end..]).await.unwrap();
        let body: Value = serde_json::from_slice(&request[header_end..]).unwrap_or(Value::Null);
        if body["method"] == "tools/call" && cleanup.is_none() {
            live.fetch_add(1, Ordering::SeqCst);
            entered.notify_one();
            let mut byte = [0];
            assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
            live.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let (status, body) = if body["method"] == "initialize" {
            (
                "200 OK",
                json!({"jsonrpc":"2.0", "id":body["id"], "result": {
                    "protocolVersion":"2025-03-26", "capabilities":{},
                    "serverInfo":{"name":"hanging-test", "version":"1"}
                }})
                .to_string(),
            )
        } else if body["method"] == "tools/call" {
            (
                "200 OK",
                json!({"jsonrpc":"2.0", "id":body["id"], "result": {
                    "content":[], "structuredContent":{"reading":42}
                }})
                .to_string(),
            )
        } else if method == "DELETE" {
            if let Some(cleanup) = cleanup {
                entered.notify_one();
                cleanup.notified().await;
            }
            ("200 OK", String::new())
        } else if method == "GET" {
            ("405 Method Not Allowed", String::new())
        } else {
            ("202 Accepted", String::new())
        };
        socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nMcp-Session-Id: test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_waits_for_hanging_http_requests_to_terminate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let entered = Arc::new(Notify::new());
        let live = Arc::new(AtomicUsize::new(0));
        let server = {
            let entered = entered.clone();
            let live = live.clone();
            tokio::spawn(async move {
                let mut requests = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            requests.spawn(serve_request(accepted.unwrap().0, entered.clone(), live.clone(), None));
                        }
                        result = requests.join_next(), if !requests.is_empty() => { result.unwrap().unwrap(); }
                    }
                }
            })
        };
        for _ in 0..2 {
            let (cancel, cancelled) = oneshot::channel();
            let call = acquire(
                &endpoint,
                "sensor/value",
                async {
                    let _ = cancelled.await;
                },
                |_| {},
            );
            tokio::pin!(call);
            timeout(Duration::from_secs(10), async {
                tokio::select! {
                    _ = entered.notified() => {},
                    result = &mut call => panic!("call ended before cancellation: {result:?}"),
                }
            })
            .await
            .unwrap();
            assert_eq!(live.load(Ordering::SeqCst), 1);
            cancel.send(()).unwrap();
            assert!(
                timeout(Duration::from_secs(15), &mut call)
                    .await
                    .unwrap()
                    .is_none()
            );
            timeout(Duration::from_secs(1), async {
                while live.load(Ordering::SeqCst) != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn reports_receipt_before_waiting_for_session_cleanup() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server = {
            let entered = entered.clone();
            let release = release.clone();
            tokio::spawn(async move {
                let mut requests = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            requests.spawn(serve_request(accepted.unwrap().0, entered.clone(), Arc::new(AtomicUsize::new(0)), Some(release.clone())));
                        }
                        result = requests.join_next(), if !requests.is_empty() => { result.unwrap().unwrap(); }
                    }
                }
            })
        };
        let (received, mut arrival) = oneshot::channel();
        let call = acquire(&endpoint, "sensor/value", std::future::pending(), |at| {
            let _ = received.send(at);
        });
        tokio::pin!(call);
        // Initialization and transport shutdown have separate five-second timeouts.
        timeout(Duration::from_secs(15), async {
            tokio::select! {
                _ = entered.notified() => {},
                result = &mut call => panic!("acquisition ended before cleanup was released: {result:?}"),
            }
        }).await.unwrap();
        let acquired_at = arrival
            .try_recv()
            .expect("receipt notification precedes cleanup");
        assert!(futures::poll!(&mut call).is_pending());
        release.notify_one();
        let response = timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response.acquired_at, acquired_at);
        assert_eq!(response.value["reading"], 42);
        server.abort();
        let _ = server.await;
    }

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
