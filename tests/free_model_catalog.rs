use aiproxy::{config::Config, server};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::Request,
    routing::get,
};
use serde_json::{Value, json};
use tempfile::tempdir;
use tower::ServiceExt;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

async fn spawn_catalog() -> (String, Arc<AtomicBool>) {
    // Never let an integration test read or write the runtime state of a live proxy.
    assert!(
        std::env::var_os("AIPROXY_RUNTIME_DIR").is_none_or(|path| path.is_empty())
            && !std::path::Path::new("/runtime").is_dir(),
        "run this suite outside a proxy runtime directory"
    );
    let down = Arc::new(AtomicBool::new(false));
    let fail = down.clone();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/models", get(move || {
            let fail = fail.clone();
            async move {
            let status = if fail.load(Ordering::SeqCst) {
                axum::http::StatusCode::BAD_GATEWAY
            } else {
                axum::http::StatusCode::OK
            };
            (status, Json(json!({"data": [
                {"id":"free","pricing":{"prompt":"0","completion":"0.000","request":"0"}},
                {"id":"paid","pricing":{"prompt":"0","completion":"0","request":"0.01"}},
                {"id":"unknown:free"},
                {"id":"dynamic","pricing":{"prompt":"-1","completion":"-1"}},
                {"id":"tiny","pricing":{"prompt":"1e-999","completion":"0"}},
                {"id":"bad","pricing":{"prompt":"0","completion":"0","image":"unknown"}}
            ]})))
            }
        }))).await.unwrap();
    });
    (format!("http://{addr}"), down)
}

fn yaml(url: &str, override_line: &str) -> String {
    format!(
        r#"
bind: 127.0.0.1:0
token: global
{override_line}
upstreams:
  - kind: openrouter
    name: a
    token: limited
    base_url: {url}
    discover: true
  - kind: openrouter
    name: b
    models: [static-unknown]
  - kind: openai
    surface: chat
    models: [unaffected]
"#
    )
}

async fn request(
    app: &Router,
    method: &str,
    url: &str,
    token: &str,
    data: Option<Value>,
) -> (u16, Value) {
    let mut req = Request::builder().method(method).uri(url);
    if !token.is_empty() {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let body = match data {
        Some(value) => {
            req = req.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let code = res.status().as_u16();
    let bytes = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn listed(app: &Router) -> Vec<String> {
    let (code, body) = request(app, "GET", "/v1/models", "global", None).await;
    assert_eq!(code, 200);
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn default_hides_nonfree_and_unknown_without_touching_other_providers() {
    let (url, _) = spawn_catalog().await;
    let dir = tempdir().unwrap();
    let cfg = Config::from_yaml(&yaml(&url, "")).unwrap();
    let (_, app) = server::build_with_port(cfg, dir.path().join("aiproxy.yaml"), None)
        .await
        .unwrap();
    assert_eq!(
        listed(&app).await,
        ["openai/unaffected", "openrouter=a/free"]
    );

    let (code, _) = request(
        &app,
        "PUT",
        "/api/aliases",
        "global",
        Some(json!({
            "aliases": {"default/fast": "openrouter=a/free", "default/paid": "openrouter=a/paid"}
        })),
    )
    .await;
    assert_eq!(
        code, 200,
        "listing policy must not prevent alias management"
    );
    assert_eq!(
        listed(&app).await,
        ["openai/unaffected", "openrouter=a/free", "default/fast"]
    );
}

#[tokio::test]
async fn yaml_opt_out_lists_all_discovered_models() {
    let (url, _) = spawn_catalog().await;
    let dir = tempdir().unwrap();
    let cfg = Config::from_yaml(&yaml(&url, "free_models_only: false")).unwrap();
    let (_, app) = server::build_with_port(cfg, dir.path().join("aiproxy.yaml"), None)
        .await
        .unwrap();
    let models = listed(&app).await;
    assert_eq!(models.len(), 8);
    assert!(models.contains(&"openrouter=a/paid".into()));
    assert!(models.contains(&"openrouter=b/static-unknown".into()));
}

#[tokio::test]
async fn upstream_failure_revokes_free_claim_but_retains_raw_catalog() {
    let (url, down) = spawn_catalog().await;
    let dir = tempdir().unwrap();
    let cfg = Config::from_yaml(&yaml(&url, "")).unwrap();
    let (_, app) = server::build_with_port(cfg, dir.path().join("aiproxy.yaml"), None)
        .await
        .unwrap();
    assert!(listed(&app).await.contains(&"openrouter=a/free".into()));
    down.store(true, Ordering::SeqCst);
    assert_eq!(request(&app, "POST", "/api/reload", "", None).await.0, 200);
    assert_eq!(listed(&app).await, ["openai/unaffected"]);
    let (_, catalog) = request(&app, "GET", "/api/models", "", None).await;
    assert_eq!(catalog["providers"][0]["count"], 6);
}

#[tokio::test]
async fn toggle_requires_global_token_and_survives_restart() {
    let (url, _) = spawn_catalog().await;
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("aiproxy.yaml");
    let cfg = Config::from_yaml(&yaml(&url, "")).unwrap();
    let (_, app) = server::build_with_port(cfg.clone(), config_path.clone(), None)
        .await
        .unwrap();
    let change = Some(json!({"free_models_only": false}));
    assert_eq!(
        request(&app, "PUT", "/api/models", "", change.clone())
            .await
            .0,
        401
    );
    assert_eq!(
        request(&app, "PUT", "/api/models", "limited", change.clone())
            .await
            .0,
        401
    );
    assert_eq!(listed(&app).await.len(), 2);

    let (code, catalog) = request(&app, "PUT", "/api/models", "global", change).await;
    assert_eq!(code, 200);
    assert_eq!(catalog["free_models_only"], false);
    assert_eq!(catalog["total"], 8);
    assert_eq!(listed(&app).await.len(), 8);
    assert_eq!(
        serde_json::from_slice::<Value>(
            &std::fs::read(config_path.with_file_name("aiproxy-model-visibility.json")).unwrap()
        )
        .unwrap(),
        json!({"free_models_only": false})
    );

    let (_, app) = server::build_with_port(cfg, config_path, None)
        .await
        .unwrap();
    assert_eq!(listed(&app).await.len(), 8);
}

#[tokio::test]
async fn failed_save_leaves_the_live_policy_unchanged() {
    let (url, _) = spawn_catalog().await;
    let dir = tempdir().unwrap();
    let path = dir.path().join("aiproxy.yaml");
    let cfg = Config::from_yaml(&yaml(&url, "")).unwrap();
    let (_, app) = server::build_with_port(cfg, path.clone(), None)
        .await
        .unwrap();
    std::fs::create_dir(path.with_file_name("aiproxy-model-visibility.json")).unwrap();
    let (code, _) = request(
        &app,
        "PUT",
        "/api/models",
        "global",
        Some(json!({"free_models_only": false})),
    )
    .await;
    assert_eq!(code, 500);
    assert_eq!(listed(&app).await.len(), 2);
}
