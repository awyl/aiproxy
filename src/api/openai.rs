//! OpenAI API routes (/v1/models, /v1/chat/completions, /v1/responses) with
//! surface gating. Handlers resolve the prefixed model, strip the prefix,
//! verify the wire surface, then relay the provider's SSE stream.

use crate::api::{AppState, Surface, check_surface, openai_error, relay_or_error};
use crate::auth::apply_auth;
use crate::provider::{ModelSurface, Provider, ProviderError, ProviderStream, RequestContext};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Future;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;

pub fn openai_router(token: Option<String>) -> Router<AppState> {
    openai_router_with_subs(token, &[])
}

/// Router builder that additionally accepts subscription tokens for auth.
pub fn openai_router_with_subs(token: Option<String>, subs: &[String]) -> Router<AppState> {
    apply_auth(
        Router::new()
            .route("/v1/models", get(list_models))
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/responses", post(responses))
            .route("/v1/embeddings", post(embeddings)),
        crate::auth::auth_state(token, subs),
    )
}

pub async fn list_models(State(state): State<AppState>) -> axum::response::Response {
    let mut data: Vec<Value> = state
        .registry
        .models()
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "created": m.created_at,
                "owned_by": "",
                "display_name": m.display_name,
                "surface": m.surface.as_str(),
            })
        })
        .collect();
    // Fake local-embeddings provider: config-driven, never discovered.
    for id in state.embeddings.model_ids() {
        data.push(json!({
            "id": format!("embeddings-local/{id}"),
            "object": "model",
            "created": null,
            "owned_by": "",
            "display_name": null,
            "surface": "embedding",
        }));
    }
    (
        StatusCode::OK,
        Json(json!({"object": "list", "data": data})),
    )
        .into_response()
}

/// Resolve prefixed model -> provider + stripped id, verify the wire surface,
/// then invoke the provider method (passed as a closure to share this prelude
/// between the chat and responses handlers).
async fn route_one(
    state: &AppState,
    req: Value,
    token: Option<String>,
    required: ModelSurface,
    surface: Surface,
    call: impl FnOnce(
        Arc<dyn Provider>,
        Value,
        String,
    )
        -> Pin<Box<dyn Future<Output = Result<ProviderStream, ProviderError>> + Send>>,
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
    // Per-upstream subscription gate: subscription tokens (token_env) lock
    // each upstream to the holder. Global auth already accepted this request;
    // this checks the subscription-scoped token.
    if crate::api::check_subscription(state, &pid, token.as_deref()).is_err() {
        return Err(openai_error(
            StatusCode::UNAUTHORIZED,
            format!(
                "upstream '{pid}' is subscription-gated; this token does not own it (or its token_env is not set)"
            ),
            "authentication_error",
        ));
    }
    let provider = state.registry.provider(&pid).unwrap();
    check_surface(provider.as_ref(), &mid, required, surface)?;
    let mut stripped = req.clone();
    stripped["model"] = json!(mid);
    call(provider, stripped, mid).await.map_err(|e| match e {
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
            Surface::Anthropic => {
                crate::api::anthropic_error(StatusCode::BAD_GATEWAY, msg, "api_error")
            }
        },
    }
}

