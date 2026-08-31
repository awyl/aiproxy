//! Test fixture: minimal stdio MCP server exposing one `echo` tool.
//! Spawned by aiproxy's stdio backend in tests/mcp_test.rs.

use rmcp::model::*;
use rmcp::{ErrorData, ServerHandler, serve_server};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
struct EchoServer;

impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let schema = json!({
            "type": "object",
            "properties": { "input": { "type": "string" } },
            "required": ["input"]
        });
        let schema_arc: std::sync::Arc<serde_json::Map<String, serde_json::Value>> =
            schema.as_object().cloned().unwrap().into();
        let tool = Tool::new("echo", "echoes input back", schema_arc);
        Ok(ListToolsResult::with_all_items(vec![tool]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let input = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("input"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let result = CallToolResult::success(vec![ContentBlock::text(
            json!({"echoed": input}).to_string(),
        )]);
        Ok(CallToolResponse::Complete(result))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let running = serve_server(EchoServer, rmcp::transport::stdio()).await?;
    tracing::info!("echo mcp server ready");
    // keep the process alive; RunningService's background tasks serve the client
    std::future::pending::<()>().await;
    let _ = running;
    Ok(())
}
