//! Shared API-layer helpers: AppState, SSE relay (raw byte passthrough),
//! per-surface error shapes, and `relay_or_error`.
//!
//! (TDD: tests first.)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Event, ProviderError};
    use bytes::Bytes;
    use futures::stream;
    use serde_json::json;

    async fn body_of(resp: Response) -> String {
        let bytes = bytes_of(resp).await;
        String::from_utf8_lossy(&bytes).to_string()
    }

    async fn bytes_of(resp: Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap_or_default()
            .to_vec()
    }

    #[tokio::test]
    async fn openai_error_shape() {
        let resp = openai_error(
            StatusCode::BAD_REQUEST,
            "bad model",
            "invalid_request_error",
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = serde_json::from_str(&body_of(resp).await).unwrap();
        assert_eq!(body["error"]["message"], "bad model");
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn anthropic_error_shape() {
        let resp = anthropic_error(
            StatusCode::TOO_MANY_REQUESTS,
            "slow down",
            "rate_limit_error",
        );
        let body: serde_json::Value = serde_json::from_str(&body_of(resp).await).unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["message"], "slow down");
    }

    #[tokio::test]
    async fn sse_relay_preserves_raw_bytes_across_chunk_boundaries() {
        // upstream emits mid-line chunk splits; client must receive exact bytes
        let stream = stream::iter(vec![
            Ok::<_, ProviderError>(Event(Bytes::from_static(b"data: {\"a\":"))),
            Ok(Event(Bytes::from_static(
                b"1,\"b\":[2,3]}\n\ndata: {\"c\":4}\n\n",
            ))),
        ]);
        let resp = sse_relay(Box::new(stream));
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let body = bytes_of(resp).await;
        assert_eq!(
            body,
            b"data: {\"a\":1,\"b\":[2,3]}\n\ndata: {\"c\":4}\n\n".to_vec()
        );
    }

    #[tokio::test]
    async fn relay_or_error_translates_transport_failure() {
        let err: Result<ProviderStream, ProviderError> =
            Err(ProviderError::Transport("connect timed out".into()));
        let resp = relay_or_error(err, Surface::Openai);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body: serde_json::Value = serde_json::from_str(&body_of(resp).await).unwrap();
        assert_eq!(body["error"]["type"], "upstream_error");
    }

    #[tokio::test]
    async fn relay_or_error_preserves_upstream_http_body() {
        let err: Result<ProviderStream, ProviderError> = Err(ProviderError::Http {
            status: 429,
            body: json!({"error": {"message": "rate limited by upstream", "type": "rate_limit_error"}}),
        });
        let resp = relay_or_error(err, Surface::Openai);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: serde_json::Value = serde_json::from_str(&body_of(resp).await).unwrap();
        assert_eq!(body["error"]["message"], "rate limited by upstream");
    }
}

use crate::discovery::ModelRegistry;
use crate::embeddings::EmbeddingManager;
use crate::provider::{ModelSurface, Provider, ProviderError, ProviderStream};
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::json;
use std::sync::Arc;

pub mod anthropic;
pub mod body;
pub mod openai;

/// Gate a route on the model's wire surface. Returns a 400 naming the
/// model's actual surface on mismatch (Unknown -> cannot stream).
#[allow(clippy::result_large_err)]
pub fn check_surface(
    provider: &dyn Provider,
    model: &str,
    required: ModelSurface,
    surface: Surface,
) -> Result<(), Response> {
    let got = provider.surface_of(model);
    if got == required {
        return Ok(());
    }
    let msg = match got {
        ModelSurface::Unknown => format!(
            "model '{model}' on upstream '{}' has no known wire surface (static catalog entry); it cannot be streamed",
            provider.id()
        ),
        other => format!(
            "model '{model}' on upstream '{}' is served via the {other:?} surface; use the matching route",
            provider.id()
        ),
    };
    Err(match surface {
        Surface::Openai => openai_error(StatusCode::BAD_REQUEST, msg, "invalid_request_error"),
        Surface::Anthropic => {
            anthropic_error(StatusCode::BAD_REQUEST, msg, "invalid_request_error")
        }
    })
}

/// Gate a route on a per-upstream subscription token. Returns `Ok` when the
/// upstream has no subscription gate, `Err` when the request token does not
/// match the upstream's subscription token (or the gate is misconfigured).
pub fn check_subscription(state: &AppState, prefix: &str, token: Option<&str>) -> Result<(), &'static str> {
    let Some(gate) = state.subscriptions.get(prefix) else {
        return Ok(()); // no gate for this upstream
    };
    match gate {
        None => Err("token_env set but env missing"), // deny all
        Some(sub) => {
            if token.is_some_and(|t| crate::auth::constant_time_eq(t, sub)) {
                Ok(())
            } else {
                Err("token mismatch")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub registry: Arc<ModelRegistry>,
    pub embeddings: Arc<EmbeddingManager>,
    pub token: Option<String>,
    /// Upstream prefix -> subscription token (from `token_env`).
    /// Missing entry: no gate. `Some(None)`: deny-all (misconfig).
    pub subscriptions: std::collections::HashMap<String, Option<String>>,
}

/// Which agent-facing error schema to speak when translating failures.
#[derive(Debug, Clone, Copy)]
pub enum Surface {
    Openai,
    Anthropic,
}

/// Raw SSE byte relay: the provider stream's chunks become the response body
/// verbatim. No re-framing, no buffering beyond the transport chunk.
pub fn sse_relay(stream: ProviderStream) -> Response {
    let body = Body::from_stream(stream.map(|item| item.map(|e| e.0).map_err(stream_err)));
    // stream error -> std::io::Error to satisfy Body::from_stream's error bound
    fn stream_err(e: ProviderError) -> std::io::Error {
        std::io::Error::other(format!("{e:?}"))
    }
    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    resp
}

pub fn openai_error(status: StatusCode, message: impl Into<String>, code: &str) -> Response {
    let body = json!({"error": {"message": message.into(), "type": code}});
    (status, axum::Json(body)).into_response()
}

pub fn anthropic_error(status: StatusCode, message: impl Into<String>, kind: &str) -> Response {
    let body = json!({"type": "error", "error": {"type": kind, "message": message.into()}});
    (status, axum::Json(body)).into_response()
}

/// Turn a provider call result into an HTTP response: stream on success,
/// translate upstream Http/Transport failures to the surface's error shape.
pub fn relay_or_error(
    provider_result: Result<ProviderStream, ProviderError>,
    surface: Surface,
) -> Response {
    match provider_result {
        Ok(stream) => sse_relay(stream),
        Err(ProviderError::Http { status, body }) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            (status, axum::Json(body)).into_response()
        }
        Err(ProviderError::Transport(msg)) => match surface {
            Surface::Openai => openai_error(StatusCode::BAD_GATEWAY, msg, "upstream_error"),
            Surface::Anthropic => anthropic_error(StatusCode::BAD_GATEWAY, msg, "api_error"),
        },
    }
}
