//! OpenAI Codex (ChatGPT subscription) OAuth support: constants, pure helpers,
//! device-code login, token manager, and the Codex request shape.
//!
//! Reference implementation replicated exactly (no guessing):
//! `pi-ai/dist/auth/oauth/openai-codex.js`, `pi-ai/dist/auth/oauth/device-code.js`,
//! `pi-ai/dist/api/openai-codex-responses.js`.

use axum::body::Bytes;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

// ── Constants (mirror the reference verbatim) ───────────────────────────────

/// Public client id used by the Codex CLI / pi.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_BASE_URL: &str = "https://auth.openai.com";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
pub const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
pub const SCOPE: &str = "openid profile email offline_access";
pub const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
/// Device flow deadline (reference: `DEVICE_CODE_TIMEOUT_SECONDS = 15 * 60`).
pub const DEVICE_CODE_TIMEOUT_SECS: u64 = 15 * 60;
/// RFC 8628 §3.2: minimum poll interval.
pub const MIN_POLL_INTERVAL_MS: u64 = 1000;
/// RFC 8628 §3.2: interval to use when the server omits one.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// RFC 8628 §3.5: `slow_down` widens the interval by 5s.
pub const SLOW_DOWN_INCREMENT_MS: u64 = 5000;

#[derive(Debug, thiserror::Error)]
pub enum CodexError {
    #[error("invalid request body: {0}")]
    InvalidJson(String),
    #[error("OpenAI Codex not logged in — open /setup")]
    LoggedOut,
    #[error("codex upstream error {status}: {body}")]
    Http { status: u16, body: Value },
    #[error("refresh token rejected (invalid_grant) — open /setup")]
    InvalidGrant,
    #[error("transport: {0}")]
    Transport(String),
    #[error("device flow: {0}")]
    DeviceFlow(String),
}

// ── Token manager ──────────────────────────────────────────────────────────

/// Refresh once the access token is within this margin of expiry.
pub const REFRESH_MARGIN_MS: u64 = 60 * 60 * 1000;
const BACKOFF_START_MS: u64 = 60 * 1000;
const BACKOFF_MAX_MS: u64 = 32 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexStatus {
    LoggedOut,
    LoggedIn { expires_at_ms: u64 },
}

#[derive(Debug)]
struct ManagerInner {
    tokens: Option<Tokens>,
    /// Refresh token that produced `invalid_grant` — a differing token in the
    /// state file means a fresh login happened and the latch is cleared.
    bad_refresh: Option<String>,
    backoff_ms: u64,
    backoff_until_ms: u64,
    last_error: Option<String>,
}

/// Owns the Codex credential lifecycle: load, single-flight refresh, rotation
/// persistence, `invalid_grant` latch, and backoff.
pub struct CodexTokenManager {
    state_path: std::path::PathBuf,
    token_url: String,
    client: reqwest::Client,
    inner: tokio::sync::Mutex<ManagerInner>,
}

impl std::fmt::Debug for CodexTokenManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexTokenManager")
            .field("state_path", &self.state_path)
            .finish_non_exhaustive()
    }
}

impl CodexTokenManager {
    pub fn new(state_path: &std::path::Path, token_url: &str) -> Self {
        Self {
            state_path: state_path.to_path_buf(),
            token_url: token_url.to_string(),
            client: crate::providers::default_http_client(),
            inner: tokio::sync::Mutex::new(ManagerInner {
                tokens: None,
                bad_refresh: None,
                backoff_ms: 0,
                backoff_until_ms: 0,
                last_error: None,
            }),
        }
    }

    pub fn state_path(&self) -> &std::path::Path {
        &self.state_path
    }

    /// OAuth token endpoint this manager refreshes against.
    pub fn token_url(&self) -> &str {
        &self.token_url
    }

    /// A valid access token, refreshing when needed (single-flight).
    pub async fn access(&self) -> Result<String, CodexError> {
        let mut inner = self.inner.lock().await;
        self.adopt_relogin(&mut inner);
        if inner.bad_refresh.is_some() {
            // Logged-out latch: no file/refresh work until /setup writes new state.
            return Err(CodexError::LoggedOut);
        }
        let now = now_ms();
        if let Some(tokens) = inner.tokens.clone() {
            if tokens.expires_at_ms > now + REFRESH_MARGIN_MS {
                return Ok(tokens.access);
            }
            if now < inner.backoff_until_ms {
                return Err(backoff_error(&inner));
            }
            return self.refresh_locked(&mut inner).await;
        }
        match load_persisted(&self.state_path) {
            Some(tokens) => {
                let usable = tokens.expires_at_ms > now + REFRESH_MARGIN_MS;
                inner.tokens = Some(tokens.clone());
                if usable {
                    inner.bad_refresh = None;
                    return Ok(tokens.access);
                }
                if now < inner.backoff_until_ms {
                    return Err(backoff_error(&inner));
                }
                self.refresh_locked(&mut inner).await
            }
            None => Err(CodexError::LoggedOut),
        }
    }

    /// Refresh regardless of expiry, still single-flight.
    pub async fn force_refresh(&self) -> Result<String, CodexError> {
        let mut inner = self.inner.lock().await;
        self.adopt_relogin(&mut inner);
        if inner.bad_refresh.is_some() {
            return Err(CodexError::LoggedOut);
        }
        inner.backoff_until_ms = 0;
        if inner.tokens.is_none() {
            inner.tokens = load_persisted(&self.state_path);
        }
        if inner.tokens.is_none() {
            return Err(CodexError::LoggedOut);
        }
        self.refresh_locked(&mut inner).await
    }

