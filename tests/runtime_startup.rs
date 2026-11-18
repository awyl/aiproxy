use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    routing::get,
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[tokio::test]
async fn startup_discovers_once_and_polls_usage_with_the_inference_key() {
    let probes = Arc::new(AtomicUsize::new(0));
    let (auth_tx, mut auth_rx) = tokio::sync::mpsc::unbounded_channel();
    let fixture = Router::new()
        .route(
            "/models",
            get({
                let probes = probes.clone();
                move || async move {
                    probes.fetch_add(1, Ordering::Relaxed);
                    Json(json!({"data": [{"id":"demo"}]}))
                }
            }),
        )
        .route(
            "/usage",
            get(move |headers: HeaderMap| {
                let auth_tx = auth_tx.clone();
                async move {
                    let correct = headers
                        .get("authorization")
                        .is_some_and(|h| h == "Bearer fixture-inference");
                    let _ = auth_tx.send(correct);
                    if correct {
                        (
                            StatusCode::OK,
                            Json(json!({"usage": {"rolling": {"percent": 12.5}}})),
                        )
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "expected inference key"})),
                        )
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aiproxy.yaml");
    std::fs::write(
        &path,
        format!(
            r#"
bind: 127.0.0.1:0
model_refresh_secs: 60
upstreams:
  - kind: opencode-go
    base_url: {url}
    surface_map_url: {url}/surfaces
    discover: true
    api_key_env: FIXTURE_INFERENCE
    token_env: FIXTURE_PROXY_SUBSCRIPTION
"#
        ),
    )
    .unwrap();
    let mut proxy = tokio::process::Command::new(env!("CARGO_BIN_EXE_aiproxy"))
        .arg("--config")
        .arg(path)
        .env("AIPROXY_RUNTIME_DIR", dir.path())
        .env("FIXTURE_INFERENCE", "fixture-inference")
        .env("FIXTURE_PROXY_SUBSCRIPTION", "fixture-proxy-subscription")
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let correct_key = tokio::time::timeout(Duration::from_secs(5), auth_rx.recv())
        .await
        .expect("startup usage poll")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    proxy.kill().await.unwrap();
    assert_eq!(
        (correct_key, probes.load(Ordering::Relaxed)),
        (true, 1),
        "expected inference credentials and one discovery round before the interval"
    );
}
