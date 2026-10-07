//! Server assembly: providers -> registry -> AppState -> routers, listener.
//! Real MCP hosting lands in Task 12 (placeholder now).

use crate::api::AppState;
use crate::api::anthropic::anthropic_router_with_subs;
use crate::api::openai::openai_router_with_subs;
use crate::config::Config;
use crate::discovery::ModelRegistry;
use crate::providers::build_providers;
use axum::Router;
use axum::routing::{get, post};
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpListener;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("bind failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("mcp setup failed: {0}")]
    Mcp(String),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
}

/// Codex OAuth endpoints. Defaults come from the env hooks
/// (`AIPROXY_CODEX_AUTH_BASE_URL`, `AIPROXY_CODEX_TOKEN_URL`) so integration
/// tests can point a real daemon at mock servers instead of mutating
/// process-global env mid-run.
#[derive(Debug, Clone)]
pub struct CodexOptions {
    pub auth_base_url: String,
    pub token_url: String,
}

impl CodexOptions {
    pub fn from_env() -> Self {
        Self {
            auth_base_url: crate::codex_oauth::auth_base_url(),
            token_url: crate::codex_oauth::token_url(),
        }
    }
}

/// Build the app. Binds per `config.bind` unless `port_override` is set
/// (CLI `--port`), which replaces the port portion of the bind string.
/// `config_path` locates on-disk state that sits next to the config file
/// (Codex OAuth tokens).
pub async fn build_with_port(
    config: Config,
    config_path: std::path::PathBuf,
    port_override: Option<u16>,
) -> Result<(TcpListener, Router), ServerError> {
    build_with_options(config, config_path, port_override, CodexOptions::from_env()).await
}

