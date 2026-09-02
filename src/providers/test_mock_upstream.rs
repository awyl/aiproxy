//! Test mock upstreams: tiny axum servers speaking the wire shapes the
//! providers call. Shared by providers tests.
#![cfg(test)]

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
pub struct Capture {
    pub headers: Mutex<HashMap<String, String>>,
    pub body: Mutex<Option<bytes::Bytes>>,
}

pub type SharedCapture = Arc<Capture>;

pub const MODELS_JSON: &str = r#"{"object":"list","data":[{"id":"gpt-4o","object":"model","created":1720000000,"owned_by":"openai"}]}"#;

/// Convert a serde_json::Value to Bytes for test convenience (preserves json! macro usage).
pub fn jb(v: serde_json::Value) -> bytes::Bytes {
    bytes::Bytes::from(v.to_string())
}

fn capture_headers(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect()
}

fn sse(body: &'static [u8]) -> Response {
    let mut resp = Response::new(Body::from(Bytes::from_static(body)));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    resp
}

async fn relay_chat(
    State(state): State<SharedCapture>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    *state.headers.lock().unwrap() = capture_headers(&headers);
    *state.body.lock().unwrap() = Some(body);
    // SSE stream with a mid-content split to exercise relay integrity
    sse(b"data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: [DONE]\n\n")
}

/// OpenAI-compatible mock. `base` is served at the socket root: /v1/models,
/// /v1/chat/completions (happy SSE), /chat/completions (500 JSON error, for
/// a provider whose base is the root so error mapping is exercised).
pub fn mock_openai_server(state: SharedCapture) -> Router {
    Router::new()
        .route("/v1/models", get(|| async { MODELS_JSON }))
        .route("/v1/chat/completions", post(relay_chat))
        .route(
            "/chat/completions",
            post(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": {"message": "upstream boom", "type": "server_error"}})),
                )
            }),
        )
        .with_state(state)
}