async fn chat_completions(
    State(state): State<AppState>,
    Extension(token): Extension<Option<String>>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    match route_one(
        &state,
        req,
        token,
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
    Extension(token): Extension<Option<String>>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    match route_one(
        &state,
        req,
        token,
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

/// Fake `embeddings-local` provider: relay a /v1/embeddings request to the
/// on-demand local llama-server child for the model.
async fn embeddings(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    let Some(model) = req.get("model").and_then(Value::as_str) else {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "missing 'model' field",
            "invalid_request_error",
        );
    };
    let Some(mid) = model.strip_prefix("embeddings-local/") else {
        return openai_error(
            StatusCode::BAD_REQUEST,
            format!("unknown model '{model}'; use embeddings-local/<id> for embedding models"),
            "invalid_request_error",
        );
    };
    match state.embeddings.embed(mid, &req).await {
        Ok(v) => (StatusCode::OK, axum::Json(v)).into_response(),
        Err(e) => match e {
            crate::embeddings::EmbedError::UnknownModel(id) => openai_error(
                StatusCode::BAD_REQUEST,
                format!("unknown embedding model '{id}'"),
                "invalid_request_error",
            ),
            crate::embeddings::EmbedError::SpawnFailed(id, msg) => openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "embedding backend '{id}' failed to start: {msg}; check llama_bin and model_file"
                ),
                "upstream_error",
            ),
            crate::embeddings::EmbedError::NotReady(id, msg) => openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("embedding backend '{id}' not ready: {msg}"),
                "upstream_error",
            ),
            crate::embeddings::EmbedError::Http(status, body) => {
                let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                let body = serde_json::from_str::<Value>(&body)
                    .unwrap_or_else(|_| json!({ "error": { "message": body } }));
                (status, axum::Json(body)).into_response()
            }
            crate::embeddings::EmbedError::Transport(msg) => {
                openai_error(StatusCode::BAD_GATEWAY, msg, "upstream_error")
            }
        },
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
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap())); // outlive spawned fakes
        let embed = crate::embeddings::testutil::manager_with_fake(
            dir.path(),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(20010)),
            3600,
            &["nomic"],
        );
        AppState {
            registry: std::sync::Arc::new(reg),
            embeddings: std::sync::Arc::new(embed),
            token: Some("tok".into()),
            subscriptions: Default::default(),
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

    fn post_with_token(path: &str, body: &str, token: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn state_with_subscriptions(subs: Vec<(&str, Option<String>)>) -> AppState {
        let mut state = test_state().await;
        state.subscriptions = subs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        state
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
            vec![
                "anthropic/claude-sonnet-4",
                "openai/gpt-4o",
                "opencode-go/grok-4.6",
                "embeddings-local/nomic"
            ]
        );
        assert_eq!(v["data"][0]["object"], "model");
        assert_eq!(v["data"][3]["surface"], "embedding");
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
        let req: Value =
            json!({"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "hi"}]});
        let (status, body) = send(app, post("/v1/chat/completions", &req.to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("data: {\"ok\":true}\n\n"));
    }

    #[tokio::test]
    async fn subscription_gate_accepts_owner_and_rejects_others() {
        let state = state_with_subscriptions(vec![("openai", Some("alice-tok".into()))]).await;
        let app =
            openai_router_with_subs(Some("tok".into()), &["alice-tok".into()]).with_state(state);
        let req = json!({"model": "openai/gpt-4o", "messages": [{"role":"user","content":"hi"}]});

        // owner token -> relayed to mock upstream (200)
        let (status, body) = send(
            app.clone(),
            post_with_token("/v1/chat/completions", &req.to_string(), "alice-tok"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "owner token must pass: {body}");

        // global token but NOT the subscription token -> 401
        let (status, body) =
            send(app.clone(), post("/v1/chat/completions", &req.to_string())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("subscription"), "{body}");

        // no bearer token at all -> 401
        let (status, _) = send(
            app.clone(),
            Request::builder()
                .uri("/v1/chat/completions")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn subscription_gate_without_token_env_is_deny_all() {
        let state = state_with_subscriptions(vec![("openai", None)]).await;
        let app = openai_router_with_subs(Some("tok".into()), &[]).with_state(state);
        let req = json!({"model": "openai/gpt-4o", "messages": []});
        let (status, body) = send(
            app,
            post_with_token("/v1/chat/completions", &req.to_string(), "anything"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "misconfig must deny: {body}"
        );
    }

    #[tokio::test]
    async fn un_gated_upstream_ignores_subscription() {
        let state = test_state().await; // no subscriptions
        let app = openai_router(Some("tok".into())).with_state(state);
        let req = json!({"model": "openai/gpt-4o", "messages": [{"role":"user","content":"hi"}]});
        let (status, body) = send(app, post("/v1/chat/completions", &req.to_string())).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "no gate = global token suffices: {body}"
        );
    }

    #[tokio::test]
    async fn list_models_emits_surface_and_display_name() {
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
        assert!(
            body.contains("\"surface\":\"responses\""),
            "go grok model surface"
        );
        assert!(
            body.contains("\"surface\":\"chat\""),
            "openai model surface"
        );
        assert!(
            body.contains("\"surface\":\"messages\""),
            "anthropic model surface"
        );
        assert!(
            body.contains("\"display_name\":null"),
            "display_name present"
        );
    }

    #[tokio::test]
    async fn embeddings_relays_to_local_backend() {
        let app = openai_router(Some("tok".into())).with_state(test_state().await);
        let req: Value = json!({"model": "embeddings-local/nomic", "input": "hello"});
        let (status, body) = send(app, post("/v1/embeddings", &req.to_string())).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["data"][0]["embedding"], json!([0.1, 0.2]));
    }

    #[tokio::test]
    async fn embeddings_rejects_non_embedding_or_unknown_model() {
        let app = openai_router(Some("tok".into())).with_state(test_state().await);
        // chat-upstream model id on the embeddings route
        let req: Value = json!({"model": "openai/gpt-4o", "input": "hi"});
        let (status, body) = send(app.clone(), post("/v1/embeddings", &req.to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("embedding models"), "{body}");
        // unknown embedding id
        let req: Value = json!({"model": "embeddings-local/nope", "input": "hi"});
        let (status, body) = send(app.clone(), post("/v1/embeddings", &req.to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("nope"), "{body}");
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
