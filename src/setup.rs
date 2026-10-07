//! `/setup` page and the Codex device-code login API.
//!
//! The proxy owns the poll loop, so the page can be closed (or never opened
//! after the first request) and the flow still finishes: the browser only
//! starts a flow and reads its state.

use crate::api::AppState;
use crate::codex_oauth::{
    self, CodexEndpoints, CodexTokenManager, DEVICE_CODE_TIMEOUT_SECS, DeviceFlow,
    MIN_POLL_INTERVAL_MS, PollStatus, SLOW_DOWN_INCREMENT_MS,
};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// Login-flow state, keyed by provider id.
#[derive(Debug, Clone)]
pub enum CodexFlowState {
    Pending {
        user_code: String,
        verification_uri: String,
        interval_secs: u64,
        started_at_ms: u64,
        /// Distinguishes this flow from a later one for the same provider, so a
        /// stale poll loop can never overwrite a newer flow's state.
        flow_id: u64,
    },
    LoggedIn,
    Failed(String),
}

pub type CodexFlows = Arc<tokio::sync::Mutex<HashMap<String, CodexFlowState>>>;

/// Monotonic flow ids; see `CodexFlowState::Pending::flow_id`.
static FLOW_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_flow_id() -> u64 {
    FLOW_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

#[derive(Debug, Deserialize, Default)]
pub struct StartBody {
    pub provider: Option<String>,
    /// Force a brand-new device code even while another flow is pending (a lost
    /// or expired code — the page can't wait out the old flow).
    pub fresh: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
pub struct StatusQuery {
    pub provider: Option<String>,
    pub fresh: Option<bool>,
}

type ApiError = (StatusCode, Json<Value>);

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (
        status,
        Json(json!({"error": {"message": message.into(), "type": "invalid_request_error"}})),
    )
}

/// Resolve the targeted openai-codex upstream: explicit `provider`, else the
/// only configured one.
fn select_provider(
    state: &AppState,
    requested: Option<&str>,
) -> Result<(String, Arc<CodexTokenManager>), ApiError> {
    match requested {
        Some(id) => state
            .codex_managers
            .get(id)
            .cloned()
            .map(|m| (id.to_string(), m))
            .ok_or_else(|| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    format!("no openai-codex upstream named '{id}'"),
                )
            }),
        None => match state.codex_managers.len() {
            0 => Err(api_error(
                StatusCode::BAD_REQUEST,
                "no openai-codex upstream configured",
            )),
            1 => {
                let (id, manager) = state.codex_managers.iter().next().expect("len == 1");
                Ok((id.clone(), manager.clone()))
            }
            _ => Err(api_error(
                StatusCode::BAD_REQUEST,
                "multiple openai-codex upstreams configured — pass ?provider=<id>",
            )),
        },
    }
}

fn is_expired(flow: &CodexFlowState) -> bool {
    match flow {
        CodexFlowState::Pending { started_at_ms, .. } => {
            codex_oauth::now_ms().saturating_sub(*started_at_ms) >= DEVICE_CODE_TIMEOUT_SECS * 1000
        }
        _ => false,
    }
}

fn flow_json(provider: &str, flow: Option<&CodexFlowState>) -> Value {
    match flow {
        Some(CodexFlowState::Pending {
            user_code,
            verification_uri,
            interval_secs,
            ..
        }) => json!({
            "provider": provider,
            "state": "pending",
            "user_code": user_code,
            "verification_uri": verification_uri,
            "interval": interval_secs,
            "expires_in": DEVICE_CODE_TIMEOUT_SECS,
        }),
        Some(CodexFlowState::LoggedIn) => json!({"provider": provider, "state": "logged_in"}),
        Some(CodexFlowState::Failed(message)) => {
            json!({"provider": provider, "state": "failed", "message": message})
        }
        None => json!({"provider": provider, "state": "idle"}),
    }
}

