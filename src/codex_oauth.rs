//! OpenAI Codex (ChatGPT subscription) OAuth support: constants, pure helpers,
//! device-code login, token manager, and the Codex request shape.
//!
//! Reference implementation replicated exactly (no guessing):
//! `pi-ai/dist/auth/oauth/openai-codex.js`, `pi-ai/dist/auth/oauth/device-code.js`,
//! `pi-ai/dist/api/openai-codex-responses.js`.

use axum::body::Bytes;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
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
    #[error("transport: {0}")]
    Transport(String),
    #[error("device flow: {0}")]
    DeviceFlow(String),
}

// ── Pure helpers ────────────────────────────────────────────────────────────

/// Extract `chatgpt_account_id` from an access token's JWT payload.
pub fn account_id_from_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64_url_decode(payload)?;
    let json: Value = serde_json::from_slice(&decoded).ok()?;
    let id = json.get(JWT_CLAIM_PATH)?.get("chatgpt_account_id")?.as_str()?;
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
    let mut value: Value = serde_json::from_slice(&body)
        .map_err(|e| CodexError::InvalidJson(e.to_string()))?;
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
    if session_id.is_none() {
        if let Some(sid) = obj.get("session_id").and_then(|v| v.as_str()) {
            if !sid.is_empty() {
                obj.insert("prompt_cache_key".into(), json!(sid));
            }
        }
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
        assert_eq!(v["include"], json!(["reasoning.encrypted_content", "other"]));
        assert_eq!(v["text"], json!({"verbosity": "high"}));
        assert_eq!(out.session_id, None);
    }

    #[test]
    fn transform_appends_missing_include_entry() {
        let out = transform_codex_body(body(json!({"model": "m", "include": ["other"]}))).unwrap();
        let v = parse(&out.body);
        assert_eq!(v["include"], json!(["reasoning.encrypted_content", "other"]));
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
}
