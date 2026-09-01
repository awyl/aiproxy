//! Provider registry: build provider impls from config.
//! `StaticProvider` covers upstreams with a static `models:` list (catalog-only).

pub mod anthropic;
pub mod go;
pub mod openai;
#[cfg(test)]
pub mod test_mock_upstream;

use crate::config::{Config, UpstreamConfig, UpstreamKind};
use crate::provider::{
    Model, ModelSurface, Provider, ProviderError, ProviderStream, RequestContext,
};
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::go::OpencodeGoProvider;
use crate::providers::openai::OpenAiProvider;
use serde_json::Value;
use std::sync::Arc;

/// Upstream whose `models:` list is static: serves discovery; streams when a
/// default surface is known (e.g. minimax -> chat) or `surface:` is set.
#[derive(Debug, Clone)]
pub struct StaticProvider {
    id: String,
    models: Vec<String>,
    surface: Option<ModelSurface>,
}

impl StaticProvider {
    pub fn from_cfg(
        cfg: &UpstreamConfig,
        id: &str,
        default_surface: Option<ModelSurface>,
    ) -> Option<Arc<dyn Provider>> {
        if cfg.models.is_empty() {
            return None;
        }
        let surface = cfg.surface.or(default_surface);
        Some(Arc::new(StaticProvider {
            id: id.to_string(),
            models: cfg.models.clone(),
            surface,
        }))
    }
}

#[async_trait::async_trait]
impl Provider for StaticProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn surface_of(&self, _model: &str) -> ModelSurface {
        self.surface.unwrap_or(ModelSurface::Unknown)
    }
    async fn list_models(&self) -> Result<Vec<Model>, ProviderError> {
        Ok(self
            .models
            .iter()
            .map(|m| Model {
                id: m.clone(),
                display_name: None,
                created_at: None,
                surface: ModelSurface::Unknown,
            })
            .collect())
    }
    async fn chat_completions(
        &self,
        _req: Value,
        _ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::Transport(
            "static provider only serves discovery".into(),
        ))
    }
    async fn messages(
        &self,
        _req: Value,
        _ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::Transport(
            "static provider only serves discovery".into(),
        ))
    }
    async fn responses(
        &self,
        _req: Value,
        _ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::Transport(
            "static provider only serves discovery".into(),
        ))
    }
}

/// Upstream whose discovery is disabled or whose `models:` list is static.
/// "Static" providers serve discovery only; `NoDiscoveryProvider` keeps the
/// real streaming impl but never probes the upstream `list_models` endpoint.
pub struct NoDiscoveryProvider {
    inner: Arc<dyn Provider>,
    fallback: Vec<Model>,
}

impl std::fmt::Debug for NoDiscoveryProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoDiscoveryProvider")
            .field("inner", &self.inner.id())
            .finish()
    }
}
impl NoDiscoveryProvider {
    pub fn wrap(inner: Arc<dyn Provider>) -> Arc<dyn Provider> {
        NoDiscoveryProvider::with_models(inner, Vec::new())
    }

    /// Opt-out wrapper that still serves a static `models:` catalog.
    pub fn with_models(inner: Arc<dyn Provider>, fallback: Vec<Model>) -> Arc<dyn Provider> {
        Arc::new(NoDiscoveryProvider { inner, fallback })
    }
}

#[async_trait::async_trait]
impl Provider for NoDiscoveryProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn surface_of(&self, model: &str) -> ModelSurface {
        self.inner.surface_of(model)
    }
    async fn list_models(&self) -> Result<Vec<Model>, ProviderError> {
        Ok(self.fallback.clone())
    }
    async fn chat_completions(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        self.inner.chat_completions(req, ctx).await
    }
    async fn messages(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        self.inner.messages(req, ctx).await
    }
    async fn responses(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        self.inner.responses(req, ctx).await
    }
}

