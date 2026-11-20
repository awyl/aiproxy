use axum::{Router, routing::get};
use serde_json::json;
use std::{path::Path, process::Output};

async fn fetch(body: String, path: &Path) -> Output {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/api/skills/example/bundle",
        get(
            move |headers: axum::http::HeaderMap,
                  axum::extract::RawQuery(query): axum::extract::RawQuery| async move {
                assert!(!headers.contains_key("authorization"));
                assert_eq!(query.as_deref(), Some("revision=7"));
                body
            },
        ),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_aiproxy"))
        .args([
            "skills",
            "fetch",
            "example",
            "--revision",
            "7",
            "--base-url",
            &url,
            "--output",
        ])
        .arg(path)
        .env_remove("AIPROXY_SKILLS_TOKEN")
        .output()
        .await
        .unwrap();
    task.abort();
    output
}
fn file(bytes: &[u8], executable: bool) -> aiproxy::skills::SkillFile {
    aiproxy::skills::SkillFile::new(bytes, executable, None)
}
#[tokio::test]
async fn skills_fetch_layout() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("bundle");
    let script = b"#!/bin/sh
touch ran
";
    let body = json!({"revision":7,"files":{"SKILL.md":file(b"instructions",false),"scripts/run.sh":file(script,true),"assets/data.bin":file(&[0,255],false)}}).to_string();
    let result = fetch(body.clone(), &out).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read(out.join("scripts/run.sh")).unwrap(), script);
    assert_eq!(
        std::fs::read(out.join("assets/data.bin")).unwrap(),
        [0, 255]
    );
    assert!(!out.join("ran").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            std::fs::metadata(out.join("scripts/run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o100,
            0
        );
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&out, &link).unwrap();
        assert!(!fetch(body.clone(), &link).await.status.success());
    }
    assert!(!fetch(body, &out).await.status.success());
    assert_eq!(std::fs::read(out.join("scripts/run.sh")).unwrap(), script);
}
#[tokio::test]
async fn skills_fetch_bad_paths() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("bundle");
    let item = serde_json::to_string(&file(b"fixture", false)).unwrap();
    for (rev, extra) in [
        (7, "../escape"),
        (7, "/absolute"),
        (7, "SKILL.md"),
        (7, "SKILL.md/child"),
        (8, "normal"),
    ] {
        let body =
            format!(r#"{{"revision":{rev},"files":{{"SKILL.md":{item},"{extra}":{item}}}}}"#);
        assert!(!fetch(body, &out).await.status.success(), "{rev}: {extra}");
        assert!(!out.exists(), "{rev}: {extra}");
        assert!(!dir.path().join("escape").exists());
    }
}
