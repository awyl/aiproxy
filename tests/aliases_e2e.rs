//! Real local HTTP evidence: no vendor credentials, no external calls.
use aiproxy::{config::Config, server};
use axum::{
    Router,
    body::Bytes,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

async fn daemon(path: &std::path::Path) -> (String, tokio::task::JoinHandle<()>) {
    let (listener, app) = server::build(Config::load(path).unwrap(), path.to_path_buf())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn aliases_http_save_route_and_restart_preserve_bytes() {
    let captured = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let capture = captured.clone();
    let upstream = Router::new()
        .route(
            "/v1/models",
            get(|| async { axum::Json(json!({"data":[{"id":"m"}]})) }),
        )
        .route(
            "/v1/chat/completions",
            post(move |body: Bytes| {
                let capture = capture.clone();
                async move {
                    capture.lock().unwrap().push(body.to_vec());
                    (
                        [("content-type", "text/event-stream")],
                        "data: {\"ok\":true}\n\n",
                    )
                        .into_response()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("aiproxy.yaml");
    std::fs::write(&config, format!("bind: 127.0.0.1:0\ntoken: admin\nupstreams:\n  - kind: openai\n    base_url: http://{upstream_addr}/v1\n    discover: true\n")).unwrap();
    let client = reqwest::Client::new();
    let (base, task) = daemon(&config).await;

    let payload = json!({"aliases": {"default/fast": "openai/m"}});
    for token in ["wrong", ""] {
        let response = client
            .put(format!("{base}/api/aliases"))
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            401,
            "wrong auth must reject, not modify aliases"
        );
    }
    let response = client
        .put(format!("{base}/api/aliases"))
        .bearer_auth("admin")
        .json(&payload)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let raw = response.text().await.unwrap();
    println!("PUT /api/aliases {status}: {raw}");
    assert_eq!(status, 200, "{raw}");
    let saved: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(saved["aliases"][0]["id"], "default/fast");
    assert_eq!(saved["aliases"][0]["available"], true);
    assert!(
        saved["state_path"]
            .as_str()
            .unwrap()
            .ends_with("aiproxy-aliases.json")
    );
    let state_path = std::path::PathBuf::from(saved["state_path"].as_str().unwrap());

    let response = client
        .get(format!("{base}/v1/models"))
        .bearer_auth("admin")
        .send()
        .await
        .unwrap();
    let listing: Value = response.json().await.unwrap();
    let alias = listing["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "default/fast")
        .expect("alias advertised");
    assert_eq!(alias["target"], "openai/m");
    assert_eq!(alias["surface"], "chat");
    println!("GET /v1/models alias: {alias}");

    let body = "{ \"model\" : \"default/fast\", \"temperature\": 1.00, \"messages\":[{\"model\":\"leave-me\",\"content\":\"hi\"}] }";
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("admin")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "data: {\"ok\":true}\n\n");
    let seen = captured.lock().unwrap()[0].clone();
    let expected = "{ \"model\" : \"m\", \"temperature\": 1.00, \"messages\":[{\"model\":\"leave-me\",\"content\":\"hi\"}] }";
    assert_eq!(seen, expected.as_bytes());
    println!("upstream raw body: {}", String::from_utf8_lossy(&seen));

    for target in ["default/fast", "openai/missing"] {
        let response = client
            .put(format!("{base}/api/aliases"))
            .bearer_auth("admin")
            .json(&json!({"aliases":{"default/fast":target}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    task.abort();
    let (base, task) = daemon(&config).await;
    let response = client
        .get(format!("{base}/api/aliases"))
        .bearer_auth("admin")
        .send()
        .await
        .unwrap();
    let raw = response.text().await.unwrap();
    println!("GET /api/aliases after restart: {raw}");
    let restored: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(restored["aliases"][0]["target"], "openai/m");
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&state_path).unwrap()).unwrap(),
        payload["aliases"]
    );

    // A failed write returns 500 while the old alias remains routable.
    std::fs::remove_file(&state_path).unwrap();
    std::fs::create_dir(&state_path).unwrap();
    let response = client
        .put(format!("{base}/api/aliases"))
        .bearer_auth("admin")
        .json(&json!({"aliases":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    let current: Value = client
        .get(format!("{base}/api/aliases"))
        .bearer_auth("admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(current["aliases"][0]["target"], "openai/m");

    task.abort();
    upstream_task.abort();
}
