use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// ── Unified usage types ──────────────────────────────────────────────

/// A single usage window.
#[derive(Debug, Clone, Serialize)]
pub struct UsageWindow {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<i64>,
}

/// A credit/pool balance.
#[derive(Debug, Clone, Serialize)]
pub struct CreditPool {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    pub unit: String,
}

/// Usage data from any provider.
#[derive(Debug, Clone, Default)]
pub struct UsageData {
    pub windows: Vec<UsageWindow>,
    pub pools: Vec<CreditPool>,
}

/// Trait for providers that report usage.
#[async_trait::async_trait]
pub trait UsageProvider: Send + Sync {
    /// Provider name (e.g. "minimax", "opencode-go").
    fn name(&self) -> &str;
    /// Fetch usage from the provider's billing endpoint.
    async fn fetch(&self) -> Result<UsageData, String>;
}

// ── API response types (minimax, openrouter, zai) ──────────────────────

// -- minimax --

#[derive(Debug, Deserialize)]
struct MinimaxBaseResp {
    #[serde(rename = "status_code")]
    status_code: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
struct MinimaxModelRemains {
    #[serde(rename = "model_name")]
    model_name: Option<String>,
    #[serde(rename = "current_interval_total_count")]
    current_interval_total_count: Option<serde_json::Value>,
    #[serde(rename = "current_interval_usage_count")]
    current_interval_usage_count: Option<serde_json::Value>,
    #[serde(rename = "current_interval_status")]
    current_interval_status: Option<serde_json::Value>,
    #[serde(rename = "current_interval_remaining_percent")]
    current_interval_remaining_percent: Option<serde_json::Value>,
    #[serde(rename = "start_time")]
    start_time: Option<serde_json::Value>,
    #[serde(rename = "end_time")]
    end_time: Option<serde_json::Value>,
    #[serde(rename = "remains_time")]
    remains_time: Option<serde_json::Value>,
    #[serde(rename = "current_weekly_total_count")]
    current_weekly_total_count: Option<serde_json::Value>,
    #[serde(rename = "current_weekly_usage_count")]
    current_weekly_usage_count: Option<serde_json::Value>,
    #[serde(rename = "current_weekly_remaining_percent")]
    current_weekly_remaining_percent: Option<serde_json::Value>,
    #[serde(rename = "weekly_end_time")]
    weekly_end_time: Option<serde_json::Value>,
    #[serde(rename = "weekly_remains_time")]
    weekly_remains_time: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct MinimaxCodingPlanData {
    #[serde(rename = "base_resp")]
    base_resp: Option<MinimaxBaseResp>,
    #[serde(rename = "model_remains", default)]
    model_remains: Vec<MinimaxModelRemains>,
}

#[derive(Debug, Deserialize)]
struct MinimaxCodingPlanPayload {
    #[serde(rename = "base_resp")]
    base_resp: Option<MinimaxBaseResp>,
    data: Option<MinimaxCodingPlanData>,
    #[serde(rename = "model_remains", default)]
    model_remains_root: Vec<MinimaxModelRemains>,
}

// -- openrouter --

#[derive(Debug, Deserialize)]
struct OpenRouterCreditsData {
    total_credits: serde_json::Number,
    total_usage: serde_json::Number,
}

#[derive(Debug, Deserialize)]
struct OpenRouterCreditsResponse {
    data: OpenRouterCreditsData,
}

// -- zai --

#[derive(Debug, Deserialize)]
struct ZaiQuotaLimitResponse {
    code: i64,
    success: bool,
    data: Option<ZaiQuotaLimitData>,
}

#[derive(Debug, Deserialize)]
struct ZaiQuotaLimitData {
    limits: Vec<ZaiLimitRaw>,
}

#[derive(Debug, Deserialize)]
struct ZaiLimitRaw {
    #[serde(rename = "type")]
    limit_type: String,
    unit: i64,
    number: i64,
    percentage: i64,
    #[serde(rename = "nextResetTime")]
    next_reset_time: Option<i64>,
}

// ── Serialization types for the API response ───────────────────────────

/// One provider's usage snapshot in the API response.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderUsage {
    pub provider: String,
    pub windows: Vec<UsageWindow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pools: Vec<CreditPool>,
    pub updated_at: u64,
}

// ── UsageTracker ───────────────────────────────────────────────────────

/// Thread-safe in-memory usage tracker.
#[derive(Clone)]
pub struct UsageTracker {
    inner: Arc<RwLock<HashMap<String, (UsageData, u64)>>>,
}

impl std::fmt::Debug for UsageTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageTracker").finish_non_exhaustive()
    }
}

impl Default for UsageTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Update usage for a provider. Window labels are normalized to canonical
    /// output form ("7d"/"30d") on write, so snapshot/HTML/widget agree.
    pub async fn update(&self, provider: &str, mut data: UsageData) {
        for w in &mut data.windows {
            w.label = normalize_window_label(&w.label, w.window_minutes);
        }
        let now = now_millis();
        self.inner
            .write()
            .await
            .insert(provider.to_string(), (data, now));
    }

    pub async fn snapshot(&self) -> Vec<ProviderUsage> {
        self.inner
            .read()
            .await
            .iter()
            .map(|(provider, (data, ts))| ProviderUsage {
                provider: provider.clone(),
                windows: data.windows.clone(),
                pools: data.pools.clone(),
                updated_at: *ts,
            })
            .collect()
    }
}

