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
    /// Browser (PKCE) flow: waiting for the user to paste the redirect URL.
    Authorizing {
        auth_url: String,
        redirect_uri: String,
        verifier: String,
        state: String,
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
            ..
        }) => json!({
            "provider": provider,
            "state": "authorizing",
            "method": "browser",
            "auth_url": auth_url,
            "redirect_uri": redirect_uri,
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

/// Browser (PKCE) login: hand the authorize URL to the page. There is no
/// loopback listener — the browser lands on a dead 1455 callback page and the
/// user pastes that URL back (`POST /api/codex/complete`), which also works for
/// a remote proxy and never fights the Codex CLI for the port.
async fn start_browser_flow(
    state: &AppState,
    id: &str,
    manager: Arc<CodexTokenManager>,
) -> Result<Json<Value>, ApiError> {
    let flow = codex_oauth::build_browser_flow()
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let flow_id = next_flow_id();
    let state_entry = CodexFlowState::Authorizing {
        auth_url: flow.auth_url.clone(),
        redirect_uri: flow.redirect_uri.clone(),
        verifier: flow.verifier.clone(),
        state: flow.state.clone(),
        started_at_ms: codex_oauth::now_ms(),
        flow_id,
    };
    let response = flow_json(id, Some(&state_entry));
    state
        .codex_flows
        .lock()
        .await
        .insert(id.to_string(), state_entry);

    let _ = manager;
    spawn_browser_timeout(state.codex_flows.clone(), id.to_string(), flow_id);
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
        state.registry.clone(),
    );
    Ok(Json(response))
}

/// `POST /api/codex/complete` — finish a browser login with a pasted code (the
/// path when the loopback callback never lands, e.g. the proxy is remote or
/// port 1455 is taken).
pub async fn codex_complete(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
    body: Option<Json<CompleteBody>>,
) -> Result<Json<Value>, ApiError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    // `?provider=` wins, then the body — the /setup page puts it in the query
    // (one card per subscription), so ignoring it broke Finish login whenever
    // more than one openai-codex upstream was configured.
    let requested = query.provider.clone().or_else(|| body.provider.clone());
    let (id, manager) = select_provider(&state, requested.as_deref())?;
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
    spawn_discovery_after_login(state.registry.clone());
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

/// What is on disk at the credential path: existence, size, and mtime. Reported
/// by `GET /api/codex/status` so a login that "disappeared" can be traced to a
/// path that no longer has a file behind it.
fn state_file_json(path: &std::path::Path) -> Value {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let mode = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    format!("{:o}", meta.permissions().mode() & 0o777)
                }
                #[cfg(not(unix))]
                {
                    String::new()
                }
            };
            json!({"exists": true, "size": meta.len(), "mtime_ms": mtime_ms, "mode": mode})
        }
        Err(_) => json!({"exists": false}),
    }
}

async fn current_flow_id(flows: &CodexFlows, id: &str) -> Option<u64> {
    flows.lock().await.get(id).and_then(CodexFlowState::flow_id)
}

/// Browser flows expire like device flows; a stale timeout cannot clobber a
/// newer flow.
fn spawn_browser_timeout(flows: CodexFlows, id: String, flow_id: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(DEVICE_CODE_TIMEOUT_SECS)).await;
        set_flow_state(
            &flows,
            &id,
            flow_id,
            CodexFlowState::Failed("Login timed out".into()),
        )
        .await;
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
    // Where the credentials are looked for, and what is actually there. This is
    // the difference between "the login vanished" and "the file is elsewhere":
    // the path follows the config file's directory, so a container that mounts
    // only the config file loses the state file when it is recreated.
    body["state_path"] = json!(manager.state_path().display().to_string());
    body["state_file"] = state_file_json(manager.state_path());
    Ok(Json(body))
}

/// `GET /api/codex/providers` — every `openai-codex` upstream and whether it is
/// logged in. The `/setup` page needs this to offer a picker when a proxy serves
/// more than one subscription (each gets its own state file and login flow).
pub async fn codex_providers(State(state): State<AppState>) -> Json<Value> {
    let mut ids: Vec<&String> = state.codex_managers.keys().collect();
    ids.sort();
    let mut providers = Vec::new();
    for id in ids {
        let manager = &state.codex_managers[id];
        let status = manager.status().await;
        let (logged_in, expires_at_ms) = match status {
            codex_oauth::CodexStatus::LoggedIn { expires_at_ms } => (true, Some(expires_at_ms)),
            codex_oauth::CodexStatus::LoggedOut => (false, None),
        };
        providers.push(json!({
            "id": id,
            "logged_in": logged_in,
            "expires_at_ms": expires_at_ms,
            "state_path": manager.state_path().display().to_string(),
            "state_file": state_file_json(manager.state_path()),
        }));
    }
    Json(json!({"providers": providers}))
}

