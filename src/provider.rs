//! Provider trait and shared types — the core interface every upstream plugs into.
//!
//! A provider knows how to talk to one upstream (OpenAI-compatible gateway,
//! Anthropic gateway, or OpenCode Go with per-model surface routing). The API
//! layer only ever sees `&dyn Provider`. v1 relays upstream bytes verbatim
//! (`Event`), so adding a semantic layer later does not change the trait shape.

use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The three agent-facing wire formats a model can be served on, plus Unknown
/// for static catalog entries that only appear in `/v1/models`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSurface {
    ChatCompletions,
    Messages,
    Responses,
    /// Local embeddings (fake `embeddings-local` provider) — display-only
    /// surface; chat routers never stream these.
    Embedding,
    #[default]
    Unknown,
}

impl ModelSurface {
    /// Wire name used in `/v1/models`: chat | messages | responses | unknown.
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSurface::ChatCompletions => "chat",
            ModelSurface::Messages => "messages",
            ModelSurface::Responses => "responses",
            ModelSurface::Embedding => "embedding",
            ModelSurface::Unknown => "unknown",
        }
    }
}

/// One catalog entry. `id` gets prefixed with the upstream name when exposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default)]
    pub surface: ModelSurface,
}

/// One opaque chunk of upstream response body. v1 keeps bytes pristine
/// (SSE passthrough); the wrapper exists so a semantic layer can be added
/// later without changing the trait shape.
#[derive(Debug, Clone)]
pub struct Event(pub Bytes);

/// Per-request context passed to providers. `model` is the id as sent upstream
/// (the `{prefix}/` has already been stripped by the API layer).
/// `client_headers` are the client's request headers minus hop-by-hop fields;
/// gateways forward them verbatim and override only the headers they own
/// (auth, content-type, anthropic-version, OAuth signing).
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub model: String,
    pub client_headers: axum::http::HeaderMap,
}

/// Headers aiproxy owns (or that are unsafe/unmanaged to forward). Stripped
/// from the client's forwarded set so the gateway's own values are the only
/// ones sent — reqwest `.header()` appends, never replaces.
const NON_FORWARDED_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "content-type",
    "content-length",
    "anthropic-version",
    "host",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "upgrade",
    "expect",
    "accept-encoding",
];

impl RequestContext {
    /// Client headers to forward upstream: everything the client sent minus
    /// aiproxy-owned, hop-by-hop, and transport-managed fields.
    pub fn forwarded_headers(&self) -> axum::http::HeaderMap {
        let mut out = self.client_headers.clone();
        for name in NON_FORWARDED_HEADERS {
            out.remove(*name);
        }
        out
    }
}

#[derive(Debug)]
pub enum ProviderError {
    Http { status: u16, body: Value },
    Transport(String),
}

impl ProviderError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ProviderError::Http { status, .. } => Some(*status),
            ProviderError::Transport(_) => None,
        }
    }
}

pub type ProviderStream = Box<dyn Stream<Item = Result<Event, ProviderError>> + Send + Unpin>;

