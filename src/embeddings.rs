//! Local embeddings — in-process fastembed backend behind the fake
//! `embeddings-local` provider.
//!
//! Lifecycle: a model is loaded on first request for that model (single-flight),
//! kept for reuse, and dropped by the idle reaper after `idle_ttl_secs` with no
//! traffic. All models are ONNX-based and auto-downloaded from HuggingFace on
//! first use.

use crate::config::EmbeddingsConfig;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("unknown embedding model: {0}")]
    UnknownModel(String),
    #[error("embedding model '{0}' failed to load: {1}")]
    LoadFailed(String, String),
    #[error("embedding call failed for '{0}': {1}")]
    EmbedFailed(String, String),
}

// ---------------------------------------------------------------------------
// Backend trait — real fastembed in prod, mockable in tests
// ---------------------------------------------------------------------------

/// A loaded model instance that can embed texts.
#[async_trait::async_trait]
pub trait ModelInstance: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> Result<(Vec<Vec<f32>>, usize), String>;
}

/// Backend that loads model instances on demand.
#[async_trait::async_trait]
pub trait EmbeddingBackend: Send + Sync {
    async fn load_model(&self, model: &str) -> Result<Arc<dyn ModelInstance>, EmbedError>;
}

// ---------------------------------------------------------------------------
// fastembed backend
// ---------------------------------------------------------------------------

pub struct FastembedBackend;

#[async_trait::async_trait]
impl EmbeddingBackend for FastembedBackend {
    async fn load_model(&self, model: &str) -> Result<Arc<dyn ModelInstance>, EmbedError> {
        let model_name = model.to_string();
        let instance = tokio::task::spawn_blocking(move || {
            let variant: fastembed::EmbeddingModel = model_name
                .parse()
                .map_err(|_| format!("unknown fastembed model variant: {model_name}"))?;
            let opts = fastembed::InitOptions::new(variant);
            let te = fastembed::TextEmbedding::try_new(opts)
                .map_err(|e| format!("init: {e}"))?;
            Ok::<_, String>(te)
        })
        .await
        .map_err(|e| EmbedError::LoadFailed(model.into(), format!("task join: {e}")))?
        .map_err(|e| EmbedError::LoadFailed(model.into(), e))?;

        Ok(Arc::new(FastembedInstance(Mutex::new(instance))))
    }
}

struct FastembedInstance(Mutex<fastembed::TextEmbedding>);

#[async_trait::async_trait]
impl ModelInstance for FastembedInstance {
    async fn embed(&self, texts: &[&str]) -> Result<(Vec<Vec<f32>>, usize), String> {
        let mut guard = self.0.lock().await;
        let result = guard
            .embed(texts, None)
            .map_err(|e| format!("fastembed embed: {e}"))?;
        let dim = result.first().map_or(0, |v| v.len());
        Ok((result, dim))
    }
}

// ---------------------------------------------------------------------------
// Slot and manager
// ---------------------------------------------------------------------------

struct EmbeddingSlot {
    id: String,
    model: String,
    dimensions: Option<u32>,
    state: Mutex<SlotState>,
}

enum SlotState {
    Idle,
    Loaded {
        instance: Arc<dyn ModelInstance>,
        last_used: Instant,
    },
}

pub struct EmbeddingManager {
    idle_ttl: Duration,
    slots: Vec<EmbeddingSlot>,
    backend: Arc<dyn EmbeddingBackend>,
}

impl std::fmt::Debug for EmbeddingManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingManager")
            .field("idle_ttl", &self.idle_ttl)
            .field("slots", &self.slots.len())
            .finish()
    }
}

impl EmbeddingManager {
    pub fn new(cfg: &EmbeddingsConfig) -> Self {
        Self::with_backend(cfg, Arc::new(FastembedBackend))
    }

