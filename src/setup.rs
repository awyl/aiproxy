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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexFlowState {
    /// Device-code flow: waiting for the user to enter `user_code`.
    Pending {
        user_code: String,
        verification_uri: String,
        interval_secs: u64,
        started_at_ms: u64,
        /// Distinguishes this flow from a later one for the same provider, so a
        /// stale poll loop can never overwrite a newer flow's state.
        flow_id: u64,
    },
    /// Browser (PKCE) flow: waiting for the loopback callback or a pasted code.
    Authorizing {
        auth_url: String,
        redirect_uri: String,
        verifier: String,
        state: String,
        /// Whether the loopback callback listener bound (port 1455 is shared
        /// with the Codex CLI; when it is taken, paste-back still works).
        callback_listening: bool,
        started_at_ms: u64,
        flow_id: u64,
    },
    LoggedIn,
    Failed(String),
}

impl CodexFlowState {
    fn flow_id(&self) -> Option<u64> {
        match self {
            CodexFlowState::Pending { flow_id, .. }
            | CodexFlowState::Authorizing { flow_id, .. } => Some(*flow_id),
            _ => None,
        }
    }

    /// Which login method this flow belongs to — `start` reuses a live flow
    /// only when it matches what the caller asked for.
    fn method(&self) -> Option<LoginMethod> {
        match self {
            CodexFlowState::Pending { .. } => Some(LoginMethod::Device),
            CodexFlowState::Authorizing { .. } => Some(LoginMethod::Browser),
            _ => None,
        }
    }

    fn started_at_ms(&self) -> Option<u64> {
        match self {
            CodexFlowState::Pending { started_at_ms, .. }
            | CodexFlowState::Authorizing { started_at_ms, .. } => Some(*started_at_ms),
            _ => None,
        }
    }
}

pub type CodexFlows = Arc<tokio::sync::Mutex<HashMap<String, CodexFlowState>>>;

/// Loopback callback listeners by provider id, with the flow that owns them.
/// The callback port is shared (1455 by default), so a listener has to be
/// released before another flow — or the Codex CLI itself — can bind it.
pub type CodexListeners = Arc<tokio::sync::Mutex<HashMap<String, (u64, Arc<tokio::sync::Notify>)>>>;

/// Monotonic flow ids; see `CodexFlowState::Pending::flow_id`.
static FLOW_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_flow_id() -> u64 {
    FLOW_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// Which login flow `/setup` should run. Browser (PKCE) is the default, like
/// pi's own Codex login; device code is the headless fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMethod {
    Browser,
    Device,
}

