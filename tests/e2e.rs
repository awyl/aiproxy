//! Full-stack integration: real daemon (server::build) against a real mock
//! upstream. healthz, models catalog, chat streaming relay, auth rejection.

mod mock_upstream {
    use axum::Router;
    use axum::body::Body;
    use axum::http::HeaderValue;
    use axum::http::header;
    use axum::response::Response;
    use axum::routing::{get, post};
    use bytes::Bytes;

    pub const MODELS: &str = r#"{"object":"list","data":[{"id":"gpt-4o","object":"model","created":1720000000,"owned_by":"openai"}]}"#;

    /// Last chat/completions request headers captured from the daemon.
    pub static CAPTURED_HEADERS: std::sync::Mutex<Option<axum::http::HeaderMap>> =
        std::sync::Mutex::new(None);

    /// Serializes tests that touch the process-global capture.
    pub static TEST_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    pub fn app() -> Router {
        Router::new()
            .route("/v1/models", get(|| async { MODELS }))
            .route(
                "/v1/chat/completions",
                post(|headers: axum::http::HeaderMap| async move {
                    *CAPTURED_HEADERS.lock().unwrap() = Some(headers);
                    let body = Body::from(Bytes::from_static(
                        b"data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
                    ));
                    let mut resp = Response::new(body);
                    resp.headers_mut()
                        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
                    resp
                }),
            )
    }
}

use aiproxy::config::Config;
use aiproxy::server;
use axum::http::StatusCode;
use serde_json::{Value, json};

async fn spawn_daemon(tag: &str) -> (String, tokio::task::JoinHandle<()>) {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uaddr = upstream.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(upstream, mock_upstream::app()).await.unwrap() });

    // SAFETY: test-local env, unique name.
    unsafe { std::env::set_var("E2E_UPSTREAM_KEY", "e2e-upstream-key") };
    let yaml = format!(
        "bind: 127.0.0.1:0\ntoken: e2e-tok\nupstreams:\n  - name: mock\n    kind: openai\n    api_key_env: E2E_UPSTREAM_KEY\n    base_url: http://127.0.0.1:{port}/v1\n    discover: true\n",
        port = uaddr.port(),
    );
    let path = std::env::temp_dir().join(format!("aiproxy-e2e-{tag}.yaml"));
    std::fs::write(&path, yaml).unwrap();
    let cfg = Config::load(&path).unwrap();
    let (listener, router) = server::build(cfg, path.clone()).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), handle)
}

async fn get(url: &str, path: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut b = reqwest::Client::new().get(format!("{url}{path}"));
    if let Some(t) = token {
        b = b.bearer_auth(t);
    }
    let resp = b.send().await.unwrap();
    (resp.status(), resp.text().await.unwrap())
}

async fn post_json(
    url: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, String) {
    let mut b = reqwest::Client::new()
        .post(format!("{url}{path}"))
        .json(&body);
    if let Some(t) = token {
        b = b.bearer_auth(t);
    }
    let resp = b.send().await.unwrap();
    (resp.status(), resp.text().await.unwrap())
}

#[tokio::test]
async fn full_stack_health_models_chat_auth() {
    let _guard = mock_upstream::TEST_LOCK.lock().await;
    let (base, handle) = spawn_daemon("main").await;

    let (status, body) = get(&base, "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("ok"));

    let (status, body) = get(&base, "/v1/models", Some("e2e-tok")).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["data"][0]["id"], "openai/gpt-4o");

    let (status, body) = post_json(
        &base,
        "/v1/chat/completions",
        Some("e2e-tok"),
        json!({"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("data: {\"id\":\"c1\""));
    assert!(body.contains("data: [DONE]"));

    let (status, _) = post_json(
        &base,
        "/v1/chat/completions",
        None,
        json!({"model": "openai/gpt-4o", "messages": []}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    handle.abort();
}

#[tokio::test]
async fn client_headers_relayed_to_upstream() {
    let (base, handle) = spawn_daemon("hdrs").await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("e2e-tok")
        .header("x-opencode-session", "sess-e2e-1")
        .header("x-opencode-client", "pi")
        .header("authorization", "Bearer forged-by-client")
        .json(&json!({"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let captured = mock_upstream::CAPTURED_HEADERS
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    // Faithful relay: client headers reach the upstream verbatim...
    assert_eq!(
        captured.get("x-opencode-session").unwrap(),
        "sess-e2e-1",
        "x-opencode-session must not be stripped"
    );
    assert_eq!(captured.get("x-opencode-client").unwrap(), "pi");
    // ...but aiproxy-owned auth is the only copy sent.
    assert_eq!(
        captured.get("authorization").unwrap(),
        "Bearer e2e-upstream-key"
    );

    handle.abort();
}
