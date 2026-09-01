//! Server assembly: providers -> registry -> AppState -> routers, listener.
//! Real MCP hosting lands in Task 12 (placeholder now).

use crate::api::AppState;
use crate::api::anthropic::anthropic_router_with_subs;
use crate::api::openai::openai_router_with_subs;
use crate::config::Config;
use crate::discovery::ModelRegistry;
use crate::providers::build_providers;
use axum::Router;
use axum::routing::get;
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

/// Build the app. Binds per `config.bind` unless `port_override` is set
/// (CLI `--port`), which replaces the port portion of the bind string.
pub async fn build_with_port(
    config: Config,
    port_override: Option<u16>,
) -> Result<(TcpListener, Router), ServerError> {
    let (host, port) = config.bind_host_port()?;
    let port = port_override.unwrap_or(port);

    let providers = build_providers(&config);
    let registry = Arc::new(ModelRegistry::new(providers));
    registry.refresh().await;

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

    let mut subscriptions = std::collections::HashMap::new();
    for u in &config.upstreams {
        match u.subscription_token() {
            Some(Some(tok)) => {
                subscriptions.insert(u.name.clone(), Some(tok));
                tracing::info!(upstream = %u.name, "subscription-gated upstream enabled");
            }
            Some(None) => {
                tracing::error!(
                    upstream = %u.name,
                    token_env = u.token_env.as_deref().unwrap_or(""),
                    "upstream subscription token_env set but env var missing/empty — upstream is deny-all"
                );
                subscriptions.insert(u.name.clone(), None);
            }
            None => {}
        }
    }
    let subscription_values: Vec<String> = subscriptions.values().flatten().cloned().collect();

    let token = config.effective_token();
    if token.is_none() && subscription_values.is_empty() {
        tracing::warn!("no auth token configured — API is unauthenticated");
    }
    let state = AppState {
        registry,
        embeddings: std::sync::Arc::new(crate::embeddings::EmbeddingManager::new(
            &config.embeddings,
        )),
        token: token.clone(),
        subscriptions,
    };

    // Embedding models: idle reaper (unload after idle_ttl_secs).
    let emb = state.embeddings.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            emb.reaper_round().await;
        }
    });

    let mcp_router = crate::mcp::mcp_router(
        &config.mcp.servers,
        &token,
        &host,
        &config.mcp.allowed_hosts,
    )
    .map_err(ServerError::Mcp)?;

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(openai_router_with_subs(token.clone(), &subscription_values))
        .merge(anthropic_router_with_subs(
            token.clone(),
            &subscription_values,
        ))
        .merge(mcp_router)
        .with_state(state);

    let listener = TcpListener::bind((host.as_str(), port)).await?;
    Ok((listener, app))
}

pub async fn build(config: Config) -> Result<(TcpListener, Router), ServerError> {
    build_with_port(config, None).await
}

pub async fn run(config: Config) -> Result<(), ServerError> {
    let (listener, app) = build(config).await?;
    serve(listener, app).await
}

pub async fn run_with_port(config: Config, port_override: Option<u16>) -> Result<(), ServerError> {
    let (listener, app) = build_with_port(config, port_override).await?;
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