impl LoginMethod {
    fn parse(value: Option<&str>) -> Result<Self, ApiError> {
        match value.map(str::trim) {
            None | Some("") | Some("browser") => Ok(Self::Browser),
            Some("device") | Some("device_code") | Some("device-code") => Ok(Self::Device),
            Some(other) => Err(api_error(
                StatusCode::BAD_REQUEST,
                format!("unknown login method '{other}' — use browser or device"),
            )),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct StartBody {
    pub provider: Option<String>,
    /// `browser` (default) or `device`.
    pub method: Option<String>,
    /// Force a brand-new flow even while another is pending (a lost or expired
    /// code — the page can't wait out the old flow).
    pub fresh: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
pub struct StatusQuery {
    pub provider: Option<String>,
    pub fresh: Option<bool>,
    pub method: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CompleteBody {
    pub provider: Option<String>,
    /// Pasted redirect URL, `code#state`, or a bare code.
    pub input: Option<String>,
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
    flow.started_at_ms()
        .map(|started| {
            codex_oauth::now_ms().saturating_sub(started) >= DEVICE_CODE_TIMEOUT_SECS * 1000
        })
        .unwrap_or(false)
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
            "method": "device",
            "user_code": user_code,
            "verification_uri": verification_uri,
            "interval": interval_secs,
            "expires_in": DEVICE_CODE_TIMEOUT_SECS,
        }),
        Some(CodexFlowState::Authorizing {
            auth_url,
            redirect_uri,
            callback_listening,
            ..
        }) => json!({
            "provider": provider,
            "state": "authorizing",
            "method": "browser",
            "auth_url": auth_url,
            "redirect_uri": redirect_uri,
            "callback_listening": callback_listening,
            "expires_in": DEVICE_CODE_TIMEOUT_SECS,
        }),
        Some(CodexFlowState::LoggedIn) => json!({"provider": provider, "state": "logged_in"}),
        Some(CodexFlowState::Failed(message)) => {
            json!({"provider": provider, "state": "failed", "message": message})
        }
        None => json!({"provider": provider, "state": "idle"}),
    }
}

/// `POST /api/codex/start` — begin (or re-read) a login. `provider` and
/// `method` may come from the query string or the JSON body; browser (PKCE) is
/// the default method, device code the headless fallback.
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
        .clone()
        .or_else(|| body.as_ref().and_then(|Json(b)| b.provider.clone()));
    let method = LoginMethod::parse(
        query
            .method
            .as_deref()
            .or_else(|| body.as_ref().and_then(|Json(b)| b.method.as_deref())),
    )?;
    let (id, manager) = select_provider(&state, requested.as_deref())?;

    // Reuse a live flow of the requested kind: the page may be reloaded, or two
    // tabs opened. `fresh` forces a new flow for one that was lost or expired.
    if !fresh {
        let flows = state.codex_flows.lock().await;
        if let Some(existing) = flows.get(&id).filter(|f| {
            !is_expired(f) && !matches!(f, CodexFlowState::Failed(_)) && f.method() == Some(method)
        }) {
            return Ok(Json(flow_json(&id, Some(existing))));
        }
    }

    match method {
        LoginMethod::Browser => start_browser_flow(&state, &id, manager).await,
        LoginMethod::Device => start_device_flow(&state, &id, manager).await,
    }
}

/// Browser (PKCE) login: bind the loopback callback (best effort — paste-back
/// covers a busy port), then hand the authorize URL to the page.
async fn start_browser_flow(
    state: &AppState,
    id: &str,
    manager: Arc<CodexTokenManager>,
) -> Result<Json<Value>, ApiError> {
    let requested_port = state.codex_callback_port;
    // This process may already be holding the callback port for a previous
    // flow: reclaim it instead of reporting it busy (it is our own listener).
    release_callback_listener(&state.codex_listeners, id).await;
    let listener = bind_callback(requested_port).await;
    let bound_port = match &listener {
        Some(l) => l.local_addr().map(|a| a.port()).unwrap_or(requested_port),
        None => requested_port,
    };
    let flow = codex_oauth::build_browser_flow(bound_port)
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let flow_id = next_flow_id();
    let state_entry = CodexFlowState::Authorizing {
        auth_url: flow.auth_url.clone(),
        redirect_uri: flow.redirect_uri.clone(),
        verifier: flow.verifier.clone(),
        state: flow.state.clone(),
        callback_listening: listener.is_some(),
        started_at_ms: codex_oauth::now_ms(),
        flow_id,
    };
    let response = flow_json(id, Some(&state_entry));
    state
        .codex_flows
        .lock()
        .await
        .insert(id.to_string(), state_entry);

    let shutdown = Arc::new(tokio::sync::Notify::new());
    if let Some(listener) = listener {
        state
            .codex_listeners
            .lock()
            .await
            .insert(id.to_string(), (flow_id, shutdown.clone()));
        spawn_callback_listener(
            listener,
            state.codex_flows.clone(),
            id.to_string(),
            flow_id,
            manager,
            flow.verifier.clone(),
            flow.state.clone(),
            flow.redirect_uri.clone(),
            shutdown.clone(),
        );
    }
    spawn_browser_timeout(state.codex_flows.clone(), id.to_string(), flow_id, shutdown);
    Ok(Json(response))
}

/// Device-code login: ask the auth server for a code and poll it proxy-side.
async fn start_device_flow(
    state: &AppState,
    id: &str,
    manager: Arc<CodexTokenManager>,
) -> Result<Json<Value>, ApiError> {
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
    let response = flow_json(id, Some(&flow));
    let flow_id = flow.flow_id().expect("pending flow has an id");
    state.codex_flows.lock().await.insert(id.to_string(), flow);

    spawn_poll_loop(
        state.codex_flows.clone(),
        id.to_string(),
        flow_id,
        manager,
        endpoints,
        device,
        client,
    );
    Ok(Json(response))
}

/// `POST /api/codex/complete` — finish a browser login with a pasted code (the
/// path when the loopback callback never lands, e.g. the proxy is remote or
/// port 1455 is taken).
pub async fn codex_complete(
    State(state): State<AppState>,
    body: Option<Json<CompleteBody>>,
) -> Result<Json<Value>, ApiError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let (id, manager) = select_provider(&state, body.provider.as_deref())?;
    let (verifier, expected_state, redirect_uri) = {
        let flows = state.codex_flows.lock().await;
        match flows.get(&id) {
            Some(CodexFlowState::Authorizing {
                verifier,
                state: expected_state,
                redirect_uri,
                ..
            }) => (
                verifier.clone(),
                expected_state.clone(),
                redirect_uri.clone(),
            ),
            Some(_) => {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "no browser login in progress — start one from /setup",
                ));
            }
            None => {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "no browser login in progress — start one from /setup",
                ));
            }
        }
    };

    let input = body.input.unwrap_or_default();
    let (code, state_param) = codex_oauth::parse_authorization_input(&input);
    if let Some(got) = &state_param
        && got != &expected_state
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "state mismatch — the pasted code belongs to a different login attempt",
        ));
    }
    let Some(code) = code else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "missing authorization code",
        ));
    };

    finish_browser_login(&state, &id, &manager, &code, &verifier, &redirect_uri).await?;
    // The pasted code finished the login, so the loopback listener has nothing
    // left to do — and holding 1455 would block the Codex CLI's own login and
    // the next /setup flow (which would then wrongly report "port busy").
    release_callback_listener(&state.codex_listeners, &id).await;
    Ok(Json(json!({"provider": id, "state": "logged_in"})))
}

