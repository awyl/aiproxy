// ── MCP multiplexer: aggregate multiple backends under /mcp ────────────────

use crate::config::McpServerConfig;
use crate::mcp::{
    Backend, McpServerEntry, check_server_auth, parse_mcp_servers_header, resolve_check_token,
};
use axum::Router;
use axum::extract::{Json, State};
use axum::http::HeaderMap;
use axum::http::{HeaderName, StatusCode};
use axum::response::IntoResponse;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;

/// Shared state for the multiplexed /mcp endpoint.
#[derive(Clone)]
pub struct McpMultiplexState {
    pub servers: Vec<McpServerConfig>,
    pub global_token: Option<String>,
}

/// Extract auth token from Authorization header (Bearer <token>).
fn extract_auth_token(headers: &HeaderMap) -> Option<String> {
    let val = headers
        .get(HeaderName::from_static("authorization"))?
        .to_str()
        .ok()?;
    val.strip_prefix("Bearer ").map(String::from)
}

/// Build the multiplexed /mcp route.
pub fn mcp_multiplex_route() -> Router<McpMultiplexState> {
    Router::new().route("/mcp", axum::routing::post(mcp_multiplex_handler))
}

/// POST /mcp — aggregate MCP endpoint with per-server auth via X-MCP-Servers header.
async fn mcp_multiplex_handler(
    State(state): State<McpMultiplexState>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> axum::response::Response {
    let auth_token = extract_auth_token(&headers);

    // Parse X-MCP-Servers header (or default to all servers)
    let entries = match headers.get("x-mcp-servers") {
        Some(val) => {
            let s = val.to_str().unwrap_or("");
            parse_mcp_servers_header(s)
        }
        None => state
            .servers
            .iter()
            .map(|s| McpServerEntry {
                name: s.name.clone(),
                token: None,
            })
            .collect(),
    };

    // Resolve which servers the client has access to
    let mut matched: Vec<&McpServerConfig> = Vec::new();
    for entry in &entries {
        if let Some(server) = state.servers.iter().find(|s| s.name == entry.name) {
            let check_token = resolve_check_token(entry, auth_token.as_deref());
            let effective = server.effective_token(&state.global_token);
            if check_server_auth(check_token.as_deref(), &effective) {
                matched.push(server);
            }
        }
    }

    // Dispatch MCP request
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id");

    match method {
        "initialize" => {
            let result = json!({
                "protocolVersion": "2025-03-26",
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "aiproxy",
                    "version": env!("CARGO_PKG_VERSION")
                }
            });
            jsonrpc_response(id, &result)
        }
        "tools/list" => {
            let mut all_tools: Vec<Value> = Vec::new();
            for server in &matched {
                match connect_backend(server).await {
                    Ok(backend) => {
                        let mut guard = backend.lock().await;
                        if let Some(b) = guard.as_mut() {
                            match b.list_tools(None).await {
                                Ok(result) => {
                                    for tool in result.tools {
                                        let prefixed = format!("{}__{}", server.name, tool.name);
                                        all_tools.push(json!({
                                            "name": prefixed,
                                            "description": tool.description,
                                            "inputSchema": tool.input_schema,
                                        }));
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        server = %server.name,
                                        "list_tools failed: {e}"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(server = %server.name, "connect failed: {e}");
                    }
                }
            }
            let result = json!({ "tools": all_tools });
            jsonrpc_response(id, &result)
        }
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(json!({}));
            let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");

            // Parse prefix: "searxng__search" -> ("searxng", "search")
            let (prefix, real_name) = match tool_name.split_once("__") {
                Some((p, n)) => (p, n),
                None => {
                    return jsonrpc_error(
                        id,
                        -32602,
                        &format!("tool name must be prefixed: <server>__<tool>, got '{tool_name}'"),
                    );
                }
            };

            // Find the server
            let server = match matched.iter().find(|s| s.name == prefix) {
                Some(s) => *s,
                None => {
                    return jsonrpc_error(
                        id,
                        -32602,
                        &format!("no access to server '{prefix}' (check X-MCP-Servers header)"),
                    );
                }
            };

            // Route to backend
            match connect_backend(server).await {
                Ok(backend) => {
                    let mut guard = backend.lock().await;
                    if let Some(b) = guard.as_mut() {
                        let arguments = params.get("arguments").and_then(Value::as_object).cloned();
                        let mut call_params = CallToolRequestParams::new(real_name.to_string());
                        if let Some(args) = arguments {
                            call_params = call_params.with_arguments(args);
                        }
                        match b.call_tool(call_params).await {
                            Ok(call_result) => {
                                let val = serde_json::to_value(&call_result).unwrap_or(json!({}));
                                jsonrpc_response(id, &val)
                            }
                            Err(e) => {
                                jsonrpc_error(id, -32603, &format!("backend call failed: {e}"))
                            }
                        }
                    } else {
                        jsonrpc_error(id, -32603, "backend not connected")
                    }
                }
                Err(e) => jsonrpc_error(id, -32603, &format!("connect failed: {e}")),
            }
        }
        _ => jsonrpc_error(id, -32601, &format!("method not found: {method}")),
    }
}

/// Connect to a server's backend (lazy, cached).
async fn connect_backend(server: &McpServerConfig) -> Result<Arc<Mutex<Option<Backend>>>, String> {
    let info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("aiproxy", env!("CARGO_PKG_VERSION")),
    );
    let backend: Backend = if let Some(cmd) = &server.command {
        let mut command = Command::new(cmd);
        command.args(&server.args).envs(&server.env);
        let transport = TokioChildProcess::builder(command)
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("stdio spawn failed: {e}"))?
            .0;
        rmcp::serve_client(info, transport)
            .await
            .map_err(|e| format!("stdio backend: {e}"))?
    } else if let Some(url) = &server.url {
        let config = StreamableHttpClientTransportConfig::with_uri(url.clone());
        let config = match server.api_key() {
            Some(k) => config.auth_header(k),
            None => config,
        };
        let transport = StreamableHttpClientTransport::from_config(config);
        rmcp::serve_client(info, transport)
            .await
            .map_err(|e| format!("remote backend: {e}"))?
    } else {
        return Err("server has neither command nor url".into());
    };
    Ok(Arc::new(Mutex::new(Some(backend))))
}

fn jsonrpc_response(id: Option<&Value>, result: &Value) -> axum::response::Response {
    let resp = json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(&Value::Null),
        "result": result,
    });
    (StatusCode::OK, Json(resp)).into_response()
}

