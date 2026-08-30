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

/// Upstream whose `models:` list is static: serves discovery, never streams.
#[derive(Debug, Clone)]
pub struct StaticProvider {
    id: String,
    models: Vec<String>,
}

impl StaticProvider {
    pub fn from_cfg(cfg: &UpstreamConfig) -> Option<Arc<dyn Provider>> {
        if cfg.models.is_empty() {
            return None;
        }
        Some(Arc::new(StaticProvider {
            id: cfg.name.clone(),
            models: cfg.models.clone(),
        }))
    }
}

#[async_trait::async_trait]
impl Provider for StaticProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn surface_of(&self, _model: &str) -> ModelSurface {
        ModelSurface::Unknown
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

pub fn build_providers(cfg: &Config) -> Vec<Arc<dyn Provider>> {
    cfg.upstreams
        .iter()
        .map(|u| match u.kind {
            UpstreamKind::Openai => {
                StaticProvider::from_cfg(u).unwrap_or_else(|| Arc::new(OpenAiProvider::new(u)))
            }
            UpstreamKind::Anthropic => {
                StaticProvider::from_cfg(u).unwrap_or_else(|| Arc::new(AnthropicProvider::new(u)))
            }
            UpstreamKind::OpencodeGo => {
                StaticProvider::from_cfg(u).unwrap_or_else(|| Arc::new(OpencodeGoProvider::new(u)))
            }
        })
        .collect()
}