    /// `chatgpt_account_id` of the current access token.
    pub async fn account_id(&self) -> Result<String, CodexError> {
        let access = self.access().await?;
        account_id_from_token(&access)
            .ok_or_else(|| CodexError::Transport("access token has no chatgpt_account_id".into()))
    }

    /// Credential status for the /setup page.
    pub async fn status(&self) -> CodexStatus {
        {
            let inner = self.inner.lock().await;
            if inner.bad_refresh.is_some() {
                return CodexStatus::LoggedOut;
            }
            if let Some(tokens) = &inner.tokens {
                return CodexStatus::LoggedIn {
                    expires_at_ms: tokens.expires_at_ms,
                };
            }
        }
        match load_persisted(&self.state_path) {
            Some(tokens) => CodexStatus::LoggedIn {
                expires_at_ms: tokens.expires_at_ms,
            },
            None => CodexStatus::LoggedOut,
        }
    }

    /// Persist freshly exchanged tokens (after a device-code login).
    pub async fn store_tokens(&self, tokens: Tokens) -> Result<(), CodexError> {
        save_persisted(&self.state_path, &tokens)
            .map_err(|e| CodexError::Transport(e.to_string()))?;
        let mut inner = self.inner.lock().await;
        inner.tokens = Some(tokens);
        inner.bad_refresh = None;
        inner.backoff_ms = 0;
        inner.backoff_until_ms = 0;
        inner.last_error = None;
        Ok(())
    }

    /// Drop in-memory tokens so the next call re-reads the state file.
    pub async fn reload(&self) {
        let mut inner = self.inner.lock().await;
        inner.tokens = None;
        inner.bad_refresh = None;
        inner.backoff_ms = 0;
        inner.backoff_until_ms = 0;
        inner.last_error = None;
    }

    /// Background refresh: refresh when inside the margin. Errors are recorded,
    /// never surfaced (the request path reports them).
    pub async fn background_tick(&self) {
        let mut inner = self.inner.lock().await;
        if inner.bad_refresh.is_some() {
            return;
        }
        if inner.tokens.is_none() {
            inner.tokens = load_persisted(&self.state_path);
        }
        let Some(tokens) = inner.tokens.clone() else {
            return;
        };
        if tokens.expires_at_ms > now_ms() + REFRESH_MARGIN_MS {
            return;
        }
        if let Err(e) = self.refresh_locked(&mut inner).await {
            tracing::warn!(error = %e, "codex background token refresh failed");
        }
    }

    /// Adopt credentials written by a later login while the manager holds an
    /// `invalid_grant` latch from the previous refresh token.
    fn adopt_relogin(&self, inner: &mut ManagerInner) -> bool {
        let Some(bad) = inner.bad_refresh.clone() else {
            return false;
        };
        let Some(loaded) = load_persisted(&self.state_path) else {
            return false;
        };
        if loaded.refresh == bad {
            return false;
        }
        inner.tokens = Some(loaded);
        inner.bad_refresh = None;
        inner.backoff_ms = 0;
        inner.backoff_until_ms = 0;
        inner.last_error = None;
        true
    }

    /// Single-flight refresh: the caller holds the manager lock for the whole
    /// HTTP round trip, so concurrent 401s coalesce into one refresh.
    async fn refresh_locked(&self, inner: &mut ManagerInner) -> Result<String, CodexError> {
        let current = match inner
            .tokens
            .clone()
            .or_else(|| load_persisted(&self.state_path))
        {
            Some(tokens) => tokens,
            None => {
                inner.tokens = None;
                return Err(CodexError::LoggedOut);
            }
        };
        inner.tokens = Some(current.clone());
        match refresh_tokens(&self.client, &self.token_url, &current.refresh).await {
            Ok(fresh) => {
                // Rotation safety: the new refresh token hits the disk before any
                // caller can see the new access token.
                save_persisted(&self.state_path, &fresh)
                    .map_err(|e| CodexError::Transport(format!("saving oauth state: {e}")))?;
                let access = fresh.access.clone();
                inner.tokens = Some(fresh);
                inner.bad_refresh = None;
                inner.backoff_ms = 0;
                inner.backoff_until_ms = 0;
                inner.last_error = None;
                Ok(access)
            }
            Err(CodexError::InvalidGrant) => {
                inner.tokens = None;
                inner.bad_refresh = Some(current.refresh);
                inner.last_error = Some("refresh token rejected".into());
                Err(CodexError::LoggedOut)
            }
            Err(e) => {
                inner.backoff_ms = if inner.backoff_ms == 0 {
                    BACKOFF_START_MS
                } else {
                    (inner.backoff_ms * 2).min(BACKOFF_MAX_MS)
                };
                inner.backoff_until_ms = now_ms() + inner.backoff_ms;
                inner.last_error = Some(e.to_string());
                Err(e)
            }
        }
    }
}

fn backoff_error(inner: &ManagerInner) -> CodexError {
    CodexError::Transport(format!(
        "codex token refresh backed off: {}",
        inner.last_error.as_deref().unwrap_or("refresh failed")
    ))
}

fn load_persisted(path: &std::path::Path) -> Option<Tokens> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Tokens>(&text).ok()
}

fn save_persisted(path: &std::path::Path, tokens: &Tokens) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(tokens).map_err(std::io::Error::other)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&json)?;
        file.flush()?;
        // an existing file keeps its old mode — force it
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, &json)?;
    }
    Ok(())
}

// ── Pure helpers ────────────────────────────────────────────────────────────