fn jsonrpc_error(id: Option<&Value>, code: i64, message: &str) -> axum::response::Response {
    let resp = json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(&Value::Null),
        "error": { "code": code, "message": message },
    });
    (StatusCode::OK, Json(resp)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state(servers: Vec<McpServerConfig>, token: Option<String>) -> McpMultiplexState {
        McpMultiplexState {
            servers,
            global_token: token,
        }
    }

    fn test_server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: Some("echo".to_string()),
            args: vec![],
            env: Default::default(),
            url: None,
            api_key_env: None,
            token: None,
            token_env: None,
        }
    }

    fn test_server_with_token(name: &str, token: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: Some("echo".to_string()),
            args: vec![],
            env: Default::default(),
            url: None,
            api_key_env: None,
            token: Some(token.to_string()),
            token_env: None,
        }
    }

    async fn send(app: axum::Router<()>, req: Request<Body>) -> (StatusCode, Value) {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
        (status, body)
    }

    fn mcp_request(method: &str) -> Request<Body> {
        Request::builder()
            .uri("/mcp")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": method}).to_string(),
            ))
            .unwrap()
    }

    fn mcp_request_with_headers(method: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut builder = Request::builder()
            .uri("/mcp")
            .method("POST")
            .header("content-type", "application/json");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": method}).to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let state = test_state(vec![], None);
        let app = mcp_multiplex_route().with_state(state);
        let (status, body) = send(app, mcp_request("initialize")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["result"]["serverInfo"]["name"], "aiproxy");
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let state = test_state(vec![], None);
        let app = mcp_multiplex_route().with_state(state);
        let (status, body) = send(app, mcp_request("bogus")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not found")
        );
    }

    #[tokio::test]
    async fn tools_list_no_servers_returns_empty() {
        let state = test_state(vec![], None);
        let app = mcp_multiplex_route().with_state(state);
        let (status, body) = send(app, mcp_request("tools/list")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["tools"], json!([]));
    }

    #[tokio::test]
    async fn x_mcp_servers_header_filters_servers() {
        let servers = vec![test_server("a"), test_server("b"), test_server("c")];
        let state = test_state(servers, None);
        let app = mcp_multiplex_route().with_state(state);
        let req = mcp_request_with_headers("tools/list", &[("x-mcp-servers", "a,c")]);
        let (status, _body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_rejects_wrong_token() {
        let servers = vec![test_server_with_token("sec", "right-token")];
        let state = test_state(servers, Some("global-tok".into()));
        let app = mcp_multiplex_route().with_state(state);
        let req = mcp_request_with_headers("tools/list", &[("x-mcp-servers", "sec:wrong-token")]);
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["tools"], json!([]));
    }

    #[tokio::test]
    async fn auth_accepts_correct_token() {
        let servers = vec![test_server_with_token("sec", "right-token")];
        let state = test_state(servers, Some("global-tok".into()));
        let app = mcp_multiplex_route().with_state(state);
        let req = mcp_request_with_headers("tools/list", &[("x-mcp-servers", "sec:right-token")]);
        let (status, _body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn no_header_uses_auth_fallback() {
        let servers = vec![test_server_with_token("sec", "my-tok")];
        let state = test_state(servers, None);
        let app = mcp_multiplex_route().with_state(state);
        let req = mcp_request_with_headers("tools/list", &[("authorization", "Bearer my-tok")]);
        let (status, _body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn header_token_overrides_auth_header() {
        let servers = vec![test_server_with_token("sec", "header-tok")];
        let state = test_state(servers, None);
        let app = mcp_multiplex_route().with_state(state);
        let req = mcp_request_with_headers(
            "tools/list",
            &[
                ("x-mcp-servers", "sec:header-tok"),
                ("authorization", "Bearer auth-tok"),
            ],
        );
        let (status, _body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn open_server_always_included() {
        let servers = vec![test_server("open")];
        let state = test_state(servers, Some("global-tok".into()));
        let app = mcp_multiplex_route().with_state(state);
        let req = mcp_request("tools/list");
        let (status, _body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn tools_call_requires_prefix() {
        let state = test_state(vec![], None);
        let app = mcp_multiplex_route().with_state(state);
        let req = Request::builder()
            .uri("/mcp")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": "no_prefix"}
                })
                .to_string(),
            ))
            .unwrap();
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("prefixed")
        );
    }

    #[tokio::test]
    async fn tools_call_unknown_server() {
        let state = test_state(vec![], None);
        let app = mcp_multiplex_route().with_state(state);
        let req = Request::builder()
            .uri("/mcp")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": "unknown__tool"}
                })
                .to_string(),
            ))
            .unwrap();
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no access")
        );
    }
}