#[async_trait::async_trait]
pub trait Provider: Send + Sync + 'static {
    /// Stable id used in config (`name:` field) and in model IDs ("opencode-go/").
    fn id(&self) -> &str;

    /// Which wire surface this provider serves a given model id on. Lets the
    /// API layer reject mismatched routes before forwarding (400 with a hint).
    fn surface_of(&self, model: &str) -> ModelSurface;

    /// Auto-discovery: fetch this upstream's model list.
    async fn list_models(&self) -> Result<Vec<Model>, ProviderError>;

    /// OpenAI chat-completions surface. The raw request body is relayed
    /// byte-for-byte (only the top-level model id is prefix-stripped) so
    /// upstream-side request caching sees the client's exact serialization.
    async fn chat_completions(
        &self,
        req: Bytes,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError>;

    /// Anthropic messages surface (raw byte passthrough, see above).
    async fn messages(
        &self,
        req: Bytes,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError>;

    /// OpenAI Responses surface (raw byte passthrough, see above).
    async fn responses(
        &self,
        req: Bytes,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError>;
}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use futures::stream;

    pub struct MockProvider {
        id: String,
        models: Vec<String>,
        surface: ModelSurface,
        fail: bool,
    }

    impl MockProvider {
        pub fn new(id: &str, models: Vec<String>) -> Self {
            Self {
                id: id.to_string(),
                models,
                surface: ModelSurface::Unknown,
                fail: false,
            }
        }
        pub fn with_surface(id: &str, models: Vec<String>, surface: ModelSurface) -> Self {
            Self {
                id: id.to_string(),
                models,
                surface,
                fail: false,
            }
        }
        pub fn failing(id: &str) -> Self {
            Self {
                id: id.to_string(),
                models: vec![],
                surface: ModelSurface::Unknown,
                fail: true,
            }
        }
    }

    fn ok_stream() -> ProviderStream {
        Box::new(stream::iter(vec![Ok::<Event, ProviderError>(Event(
            Bytes::from_static(b"data: {\"ok\":true}\n\n"),
        ))]))
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn surface_of(&self, _model: &str) -> ModelSurface {
            self.surface
        }
        async fn list_models(&self) -> Result<Vec<Model>, ProviderError> {
            if self.fail {
                return Err(ProviderError::Transport("mock failure".into()));
            }
            Ok(self
                .models
                .iter()
                .map(|m| Model {
                    id: m.clone(),
                    display_name: None,
                    created_at: None,
                    surface: self.surface,
                })
                .collect())
        }
        async fn chat_completions(
            &self,
            _req: Bytes,
            _ctx: &RequestContext,
        ) -> Result<ProviderStream, ProviderError> {
            Ok(ok_stream())
        }
        async fn messages(
            &self,
            _req: Bytes,
            _ctx: &RequestContext,
        ) -> Result<ProviderStream, ProviderError> {
            Ok(ok_stream())
        }
        async fn responses(
            &self,
            _req: Bytes,
            _ctx: &RequestContext,
        ) -> Result<ProviderStream, ProviderError> {
            Ok(ok_stream())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::testutil::MockProvider;
    use futures::StreamExt;

    fn jb(v: serde_json::Value) -> Bytes {
        Bytes::from(v.to_string())
    }

    #[test]
    fn model_round_trips_through_json() {
        let m = Model {
            id: "gpt-4o".into(),
            display_name: Some("GPT-4o".into()),
            created_at: Some("1720000000".into()),
            surface: ModelSurface::ChatCompletions,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: Model = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "gpt-4o");
        assert_eq!(back.display_name.as_deref(), Some("GPT-4o"));
        assert_eq!(back.surface, ModelSurface::ChatCompletions);
        // missing surface field in input -> Unknown
        let back2: Model = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert_eq!(back2.surface, ModelSurface::Unknown);
    }

    #[test]
    fn surface_serde_values() {
        assert_eq!(
            serde_json::from_str::<ModelSurface>("\"messages\"").unwrap(),
            ModelSurface::Messages
        );
        assert!(serde_json::from_str::<ModelSurface>("\"bogus\"").is_err());
    }

    #[test]
    fn mock_provider_surface_defaults_and_overrides() {
        let p = MockProvider::new("m", vec![]);
        assert_eq!(p.surface_of("anything"), ModelSurface::Unknown);
        let p2 = MockProvider::with_surface("m", vec![], ModelSurface::Messages);
        assert_eq!(p2.surface_of("anything"), ModelSurface::Messages);
    }

    #[test]
    fn provider_error_status_mapping() {
        let e = ProviderError::Http {
            status: 502,
            body: serde_json::json!({"x": 1}),
        };
        assert_eq!(e.status(), Some(502));
        let t = ProviderError::Transport("boom".into());
        assert_eq!(t.status(), None);
    }

    #[tokio::test]
    async fn trait_object_stream_flows() {
        let p: Box<dyn Provider> = Box::new(MockProvider::with_surface(
            "mock",
            vec!["m1".into()],
            ModelSurface::Messages,
        ));
        assert_eq!(p.id(), "mock");
        assert_eq!(p.surface_of("m1"), ModelSurface::Messages);
        let models = p.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].surface, ModelSurface::Messages);
        let ctx = RequestContext {
            model: "m1".into(),
            ..Default::default()
        };
        let mut stream = p
            .messages(jb(serde_json::json!({"model": "m1"})), &ctx)
            .await
            .unwrap();
        let ev = stream.next().await.unwrap().unwrap();
        assert_eq!(String::from_utf8_lossy(&ev.0), "data: {\"ok\":true}\n\n");
        // responses surface also streams on the mock
        let mut rstream = p
            .responses(jb(serde_json::json!({"model": "m1"})), &ctx)
            .await
            .unwrap();
        let rev = rstream.next().await.unwrap().unwrap();
        assert_eq!(String::from_utf8_lossy(&rev.0), "data: {\"ok\":true}\n\n");
    }
}
