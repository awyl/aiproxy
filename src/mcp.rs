//! MCP hosting: one streamable-HTTP endpoint per configured server at
//! `/mcp/<name>`, each backed by a `ProxyHandler` that forwards `tools/list`
//! and `tools/call` to a lazily-connected backend (stdio child or remote
//! streamable-HTTP server). Reconnect on failure: a failed backend call
//! drops the cached handle; the next request reconnects.

use crate::api::AppState;
use crate::auth::apply_auth;
use crate::config::McpServerConfig;
use axum::Router;
use rmcp::model::*;
use rmcp::service::RunningService;
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ErrorData, ServerHandler};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;

pub type Backend = RunningService<rmcp::RoleClient, ClientInfo>;

pub fn mcp_router(
    servers: &[McpServerConfig],
    global_token: &Option<String>,
    bind_host: &str,
    allowed_hosts: &[String],
) -> Result<Router<AppState>, String> {
    let mut router = Router::<AppState>::new();
    let mut allowed: Vec<String> = if allowed_hosts.is_empty() {
        vec!["localhost".into(), "127.0.0.1".into(), "::1".into()]
    } else {
        allowed_hosts.to_vec()
    };
    if !allowed.contains(&bind_host.to_string()) {
        allowed.push(bind_host.into());
    }
    for server in servers {
        // Per-server auth: token_env > literal token > global fallback.
        let effective_token = server.effective_token(global_token);

        let name = server.clone();
        let mut server_config = StreamableHttpServerConfig::default();
        server_config.allowed_hosts = allowed.clone();
        // Stateless mode: no session IDs, no DNS-rebinding-like session lookup.
        // Clients re-initialize on every connection instead of tracking session IDs,
        // which avoids "Session not found" when SSE streams reconnect.
        server_config.legacy_session_mode = false;
        let service: StreamableHttpService<ProxyHandler, _> = StreamableHttpService::new(
            move || Ok(ProxyHandler::new(name.clone())),
            LocalSessionManager::default().into(),
            server_config,
        );
        let path = format!("/mcp/{}", server.name);
        let mut server_router = Router::new().nest_service(path.as_str(), service);
        // Wrap this server's routes with its own auth layer.
        if let Some(tok) = effective_token {
            server_router = apply_auth(server_router, crate::auth::auth_state(Some(tok), &[]));
        }
        router = router.merge(server_router);
    }
    Ok(router)
}

#[derive(Debug, Clone)]
pub struct ProxyHandler {
    cfg: McpServerConfig,
    backend: Arc<Mutex<Option<Backend>>>,
}

impl ProxyHandler {
    pub fn new(cfg: McpServerConfig) -> Self {
        Self {
            cfg,
            backend: Arc::new(Mutex::new(None)),
        }
    }

    async fn backend(&self) -> Result<tokio::sync::MutexGuard<'_, Option<Backend>>, ErrorData> {
        let mut guard = self.backend.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect().await?);
        }
        Ok(guard)
    }

    async fn connect(&self) -> Result<Backend, ErrorData> {
        let info = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("aiproxy", env!("CARGO_PKG_VERSION")),
        );
        if let Some(cmd) = &self.cfg.command {
            let mut command = Command::new(cmd);
            command.args(&self.cfg.args).envs(&self.cfg.env);
            // stderr -> null: children must not inherit the daemon's stderr
            // (a long-lived child would otherwise hold the terminal pipe open).
            let transport = TokioChildProcess::builder(command)
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| ErrorData::internal_error(format!("stdio spawn failed: {e}"), None))?
                .0;
            rmcp::serve_client(info, transport)
                .await
                .map_err(|e| ErrorData::internal_error(format!("stdio backend: {e}"), None))
        } else if let Some(url) = &self.cfg.url {
            let config = StreamableHttpClientTransportConfig::with_uri(url.clone());
            let config = match self.cfg.api_key() {
                Some(k) => config.auth_header(k), // reqwest adds the "Bearer " prefix
                None => config,
            };
            let transport = StreamableHttpClientTransport::from_config(config);
            rmcp::serve_client(info, transport)
                .await
                .map_err(|e| ErrorData::internal_error(format!("remote backend: {e}"), None))
        } else {
            Err(ErrorData::invalid_params(
                "server has neither command nor url",
                None,
            ))
        }
    }
}

