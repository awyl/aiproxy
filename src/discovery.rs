//! Model registry + discovery: parallel per-provider model fetch, prefixed
//! catalog, prefix resolution for the API layer.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::task::JoinSet;
use crate::provider::{Model, Provider, ProviderError};

pub struct ModelRegistry {
    providers: Vec<Arc<dyn Provider>>,
    catalog: RwLock<BTreeMap<String, Vec<Model>>>,
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRegistry")
            .field("provider_ids", &self.providers.iter().map(|p| p.id()).collect::<Vec<_>>())
            .finish()
    }
}

impl ModelRegistry {
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        Self {
            providers,
            catalog: RwLock::new(BTreeMap::new()),
        }
    }

    /// Fetch every provider's model list in parallel, per-provider 10s
    /// timeout. Failing providers are logged and skipped; last-known
    /// catalog entries for them are retained.
    pub async fn refresh(&self) {
        let mut set = JoinSet::new();
        for p in &self.providers {
            let p = p.clone();
            set.spawn(async move {
                let deadline = tokio::time::timeout(Duration::from_secs(10), p.list_models());
                        let models = match deadline.await {
                    Ok(Ok(models)) => models,
                    Ok(Err(e)) => {
                        tracing::warn!(provider = %p.id(), "model discovery failed: {e:?}");
                        return None;
                    }
                    Err(_) => {
                        tracing::warn!(provider = %p.id(), "model discovery timed out");
                        return None;
                    }
                };
                Some((p.id().to_string(), models))
            });
        }
        let mut updated: BTreeMap<String, Vec<Model>> = BTreeMap::new();
        while let Some(res) = set.join_next().await {
            if let Ok(Some((id, models))) = res {
                updated.insert(id, models);
            }
        }
        // retain last-known entries for providers that failed this round
        {
            let cat = self.catalog.read().unwrap();
            for (id, models) in cat.iter() {
                updated.entry(id.clone()).or_insert_with(|| models.clone());
            }
        }
        *self.catalog.write().unwrap() = updated;
    }

    /// Flattened prefixed catalog, sorted by id, deduplicated.
    pub fn models(&self) -> Vec<Model> {
        let cat = self.catalog.read().unwrap();
        let mut out: Vec<Model> = cat
            .iter()
            .flat_map(|(pid, models)| {
                models.iter().map(move |m| Model {
                    id: format!("{pid}/{}", m.id),
                    display_name: m.display_name.clone(),
                    created_at: m.created_at.clone(),
                    surface: m.surface,
                })
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }

    pub fn provider(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers.iter().find(|p| p.id() == id).cloned()
    }

    pub fn prefixes(&self) -> impl Iterator<Item = &str> {
        self.providers.iter().map(|p| p.id())
    }

    /// Split "{prefix}/{model}" if the prefix names a known provider.
    pub fn resolve(&self, prefixed: &str) -> Option<(String, String)> {
        let (pid, model) = prefixed.split_once('/')?;
        if self.provider(pid).is_some() {
            Some((pid.to_string(), model.to_string()))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::testutil::MockProvider;
    use crate::provider::Provider;
    use std::sync::Arc;

    fn providers() -> Vec<Arc<dyn Provider>> {
        vec![
            Arc::new(MockProvider::new("openai", vec!["gpt-4o".into(), "gpt-4o-mini".into()])),
            Arc::new(MockProvider::new("anthropic", vec!["claude-sonnet-4".into()])),
        ]
    }

    #[tokio::test]
    async fn refresh_merges_and_prefixes_catalog() {
        let reg = ModelRegistry::new(providers());
        reg.refresh().await;
        let models = reg.models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "anthropic/claude-sonnet-4",
                "openai/gpt-4o",
                "openai/gpt-4o-mini"
            ]
        );
    }

    #[tokio::test]
    async fn failing_provider_does_not_block_others() {
        let bad = MockProvider::failing("bad");
        let reg = ModelRegistry::new(vec![
            Arc::new(MockProvider::new("good", vec!["a".into()])),
            Arc::new(bad),
        ]);
        reg.refresh().await;
        let models = reg.models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["good/a"]);
    }

    #[test]
    fn resolve_prefix_and_provider_lookup() {
        let reg = ModelRegistry::new(providers());
        assert_eq!(
            reg.resolve("openai/gpt-4o"),
            Some(("openai".into(), "gpt-4o".into()))
        );
        assert_eq!(reg.resolve("gpt-4o"), None);
        assert_eq!(reg.resolve("unknown/gpt-4o"), None);
        assert_eq!(reg.resolve("openai/"), Some(("openai".into(), "".into())));
        assert!(reg.provider("openai").is_some());
        assert!(reg.provider("unknown").is_none());
        assert_eq!(reg.prefixes().collect::<Vec<_>>(), vec!["openai", "anthropic"]); // insertion order
    }
}