/// A login just changed what the upstream can answer, so re-run discovery.
/// Discovery otherwise only happens at startup (`model_refresh_secs: 0`), which
/// left the Codex catalog empty until the proxy was restarted. Spawned, so the
/// login response does not wait on upstream probes.
fn spawn_discovery_after_login(registry: Arc<crate::discovery::ModelRegistry>) {
    tokio::spawn(async move {
        let report = registry.refresh_report().await;
        for p in &report {
            tracing::info!(
                provider = %p.id,
                models = p.models,
                error = p.error.as_deref().unwrap_or(""),
                "model discovery refreshed after login"
            );
        }
    });
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
#[allow(clippy::too_many_arguments)]
fn spawn_poll_loop(
    flows: CodexFlows,
    id: String,
    flow_id: u64,
    manager: Arc<CodexTokenManager>,
    endpoints: CodexEndpoints,
    device: DeviceFlow,
    client: reqwest::Client,
    registry: Arc<crate::discovery::ModelRegistry>,
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
                        Ok(()) => {
                            // the upstream just gained a subscription: re-probe
                            spawn_discovery_after_login(registry.clone());
                            CodexFlowState::LoggedIn
                        }
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
                    let token = format!(
                        "header.{}.sig",
                        crate::codex_oauth::base64_url_encode(payload.to_string().as_bytes())
                    );
                    Json(json!({
                        "access_token": token,
                        "refresh_token": "rt_1",
                        "expires_in": 86_400
                    }))
                }),
            );
        spawn(app).await
    }

    struct Env {
        _dir: tempfile::TempDir,
        state: AppState,
        flow: CodexFlows,
        manager: Arc<CodexTokenManager>,
    }

    /// Two openai-codex upstreams: `openai-codex=alice` and `openai-codex=bob`,
    /// each with its own state file.
    async fn multi_env(auth_base: &str, ids: &[&str]) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let mut managers = HashMap::new();
        let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
        let mut first = None;
        for id in ids {
            let manager = Arc::new(CodexTokenManager::new(
                &dir.path().join(format!("{id}-oauth-state.json")),
                &format!("{auth_base}/oauth/token"),
            ));
            managers.insert((*id).to_string(), manager.clone());
            providers.push(Arc::new(MockProvider::with_surface(
                id,
                vec!["gpt-5.6-sol".into()],
                crate::provider::ModelSurface::Responses,
            )));
            first.get_or_insert(manager);
        }
        let flows: CodexFlows = Default::default();
        let state = AppState {
            registry: Arc::new(crate::discovery::ModelRegistry::new(providers)),
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
            manager: first.expect("at least one id"),
        }
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
            .route("/api/codex/complete", post(codex_complete))
            .route("/api/codex/status", get(codex_status))
            .route("/api/codex/providers", get(codex_providers))
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

    /// `POST` a JSON body and decode the response.
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

    /// `POST` an empty JSON object, the body every `/api/codex/*` route takes.
    async fn post_empty(app: Router, uri: &str) -> (StatusCode, Value) {
        post_json(app, uri, "{}").await
    }

    /// `GET` a route and decode the JSON body.
    async fn get_json(app: Router, uri: &str) -> (StatusCode, Value) {
        body_json(
            app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap(),
        )
        .await
    }

    #[tokio::test]
    async fn status_reports_where_credentials_live() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = env(&auth, "openai-codex").await;
        let state_path = e.manager.state_path().to_path_buf();

        // logged out: the path is reported, and the file is not there
        let (status, body) = get_json(router(&e.state), "/api/codex/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "logged_out", "got {body}");
        assert_eq!(body["state_path"], state_path.display().to_string());
        assert_eq!(body["state_file"]["exists"], false);

        // after a login the same path reports a real file — this is the line to
        // check when a restart appears to lose the login
        let body = start_browser(&e).await;
        let state = state_of(&body);
        let redirect_uri = body["redirect_uri"].as_str().unwrap().to_string();
        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/complete",
            &json!({"input": format!("{redirect_uri}?code=ac_paste&state={state}")}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "got {body}");

        let (_, body) = get_json(router(&e.state), "/api/codex/status").await;
        assert_eq!(body["state"], "logged_in", "got {body}");
        assert_eq!(body["state_file"]["exists"], true, "got {body}");
        assert!(
            body["state_file"]["size"].as_u64().unwrap_or(0) > 0,
            "got {body}"
        );
        assert!(
            body["state_file"]["mtime_ms"].as_u64().unwrap_or(0) > 0,
            "got {body}"
        );
    }

    #[tokio::test]
    async fn login_triggers_model_discovery() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = env(&auth, "openai-codex").await;
        // The catalog starts empty (discovery runs at startup in production).
        assert!(
            e.state.registry.models().is_empty(),
            "fixture must start with an unpopulated catalog"
        );

        let body = start_browser(&e).await;
        let state = state_of(&body);
        let redirect_uri = body["redirect_uri"].as_str().unwrap().to_string();
        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/complete",
            &json!({"input": format!("{redirect_uri}?code=ac_paste&state={state}")}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "got {body}");

        // The refresh is spawned (the login response must not wait on upstream
        // probes), so poll briefly for it to land.
        let mut ids: Vec<String> = Vec::new();
        for _ in 0..100 {
            ids = e
                .state
                .registry
                .models()
                .into_iter()
                .map(|m| m.id)
                .collect();
            if !ids.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            ids,
            vec!["openai-codex/gpt-5.6-sol"],
            "a login must re-run discovery so clients see the new catalog"
        );
    }

    #[tokio::test]
    async fn providers_endpoint_lists_every_subscription() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = multi_env(&auth, &["openai-codex=alice", "openai-codex=bob"]).await;

        let (status, body) = get_json(router(&e.state), "/api/codex/providers").await;
        assert_eq!(status, StatusCode::OK);
        let subs = body["providers"].as_array().expect("providers array");
        let ids: Vec<&str> = subs.iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["openai-codex=alice", "openai-codex=bob"]);
        for sub in subs {
            assert_eq!(sub["logged_in"], false, "got {sub}");
            assert!(
                sub["state_path"]
                    .as_str()
                    .unwrap()
                    .contains("oauth-state.json"),
                "each subscription must report where its credentials live: {sub}"
            );
        }
        assert_ne!(
            subs[0]["state_path"], subs[1]["state_path"],
            "subscriptions must not share a state file"
        );

        // With more than one subscription, the bare status endpoint must say so
        // rather than silently answering for one of them.
        let (status, body) = get_json(router(&e.state), "/api/codex/status").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("multiple"),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn logging_in_one_subscription_leaves_the_other_alone() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = multi_env(&auth, &["openai-codex=alice", "openai-codex=bob"]).await;

        // start + paste-back against alice only
        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/start?provider=openai-codex=alice",
            "{}",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "got {body}");
        let state = state_of(&body);
        let redirect_uri = body["redirect_uri"].as_str().unwrap().to_string();
        let (status, body) = post_json(
            router(&e.state),
            "/api/codex/complete?provider=openai-codex=alice",
            &json!({"input": format!("{redirect_uri}?code=ac_alice&state={state}")}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "got {body}");

        let (_, body) = get_json(
            router(&e.state),
            "/api/codex/status?provider=openai-codex=alice",
        )
        .await;
        assert_eq!(body["state"], "logged_in", "got {body}");
        assert_eq!(body["state_file"]["exists"], true, "got {body}");

        let (_, body) = get_json(
            router(&e.state),
            "/api/codex/status?provider=openai-codex=bob",
        )
        .await;
        assert_eq!(
            body["state"], "logged_out",
            "the other subscription must be untouched: {body}"
        );
        assert_eq!(body["state_file"]["exists"], false, "got {body}");

        let (_, body) = get_json(router(&e.state), "/api/codex/providers").await;
        let subs = body["providers"].as_array().unwrap();
        assert_eq!(subs[0]["logged_in"], true, "alice: {body}");
        assert_eq!(subs[1]["logged_in"], false, "bob: {body}");
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
        let (status, body) = post_empty(router(&e.state), "/api/codex/start").await;
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
        let (status, body) = post_empty(router(&e.state), "/api/codex/start?provider=other").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]["message"].as_str().unwrap().contains("other"),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn start_reports_gateway_error_when_auth_server_fails() {
        let e = env("http://127.0.0.1:1", "openai-codex").await;
        let (status, _) = post_empty(router(&e.state), "/api/codex/start?method=device").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
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

        let first = post_empty(app.clone(), "/api/codex/start?method=device").await;
        let second = post_empty(app.clone(), "/api/codex/start?method=device").await;
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
            async move { post_empty(app, uri).await }
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
    async fn browser_start_is_paste_back_only() {
        let auth = spawn_auth_server(Arc::new(AtomicUsize::new(0))).await;
        let e = env(&auth, "openai-codex").await;
        let body = start_browser(&e).await;
        // No loopback listener: the redirect URI is always the fixed 1455
        // callback, the browser lands on a dead page, and the user pastes the
        // URL back. Nothing reports a listener state any more.
        assert_eq!(
            body["redirect_uri"], "http://localhost:1455/auth/callback",
            "got {body}"
        );
        assert!(
            body["auth_url"]
                .as_str()
                .unwrap()
                .contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"),
            "got {body}"
        );
        assert_eq!(body.get("callback_listening"), None);
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

        let (status, body) = post_empty(app.clone(), "/api/codex/start?method=device").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["verification_uri"], format!("{auth}/codex/device"));

        // proxy-side poll loop finishes on its own
        let mut final_state = String::new();
        for _ in 0..80 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let (_, body) = get_json(app.clone(), "/api/codex/status").await;
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
