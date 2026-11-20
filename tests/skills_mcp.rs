use aiproxy::{config::Config, server};
use serde_json::{Value, json};
const CFG: &str = "bind: 127.0.0.1:0
token: admin-secret
upstreams: [{kind: openai, models: [example]}]
";

#[test]
fn skills_mcp_name_is_reserved_without_opt_in() {
    assert!(
        Config::from_yaml(&format!(
            "{CFG}mcp: {{servers: [{{name: skills, command: unused}}]}}
"
        ))
        .unwrap_err()
        .to_string()
        .contains("MCP backend name 'skills' is reserved")
    );
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
    let issue_url = url.replace("/mcp/skills", "/api/skills/draft-token");
    let issue = || http.post(&issue_url).bearer_auth("admin-secret");
    let revoke = || http.delete(&issue_url).bearer_auth("admin-secret");
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
    let r = send(None, "initialize", init).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-store");
    assert!(r.text().await.unwrap().contains("list_skills"));
    let r = send(None, "tools/list", json!({}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(r.contains("read_skill_file"), "{r}");
    let args = json!({"name":"save_skill_draft","arguments":{"name":"example","content":"---\nname: example\ndescription: Example\n---\nBODY-CANARY".replace("\n","
")}});
    for token in [None, Some("admin-secret"), Some("wrong")] {
        let r = send(token, "tools/call", args.clone())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(r.contains("ask the user for a fresh draft token"), "{r}");
    }
    let denied = http.post(&issue_url).send().await.unwrap();
    assert_eq!(denied.status(), 401, "{}", denied.text().await.unwrap());
    let first = issue().send().await.unwrap().error_for_status().unwrap();
    assert_eq!(first.headers()["cache-control"], "no-store");
    let first: Value = first.json().await.unwrap();
    assert_eq!(first["expires_in_seconds"], 900);
    let author = first["token"].as_str().unwrap();
    assert_eq!(
        http.delete(&issue_url)
            .bearer_auth(author)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        http.get(url.replace("/mcp/skills", "/api/skills"))
            .bearer_auth(author)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let second: Value = issue()
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let next = second["token"].as_str().unwrap();
    assert_ne!(author, next);
    let stale = send(Some(author), "tools/call", args.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        stale.contains("ask the user for a fresh draft token"),
        "{stale}"
    );
    let r = send(Some(next), "tools/call", args.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    revoke().send().await.unwrap().error_for_status().unwrap();
    let revoked = send(Some(next), "tools/call", args)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        revoked.contains("ask the user for a fresh draft token"),
        "{revoked}"
    );
    assert!(
        r.contains("draft_revision") && !r.contains("BODY-CANARY"),
        "{r}"
    );
    let r = send(
        None,
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
            None,
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

#[tokio::test]
async fn skills_multiplexer_discovery_loading_and_draft_auth() {
    let dir = tempfile::tempdir().unwrap();
    let (listener, app) = server::build(
        Config::from_yaml(CFG).unwrap(),
        dir.path().join("config.yaml"),
    )
    .await
    .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();
    let rpc = |selection: &str, token: Option<&str>, method: &str, params: Value| {
        let mut request = http
            .post(format!("{base}/mcp"))
            .header("X-MCP-Servers", selection)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request
    };
    let tools: Value = rpc("missing,skills", None, "tools/list", json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "skills__list_skills",
            "skills__load_skill",
            "skills__read_skill_file",
            "skills__save_skill_draft"
        ]
    );
    let init: Value = rpc("skills", None, "initialize", json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("list_skills")
    );
    let hidden: Value = rpc("missing", None, "tools/list", json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hidden["result"]["tools"], json!([]));
    let blocked: Value = rpc(
        "missing",
        None,
        "tools/call",
        json!({"name":"skills__list_skills"}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert!(
        blocked["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no access")
    );
    let draft = json!({"name":"skills__save_skill_draft","arguments":{"name":"example","content":"---\nname: example\ndescription: Example\n---\nBODY-CANARY"}});
    for token in [None, Some("wrong"), Some("admin-secret")] {
        let denied: Value = rpc("skills", token, "tools/call", draft.clone())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(denied["result"]["isError"], true, "{denied}");
    }
    let grant: Value = http
        .post(format!("{base}/api/skills/draft-token"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = grant["token"].as_str().unwrap();
    let saved: Value = rpc(
        &format!("skills:{token}"),
        Some("admin-secret"),
        "tools/call",
        draft.clone(),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_ne!(saved["result"]["isError"], true, "{saved}");
    let saved_text = saved["result"]["content"][0]["text"].as_str().unwrap();
    let saved: Value = serde_json::from_str(saved_text).unwrap();
    let revision = saved["draft_revision"].as_u64().unwrap();
    let pending: Value = rpc(
        "skills",
        None,
        "tools/call",
        json!({"name":"skills__list_skills"}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(pending["result"]["content"][0]["text"], "[]");
    let published: Value = http
        .post(format!("{base}/api/skills/example/actions"))
        .bearer_auth("admin-secret")
        .json(&json!({"action":"publish","draft_revision":revision,"base_revision":null}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let revision = published["skills"]["example"]["published"]["id"]
        .as_u64()
        .unwrap();
    http.post(format!("{base}/api/skills/example/actions"))
        .bearer_auth("admin-secret")
        .json(&json!({"action":"set_enabled","revision":revision,"enabled":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let loaded: Value = rpc(
        "skills",
        None,
        "tools/call",
        json!({"name":"skills__load_skill","arguments":{"name":"example"}}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert!(
        loaded["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("BODY-CANARY"),
        "{loaded}"
    );
    http.post(format!("{base}/api/skills/example/actions"))
        .bearer_auth("admin-secret")
        .json(&json!({"action":"set_enabled","revision":revision,"enabled":false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let disabled: Value = rpc(
        "skills",
        None,
        "tools/call",
        json!({"name":"skills__load_skill","arguments":{"name":"example"}}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(disabled["result"]["isError"], true);
    http.delete(format!("{base}/api/skills/draft-token"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let revoked: Value = rpc("skills", Some(token), "tools/call", draft)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(revoked["result"]["isError"], true);
    task.abort();
}
