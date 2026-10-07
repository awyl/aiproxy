//! Human-facing HTML pages served by the proxy.

use axum::response::Html;

use crate::api::AppState;
use axum::Json;
use axum::extract::State;

// ── Model discovery reload ──────────────────────────────────────────

/// The one shape both model endpoints answer with: every upstream with its
/// models (bare ids + surface) and the last probe's error, if any.
fn providers_json(snapshot: &[crate::discovery::ProviderCatalog]) -> serde_json::Value {
    serde_json::Value::Array(
        snapshot
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "count": p.models.len(),
                    "models": p.models.iter().map(|m| serde_json::json!({
                        "id": m.id,
                        "surface": m.surface.as_str(),
                    })).collect::<Vec<_>>(),
                    "error": p.error,
                })
            })
            .collect(),
    )
}

/// `GET /api/models` — the catalog as served, grouped by upstream, without
/// re-probing anything. `total` is what a client sees in `/v1/models`.
pub async fn models_api(State(state): State<AppState>) -> Json<serde_json::Value> {
    let snapshot = state.registry.catalog_snapshot();
    Json(serde_json::json!({
        "total": state.registry.models().len(),
        "providers": providers_json(&snapshot),
    }))
}

/// `POST /api/reload` — run one discovery round now and report per upstream.
/// Discovery normally runs once at startup (`model_refresh_secs: 0`), so an
/// upstream that was unreachable then — an unauthenticated Codex upstream, an
/// upstream down at boot — keeps an empty catalog until this is called.
pub async fn reload_models(State(state): State<AppState>) -> Json<serde_json::Value> {
    state.registry.refresh().await;
    let snapshot = state.registry.catalog_snapshot();
    let reloaded_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Json(serde_json::json!({
        "reloaded_at_ms": reloaded_at_ms,
        "total": state.registry.models().len(),
        "providers": providers_json(&snapshot),
    }))
}

// ── Usage page handler ──────────────────────────────────────────────

