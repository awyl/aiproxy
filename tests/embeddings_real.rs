//! Integration tests for the real fastembed embedding backend.
//!
//! These tests download models from HuggingFace and run actual ONNX inference.
//! They are slow (first run downloads ~90MB per model) and require network access.
//!
//! Run with:
//!   cargo test --test embeddings_real -- --nocapture
//!
//! To test with a custom cache dir:
//!   FASTEMBED_CACHE_DIR=/tmp/models cargo test --test embeddings_real -- --nocapture

use aiproxy::embeddings::{EmbedError, EmbeddingBackend, FastembedBackend};

/// Serializes tests that share the fastembed cache dir to prevent download races.
static CACHE_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// All three models configured in aiproxy.yaml.example.
const TEST_MODELS: &[(&str, &str, usize)] = &[
    // (fastembed variant name, display id, expected_dimensions)
    ("AllMiniLML6V2", "all-mini-lm-l6-v2", 384),
    ("NomicEmbedTextV15", "nomic-embed-text-v1.5", 768),
    ("BGESmallENV15", "bge-small-en-v1.5", 384),
];

/// Test that each model can be downloaded, loaded, and produces valid embeddings.
#[tokio::test]
async fn real_download_load_and_embed() {
    let _guard = CACHE_LOCK.lock().await;
    let backend = FastembedBackend;
    let sample_texts = ["Hello, world!", "This is a test sentence."];

    for (variant, id, expected_dims) in TEST_MODELS {
        eprintln!("\n=== Testing {variant} (id={id}, dims={expected_dims}) ===");

        // Step 1: Load model (downloads on first run)
        let t0 = std::time::Instant::now();
        let instance = backend.load_model(variant).await.unwrap_or_else(|e| {
            panic!("failed to load model {variant}: {e}");
        });
        let load_ms = t0.elapsed().as_millis();
        eprintln!("  load: {load_ms}ms");

        // Step 2: Embed sample texts
        let t1 = std::time::Instant::now();
        let refs: Vec<&str> = sample_texts.to_vec();
        let (embeddings, dims) = instance.embed(&refs).await.unwrap_or_else(|e| {
            panic!("failed to embed with {variant}: {e}");
        });
        let embed_ms = t1.elapsed().as_millis();
        eprintln!(
            "  embed: {embed_ms}ms, got {} vectors of dim {dims}",
            embeddings.len()
        );

        // Step 3: Validate output shape
        assert_eq!(
            embeddings.len(),
            sample_texts.len(),
            "{variant}: wrong number of vectors"
        );
        assert_eq!(dims, *expected_dims, "{variant}: unexpected dimensions");

        // Step 4: Validate embeddings are not all zeros (model actually ran)
        let all_zero = embeddings.iter().all(|v| v.iter().all(|x| *x == 0.0));
        assert!(
            !all_zero,
            "{variant}: all-zero embeddings — model may not have loaded"
        );

        // Step 5: Validate embeddings differ between inputs (model is actually computing)
        assert_ne!(
            embeddings[0], embeddings[1],
            "{variant}: identical embeddings for different inputs"
        );

        eprintln!("  ✓ {variant} passed");

        // Step 6: Verify model can be loaded again (cache hit path)
        let t2 = std::time::Instant::now();
        let _instance2 = backend.load_model(variant).await.unwrap_or_else(|e| {
            panic!("failed to reload model {variant}: {e}");
        });
        let reload_ms = t2.elapsed().as_millis();
        eprintln!("  reload (cache): {reload_ms}ms");

        // Reload should be significantly faster than initial load (download skipped)
        if load_ms > 5000 {
            assert!(
                reload_ms < load_ms / 2,
                "{variant}: reload ({reload_ms}ms) should be much faster than initial load ({load_ms}ms)"
            );
        }
    }
}

/// Test that the EmbeddingManager (with real backend) works end-to-end.
#[tokio::test]
async fn real_manager_embed_flow() {
    let _guard = CACHE_LOCK.lock().await;
    use aiproxy::config::{EmbeddingModelConfig, EmbeddingsConfig};
    use aiproxy::embeddings::EmbeddingManager;

    let cfg = EmbeddingsConfig {
        idle_ttl_secs: 60,
        models: TEST_MODELS
            .iter()
            .map(|(variant, id, dims)| EmbeddingModelConfig {
                id: id.to_string(),
                model: variant.to_string(),
                dimensions: Some(*dims as u32),
            })
            .collect(),
    };

    let mgr = EmbeddingManager::new(&cfg);
    assert_eq!(
        mgr.model_ids(),
        TEST_MODELS
            .iter()
            .map(|(_, id, _)| id.to_string())
            .collect::<Vec<_>>()
    );

    // Embed through each model via the manager
    for (_, id, expected_dims) in TEST_MODELS {
        let req = serde_json::json!({
            "model": id,
            "input": "The quick brown fox jumps over the lazy dog."
        });
        let out = mgr.embed(id, &req).await.unwrap_or_else(|e| {
            panic!("manager embed failed for {id}: {e}");
        });

        assert_eq!(out["object"], "list");
        assert_eq!(out["model"], format!("embeddings-local/{id}"));
        assert_eq!(out["data"].as_array().unwrap().len(), 1);
        assert_eq!(out["dimensions"], *expected_dims);

        let embedding = &out["data"][0]["embedding"];
        assert!(embedding.is_array(), "{id}: embedding is not an array");
        assert_eq!(embedding.as_array().unwrap().len(), *expected_dims);

        eprintln!("✓ manager embed for {id} passed (dims={expected_dims})");
    }
}

/// Test that unknown model names are properly rejected.
#[tokio::test]
async fn real_unknown_model_rejected() {
    let backend = FastembedBackend;
    let err = match backend.load_model("NonExistentModel999").await {
        Ok(_) => panic!("should have failed for unknown model"),
        Err(e) => e,
    };
    match err {
        EmbedError::LoadFailed(model, msg) => {
            assert_eq!(model, "NonExistentModel999");
            assert!(
                msg.contains("unknown fastembed model variant"),
                "unexpected error: {msg}"
            );
        }
        other => panic!("unexpected error variant: {other}"),
    }
}