/// `POST /api/codex/start` — begin (or re-read) a device-code login.
/// `provider` may come from the query string or the JSON body.
pub async fn codex_start(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
    body: Option<Json<StartBody>>,
) -> Result<Json<Value>, ApiError> {
    let fresh = query
        .fresh
        .or(body.as_ref().and_then(|Json(b)| b.fresh))
        .unwrap_or(false);
    let requested = query
        .provider
        .or_else(|| body.and_then(|Json(b)| b.provider));
    let (id, manager) = select_provider(&state, requested.as_deref())?;

    // Reuse a live flow: the page may be reloaded, or two tabs opened. `fresh`
    // forces a new code for one that was lost or has expired.
    if !fresh {
        let flows = state.codex_flows.lock().await;
        if let Some(existing) = flows
            .get(&id)
            .filter(|f| !is_expired(f) && !matches!(f, CodexFlowState::Failed(_)))
        {
            return Ok(Json(flow_json(&id, Some(existing))));
        }
    }

    let endpoints = CodexEndpoints::from_auth_base(&state.codex_auth_base);
    let client = crate::providers::default_http_client();
    let device = codex_oauth::start_device_flow(&client, &endpoints.user_code_url)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e.to_string()))?;

    let flow = CodexFlowState::Pending {
        user_code: device.user_code.clone(),
        verification_uri: endpoints.verification_uri.clone(),
        interval_secs: device.interval_secs,
        started_at_ms: codex_oauth::now_ms(),
        flow_id: next_flow_id(),
    };
    let response = flow_json(&id, Some(&flow));
    let flow_id = match &flow {
        CodexFlowState::Pending { flow_id, .. } => *flow_id,
        _ => unreachable!("just built a Pending flow"),
    };
    state.codex_flows.lock().await.insert(id.clone(), flow);

    spawn_poll_loop(
        state.codex_flows.clone(),
        id,
        flow_id,
        manager,
        endpoints,
        device,
        client,
    );
    Ok(Json(response))
}

