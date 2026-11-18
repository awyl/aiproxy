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
    spawn_catalog_with(json!([
        {"id":"free","pricing":{"prompt":"0","completion":"0.000","request":"0"}},
        {"id":"paid","pricing":{"prompt":"0","completion":"0","request":"0.01"}},
        {"id":"unknown:free"},
        {"id":"dynamic","pricing":{"prompt":"-1","completion":"-1"}},
        {"id":"tiny","pricing":{"prompt":"1e-999","completion":"0"}},
        {"id":"bad","pricing":{"prompt":"0","completion":"0","image":"unknown"}}
    ]))
    .await
}

fn assert_isolated_runtime() {
    // Never let an integration test read or write the runtime state of a live proxy.
    assert!(
        std::env::var_os("AIPROXY_RUNTIME_DIR").is_none_or(|path| path.is_empty())
            && !std::path::Path::new("/runtime").is_dir(),
        "run this suite outside a proxy runtime directory"
    );
}

async fn spawn_catalog_with(models: Value) -> (String, Arc<AtomicBool>) {
    assert_isolated_runtime();
    let down = Arc::new(AtomicBool::new(false));
    let fail = down.clone();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = Router::new().route(
            "/models",
            get(move || {
                let fail = fail.clone();
                let models = models.clone();
                async move {
                    let status = if fail.load(Ordering::SeqCst) {
                        axum::http::StatusCode::BAD_GATEWAY
                    } else {
                        axum::http::StatusCode::OK
                    };
                    (status, Json(json!({"data": models})))
                }
            }),
        );
        axum::serve(listener, app).await.unwrap();
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

#[tokio::test]
async fn opencode_go_badges_zero_prices_without_hiding_subscription_models() {
    let (url, down) = spawn_catalog_with(json!([
        {"id":"promo","cost":{"text":{"input":{"price":0,"cache":{"read":0.0}},"output":{"price":0}}}},
        {"id":"included","go":{"monthly_usage":{"go":15}},"cost":{
            "text":{"input":{"price":3,"cache":{"read":0.3,"write":3}},"output":{"price":15}}
        }},
        {"id":"unknown-free","go":{"monthly_usage":null}},
        {"id":"missing-output","cost":{"text":{"input":{"price":0}}}},
        {"id":"billable-cache","cost":{"text":{"input":{"price":0,"cache":{"read":0,"write":0.01}},"output":{"price":0}}}},
        {"id":"billable-image","cost":{
            "text":{"input":{"price":0},"output":{"price":0}},"image":{"input":{"price":0.001}}
        }}
    ])).await;
    let dir = tempdir().unwrap();
    let cfg = Config::from_yaml(&format!(
        r#"
bind: 127.0.0.1:0
token: global
upstreams:
  - kind: opencode-go
    discover: true
    base_url: {url}
    surface_map_url: {url}/surfaces
"#
    ))
    .unwrap();
    let (_, app) = server::build_with_port(cfg, dir.path().join("aiproxy.yaml"), None)
        .await
        .unwrap();

    let (status, listed) = request(&app, "GET", "/v1/models", "global", None).await;
    assert_eq!(status, 200);
    let rows = listed["data"].as_array().unwrap();
    assert_eq!(rows.len(), 6, "free-only policy must not hide Go models");
    let flagged: Vec<_> = rows
        .iter()
        .filter(|m| m["free"] == true)
        .map(|m| &m["id"])
        .collect();
    assert_eq!(flagged, [&json!("opencode-go/promo")]);

    let (_, catalog) = request(&app, "GET", "/api/models", "", None).await;
    let rows = catalog["providers"][0]["models"].as_array().unwrap();
    assert!(rows.iter().all(|m| m["listed"] == true));
    assert_eq!(rows[0]["id"], "promo");
    assert_eq!(rows[0]["free"], true);

    down.store(true, Ordering::SeqCst);
    assert_eq!(request(&app, "POST", "/api/reload", "", None).await.0, 200);
    let (_, stale) = request(&app, "GET", "/v1/models", "global", None).await;
    let rows = stale["data"].as_array().unwrap();
    assert_eq!(rows.len(), 6, "retain the catalog if discovery fails");
    assert!(
        rows.iter().all(|m| m["free"].is_null()),
        "do not publish an unverified free badge"
    );
}

async fn policy_fixture() -> (String, Arc<std::sync::atomic::AtomicUsize>, Arc<AtomicBool>) {
    assert_isolated_runtime();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let down = Arc::new(AtomicBool::new(false));
    let app = Router::new()
        .route(
            "/models",
            get({
                let down = down.clone();
                move || {
                    let down = down.clone();
                    async move {
                        let status = if down.load(Ordering::SeqCst) {
                            502
                        } else {
                            200
                        };
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            Json(json!({"data": [
                                {"id": "free", "pricing": {"prompt": "0", "completion": "0"}},
                                {"id": "paid", "pricing": {"prompt": "1", "completion": "1"}},
                                {"id": "unknown:free"}
                            ]})),
                        )
                    }
                }
            }),
        )
        .route(
            "/chat/completions",
            axum::routing::post({
                let calls = calls.clone();
                move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            "data: [DONE]\n\n",
                        )
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, calls, down)
}

fn policy_yaml(url: &str, setting: bool) -> String {
    format!(
        r#"
bind: 127.0.0.1:0
token: global
free_models_only: {setting}
upstreams:
  - kind: openrouter
    name: a
    base_url: {url}
    discover: true
  - kind: openrouter
    name: b
    base_url: {url}
    models: [static-unknown]
  - kind: nvidia
    base_url: {url}
    models: [static-unknown]
  - kind: openai
    base_url: {url}
    discover: true
  - kind: opencode-go
    base_url: {url}
    surface_map_url: {url}/surfaces
    discover: true
"#
    )
}

async fn policy_aliases(app: &Router) {
    assert_eq!(
        request(
            app,
            "PUT",
            "/api/aliases",
            "global",
            Some(json!({"aliases": {
                "default/free": "openrouter=a/free",
                "default/paid": "openrouter=a/paid",
                "default/nvidia": "nvidia/static-unknown"
            }}))
        )
        .await
        .0,
        200
    );
}

#[tokio::test]
async fn free_only_refuses_direct_and_aliased_requests_on_every_surface_without_forwarding() {
    let (url, calls, _) = policy_fixture().await;
    let dir = tempdir().unwrap();
    let cfg = Config::from_yaml(&policy_yaml(&url, true)).unwrap();
    let (_, app) = server::build_with_port(cfg, dir.path().join("aiproxy.yaml"), None)
        .await
        .unwrap();
    policy_aliases(&app).await;
    for route in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for model in [
            "openrouter=a/paid",
            "openrouter=a/unknown:free",
            "openrouter=a/not-in-catalog",
            "openrouter=b/static-unknown",
            "nvidia/static-unknown",
            "nvidia/not-in-catalog",
            "default/paid",
            "default/nvidia",
        ] {
            let (status, body) = request(
                &app,
                "POST",
                route,
                "global",
                Some(json!({
                    "model": model, "messages": [], "input": "test", "max_tokens": 1
                })),
            )
            .await;
            assert_eq!(status, 403, "{route} {model}: {body}");
            assert_eq!(body["error"]["type"], "permission_error");
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("free_models_only")
            );
            if route == "/v1/messages" {
                assert_eq!(body["type"], "error");
            }
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn request_policy_follows_toggle_and_persisted_override_without_restricting_other_providers()
{
    let (url, calls, _) = policy_fixture().await;
    let dir = tempdir().unwrap();
    let path = dir.path().join("aiproxy.yaml");
    let cfg = Config::from_yaml(&policy_yaml(&url, true)).unwrap();
    let (_, app) = server::build_with_port(cfg.clone(), path.clone(), None)
        .await
        .unwrap();
    policy_aliases(&app).await;
    for model in [
        "openrouter=a/free",
        "default/free",
        "openai/paid",
        "opencode-go/paid",
    ] {
        let (status, body) = request(
            &app,
            "POST",
            "/v1/chat/completions",
            "global",
            Some(json!({"model": model})),
        )
        .await;
        assert_eq!(status, 200, "{model}: {body}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    for free_only in [false, true] {
        assert_eq!(
            request(
                &app,
                "PUT",
                "/api/models",
                "global",
                Some(json!({"free_models_only": free_only}))
            )
            .await
            .0,
            200
        );
        for model in [
            "openrouter=a/paid",
            "openrouter=a/not-in-catalog",
            "nvidia/static-unknown",
            "default/paid",
        ] {
            assert_eq!(
                request(
                    &app,
                    "POST",
                    "/v1/chat/completions",
                    "global",
                    Some(json!({"model": model}))
                )
                .await
                .0,
                if free_only { 403 } else { 200 }
            );
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    // The persisted ON override wins over a contrary YAML default after restart.
    let contrary_cfg = Config::from_yaml(&policy_yaml(&url, false)).unwrap();
    let (_, restarted) = server::build_with_port(contrary_cfg, path.clone(), None)
        .await
        .unwrap();
    assert_eq!(
        request(
            &restarted,
            "POST",
            "/v1/chat/completions",
            "global",
            Some(json!({"model": "default/paid"}))
        )
        .await
        .0,
        403
    );
    // The OFF override also survives restart.
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/models",
            "global",
            Some(json!({"free_models_only": false}))
        )
        .await
        .0,
        200
    );
    let (_, restarted) = server::build_with_port(cfg, path, None).await.unwrap();
    assert_eq!(
        request(
            &restarted,
            "POST",
            "/v1/chat/completions",
            "global",
            Some(json!({"model": "default/paid"}))
        )
        .await
        .0,
        200
    );
    assert_eq!(calls.load(Ordering::SeqCst), 9);
}

#[tokio::test]
async fn failed_discovery_revokes_inference_permission_and_authentication_stays_first() {
    let (url, calls, down) = policy_fixture().await;
    let dir = tempdir().unwrap();
    let cfg = Config::from_yaml(&policy_yaml(&url, true)).unwrap();
    let (_, app) = server::build_with_port(cfg, dir.path().join("aiproxy.yaml"), None)
        .await
        .unwrap();
    policy_aliases(&app).await;
    for model in ["openrouter=a/free", "default/free"] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/v1/chat/completions",
                "global",
                Some(json!({"model": model}))
            )
            .await
            .0,
            200
        );
    }
    down.store(true, Ordering::SeqCst);
    assert_eq!(request(&app, "POST", "/api/reload", "", None).await.0, 200);
    for model in ["openrouter=a/free", "default/free"] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/v1/chat/completions",
                "global",
                Some(json!({"model": model}))
            )
            .await
            .0,
            403
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/v1/chat/completions",
                "wrong",
                Some(json!({"model": model}))
            )
            .await
            .0,
            401
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    down.store(false, Ordering::SeqCst);
    assert_eq!(request(&app, "POST", "/api/reload", "", None).await.0, 200);
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/chat/completions",
            "global",
            Some(json!({"model": "default/free"}))
        )
        .await
        .0,
        200
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}
