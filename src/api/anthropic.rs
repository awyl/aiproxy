//! Anthropic API routes (/v1/models, /v1/messages) with surface gating.

use crate::api::body::replace_model_field;
use crate::api::{AppState, Surface, anthropic_error, check_surface, relay_or_error};
use crate::auth::apply_auth;
use crate::provider::{ModelSurface, ProviderError, RequestContext};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::Value;

pub fn anthropic_router(token: Option<String>) -> Router<AppState> {
    anthropic_router_with_subs(token, &[])
}

/// Router builder that additionally accepts subscription tokens for auth.
pub fn anthropic_router_with_subs(token: Option<String>, subs: &[String]) -> Router<AppState> {
    // NOTE: /v1/models is served once, by the OpenAI router (OpenAI shape).
    // Anthropic clients listing models get the same catalog; the ids are what
    // matter and the two shapes differ only cosmetically.
    apply_auth(
        Router::new().route("/v1/messages", post(messages)),
        crate::auth::auth_state(token, subs),
    )
}

pub async fn messages(
    State(state): State<AppState>,
    Extension(token): Extension<Option<String>>,
    headers: axum::http::HeaderMap,
    raw: Bytes,
) -> axum::response::Response {
    let Ok(req) = serde_json::from_slice::<Value>(&raw) else {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "request body is not valid JSON",
            "invalid_request_error",
        );
    };
    let Some(model) = req.get("model").and_then(Value::as_str) else {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "missing 'model' field",
            "invalid_request_error",
        );
    };
    let Some((pid, mid)) = state.registry.resolve(model) else {
        let prefixes: Vec<&str> = state.registry.prefixes().collect();
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            format!(
                "unknown model '{model}'; use a prefixed model id (upstream/model); known prefixes: {}",
                prefixes.join(", ")
            ),
            "invalid_request_error",
        );
    };
    // Per-upstream subscription gate (see api::check_subscription).
    if crate::api::check_subscription(&state, &pid, token.as_deref()).is_err() {
        return anthropic_error(
            StatusCode::UNAUTHORIZED,
            format!(
                "upstream '{pid}' is subscription-gated; this token does not own it (or its token_env is not set)"
            ),
            "authentication_error",
        );
    }
    let provider = state.registry.provider(&pid).unwrap();
    if let Err(resp) = check_surface(
        provider.as_ref(),
        &mid,
        ModelSurface::Messages,
        Surface::Anthropic,
    ) {
        return resp;
    }
    // Byte-faithful relay (see api::body): patch only the model id.
    let stripped = replace_model_field(&raw, &mid).unwrap_or_else(|| raw.to_vec());
    let ctx = RequestContext {
        model: mid,
        client_headers: headers,
        usage_tracker: Some(state.usage.clone()),
    };
    match provider.messages(Bytes::from(stripped), &ctx).await {
        Ok(stream) => relay_or_error(Ok(stream), Surface::Anthropic),
        Err(ProviderError::Transport(msg)) if msg.contains("surface") => {
            anthropic_error(StatusCode::BAD_REQUEST, msg, "invalid_request_error")
        }
        Err(e) => relay_or_error(Err(e), Surface::Anthropic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::provider::ModelSurface;
    use crate::provider::testutil::MockProvider;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn test_state() -> AppState {
        let providers: Vec<std::sync::Arc<dyn crate::provider::Provider>> = vec![
            std::sync::Arc::new(MockProvider::with_surface(
                "openai",
                vec!["gpt-4o".into()],
                ModelSurface::ChatCompletions,
            )),
            std::sync::Arc::new(MockProvider::with_surface(
                "anthropic",
                vec!["claude-sonnet-4".into()],
                ModelSurface::Messages,
            )),
            std::sync::Arc::new(MockProvider::with_surface(
                "opencode-go",
                vec!["grok-4.6".into()],
                ModelSurface::Responses,
            )),
        ];
        let reg = crate::discovery::ModelRegistry::new(providers);
        reg.refresh().await;
        AppState {
            registry: std::sync::Arc::new(reg),
            embeddings: std::sync::Arc::new(crate::embeddings::EmbeddingManager::new(
                &crate::config::EmbeddingsConfig::default(),
            )),
            token: Some("tok".into()),
            subscriptions: Default::default(),
            usage: crate::usage::UsageTracker::new(),
            cookie_path: std::path::PathBuf::from("/tmp/test-cookie"),
            upstream_names: vec![],
        }
    }

    async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, String) {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    fn post(path: &str, body: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .method("POST")
            .header("authorization", "Bearer tok")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn requires_auth() {
        let app = anthropic_router(Some("tok".into())).with_state(test_state().await);
        let (status, _) = send(
            app,
            Request::builder()
                .uri("/v1/messages")
                .method("POST")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn messages_streams_relayed_bytes() {
        let app = anthropic_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "anthropic/claude-sonnet-4", "messages": [{"role": "user", "content": "hi"}]});
        let (status, body) = send(app, post("/v1/messages", &req.to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("data: {\"ok\":true}\n\n"));
    }

    #[tokio::test]
    async fn rejects_unknown_prefix() {
        let app = anthropic_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "claude-sonnet-4", "messages": []});
        let (status, body) = send(app, post("/v1/messages", &req.to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("prefixed"));
    }

    #[tokio::test]
    async fn messages_route_rejects_chat_surface_model() {
        let app = anthropic_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "openai/gpt-4o", "messages": []});
        let (status, body) = send(app, post("/v1/messages", &req.to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("surface"));
    }

    #[tokio::test]
    async fn messages_route_rejects_responses_surface_model() {
        let app = anthropic_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "opencode-go/grok-4.6", "messages": []});
        let (status, body) = send(app, post("/v1/messages", &req.to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("surface"));
    }
}