/// `GET /api/codex/status` — flow state (falls back to credential state).
pub async fn codex_status(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> Result<Json<Value>, ApiError> {
    let (id, manager) = select_provider(&state, query.provider.as_deref())?;
    let flow = {
        let flows = state.codex_flows.lock().await;
        flows.get(&id).cloned()
    };
    let flow = match flow {
        Some(f) if is_expired(&f) => Some(CodexFlowState::Failed("Device flow timed out".into())),
        other => other,
    };
    let mut body = flow_json(&id, flow.as_ref());
    if !matches!(flow, Some(CodexFlowState::Pending { .. })) {
        // No live flow: report what the credentials actually say.
        match manager.status().await {
            codex_oauth::CodexStatus::LoggedIn { expires_at_ms } => {
                body["state"] = json!("logged_in");
                body["expires_at_ms"] = json!(expires_at_ms);
            }
            codex_oauth::CodexStatus::LoggedOut => {
                if body["state"] == json!("idle") {
                    body["state"] = json!("logged_out");
                }
            }
        }
    }
    Ok(Json(body))
}

/// Write this flow's result, but never over a newer flow's slot: a `fresh`
/// start (lost/expired code) owns the provider until it finishes.
async fn set_flow_state(flows: &CodexFlows, id: &str, flow_id: u64, state: CodexFlowState) {
    let mut flows = flows.lock().await;
    let current = match flows.get(id) {
        Some(CodexFlowState::Pending { flow_id, .. }) => Some(*flow_id),
        _ => None,
    };
    if current == Some(flow_id) {
        flows.insert(id.to_string(), state);
    }
}

/// Background poll loop: pending → slow_down → complete → exchange → store.
fn spawn_poll_loop(
    flows: CodexFlows,
    id: String,
    flow_id: u64,
    manager: Arc<CodexTokenManager>,
    endpoints: CodexEndpoints,
    device: DeviceFlow,
    client: reqwest::Client,
) {
    tokio::spawn(async move {
        let deadline = codex_oauth::now_ms() + DEVICE_CODE_TIMEOUT_SECS * 1000;
        let mut interval_ms = std::cmp::max(
            MIN_POLL_INTERVAL_MS,
            device.interval_secs.saturating_mul(1000),
        );
        let mut slow_downs = 0u32;
        loop {
            let now = codex_oauth::now_ms();
            if now >= deadline {
                let message = if slow_downs > 0 {
                    "Device flow timed out after one or more slow_down responses"
                } else {
                    "Device flow timed out"
                };
                set_flow_state(&flows, &id, flow_id, CodexFlowState::Failed(message.into())).await;
                return;
            }
            match codex_oauth::poll_device_flow(&client, &endpoints.device_token_url, &device).await
            {
                Ok(PollStatus::Pending) => {}
                Ok(PollStatus::SlowDown { interval_secs }) => {
                    slow_downs += 1;
                    interval_ms = match interval_secs {
                        Some(secs) if secs > 0 => {
                            std::cmp::max(MIN_POLL_INTERVAL_MS, secs.saturating_mul(1000))
                        }
                        _ => interval_ms + SLOW_DOWN_INCREMENT_MS,
                    };
                }
                Ok(PollStatus::Complete(credentials)) => {
                    let result = match codex_oauth::exchange_code(
                        &client,
                        manager.token_url(),
                        &credentials.authorization_code,
                        &credentials.code_verifier,
                        &endpoints.redirect_uri,
                    )
                    .await
                    {
                        Ok(tokens) => manager.store_tokens(tokens).await,
                        Err(e) => Err(e),
                    };
                    let state = match result {
                        Ok(()) => CodexFlowState::LoggedIn,
                        Err(e) => CodexFlowState::Failed(e.to_string()),
                    };
                    set_flow_state(&flows, &id, flow_id, state).await;
                    return;
                }
                Err(e) => {
                    set_flow_state(&flows, &id, flow_id, CodexFlowState::Failed(e.to_string()))
                        .await;
                    return;
                }
            }
            let remaining = deadline.saturating_sub(codex_oauth::now_ms());
            if remaining == 0 {
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms.min(remaining))).await;
        }
    });
}

/// `GET /setup` — ChatGPT (Codex) login page.
pub async fn setup_page() -> Html<&'static str> {
    Html(PAGE)
}