/// Extract `chatgpt_account_id` from an access token's JWT payload.
pub fn account_id_from_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64_url_decode(payload)?;
    let json: Value = serde_json::from_slice(&decoded).ok()?;
    let id = json
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// base64url (no padding) decoder — JWT payloads, no dependency needed.
fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 3);
    let (mut buf, mut bits) = (0u32, 0u32);
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Resolve the Codex responses endpoint from an upstream base URL.
pub fn codex_responses_url(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    let base = if trimmed.is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        trimmed
    };
    if base.ends_with("/codex/responses") {
        base.to_string()
    } else if base.ends_with("/codex") {
        format!("{base}/responses")
    } else {
        format!("{base}/codex/responses")
    }
}

/// `pi (<os> <release>; <arch>)` — mirrors pi's `getPiUserAgent()`.
pub fn pi_user_agent() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match kernel_release() {
        Some(release) => format!("pi ({os} {release}; {arch})"),
        None => format!("pi ({os})"),
    }
}

/// Kernel release, the way node's `os.release()` reports it (Linux).
fn kernel_release() -> Option<String> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
    let release = release.trim();
    if release.is_empty() {
        return None;
    }
    Some(release.to_string())
}

/// Transformed Codex request: serialized body + the session id used for the
/// `session-id` / `x-client-request-id` headers.
#[derive(Debug, Clone)]
pub struct CodexRequest {
    pub body: Bytes,
    pub session_id: Option<String>,
}