/// Exchange a browser-flow authorization code and persist the tokens.
async fn finish_browser_login(
    state: &AppState,
    id: &str,
    manager: &Arc<CodexTokenManager>,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<(), ApiError> {
    let client = crate::providers::default_http_client();
    let tokens =
        codex_oauth::exchange_code(&client, manager.token_url(), code, verifier, redirect_uri)
            .await
            .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e.to_string()))?;
    manager
        .store_tokens(tokens)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e.to_string()))?;
    set_flow_state(
        &state.codex_flows,
        id,
        current_flow_id(&state.codex_flows, id).await.unwrap_or(0),
        CodexFlowState::LoggedIn,
    )
    .await;
    Ok(())
}

async fn current_flow_id(flows: &CodexFlows, id: &str) -> Option<u64> {
    flows.lock().await.get(id).and_then(CodexFlowState::flow_id)
}

/// Stop a flow's loopback listener and forget it. Idempotent: a listener that
/// already exited (callback handled, state mismatch, timeout) is a no-op.
async fn release_callback_listener(listeners: &CodexListeners, id: &str) {
    if let Some((_, shutdown)) = listeners.lock().await.remove(id) {
        // `notify_one` (not `notify_waiters`): the listener task may not have
        // polled its shutdown future yet, and a lost wake-up would leave the
        // port held. A stored permit is consumed on the first poll.
        shutdown.notify_one();
    }
}

/// Bind the loopback callback, retrying briefly: a listener released a moment
/// ago (a fresh start, or a paste-back that just completed) can still hold the
/// port for a few milliseconds.
async fn bind_callback(port: u16) -> Option<tokio::net::TcpListener> {
    const ATTEMPTS: u32 = 30;
    for attempt in 1..=ATTEMPTS {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return Some(listener);
        }
        if attempt < ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    }
    None
}