/// Canonical output labels for API clients (extraction keys untouched):
/// weekly-family → "7d", monthly-family → "30d". Minutes win when known
/// (zai generates "1w"/"4w" dynamically from window length).
fn normalize_window_label(label: &str, window_minutes: Option<i64>) -> String {
    if let Some(m) = window_minutes {
        if m == 10080 {
            return "7d".into();
        }
        if m == 43200 {
            return "30d".into();
        }
    }
    match label {
        "weekly" | "week" | "1w" => "7d".into(),
        "monthly" | "month" => "30d".into(),
        _ => label.into(),
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Helper functions ───────────────────────────────────────────────────

fn json_int(val: &Option<serde_json::Value>) -> Option<i64> {
    val.as_ref().and_then(|v| match v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

fn json_float(val: &Option<serde_json::Value>) -> Option<f64> {
    val.as_ref().and_then(|v| match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Clamp percent to 0..100. Upstream values are always percent-scale
/// (0.3 means 0.3%, never a 0-1 fraction): scaling sub-1.0 values by 100
/// turned 0.3% into 30%. Consistent with zai/minimax/header paths,
/// which never rescale.
fn normalize_percent(p: f64) -> f64 {
    p.clamp(0.0, 100.0)
}

fn epoch_to_secs(raw: i64) -> Option<i64> {
    if raw > 1_000_000_000_000 {
        Some(raw / 1000)
    } else if raw > 1_000_000_000 {
        Some(raw)
    } else {
        None
    }
}

fn seconds_until_reset(end_raw: Option<i64>, remains_raw: Option<i64>) -> Option<i64> {
    if let Some(end_secs) = end_raw.and_then(epoch_to_secs) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if end_secs > now {
            return Some(end_secs - now);
        }
    }
    remains_raw
}

// ── Minimax fetcher ────────────────────────────────────────────────────

fn minimax_used_percent(total: i64, remaining: i64) -> f64 {
    let used = (total - remaining).max(0);
    ((used as f64 / total as f64) * 100.0).clamp(0.0, 100.0)
}

fn minimax_remaining_percent_to_used(remaining_percent: f64) -> f64 {
    (100.0 - remaining_percent).clamp(0.0, 100.0)
}

fn minimax_window_minutes(start_raw: Option<i64>, end_raw: Option<i64>) -> Option<i64> {
    let start = start_raw.and_then(epoch_to_secs)?;
    let end = end_raw.and_then(epoch_to_secs)?;
    let minutes = (end - start) / 60;
    if minutes > 0 { Some(minutes) } else { None }
}

fn minimax_is_text_quota_model(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    lower == "general"
        || lower.starts_with("minimax-m")
        || lower.starts_with("m2.")
        || lower.starts_with("coding-plan")
}

fn minimax_make_interval_window(m: &MinimaxModelRemains) -> Option<UsageWindow> {
    if let Some(remaining_percent) = json_float(&m.current_interval_remaining_percent) {
        let unavailable = json_int(&m.current_interval_status) == Some(3)
            && json_int(&m.current_interval_total_count).unwrap_or(0) == 0
            && json_int(&m.current_interval_usage_count).unwrap_or(0) == 0
            && remaining_percent >= 100.0;
        if unavailable {
            return None;
        }
        let resets = seconds_until_reset(json_int(&m.end_time), json_int(&m.remains_time));
        return Some(UsageWindow {
            label: "5h".into(),
            used_percent: Some(minimax_remaining_percent_to_used(remaining_percent)),
            reset_secs: resets.map(|s| s.max(0) as u64),
            window_minutes: minimax_window_minutes(json_int(&m.start_time), json_int(&m.end_time)),
        });
    }
    let total = json_int(&m.current_interval_total_count)
        .unwrap_or(0)
        .max(0);
    let remaining = json_int(&m.current_interval_usage_count)?;
    if total <= 0 {
        return None;
    }
    let resets = seconds_until_reset(json_int(&m.end_time), json_int(&m.remains_time));
    Some(UsageWindow {
        label: "5h".into(),
        used_percent: Some(minimax_used_percent(total, remaining)),
        reset_secs: resets.map(|s| s.max(0) as u64),
        window_minutes: minimax_window_minutes(json_int(&m.start_time), json_int(&m.end_time)),
    })
}

fn minimax_make_weekly_window(m: &MinimaxModelRemains) -> Option<UsageWindow> {
    let model_name = m.model_name.as_deref().unwrap_or("");
    if !minimax_is_text_quota_model(model_name) {
        return None;
    }
    if let Some(remaining_percent) = json_float(&m.current_weekly_remaining_percent) {
        let resets = seconds_until_reset(
            json_int(&m.weekly_end_time),
            json_int(&m.weekly_remains_time),
        );
        return Some(UsageWindow {
            label: "7d".into(),
            used_percent: Some(minimax_remaining_percent_to_used(remaining_percent)),
            reset_secs: resets.map(|s| s.max(0) as u64),
            window_minutes: Some(7 * 24 * 60),
        });
    }
    let total = json_int(&m.current_weekly_total_count).unwrap_or(0).max(0);
    if total <= 0 {
        return None;
    }
    let remaining = json_int(&m.current_weekly_usage_count)?;
    let resets = seconds_until_reset(
        json_int(&m.weekly_end_time),
        json_int(&m.weekly_remains_time),
    )?;
    Some(UsageWindow {
        label: "7d".into(),
        used_percent: Some(minimax_used_percent(total, remaining)),
        reset_secs: Some(resets.max(0) as u64),
        window_minutes: Some(7 * 24 * 60),
    })
}

fn minimax_model_remains_list(payload: &MinimaxCodingPlanPayload) -> Vec<MinimaxModelRemains> {
    if let Some(data) = &payload.data
        && !data.model_remains.is_empty()
    {
        return data.model_remains.clone();
    }
    payload.model_remains_root.clone()
}

pub fn parse_minimax(body: &[u8]) -> Result<UsageData, String> {
    let payload: MinimaxCodingPlanPayload =
        serde_json::from_slice(body).map_err(|e| format!("minimax decode: {e}"))?;

    let base = payload
        .data
        .as_ref()
        .and_then(|d| d.base_resp.as_ref())
        .or(payload.base_resp.as_ref());
    if let Some(b) = base {
        let code = json_int(&b.status_code).unwrap_or(0);
        if code != 0 {
            return Err(format!("minimax status_code={code}"));
        }
    }

    let models = minimax_model_remains_list(&payload);
    if models.is_empty() {
        return Err("minimax: no model_remains".into());
    }

    // Primary: interval window from text models (general preferred)
    let text_models: Vec<_> = models
        .iter()
        .filter(|m| {
            m.model_name
                .as_deref()
                .map(minimax_is_text_quota_model)
                .unwrap_or(true)
        })
        .collect();

    let primary = text_models
        .iter()
        .find(|m| {
            m.model_name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case("general"))
        })
        .and_then(|m| minimax_make_interval_window(m))
        .or_else(|| {
            text_models
                .iter()
                .find_map(|m| minimax_make_interval_window(m))
        });

    // Secondary: weekly window
    let secondary = text_models
        .iter()
        .find_map(|m| minimax_make_weekly_window(m));

    let mut windows = Vec::new();
    if let Some(w) = primary {
        windows.push(w);
    }
    if let Some(w) = secondary {
        windows.push(w);
    }

    Ok(UsageData {
        windows,
        pools: vec![],
    })
}

// ── OpenRouter fetcher ─────────────────────────────────────────────────

pub fn parse_openrouter(body: &[u8]) -> Result<UsageData, String> {
    let response: OpenRouterCreditsResponse =
        serde_json::from_slice(body).map_err(|e| format!("openrouter decode: {e}"))?;

    let total = response
        .data
        .total_credits
        .as_f64()
        .ok_or("openrouter: total_credits not a number")?;
    let usage = response
        .data
        .total_usage
        .as_f64()
        .ok_or("openrouter: total_usage not a number")?;

    let remaining = (total - usage).max(0.0);

    Ok(UsageData {
        windows: vec![],
        pools: vec![CreditPool {
            id: "credits".into(),
            label: "OpenRouter credits".into(),
            remaining: Some((remaining * 100.0).floor() / 100.0),
            total: Some((total * 100.0).floor() / 100.0),
            unit: "USD".into(),
        }],
    })
}

// ── Zai fetcher ────────────────────────────────────────────────────────

fn zai_get_window_minutes(unit: i64, number: i64) -> Option<i64> {
    if number <= 0 {
        return None;
    }
    match unit {
        5 => {
            if number == 1 {
                None // marker, not a duration
            } else {
                Some(number)
            }
        }
        3 => Some(number * 60),
        1 => Some(number * 24 * 60),
        6 => Some(number * 7 * 24 * 60),
        _ => None,
    }
}

pub fn parse_zai(body: &[u8]) -> Result<UsageData, String> {
    let response: ZaiQuotaLimitResponse =
        serde_json::from_slice(body).map_err(|e| format!("zai decode: {e}"))?;

    if !response.success || response.code != 200 {
        return Err(format!("zai API error: code={}", response.code));
    }

    let data = response.data.ok_or("zai: response missing data")?;

    let mut windows = Vec::new();
    for limit in data.limits {
        if limit.limit_type != "TOKENS_LIMIT" && limit.limit_type != "TIME_LIMIT" {
            continue;
        }
        let used_percent = (limit.percentage as f64).clamp(0.0, 100.0);
        let window_minutes = zai_get_window_minutes(limit.unit, limit.number);
        let resets = limit.next_reset_time.and_then(|ms| {
            let reset_ms = ms / 1000;
            let n = now_secs();
            (reset_ms > n).then_some((reset_ms - n) as u64)
        });

        let label = match limit.limit_type.as_str() {
            "TOKENS_LIMIT" | "TIME_LIMIT" => {
                match window_minutes {
                    // Unstated duration (e.g. TIME_LIMIT marker): display as
                    // 30d; the underlying window_minutes stays None.
                    None => "30d".into(),
                    Some(m) => {
                        if m >= 10080 {
                            format!("{}w", m / 10080)
                        } else if m >= 1440 {
                            format!("{}d", m / 1440)
                        } else if m >= 60 {
                            format!("{}h", m / 60)
                        } else {
                            format!("{}m", m)
                        }
                    }
                }
            }
            _ => continue,
        };

        windows.push(UsageWindow {
            label,
            used_percent: Some(used_percent),
            reset_secs: resets,
            window_minutes,
        });
    }

    // Sort by window length: shortest first (insula pattern)
    windows.sort_by_key(|w| w.window_minutes.unwrap_or(i64::MAX));

    Ok(UsageData {
        windows,
        pools: vec![],
    })
}

// ── OpenCode-Go fetcher (official Zen usage API) ──────────────────────

/// Official Go subscription usage route, served by opencode.ai's inference
/// proxy (see `packages/console/app/src/lib/inference-proxy.ts`:
/// `"GET /zen/go/v1/usage": "/go/v1/usage"`). Appended to the upstream's
/// `base_url`, so the default upstream hits
/// `https://opencode.ai/zen/go/v1/usage`.
///
/// Authenticated with the same inference API key the upstream already uses
/// for model traffic (`Authorization: Bearer`). This replaced a
/// browser-session cookie scrape of the retired console HTML page, then an
/// undocumented `/console/api/go/status` console route that rejected the
/// cookie — the supported route needs no cookie, no session and no HTML.
const OPENCODE_GO_USAGE_PATH: &str = "/usage";
const OPENCODE_GO_DEFAULT_BASE: &str = "https://opencode.ai/zen/go/v1";

/// The three subscription windows. `percent` is already percent-scale
/// (0..=100), so it is clamped by `normalize_percent`, never rescaled.
#[derive(Debug, Deserialize)]
struct OcGoUsageResponse {
    usage: Option<OcGoUsageWindows>,
}

#[derive(Debug, Deserialize)]
struct OcGoUsageWindows {
    rolling: Option<OcGoUsageMeter>,
    weekly: Option<OcGoUsageMeter>,
    monthly: Option<OcGoUsageMeter>,
}

#[derive(Debug, Deserialize)]
struct OcGoUsageMeter {
    // The response also carries `status` ("ok" | "rate-limited"); it is not
    // interpreted — `percent` already carries the pressure — so it is not
    // parsed. Unknown JSON fields are ignored by serde.
    #[serde(default)]
    percent: Option<f64>,
    #[serde(rename = "resetsAt", default)]
    resets_at: Option<String>,
}

/// Parse `GET /zen/go/v1/usage` into usage windows. Window cadence is
/// documented at opencode.ai/v2/docs/console/go: rolling = 5h, weekly = 7d,
/// monthly = the billing period (30d here).
fn parse_opencode_go_usage(body: &str, now: i64) -> Result<UsageData, String> {
    let payload: OcGoUsageResponse =
        serde_json::from_str(body).map_err(|e| format!("opencode usage parse: {e}"))?;
    let Some(meters) = payload.usage else {
        return Err("opencode: response has no usage windows".into());
    };
    let mut windows = Vec::new();
    for (meter, label, minutes) in [
        (meters.rolling, "5h", 300),
        (meters.weekly, "7d", 10080),
        (meters.monthly, "30d", 43200),
    ] {
        let Some(m) = meter else { continue };
        windows.push(UsageWindow {
            label: label.to_string(),
            used_percent: m.percent.map(normalize_percent),
            reset_secs: oc_go_reset_secs(m.resets_at.as_deref(), now),
            window_minutes: Some(minutes),
        });
    }
    if windows.is_empty() {
        return Err("opencode: response has no usage windows".into());
    }
    Ok(UsageData {
        windows,
        pools: vec![],
    })
}

/// Seconds until the window resets; None when resetsAt is absent/unparseable.
fn oc_go_reset_secs(resets_at: Option<&str>, now: i64) -> Option<u64> {
    let ts = parse_time_value(&serde_json::Value::String(resets_at?.to_string()))?;
    Some((ts - now).max(0) as u64)
}

/// RFC3339 string (fractional seconds ok) or epoch number -> unix seconds.
fn parse_time_value(val: &serde_json::Value) -> Option<i64> {
    if let Some(n) = val.as_f64() {
        if n > 1_000_000_000_000.0 {
            return Some((n / 1000.0) as i64);
        }
        if n > 1_000_000_000.0 {
            return Some(n as i64);
        }
        return None;
    }
    if let Some(s) = val.as_str() {
        let t = s.trim();
        if let Ok(n) = t.parse::<f64>() {
            return parse_time_value(&serde_json::Value::from(n));
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
            return Some(dt.timestamp());
        }
    }
    None
}

async fn fetch_opencode_go(
    client: &reqwest::Client,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<UsageData, String> {
    let base = base_url.unwrap_or(OPENCODE_GO_DEFAULT_BASE);
    let url = format!("{}{}", base.trim_end_matches('/'), OPENCODE_GO_USAGE_PATH);
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("opencode usage request: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("opencode usage body: {e}"))?;
    if !status.is_success() {
        return Err(format!("opencode usage HTTP {status}: {body}"));
    }
    parse_opencode_go_usage(&body, now_secs())
}

// ── Background fetcher ─────────────────────────────────────────────────

/// Configuration for one upstream's billing fetch.
#[derive(Clone)]
pub struct FetcherConfig {
    pub kind: String,
    pub provider_name: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub codex_manager: Option<Arc<crate::codex_oauth::CodexTokenManager>>,
}

impl FetcherConfig {
    /// Create a UsageProvider from this config.
    fn to_provider(&self, client: reqwest::Client) -> Option<Box<dyn UsageProvider>> {
        match self.kind.as_str() {
            "minimax" => {
                let api_key = self.api_key.clone()?;
                Some(Box::new(MinimaxProvider { client, api_key }))
            }
            "openrouter" => {
                let api_key = self.api_key.clone()?;
                Some(Box::new(OpenRouterProvider { client, api_key }))
            }
            "zai" => {
                let api_key = self.api_key.clone()?;
                let base_url = self.base_url.clone();
                Some(Box::new(ZaiProvider {
                    client,
                    api_key,
                    base_url,
                }))
            }
            "opencode-go" => {
                let api_key = self.api_key.clone()?;
                let base_url = self.base_url.clone();
                Some(Box::new(OpencodeGoProvider {
                    client,
                    api_key,
                    base_url,
                }))
            }
            "openai-codex" => {
                let manager = self.codex_manager.clone()?;
                Some(Box::new(CodexProvider {
                    client,
                    manager,
                    base_url: self.base_url.clone(),
                }))
            }
            _ => None,
        }
    }
}

/// Fetch usage from all upstreams that have billing endpoints.
pub async fn fetch_all(tracker: &UsageTracker, fetchers: Vec<FetcherConfig>) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();

    for fc in fetchers {
        let Some(provider) = fc.to_provider(client.clone()) else {
            tracing::debug!(
                "usage: no provider for {} ({}), skipping",
                fc.provider_name,
                fc.kind
            );
            continue;
        };
        tracing::debug!("usage: fetching for {} ({})", fc.provider_name, fc.kind);
        match provider.fetch().await {
            Ok(data) => {
                tracing::debug!(
                    "usage: ok for {}: {} windows",
                    fc.provider_name,
                    data.windows.len()
                );
                if !data.windows.is_empty() || !data.pools.is_empty() {
                    tracker.update(&fc.provider_name, data).await;
                }
            }
            Err(e) => {
                tracing::warn!("usage: failed for {}: {e}", fc.provider_name);
            }
        }
    }
}

// ── Provider implementations ─────────────────────────────────────────────

struct MinimaxProvider {
    client: reqwest::Client,
    api_key: String,
}

#[async_trait::async_trait]
impl UsageProvider for MinimaxProvider {
    fn name(&self) -> &str {
        "minimax"
    }
    async fn fetch(&self) -> Result<UsageData, String> {
        fetch_minimax(&self.client, &self.api_key).await
    }
}

struct OpenRouterProvider {
    client: reqwest::Client,
    api_key: String,
}

#[async_trait::async_trait]
impl UsageProvider for OpenRouterProvider {
    fn name(&self) -> &str {
        "openrouter"
    }
    async fn fetch(&self) -> Result<UsageData, String> {
        fetch_openrouter(&self.client, &self.api_key).await
    }
}

struct ZaiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: Option<String>,
}

#[async_trait::async_trait]
impl UsageProvider for ZaiProvider {
    fn name(&self) -> &str {
        "zai"
    }
    async fn fetch(&self) -> Result<UsageData, String> {
        fetch_zai(&self.client, &self.api_key, self.base_url.as_deref()).await
    }
}

struct OpencodeGoProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: Option<String>,
}

#[async_trait::async_trait]
impl UsageProvider for OpencodeGoProvider {
    fn name(&self) -> &str {
        "opencode-go"
    }
    async fn fetch(&self) -> Result<UsageData, String> {
        fetch_opencode_go(&self.client, &self.api_key, self.base_url.as_deref()).await
    }
}

struct CodexProvider {
    client: reqwest::Client,
    manager: Arc<crate::codex_oauth::CodexTokenManager>,
    base_url: Option<String>,
}

#[async_trait::async_trait]
impl UsageProvider for CodexProvider {
    fn name(&self) -> &str {
        "openai-codex"
    }

    async fn fetch(&self) -> Result<UsageData, String> {
        let access = self
            .manager
            .access()
            .await
            .map_err(|e| format!("Codex token: {e}"))?;
        let account_id = crate::codex_oauth::account_id_from_token(&access)
            .ok_or_else(|| "Codex access token has no chatgpt_account_id".to_string())?;
        let base = self
            .base_url
            .as_deref()
            .unwrap_or(crate::codex_oauth::DEFAULT_CODEX_BASE_URL);
        fetch_codex_usage(&self.client, &access, &account_id, base).await
    }
}

async fn fetch_minimax(client: &reqwest::Client, api_key: &str) -> Result<UsageData, String> {
    let url = "https://api.minimax.io/v1/api/openplatform/coding_plan/remains";
    let resp = client
        .get(url)
        .bearer_auth(api_key)
        .header("accept", "application/json")
        .header("MM-API-Source", "aiproxy")
        .send()
        .await
        .map_err(|e| format!("minimax request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("minimax HTTP {}", resp.status()));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("minimax body: {e}"))?;
    parse_minimax(&body)
}

async fn fetch_openrouter(client: &reqwest::Client, api_key: &str) -> Result<UsageData, String> {
    let url = "https://openrouter.ai/api/v1/credits";
    let resp = client
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("openrouter request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("openrouter HTTP {}", resp.status()));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("openrouter body: {e}"))?;
    parse_openrouter(&body)
}

async fn fetch_zai(
    client: &reqwest::Client,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<UsageData, String> {
    let base = base_url.unwrap_or("https://api.z.ai");
    let url = format!(
        "{}/api/monitor/usage/quota/limit",
        base.trim_end_matches('/')
    );
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("zai request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("zai HTTP {}", resp.status()));
    }
    let body = resp.bytes().await.map_err(|e| format!("zai body: {e}"))?;
    parse_zai(&body)
}

/// Spawn a background task that fetches usage from all upstreams periodically.
pub fn spawn_refresh(tracker: UsageTracker, fetchers: Vec<FetcherConfig>, interval_secs: u64) {
    if interval_secs == 0 || fetchers.is_empty() {
        return;
    }
    // Initial fetch immediately
    let tracker_clone = tracker.clone();
    let fetchers_clone = fetchers.clone();
    tokio::spawn(async move {
        fetch_all(&tracker_clone, fetchers_clone).await;
    });
    // Then refresh every interval_secs
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        interval.tick().await; // first tick is immediate
        loop {
            interval.tick().await;
            fetch_all(&tracker, fetchers.clone()).await;
        }
    });
}

async fn fetch_codex_usage(
    client: &reqwest::Client,
    access_token: &str,
    account_id: &str,
    base_url: &str,
) -> Result<UsageData, String> {
    let base = base_url.trim_end_matches('/');
    let mut last_error = None;
    for path in ["/wham/usage", "/codex/usage"] {
        let url = format!("{base}{path}");
        let response = match client
            .get(&url)
            .bearer_auth(access_token)
            .header("ChatGPT-Account-Id", account_id)
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(format!("Codex usage request: {error}"));
                continue;
            }
        };
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err("Codex usage HTTP 429".into());
        }
        if !status.is_success() {
            last_error = Some(format!("Codex usage HTTP {status}"));
            continue;
        }
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                last_error = Some(format!("Codex usage body: {error}"));
                continue;
            }
        };
        match parse_codex_usage(&body, now_secs()) {
            Ok(data) => return Ok(data),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "Codex usage unavailable".into()))
}