const PAGE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>aiproxy — Setup</title>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
         max-width: 720px; margin: 2rem auto; padding: 0 1rem; color: #1a1a1a; }
  h1 { font-size: 1.5rem; margin-bottom: 0.5rem; }
  p.sub { color: #666; margin-bottom: 1.5rem; font-size: 0.9rem; }
  .card { border: 1px solid #e5e7eb; border-radius: 8px; padding: 1rem; margin-bottom: 1rem; }
  .card h2 { font-size: 1.1rem; margin-bottom: 0.5rem; }
  .state { font-weight: 600; }
  .ok { color: #16a34a; }
  .wait { color: #d97706; }
  .bad { color: #dc2626; }
  code.user-code { display: inline-block; font-size: 1.6rem; font-weight: 700;
                   letter-spacing: 0.15em; padding: 0.4rem 0.8rem; background: #f3f4f6;
                   border-radius: 6px; margin: 0.5rem 0; }
  button { font-size: 1rem; padding: 0.5rem 1rem; border-radius: 6px; border: 1px solid #d1d5db;
           background: #111827; color: #fff; cursor: pointer; }
  button:disabled { opacity: 0.5; cursor: default; }
  a { color: #2563eb; }
  .msg { color: #666; font-size: 0.85rem; margin-top: 0.5rem; }
</style>
</head>
<body>
<h1>aiproxy Setup</h1>
<p class="sub">Connect a ChatGPT (Codex) subscription. The proxy polls for you — this page can be closed.</p>
<div id="cards">Loading...</div>
<script>
const provider = new URLSearchParams(location.search).get('provider') || '';
let timer = null;

function q() { return provider ? '?provider=' + encodeURIComponent(provider) : ''; }

async function post(path, body) {
  const r = await fetch(path + q(), {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body || {})
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error((data.error && data.error.message) || ('HTTP ' + r.status));
  return data;
}
async function get(path) {
  const r = await fetch(path + q());
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error((data.error && data.error.message) || ('HTTP ' + r.status));
  return data;
}

function render(s) {
  const el = document.getElementById('cards');
  const name = s.provider || 'openai-codex';
  let body;
  if (s.state === 'logged_in') {
    body = '<p class="state ok">Connected</p>' +
      '<p class="msg">Codex models are ready through this proxy.</p>';
    clearInterval(timer); timer = null;
  } else if (s.state === 'pending') {
    body = '<p class="state wait">Waiting for authorization</p>' +
      '<code class="user-code">' + s.user_code + '</code>' +
      '<p>Open <a href="' + s.verification_uri + '" target="_blank" rel="noreferrer">' + s.verification_uri + '</a> and enter the code above.</p>' +
      '<p class="msg">This page refreshes itself; you can close it once you have authorized.</p>' +
      '<button onclick="start(true)">New code</button>';
    if (!timer) timer = setInterval(refresh, 2000);
  } else if (s.state === 'failed') {
    body = '<p class="state bad">Login failed</p><p class="msg">' + (s.message || '') + '</p>' +
      '<button onclick="start(true)">Get a new code</button>';
    clearInterval(timer); timer = null;
  } else {
    body = '<p class="state">Not connected</p>' +
      '<button onclick="start()">Connect ChatGPT</button>' +
      '<p class="msg">Uses the Codex device-code flow; nothing is stored outside the proxy config directory.</p>';
    clearInterval(timer); timer = null;
  }
  el.innerHTML = '<div class="card"><h2>' + name + '</h2>' + body + '</div>';
}

async function refresh() {
  try { render(await get('/api/codex/status')); }
  catch (e) { document.getElementById('cards').innerHTML = '<div class="card"><p class="state bad">' + e.message + '</p></div>'; }
}
async function start(fresh) {
  try { render(await post('/api/codex/start', { fresh: !!fresh })); }
  catch (e) { document.getElementById('cards').innerHTML = '<div class="card"><p class="state bad">' + e.message + '</p></div>'; }
}
refresh();
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::provider::Provider;
    use crate::provider::testutil::MockProvider;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    async fn spawn(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// Mock auth server: usercode → /token pending once then complete → oauth/token.
    async fn spawn_auth_server(user_code_calls: Arc<AtomicUsize>) -> String {
        let poll_calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/api/accounts/deviceauth/usercode",
                post(move || {
                    let user_code_calls = user_code_calls.clone();
                    async move {
                        user_code_calls.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "device_auth_id": "dev_1",
                            "user_code": "WXYZ-1234",
                            "interval": "1"
                        }))
                    }
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
                                Json(
                                    json!({"error": {"code": "deviceauth_authorization_pending"}}),
                                ),
                            )
                                .into_response()
                        } else {
                            (
                                StatusCode::OK,
                                Json(json!({
                                    "authorization_code": "ac_1",
                                    "code_verifier": "cv_1"
                                })),
                            )
                                .into_response()
                        }
                    }
                }),
            )
            .route(
                "/oauth/token",
                post(|| async {
                    let payload = json!({
                        "https://api.openai.com/auth": {"chatgpt_account_id": "acct_1"}
                    });
                    let token =
                        format!("header.{}.sig", base64_url(payload.to_string().as_bytes()));
                    Json(json!({
                        "access_token": token,
                        "refresh_token": "rt_1",
                        "expires_in": 86_400
                    }))
                }),
            );
        spawn(app).await
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

    struct Env {
        _dir: tempfile::TempDir,
        state: AppState,
        flow: CodexFlows,
        manager: Arc<CodexTokenManager>,
    }

    async fn env(auth_base: &str, provider_id: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join(format!("{provider_id}-oauth-state.json"));
        let manager = Arc::new(CodexTokenManager::new(
            &state_path,
            &format!("{auth_base}/oauth/token"),
        ));
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(MockProvider::with_surface(
            provider_id,
            vec!["gpt-5.6-sol".into()],
            crate::provider::ModelSurface::Responses,
        ))];
        let registry = crate::discovery::ModelRegistry::new(providers);
        registry.refresh().await;
        let mut managers = HashMap::new();
        managers.insert(provider_id.to_string(), manager.clone());
        let flows: CodexFlows = Default::default();
        let state = AppState {
            registry: Arc::new(registry),
            embeddings: Arc::new(crate::embeddings::EmbeddingManager::new(
                &crate::config::EmbeddingsConfig::default(),
            )),
            token: None,
            subscriptions: Default::default(),
            usage: crate::usage::UsageTracker::new(),
            codex_managers: Arc::new(managers),
            codex_auth_base: auth_base.to_string(),
            codex_flows: flows.clone(),
        };
        Env {
            _dir: dir,
            state,
            flow: flows,
            manager,
        }
    }

    fn router(state: &AppState) -> Router {
        Router::new()
            .route("/setup", get(setup_page))
            .route("/api/codex/start", post(codex_start))
            .route("/api/codex/status", get(codex_status))
            .with_state(state.clone())
    }

    async fn body_json(resp: axum::response::Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn setup_page_is_html() {
        let e = env("http://127.0.0.1:1", "openai-codex").await;
        let resp = router(&e.state)
            .oneshot(
                Request::builder()
                    .uri("/setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("aiproxy Setup"), "got {html:.200}");
        assert!(
            html.contains("/api/codex/start"),
            "page must start the flow"
        );
    }

    #[tokio::test]
    async fn start_without_codex_upstream_is_400() {
        let mut e = env("http://127.0.0.1:1", "openai-codex").await;
        e.state.codex_managers = Arc::new(HashMap::new());
        let resp = router(&e.state)
            .oneshot(
                Request::builder()
                    .uri("/api/codex/start")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no openai-codex upstream"),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn start_with_unknown_provider_is_400() {
        let e = env("http://127.0.0.1:1", "openai-codex").await;
        let resp = router(&e.state)
            .oneshot(
                Request::builder()
                    .uri("/api/codex/start?provider=other")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]["message"].as_str().unwrap().contains("other"),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn start_reports_gateway_error_when_auth_server_fails() {
        let e = env("http://127.0.0.1:1", "openai-codex").await;
        let resp = router(&e.state)
            .oneshot(
                Request::builder()
                    .uri("/api/codex/start")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn status_without_flow_reports_logged_out() {
        let e = env("http://127.0.0.1:1", "openai-codex").await;
        let resp = router(&e.state)
            .oneshot(
                Request::builder()
                    .uri("/api/codex/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "logged_out");
    }

    #[tokio::test]
    async fn start_is_idempotent_while_pending() {
        let user_code_calls = Arc::new(AtomicUsize::new(0));
        let auth = spawn_auth_server(user_code_calls.clone()).await;
        let e = env(&auth, "openai-codex").await;
        let app = router(&e.state);

        let first = body_json(
            app.clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/codex/start")
                        .method("POST")
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        let second = body_json(
            app.clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/codex/start")
                        .method("POST")
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(first.0, StatusCode::OK);
        assert_eq!(first.1["state"], "pending");
        assert_eq!(first.1["user_code"], "WXYZ-1234");
        assert_eq!(second.1["user_code"], "WXYZ-1234");
        assert_eq!(
            user_code_calls.load(Ordering::SeqCst),
            1,
            "second start reuses the live flow"
        );
    }

    #[tokio::test]
    async fn fresh_start_issues_a_new_code_and_ignores_the_stale_flow() {
        let user_code_calls = Arc::new(AtomicUsize::new(0));
        let auth = spawn_auth_server(user_code_calls.clone()).await;
        let e = env(&auth, "openai-codex").await;
        let app = router(&e.state);

        let post_start = |uri: &'static str| {
            let app = app.clone();
            async move {
                body_json(
                    app.oneshot(
                        Request::builder()
                            .uri(uri)
                            .method("POST")
                            .header("content-type", "application/json")
                            .body(Body::from("{}"))
                            .unwrap(),
                    )
                    .await
                    .unwrap(),
                )
                .await
            }
        };

        let (_, first) = post_start("/api/codex/start").await;
        assert_eq!(first["state"], "pending");
        let first_flow_id = match e.flow.lock().await.get("openai-codex").cloned().unwrap() {
            CodexFlowState::Pending { flow_id, .. } => flow_id,
            other => panic!("expected pending, got {other:?}"),
        };

        let (status, second) = post_start("/api/codex/start?fresh=true").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(second["state"], "pending");
        assert_eq!(
            user_code_calls.load(Ordering::SeqCst),
            2,
            "fresh forces a new device code"
        );
        let second_flow_id = match e.flow.lock().await.get("openai-codex").cloned().unwrap() {
            CodexFlowState::Pending { flow_id, .. } => flow_id,
            other => panic!("expected pending, got {other:?}"),
        };
        assert_ne!(first_flow_id, second_flow_id);
    }

    #[tokio::test]
    async fn stale_poll_loop_cannot_clobber_a_newer_flow() {
        let user_code_calls = Arc::new(AtomicUsize::new(0));
        let auth = spawn_auth_server(user_code_calls).await;
        let e = env(&auth, "openai-codex").await;

        // A newer flow owns the slot …
        let newer = CodexFlowState::Pending {
            user_code: "NEW-CODE".into(),
            verification_uri: format!("{auth}/codex/device"),
            interval_secs: 60,
            started_at_ms: codex_oauth::now_ms(),
            flow_id: 9_999,
        };
        e.flow
            .lock()
            .await
            .insert("openai-codex".into(), newer.clone());

        // … and a stale loop's failure must not overwrite it.
        let flows = e.flow.clone();
        let stale = CodexFlowState::Failed("Device flow timed out".into());
        let current = match flows.lock().await.get("openai-codex") {
            Some(CodexFlowState::Pending { flow_id, .. }) => Some(*flow_id),
            _ => None,
        };
        assert_eq!(current, Some(9_999), "stale flow id 1 does not match");
        if current == Some(1) {
            flows.lock().await.insert("openai-codex".into(), stale);
        }
        let after = flows.lock().await.get("openai-codex").cloned().unwrap();
        assert!(
            matches!(after, CodexFlowState::Pending { flow_id, .. } if flow_id == 9_999),
            "newer flow survives, got {after:?}"
        );
    }

    #[tokio::test]
    async fn device_login_flow_completes_and_persists_tokens() {
        let user_code_calls = Arc::new(AtomicUsize::new(0));
        let auth = spawn_auth_server(user_code_calls).await;
        let e = env(&auth, "openai-codex").await;
        let app = router(&e.state);

        let (status, body) = body_json(
            app.clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/codex/start")
                        .method("POST")
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["verification_uri"], format!("{auth}/codex/device"));

        // proxy-side poll loop finishes on its own
        let mut final_state = String::new();
        for _ in 0..80 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let (_, body) = body_json(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/codex/status")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
            final_state = body["state"].as_str().unwrap_or_default().to_string();
            if final_state != "pending" {
                break;
            }
        }
        assert_eq!(final_state, "logged_in", "flow must complete proxy-side");

        // token file written, manager sees the credentials
        assert!(matches!(
            e.manager.status().await,
            codex_oauth::CodexStatus::LoggedIn { .. }
        ));
        assert!(e.manager.access().await.is_ok());
        assert_eq!(e.manager.account_id().await.unwrap(), "acct_1");
        let flow = e.flow.lock().await.get("openai-codex").cloned().unwrap();
        assert!(matches!(flow, CodexFlowState::LoggedIn), "got {flow:?}");
    }
}
