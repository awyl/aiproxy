//! OpenCode Go provider (kind `opencode-go`): per-model surface routing with
//! auto-discovery — ids from Go's public `/v1/models`, surfaces parsed at
//! runtime from `surface_map_url` (default opencode docs), config overrides
//! on top, builtin snapshot fallback.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use crate::config::UpstreamConfig;
use crate::provider::{Event, Model, ModelSurface, Provider, ProviderError, ProviderStream, RequestContext};

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Fallback snapshot of opencode.ai/docs/go endpoints table (2026-08-30).
/// The runtime surface map (parsed from `surface_map_url`) and config
/// `endpoint_by_model` overrides take precedence over this.
fn builtin_surface(model: &str) -> ModelSurface {
    use ModelSurface::*;
    match model {
        "grok-4.6" | "gpt-5.6-luna" | "muse-spark-1.2-contributor" => Responses,
        "minimax-m3" | "minimax-m2.7" | "minimax-m2.5"
        | "qwen3.8-max" | "qwen3.8-flash" | "qwen3.7-max"
        | "qwen3.7-plus" | "qwen3.6-plus" => Messages,
        _ => ChatCompletions,
    }
}

/// Parse the opencode docs endpoints table. Each row is `<tr><td>cells</td>...</tr>`;
/// cell 1 = display name, cell 2 = model id, cell 3 = endpoint URL. The endpoint's
/// path suffix decides the surface. Rows without a recognizable endpoint are skipped.
pub(crate) fn parse_surface_table(html: &str) -> HashMap<String, ModelSurface> {
    use ModelSurface::*;
    let mut out = HashMap::new();
    let row_re = regex::Regex::new(r"<tr[^>]*>(.*?)</tr>").expect("static regex");
    let cell_re = regex::Regex::new(r"<t[dh][^>]*>(.*?)</t[dh]>").expect("static regex");
    for row in row_re.captures_iter(html) {
        let cells: Vec<String> = cell_re
            .captures_iter(&row[1])
            .map(|c| strip_tags(&c[1]).trim().to_string())
            .collect();
        if cells.len() < 3 {
            continue;
        }
        let model = cells[1].clone();
        let endpoint = cells[2].to_lowercase();
        let surface = if endpoint.contains("/chat/completions") {
            ChatCompletions
        } else if endpoint.contains("/messages") {
            Messages
        } else if endpoint.contains("/responses") {
            Responses
        } else {
            continue; // unknown endpoint row
        };
        if !model.is_empty() {
            out.insert(model, surface);
        }
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

#[derive(Debug, Default)]
pub struct SurfaceCache {
    pub map: HashMap<String, ModelSurface>,
}

#[derive(Debug, Clone)]
pub struct OpencodeGoProvider {
    id: String,
    base_url: String,
    api_key: Option<String>,
    overrides: HashMap<String, ModelSurface>,
    surfaces_url: String,
    surfaces: Arc<RwLock<SurfaceCache>>,
    client: Client,
}

impl OpencodeGoProvider {
    pub fn new(cfg: &UpstreamConfig) -> Self {
        Self::new_with_key(cfg, cfg.api_key())
    }

    pub fn new_with_key(cfg: &UpstreamConfig, api_key: Option<String>) -> Self {
        let overrides = cfg
            .endpoint_by_model
            .iter()
            .map(|(m, s)| {
                let surface = match s.as_str() {
                    "messages" => ModelSurface::Messages,
                    "responses" => ModelSurface::Responses,
                    _ => ModelSurface::ChatCompletions,
                };
                (m.clone(), surface)
            })
            .collect();
        Self {
            id: cfg.name.clone(),
            base_url: cfg.effective_base_url(),
            api_key,
            overrides,
            surfaces_url: cfg.surface_map_url_or_default(),
            surfaces: Arc::new(RwLock::new(SurfaceCache::default())),
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client build"),
        }
    }

    pub fn surface(&self, model: &str) -> ModelSurface {
        if let Some(s) = self.overrides.get(model) {
            return *s;
        }
        if let Some(s) = self.surfaces.read().unwrap().map.get(model) {
            return *s;
        }
        builtin_surface(model)
    }

    /// Fetch and parse the surface table from `surfaces_url` (10s timeout).
    /// Any failure keeps the last-known map and logs a warning.
    pub async fn refresh_surfaces(&self) {
        let fetched = tokio::time::timeout(
            Duration::from_secs(10),
            self.client.get(&self.surfaces_url).send(),
        )
        .await;
        let resp = match fetched {
            Ok(Ok(r)) if r.status().is_success() => r,
            Ok(Ok(_)) => {
                tracing::warn!(url = %self.surfaces_url, "surface map fetch returned non-success");
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!(url = %self.surfaces_url, "surface map fetch failed: {e}");
                return;
            }
            Err(_) => {
                tracing::warn!(url = %self.surfaces_url, "surface map fetch timed out");
                return;
            }
        };
        let html = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(url = %self.surfaces_url, "surface map body read failed: {e}");
                return;
            }
        };
        let map = parse_surface_table(&html);
        if map.is_empty() {
            tracing::warn!(url = %self.surfaces_url, "surface map parse produced no rows; keeping previous");
            return;
        }
        self.surfaces.write().unwrap().map = map;
    }

    fn require_surface(&self, model: &str, want: ModelSurface) -> Result<(), ProviderError> {
        let got = self.surface(model);
        if got == want {
            Ok(())
        } else {
            Err(ProviderError::Transport(format!(
                "model '{model}' is served on this upstream via the {got:?} surface; use the matching route"
            )))
        }
    }

    async fn relay(
        &self,
        path: &str,
        req: Value,
        key_header: Option<&str>,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        let url = format!("{}{path}", self.base_url);
        let mut builder = self.client.post(&url).json(&req);
        if let Some(k) = &self.api_key {
            builder = match key_header {
                Some(h) => builder.header(h, k),
                None => builder.bearer_auth(k),
            };
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| ProviderError::Transport(format!("opencode-go {}: {e}", ctx.model)))?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp
                .json()
                .await
                .unwrap_or_else(|_| json!({"error": {"message": "opencode-go upstream error"}}));
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let stream = resp.bytes_stream().map(|chunk| match chunk {
            Ok(b) => Ok(Event(b)),
            Err(e) => Err(ProviderError::Transport(e.to_string())),
        });
        Ok(Box::new(stream))
    }
}