fn parse_codex_usage(body: &[u8], now: i64) -> Result<UsageData, String> {
    use serde_json::Value;

    let data: Value = serde_json::from_slice(body).map_err(|e| format!("Codex usage JSON: {e}"))?;
    let root = data
        .as_object()
        .ok_or("Codex usage response is not an object")?;
    let rate = root
        .get("rate_limit")
        .or_else(|| root.get("rateLimits"))
        .and_then(Value::as_object);
    let mut windows = Vec::new();
    if let Some(rate) = rate {
        for (key, alias, label) in [
            ("primary_window", "primary", "5h"),
            ("secondary_window", "secondary", "7d"),
        ] {
            if let Some(window) = rate
                .get(key)
                .or_else(|| rate.get(alias))
                .and_then(|raw| codex_window(raw, label, now))
            {
                windows.push(window);
            }
        }
    }

    if let Some(cap) = root
        .get("spend_control")
        .and_then(Value::as_object)
        .and_then(|control| control.get("individual_limit"))
        .and_then(Value::as_object)
    {
        let limit = codex_number(cap, &["limit"]);
        let used = codex_number(cap, &["used"]);
        let reached = root
            .get("spend_control")
            .and_then(Value::as_object)
            .and_then(|control| control.get("reached"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let percent = if reached {
            Some(100.0)
        } else {
            codex_number(cap, &["used_percent", "usedPercent"]).or_else(|| {
                match (used, limit.filter(|limit| *limit > 0.0)) {
                    (Some(used), Some(limit)) => Some(used / limit * 100.0),
                    _ => None,
                }
            })
        };
        windows.push(UsageWindow {
            label: "30d".into(),
            used_percent: percent.map(normalize_percent),
            reset_secs: codex_reset_secs(cap, now),
            window_minutes: Some(43200),
        });
    }

    let mut pools = Vec::new();
    if let Some(credits) = root.get("credits").and_then(Value::as_object) {
        if credits.get("unlimited").and_then(Value::as_bool) == Some(true) {
            pools.push(CreditPool {
                id: "credits".into(),
                label: "Codex credits".into(),
                remaining: None,
                total: None,
                unit: "unlimited".into(),
            });
        } else if let Some(balance) = codex_number(credits, &["balance"]) {
            pools.push(CreditPool {
                id: "credits".into(),
                label: "Codex credits".into(),
                remaining: Some(balance.max(0.0)),
                total: None,
                unit: "credits".into(),
            });
        }
    }

    if windows.is_empty() && pools.is_empty() {
        return Err("Codex usage response contains no usage limits".into());
    }
    Ok(UsageData { windows, pools })
}

fn codex_window(raw: &serde_json::Value, default_label: &str, now: i64) -> Option<UsageWindow> {
    let object = raw.as_object()?;
    let used_percent = codex_number(object, &["used_percent", "usedPercent"])?;
    let duration_secs = codex_number(object, &["limit_window_seconds", "limitWindowSeconds"]);
    let window_minutes = duration_secs.map(|seconds| (seconds / 60.0).ceil() as i64);
    let label = match window_minutes {
        Some(10080) => "7d".to_string(),
        Some(43200) => "30d".to_string(),
        Some(minutes) if minutes > 0 && minutes % 1440 == 0 => format!("{}d", minutes / 1440),
        Some(minutes) if minutes > 0 && minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) if minutes > 0 => format!("{}m", minutes),
        _ => default_label.to_string(),
    };
    Some(UsageWindow {
        label,
        used_percent: Some(normalize_percent(used_percent)),
        reset_secs: codex_reset_secs(object, now),
        window_minutes,
    })
}

fn codex_number(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        let value = object.get(*key).cloned();
        json_float(&value)
    })
}