/// Loopback callback server for the browser flow (`GET /auth/callback`).
#[allow(clippy::too_many_arguments)]
fn spawn_callback_listener(
    listener: tokio::net::TcpListener,
    flows: CodexFlows,
    id: String,
    flow_id: u64,
    manager: Arc<CodexTokenManager>,
    verifier: String,
    expected_state: String,
    redirect_uri: String,
    shutdown: Arc<tokio::sync::Notify>,
) {
    #[derive(Clone)]
    struct CallbackState {
        flows: CodexFlows,
        id: String,
        flow_id: u64,
        manager: Arc<CodexTokenManager>,
        verifier: String,
        expected_state: String,
        redirect_uri: String,
        shutdown: Arc<tokio::sync::Notify>,
    }

    async fn callback(
        State(cb): State<CallbackState>,
        Query(params): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let code = params.get("code").filter(|c| !c.is_empty());
        let state_param = params.get("state");
        if state_param.is_some_and(|s| s != &cb.expected_state) {
            cb.shutdown.notify_waiters();
            return (
                StatusCode::BAD_REQUEST,
                "State mismatch — start the login again from /setup.",
            )
                .into_response();
        }
        let Some(code) = code else {
            return (
                StatusCode::BAD_REQUEST,
                "Missing authorization code — start the login again from /setup.",
            )
                .into_response();
        };
        let client = crate::providers::default_http_client();
        let result = match codex_oauth::exchange_code(
            &client,
            cb.manager.token_url(),
            code,
            &cb.verifier,
            &cb.redirect_uri,
        )
        .await
        {
            Ok(tokens) => cb.manager.store_tokens(tokens).await,
            Err(e) => Err(e),
        };
        let response = match result {
            Ok(()) => {
                set_flow_state(&cb.flows, &cb.id, cb.flow_id, CodexFlowState::LoggedIn).await;
                (StatusCode::OK, "Login complete — you can close this tab.").into_response()
            }
            Err(e) => {
                set_flow_state(
                    &cb.flows,
                    &cb.id,
                    cb.flow_id,
                    CodexFlowState::Failed(e.to_string()),
                )
                .await;
                (StatusCode::BAD_GATEWAY, format!("Login failed: {e}")).into_response()
            }
        };
        cb.shutdown.notify_waiters();
        response
    }

    let app = axum::Router::new()
        .route(
            codex_oauth::BROWSER_CALLBACK_PATH,
            axum::routing::get(callback),
        )
        .with_state(CallbackState {
            flows,
            id,
            flow_id,
            manager,
            verifier,
            expected_state,
            redirect_uri,
            shutdown: shutdown.clone(),
        });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.notified().await })
            .await;
    });
}