impl ServerHandler for ProxyHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        {
            let mut guard = self.backend().await?;
            let backend = guard.as_mut().expect("backend just connected");
            match backend.list_tools(None).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    tracing::warn!(server = %self.cfg.name, "backend list_tools failed: {e}; reconnecting");
                    *guard = None;
                }
            }
        }
        // reconnect and retry once
        let mut guard = self.backend().await?;
        let backend = guard.as_mut().expect("backend just connected");
        backend
            .list_tools(None)
            .await
            .map_err(|e| ErrorData::internal_error(format!("backend list_tools: {e}"), None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        {
            let mut guard = self.backend().await?;
            let backend = guard.as_mut().expect("backend just connected");
            match backend.call_tool(request.clone()).await {
                Ok(result) => return Ok(CallToolResponse::from(result)),
                Err(e) => {
                    tracing::warn!(server = %self.cfg.name, "backend call_tool failed: {e}; reconnecting");
                    *guard = None;
                }
            }
        }
        let mut guard = self.backend().await?;
        let backend = guard.as_mut().expect("backend just connected");
        backend
            .call_tool(request)
            .await
            .map(CallToolResponse::from)
            .map_err(|e| ErrorData::internal_error(format!("backend call_tool: {e}"), None))
    }
}

// ── MCP multiplexer: aggregate multiple backends under /mcp ────────────────

/// Parsed entry from X-MCP-Servers header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerEntry {
    pub name: String,
    pub token: Option<String>,
}

/// Parse `X-MCP-Servers` header value.
/// Format: `name:token,name,name:token`
/// - `name:token` → token provided
/// - `name` → no token (use auth fallback)
pub fn parse_mcp_servers_header(value: &str) -> Vec<McpServerEntry> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| match entry.split_once(':') {
            Some((name, token)) => McpServerEntry {
                name: name.trim().to_string(),
                token: Some(token.trim().to_string()),
            },
            None => McpServerEntry {
                name: entry.to_string(),
                token: None,
            },
        })
        .collect()
}

/// Resolve the token to check against a server's effective_token.
/// Priority: header token > Authorization header fallback.
pub fn resolve_check_token(entry: &McpServerEntry, auth_header: Option<&str>) -> Option<String> {
    entry
        .token
        .clone()
        .or_else(|| auth_header.map(String::from))
}

/// Check if a token grants access to a server.
pub fn check_server_auth(check_token: Option<&str>, effective_token: &Option<String>) -> bool {
    match effective_token {
        // Server is open (no token required) → always grant
        None => true,
        // Server requires token → check match
        Some(required) => match check_token {
            Some(provided) => provided == required,
            None => false,
        },
    }
}

#[cfg(test)]
mod multiplexer_tests {
    use super::*;

    #[test]
    fn parse_single_server_with_token() {
        let entries = parse_mcp_servers_header("searxng:my_secret");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "searxng");
        assert_eq!(entries[0].token.as_deref(), Some("my_secret"));
    }

    #[test]
    fn parse_single_server_without_token() {
        let entries = parse_mcp_servers_header("ctx7");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "ctx7");
        assert_eq!(entries[0].token, None);
    }

    #[test]
    fn parse_multiple_servers_mixed() {
        let entries = parse_mcp_servers_header("searxng:tok_a,ctx7,grep:tok_b");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "searxng");
        assert_eq!(entries[0].token.as_deref(), Some("tok_a"));
        assert_eq!(entries[1].name, "ctx7");
        assert_eq!(entries[1].token, None);
        assert_eq!(entries[2].name, "grep");
        assert_eq!(entries[2].token.as_deref(), Some("tok_b"));
    }

    #[test]
    fn parse_empty_header() {
        let entries = parse_mcp_servers_header("");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_whitespace_handling() {
        let entries = parse_mcp_servers_header(" searxng : tok_a , ctx7 ");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "searxng");
        assert_eq!(entries[0].token.as_deref(), Some("tok_a"));
        assert_eq!(entries[1].name, "ctx7");
    }

    #[test]
    fn resolve_token_header_over_auth() {
        let entry = McpServerEntry {
            name: "x".into(),
            token: Some("header_tok".into()),
        };
        assert_eq!(
            resolve_check_token(&entry, Some("auth_tok")),
            Some("header_tok".into())
        );
    }

    #[test]
    fn resolve_token_fallback_to_auth() {
        let entry = McpServerEntry {
            name: "x".into(),
            token: None,
        };
        assert_eq!(
            resolve_check_token(&entry, Some("auth_tok")),
            Some("auth_tok".into())
        );
    }

    #[test]
    fn resolve_token_no_token_anywhere() {
        let entry = McpServerEntry {
            name: "x".into(),
            token: None,
        };
        assert_eq!(resolve_check_token(&entry, None), None);
    }

    #[test]
    fn auth_open_server_always_grants() {
        assert!(check_server_auth(None, &None));
        assert!(check_server_auth(Some("anything"), &None));
    }

    #[test]
    fn auth_required_token_match() {
        assert!(check_server_auth(Some("secret"), &Some("secret".into())));
        assert!(!check_server_auth(Some("wrong"), &Some("secret".into())));
    }

    #[test]
    fn auth_required_no_token_denies() {
        assert!(!check_server_auth(None, &Some("secret".into())));
    }
}
