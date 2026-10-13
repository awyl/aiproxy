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

pub async fn usage_page() -> Html<String> {
    with_page_frame(
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
    const providers = data.filter(p => (p.windows || []).length > 0);
    if (!providers.length) { el.innerHTML = '<div class="empty">No usage data yet. Make a request first.</div>'; return; }
    el.innerHTML = providers.map(p => {
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
        "/usage",
    )
}

// ── Models page handler ─────────────────────────────────────────────

/// `GET /models` — every upstream with its models, one collapsible section each,
/// and a button to re-run discovery. Discovery runs once at startup by default
/// (`model_refresh_secs: 0`), so a provider that was unreachable then (an
/// unauthenticated Codex upstream, an upstream down at boot) keeps an empty
/// catalog until the button is pressed.
pub async fn models_page() -> Html<String> {
    with_page_frame(include_str!("models_page.html"), "/models")
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
    async fn usage_page_hides_providers_without_windows() {
        let html = usage_page().await.0;
        assert!(
            html.contains("data.filter(p => (p.windows || []).length > 0)"),
            "providers with no quota windows must not render"
        );
    }

    #[tokio::test]
    async fn all_pages_share_navigation_frame_and_links() {
        let pages = [
            ("usage", usage_page().await.0),
            ("models", models_page().await.0),
            ("setup", crate::setup::setup_page().await.0),
        ];
        for (name, html) in pages {
            assert!(html.contains("app-frame"), "{name} missing frame");
            assert!(html.contains("nav-toggle"), "{name} missing menu toggle");
            let nav = html
                .split(r#"<nav aria-label="Pages">"#)
                .nth(1)
                .unwrap_or_default()
                .split("</nav>")
                .next()
                .unwrap_or_default();
            for href in ["/usage", "/models", "/setup"] {
                assert!(nav.contains(href), "{name} navigation missing {href} link");
            }
            assert!(
                nav.contains("aria-current"),
                "{name} missing active page state"
            );
        }
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

const PAGE_FRAME_STYLE: &str = r#"<style>
  body { max-width: none !important; margin: 0 !important; padding: 0 !important; min-height: 100vh; }
  .app-frame { display: flex; min-height: 100vh; }
  .sidebar { flex: 0 0 240px; padding: 1.25rem 1rem; background: #f8fafc; border-right: 1px solid #e5e7eb; }
  .brand { display: block; margin-bottom: 1.5rem; color: #111827; font-size: 1.1rem; font-weight: 700; text-decoration: none; }
  .sidebar nav { display: grid; gap: 0.35rem; }
  .sidebar nav a { padding: 0.65rem 0.75rem; border-radius: 6px; color: #374151; text-decoration: none; }
  .sidebar nav a:hover { background: #eef2f7; }
  .sidebar nav a[aria-current="page"] { background: #e5e7eb; color: #111827; font-weight: 600; }
  .page-main { flex: 1; min-width: 0; }
  .topbar { display: flex; align-items: center; gap: 0.75rem; height: 3.5rem; padding: 0 1rem; border-bottom: 1px solid #e5e7eb; }
  .nav-toggle { width: 2.5rem; height: 2.5rem; padding: 0; border: 1px solid #d1d5db; border-radius: 6px; background: white; color: #111827; cursor: pointer; font-size: 1.2rem; }
  .page-content { max-width: 52rem; margin: 1.5rem auto; padding: 0 1rem 2rem; }
  body.nav-collapsed .sidebar { display: none; }
  @media (max-width: 700px) {
    .sidebar { position: fixed; z-index: 2; inset: 0 auto 0 0; width: min(80vw, 260px); overflow-y: auto; visibility: hidden; transform: translateX(-100%); transition: transform 160ms ease; box-shadow: 0 0 1rem #0002; }
    body.nav-open .sidebar { visibility: visible; transform: translateX(0); }
  }
</style>"#;

const PAGE_FRAME_SCRIPT: &str = r#"<script>
  const navButton = document.getElementById('nav-toggle');
  const isMobileNav = () => window.matchMedia('(max-width: 700px)').matches;
  function setNavOpen(open) {
    document.body.classList.toggle('nav-open', isMobileNav() && open);
    document.body.classList.toggle('nav-collapsed', !isMobileNav() && !open);
    navButton.setAttribute('aria-expanded', String(open));
  }
  setNavOpen(!isMobileNav());
  navButton.addEventListener('click', () => {
    const isOpen = isMobileNav()
      ? document.body.classList.contains('nav-open')
      : !document.body.classList.contains('nav-collapsed');
    setNavOpen(!isOpen);
  });
  window.addEventListener('resize', () => setNavOpen(!isMobileNav()));
</script>"#;

pub(crate) fn with_page_frame(html: &'static str, active_path: &str) -> Html<String> {
    let nav_links = [
        ("/usage", "Usage"),
        ("/models", "Models"),
        ("/setup", "Setup"),
    ]
    .iter()
    .map(|(path, label)| {
        let current = if *path == active_path {
            " aria-current=\"page\""
        } else {
            ""
        };
        format!("<a href=\"{path}\"{current}>{label}</a>")
    })
    .collect::<String>();
    let html = html.replace("</head>", &format!("{PAGE_FRAME_STYLE}</head>"));
    let html = html.replace(
        "<body>",
        &format!(
            r#"<body><div class="app-frame"><aside class="sidebar" id="site-nav"><a class="brand" href="/usage">aiproxy</a><nav aria-label="Pages">{nav_links}</nav></aside><div class="page-main"><header class="topbar"><button class="nav-toggle" id="nav-toggle" type="button" aria-label="Toggle navigation" aria-controls="site-nav" aria-expanded="true">☰</button><span>aiproxy</span></header><main class="page-content">"#
        ),
    );
    Html(html.replace(
        "</body>",
        &format!("</main></div></div>{PAGE_FRAME_SCRIPT}</body>"),
    ))
}
