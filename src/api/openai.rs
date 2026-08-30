//! OpenAI API routes (/v1/models, /v1/chat/completions, /v1/responses) with
//! surface gating. Handlers resolve the prefixed model, strip the prefix,
//! verify the wire surface, then relay the provider's SSE stream.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Future;
use serde_json::{json, Value};
use std::pin::Pin;
use std::sync::Arc;
use crate::api::{check_surface, openai_error, relay_or_error, AppState, Surface};
use crate::auth::apply_auth;
use crate::provider::{ModelSurface, Provider, ProviderError, ProviderStream, RequestContext};

pub fn openai_router(token: Option<String>) -> Router<AppState> {
    apply_auth(
        Router::new()
            .route("/v1/models", get(list_models))
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/responses", post(responses)),
        token,
    )
}

pub async fn list_models(State(state): State<AppState>) -> axum::response::Response {
    let data: Vec<Value> = state
        .registry
        .models()
        .into_iter()
        .map(|m| json!({"id": m.id, "object": "model", "created": m.created_at, "owned_by": ""}))
        .collect();
    (StatusCode::OK, Json(json!({"object": "list", "data": data}))).into_response()
}

/// Resolve prefixed model -> provider + stripped id, verify the wire surface,
/// then invoke the provider method (passed as a closure to share this prelude
/// between the chat and responses handlers).
async fn route_one(
    state: &AppState,
    req: Value,
    required: ModelSurface,
    surface: Surface,
    call: impl FnOnce(
        Arc<dyn Provider>,
        Value,
        String,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderStream, ProviderError>> + Send>>,
) -> Result<ProviderStream, axum::response::Response> {
    let Some(model) = req.get("model").and_then(Value::as_str) else {
        return Err(openai_error(
            StatusCode::BAD_REQUEST,
            "missing 'model' field",
            "invalid_request_error",
        ));
    };
    let Some((pid, mid)) = state.registry.resolve(model) else {
        let prefixes: Vec<&str> = state.registry.prefixes().collect();
        return Err(openai_error(
            StatusCode::BAD_REQUEST,
            format!(
                "unknown model '{model}'; use a prefixed model id (upstream/model); known prefixes: {}",
                prefixes.join(", ")
            ),
            "invalid_request_error",
        ));
    };
    let provider = state.registry.provider(&pid).unwrap();
    if let Err(resp) = check_surface(provider.as_ref(), &mid, required, surface) {
        return Err(resp);
    }
    let mut stripped = req.clone();
    stripped["model"] = json!(mid);
    call(provider, stripped, mid)
        .await
        .map_err(|e| match e {
            ProviderError::Transport(msg) if msg.contains("surface") => {
                openai_error(StatusCode::BAD_REQUEST, msg, "invalid_request_error")
            }
            // upstream Http/transport failures translate to the surface's error shape
            other => relay_error(other, surface),
        })
}

/// Non-streaming variant of `relay_or_error` for the error path of `route_one`.
fn relay_error(e: ProviderError, surface: Surface) -> axum::response::Response {
    match e {
        ProviderError::Http { status, body } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            (status, axum::Json(body)).into_response()
        }
        ProviderError::Transport(msg) => match surface {
            Surface::Openai => openai_error(StatusCode::BAD_GATEWAY, msg, "upstream_error"),
            Surface::Anthropic => crate::api::anthropic_error(StatusCode::BAD_GATEWAY, msg, "api_error"),
        },
    }
}

async fn chat_completions(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    match route_one(
        &state,
        req,
        ModelSurface::ChatCompletions,
        Surface::Openai,
        |p, body, model| {
            Box::pin(async move { p.chat_completions(body, &RequestContext { model }).await })
        },
    )
    .await
    {
        Ok(stream) => relay_or_error(Ok(stream), Surface::Openai),
        Err(resp) => resp,
    }
}

async fn responses(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    match route_one(
        &state,
        req,
        ModelSurface::Responses,
        Surface::Openai,
        |p, body, model| {
            Box::pin(async move { p.responses(body, &RequestContext { model }).await })
        },
    )
    .await
    {
        Ok(stream) => relay_or_error(Ok(stream), Surface::Openai),
        Err(resp) => resp,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::provider::testutil::MockProvider;
    use crate::provider::ModelSurface;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    async fn test_state() -> AppState {
        let providers: Vec<std::sync::Arc<dyn crate::provider::Provider>> = vec![
            std::sync::Arc::new(MockProvider::with_surface("openai", vec!["gpt-4o".into()], ModelSurface::ChatCompletions)),
            std::sync::Arc::new(MockProvider::with_surface("anthropic", vec!["claude-sonnet-4".into()], ModelSurface::Messages)),
            std::sync::Arc::new(MockProvider::with_surface("opencode-go", vec!["grok-4.6".into()], ModelSurface::Responses)),
        ];
        let reg = crate::discovery::ModelRegistry::new(providers);
        reg.refresh().await;
        AppState {
            registry: std::sync::Arc::new(reg),
            token: Some("tok".into()),
        }
    }

    async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, String) {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
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
    async fn models_endpoint_lists_prefixed_catalog() {
        let app = openai_router(Some("tok".into())).with_state(test_state().await);
        let (status, body) = send(
            app,
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer tok")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        let ids: Vec<&str> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["anthropic/claude-sonnet-4", "openai/gpt-4o", "opencode-go/grok-4.6"]
        );
        assert_eq!(v["data"][0]["object"], "model");
    }

    #[tokio::test]
    async fn requires_auth() {
        let app = openai_router(Some("tok".into())).with_state(test_state().await);
        let (status, _) = send(
            app,
            Request::builder()
                .uri("/v1/chat/completions")
                .method("POST")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn chat_completions_streams_relayed_bytes() {
        let app = openai_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "hi"}]});
        let (status, body) = send(app, post("/v1/chat/completions", &req.to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("data: {\"ok\":true}\n\n"));
    }

    #[tokio::test]
    async fn responses_streams_relayed_bytes() {
        let app = openai_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "opencode-go/grok-4.6", "input": "hi"});
        let (status, body) = send(app, post("/v1/responses", &req.to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("data: {\"ok\":true}\n\n"));
    }

    #[tokio::test]
    async fn rejects_unknown_prefix() {
        let app = openai_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "gpt-4o", "messages": []});
        let (status, body) = send(app, post("/v1/chat/completions", &req.to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("prefixed"));
    }

    #[tokio::test]
    async fn chat_route_rejects_messages_surface_model() {
        let app = openai_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "anthropic/claude-sonnet-4", "messages": []});
        let (status, body) = send(app, post("/v1/chat/completions", &req.to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("surface"));
    }

    #[tokio::test]
    async fn chat_route_rejects_responses_surface_model() {
        let app = openai_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "opencode-go/grok-4.6", "messages": []});
        let (status, body) = send(app, post("/v1/chat/completions", &req.to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("surface"));
    }

    #[tokio::test]
    async fn responses_route_rejects_chat_surface_model() {
        let app = openai_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "openai/gpt-4o", "input": "hi"});
        let (status, body) = send(app, post("/v1/responses", &req.to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("surface"));
    }
}