    pub(crate) fn with_backend(cfg: &EmbeddingsConfig, backend: Arc<dyn EmbeddingBackend>) -> Self {
        let idle_ttl = Duration::from_secs(cfg.idle_ttl_secs);
        let slots = cfg
            .models
            .iter()
            .map(|m| EmbeddingSlot {
                id: m.id.clone(),
                model: m.model.clone(),
                dimensions: m.dimensions,
                state: Mutex::new(SlotState::Idle),
            })
            .collect();
        Self {
            idle_ttl,
            slots,
            backend,
        }
    }

    /// Unprefixed embedding model ids for the /v1/models catalog.
    pub fn model_ids(&self) -> Vec<String> {
        self.slots.iter().map(|s| s.id.clone()).collect()
    }

    fn slot(&self, id: &str) -> Option<&EmbeddingSlot> {
        self.slots.iter().find(|s| s.id == id)
    }

    /// Load the model for `id` if not already loaded (single-flight via slot
    /// mutex), then embed the input texts and return an OpenAI-compatible JSON
    /// response.
    pub async fn embed(&self, id: &str, req: &Value) -> Result<Value, EmbedError> {
        let slot = self
            .slot(id)
            .ok_or_else(|| EmbedError::UnknownModel(id.to_string()))?;

        let input_texts = extract_input_texts(req)?;
        tracing::debug!(
            model = %id,
            input_count = input_texts.len(),
            "embedding request"
        );

        let t0 = Instant::now();
        let (embeddings, dim) = {
            let mut st = slot.state.lock().await;
            self.ensure_loaded(slot, &mut st).await?;
            if let SlotState::Loaded {
                instance, last_used, ..
            } = &mut *st
            {
                *last_used = Instant::now();
                let inst = Arc::clone(instance);
                drop(st); // release lock before embed call

                let refs: Vec<&str> = input_texts.iter().map(|s| s.as_str()).collect();
                let (vecs, d) = inst.embed(&refs).await.map_err(|e| {
                    EmbedError::EmbedFailed(slot.id.clone(), e)
                })?;
                let dim_override = slot.dimensions.unwrap_or(d as u32);
                (vecs, dim_override)
            } else {
                unreachable!("ensure_loaded just set Loaded");
            }
        };

        // Build OpenAI-compatible response.
        let dim = dim as usize;
        let data: Vec<Value> = embeddings
            .into_iter()
            .enumerate()
            .map(|(i, vec)| {
                serde_json::json!({
                    "object": "embedding",
                    "index": i,
                    "embedding": vec,
                })
            })
            .collect();
        let total_tokens: usize = input_texts.iter().map(|t| t.len() / 4).sum(); // rough estimate
        let elapsed_ms = t0.elapsed().as_millis() as u64;

        tracing::debug!(
            model = %id,
            input_count = input_texts.len(),
            dimensions = dim,
            elapsed_ms,
            "embedding completed"
        );

        Ok(serde_json::json!({
            "object": "list",
            "model": format!("embeddings-local/{}", id),
            "data": data,
            "usage": {
                "prompt_tokens": total_tokens,
                "total_tokens": total_tokens,
            },
            "dimensions": dim,
        }))
    }

    /// Ensure the model for `slot` is loaded. Single-flight: the caller holds
    /// the slot lock; load happens under it.
    async fn ensure_loaded(
        &self,
        slot: &EmbeddingSlot,
        st: &mut SlotState,
    ) -> Result<(), EmbedError> {
        match st {
            SlotState::Loaded { last_used, .. } => {
                *last_used = Instant::now();
                Ok(())
            }
            SlotState::Idle => {
                tracing::info!(model = %slot.id, fastembed_model = %slot.model, "loading embedding model");
                let t0 = Instant::now();
                let instance = self.backend.load_model(&slot.model).await?;
                tracing::info!(
                    model = %slot.id,
                    elapsed_ms = t0.elapsed().as_millis() as u64,
                    "embedding model loaded"
                );
                *st = SlotState::Loaded {
                    instance,
                    last_used: Instant::now(),
                };
                Ok(())
            }
        }
    }

