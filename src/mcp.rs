//! MCP hosting — Task 12 placeholder. Signature is final:
//! `mcp_router(servers, token)`, auth applied, real rmcp hosting lands in Task 12.

use axum::Router;
use crate::api::AppState;
use crate::auth::apply_auth;
use crate::config::McpServerConfig;

pub fn mcp_router(servers: &[McpServerConfig], token: Option<String>) -> Result<Router<AppState>, String> {
    if servers.is_empty() {
        Ok(apply_auth(Router::<AppState>::new(), token))
    } else {
        Err("mcp hosting not implemented yet".to_string())
    }
}