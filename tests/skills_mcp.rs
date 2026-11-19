use aiproxy::{config::Config, server};
use serde_json::{Value, json};
const CFG: &str = "bind: 127.0.0.1:0
token: admin-secret
upstreams: [{kind: openai, models: [example]}]
skills: {read_token_env: PATH, author_token_env: HOME}
";

#[test]
fn skills_config_rejects_conflicting_credentials_and_paths() {
    for yaml in [
        CFG.replace("author_token_env: HOME", "author_token_env: PATH"),
        CFG.replace(
            "read_token_env: PATH",
            "read_token_env: AIPROXY_TEST_MISSING_SKILLS_TOKEN",
        ),
        format!(
            "{CFG}mcp: {{servers: [{{name: skills, command: unused}}]}}
"
        ),
        CFG.replace(
            "token: admin-secret
",
            "",
        ),
    ] {
        assert!(Config::from_yaml(&yaml).is_err());
    }
}

#[tokio::test]
async fn skills_mcp_wire_and_auth() {
    let dir = tempfile::tempdir().unwrap();
    let (listener, app) = server::build(
        Config::from_yaml(CFG).unwrap(),
        dir.path().join("config.yaml"),
    )
    .await
    .unwrap();
    let url = format!("http://{}/mcp/skills", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();
    let reader = std::env::var("PATH").unwrap();
    let author = std::env::var("HOME").unwrap();
    let send = |token: Option<&str>, method: &str, params: Value| {
        let r = http
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if let Some(token) = token {
            r.bearer_auth(token)
        } else {
            r
        }
    };
    let init = json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"probe","version":"1"}});
    for token in [None, Some("admin-secret"), Some("wrong")] {
        assert_eq!(
            send(token, "initialize", init.clone())
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    let r = send(Some(&reader), "initialize", init)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-store");
    assert!(r.text().await.unwrap().contains("list_skills"));
    let r = send(Some(&reader), "tools/list", json!({}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(r.contains("read_skill_file"), "{r}");
    let args = json!({"name":"save_skill_draft","arguments":{"name":"example","content":"---\nname: example\ndescription: Example\n---\nBODY-CANARY".replace("\n","
")}});
    let r = send(Some(&reader), "tools/call", args.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(r.contains("author credential required"), "{r}");
    let r = send(Some(&author), "tools/call", args)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        r.contains("draft_revision") && !r.contains("BODY-CANARY"),
        "{r}"
    );
    let r = send(
        Some(&reader),
        "tools/call",
        json!({"name":"list_skills","arguments":{}}),
    )
    .send()
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
    assert!(!r.contains("example") && !r.contains("BODY-CANARY"), "{r}");
    let admin = |body: Value| {
        http.post(url.replace("/mcp/skills", "/api/skills/example/actions"))
            .bearer_auth("admin-secret")
            .json(&body)
    };
    let snapshot: Value = admin(json!({"action":"save_text_draft","content":"---\nname: example\ndescription: Catalog only\n---\nBODY-CANARY","note":"NOTE-CANARY","expected_draft_revision":1}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let draft = snapshot["skills"]["example"]["draft"]["id"]
        .as_u64()
        .unwrap();
    let snapshot: Value = admin(json!({"action":"publish","draft_revision":draft}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let revision = snapshot["skills"]["example"]["published"]["id"]
        .as_u64()
        .unwrap();
    let enabled =
        |enabled| admin(json!({"action":"set_enabled","revision":revision,"enabled":enabled}));
    enabled(true)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let call = |name, arguments| {
        send(
            Some(&reader),
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        )
    };
    let catalog = call("list_skills", json!({}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(catalog.contains("Catalog only"), "{catalog}");
    assert!(
        !catalog.contains("BODY-CANARY") && !catalog.contains("NOTE-CANARY"),
        "{catalog}"
    );
    let load = call("load_skill", json!({"name":"example"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        load.contains("BODY-CANARY") && load.contains("NOTE-CANARY"),
        "{load}"
    );
    let bundle = || {
        http.get(url.replace(
            "/mcp/skills",
            &format!("/api/skills/example/bundle?revision={revision}"),
        ))
        .bearer_auth(&reader)
    };
    assert_eq!(bundle().send().await.unwrap().status(), 200);
    enabled(false)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(bundle().send().await.unwrap().status(), 404);
    admin(json!({"action":"remove","revision":revision}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(bundle().send().await.unwrap().status(), 404);
    server.abort();
}