fn codex_reset_secs(object: &serde_json::Map<String, serde_json::Value>, now: i64) -> Option<u64> {
    if let Some(after) = codex_number(object, &["reset_after_seconds", "resetAfterSeconds"]) {
        return Some(after.max(0.0) as u64);
    }
    let reset_at = codex_number(object, &["reset_at", "resetAt"])? as i64;
    let reset_at = epoch_to_secs(reset_at)?;
    Some(reset_at.saturating_sub(now).max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- tracker tests --
    #[test]
    fn codex_personal_plan_returns_five_hour_and_weekly_windows() {
        let json = r#"{
            "plan_type":"plus",
            "rate_limit":{
                "primary_window":{"used_percent":37.5,"limit_window_seconds":18000,"reset_after_seconds":5400},
                "secondary_window":{"used_percent":22,"limit_window_seconds":604800,"reset_after_seconds":7200}
            }
        }"#;
        let usage = parse_codex_usage(json.as_bytes(), 1_800_000_000).unwrap();
        let five_hour = usage.windows.iter().find(|w| w.label == "5h").unwrap();
        assert_eq!(five_hour.used_percent, Some(37.5));
        assert_eq!(five_hour.reset_secs, Some(5400));
        assert_eq!(five_hour.window_minutes, Some(300));
        let weekly = usage.windows.iter().find(|w| w.label == "7d").unwrap();
        assert_eq!(weekly.used_percent, Some(22.0));
        assert_eq!(weekly.reset_secs, Some(7200));
        assert_eq!(weekly.window_minutes, Some(10080));
    }

    #[test]
    fn codex_business_plan_returns_individual_spend_cap() {
        let json = r#"{
            "plan_type":"business",
            "rate_limit":null,
            "spend_control":{"reached":false,"individual_limit":{
                "unit":"credit","limit":"2400","used":"1434.58","remaining":"965.42",
                "used_percent":60,"remaining_percent":40,"reset_after_seconds":11831
            }}
        }"#;
        let usage = parse_codex_usage(json.as_bytes(), 1_800_000_000).unwrap();
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].label, "30d");
        assert_eq!(usage.windows[0].used_percent, Some(60.0));
        assert_eq!(usage.windows[0].reset_secs, Some(11831));
        assert_eq!(usage.windows[0].window_minutes, Some(43200));
    }

    #[test]
    fn codex_unlimited_credits_is_a_valid_usage_report() {
        let usage = parse_codex_usage(
            br#"{"plan_type":"business","credits":{"has_credits":true,"unlimited":true,"balance":null}}"#,
            1_800_000_000,
        )
        .unwrap();
        assert!(usage.windows.is_empty());
        assert_eq!(usage.pools.len(), 1);
        assert_eq!(usage.pools[0].unit, "unlimited");
        assert_eq!(usage.pools[0].remaining, None);
    }

    #[tokio::test]
    async fn codex_usage_falls_back_and_sends_oauth_account_headers() {
        use axum::{
            Json, Router,
            http::{HeaderMap, StatusCode},
            routing::get,
        };
        use serde_json::json;
        use std::sync::Mutex;

        let seen = Arc::new(Mutex::new(Vec::<(String, HeaderMap)>::new()));
        let wham_seen = seen.clone();
        let codex_seen = seen.clone();
        let app = Router::new()
            .route(
                "/backend-api/wham/usage",
                get(move |headers: HeaderMap| {
                    let seen = wham_seen.clone();
                    async move {
                        seen.lock()
                            .unwrap()
                            .push(("/backend-api/wham/usage".into(), headers));
                        StatusCode::NOT_FOUND
                    }
                }),
            )
            .route(
                "/backend-api/codex/usage",
                get(move |headers: HeaderMap| {
                    let seen = codex_seen.clone();
                    async move {
                        seen.lock()
                            .unwrap()
                            .push(("/backend-api/codex/usage".into(), headers));
                        Json(json!({"rate_limit":{"primary_window":{
                            "used_percent":12,"limit_window_seconds":18000,"reset_after_seconds":300
                        }}}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let result = fetch_codex_usage(
            &reqwest::Client::new(),
            "access-token",
            "account-123",
            &format!("http://{addr}/backend-api"),
        )
        .await
        .unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            vec!["/backend-api/wham/usage", "/backend-api/codex/usage"]
        );
        for (_, headers) in seen.iter() {
            assert_eq!(headers["authorization"], "Bearer access-token");
            assert_eq!(headers["chatgpt-account-id"], "account-123");
            assert_eq!(headers["accept"], "application/json");
        }
        assert_eq!(result.windows[0].used_percent, Some(12.0));
    }

    #[tokio::test]
    async fn codex_fetcher_uses_token_manager_and_updates_usage_snapshot() {
        use axum::{Json, Router, http::HeaderMap, routing::get};
        use serde_json::json;
        use std::sync::Mutex;

        let seen = Arc::new(Mutex::new(None::<HeaderMap>));
        let route_seen = seen.clone();
        let app = Router::new().route(
            "/backend-api/wham/usage",
            get(move |headers: HeaderMap| {
                let seen = route_seen.clone();
                async move {
                    *seen.lock().unwrap() = Some(headers);
                    Json(json!({"rate_limit":{"primary_window":{
                        "used_percent":44,"limit_window_seconds":18000,"reset_after_seconds":900
                    }}}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let payload = serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id":"acct-7"}
        });
        let token = format!(
            "header.{}.sig",
            crate::codex_oauth::base64_url_encode(payload.to_string().as_bytes())
        );
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(crate::codex_oauth::CodexTokenManager::new(
            &dir.path().join("codex-state.json"),
            "http://127.0.0.1:1/oauth/token",
        ));
        manager
            .store_tokens(crate::codex_oauth::Tokens {
                access: token.clone(),
                refresh: "refresh-token".into(),
                expires_at_ms: now_millis_for_test() + 3 * 3_600_000,
            })
            .await
            .unwrap();
        let tracker = UsageTracker::new();
        fetch_all(
            &tracker,
            vec![FetcherConfig {
                kind: "openai-codex".into(),
                provider_name: "openai-codex=alice".into(),
                api_key: None,
                base_url: Some(format!("http://{addr}/backend-api")),
                codex_manager: Some(manager),
            }],
        )
        .await;

        let snapshot = tracker.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].provider, "openai-codex=alice");
        assert_eq!(snapshot[0].windows[0].used_percent, Some(44.0));
        let headers = seen.lock().unwrap().clone().unwrap();
        assert_eq!(headers["authorization"], format!("Bearer {token}"));
        assert_eq!(headers["chatgpt-account-id"], "acct-7");
    }

    fn now_millis_for_test() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn new_tracker_is_empty() {
        let t = UsageTracker::new();
        assert!(t.inner.blocking_read().is_empty());
    }

    #[tokio::test]
    async fn update_and_snapshot() {
        let t = UsageTracker::new();
        t.update(
            "minimax",
            UsageData {
                windows: vec![UsageWindow {
                    label: "5h".into(),
                    used_percent: Some(75.0),
                    reset_secs: Some(1800),
                    window_minutes: Some(300),
                }],
                pools: vec![],
            },
        )
        .await;
        let snap = t.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].windows.len(), 1);
        assert_eq!(snap[0].windows[0].used_percent, Some(75.0));
        assert!(snap[0].pools.is_empty());
    }

    #[tokio::test]
    async fn mixed_sources_per_provider() {
        let t = UsageTracker::new();
        t.update(
            "openai",
            UsageData {
                windows: vec![UsageWindow {
                    label: "requests".into(),
                    used_percent: Some(50.0),
                    reset_secs: Some(1800),
                    window_minutes: None,
                }],
                pools: vec![],
            },
        )
        .await;
        t.update(
            "minimax",
            UsageData {
                windows: vec![UsageWindow {
                    label: "5h".into(),
                    used_percent: Some(30.0),
                    reset_secs: None,
                    window_minutes: Some(300),
                }],
                pools: vec![],
            },
        )
        .await;
        let snap = t.snapshot().await;
        assert_eq!(snap.len(), 2);
        let o = snap.iter().find(|u| u.provider == "openai").unwrap();
        assert_eq!(o.windows.len(), 1);
        let m = snap.iter().find(|u| u.provider == "minimax").unwrap();
        assert_eq!(m.windows.len(), 1);
    }

    // -- minimax tests --

    #[test]
    fn minimax_basic_payload() {
        let start = 1_800_000_000_000_i64;
        let end = start + 5 * 60 * 60 * 1000;
        let json = format!(
            r#"{{"base_resp":{{"status_code":0}},"model_remains":[{{"model_name":"M2.7","current_interval_total_count":1000,"current_interval_usage_count":250,"start_time":{start},"end_time":{end},"remains_time":240000}}]}}"#
        );
        let usage = parse_minimax(json.as_bytes()).unwrap();
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].used_percent, Some(75.0));
        assert_eq!(usage.windows[0].window_minutes, Some(300));
    }

    #[test]
    fn minimax_weekly_window() {
        // Use future timestamps (2027) so seconds_until_reset returns Some
        let start = 1_800_000_000_000_i64;
        let end = start + 5 * 60 * 60 * 1000;
        let week_start = start - 2 * 24 * 60 * 60 * 1000;
        let week_end = week_start + 7 * 24 * 60 * 60 * 1000;
        let json = format!(
            r#"{{"base_resp":{{"status_code":0}},"model_remains":[{{"model_name":"MiniMax-M1","current_interval_total_count":1000,"current_interval_usage_count":250,"start_time":{start},"end_time":{end},"current_weekly_total_count":6000,"current_weekly_usage_count":5376,"weekly_start_time":{week_start},"weekly_end_time":{week_end}}}]}}"#
        );
        let usage = parse_minimax(json.as_bytes()).unwrap();
        assert_eq!(usage.windows.len(), 2);
        let weekly = usage.windows.iter().find(|w| w.label == "7d").unwrap();
        assert!((weekly.used_percent.unwrap() - 10.4).abs() < 0.1);
    }

    // -- opencode-go usage API tests --

    /// Captured live from `GET https://opencode.ai/zen/go/v1/usage` with a real
    /// Go subscription key (2026-10-03). `percent` is already percent-scale and
    /// pre-rounded upstream, so it must pass through unscaled.
    const OC_GO_USAGE_REAL: &str = r#"{"usage":{
        "rolling":{"status":"ok","percent":0,"resetsAt":"2026-10-03T05:59:19.000Z"},
        "weekly":{"status":"ok","percent":1,"resetsAt":"2026-10-05T00:00:00.000Z"},
        "monthly":{"status":"ok","percent":0,"resetsAt":"2026-10-29T18:38:03.000Z"}}}"#;

    #[test]
    fn opencode_usage_api_parses_three_windows() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-03T00:00:00.000Z")
            .unwrap()
            .timestamp();
        let usage = parse_opencode_go_usage(OC_GO_USAGE_REAL, now).unwrap();
        assert_eq!(usage.windows.len(), 3, "5h + 7d + 30d");

        let rolling = usage.windows.iter().find(|w| w.label == "5h").unwrap();
        assert_eq!(rolling.used_percent, Some(0.0));
        assert_eq!(rolling.window_minutes, Some(300));
        assert_eq!(rolling.reset_secs, Some(21_559)); // 05:59:19

        let weekly = usage.windows.iter().find(|w| w.label == "7d").unwrap();
        assert_eq!(weekly.used_percent, Some(1.0));
        assert_eq!(weekly.window_minutes, Some(10_080));
        assert_eq!(weekly.reset_secs, Some(172_800)); // +2 days

        let monthly = usage.windows.iter().find(|w| w.label == "30d").unwrap();
        assert_eq!(monthly.used_percent, Some(0.0));
        assert_eq!(monthly.window_minutes, Some(43_200));
        assert_eq!(monthly.reset_secs, Some(2_313_483)); // +26d 18:38:03
    }

    /// Sub-1% must survive: the reason `normalize_percent` clamps instead of
    /// rescaling. 0.3% must stay 0.3, never become 30.
    #[test]
    fn opencode_usage_api_keeps_sub_one_percent() {
        let body = r#"{"usage":{"rolling":{"percent":0.3,"resetsAt":"2026-10-03T05:59:19.000Z"}}}"#;
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-03T00:00:00.000Z")
            .unwrap()
            .timestamp();
        let usage = parse_opencode_go_usage(body, now).unwrap();
        assert_eq!(usage.windows[0].used_percent, Some(0.3));
    }

    #[test]
    fn opencode_usage_api_rejects_empty_and_malformed() {
        let now = 1_790_979_600;
        assert!(parse_opencode_go_usage(r#"{"usage":{}}"#, now).is_err());
        assert!(parse_opencode_go_usage("not json", now).is_err());
    }

    #[test]
    fn minimax_error_status() {
        let json = r#"{"base_resp":{"status_code":1004,"status_msg":"cookie required"}}"#;
        let err = parse_minimax(json.as_bytes()).unwrap_err();
        assert!(err.contains("1004"));
    }

    // -- openrouter tests --

    #[test]
    fn openrouter_credits() {
        let json = r#"{"data":{"total_credits":25,"total_usage":19.506}}"#;
        let usage = parse_openrouter(json.as_bytes()).unwrap();
        assert!(usage.windows.is_empty());
        assert_eq!(usage.pools.len(), 1);
        assert_eq!(usage.pools[0].id, "credits");
        assert_eq!(usage.pools[0].unit, "USD");
        let remaining = usage.pools[0].remaining.unwrap();
        assert!((remaining - 5.49).abs() < 0.01);
    }

    #[test]
    fn openrouter_overdrawn() {
        let json = r#"{"data":{"total_credits":5,"total_usage":6}}"#;
        let usage = parse_openrouter(json.as_bytes()).unwrap();
        assert_eq!(usage.pools[0].remaining, Some(0.0));
    }

    // -- zai tests --

    #[test]
    fn zai_token_and_time_limits() {
        let json = r#"{"code":200,"success":true,"data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":20,"nextResetTime":1782135879000},{"type":"TIME_LIMIT","unit":1,"number":30,"percentage":10,"nextResetTime":1782135879000}]}}"#;
        let usage = parse_zai(json.as_bytes()).unwrap();
        assert_eq!(usage.windows.len(), 2);
        // Shortest window first
        assert_eq!(usage.windows[0].window_minutes, Some(300));
        assert_eq!(usage.windows[0].used_percent, Some(20.0));
        assert_eq!(usage.windows[1].window_minutes, Some(43200));
        assert_eq!(usage.windows[1].used_percent, Some(10.0));
    }

    #[test]
    fn zai_marker_time_limit() {
        let json = r#"{"code":200,"success":true,"data":{"limits":[{"type":"TIME_LIMIT","unit":5,"number":1,"percentage":14,"nextResetTime":1784706344993}]}}"#;
        let usage = parse_zai(json.as_bytes()).unwrap();
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].window_minutes, None); // marker, not duration
        assert_eq!(usage.windows[0].label, "30d"); // displays as 30d, never 0m
        assert_eq!(usage.windows[0].used_percent, Some(14.0));
    }

    #[test]
    fn zai_error_code() {
        let json = r#"{"code":401,"success":false,"msg":"unauthorized"}"#;
        let err = parse_zai(json.as_bytes()).unwrap_err();
        assert!(err.contains("401"));
    }

    // -- label normalization tests --

    #[test]
    fn normalize_weekly_family_to_7d() {
        for l in ["weekly", "week", "1w"] {
            assert_eq!(normalize_window_label(l, None), "7d");
        }
    }

    #[test]
    fn normalize_monthly_family_to_30d() {
        for l in ["monthly", "month"] {
            assert_eq!(normalize_window_label(l, None), "30d");
        }
    }

    #[test]
    fn normalize_minutes_win_over_label() {
        assert_eq!(normalize_window_label("1w", Some(10080)), "7d");
        assert_eq!(normalize_window_label("4w", Some(43200)), "30d");
        assert_eq!(normalize_window_label("5h", Some(300)), "5h");
    }

    #[tokio::test]
    async fn update_normalizes_labels() {
        let t = UsageTracker::new();
        t.update(
            "zai",
            UsageData {
                windows: vec![
                    UsageWindow {
                        label: "1w".into(),
                        used_percent: Some(10.0),
                        reset_secs: None,
                        window_minutes: Some(10080),
                    },
                    UsageWindow {
                        label: "weekly".into(),
                        used_percent: Some(20.0),
                        reset_secs: None,
                        window_minutes: None,
                    },
                    UsageWindow {
                        label: "monthly".into(),
                        used_percent: Some(30.0),
                        reset_secs: None,
                        window_minutes: None,
                    },
                ],
                pools: vec![],
            },
        )
        .await;
        let snap = t.snapshot().await;
        let labels: Vec<_> = snap[0].windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, vec!["7d", "7d", "30d"]);
    }
}
