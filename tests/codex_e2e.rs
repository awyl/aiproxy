//! End-to-end Codex path: real daemon (server::build) + mock ChatGPT auth
//! server + mock Codex backend. Device-code login through /api/codex/*, then a
//! streamed /v1/responses call with `openai-codex/<model>`.

use aiproxy::config::Config;
use aiproxy::server;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const SSE: &str = "event: response.output_text.delta\ndata: {\"delta\":\"hel\"}\n\nevent: response.completed\ndata: {\"response\":{\"status\":\"completed\"}}\n\ndata: [DONE]\n\n";

#[derive(Default)]
struct Backend {
    calls: AtomicUsize,
    seen: Mutex<Vec<(std::collections::HashMap<String, String>, Value)>>,
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn base64_url(input: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(A[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(A[n as usize & 63] as char);
        }
    }
    out
}

fn access_token() -> String {
    let payload = json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct_e2e"}
    });
    format!(
        "e2e-header.{}.e2e-sig",
        base64_url(payload.to_string().as_bytes())
    )
}

/// Mock ChatGPT auth server: device code, device token, OAuth token.
async fn spawn_auth_server() -> String {
    let poll_calls = Arc::new(AtomicUsize::new(0));
    let token = access_token();
    let app = Router::new()
        .route(
            "/api/accounts/deviceauth/usercode",
            post(|| async {
                json_response(json!({
                    "device_auth_id": "dev_e2e",
                    "user_code": "E2E1-2345",
                    "interval": "1"
                }))
            }),
        )
        .route(
            "/api/accounts/deviceauth/token",
            post(move || {
                let poll_calls = poll_calls.clone();
                async move {
                    if poll_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            StatusCode::FORBIDDEN,
                            axum::Json(
                                json!({"error": {"code": "deviceauth_authorization_pending"}}),
                            ),
                        )
                            .into_response()
                    } else {
                        json_response(json!({
                            "authorization_code": "ac_e2e",
                            "code_verifier": "cv_e2e"
                        }))
                        .into_response()
                    }
                }
            }),
        )
        .route(
            "/oauth/token",
            post(move || {
                let token = token.clone();
                async move {
                    json_response(json!({
                        "access_token": token,
                        "refresh_token": "rt_e2e",
                        "expires_in": 86_400
                    }))
                }
            }),
        )
        .route("/codex/device", get(|| async { "device page" }));
    spawn(app).await
}

fn json_response(value: Value) -> axum::response::Response {
    (StatusCode::OK, axum::Json(value)).into_response()
}

/// Mock Codex backend at /codex/responses.
async fn spawn_codex_backend(backend: Arc<Backend>) -> String {
    let app = Router::new().route(
        "/codex/responses",
        post(move |headers: HeaderMap, body: String| {
            let backend = backend.clone();
            async move {
                backend.calls.fetch_add(1, Ordering::SeqCst);
                let seen_headers = headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                backend.seen.lock().unwrap().push((seen_headers, parsed));
                axum::response::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(SSE))
                    .unwrap()
            }
        }),
    );
    spawn(app).await
}

async fn wait_for_login(base: &str) {
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let resp = reqwest::get(format!("{base}/api/codex/status"))
            .await
            .expect("status request");
        let body: Value = resp.json().await.expect("status json");
        match body["state"].as_str() {
            Some("logged_in") => return,
            Some("failed") => panic!("login failed: {body}"),
            _ => {}
        }
    }
    panic!("device login did not complete");
}

/// Real daemon + mock ChatGPT auth server + mock Codex backend.
struct Stack {
    base: String,
    auth: String,
    backend: Arc<Backend>,
    _dir: tempfile::TempDir,
}

async fn spawn_stack() -> Stack {
    let auth = spawn_auth_server().await;
    let backend = Arc::new(Backend::default());
    let codex_base = spawn_codex_backend(backend.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("aiproxy.yaml");
    std::fs::write(
        &config_path,
        format!(
            "bind: 127.0.0.1:0\ntoken: e2e-tok\nupstreams:\n  - kind: openai-codex\n    models: [gpt-5.6-sol]\n    base_url: {codex_base}\n"
        ),
    )
    .unwrap();
    let cfg = Config::load(&config_path).unwrap();
    // Mock auth server + an OS-assigned loopback callback port, supplied
    // explicitly rather than through process-global env hooks.
    let options = server::CodexOptions {
        auth_base_url: auth.clone(),
        token_url: format!("{auth}/oauth/token"),
        callback_port: 0,
    };
    let (listener, router) = server::build_with_options(cfg, config_path.clone(), None, options)
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Stack {
        base: format!("http://{addr}"),
        auth,
        backend,
        _dir: dir,
    }
}

/// The upstream wire-shape assertions shared by both login flows.
fn assert_codex_wire(backend: &Backend) {
    let seen = backend.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (headers, body) = &seen[0];
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some(format!("Bearer {}", access_token()).as_str())
    );
    assert_eq!(
        headers.get("chatgpt-account-id").map(String::as_str),
        Some("acct_e2e")
    );
    assert_eq!(headers.get("originator").map(String::as_str), Some("pi"));
    assert_eq!(
        headers.get("openai-beta").map(String::as_str),
        Some("responses=experimental")
    );
    assert_eq!(
        headers.get("session-id").map(String::as_str),
        Some("e2e-session")
    );
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["instructions"], "You are a helpful assistant.");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["text"], json!({"verbosity": "low"}));
    assert!(body.get("max_output_tokens").is_none());
}