pub fn build_providers(cfg: &Config) -> Vec<Arc<dyn Provider>> {
    let ids = cfg.provider_ids();
    cfg.upstreams
        .iter()
        .zip(ids)
        .map(|(u, id)| match u.kind {
            UpstreamKind::Openai => discoverable(u, &id, |u, id| Arc::new(OpenAiProvider::new(u, id))),
            UpstreamKind::Anthropic => discoverable(u, &id, |u, id| Arc::new(AnthropicProvider::new(u, id))),
            UpstreamKind::OpencodeGo => discoverable(u, &id, |u, id| Arc::new(OpencodeGoProvider::new(u, id))),
            UpstreamKind::Minimax
            | UpstreamKind::Zai
            | UpstreamKind::Openrouter
            | UpstreamKind::Nvidia => {
                chat_kind(u, &id)
            }
        })
        .collect()
}

/// Static `models:` win; else `discover: true` probes live; else the catalog is
/// empty but requests still route. Shared by the openai/anthropic/go kinds.
fn discoverable(
    u: &UpstreamConfig,
    id: &str,
    make: impl Fn(&UpstreamConfig, &str) -> Arc<dyn Provider>,
) -> Arc<dyn Provider> {
    if let Some(p) = StaticProvider::from_cfg(u, id, None) {
        p
    } else if u.discover {
        make(u, id)
    } else {
        NoDiscoveryProvider::wrap(make(u, id))
    }
}

/// Chat-fixed OpenAI-compatible gateways: static `models:` act as the catalog
/// with `surface = chat` while the gateway still streams; `discover: true`
/// probes live instead.
fn chat_kind(u: &UpstreamConfig, id: &str) -> Arc<dyn Provider> {
    let provider = Arc::new(OpenAiProvider::new(u, id)) as Arc<dyn Provider>;
    if u.discover {
        provider
    } else {
        let catalog = u
            .models
            .iter()
            .map(|m| Model {
                id: m.clone(),
                display_name: None,
                created_at: None,
                surface: ModelSurface::ChatCompletions,
            })
            .collect();
        NoDiscoveryProvider::with_models(provider, catalog)
    }
}

/// Shared HTTP client for the gateway providers (10s connect, 600s total).
pub(crate) fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .expect("reqwest client build")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::testutil::MockProvider;

    #[tokio::test]
    async fn no_discovery_wrapper_passes_through_routing_but_not_probing() {
        let inner = Arc::new(MockProvider::with_surface(
            "openai",
            vec!["gpt-4o".into()],
            crate::provider::ModelSurface::ChatCompletions,
        ));
        let wrapped = NoDiscoveryProvider::wrap(inner);
        assert_eq!(wrapped.id(), "openai");
        assert_eq!(
            wrapped.surface_of("gpt-4o"),
            crate::provider::ModelSurface::ChatCompletions
        );
        // no probing: empty catalog, no network
        assert!(wrapped.list_models().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn build_providers_respects_discover_opt_in() {
        let cfg = Config::from_yaml(
            r#"
upstreams:
  - { name: skipped, kind: openai }
  - { name: pinned, kind: anthropic, models: [claude-x] }
  - { name: go, kind: opencode-go }
"#,
        )
        .unwrap();

        let providers = build_providers(&cfg);
        assert_eq!(providers.len(), 3);

        // openai without models and without discover -> exists, but catalog empty
        // ID = kind name (single upstream of kind)
        let skipped = providers
            .iter()
            .find(|p| p.id() == "openai")
            .expect("provider present");
        assert!(skipped.list_models().await.unwrap().is_empty());

        // static models list -> catalog-only upstream
        let pinned = providers.iter().find(|p| p.id() == "anthropic").unwrap();
        let ids: Vec<String> = pinned
            .list_models()
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["claude-x"]);
    }

    #[tokio::test]
    async fn opencode_go_discovery_is_also_opt_in() {
        // opt out via static list
        let cfg = Config::from_yaml(
            r#"
upstreams:
  - { name: go, kind: opencode-go, models: [kimi-k3] }
"#,
        )
        .unwrap();
        let p = build_providers(&cfg);
        let ids: Vec<String> = p[0]
            .list_models()
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["kimi-k3"]);

        // opt out via missing discover flag
        let cfg = Config::from_yaml(
            r#"
upstreams:
  - { name: go, kind: opencode-go }
"#,
        )
        .unwrap();
        let p = build_providers(&cfg);
        assert!(
            p[0].list_models().await.unwrap().is_empty(),
            "go without discover: true must not probe"
        );
    }
}