#[async_trait::async_trait]
impl Provider for OpencodeGoProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn surface_of(&self, model: &str) -> ModelSurface {
        self.surface(model)
    }

    async fn list_models(&self) -> Result<Vec<Model>, ProviderError> {
        // Surfaces ride the discovery pass: no independent TTL. Any failure
        // logs + keeps the last-known map (refresh_surfaces is best-effort).
        self.refresh_surfaces().await;
        let resp = self
            .client
            .get(format!("{}/models", self.base_url))
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
                    .map(|m| {
                        let id = m["id"].as_str().unwrap_or_default().to_string();
                        Model {
                            surface: self.surface(&id),
                            id,
                            display_name: None,
                            created_at: m["created"].as_u64().map(|c| c.to_string()),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn chat_completions(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        self.require_surface(&ctx.model, ModelSurface::ChatCompletions)?;
        self.relay("/chat/completions", req, None, ctx).await
    }

    async fn messages(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        self.require_surface(&ctx.model, ModelSurface::Messages)?;
        let url = format!("{}/messages", self.base_url);
        let mut builder = self
            .client
            .post(&url)
            .json(&req)
            .header("anthropic-version", ANTHROPIC_VERSION);
        if let Some(k) = &self.api_key {
            builder = builder.header("x-api-key", k);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| ProviderError::Transport(format!("opencode-go {}: {e}", ctx.model)))?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp
                .json()
                .await
                .unwrap_or_else(|_| json!({"type": "error", "error": {"message": "opencode-go upstream error"}}));
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let stream = resp.bytes_stream().map(|chunk| match chunk {
            Ok(b) => Ok(Event(b)),
            Err(e) => Err(ProviderError::Transport(e.to_string())),
        });
        Ok(Box::new(stream))
    }

    async fn responses(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        self.require_surface(&ctx.model, ModelSurface::Responses)?;
        self.relay("/responses", req, None, ctx).await
    }
}

// (TDD: tests written first.)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_mock_upstream::{Capture, SharedCapture};
    use crate::provider::RequestContext;
    use axum::body::Body;
use bytes::Bytes;
    use axum::extract::State;
    use axum::http::{header, HeaderMap};
    use axum::response::Response;
    use axum::routing::{get, post};
    use axum::Router;
    use futures::StreamExt;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    // Go-shaped mock: same base, different path per surface, headers captured.
    const GO_MODELS: &str = r#"{"object":"list","data":[{"id":"kimi-k3","object":"model","created":1788054263,"owned_by":"opencode"},{"id":"minimax-m3","object":"model","created":1788054263,"owned_by":"opencode"},{"id":"grok-4.6","object":"model","created":1788054263,"owned_by":"opencode"}]}"#;

    // Snippet of the opencode docs endpoints table (same shape as opencode.ai/docs/go).
    const DOCS_TABLE: &str = "<table><thead><tr><th>Model</th><th>Model ID</th><th>Endpoint</th><th>AI SDK Package</th></tr></thead><tbody>
<tr><td>Grok 4.6</td><td>grok-4.6</td><td>https://opencode.ai/zen/go/v1/responses</td><td>@ai-sdk/openai</td></tr>
<tr><td>GLM-5.3-Flash</td><td>glm-5.3-flash</td><td>https://opencode.ai/zen/go/v1/chat/completions</td><td>@ai-sdk/openai-compatible</td></tr>
<tr><td>MiniMax M3</td><td>minimax-m3</td><td>https://opencode.ai/zen/go/v1/messages</td><td>@ai-sdk/anthropic</td></tr>
<tr><td>Broken row</td><td>bogus-model</td><td>https://example.com/weird</td><td>@ai-sdk/none</td></tr>
</tbody></table>";

    async fn spawn_go_mock(state: SharedCapture, docs_html: Option<&str>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let docs_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut app = Router::new()
            .route("/v1/models", get(|| async { GO_MODELS }))
            .route("/v1/chat/completions", post(go_relay))
            .route("/v1/messages", post(go_relay))
            .route("/v1/responses", post(go_relay))
            .with_state(state.clone());
        if let Some(html) = docs_html {
            let html = html.to_string();
            let counter = docs_hits.clone();
            app = app.route(
                "/docs/go",
                get(move || {
                    let h = html.clone();
                    let c = counter.clone();
                    async move {
                        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        h
                    }
                }),
            );
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/v1"), docs_hits)
    }

    async fn go_relay(
        State(state): State<SharedCapture>,
        req: axum::http::Request<Body>,
    ) -> Response {
        let path = req.uri().path().to_string();
        let headers: HeaderMap = req.headers().clone();
        let mut map: HashMap<String, String> = HashMap::new();
        for (k, v) in headers.iter() {
            map.insert(k.to_string(), v.to_str().unwrap_or("").to_string());
        }
        map.insert("__path".to_string(), path);
        *state.headers.lock().unwrap() = map;
        let mut resp = Response::new(Body::from(Bytes::from_static(b"data: {\"ok\":true}\n\n")));
        resp.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
        resp
    }

    fn provider(base: &str, docs_url: Option<&str>, overrides: &[(&str, &str)]) -> OpencodeGoProvider {
        let cfg = crate::config::UpstreamConfig {
            discover: false,
            token_env: None,
            surface: None,
            name: "opencode-go".into(),
            kind: crate::config::UpstreamKind::OpencodeGo,
            base_url: Some(base.into()),
            api_key_env: None,
            models: vec![],
            endpoint_by_model: overrides.iter().map(|(m, s)| (m.to_string(), s.to_string())).collect(),
            surface_map_url: docs_url.map(String::from),
        };
        OpencodeGoProvider::new_with_key(&cfg, Some("go-sk-test".into()))
    }

    #[test]
    fn parse_surface_table_extracts_rows() {
        let map = parse_surface_table(DOCS_TABLE);
        assert_eq!(map.get("grok-4.6"), Some(&ModelSurface::Responses));
        assert_eq!(map.get("glm-5.3-flash"), Some(&ModelSurface::ChatCompletions));
        assert_eq!(map.get("minimax-m3"), Some(&ModelSurface::Messages));
        assert!(!map.contains_key("bogus-model"), "unknown endpoint rows skipped");
        assert!(parse_surface_table("<html>no table</html>").is_empty());
        assert!(parse_surface_table("").is_empty());
    }

    #[tokio::test]
    async fn runtime_surface_map_extends_builtin() {
        let state = Arc::new(Capture::default());
        let (base, _) = spawn_go_mock(state, Some(DOCS_TABLE)).await;
        let root = base.rsplit_once("/v1").unwrap().0;
        let p = provider(&base, Some(&format!("{root}/docs/go")), &[]);
        p.refresh_surfaces().await;
        assert_eq!(p.surface_of("grok-4.6"), ModelSurface::Responses);
        assert_eq!(p.surface_of("minimax-m3"), ModelSurface::Messages);
        // a model ONLY in the runtime table (not hardcoded) resolves too
        assert_eq!(p.surface_of("glm-5.3-flash"), ModelSurface::ChatCompletions);
    }

    #[tokio::test]
    async fn surface_fetch_failure_falls_back_to_builtin() {
        let state = Arc::new(Capture::default());
        let (base, _) = spawn_go_mock(state, None).await; // no /docs/go route -> 404
        let root = base.rsplit_once("/v1").unwrap().0;
        let p = provider(&base, Some(&format!("{root}/docs/go")), &[]);
        p.refresh_surfaces().await; // fails silently
        assert_eq!(p.surface_of("grok-4.6"), ModelSurface::Responses); // builtin
        assert_eq!(p.surface_of("minimax-m3"), ModelSurface::Messages);
    }

    #[tokio::test]
    async fn surface_table_refreshed_on_every_discovery_pass() {
        let state = Arc::new(Capture::default());
        let (base, docs_hits) = spawn_go_mock(state, Some(DOCS_TABLE)).await;
        let root = base.rsplit_once("/v1").unwrap().0;
        let p = provider(&base, Some(&format!("{root}/docs/go")), &[]);
        p.list_models().await.unwrap(); // pass 1: models + surfaces
        p.list_models().await.unwrap(); // pass 2: models + surfaces again
        assert_eq!(
            docs_hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "surface table must be re-fetched on every discovery pass (no independent TTL)"
        );
    }

    #[test]
    fn config_override_beats_runtime_and_builtin() {
        let cfg = crate::config::UpstreamConfig {
            discover: false,
            token_env: None,
            surface: None,
            name: "opencode-go".into(),
            kind: crate::config::UpstreamKind::OpencodeGo,
            base_url: Some("http://x/v1".into()),
            api_key_env: None,
            models: vec![],
            endpoint_by_model: vec![("grok-4.6".into(), "chat".into())].into_iter().collect(),
            surface_map_url: None,
        };
        let p = OpencodeGoProvider::new_with_key(&cfg, None);
        assert_eq!(p.surface_of("grok-4.6"), ModelSurface::ChatCompletions); // override wins
        assert_eq!(p.surface_of("minimax-m3"), ModelSurface::Messages); // builtin still works
        assert_eq!(p.surface_of("unknown-xyz"), ModelSurface::ChatCompletions); // default
    }

    #[tokio::test]
    async fn discovery_tags_models_with_surface() {
        let (base, _) = spawn_go_mock(Arc::new(Capture::default()), None).await;
        let p = provider(&base, None, &[]);
        let models = p.list_models().await.unwrap();
        let by_id: HashMap<_, _> = models.iter().map(|m| (m.id.as_str(), m.surface)).collect();
        assert_eq!(by_id.get("kimi-k3"), Some(&ModelSurface::ChatCompletions));
        assert_eq!(by_id.get("minimax-m3"), Some(&ModelSurface::Messages));
        assert_eq!(by_id.get("grok-4.6"), Some(&ModelSurface::Responses));
    }

    #[tokio::test]
    async fn surfaces_forward_to_correct_path_and_headers() {
        let state = Arc::new(Capture::default());
        let (base, _) = spawn_go_mock(state.clone(), None).await;
        let p = provider(&base, None, &[]);

        let mut s1 = p
            .chat_completions(json!({"model": "kimi-k3"}), &RequestContext { model: "kimi-k3".into() })
            .await
            .unwrap();
        while s1.next().await.is_some() {}
        {
            let h = state.headers.lock().unwrap();
            assert_eq!(h.get("__path").map(String::as_str), Some("/v1/chat/completions"));
            assert_eq!(h.get("authorization").map(String::as_str), Some("Bearer go-sk-test"));
        }

        let mut s2 = p
            .messages(json!({"model": "minimax-m3"}), &RequestContext { model: "minimax-m3".into() })
            .await
            .unwrap();
        while s2.next().await.is_some() {}
        {
            let h = state.headers.lock().unwrap();
            assert_eq!(h.get("__path").map(String::as_str), Some("/v1/messages"));
            assert_eq!(h.get("x-api-key").map(String::as_str), Some("go-sk-test"));
            assert_eq!(h.get("anthropic-version").map(String::as_str), Some("2023-06-01"));
        }

        let mut s3 = p
            .responses(json!({"model": "grok-4.6"}), &RequestContext { model: "grok-4.6".into() })
            .await
            .unwrap();
        while s3.next().await.is_some() {}
        {
            let h = state.headers.lock().unwrap();
            assert_eq!(h.get("__path").map(String::as_str), Some("/v1/responses"));
        }
    }

    #[tokio::test]
    async fn wrong_surface_call_is_rejected() {
        let state = Arc::new(Capture::default());
        let (base, _) = spawn_go_mock(state, None).await;
        let p = provider(&base, None, &[]);
        let err = p
            .chat_completions(json!({"model": "grok-4.6"}), &RequestContext { model: "grok-4.6".into() })
            .await
            .err()
            .expect("expected ding");
        let msg = match &err {
            ProviderError::Transport(m) => m.clone(),
            other => panic!("expected Transport, got {other:?}"),
        };
        assert!(msg.to_lowercase().contains("responses"), "message should hint the correct surface: {msg}");
        let err2 = p
            .messages(json!({"model": "kimi-k3"}), &RequestContext { model: "kimi-k3".into() })
            .await
            .err()
            .expect("expected ding on messages");
        assert!(matches!(err2, ProviderError::Transport(_)));
    }
}