pub async fn usage_page() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>aiproxy — Usage</title>
<meta http-equiv="refresh" content="60">
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
         max-width: 800px; margin: 2rem auto; padding: 0 1rem; color: #1a1a1a; }
  h1 { font-size: 1.5rem; margin-bottom: 0.5rem; }
  p.sub { color: #666; margin-bottom: 1.5rem; font-size: 0.9rem; }
  .provider { border: 1px solid #e5e7eb; border-radius: 8px; padding: 1rem; margin-bottom: 1rem; }
  .provider h2 { font-size: 1.1rem; margin-bottom: 0.5rem; }
  .window { display: flex; justify-content: space-between; align-items: center;
            padding: 0.5rem 0; border-bottom: 1px solid #f3f4f6; }
  .window:last-child { border-bottom: none; }
  .window-label { font-weight: 600; }
  .window-pct { font-size: 1.2rem; font-weight: 700; }
  .window-reset { color: #666; font-size: 0.85rem; }
  .pct-high { color: #dc2626; }
  .pct-med { color: #d97706; }
  .pct-low { color: #16a34a; }
  .empty { color: #999; font-style: italic; padding: 2rem; text-align: center; }
  .updated { color: #999; font-size: 0.8rem; margin-top: 1rem; }
</style>
</head>
<body>
<h1>aiproxy Usage</h1>
<p class="sub">Per-provider billing window usage. Auto-refreshes every 60s.</p>
<div id="data">Loading...</div>
<div class="updated" id="updated"></div>
<script>
async function load() {
  try {
    const r = await fetch('/v1/usage');
    if (!r.ok) throw new Error('HTTP ' + r.status);
    const data = await r.json();
    const el = document.getElementById('data');
    if (!data.length) { el.innerHTML = '<div class="empty">No usage data yet. Make a request first.</div>'; return; }
    el.innerHTML = data.map(p => {
      const wins = (p.windows || []).map(w => {
        const pct = w.used_percent != null ? w.used_percent.toFixed(1) : '?';
        const cls = pct > 80 ? 'pct-high' : pct > 50 ? 'pct-med' : 'pct-low';
        const reset = w.reset_secs ? formatDur(w.reset_secs) : '';
        return `<div class="window"><span class="window-label">${w.label || 'unknown'}</span><span class="window-pct ${cls}">${pct}%</span><span class="window-reset">${reset ? 'resets in ' + reset : ''}</span></div>`;
      }).join('');
      return `<div class="provider"><h2>${p.provider}</h2>${wins || '<div class="empty">No windows</div>'}</div>`;
    }).join('');
    document.getElementById('updated').textContent = 'Updated: ' + new Date().toLocaleTimeString();
  } catch(e) {
    document.getElementById('data').innerHTML = '<div class="empty">Error: ' + e.message + '</div>';
  }
}
function formatDur(s) {
  const d = Math.floor(s/86400), h = Math.floor((s%86400)/3600), m = Math.floor((s%3600)/60);
  if (d>0) return h>0 ? d+'d '+h+'h' : d+'d';
  if (h>0) return m>0 ? h+'h '+m+'m' : h+'h';
  return m+'m';
}
load();
</script>
</body>
</html>"#,
    )
}

// ── Models page handler ─────────────────────────────────────────────

/// `GET /models` — every upstream with its models, one collapsible section each,
/// and a button to re-run discovery. Discovery runs once at startup by default
/// (`model_refresh_secs: 0`), so a provider that was unreachable then (an
/// unauthenticated Codex upstream, an upstream down at boot) keeps an empty
/// catalog until the button is pressed.
pub async fn models_page() -> Html<&'static str> {
    Html(include_str!("models_page.html"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use crate::provider::testutil::MockProvider;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::post;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn state(providers: Vec<Arc<dyn Provider>>) -> AppState {
        let registry = Arc::new(crate::discovery::ModelRegistry::new(providers));
        AppState {
            registry,
            embeddings: Arc::new(crate::embeddings::EmbeddingManager::new(
                &crate::config::EmbeddingsConfig::default(),
            )),
            token: None,
            subscriptions: Default::default(),
            usage: crate::usage::UsageTracker::new(),
            codex_managers: Arc::new(Default::default()),
            codex_auth_base: "http://127.0.0.1:1".into(),
            codex_flows: Default::default(),
        }
    }

    async fn post_reload(state: &AppState) -> (u16, serde_json::Value) {
        let app = Router::new()
            .route("/api/reload", post(reload_models))
            .with_state(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/reload")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn reload_reports_every_provider_and_its_models() {
        let state = state(vec![
            Arc::new(MockProvider::new(
                "openai",
                vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            )),
            Arc::new(MockProvider::failing("openai-codex")),
        ]);
        let (status, body) = post_reload(&state).await;
        assert_eq!(status, 200);
        assert_eq!(body["total"], 2, "got {body}");
        let providers = body["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 2);
        let codex = providers
            .iter()
            .find(|p| p["id"] == "openai-codex")
            .expect("failing provider must be listed");
        assert_eq!(codex["count"], 0);
        assert!(
            codex["error"]
                .as_str()
                .unwrap_or_default()
                .contains("mock failure"),
            "the failure must reach the page, not only the log: {codex}"
        );
        assert!(body["reloaded_at_ms"].as_u64().unwrap_or(0) > 0);
    }

    async fn get_models_api(state: &AppState) -> (u16, serde_json::Value) {
        let app = Router::new()
            .route("/api/models", axum::routing::get(models_api))
            .with_state(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn models_api_lists_every_provider_and_its_models() {
        let state = state(vec![
            Arc::new(MockProvider::with_surface(
                "openai",
                vec!["gpt-4o".into(), "gpt-4o-mini".into()],
                crate::provider::ModelSurface::ChatCompletions,
            )),
            Arc::new(MockProvider::failing("openai-codex")),
        ]);
        state.registry.refresh().await;
        let (status, body) = get_models_api(&state).await;
        assert_eq!(status, 200);
        assert_eq!(body["total"], 2);
        let providers = body["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 2, "the failing provider is listed too");
        let openai = providers.iter().find(|p| p["id"] == "openai").unwrap();
        assert_eq!(openai["count"], 2);
        assert_eq!(openai["models"][0]["id"], "gpt-4o");
        assert_eq!(
            openai["models"][0]["surface"], "chat",
            "the surface is what makes a missing model diagnosable: {openai}"
        );
        assert!(openai["error"].is_null());
        let codex = providers
            .iter()
            .find(|p| p["id"] == "openai-codex")
            .unwrap();
        assert_eq!(codex["count"], 0);
        assert!(
            codex["error"]
                .as_str()
                .unwrap_or_default()
                .contains("mock failure"),
            "got {codex}"
        );
    }

    #[tokio::test]
    async fn reload_answers_the_same_shape_as_the_models_api() {
        let state = state(vec![Arc::new(MockProvider::new(
            "openai",
            vec!["gpt-4o".into()],
        ))]);
        let (status, body) = post_reload(&state).await;
        assert_eq!(status, 200);
        assert!(body["reloaded_at_ms"].as_u64().unwrap_or(0) > 0);
        assert_eq!(body["providers"][0]["models"][0]["id"], "gpt-4o");
        assert_eq!(body["providers"][0]["count"], 1);
    }

    #[tokio::test]
    async fn models_page_toggles_providers_and_keeps_the_reload_button() {
        let html = models_page().await.0;
        assert!(html.contains("/api/models"), "the page reads the catalog");
        assert!(html.contains("/api/reload"), "and can re-run discovery");
        assert!(html.contains("Reload models"));
        assert!(
            html.contains("<details") && html.contains("<summary"),
            "a provider expands and collapses natively, without JS state"
        );
    }
}
