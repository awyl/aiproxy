//! Provider registry: build provider impls from config.
//! `StaticProvider` covers upstreams with a static `models:` list (catalog-only).

pub mod anthropic;
pub mod go;
pub mod openai;
#[cfg(test)]
pub mod test_mock_upstream;

use std::sync::Arc;
use crate::config::{Config, UpstreamConfig, UpstreamKind};
use crate::provider::{Model, ModelSurface, Provider, ProviderError, ProviderStream, RequestContext};
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::go::OpencodeGoProvider;
use crate::providers::openai::OpenAiProvider;
use serde_json::Value;

/// Upstream whose `models:` list is static: serves discovery; streams when a
/// default surface is known (e.g. minimax -> chat) or `surface:` is set.
#[derive(Debug, Clone)]
pub struct StaticProvider {
    id: String,
    models: Vec<String>,
    surface: Option<ModelSurface>,
}

impl StaticProvider {
    pub fn from_cfg(cfg: &UpstreamConfig, default_surface: Option<ModelSurface>) -> Option<Arc<dyn Provider>> {
        if cfg.models.is_empty() {
            return None;
        }
        let surface = cfg.surface.or(default_surface);
        Some(Arc::new(StaticProvider {
            id: cfg.name.clone(),
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
    pub fn new(inner: Arc<dyn Provider>) -> Arc<dyn Provider> {
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
    cfg.upstreams
        .iter()
        .map(|u| match u.kind {
            UpstreamKind::Openai => {
                if let Some(p) = StaticProvider::from_cfg(u, None) {
                    p
                } else if u.discover {
                    Arc::new(OpenAiProvider::new(u)) as Arc<dyn Provider>
                } else {
                    NoDiscoveryProvider::new(Arc::new(OpenAiProvider::new(u)))
                }
            }
            UpstreamKind::Anthropic => {
                if let Some(p) = StaticProvider::from_cfg(u, None) {
                    p
                } else if u.discover {
                    Arc::new(AnthropicProvider::new(u)) as Arc<dyn Provider>
                } else {
                    NoDiscoveryProvider::new(Arc::new(AnthropicProvider::new(u)))
                }
            }
            UpstreamKind::Minimax | UpstreamKind::Zai | UpstreamKind::Openrouter | UpstreamKind::Nvidia => {
                // OpenAI-compatible gateways with a fixed chat surface:
                //   Minimax — api.minimax.io/v1 (Token Plan / pay-as-you-go,
                //             MINIMAX_API_KEY)
                //   Z.AI GLM Coding Plan — api.z.ai/api/coding/paas/v4
                //             (subscription key, ZAI_API_KEY)
                //   OpenRouter — openrouter.ai/api/v1 (aggregator, id
                //             `provider/model`, catalog public/keyless,
                //             OPENROUTER_API_KEY)
                //   NVIDIA NIM — integrate.api.nvidia.com/v1 (id `org/model`,
                //             catalog public/keyless, NVIDIA_API_KEY);
                //             self-hosted NIM = custom base_url
                let provider = Arc::new(OpenAiProvider::new(u)) as Arc<dyn Provider>;
                if u.discover {
                    provider
                } else {
                    // Static `models:` act as the catalog; the gateway still
                    // streams (surface = chat for the whole kind).
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
            UpstreamKind::OpencodeGo => {
                if let Some(p) = StaticProvider::from_cfg(u, None) {
                    p
                } else if u.discover {
                    Arc::new(OpencodeGoProvider::new(u)) as Arc<dyn Provider>
                } else {
                    NoDiscoveryProvider::new(Arc::new(OpencodeGoProvider::new(u)))
                }
            }
        })
        .collect()
}#[cfg(test)]
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
        let wrapped = NoDiscoveryProvider::new(inner);
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
        let skipped = providers
            .iter()
            .find(|p| p.id() == "skipped")
            .expect("provider present");
        assert!(skipped.list_models().await.unwrap().is_empty());

        // static models list -> catalog-only upstream
        let pinned = providers
            .iter()
            .find(|p| p.id() == "pinned")
            .unwrap();
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