/// Apply the Codex request shape to a client Responses body.
pub fn transform_codex_body(body: Bytes) -> Result<CodexRequest, CodexError> {
    let mut value: Value =
        serde_json::from_slice(&body).map_err(|e| CodexError::InvalidJson(e.to_string()))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| CodexError::InvalidJson("request body must be a JSON object".into()))?;

    obj.insert("stream".into(), json!(true));
    obj.insert("store".into(), json!(false));

    let has_instructions = obj
        .get("instructions")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    if !has_instructions {
        obj.insert("instructions".into(), json!("You are a helpful assistant."));
    }

    let mut include: Vec<String> = obj
        .get("include")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if !include.iter().any(|v| v == "reasoning.encrypted_content") {
        include.insert(0, "reasoning.encrypted_content".into());
    }
    obj.insert("include".into(), json!(include));

    if !obj.contains_key("text") {
        obj.insert("text".into(), json!({"verbosity": "low"}));
    }
    let session_id = obj
        .get("prompt_cache_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if session_id.is_none()
        && let Some(sid) = obj
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    {
        obj.insert("prompt_cache_key".into(), json!(sid));
    }
    if !obj.contains_key("tool_choice") {
        obj.insert("tool_choice".into(), json!("auto"));
    }
    if !obj.contains_key("parallel_tool_calls") {
        obj.insert("parallel_tool_calls".into(), json!(true));
    }
    obj.remove("max_output_tokens");

    let session_id = session_id.or_else(|| {
        obj.get("prompt_cache_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });
    let body = serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|e| CodexError::InvalidJson(e.to_string()))?;
    Ok(CodexRequest { body, session_id })
}

/// Headers the Codex backend requires, all owned by the proxy.
pub fn codex_headers(access: &str, account_id: &str, session_id: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    hdr(&mut headers, "authorization", &format!("Bearer {access}"));
    hdr(&mut headers, "chatgpt-account-id", account_id);
    hdr(&mut headers, "originator", "pi");
    hdr(&mut headers, "user-agent", &pi_user_agent());
    hdr(&mut headers, "openai-beta", "responses=experimental");
    hdr(&mut headers, "accept", "text/event-stream");
    hdr(&mut headers, "content-type", "application/json");
    if let Some(session_id) = session_id {
        hdr(&mut headers, "session-id", session_id);
        hdr(&mut headers, "x-client-request-id", session_id);
    }
    headers
}

fn hdr(map: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        map.insert(HeaderName::from_static(name), v);
    }
}

// ── Endpoint resolution (env-overridable test hooks) ────────────────────────

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Auth base URL; `AIPROXY_CODEX_AUTH_BASE_URL` overrides (test hook).
pub fn auth_base_url() -> String {
    env_or("AIPROXY_CODEX_AUTH_BASE_URL", AUTH_BASE_URL)
}

/// Token endpoint; `AIPROXY_CODEX_TOKEN_URL` overrides (test hook).
pub fn token_url() -> String {
    env_or("AIPROXY_CODEX_TOKEN_URL", TOKEN_URL)
}

/// Device-flow + token endpoints for one auth base URL.
#[derive(Debug, Clone)]
pub struct CodexEndpoints {
    pub user_code_url: String,
    pub device_token_url: String,
    pub verification_uri: String,
    pub redirect_uri: String,
}

impl CodexEndpoints {
    /// Empty base → the real `https://auth.openai.com`.
    pub fn from_auth_base(base: &str) -> Self {
        let trimmed = base.trim().trim_end_matches('/');
        let base = if trimmed.is_empty() {
            AUTH_BASE_URL
        } else {
            trimmed
        };
        Self {
            user_code_url: format!("{base}/api/accounts/deviceauth/usercode"),
            device_token_url: format!("{base}/api/accounts/deviceauth/token"),
            verification_uri: format!("{base}/codex/device"),
            redirect_uri: format!("{base}/deviceauth/callback"),
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Device-code flow ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFlow {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCredentials {
    pub authorization_code: String,
    pub code_verifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollStatus {
    Pending,
    SlowDown { interval_secs: Option<u64> },
    Complete(DeviceCredentials),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub expires_at_ms: u64,
}

/// `POST {url}` with `{"client_id": CLIENT_ID}` → device code + user code.
pub async fn start_device_flow(
    client: &reqwest::Client,
    url: &str,
) -> Result<DeviceFlow, CodexError> {
    let resp = client
        .post(url)
        .json(&json!({"client_id": CLIENT_ID}))
        .send()
        .await
        .map_err(|e| CodexError::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status == 404 {
        return Err(CodexError::DeviceFlow(
            "OpenAI Codex device code login is not enabled for this server".into(),
        ));
    }
    if !(200..300).contains(&status) {
        return Err(CodexError::DeviceFlow(format!(
            "device code request failed with status {status}{}",
            detail(&text)
        )));
    }
    let json: Value = serde_json::from_str(&text)
        .map_err(|e| CodexError::DeviceFlow(format!("invalid device code response: {e}")))?;
    let device_auth_id = json
        .get("device_auth_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let user_code = json
        .get("user_code")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    // The reference accepts a numeric string for `interval` too.
    let interval = json.get("interval").and_then(|v| match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    });
    match (device_auth_id, user_code, interval) {
        (Some(device_auth_id), Some(user_code), Some(interval_secs)) => Ok(DeviceFlow {
            device_auth_id: device_auth_id.to_string(),
            user_code: user_code.to_string(),
            interval_secs,
        }),
        _ => Err(CodexError::DeviceFlow(format!(
            "invalid device code response: {text}"
        ))),
    }
}

/// One poll of `POST {url}` with `{device_auth_id, user_code}`.
pub async fn poll_device_flow(
    client: &reqwest::Client,
    url: &str,
    flow: &DeviceFlow,
) -> Result<PollStatus, CodexError> {
    let resp = client
        .post(url)
        .json(&json!({
            "device_auth_id": flow.device_auth_id,
            "user_code": flow.user_code,
        }))
        .send()
        .await
        .map_err(|e| CodexError::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if (200..300).contains(&status) {
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let code = json
            .get("authorization_code")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let verifier = json
            .get("code_verifier")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        return match (code, verifier) {
            (Some(authorization_code), Some(code_verifier)) => {
                Ok(PollStatus::Complete(DeviceCredentials {
                    authorization_code: authorization_code.to_string(),
                    code_verifier: code_verifier.to_string(),
                }))
            }
            _ => Err(CodexError::DeviceFlow(format!(
                "invalid device auth token response: {text}"
            ))),
        };
    }
    if status == 403 || status == 404 {
        return Ok(PollStatus::Pending);
    }
    match error_code(&text).as_deref() {
        Some("deviceauth_authorization_pending") => Ok(PollStatus::Pending),
        Some("slow_down") => Ok(PollStatus::SlowDown {
            interval_secs: None,
        }),
        _ => Err(CodexError::DeviceFlow(format!(
            "device auth failed with status {status}{}",
            detail(&text)
        ))),
    }
}

fn detail(text: &str) -> String {
    if text.trim().is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

/// `error.code` or `error` from an OAuth-style error body.
fn error_code(text: &str) -> Option<String> {
    let json: Value = serde_json::from_str(text).ok()?;
    match json.get("error")? {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("code")?.as_str().map(str::to_string),
        _ => None,
    }
}

// ── Token exchange + refresh ───────────────────────────────────────────────

/// Exchange an authorization code (form-encoded, per the reference).
pub async fn exchange_code(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens, CodexError> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ];
    let resp = client
        .post(token_url)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form_encode(&form))
        .send()
        .await
        .map_err(|e| CodexError::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    parse_tokens(status, &text, None)
}

/// Refresh an access token. `existing_refresh` is kept when the server omits
/// `refresh_token` (rotation safety; the reference always sends one).
pub async fn refresh_tokens(
    client: &reqwest::Client,
    token_url: &str,
    refresh: &str,
) -> Result<Tokens, CodexError> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", CLIENT_ID),
    ];
    let resp = client
        .post(token_url)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form_encode(&form))
        .send()
        .await
        .map_err(|e| CodexError::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    parse_tokens(status, &text, Some(refresh))
}

/// `application/x-www-form-urlencoded` body (reqwest's `form()` needs a feature
/// this crate does not enable).
fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn parse_tokens(
    status: u16,
    text: &str,
    existing_refresh: Option<&str>,
) -> Result<Tokens, CodexError> {
    let json: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        if (status == 400 || status == 401) && text.contains("invalid_grant") {
            return Err(CodexError::InvalidGrant);
        }
        let body = if json.is_null() {
            json!({"error": text})
        } else {
            json
        };
        return Err(CodexError::Http { status, body });
    }
    let access = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let refresh = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| existing_refresh.map(str::to_string));
    let expires_in = json.get("expires_in").and_then(|v| v.as_u64());
    match (access, refresh, expires_in) {
        (Some(access), Some(refresh), Some(expires_in)) => Ok(Tokens {
            access,
            refresh,
            expires_at_ms: now_ms() + expires_in * 1000,
        }),
        _ => Err(CodexError::DeviceFlow(format!(
            "token response missing fields: {text}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// base64url (no padding) encoder, independent of the implementation.
    fn b64url(input: &[u8]) -> String {
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

    fn token_with(payload: Value) -> String {
        format!(
            "header.{}.sig",
            b64url(serde_json::to_vec(&payload).unwrap().as_slice())
        )
    }

    #[test]
    fn account_id_from_valid_token() {
        let token = token_with(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct_123"}
        }));
        assert_eq!(account_id_from_token(&token).as_deref(), Some("acct_123"));
    }

    #[test]
    fn account_id_rejects_malformed_tokens() {
        assert_eq!(account_id_from_token("not-a-jwt"), None);
        assert_eq!(account_id_from_token("a.b"), None);
        assert_eq!(account_id_from_token("a.!!!.c"), None);
        // valid JWT but no account claim
        let token = token_with(json!({"sub": "user"}));
        assert_eq!(account_id_from_token(&token), None);
        // claim present but empty
        let token = token_with(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": ""}
        }));
        assert_eq!(account_id_from_token(&token), None);
    }

    #[test]
    fn codex_url_join_matrix() {
        assert_eq!(
            codex_responses_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex_responses_url("https://chatgpt.com/backend-api/"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex_responses_url("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex_responses_url("https://chatgpt.com/backend-api/codex/responses"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex_responses_url(""),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn user_agent_has_pi_shape() {
        let ua = pi_user_agent();
        assert!(ua.starts_with("pi ("), "got {ua}");
        assert!(ua.ends_with(')'), "got {ua}");
        assert!(ua.contains(std::env::consts::ARCH), "got {ua}");
    }

    fn body(v: Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&v).unwrap())
    }

    fn parse(b: &Bytes) -> Value {
        serde_json::from_slice(b).unwrap()
    }

    #[test]
    fn transform_forces_codex_fields_and_keeps_client_fields() {
        let out = transform_codex_body(body(json!({
            "model": "gpt-5.6-sol",
            "input": [{"role": "user", "content": "hi"}],
            "store": true,
            "max_output_tokens": 4096,
            "temperature": 0.3,
            "prompt_cache_key": "sess-1"
        })))
        .unwrap();
        let v = parse(&out.body);
        assert_eq!(v["model"], "gpt-5.6-sol");
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false);
        assert_eq!(v["instructions"], "You are a helpful assistant.");
        assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(v["text"], json!({"verbosity": "low"}));
        assert_eq!(v["tool_choice"], "auto");
        assert_eq!(v["parallel_tool_calls"], true);
        assert_eq!(v["temperature"], 0.3);
        assert!(v.get("max_output_tokens").is_none());
        assert_eq!(out.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn transform_preserves_client_instructions_include_and_text() {
        let out = transform_codex_body(body(json!({
            "model": "m",
            "instructions": "be terse",
            "include": ["reasoning.encrypted_content", "other"],
            "text": {"verbosity": "high"}
        })))
        .unwrap();
        let v = parse(&out.body);
        assert_eq!(v["instructions"], "be terse");
        assert_eq!(
            v["include"],
            json!(["reasoning.encrypted_content", "other"])
        );
        assert_eq!(v["text"], json!({"verbosity": "high"}));
        assert_eq!(out.session_id, None);
    }

    #[test]
    fn transform_appends_missing_include_entry() {
        let out = transform_codex_body(body(json!({"model": "m", "include": ["other"]}))).unwrap();
        let v = parse(&out.body);
        assert_eq!(
            v["include"],
            json!(["reasoning.encrypted_content", "other"])
        );
    }

    #[test]
    fn transform_rejects_non_object_and_invalid_json() {
        assert!(matches!(
            transform_codex_body(Bytes::from_static(b"{oops")),
            Err(CodexError::InvalidJson(_))
        ));
        assert!(matches!(
            transform_codex_body(Bytes::from_static(b"[1,2]")),
            Err(CodexError::InvalidJson(_))
        ));
    }

    fn get<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
        h.get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn codex_headers_full_set() {
        let h = codex_headers("tok", "acct_123", Some("sess-1"));
        assert_eq!(get(&h, "authorization"), Some("Bearer tok"));
        assert_eq!(get(&h, "chatgpt-account-id"), Some("acct_123"));
        assert_eq!(get(&h, "originator"), Some("pi"));
        assert_eq!(get(&h, "openai-beta"), Some("responses=experimental"));
        assert_eq!(get(&h, "accept"), Some("text/event-stream"));
        assert_eq!(get(&h, "content-type"), Some("application/json"));
        assert_eq!(get(&h, "session-id"), Some("sess-1"));
        assert_eq!(get(&h, "x-client-request-id"), Some("sess-1"));
        assert!(get(&h, "user-agent").unwrap().starts_with("pi ("));
    }

    #[test]
    fn codex_headers_omit_session_headers_without_session() {
        let h = codex_headers("tok", "acct", None);
        assert!(get(&h, "session-id").is_none());
        assert!(get(&h, "x-client-request-id").is_none());
    }

    // ── device-code + token client tests ───────────────────────────────────

    use axum::Json;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    type Seen = Arc<StdMutex<(String, String)>>;

    async fn spawn_router(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn capture(state: Seen, headers: HeaderMap, body: String) {
        let ctype = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        *state.lock().unwrap() = (ctype, body);
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn device_start_parses_numeric_string_interval_and_sends_client_id() {
        let seen: Seen = Default::default();
        let s = seen.clone();
        let app = axum::Router::new().route(
            "/usercode",
            post(move |headers: HeaderMap, body: String| {
                let s = s.clone();
                async move {
                    capture(s, headers, body);
                    Json(json!({
                        "device_auth_id": "dev_1",
                        "user_code": "ABCD-EFGH",
                        "interval": "3"
                    }))
                }
            }),
        );
        let base = spawn_router(app).await;
        let flow = start_device_flow(&client(), &format!("{base}/usercode"))
            .await
            .unwrap();
        assert_eq!(flow.device_auth_id, "dev_1");
        assert_eq!(flow.user_code, "ABCD-EFGH");
        assert_eq!(flow.interval_secs, 3);
        let (_, body) = seen.lock().unwrap().clone();
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["client_id"],
            CLIENT_ID
        );
    }

    #[tokio::test]
    async fn device_start_404_is_device_flow_error() {
        let app = axum::Router::new().route(
            "/usercode",
            post(|| async { StatusCode::NOT_FOUND.into_response() }),
        );
        let base = spawn_router(app).await;
        let err = start_device_flow(&client(), &format!("{base}/usercode"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not enabled"), "got {err}");
    }

    #[tokio::test]
    async fn device_poll_pending_then_complete_and_sends_device_shape() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen: Seen = Default::default();
        let (c, s) = (calls.clone(), seen.clone());
        let app = axum::Router::new().route(
            "/token",
            post(move |headers: HeaderMap, body: String| {
                let (c, s) = (c.clone(), s.clone());
                async move {
                    capture(s, headers, body);
                    if c.fetch_add(1, Ordering::SeqCst) == 0 {
                        (StatusCode::FORBIDDEN, Json(json!({}))).into_response()
                    } else {
                        Json(json!({
                            "authorization_code": "ac_1",
                            "code_verifier": "cv_1"
                        }))
                        .into_response()
                    }
                }
            }),
        );
        let base = spawn_router(app).await;
        let url = format!("{base}/token");
        let flow = DeviceFlow {
            device_auth_id: "dev_1".into(),
            user_code: "CODE".into(),
            interval_secs: 5,
        };
        assert_eq!(
            poll_device_flow(&client(), &url, &flow).await.unwrap(),
            PollStatus::Pending
        );
        let done = poll_device_flow(&client(), &url, &flow).await.unwrap();
        assert_eq!(
            done,
            PollStatus::Complete(DeviceCredentials {
                authorization_code: "ac_1".into(),
                code_verifier: "cv_1".into(),
            })
        );
        let (_, body) = seen.lock().unwrap().clone();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["device_auth_id"], "dev_1");
        assert_eq!(v["user_code"], "CODE");
    }

    #[tokio::test]
    async fn device_poll_authorization_pending_error_code_is_pending() {
        let app = axum::Router::new().route(
            "/token",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": {"code": "deviceauth_authorization_pending"}})),
                )
                    .into_response()
            }),
        );
        let base = spawn_router(app).await;
        let flow = DeviceFlow {
            device_auth_id: "d".into(),
            user_code: "c".into(),
            interval_secs: 5,
        };
        assert_eq!(
            poll_device_flow(&client(), &format!("{base}/token"), &flow)
                .await
                .unwrap(),
            PollStatus::Pending
        );
    }

    #[tokio::test]
    async fn device_poll_slow_down_is_reported() {
        let app = axum::Router::new().route(
            "/token",
            post(|| async {
                (StatusCode::BAD_REQUEST, Json(json!({"error": "slow_down"}))).into_response()
            }),
        );
        let base = spawn_router(app).await;
        let flow = DeviceFlow {
            device_auth_id: "d".into(),
            user_code: "c".into(),
            interval_secs: 5,
        };
        assert_eq!(
            poll_device_flow(&client(), &format!("{base}/token"), &flow)
                .await
                .unwrap(),
            PollStatus::SlowDown {
                interval_secs: None
            }
        );
    }

    #[tokio::test]
    async fn device_poll_other_error_is_device_flow_error() {
        let app = axum::Router::new().route(
            "/token",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": {"code": "boom"}})),
                )
                    .into_response()
            }),
        );
        let base = spawn_router(app).await;
        let flow = DeviceFlow {
            device_auth_id: "d".into(),
            user_code: "c".into(),
            interval_secs: 5,
        };
        let err = poll_device_flow(&client(), &format!("{base}/token"), &flow)
            .await
            .unwrap_err();
        assert!(matches!(err, CodexError::DeviceFlow(_)), "got {err}");
    }

    #[tokio::test]
    async fn exchange_code_sends_form_shape_and_parses_tokens() {
        let seen: Seen = Default::default();
        let s = seen.clone();
        let app = axum::Router::new().route(
            "/oauth/token",
            post(move |headers: HeaderMap, body: String| {
                let s = s.clone();
                async move {
                    capture(s, headers, body);
                    Json(json!({
                        "access_token": "at_1",
                        "refresh_token": "rt_1",
                        "expires_in": 3600
                    }))
                }
            }),
        );
        let base = spawn_router(app).await;
        let before = now_ms();
        let tokens = exchange_code(
            &client(),
            &format!("{base}/oauth/token"),
            "ac_1",
            "cv_1",
            "https://auth.openai.com/deviceauth/callback",
        )
        .await
        .unwrap();
        assert_eq!(tokens.access, "at_1");
        assert_eq!(tokens.refresh, "rt_1");
        assert!(tokens.expires_at_ms >= before + 3_600_000);
        let (ctype, body) = seen.lock().unwrap().clone();
        assert!(
            ctype.starts_with("application/x-www-form-urlencoded"),
            "got {ctype}"
        );
        assert!(body.contains("grant_type=authorization_code"), "got {body}");
        assert!(
            body.contains(&format!("client_id={CLIENT_ID}")),
            "got {body}"
        );
        assert!(body.contains("code=ac_1"), "got {body}");
        assert!(body.contains("code_verifier=cv_1"), "got {body}");
        assert!(
            body.contains("redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback"),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn exchange_missing_refresh_token_is_error() {
        let app = axum::Router::new().route(
            "/oauth/token",
            post(|| async { Json(json!({"access_token": "at", "expires_in": 10})) }),
        );
        let base = spawn_router(app).await;
        let err = exchange_code(&client(), &format!("{base}/oauth/token"), "c", "v", "r")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing fields"), "got {err}");
    }

    #[tokio::test]
    async fn refresh_sends_form_shape_without_scope() {
        let seen: Seen = Default::default();
        let s = seen.clone();
        let app = axum::Router::new().route(
            "/oauth/token",
            post(move |headers: HeaderMap, body: String| {
                let s = s.clone();
                async move {
                    capture(s, headers, body);
                    Json(json!({
                        "access_token": "at_2",
                        "refresh_token": "rt_2",
                        "expires_in": 60
                    }))
                }
            }),
        );
        let base = spawn_router(app).await;
        let tokens = refresh_tokens(&client(), &format!("{base}/oauth/token"), "rt_1")
            .await
            .unwrap();
        assert_eq!(tokens.refresh, "rt_2");
        let (ctype, body) = seen.lock().unwrap().clone();
        assert!(ctype.starts_with("application/x-www-form-urlencoded"));
        assert!(body.contains("grant_type=refresh_token"), "got {body}");
        assert!(body.contains("refresh_token=rt_1"), "got {body}");
        assert!(
            body.contains(&format!("client_id={CLIENT_ID}")),
            "got {body}"
        );
        assert!(!body.contains("scope="), "no scope in refresh: {body}");
    }

    #[tokio::test]
    async fn refresh_keeps_old_token_when_response_omits_it() {
        let app = axum::Router::new().route(
            "/oauth/token",
            post(|| async { Json(json!({"access_token": "at_2", "expires_in": 60})) }),
        );
        let base = spawn_router(app).await;
        let tokens = refresh_tokens(&client(), &format!("{base}/oauth/token"), "rt_1")
            .await
            .unwrap();
        assert_eq!(tokens.refresh, "rt_1");
    }

    #[tokio::test]
    async fn refresh_invalid_grant_maps_to_invalid_grant() {
        let app = axum::Router::new().route(
            "/oauth/token",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "invalid_grant"})),
                )
                    .into_response()
            }),
        );
        let base = spawn_router(app).await;
        let err = refresh_tokens(&client(), &format!("{base}/oauth/token"), "rt")
            .await
            .unwrap_err();
        assert!(matches!(err, CodexError::InvalidGrant), "got {err}");
    }

    #[tokio::test]
    async fn refresh_http_error_is_http_error() {
        let app = axum::Router::new().route(
            "/oauth/token",
            post(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"oops": true})),
                )
                    .into_response()
            }),
        );
        let base = spawn_router(app).await;
        let err = refresh_tokens(&client(), &format!("{base}/oauth/token"), "rt")
            .await
            .unwrap_err();
        match err {
            CodexError::Http { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Http, got {other}"),
        }
    }

    // ── token manager tests ────────────────────────────────────────────────

    struct TokenServer {
        base: String,
        calls: Arc<AtomicUsize>,
    }

    async fn token_server(
        handler: impl Fn(usize) -> (u16, Value) + Send + Sync + 'static,
    ) -> TokenServer {
        let calls = Arc::new(AtomicUsize::new(0));
        let (c, h) = (calls.clone(), Arc::new(handler));
        let app = axum::Router::new().route(
            "/oauth/token",
            post(move || {
                let (c, h) = (c.clone(), h.clone());
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = h(n);
                    (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
                }
            }),
        );
        TokenServer {
            base: spawn_router(app).await,
            calls,
        }
    }

    fn ok_refresh(access: &'static str, refresh: &'static str) -> (u16, Value) {
        (
            200,
            json!({"access_token": access, "refresh_token": refresh, "expires_in": 86_400}),
        )
    }

    fn write_state(path: &std::path::Path, access: &str, refresh: &str, expires_at_ms: u64) {
        let tokens = json!({"access": access, "refresh": refresh, "expires_at_ms": expires_at_ms});
        std::fs::write(path, serde_json::to_vec(&tokens).unwrap()).unwrap();
    }

    fn manager(path: &std::path::Path, base: &str) -> CodexTokenManager {
        CodexTokenManager::new(path, &format!("{base}/oauth/token"))
    }

    fn state_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("openai-codex-oauth-state.json")
    }

    #[tokio::test]
    async fn manager_loads_valid_token_from_file_without_http() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|_| ok_refresh("at_new", "rt_new")).await;
        write_state(&path, "at_file", "rt_file", now_ms() + 24 * 3600 * 1000);
        let mgr = manager(&path, &srv.base);
        assert_eq!(mgr.access().await.unwrap(), "at_file");
        assert_eq!(srv.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn manager_refreshes_expired_token_single_flight() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|_| ok_refresh("at_new", "rt_new")).await;
        write_state(&path, "at_old", "rt_old", now_ms() - 1000);
        let mgr = manager(&path, &srv.base);
        let (a, b) = tokio::join!(mgr.access(), mgr.access());
        assert_eq!(a.unwrap(), "at_new");
        assert_eq!(b.unwrap(), "at_new");
        assert_eq!(srv.calls.load(Ordering::SeqCst), 1, "one HTTP refresh");
    }

    #[tokio::test]
    async fn manager_refreshes_within_margin() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|_| ok_refresh("at_new", "rt_new")).await;
        // 30 min left < 60 min margin
        write_state(&path, "at_old", "rt_old", now_ms() + 30 * 60 * 1000);
        let mgr = manager(&path, &srv.base);
        assert_eq!(mgr.access().await.unwrap(), "at_new");
        assert_eq!(srv.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn manager_missing_state_file_is_logged_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|_| ok_refresh("at_new", "rt_new")).await;
        let mgr = manager(&path, &srv.base);
        let err = mgr.access().await.unwrap_err();
        assert!(matches!(err, CodexError::LoggedOut), "got {err}");
        assert!(err.to_string().contains("/setup"), "got {err}");
        assert_eq!(srv.calls.load(Ordering::SeqCst), 0);
        assert_eq!(mgr.status().await, CodexStatus::LoggedOut);
    }

    #[tokio::test]
    async fn manager_persists_rotated_refresh_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|_| ok_refresh("at_new", "rt_new")).await;
        write_state(&path, "at_old", "rt_old", now_ms() - 1000);
        let mgr = manager(&path, &srv.base);
        assert_eq!(mgr.access().await.unwrap(), "at_new");
        let on_disk: Tokens =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.access, "at_new");
        assert_eq!(on_disk.refresh, "rt_new");
        assert_eq!(
            mgr.status().await,
            CodexStatus::LoggedIn {
                expires_at_ms: on_disk.expires_at_ms
            }
        );
    }

    #[tokio::test]
    async fn manager_keeps_old_refresh_when_response_omits_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv =
            token_server(|_| (200, json!({"access_token": "at_new", "expires_in": 3600}))).await;
        write_state(&path, "at_old", "rt_old", now_ms() - 1000);
        let mgr = manager(&path, &srv.base);
        assert_eq!(mgr.access().await.unwrap(), "at_new");
        let on_disk: Tokens =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.refresh, "rt_old");
    }

    #[tokio::test]
    async fn manager_invalid_grant_latches_logged_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|_| (400, json!({"error": "invalid_grant"}))).await;
        write_state(&path, "at_old", "rt_old", now_ms() - 1000);
        let mgr = manager(&path, &srv.base);
        assert!(matches!(mgr.access().await, Err(CodexError::LoggedOut)));
        assert!(matches!(mgr.access().await, Err(CodexError::LoggedOut)));
        assert_eq!(
            srv.calls.load(Ordering::SeqCst),
            1,
            "latch prevents re-polling"
        );
        assert_eq!(mgr.status().await, CodexStatus::LoggedOut);
    }

    #[tokio::test]
    async fn manager_adopts_relogin_written_file_after_latch() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|n| {
            if n == 0 {
                (400, json!({"error": "invalid_grant"}))
            } else {
                ok_refresh("at_new", "rt_new")
            }
        })
        .await;
        write_state(&path, "at_old", "rt_old", now_ms() - 1000);
        let mgr = manager(&path, &srv.base);
        assert!(matches!(mgr.access().await, Err(CodexError::LoggedOut)));
        // "aiproxy login" wrote a fresh state file while the proxy kept running
        let expires_at_ms = now_ms() + 24 * 3600 * 1000;
        write_state(&path, "at_fresh", "rt_fresh", expires_at_ms);
        assert_eq!(mgr.access().await.unwrap(), "at_fresh");
        assert_eq!(srv.calls.load(Ordering::SeqCst), 1, "no extra HTTP call");
        assert_eq!(mgr.status().await, CodexStatus::LoggedIn { expires_at_ms });
    }

    #[tokio::test]
    async fn manager_backs_off_after_transient_failure_and_force_refresh_bypasses_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|_| (500, json!({"oops": true}))).await;
        write_state(&path, "at_old", "rt_old", now_ms() - 1000);
        let mgr = manager(&path, &srv.base);
        assert!(matches!(
            mgr.access().await,
            Err(CodexError::Http { status: 500, .. })
        ));
        assert!(mgr.access().await.is_err());
        assert_eq!(
            srv.calls.load(Ordering::SeqCst),
            1,
            "backoff suppresses retry"
        );
        assert!(mgr.force_refresh().await.is_err());
        assert_eq!(
            srv.calls.load(Ordering::SeqCst),
            2,
            "force_refresh bypasses backoff"
        );
    }

    #[tokio::test]
    async fn manager_store_tokens_writes_file_with_owner_only_perms() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|_| ok_refresh("at", "rt")).await;
        let mgr = manager(&path, &srv.base);
        mgr.store_tokens(Tokens {
            access: "at_login".into(),
            refresh: "rt_login".into(),
            expires_at_ms: now_ms() + 24 * 3600 * 1000,
        })
        .await
        .unwrap();
        assert_eq!(mgr.access().await.unwrap(), "at_login");
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "state file must be owner-only");
        }
    }

    #[tokio::test]
    async fn manager_account_id_comes_from_access_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|_| ok_refresh("at", "rt")).await;
        let token = token_with(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct_9"}
        }));
        write_state(&path, &token, "rt", now_ms() + 24 * 3600 * 1000);
        let mgr = manager(&path, &srv.base);
        assert_eq!(mgr.account_id().await.unwrap(), "acct_9");
    }

    #[tokio::test]
    async fn manager_background_tick_refreshes_inside_margin() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let srv = token_server(|_| ok_refresh("at_new", "rt_new")).await;
        write_state(&path, "at_old", "rt_old", now_ms() + 30 * 60 * 1000);
        let mgr = manager(&path, &srv.base);
        mgr.background_tick().await;
        assert_eq!(srv.calls.load(Ordering::SeqCst), 1);
        let on_disk: Tokens =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.access, "at_new");
    }
}
