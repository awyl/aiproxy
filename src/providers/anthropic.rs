//! Anthropic gateway provider (kind `anthropic`). Serves the messages surface
//! only; `chat_completions`/`responses` are rejected.

use crate::config::UpstreamConfig;
use crate::provider::{
    Event, Model, ModelSurface, Provider, ProviderError, ProviderStream, RequestContext,
};
use crate::usage::{UsageData, UsageWindow};
use axum::http::header;
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};

fn extract_anthropic_limits(resp: &reqwest::Response) -> UsageData {
    let get = |name: &str| -> Option<u64> { resp.headers().get(name)?.to_str().ok()?.parse().ok() };
    let mut windows = Vec::new();
    if let (Some(limit), Some(remaining)) = (
        get("anthropic-ratelimit-requests-limit"),
        get("anthropic-ratelimit-requests-remaining"),
    ) {
        let used_percent = if limit > 0 {
            Some(((limit - remaining) as f64 / limit as f64) * 100.0)
        } else {
            None
        };
        // Anthropic reset is ISO timestamp — convert to seconds
        let reset_secs = resp
            .headers()
            .get("anthropic-ratelimit-requests-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(s).ok().map(|dt| {
                    let now = chrono::Utc::now();
                    dt.signed_duration_since(now).num_seconds().max(0) as u64
                })
            });
        windows.push(UsageWindow {
            label: "requests".into(),
            used_percent,
            reset_secs,
            window_minutes: None,
        });
    }
    if let (Some(limit), Some(remaining)) = (
        get("anthropic-ratelimit-tokens-limit"),
        get("anthropic-ratelimit-tokens-remaining"),
    ) {
        let used_percent = if limit > 0 {
            Some(((limit - remaining) as f64 / limit as f64) * 100.0)
        } else {
            None
        };
        let reset_secs = resp
            .headers()
            .get("anthropic-ratelimit-tokens-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(s).ok().map(|dt| {
                    let now = chrono::Utc::now();
                    dt.signed_duration_since(now).num_seconds().max(0) as u64
                })
            });
        windows.push(UsageWindow {
            label: "tokens".into(),
            used_percent,
            reset_secs,
            window_minutes: None,
        });
    }
    UsageData {
        windows,
        pools: vec![],
    }
}

const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    pub id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    client: Client,
}

impl AnthropicProvider {
    pub fn new(cfg: &UpstreamConfig, id: &str) -> Self {
        Self::new_with_key(cfg, id, cfg.api_key())
    }

    pub fn new_with_key(cfg: &UpstreamConfig, id: &str, api_key: Option<String>) -> Self {
        Self {
            id: id.to_string(),
            base_url: cfg.effective_base_url(),
            api_key,
            client: crate::providers::default_http_client(),
        }
    }

    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let b = builder.header("anthropic-version", ANTHROPIC_VERSION);
        match &self.api_key {
            Some(k) => b.header("x-api-key", k),
            None => b,
        }
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn surface_of(&self, _model: &str) -> ModelSurface {
        ModelSurface::Messages
    }

    async fn list_models(&self) -> Result<Vec<Model>, ProviderError> {
        let resp = self
            .authed(self.client.get(format!("{}/models", self.base_url)))
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body,
            });
        }
        Ok(body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|m| Model {
                        id: m["id"].as_str().unwrap_or_default().to_string(),
                        display_name: m["display_name"].as_str().map(String::from),
                        created_at: m["created_at"].as_str().map(String::from),
                        surface: ModelSurface::Messages,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn messages(
        &self,
        req: Bytes,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        let resp = self
            .authed(self.client.post(format!("{}/messages", self.base_url)))
            .headers(ctx.forwarded_headers())
            .header(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            )
            .body(req)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(format!("upstream {}: {e}", ctx.model)))?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or_else(
                |_| json!({"type": "error", "error": {"message": "upstream error"}}),
            );
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body,
            });
        }
        // Extract rate-limit headers before consuming the response
        if let Some(tracker) = &ctx.usage_tracker {
            let snapshot = extract_anthropic_limits(&resp);
            if !snapshot.windows.is_empty() {
                tracker.update(&self.id, snapshot).await;
            }
        }
        let stream = resp.bytes_stream().map(|chunk| match chunk {
            Ok(b) => Ok(Event(b)),
            Err(e) => Err(ProviderError::Transport(e.to_string())),
        });
        Ok(Box::new(stream))
    }

    async fn chat_completions(
        &self,
        _req: Bytes,
        _ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::Transport(
            "openai chat completions surface is not supported by anthropic-kind gateway".into(),
        ))
    }

    async fn responses(
        &self,
        _req: Bytes,
        _ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::Transport(
            "openai responses surface is not supported by anthropic-kind gateway".into(),
        ))
    }
}

