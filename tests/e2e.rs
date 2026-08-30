//! Full-stack integration: real daemon (server::build) against a real mock
//! upstream. healthz, models catalog, chat streaming relay, auth rejection.

mod mock_upstream {
    use axum::body::Body;
    use axum::http::header;
    use axum::http::HeaderValue;
    use axum::response::Response;
    use axum::routing::{get, post};
    use axum::Router;
    use bytes::Bytes;

    pub const MODELS: &str = r#"{"object":"list","data":[{"id":"gpt-4o","object":"model","created":1720000000,"owned_by":"openai"}]}"#;

    pub fn app() -> Router {
        Router::new()
            .route("/v1/models", get(|| async { MODELS }))
            .route(
                "/v1/chat/completions",
                post(|| async {
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
use serde_json::{json, Value};

async fn spawn_daemon() -> (String, tokio::task::JoinHandle<()>) {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uaddr = upstream.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(upstream, mock_upstream::app()).await.unwrap() });

    let yaml = format!(
        "port: 0\ntoken: e2e-tok\nupstreams:\n  - name: mock\n    kind: openai\n    base_url: http://127.0.0.1:{port}/v1\n    discover: true\n",
        port = uaddr.port(),
    );
    let path = std::env::temp_dir().join(format!("aiproxy-e2e-{}.yaml", std::process::id()));
    std::fs::write(&path, yaml).unwrap();
    let cfg = Config::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let (listener, router) = server::build(cfg).await.unwrap();
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
    let mut b = reqwest::Client::new().post(format!("{url}{path}")).json(&body);
    if let Some(t) = token {
        b = b.bearer_auth(t);
    }
    let resp = b.send().await.unwrap();
    (resp.status(), resp.text().await.unwrap())
}

#[tokio::test]
async fn full_stack_health_models_chat_auth() {
    let (base, handle) = spawn_daemon().await;

    let (status, body) = get(&base, "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("ok"));

    let (status, body) = get(&base, "/v1/models", Some("e2e-tok")).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["data"][0]["id"], "mock/gpt-4o");

    let (status, body) = post_json(
        &base,
        "/v1/chat/completions",
        Some("e2e-tok"),
        json!({"model": "mock/gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("data: {\"id\":\"c1\""));
    assert!(body.contains("data: [DONE]"));

    let (status, _) = post_json(
        &base,
        "/v1/chat/completions",
        None,
        json!({"model": "mock/gpt-4o", "messages": []}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    handle.abort();
}