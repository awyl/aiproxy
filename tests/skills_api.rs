use aiproxy::{config::Config, server};
use serde_json::{Value, json};
#[tokio::test]
async fn skills_api_exact_review() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::from_yaml(
        "bind: 127.0.0.1:0
token: admin-secret
upstreams: [{kind: openai, models: [example]}]
",
    )
    .unwrap();
    let (listener, app) = server::build(cfg, dir.path().join("config.yaml"))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();
    let login = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{url}/dashboard/login"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("password=admin-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    for page in ["usage", "models", "aliases", "setup"] {
        let html = http
            .get(format!("{url}/{page}"))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            html.contains("href=\"/skills\""),
            "no Skills link on /{page}"
        );
    }
    let shell = http
        .get(format!("{url}/skills"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(shell.status(), 200);
    let get = || http.get(format!("{url}/api/skills"));
    assert_eq!(get().send().await.unwrap().status(), 401);
    assert_eq!(
        get()
            .bearer_auth(std::env::var("HOME").unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let send = |body: Value| {
        http.post(format!("{url}/api/skills/example/actions"))
            .bearer_auth("admin-secret")
            .json(&body)
    };
    let save = send(json!({"action":"save_text_draft","content":"---
name: example
description: Example
---
BODY-CANARY","note":"<img>"}))
    .send()
    .await
    .unwrap();
    assert_eq!(save.status(), 200);
    assert_eq!(save.headers()["cache-control"], "no-store");
    let snapshot: Value = save.json().await.unwrap();
    let id = snapshot["skills"]["example"]["draft"]["id"]
        .as_u64()
        .unwrap();
    assert_eq!(
        send(json!({"action":"publish","draft_revision":id+1}))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let published: Value = send(json!({"action":"publish","draft_revision":id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(published["skills"]["example"]["enabled"], false);
    assert_eq!(published["skills"]["example"]["published"]["note"], "<img>");
    task.abort();
}

#[tokio::test]
async fn skills_dashboard_without_global_token_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::from_yaml(
        "bind: 127.0.0.1:0
upstreams: [{kind: openai, models: [example]}]",
    )
    .unwrap();
    let (listener, app) = server::build(cfg, dir.path().join("config.yaml"))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();
    assert_eq!(
        http.get(format!("{url}/skills"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    let response = http
        .post(format!("{url}/api/skills/draft-token"))
        .bearer_auth("any-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("no global token configured")
    );
    task.abort();
}
