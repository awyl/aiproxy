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
    let mut args = json!({"name":"save_skill_draft","arguments":{"name":"example","content":"---\nname: example\ndescription: Example\n---\nBODY-CANARY"}});
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
    args["arguments"]["draft_token"] = json!(author);
    let stale = send(None, "tools/call", args.clone())
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
    args["arguments"]["draft_token"] = json!(next);
    let r = send(None, "tools/call", args.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    revoke().send().await.unwrap().error_for_status().unwrap();
    let revoked = send(None, "tools/call", args)
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
            "skills__read_skill_draft",
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
    let mut draft = json!({"name":"skills__save_skill_draft","arguments":{"name":"example","content":"---\nname: example\ndescription: Example\n---\nBODY-CANARY"}});
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
    draft["arguments"]["draft_token"] = json!(token);
    let saved: Value = rpc("skills", None, "tools/call", draft.clone())
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
    let revoked: Value = rpc("skills", None, "tools/call", draft)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(revoked["result"]["isError"], true);
    task.abort();
}

#[tokio::test]
async fn draft_argument_creates_and_edits_through_both_mcp_routes() {
    for (path, tool) in [
        ("/mcp/skills", "save_skill_draft"),
        ("/mcp", "skills__save_skill_draft"),
    ] {
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
        let call = |arguments: Value| {
            http.post(format!("{base}{path}"))
            .header("Accept","application/json, text/event-stream")
            .header("X-MCP-Servers","skills")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}}))
        };
        let mut args = json!({"draft_token":token,"name":"example","content":"---\nname: example\ndescription: Example\n---\nFIRST","base_revision":null,"expected_draft_revision":null});
        let saved: Value = call(args.clone())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(saved["result"]["isError"], true, "{path}: {saved}");
        assert!(!saved.to_string().contains(token));
        let saved: Value =
            serde_json::from_str(saved["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let draft = saved["draft_revision"].as_u64().unwrap();
        let read_tool = if path == "/mcp" {
            "skills__read_skill_draft"
        } else {
            "read_skill_draft"
        };
        let read = |credential: Option<&str>, name: &str| {
            let mut arguments = json!({"name":name});
            if let Some(token) = credential {
                arguments["draft_token"] = json!(token);
            }
            http.post(format!("{base}{path}"))
                .header("Accept","application/json, text/event-stream").header("X-MCP-Servers","skills")
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":read_tool,"arguments":arguments}}))
        };
        for credential in [None, Some("wrong"), Some("admin-secret")] {
            let denied: Value = read(credential, "example")
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(denied["result"]["isError"], true, "{denied}");
            assert!(!denied.to_string().contains("FIRST"), "{denied}");
        }
        let loaded: Value = read(Some(token), "example")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(loaded["result"]["isError"], true, "{loaded}");
        assert!(!loaded.to_string().contains(token));
        let loaded: Value =
            serde_json::from_str(loaded["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(loaded["content"], args["content"]);
        assert_eq!(loaded["name"], "example");
        assert_eq!(loaded["draft_revision"], draft);
        assert_eq!(loaded["base_revision"], Value::Null);
        let missing: Value = read(Some(token), "missing")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(missing["result"]["isError"], true, "{missing}");
        args["expected_draft_revision"] = loaded["draft_revision"].clone();
        // An intervening save makes the previously read draft revision stale.
        let concurrent: Value = call(args.clone())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(concurrent["result"]["isError"], true, "{concurrent}");
        let stale: Value = call(args.clone())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stale["result"]["isError"], true, "{stale}");
        let latest: Value = read(Some(token), "example")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let latest: Value =
            serde_json::from_str(latest["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let draft = latest["draft_revision"].as_u64().unwrap();
        args["expected_draft_revision"] = Value::Null;
        args["content"] = json!("---\nname: example\ndescription: Example\n---\nEDITED");
        // Missing the expected draft revision must not overwrite the open draft.
        let conflict: Value = call(args.clone())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(conflict["result"]["isError"], true, "{conflict}");
        args["expected_draft_revision"] = json!(draft);
        let edited: Value = call(args.clone())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(edited["result"]["isError"], true, "{edited}");
        let edited: Value =
            serde_json::from_str(edited["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let edited_draft = edited["draft_revision"].as_u64().unwrap();
        assert_ne!(edited_draft, draft);
        let loaded: Value = read(Some(token), "example")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let loaded: Value =
            serde_json::from_str(loaded["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(loaded["content"], args["content"]);
        assert_eq!(loaded["draft_revision"], edited_draft);
        let admin = || {
            http.get(format!("{base}/api/skills"))
                .bearer_auth("admin-secret")
        };
        let state: Value = admin().send().await.unwrap().json().await.unwrap();
        assert_eq!(state["skills"]["example"]["draft"]["id"], edited_draft);
        assert_eq!(
            state["skills"]["example"]["draft"]["files"]["SKILL.md"]["data"],
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                "---\nname: example\ndescription: Example\n---\nEDITED"
            )
        );
        assert!(state["skills"]["example"]["published"].is_null());
        assert_eq!(state["skills"]["example"]["enabled"], false);
        assert!(!state.to_string().contains(token));
        let published: Value = http
            .post(format!("{base}/api/skills/example/actions"))
            .bearer_auth("admin-secret")
            .json(&json!({"action":"publish","draft_revision":edited_draft,"base_revision":null}))
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
        let no_draft: Value = read(Some(token), "example")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(no_draft["result"]["isError"], true, "{no_draft}");
        assert!(!no_draft.to_string().contains("EDITED"));
        args["expected_draft_revision"] = Value::Null;
        args["base_revision"] = json!(revision + 1);
        let stale: Value = call(args.clone())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stale["result"]["isError"], true, "{stale}");
        args["base_revision"] = json!(revision);
        args["content"] = json!("---\nname: example\ndescription: Example\n---\nNEXT");
        let next: Value = call(args.clone())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(next["result"]["isError"], true, "{next}");
        let loaded: Value = read(Some(token), "example")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let loaded: Value =
            serde_json::from_str(loaded["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(loaded["base_revision"], revision);
        assert_eq!(loaded["content"], args["content"]);
        let state: Value = admin().send().await.unwrap().json().await.unwrap();
        assert_eq!(state["skills"]["example"]["published"]["id"], revision);
        assert_eq!(
            state["skills"]["example"]["draft"]["files"]["SKILL.md"]["data"],
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                "---\nname: example\ndescription: Example\n---\nNEXT"
            )
        );
        assert!(state["skills"]["example"]["draft"]["id"].as_u64().is_some());
        http.post(format!("{base}/api/skills/example/actions"))
            .bearer_auth("admin-secret")
            .json(&json!({"action":"set_enabled","revision":revision,"enabled":false}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let disabled: Value = read(Some(token), "example")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(disabled["result"]["isError"], true, "{disabled}");
        http.post(format!("{base}/api/skills/example/actions"))
            .bearer_auth("admin-secret")
            .json(&json!({"action":"set_enabled","revision":revision,"enabled":true}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let state_path = dir.path().join("aiproxy-skills.json");
        assert!(!std::fs::read_to_string(state_path).unwrap().contains(token));
        // A valid argument is authoritative even if a client still has old credentials.
        let next: Value =
            serde_json::from_str(next["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        args["expected_draft_revision"] = next["draft_revision"].clone();
        args["draft_token"] = json!("wrong");
        let denied: Value = call(args.clone())
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(denied["result"]["isError"], true, "{denied}");
        assert!(denied.to_string().contains("draft_token"), "{denied}");
        for invalid in ["", "admin-secret", "wrong"] {
            args["draft_token"] = json!(invalid);
            let denied: Value = call(args.clone())
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(denied["result"]["isError"], true, "{denied}");
        }
        args["draft_token"] = json!(token);
        let rotated: Value = http
            .post(format!("{base}/api/skills/draft-token"))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let denied: Value = read(Some(token), "example")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(denied["result"]["isError"], true, "{denied}");
        let new_token = rotated["token"].as_str().unwrap();
        let fresh: Value = read(Some(new_token), "example")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(fresh["result"]["isError"], true, "{fresh}");
        http.delete(format!("{base}/api/skills/draft-token"))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let denied: Value = read(Some(new_token), "example")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(denied["result"]["isError"], true, "{denied}");
        assert!(!denied.to_string().contains("NEXT"));
        let revoked: Value = call(args).send().await.unwrap().json().await.unwrap();
        assert_eq!(revoked["result"]["isError"], true, "{revoked}");
        assert!(!revoked.to_string().contains(token));
        task.abort();
    }
}

#[tokio::test]
async fn legacy_transport_credentials_do_not_authorize_skill_drafts() {
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
    for (path, tool, selection) in [
        ("/mcp/skills", "save_skill_draft", "skills".to_string()),
        ("/mcp", "skills__save_skill_draft", "skills".to_string()),
        (
            "/mcp",
            "skills__save_skill_draft",
            format!("skills:{token}"),
        ),
    ] {
        let denied:Value=http.post(format!("{base}{path}")).header("Accept","application/json, text/event-stream")
            .header("X-MCP-Servers",selection).bearer_auth(token)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":{"name":"example","content":"---\nname: example\ndescription: Example\n---\nBODY"}}}))
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(denied["result"]["isError"], true, "{path}: {denied}");
        assert!(denied.to_string().contains("draft_token"), "{denied}");
        assert!(!denied.to_string().contains(token));
    }
    let state: Value = http
        .get(format!("{base}/api/skills"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["skills"], json!({}));
    task.abort();
}