/// One streamed /v1/responses call, relayed verbatim.
async fn streamed_responses_call(base: &str) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .bearer_auth("e2e-tok")
        .json(&json!({
            "model": "openai-codex/gpt-5.6-sol",
            "input": [{"role": "user", "content": "hi"}],
            "max_output_tokens": 1234,
            "prompt_cache_key": "e2e-session"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    resp.text().await.unwrap()
}

#[tokio::test]
async fn codex_browser_login_then_responses_relay_end_to_end() {
    let Stack {
        base,
        auth: _auth,
        backend,
        _dir,
    } = spawn_stack().await;

    // pre-login: 502 with the /setup hint, and no upstream traffic
    let pre = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .bearer_auth("e2e-tok")
        .json(&json!({"model": "openai-codex/gpt-5.6-sol", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(pre.status(), 502);
    let pre_body: Value = pre.json().await.unwrap();
    assert!(
        pre_body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("/setup"),
        "got {pre_body}"
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);

    // browser login: the loopback callback finishes it
    let start: Value = reqwest::Client::new()
        .post(format!("{base}/api/codex/start"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(start["state"], "authorizing", "got {start}");
    assert_eq!(start["method"], "browser");
    assert_eq!(start["callback_listening"], true);
    let auth_url = start["auth_url"].as_str().unwrap();
    // authorize URL follows the (mocked) auth base and carries the reference
    // parameters
    assert!(auth_url.contains("/oauth/authorize?"), "got {auth_url}");
    for expected in [
        "response_type=code",
        "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
        "code_challenge_method=S256",
        "codex_cli_simplified_flow=true",
        "originator=pi",
    ] {
        assert!(
            auth_url.contains(expected),
            "missing {expected}: {auth_url}"
        );
    }
    let redirect_uri = start["redirect_uri"].as_str().unwrap().to_string();
    let state = auth_url
        .split("state=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .unwrap();

    let callback = reqwest::Client::new()
        .get(format!("{redirect_uri}?code=ac_e2e_browser&state={state}"))
        .send()
        .await
        .expect("loopback callback");
    assert_eq!(callback.status(), 200);
    assert!(
        callback.text().await.unwrap().contains("Login complete"),
        "callback must confirm the login"
    );

    let status: Value = reqwest::get(format!("{base}/api/codex/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["state"], "logged_in", "got {status}");

    // token file lives next to the config file, owner-only
    let state_path = _dir.path().join("openai-codex-oauth-state.json");
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(stored["refresh"], "rt_e2e");
    assert_eq!(stored["access"], access_token());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // the streamed relay, and the Codex wire shape the backend saw
    assert_eq!(streamed_responses_call(&base).await, SSE);
    assert_codex_wire(&backend);

    // chat surface stays closed for a codex model
    let chat = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("e2e-tok")
        .json(&json!({"model": "openai-codex/gpt-5.6-sol", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 400);
    let chat_body: Value = chat.json().await.unwrap();
    assert!(
        chat_body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Responses surface"),
        "got {chat_body}"
    );
    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        1,
        "no extra upstream calls"
    );
}

#[tokio::test]
async fn codex_device_login_then_responses_relay_end_to_end() {
    let Stack {
        base,
        auth,
        backend,
        _dir,
    } = spawn_stack().await;

    // catalog exposes the codex model on the responses surface
    let models: Value = reqwest::Client::new()
        .get(format!("{base}/v1/models"))
        .bearer_auth("e2e-tok")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "openai-codex/gpt-5.6-sol")
        .expect("codex model in catalog");
    assert_eq!(entry["surface"], "responses");

    // device-code fallback: the proxy polls, so this finishes on its own
    let start: Value = reqwest::Client::new()
        .post(format!("{base}/api/codex/start?method=device"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(start["state"], "pending", "got {start}");
    assert_eq!(start["user_code"], "E2E1-2345");
    assert_eq!(start["verification_uri"], format!("{auth}/codex/device"));
    wait_for_login(&base).await;

    assert_eq!(streamed_responses_call(&base).await, SSE);
    assert_codex_wire(&backend);
}