/// Browser flows expire like device flows; a stale timeout cannot clobber a
/// newer flow.
fn spawn_browser_timeout(
    flows: CodexFlows,
    id: String,
    flow_id: u64,
    shutdown: Arc<tokio::sync::Notify>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(DEVICE_CODE_TIMEOUT_SECS)).await;
        set_flow_state(
            &flows,
            &id,
            flow_id,
            CodexFlowState::Failed("Login timed out".into()),
        )
        .await;
        shutdown.notify_waiters();
    });
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
    let current = flows.get(id).and_then(CodexFlowState::flow_id);
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
    Html(include_str!("setup_page.html"))
}

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
        env_with_port(auth_base, provider_id, 0).await
    }

    /// `callback_port` 0 → the OS assigns a free port (tests); a busy port
    /// exercises the paste-back fallback.
    async fn env_with_port(auth_base: &str, provider_id: &str, callback_port: u16) -> Env {
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
            codex_callback_port: callback_port,
            codex_listeners: Default::default(),
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
            .route("/api/codex/complete", post(codex_complete))
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
                    .uri("/api/codex/start?method=device")
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
                        .uri("/api/codex/start?method=device")
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
                        .uri("/api/codex/start?method=device")
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

        let (_, first) = post_start("/api/codex/start?method=device").await;
        assert_eq!(first["state"], "pending");
        let first_flow_id = match e.flow.lock().await.get("openai-codex").cloned().unwrap() {
            CodexFlowState::Pending { flow_id, .. } => flow_id,
            other => panic!("expected pending, got {other:?}"),
        };

        let (status, second) = post_start("/api/codex/start?method=device&fresh=true").await;
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

    // ── browser (PKCE) flow ────────────────────────────────────────────────

    async fn post_json(app: Router, uri: &str, body: &str) -> (StatusCode, Value) {
        body_json(
            app.oneshot(
                Request::builder()
                    .uri(uri)
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await
    }

    async fn get_uri(app: Router, uri: &str) -> (StatusCode, String) {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// Start a browser login and return the flow's authorize URL + state.
    async fn start_browser(e: &Env) -> Value {
        let (status, body) = post_json(router(&e.state), "/api/codex/start", "{}").await;
        assert_eq!(status, StatusCode::OK, "start failed: {body}");
        assert_eq!(body["state"], "authorizing", "got {body}");
        assert_eq!(body["method"], "browser");
        body
    }

    fn state_of(body: &Value) -> String {
        let url = body["auth_url"].as_str().unwrap();
        let state = url
            .split("state=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .expect("state in auth_url");
        state.to_string()
    }

    #[tokio::test]
    async fn browser_start_returns_authorize_url_and_binds_loopback() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = env(&auth, "openai-codex").await;
        let body = start_browser(&e).await;
        assert_eq!(body["callback_listening"], true);
        let redirect_uri = body["redirect_uri"].as_str().unwrap();
        assert!(
            redirect_uri.starts_with("http://localhost:")
                && redirect_uri.ends_with("/auth/callback"),
            "got {redirect_uri}"
        );
        // port 0 → the real bound port, and the auth URL advertises it
        assert!(!redirect_uri.contains(":0/"), "bound port must be real");
        assert!(
            body["auth_url"]
                .as_str()
                .unwrap()
                .contains("https://auth.openai.com/oauth/authorize?")
        );
    }

    #[tokio::test]
    async fn browser_callback_completes_login() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = env(&auth, "openai-codex").await;
        let body = start_browser(&e).await;
        let redirect_uri = body["redirect_uri"].as_str().unwrap().to_string();
        let state = state_of(&body);

        // what the browser would hit when the callback port is reachable — a
        // real request to the loopback listener the flow bound
        let resp = reqwest::Client::new()
            .get(format!("{redirect_uri}?code=ac_browser&state={state}"))
            .send()
            .await
            .expect("callback request");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        assert_eq!(status, 200, "got {text}");
        assert!(text.contains("Login complete"), "got {text}");

        assert!(matches!(
            e.manager.status().await,
            codex_oauth::CodexStatus::LoggedIn { .. }
        ));
        assert_eq!(e.manager.account_id().await.unwrap(), "acct_1");
        assert_eq!(
            e.flow.lock().await.get("openai-codex").cloned(),
            Some(CodexFlowState::LoggedIn)
        );
        let (_, status_body) = get_uri(router(&e.state), "/api/codex/status").await;
        assert!(
            status_body.contains("logged_in"),
            "status must report the login: {status_body}"
        );
    }

    #[tokio::test]
    async fn browser_callback_rejects_state_mismatch() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = env(&auth, "openai-codex").await;
        let body = start_browser(&e).await;
        let redirect_uri = body["redirect_uri"].as_str().unwrap().to_string();
        let resp = reqwest::Client::new()
            .get(format!("{redirect_uri}?code=ac_browser&state=wrong"))
            .send()
            .await
            .expect("callback request");
        assert_eq!(resp.status().as_u16(), 400);
        assert!(matches!(
            e.manager.status().await,
            codex_oauth::CodexStatus::LoggedOut
        ));
    }

    /// Does the flow's loopback listener still accept connections?
    ///
    /// Probes without a `code`/`state`: a live listener answers 400
    /// "missing authorization code" and stays up, whereas probing with a
    /// wrong `state` would shut the listener down and make this a lie.
    async fn callback_answers(redirect_uri: &str) -> bool {
        let client = reqwest::Client::new();
        for _ in 0..20 {
            if client.get(redirect_uri).send().await.is_ok() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn fresh_browser_start_reclaims_the_port_it_was_holding() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        // A port we pick, so both flows target the same one — with port 0 the OS
        // would hand the second flow a different port and hide the problem.
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let e = env_with_port(&auth, "openai-codex", port).await;

        let first = start_browser(&e).await;
        assert_eq!(first["callback_listening"], true);
        let first_uri = first["redirect_uri"].as_str().unwrap().to_string();
        assert!(first_uri.contains(&format!(":{port}/")), "got {first_uri}");

        // Clicking "start" again must not report the port as busy: the previous
        // listener is this process's own, and it is done once a new flow starts.
        let (status, second) =
            post_json(router(&e.state), "/api/codex/start?fresh=true", "{}").await;
        assert_eq!(status, StatusCode::OK, "got {second}");
        assert_eq!(
            second["callback_listening"], true,
            "stale flow's listener still held port {port}: {second}"
        );
        let second_uri = second["redirect_uri"].as_str().unwrap().to_string();
        assert_eq!(second_uri, first_uri, "the new flow reuses the freed port");
        assert!(
            callback_answers(&second_uri).await,
            "the new listener must be up"
        );
    }

    #[tokio::test]
    async fn browser_paste_back_releases_the_loopback_listener() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = env(&auth, "openai-codex").await;
        let body = start_browser(&e).await;
        let redirect_uri = body["redirect_uri"].as_str().unwrap().to_string();
        let state = state_of(&body);
        assert!(callback_answers(&redirect_uri).await, "listener must be up");

        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/complete",
            &json!({"input": format!("{redirect_uri}?code=ac_pasted&state={state}")}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "got {body}");

        // The pasted code finished the login: the callback listener has nothing
        // left to do, and holding 1455 would block the Codex CLI's own login and
        // the next /setup flow (which would then wrongly report "port busy").
        assert!(
            !callback_answers(&redirect_uri).await,
            "loopback listener is still serving after a pasted code completed the login"
        );
    }

    #[tokio::test]
    async fn browser_callback_releases_the_loopback_listener() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = env(&auth, "openai-codex").await;
        let body = start_browser(&e).await;
        let redirect_uri = body["redirect_uri"].as_str().unwrap().to_string();
        let state = state_of(&body);

        let resp = reqwest::Client::new()
            .get(format!("{redirect_uri}?code=ac_browser&state={state}"))
            .send()
            .await
            .expect("callback request");
        assert_eq!(resp.status().as_u16(), 200);
        assert!(!callback_answers(&redirect_uri).await);
    }

    #[tokio::test]
    async fn browser_paste_back_completes_login() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = env(&auth, "openai-codex").await;
        let body = start_browser(&e).await;
        let redirect_uri = body["redirect_uri"].as_str().unwrap().to_string();
        let state = state_of(&body);

        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/complete",
            &json!({"input": format!("{redirect_uri}?code=ac_pasted&state={state}")}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "got {body}");
        assert_eq!(body["state"], "logged_in");
        assert!(matches!(
            e.manager.status().await,
            codex_oauth::CodexStatus::LoggedIn { .. }
        ));
    }

    #[tokio::test]
    async fn browser_paste_back_accepts_bare_code_and_rejects_wrong_state() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = env(&auth, "openai-codex").await;
        start_browser(&e).await;

        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/complete",
            &json!({"input": "ac_bare#wrong-state"}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "got {body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("state mismatch"),
            "got {body}"
        );

        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/complete",
            &json!({"input": "ac_bare"}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "got {body}");
        assert!(matches!(
            e.manager.status().await,
            codex_oauth::CodexStatus::LoggedIn { .. }
        ));
    }

    #[tokio::test]
    async fn browser_paste_back_without_a_flow_is_400() {
        let e = env("http://127.0.0.1:1", "openai-codex").await;
        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/complete",
            &json!({"input": "code"}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no browser login"),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn browser_start_falls_back_to_paste_back_when_the_port_is_busy() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        // Occupy a port, then ask the flow to bind it (port 1455 is shared with
        // the Codex CLI, so this happens in practice).
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken_port = taken.local_addr().unwrap().port();
        let e = env_with_port(&auth, "openai-codex", taken_port).await;

        let body = start_browser(&e).await;
        assert_eq!(body["callback_listening"], false);
        assert_eq!(
            body["redirect_uri"],
            format!("http://localhost:{taken_port}/auth/callback")
        );

        // …and paste-back still finishes the login.
        let state = state_of(&body);
        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/complete",
            &json!({"input": format!("ac_fallback#{state}")}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "got {body}");
    }

    #[tokio::test]
    async fn start_rejects_an_unknown_login_method() {
        let e = env("http://127.0.0.1:1", "openai-codex").await;
        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/start?method=carrier-pigeon",
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("carrier-pigeon"),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn device_start_is_available_as_the_fallback_method() {
        let user_code_calls = Arc::new(AtomicUsize::new(0));
        let auth = spawn_auth_server(user_code_calls.clone()).await;
        let e = env(&auth, "openai-codex").await;
        let (status, body) =
            post_json(router(&e.state), "/api/codex/start?method=device", "{}").await;
        assert_eq!(status, StatusCode::OK, "got {body}");
        assert_eq!(body["state"], "pending");
        assert_eq!(body["method"], "device");
        assert_eq!(body["user_code"], "WXYZ-1234");
        assert_eq!(user_code_calls.load(Ordering::SeqCst), 1);

        // switching methods does not reuse the other flow's state
        let body = start_browser(&e).await;
        assert_eq!(body["method"], "browser");
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
                        .uri("/api/codex/start?method=device")
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
