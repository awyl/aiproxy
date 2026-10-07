//! MCP hosting e2e: proxy spins up the echo stdio fixture backend and serves
//! it over streamable HTTP at /mcp/<name>; a real rmcp client connects with
//! the shared token, lists tools, and calls `echo`.

use aiproxy::config::Config;
use aiproxy::server;
use rmcp::model::*;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::json;

fn echo_cfg(exe: &str) -> Config {
    let yaml = format!(
        "bind: 127.0.0.1:0\ntoken: mcp-tok\nupstreams:\n  - name: mock\n    kind: openai\n    models: [gpt-4o]\nmcp:\n  servers:\n    - name: echo\n      command: {exe}\n"
    );
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("aiproxy-mcp-{}-{}.yaml", std::process::id(), n));
    std::fs::write(&path, yaml).unwrap();
    let cfg = Config::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    cfg
}

#[tokio::test]
async fn stdio_backend_serves_tools_through_http() {
    let exe = env!("CARGO_BIN_EXE_echo_mcp_server");
    let cfg = echo_cfg(exe);
    let (listener, router) = server::build(cfg, std::env::temp_dir().join("aiproxy-mcp-test.yaml"))
        .await
        .expect("daemon build");
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(
            format!("http://127.0.0.1:{}/mcp/echo", addr.port()), // loopback: sandbox rejects Host: 0.0.0.0
        )
        .auth_header("mcp-tok"), // reqwest adds the "Bearer " prefix
    );
    let info = ClientInfo::new(
        rmcp::model::ClientCapabilities::default(),
        Implementation::new("mcp-test-client", "0.0.1"),
    );
    let mut client = rmcp::serve_client(info, transport)
        .await
        .expect("mcp client connect");

    let _server_info = client.peer_info().expect("peer info");

    let tools = client.list_tools(None).await.expect("list_tools");
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains(&"echo"));

    let call_result = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("echo").with_arguments(
                json!({"input": "hello world"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .expect("call_tool");
    let text: String = call_result
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("hello world"), "echoed text: {text}");

    client.close().await.ok();
    handle.abort();
}

/// Multiplexer e2e: the shared /mcp endpoint lists prefixed tools and routes
/// calls through the same cached backend as /mcp/<name>.
#[tokio::test]
async fn multiplexer_lists_and_calls_through_shared_backend() {
    let exe = env!("CARGO_BIN_EXE_echo_mcp_server");
    let cfg = echo_cfg(exe);
    let (listener, router) = server::build(cfg, std::env::temp_dir().join("aiproxy-mcp-test.yaml"))
        .await
        .expect("daemon build");
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/mcp", addr.port());

    // tools/list → prefixed tool names
    let list: serde_json::Value = client
        .post(&url)
        .header("authorization", "Bearer mcp-tok")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .send()
        .await
        .expect("tools/list send")
        .json()
        .await
        .expect("tools/list json");
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(names.contains(&"echo__echo"), "prefixed tools: {names:?}");

    // tools/call → routed to the echo backend
    let call: serde_json::Value = client
        .post(&url)
        .header("authorization", "Bearer mcp-tok")
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "echo__echo", "arguments": {"input": "via-multiplexer"}}
        }))
        .send()
        .await
        .expect("tools/call send")
        .json()
        .await
        .expect("tools/call json");
    let text = call.to_string();
    assert!(text.contains("via-multiplexer"), "echoed text: {text}");

    handle.abort();
}