    /// Unload every loaded model (shutdown / test cleanup).
    pub async fn shutdown_all(&self) {
        for slot in &self.slots {
            let mut st = slot.state.lock().await;
            if matches!(&*st, SlotState::Loaded { .. }) {
                *st = SlotState::Idle;
            }
        }
    }

    /// One reaper pass: drop models idle longer than the TTL.
    /// Called from the reaper task at an interval; public so tests drive it.
    pub async fn reaper_round(&self) {
        for slot in &self.slots {
            let mut st = slot.state.lock().await;
            if let SlotState::Loaded { last_used, .. } = &*st
                && last_used.elapsed() >= self.idle_ttl
            {
                tracing::info!(model = %slot.id, "unloading idle embedding model");
                *st = SlotState::Idle;
            }
        }
    }
}

/// Extract input texts from an OpenAI-format embedding request.
/// Supports both `"input": "single string"` and `"input": ["array", "of", "strings"]`.
fn extract_input_texts(req: &Value) -> Result<Vec<String>, EmbedError> {
    let input = req
        .get("input")
        .ok_or_else(|| EmbedError::EmbedFailed("?".into(), "missing 'input' field".into()))?;
    match input {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(arr) => arr
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v.as_str()
                    .map(String::from)
                    .ok_or_else(|| {
                        EmbedError::EmbedFailed(
                            "?".into(),
                            format!("input[{i}] is not a string"),
                        )
                    })
            })
            .collect(),
        _ => Err(EmbedError::EmbedFailed(
            "?".into(),
            "'input' must be a string or array of strings".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use crate::config::{EmbeddingModelConfig, EmbeddingsConfig};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fake backend for unit tests — no real model loading, fast, deterministic.
    pub struct FakeBackend {
        pub load_count: AtomicUsize,
    }

    impl FakeBackend {
        pub fn new() -> Self {
            Self {
                load_count: AtomicUsize::new(0),
            }
        }
    }

    struct FakeInstance;

    #[async_trait::async_trait]
    impl EmbeddingBackend for FakeBackend {
        async fn load_model(&self, _model: &str) -> Result<Arc<dyn ModelInstance>, EmbedError> {
            self.load_count.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(FakeInstance))
        }
    }

    #[async_trait::async_trait]
    impl ModelInstance for FakeInstance {
        async fn embed(&self, texts: &[&str]) -> Result<(Vec<Vec<f32>>, usize), String> {
            // Return fixed embeddings with integer values for clean assertion.
            let vecs = texts.iter().map(|_| vec![1.0, 2.0, 3.0]).collect();
            Ok((vecs, 3))
        }
    }

    pub fn manager_with_fake(
        ttl_secs: u64,
        ids: &[&str],
    ) -> (EmbeddingManager, Arc<FakeBackend>) {
        let backend = Arc::new(FakeBackend::new());
        let models: Vec<EmbeddingModelConfig> = ids
            .iter()
            .map(|id| EmbeddingModelConfig {
                id: (*id).to_string(),
                model: "FakeModel".to_string(),
                dimensions: None,
            })
            .collect();
        let cfg = EmbeddingsConfig {
            idle_ttl_secs: ttl_secs,
            models,
        };
        let mgr = EmbeddingManager::with_backend(&cfg, backend.clone());
        (mgr, backend)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn embed_returns_openai_compatible_response() {
        let (mgr, _) = manager_with_fake(3600, &["nomic"]);
        let req = serde_json::json!({ "model": "nomic", "input": "hello world" });
        let out = mgr.embed("nomic", &req).await.expect("embed ok");
        assert_eq!(out["object"], "list");
        assert_eq!(out["model"], "embeddings-local/nomic");
        assert_eq!(out["data"][0]["object"], "embedding");
        assert_eq!(out["data"][0]["index"], 0);
        assert_eq!(
            out["data"][0]["embedding"],
            serde_json::json!([1.0, 2.0, 3.0])
        );
        assert_eq!(out["usage"]["prompt_tokens"], 2); // "hello world" / 4 ≈ 2
        assert_eq!(out["dimensions"], 3);
    }

    #[tokio::test]
    async fn embed_batch_input() {
        let (mgr, _) = manager_with_fake(3600, &["nomic"]);
        let req = serde_json::json!({ "model": "nomic", "input": ["hello", "world"] });
        let out = mgr.embed("nomic", &req).await.expect("embed ok");
        assert_eq!(out["data"].as_array().unwrap().len(), 2);
        assert_eq!(out["data"][0]["index"], 0);
        assert_eq!(out["data"][1]["index"], 1);
    }

    #[tokio::test]
    async fn loads_model_on_demand_and_reuses() {
        let (mgr, backend) = manager_with_fake(3600, &["nomic"]);
        let req = serde_json::json!({ "model": "nomic", "input": "hi" });

        let _ = mgr.embed("nomic", &req).await.expect("embed 1");
        assert_eq!(backend.load_count.load(Ordering::SeqCst), 1);

        let _ = mgr.embed("nomic", &req).await.expect("embed 2");
        assert_eq!(
            backend.load_count.load(Ordering::SeqCst),
            1,
            "must not reload"
        );
    }

    #[tokio::test]
    async fn idle_reaper_unloads_model() {
        let (mgr, backend) = manager_with_fake(1, &["nomic"]);
        let req = serde_json::json!({ "model": "nomic", "input": "hi" });

        let _ = mgr.embed("nomic", &req).await.expect("embed ok");
        assert_eq!(backend.load_count.load(Ordering::SeqCst), 1);

        // Wait for the model to become idle (ttl = 1s)
        tokio::time::sleep(Duration::from_millis(1100)).await;
        mgr.reaper_round().await;

        // Next request must reload
        let _ = mgr.embed("nomic", &req).await.expect("embed after reap");
        assert_eq!(
            backend.load_count.load(Ordering::SeqCst),
            2,
            "must reload after reaper"
        );
    }

    #[tokio::test]
    async fn traffic_within_ttl_keeps_model() {
        let (mgr, backend) = manager_with_fake(10, &["nomic"]);
        let req = serde_json::json!({ "model": "nomic", "input": "hi" });

        let _ = mgr.embed("nomic", &req).await.expect("embed 1");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = mgr.embed("nomic", &req).await.expect("embed 2"); // refresh
        tokio::time::sleep(Duration::from_millis(200)).await;
        mgr.reaper_round().await;

        let _ = mgr.embed("nomic", &req).await.expect("embed 3");
        assert_eq!(
            backend.load_count.load(Ordering::SeqCst),
            1,
            "recent traffic must keep the model"
        );
    }

    #[tokio::test]
    async fn unknown_model_is_rejected() {
        let (mgr, _) = manager_with_fake(3600, &[]);
        let err = mgr
            .embed("nope", &serde_json::json!({ "model": "nope" }))
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::UnknownModel(_)));
    }

    #[tokio::test]
    async fn shutdown_all_unloads_models() {
        let (mgr, backend) = manager_with_fake(3600, &["nomic"]);
        let req = serde_json::json!({ "model": "nomic", "input": "hi" });
        let _ = mgr.embed("nomic", &req).await.expect("embed ok");
        assert_eq!(backend.load_count.load(Ordering::SeqCst), 1);

        mgr.shutdown_all().await;

        // Next request must reload
        let _ = mgr.embed("nomic", &req).await.expect("embed after shutdown");
        assert_eq!(backend.load_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn missing_input_is_rejected() {
        let (mgr, _) = manager_with_fake(3600, &["nomic"]);
        let err = mgr
            .embed("nomic", &serde_json::json!({ "model": "nomic" }))
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::EmbedFailed(..)));
    }

    #[tokio::test]
    async fn model_ids_exposed_for_catalog() {
        let (mgr, _) = manager_with_fake(3600, &["a", "b", "c"]);
        assert_eq!(mgr.model_ids(), vec!["a", "b", "c"]);
    }
}