/// `build_with_port` with the Codex endpoints supplied explicitly.
pub async fn build_with_options(
    config: Config,
    config_path: std::path::PathBuf,
    port_override: Option<u16>,
    codex: CodexOptions,
) -> Result<(TcpListener, Router), ServerError> {
    let (host, port) = config.bind_host_port()?;
    let port = port_override.unwrap_or(port);

    let codex_managers = Arc::new(crate::providers::create_codex_managers(
        &config,
        Some(config_path.as_path()),
        &codex.token_url,
    ));
    let providers = build_providers(&config, &codex_managers);
    let registry = Arc::new(ModelRegistry::new(providers));
    registry.refresh().await;

    // Codex background refresh: wake up every ~10 min (plus jitter), refresh
    // any access token inside the 60-minute margin. The manager coalesces
    // concurrent refreshes and backs off on failures.
    for manager in codex_managers.values() {
        // Say where credentials are looked for, at boot. "The login vanished
        // after a restart" is almost always this path pointing at a directory
        // that did not survive the restart (e.g. only the config file is
        // mounted), and the log is the only place that shows it.
        let state = manager.status().await;
        tracing::info!(
            path = %manager.state_path().display(),
            logged_in = matches!(state, crate::codex_oauth::CodexStatus::LoggedIn { .. }),
            "codex credentials"
        );
        let manager = manager.clone();
        tokio::spawn(async move {
            loop {
                let jitter = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| u64::from(d.subsec_nanos()) % 60)
                    .unwrap_or(0);
                tokio::time::sleep(std::time::Duration::from_secs(10 * 60 + jitter)).await;
                manager.background_tick().await;
            }
        });
    }

    let refresh_secs = config.model_refresh_secs;
    if refresh_secs > 0 {
        let reg = registry.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(refresh_secs));
            loop {
                tick.tick().await;
                reg.refresh().await;
            }
        });
    }

    let ids = config.provider_ids();
    let mut subscriptions = std::collections::HashMap::new();
    for (u, id) in config.upstreams.iter().zip(&ids) {
        match u.subscription_token() {
            Some(Some(tok)) => {
                subscriptions.insert(id.clone(), Some(tok));
                tracing::info!(upstream = %id, "subscription-gated upstream enabled");
            }
            Some(None) => {
                tracing::error!(
                    upstream = %id,
                    token_env = u.token_env.as_deref().unwrap_or(""),
                    "upstream subscription token_env set but env var missing/empty — upstream is deny-all"
                );
                subscriptions.insert(id.clone(), None);
            }
            None => {}
        }
    }
    let subscription_values: Vec<String> = subscriptions.values().flatten().cloned().collect();

    let token = config.effective_token();
    if token.is_none() && subscription_values.is_empty() {
        tracing::warn!("no auth token configured — API is unauthenticated");
    }
    let usage = crate::usage::UsageTracker::new();
    let state = AppState {
        registry,
        embeddings: std::sync::Arc::new(crate::embeddings::EmbeddingManager::new(
            &config.embeddings,
        )),
        token: token.clone(),
        subscriptions,
        usage: usage.clone(),
        codex_managers: codex_managers.clone(),
        codex_auth_base: codex.auth_base_url.clone(),
        codex_flows: Default::default(),
    };

    // Background usage fetcher for upstreams with billing endpoints.
    let usage_fetchers: Vec<crate::usage::FetcherConfig> = config
        .upstreams
        .iter()
        .filter(|u| {
            matches!(
                u.kind,
                crate::config::UpstreamKind::Minimax
                    | crate::config::UpstreamKind::Openrouter
                    | crate::config::UpstreamKind::Zai
                    | crate::config::UpstreamKind::OpencodeGo
            )
        })
        .map(|u| {
            let api_key = u
                .token_env
                .as_ref()
                .and_then(|env_var| std::env::var(env_var).ok())
                .or_else(|| {
                    u.api_key_env
                        .as_ref()
                        .and_then(|env_var| std::env::var(env_var).ok())
                });
            let provider_name = if config
                .upstreams
                .iter()
                .filter(|u2| u2.kind == u.kind)
                .count()
                > 1
            {
                format!("{}={}", u.kind.as_str(), u.name.as_deref().unwrap_or(""))
            } else {
                u.kind.as_str().to_string()
            };
            crate::usage::FetcherConfig {
                kind: u.kind.as_str().to_string(),
                provider_name: provider_name.clone(),
                api_key,
                base_url: u.base_url.clone(),
            }
        })
        .collect();
    crate::usage::spawn_refresh(usage, usage_fetchers, 60);

    // Embedding models: idle reaper (unload after idle_ttl_secs).
    let emb = state.embeddings.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            emb.reaper_round().await;
        }
    });

    // MCP backend cache: one backend session per server, shared by the
    // per-server /mcp/<name> routes and the /mcp multiplexer.
    let mcp_cache = crate::mcp::BackendCache::default();
    let mcp_router = crate::mcp::mcp_router(
        &config.mcp.servers,
        &token,
        &host,
        &config.mcp.allowed_hosts,
        &mcp_cache,
    )
    .map_err(ServerError::Mcp)?;

    // MCP multiplexer: single /mcp endpoint aggregating all servers
    let multiplex_state = crate::mcp_multiplexer::McpMultiplexState {
        servers: config.mcp.servers.clone(),
        global_token: token.clone(),
        cache: mcp_cache,
    };
    let multiplex_router =
        crate::mcp_multiplexer::mcp_multiplex_route().with_state(multiplex_state);

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/usage", get(crate::pages::usage_page))
        .route("/models", get(crate::pages::models_page))
        .route("/api/models", get(crate::pages::models_api))
        .route("/api/reload", post(crate::pages::reload_models))
        .route("/setup", get(crate::setup::setup_page))
        .route("/api/codex/start", post(crate::setup::codex_start))
        .route("/api/codex/complete", post(crate::setup::codex_complete))
        .route("/api/codex/status", get(crate::setup::codex_status))
        .route("/api/codex/providers", get(crate::setup::codex_providers))
        .merge(openai_router_with_subs(token.clone(), &subscription_values))
        .merge(anthropic_router_with_subs(
            token.clone(),
            &subscription_values,
        ))
        .merge(mcp_router)
        .merge(multiplex_router)
        .with_state(state);

    let listener = TcpListener::bind((host.as_str(), port)).await?;
    Ok((listener, app))
}

pub async fn build(
    config: Config,
    config_path: std::path::PathBuf,
) -> Result<(TcpListener, Router), ServerError> {
    build_with_port(config, config_path, None).await
}

pub async fn run(config: Config, config_path: std::path::PathBuf) -> Result<(), ServerError> {
    let (listener, app) = build(config, config_path).await?;
    serve(listener, app).await
}

pub async fn run_with_port(
    config: Config,
    config_path: std::path::PathBuf,
    port_override: Option<u16>,
) -> Result<(), ServerError> {
    let (listener, app) = build_with_port(config, config_path, port_override).await?;
    serve(listener, app).await
}

async fn serve(listener: TcpListener, app: Router) -> Result<(), ServerError> {
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await
        .map_err(Into::into)
}
