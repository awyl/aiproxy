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
    let path = std::env::temp_dir().join(format!("aiproxy-mcp-{}.yaml", std::process::id()));
    std::fs::write(&path, yaml).unwrap();
    let cfg = Config::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    cfg
}

#[tokio::test]
async fn stdio_backend_serves_tools_through_http() {
    let exe = env!("CARGO_BIN_EXE_echo_mcp_server");
    let (listener, router) = server::build(echo_cfg(exe)).await.expect("daemon build");
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