// (TDD note: tests written first, then this impl.)

#[cfg(test)]
mod tests {
    use super::*;

    fn jb(v: serde_json::Value) -> Bytes {
        Bytes::from(v.to_string())
    }
    use crate::provider::RequestContext;
    use crate::providers::test_mock_upstream::{Capture, SharedCapture};
    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap, header};
    use axum::response::Response;
    use axum::routing::{get, post};
    use bytes::Bytes;
    use futures::StreamExt;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    const MODELS_ANTHROPIC: &str = r#"{"data":[{"type":"model","id":"claude-sonnet-4","display_name":"Claude Sonnet 4","created_at":"2026-05-01T00:00:00Z"}]}"#;
    const MESSAGES_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n";

    async fn spawn_mock(state: SharedCapture) -> String {
        let app = Router::new()
            .route("/v1/models", get(|| async { MODELS_ANTHROPIC }))
            .route("/v1/messages", post(relay_messages))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/v1")
    }

    async fn relay_messages(
        State(state): State<SharedCapture>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Response {
        let mut map: HashMap<String, String> = HashMap::new();
        for (k, v) in headers.iter() {
            map.insert(k.to_string(), v.to_str().unwrap_or("").to_string());
        }
        *state.headers.lock().unwrap() = map;
        *state.body.lock().unwrap() = Some(body);
        let mut resp = Response::new(Body::from(Bytes::from_static(MESSAGES_SSE.as_bytes())));
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/event-stream"),
        );
        resp
    }

    fn provider(base: &str) -> AnthropicProvider {
        let cfg = crate::config::UpstreamConfig {
            discover: false,
            token_env: None,
            surface: None,
            name: Some("anthropic".into()),
            kind: crate::config::UpstreamKind::Anthropic,
            base_url: Some(base.into()),
            api_key_env: None,
            models: vec![],
            endpoint_by_model: Default::default(),
            surface_map_url: None,
        };
        AnthropicProvider::new_with_key(&cfg, "anthropic", Some("sk-ant-test".into()))
    }

    #[tokio::test]
    async fn list_models_parses_anthropic_shape() {
        let state = Arc::new(Capture::default());
        let base = spawn_mock(state).await;
        let p = provider(&base);
        let models = p.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-sonnet-4");
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Sonnet 4"));
        assert_eq!(
            models[0].created_at.as_deref(),
            Some("2026-05-01T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn messages_sends_x_api_key_and_version_header() {
        let state = Arc::new(Capture::default());
        let base = spawn_mock(state.clone()).await;
        let p = provider(&base);
        let req =
            json!({"model": "claude-sonnet-4", "messages": [{"role": "user", "content": "hi"}]});
        let mut stream = p
            .messages(
                jb(req),
                &RequestContext {
                    model: "claude-sonnet-4".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut out = String::new();
        while let Some(ev) = stream.next().await {
            out.push_str(&String::from_utf8_lossy(&ev.unwrap().0));
        }
        assert!(out.contains("message_start"));
        assert!(out.contains("content_block_delta"));
        let headers = state.headers.lock().unwrap();
        assert_eq!(
            headers.get("x-api-key").map(String::as_str),
            Some("sk-ant-test")
        );
        assert_eq!(
            headers.get("anthropic-version").map(String::as_str),
            Some("2023-06-01")
        );
        let body: Value =
            serde_json::from_slice(&state.body.lock().unwrap().clone().unwrap()).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4");
    }

    #[tokio::test]
    async fn surface_is_messages_only() {
        let state = Arc::new(Capture::default());
        let base = spawn_mock(state).await;
        let p = provider(&base);
        assert_eq!(p.surface_of("claude-sonnet-4"), ModelSurface::Messages);
        let err = p
            .chat_completions(
                jb(json!({"model": "x"})),
                &RequestContext {
                    model: "x".into(),
                    ..Default::default()
                },
            )
            .await
            .err()
            .expect("expected unsupported-surface error");
        assert!(matches!(err, ProviderError::Transport(_)));
        let err2 = p
            .responses(
                jb(json!({"model": "x"})),
                &RequestContext {
                    model: "x".into(),
                    ..Default::default()
                },
            )
            .await
            .err()
            .expect("expected unsupported-surface error");
        assert!(matches!(err2, ProviderError::Transport(_)));
    }
}
