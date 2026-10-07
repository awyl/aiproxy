//! Model registry + discovery: parallel per-provider model fetch, prefixed
//! catalog, prefix resolution for the API layer.

use crate::provider::{Model, Provider};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::task::JoinSet;

pub struct ModelRegistry {
    providers: Vec<Arc<dyn Provider>>,
    catalog: RwLock<BTreeMap<String, Vec<Model>>>,
    /// Last probe failure per provider, kept so the `/models` page can show why
    /// a provider has no models (a failed probe retains last-known entries, so
    /// the count alone does not say a probe failed).
    last_errors: RwLock<BTreeMap<String, String>>,
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRegistry")
            .field(
                "provider_ids",
                &self.providers.iter().map(|p| p.id()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// One provider's slice of the catalog, for the `/models` page: the models as
/// stored (no `{prefix}/`), plus why the last probe failed if it did.
#[derive(Debug, Clone)]
pub struct ProviderCatalog {
    pub id: String,
    pub models: Vec<Model>,
    pub error: Option<String>,
}

/// Next error map for a discovery round: a provider that answered clears its
/// error, a failure sets it, and a provider not in this round keeps what it had.
fn merge_errors(
    prev: &BTreeMap<String, String>,
    failed: &[(String, String)],
    answered: &[String],
) -> BTreeMap<String, String> {
    let mut next = prev.clone();
    for id in answered {
        next.remove(id);
    }
    for (id, error) in failed {
        next.insert(id.clone(), error.clone());
    }
    next
}

/// Outcome of one provider's discovery round (`refresh_report`).
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub id: String,
    /// Models now in the catalog for this provider (last-known count if the
    /// probe failed).
    pub models: usize,
    /// `None` when the probe answered; the failure otherwise.
    pub error: Option<String>,
}

impl ModelRegistry {
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        Self {
            providers,
            catalog: RwLock::new(BTreeMap::new()),
            last_errors: RwLock::new(BTreeMap::new()),
        }
    }

    /// Fetch every provider's model list in parallel, per-provider 10s
    /// timeout. Failing providers are logged and skipped; last-known
    /// catalog entries for them are retained.
    pub async fn refresh(&self) {
        let _ = self.refresh_report().await;
    }

    /// One discovery round, reporting per provider what happened — the same
    /// work as `refresh`, but the failures come back to the caller instead of
    /// only reaching the log (`POST /api/reload` shows them).
    pub async fn refresh_report(&self) -> Vec<RefreshOutcome> {
        let mut set = JoinSet::new();
        for p in &self.providers {
            let p = p.clone();
            set.spawn(async move {
                let deadline = tokio::time::timeout(Duration::from_secs(10), p.list_models());
                match deadline.await {
                    Ok(Ok(models)) => (p.id().to_string(), Some(models), None),
                    Ok(Err(e)) => {
                        tracing::warn!(provider = %p.id(), "model discovery failed: {e:?}");
                        (p.id().to_string(), None, Some(format!("{e:?}")))
                    }
                    Err(_) => {
                        tracing::warn!(provider = %p.id(), "model discovery timed out");
                        (
                            p.id().to_string(),
                            None,
                            Some("timed out after 10s".to_string()),
                        )
                    }
                }
            });
        }
        let mut updated: BTreeMap<String, Vec<Model>> = BTreeMap::new();
        let mut failed: Vec<(String, String)> = Vec::new();
        while let Some(res) = set.join_next().await {
            if let Ok((id, models, error)) = res {
                match (models, error) {
                    (Some(models), _) => {
                        updated.insert(id, models);
                    }
                    (None, Some(error)) => failed.push((id, error)),
                    (None, None) => {}
                }
            }
        }
        // retain last-known entries for providers that failed this round
        {
            let cat = self.catalog.read().unwrap();
            for (id, models) in cat.iter() {
                updated.entry(id.clone()).or_insert_with(|| models.clone());
            }
        }
        // counts come after the retain, so they show what a client will actually
        // see (a failed probe keeps serving last-known models)
        let mut report: Vec<RefreshOutcome> = updated
            .iter()
            .map(|(id, models)| RefreshOutcome {
                id: id.clone(),
                models: models.len(),
                error: None,
            })
            .collect();
        for (id, error) in &failed {
            match report.iter_mut().find(|r| &r.id == id) {
                Some(entry) => entry.error = Some(error.clone()),
                None => report.push(RefreshOutcome {
                    id: id.clone(),
                    models: 0,
                    error: Some(error.clone()),
                }),
            }
        }
        report.sort_by(|a, b| a.id.cmp(&b.id));
        *self.catalog.write().unwrap() = updated;
        // `updated` also holds the retained entries of providers that failed, so
        // pass only the ids that actually answered this round.
        let answered: Vec<String> = report
            .iter()
            .filter(|r| r.error.is_none())
            .map(|r| r.id.clone())
            .collect();
        let next = {
            let errors = self.last_errors.read().unwrap();
            merge_errors(&errors, &failed, &answered)
        };
        *self.last_errors.write().unwrap() = next;
        report
    }

    /// Catalog as served, grouped by provider (models keep their bare ids), with
    /// the last probe error. One entry per configured provider, in registry
    /// order — a provider with nothing discovered still appears, which is the
    /// case worth looking at.
    pub fn catalog_snapshot(&self) -> Vec<ProviderCatalog> {
        let cat = self.catalog.read().unwrap();
        let errors = self.last_errors.read().unwrap();
        self.providers
            .iter()
            .map(|p| {
                let id = p.id().to_string();
                ProviderCatalog {
                    models: cat.get(&id).cloned().unwrap_or_default(),
                    error: errors.get(&id).cloned(),
                    id,
                }
            })
            .collect()
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
    use crate::provider::ModelSurface;
    use crate::provider::Provider;
    use crate::provider::testutil::MockProvider;
    use std::sync::Arc;

    fn providers() -> Vec<Arc<dyn Provider>> {
        vec![
            Arc::new(MockProvider::new(
                "openai",
                vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            )),
            Arc::new(MockProvider::new(
                "anthropic",
                vec!["claude-sonnet-4".into()],
            )),
        ]
    }

    #[tokio::test]
    async fn refresh_report_names_each_provider_and_its_failure() {
        let reg = ModelRegistry::new(vec![
            Arc::new(MockProvider::new("openai", vec!["gpt-4o".into()])),
            Arc::new(MockProvider::failing("openai-codex")),
        ]);
        let report = reg.refresh_report().await;
        assert_eq!(report.len(), 2, "one entry per provider");
        let openai = report.iter().find(|r| r.id == "openai").unwrap();
        assert_eq!(openai.models, 1);
        assert_eq!(openai.error, None);
        let codex = report.iter().find(|r| r.id == "openai-codex").unwrap();
        assert_eq!(codex.models, 0);
        assert!(
            codex
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("mock failure"),
            "a failed probe must be reported, not silent: {codex:?}"
        );
        // the failing provider's last-known entries are still retained
        assert!(reg.models().iter().any(|m| m.id == "openai/gpt-4o"));
    }

    #[tokio::test]
    async fn catalog_snapshot_lists_every_provider_its_models_and_last_error() {
        let reg = ModelRegistry::new(vec![
            Arc::new(MockProvider::with_surface(
                "openai",
                vec!["gpt-4o".into()],
                ModelSurface::ChatCompletions,
            )),
            Arc::new(MockProvider::failing("openai-codex")),
            Arc::new(MockProvider::new("empty", vec![])),
        ]);
        // before the first round: every provider is listed, nothing is known yet
        let snap = reg.catalog_snapshot();
        assert_eq!(
            snap.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["openai", "openai-codex", "empty"]
        );
        assert!(
            snap.iter()
                .all(|p| p.models.is_empty() && p.error.is_none())
        );

        reg.refresh().await;
        let snap = reg.catalog_snapshot();
        let openai = snap.iter().find(|p| p.id == "openai").unwrap();
        assert_eq!(
            openai
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-4o"],
            "unprefixed ids: the page shows the model, not the routing key"
        );
        assert_eq!(openai.models[0].surface, ModelSurface::ChatCompletions);
        assert_eq!(openai.error, None);
        let codex = snap.iter().find(|p| p.id == "openai-codex").unwrap();
        assert!(
            codex
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("mock failure"),
            "the probe's failure belongs next to its (empty) model list: {codex:?}"
        );
        // a provider that answered with nothing is still listed
        assert_eq!(snap.iter().find(|p| p.id == "empty").unwrap().error, None);
    }

    #[test]
    fn merge_errors_clears_providers_that_recovered() {
        let prev = BTreeMap::from([
            ("stale".to_string(), "boom".to_string()),
            ("failing".to_string(), "old".to_string()),
        ]);
        let merged = merge_errors(
            &prev,
            &[("failing".to_string(), "new".to_string())],
            &["stale".to_string()],
        );
        assert_eq!(
            merged.get("stale"),
            None,
            "a provider that answered this round has no error to show"
        );
        assert_eq!(merged.get("failing").map(String::as_str), Some("new"));
        assert_eq!(merged.len(), 1);
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
        assert_eq!(
            reg.prefixes().collect::<Vec<_>>(),
            vec!["openai", "anthropic"]
        ); // insertion order
    }
}
