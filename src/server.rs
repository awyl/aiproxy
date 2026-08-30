//! Server assembly: providers -> registry -> AppState -> routers, listener.
//! Real MCP hosting lands in Task 12 (placeholder now).

use std::sync::Arc;
use axum::routing::get;
use axum::Router;
use thiserror::Error;
use tokio::net::TcpListener;
use crate::api::anthropic::anthropic_router;
use crate::api::openai::openai_router;
use crate::api::AppState;
use crate::config::Config;
use crate::discovery::ModelRegistry;
use crate::providers::build_providers;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("bind failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("mcp setup failed: {0}")]
    Mcp(String),
}

pub async fn build(config: Config) -> Result<(TcpListener, Router), ServerError> {
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

    let token = config.effective_token();
    if token.is_none() {
        tracing::warn!("no auth token configured — API is unauthenticated");
    }
    let state = AppState {
        registry,
        token: token.clone(),
    };

    let mcp_router = crate::mcp::mcp_router(&config.mcp.servers, token.clone()).map_err(ServerError::Mcp)?;

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(openai_router(token.clone()))
        .merge(anthropic_router(token.clone()))
        .merge(mcp_router)
        .with_state(state);

    let listener = TcpListener::bind(("127.0.0.1", config.port)).await?;
    Ok((listener, app))
}

pub async fn run(config: Config) -> Result<(), ServerError> {
    let (listener, app) = build(config).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await
        .map_err(Into::into)
}