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

/// Normalize percent: values in 0..=1 are treated as fractions and scaled to 0..100.
fn normalize_percent(p: f64) -> f64 {
    let p = if (0.0..=1.0).contains(&p) {
        p * 100.0
    } else {
        p
    };
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

// ── OpenCode-Go fetcher (browser cookie) ──────────────────────────────

/// OPAQUE-UPSTREAM-CONSTANT: copied from insula, unvalidatable here.
/// Hash naming the server function that lists workspaces.
const OPENCODE_WORKSPACE_SERVER_ID: &str =
    "def39973159c7f0483d8793a822b8dbb10d067e12c65455fcb4608459ba0234f";
const OPENCODE_BASE: &str = "https://opencode.ai";
const OPENCODE_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

/// Percent keys (insula PERCENT_KEYS, to the T).
const OPENCODE_PERCENT_KEYS: &[&str] = &[
    "usagePercent",
    "usedPercent",
    "percentUsed",
    "percent",
    "usage_percent",
    "used_percent",
    "utilization",
    "utilizationPercent",
    "utilization_percent",
    "usage",
];
/// Relative-reset keys (insula RESET_IN_KEYS, to the T).
const OPENCODE_RESET_IN_KEYS: &[&str] = &[
    "resetInSec",
    "resetInSeconds",
    "resetSeconds",
    "reset_sec",
    "reset_in_sec",
    "resetsInSec",
    "resetsInSeconds",
    "resetIn",
    "resetSec",
];
/// Absolute-reset keys (insula RESET_AT_KEYS, to the T).
const OPENCODE_RESET_AT_KEYS: &[&str] = &[
    "resetAt",
    "resetsAt",
    "reset_at",
    "resets_at",
    "nextReset",
    "next_reset",
    "renewAt",
    "renew_at",
];
/// Window dict aliases (insula parse_usage_dict, to the T).
const OPENCODE_ROLLING_KEYS: &[&str] = &[
    "rollingUsage",
    "rolling",
    "rolling_usage",
    "rollingWindow",
    "rolling_window",
];
const OPENCODE_WEEKLY_KEYS: &[&str] = &[
    "weeklyUsage",
    "weekly",
    "weekly_usage",
    "weeklyWindow",
    "weekly_window",
];
const OPENCODE_MONTHLY_KEYS: &[&str] = &[
    "monthlyUsage",
    "monthly",
    "monthly_usage",
    "monthlyWindow",
    "monthly_window",
];
/// Nested JSON wrappers searched before giving up (insula try_parse_json_usage).
const OPENCODE_NESTED_KEYS: &[&str] = &["data", "result", "usage", "billing", "payload"];

/// Generate X-Server-Instance header value (unique per request).
fn server_instance_header() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("server-fn:{nanos:x}{:x}", std::process::id())
}

/// Parse opencode-go usage from the subscription response.
pub fn parse_opencode_go(text: &str) -> Result<UsageData, String> {
    if let Some(usage) = try_parse_opencode_go_json(text) {
        return Ok(usage);
    }
    try_parse_opencode_go_js(text)
}

fn try_parse_opencode_go_json(text: &str) -> Option<UsageData> {
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    let root = root.as_object()?;
    if let Some(usage) = parse_opencode_go_dict(root) {
        return Some(usage);
    }
    // Nested wrappers (insula try_parse_json_usage).
    for key in OPENCODE_NESTED_KEYS {
        if let Some(nested) = root.get(*key).and_then(|v| v.as_object())
            && let Some(usage) = parse_opencode_go_dict(nested)
        {
            return Some(usage);
        }
    }
    None
}

fn parse_opencode_go_dict(dict: &serde_json::Map<String, serde_json::Value>) -> Option<UsageData> {
    let rolling = find_window_dict(dict, OPENCODE_ROLLING_KEYS);
    let weekly = find_window_dict(dict, OPENCODE_WEEKLY_KEYS);
    let monthly = find_window_dict(dict, OPENCODE_MONTHLY_KEYS);

    let mut windows = Vec::new();
    if let Some(w) = rolling.and_then(|m| opencode_window_from_map(m, "5h", 300)) {
        windows.push(w);
    }
    if let Some(w) = weekly.and_then(|m| opencode_window_from_map(m, "7d", 10080)) {
        windows.push(w);
    }
    if let Some(w) = monthly.and_then(|m| opencode_window_from_map(m, "30d", 43200)) {
        windows.push(w);
    }

    (!windows.is_empty()).then_some(UsageData {
        windows,
        pools: vec![],
    })
}

fn find_window_dict<'a>(
    dict: &'a serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    keys.iter()
        .find_map(|k| dict.get(*k).and_then(|v| v.as_object()))
}

/// Parse a date/reset value: epoch ms/s numbers, numeric strings, RFC3339 (insula parse_date_value).
fn opencode_parse_date_value(val: &serde_json::Value) -> Option<i64> {
    if let Some(n) = val.as_f64() {
        if !n.is_finite() || n >= i64::MAX as f64 {
            return None;
        }
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
            return opencode_parse_date_value(&serde_json::Value::from(n));
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
            return Some(dt.timestamp());
        }
    }
    None
}

/// Seconds until reset for a window map: relative resetIn* wins, else absolute
/// resetAt* minus now; None when absent (insula reset_epoch_for_map, window kept).
fn opencode_reset_secs(map: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    if let Some(sec) = OPENCODE_RESET_IN_KEYS
        .iter()
        .find_map(|k| map.get(*k))
        .and_then(|v| v.as_f64())
    {
        if !sec.is_finite() {
            return None;
        }
        return Some((sec as i64).max(0) as u64);
    }
    for key in OPENCODE_RESET_AT_KEYS {
        if let Some(v) = map.get(*key)
            && let Some(epoch) = opencode_parse_date_value(v)
        {
            return Some((epoch - now_secs()).max(0) as u64);
        }
    }
    None
}

fn opencode_window_from_map(
    map: &serde_json::Map<String, serde_json::Value>,
    label: &str,
    window_minutes: i64,
) -> Option<UsageWindow> {
    let used_percent = OPENCODE_PERCENT_KEYS
        .iter()
        .find_map(|k| map.get(*k))
        .and_then(|v| v.as_f64())?;
    if !used_percent.is_finite() {
        return None;
    }
    // Reset is optional: a window with percent but no reset is kept with
    // reset_secs=None (insula percent_without_reset_keeps_window).
    Some(UsageWindow {
        label: label.into(),
        used_percent: Some(normalize_percent(used_percent)),
        reset_secs: opencode_reset_secs(map),
        window_minutes: Some(window_minutes),
    })
}

/// Parse opencode-go JS object notation.
fn try_parse_opencode_go_js(text: &str) -> Result<UsageData, String> {
    let mut windows = Vec::new();

    for (label, key, minutes) in [
        ("5h", "rollingUsage", 300_i64),
        ("7d", "weeklyUsage", 10080_i64),
        ("30d", "monthlyUsage", 43200_i64),
    ] {
        match find_js_window(text, key) {
            Some((percent, reset)) => {
                tracing::debug!("opencode-go: {key} parsed, percent={percent:?}, reset={reset:?}");
                if !percent.is_finite() {
                    continue;
                }
                // Reset optional: keep the window with reset_secs=None when
                // absent (insula percent_without_reset_keeps_window).
                windows.push(UsageWindow {
                    label: label.into(),
                    used_percent: Some(normalize_percent(percent)),
                    reset_secs: reset
                        .filter(|r| r.is_finite())
                        .map(|r| (r as i64).max(0) as u64),
                    window_minutes: Some(minutes),
                });
            }
            None => {
                tracing::debug!("opencode-go: {key}: no usable block in response");
            }
        }
    }

    (!windows.is_empty())
        .then_some(UsageData {
            windows,
            pools: vec![],
        })
        .ok_or_else(|| "opencode-go: no usage windows found".into())
}

/// Find a window's (percent, reset?) scanning EVERY occurrence of key: a
/// `:null` occurrence (e.g. `monthlyUsage:null` in a plan-less record) is
/// skipped, first occurrence yielding a percent wins.
fn find_js_window(text: &str, key: &str) -> Option<(f64, Option<f64>)> {
    let mut search = text;
    while let Some(pos) = search.find(key) {
        let after_key = &search[pos + key.len()..];
        let trimmed = after_key.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        if trimmed.starts_with("null") {
            search = &search[pos + 1..];
            continue;
        }
        if let Some(block) = extract_js_block(&search[pos..], key)
            && let Some(percent) = OPENCODE_PERCENT_KEYS
                .iter()
                .find_map(|k| extract_js_number(block, k))
        {
            let reset = OPENCODE_RESET_IN_KEYS
                .iter()
                .find_map(|k| extract_js_number(block, k));
            return Some((percent, reset));
        }
        if search.len() <= pos + 1 {
            break;
        }
        search = &search[pos + 1..];
    }
    None
}

/// Bound one window's block at its closing brace, or at a 2500-byte budget
/// rounded down to a char boundary when the brace is missing (insula window_block).
fn extract_js_block<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let pos = text.find(key)?;
    let slice = &text[pos..];
    let start = slice.find('{')?;
    let rest = &slice[start..];
    let end = rest.find('}').unwrap_or_else(|| {
        let mut cap = rest.len().min(2500);
        while cap > 0 && !rest.is_char_boundary(cap) {
            cap -= 1;
        }
        cap
    });
    Some(&rest[..end])
}

/// First number after any occurrence of key, scanning all occurrences
/// (insula field_after_key).
fn extract_js_number(block: &str, key: &str) -> Option<f64> {
    let mut search = block;
    while let Some(pos) = search.find(key) {
        let after = &search[pos + key.len()..];
        let after = after.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        let num_end = after
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit() && *c != '.')
            .map(|(i, _)| i)
            .unwrap_or(after.len());
        if num_end > 0
            && let Ok(n) = after[..num_end].trim().parse()
        {
            return Some(n);
        }
        if search.len() <= pos + 1 {
            break;
        }
        search = &search[pos + 1..];
    }
    None
}

/// Detect signed-out response (insula looks_signed_out, to the T).
fn looks_signed_out(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("login")
        || lower.contains("sign in")
        || lower.contains("auth/authorize")
        || lower.contains("not associated with an account")
        || lower.contains("actor of type \"public\"")
}

/// Whether the /go page states this workspace has no Go plan (insula
/// looks_unsubscribed, to the T). All three nulls required: a single null can
/// be a rollout or lapsed payment on a live plan.
fn looks_unsubscribed(text: &str) -> bool {
    text.contains("subscription:null")
        && text.contains("subscriptionID:null")
        && text.contains("subscriptionPlan:null")
}

/// Whether a _server response states "there is nothing here" (insula
/// is_explicit_null, to the T): bare null, JSON null, or the envelope value
/// slot `,null)`.
fn is_explicit_null(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return true;
    }
    if serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .map(|v| v.is_null())
        .unwrap_or(false)
    {
        return true;
    }
    trimmed.contains("server-fn:") && trimmed.ends_with(",null)")
}

/// Fetch ALL workspace IDs from opencode.ai/_server (insula fetch_workspace_id,
/// to the T, minus the take-first: the caller tries each workspace because the
/// Go plan may live on any of them). GET, then POST retry when no ids;
/// explicit-null means the account has no workspace (not a parse failure).
async fn fetch_workspace_ids(
    client: &reqwest::Client,
    cookie: &str,
) -> Result<Vec<String>, String> {
    let url = format!("{OPENCODE_BASE}/_server?id={OPENCODE_WORKSPACE_SERVER_ID}");
    tracing::debug!("opencode workspace: GET {url}");
    let resp = client
        .get(&url)
        .header("Cookie", cookie)
        .header("X-Server-Id", OPENCODE_WORKSPACE_SERVER_ID)
        .header("X-Server-Instance", server_instance_header())
        .header("User-Agent", OPENCODE_USER_AGENT)
        .header("Origin", OPENCODE_BASE)
        .header("Referer", OPENCODE_BASE)
        .header(
            "Accept",
            "text/javascript, application/json;q=0.9, */*;q=0.8",
        )
        .send()
        .await
        .map_err(|e| format!("opencode workspace request: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("opencode workspace HTTP {status}: {body}");
        return Err(format!("opencode workspace HTTP {status}"));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("opencode workspace body: {e}"))?;
    if looks_signed_out(&text) {
        return Err("opencode session expired".into());
    }
    if is_explicit_null(&text) {
        return Err("opencode: this account has no workspace".into());
    }
    let mut ids = parse_workspace_ids(&text);
    if ids.is_empty() {
        // POST retry (insula): the GET body sometimes carries no ids.
        tracing::debug!("opencode workspace: GET yielded no ids, retrying POST");
        let post_resp = client
            .post(format!("{OPENCODE_BASE}/_server"))
            .header("Cookie", cookie)
            .header("X-Server-Id", OPENCODE_WORKSPACE_SERVER_ID)
            .header("X-Server-Instance", server_instance_header())
            .header("User-Agent", OPENCODE_USER_AGENT)
            .header("Origin", OPENCODE_BASE)
            .header("Referer", OPENCODE_BASE)
            .header(
                "Accept",
                "text/javascript, application/json;q=0.9, */*;q=0.8",
            )
            .header("Content-Type", "application/json")
            .body("[]")
            .send()
            .await
            .map_err(|e| format!("opencode workspace POST request: {e}"))?;
        if !post_resp.status().is_success() {
            let status = post_resp.status();
            let body = post_resp.text().await.unwrap_or_default();
            tracing::warn!("opencode workspace POST HTTP {status}: {body}");
            return Err(format!("opencode workspace POST HTTP {status}"));
        }
        let fallback = post_resp
            .text()
            .await
            .map_err(|e| format!("opencode workspace POST body: {e}"))?;
        if looks_signed_out(&fallback) {
            return Err("opencode session expired".into());
        }
        if is_explicit_null(&fallback) {
            return Err("opencode: this account has no workspace".into());
        }
        ids = parse_workspace_ids(&fallback);
    }
    if ids.is_empty() {
        return Err("opencode: no workspace id found".into());
    }
    Ok(ids)
}

fn parse_workspace_ids(text: &str) -> Vec<String> {
    let mut ids = scan_wrk_ids(text);
    if ids.is_empty()
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
    {
        collect_workspace_ids(&v, &mut ids);
    }
    ids.sort();
    ids.dedup();
    ids
}

/// Byte-walk for `wrk_...` ids (insula scan_wrk_ids: byte-based because the
/// response can carry user-chosen UTF-8; the id itself is ASCII).
fn scan_wrk_ids(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = b"wrk_";
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if bytes[i..].starts_with(needle) {
            let start = i;
            i += needle.len();
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let id = String::from_utf8_lossy(&bytes[start..i]).into_owned();
            if id.len() > 4 && !out.contains(&id) {
                out.push(id);
            }
        } else {
            i += 1;
        }
    }
    out
}

fn collect_workspace_ids(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) if s.starts_with("wrk_") && s.len() > 4 => {
            if !out.contains(s) {
                out.push(s.clone());
            }
        }
        serde_json::Value::Array(a) => {
            for v in a {
                collect_workspace_ids(v, out);
            }
        }
        serde_json::Value::Object(m) => {
            for v in m.values() {
                collect_workspace_ids(v, out);
            }
        }
        _ => {}
    }
}

/// Fetch the /go HTML page and parse usage (insula pattern).
async fn fetch_go_page(
    client: &reqwest::Client,
    cookie: &str,
    workspace_id: &str,
) -> Result<String, String> {
    let url = format!("{}/workspace/{}/go", OPENCODE_BASE, workspace_id);
    tracing::debug!("opencode go: GET {url}");
    let resp = client
        .get(&url)
        .header("Cookie", cookie)
        .header("User-Agent", OPENCODE_USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .send()
        .await
        .map_err(|e| format!("opencode go request: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("opencode go HTTP {status}: {body}");
        return Err(format!("opencode go HTTP {status}"));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("opencode go body: {e}"))?;
    // Signed-out is session-level: fail fast. No-plan and parse-ability are
    // per-workspace: the caller iterates workspaces, so just return the page.
    if looks_signed_out(&text) {
        return Err("opencode session expired".into());
    }
    Ok(text)
}

async fn fetch_opencode_go(client: &reqwest::Client, cookie: &str) -> Result<UsageData, String> {
    let ids = fetch_workspace_ids(client, cookie).await?;
    tracing::debug!("opencode go: trying {} workspace(s)", ids.len());
    let mut no_plan_count = 0;
    let mut last_err = "opencodego: usage fields missing on /go page".to_string();
    for workspace_id in &ids {
        let text = match fetch_go_page(client, cookie, workspace_id).await {
            Ok(t) => t,
            Err(e) => {
                // Session-level: stop immediately, another workspace won't fix it.
                if e.contains("session expired") {
                    return Err(e);
                }
                last_err = e;
                continue;
            }
        };
        // Windows present = subscribed, regardless of stray null records
        // elsewhere on the page. No-plan is only reported when no workspace
        // yields windows AND every page carries the triple-null record.
        match parse_opencode_go(&text) {
            Ok(data) => {
                tracing::debug!(
                    "opencode go: workspace {workspace_id} yielded {} window(s)",
                    data.windows.len()
                );
                return Ok(data);
            }
            Err(e) => {
                last_err = e;
            }
        }
        if looks_unsubscribed(&text) {
            tracing::debug!(
                "opencode go: workspace {workspace_id} has no Go subscription record, trying next"
            );
            no_plan_count += 1;
            continue;
        }
        tracing::debug!(
            "opencode go: workspace {workspace_id}: no windows, no no-plan record (page_len={})",
            text.len()
        );
    }
    if !ids.is_empty() && no_plan_count == ids.len() {
        return Err("opencodego: workspace has no Go subscription".into());
    }
    Err(last_err)
}

// ── Background fetcher ─────────────────────────────────────────────────

/// Configuration for one upstream's billing fetch.
#[derive(Clone)]
pub struct FetcherConfig {
    pub kind: String,
    pub provider_name: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub cookie: Option<String>,
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
                let cookie = self.cookie.clone()?;
                Some(Box::new(OpencodeGoProvider { client, cookie }))
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
    cookie: String,
}

#[async_trait::async_trait]
impl UsageProvider for OpencodeGoProvider {
    fn name(&self) -> &str {
        "opencode-go"
    }
    async fn fetch(&self) -> Result<UsageData, String> {
        fetch_opencode_go(&self.client, &self.cookie).await
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

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- tracker tests --

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

    // -- opencode-go insula-parity tests --

    #[test]
    fn opencode_percent_without_reset_keeps_window() {
        // Insula: a window with percent but no reset is kept with
        // reset_secs=None, not dropped (this was the monthly-disappear bug).
        let json = r#"{"rollingUsage":{"usagePercent":50},"monthlyUsage":{"usagePercent":3}}"#;
        let usage = parse_opencode_go(json).unwrap();
        assert_eq!(usage.windows.len(), 2);
        let monthly = usage.windows.iter().find(|w| w.label == "30d").unwrap();
        assert_eq!(monthly.used_percent, Some(3.0));
        assert_eq!(monthly.reset_secs, None);
    }

    #[test]
    fn opencode_alt_reset_key_parses() {
        // Insula RESET_IN_KEYS: resetSeconds / resetsInSec variants must work.
        let json = r#"{"monthlyUsage":{"usagePercent":3,"resetSeconds":2592000}}"#;
        let usage = parse_opencode_go(json).unwrap();
        let monthly = usage.windows.iter().find(|w| w.label == "30d").unwrap();
        assert_eq!(monthly.reset_secs, Some(2592000));
    }

    #[test]
    fn opencode_absolute_reset_at_parses() {
        // Insula RESET_AT_KEYS: absolute resetAt timestamp → relative secs.
        let future = now_secs() + 3600;
        let json = format!(r#"{{"rollingUsage":{{"usagePercent":50,"resetAt":{future}}}}}"#);
        let usage = parse_opencode_go(&json).unwrap();
        let reset = usage.windows[0].reset_secs.unwrap();
        assert!((reset as i64 - 3600).abs() < 5);
    }

    #[test]
    fn opencode_alias_window_keys_parse() {
        // Insula dict aliases: rolling_usage / weeklyWindow / monthly_window.
        let json = r#"{"rolling_usage":{"usagePercent":5,"resetInSec":100},"weeklyWindow":{"usagePercent":2,"resetInSec":200},"monthly_window":{"usagePercent":3,"resetInSec":300}}"#;
        let usage = parse_opencode_go(json).unwrap();
        assert_eq!(usage.windows.len(), 3);
    }

    #[test]
    fn opencode_nested_data_wrapper_parses() {
        // Insula: nested data/result/usage wrappers are searched.
        let json = r#"{"data":{"rollingUsage":{"usagePercent":5,"resetInSec":100}}}"#;
        let usage = parse_opencode_go(json).unwrap();
        assert_eq!(usage.windows.len(), 1);
    }

    #[test]
    fn opencode_unsubscribed_record_detected() {
        // Insula live capture: triple-null record means no plan, not a failure.
        let text = concat!(
            r#"paymentMethodType:"card",balance:0,"#,
            "monthlyLimit:null,monthlyUsage:null,",
            "subscription:null,",
            "subscriptionID:null,subscriptionPlan:null,timeSubscriptionBooked:null"
        );
        assert!(looks_unsubscribed(text));
        // One populated field withholds the verdict.
        let live = text.replace("subscription:null", r#"subscription:"active""#);
        assert!(!looks_unsubscribed(&live));
    }

    #[test]
    fn opencode_actor_public_is_signed_out() {
        assert!(looks_signed_out(
            r#"{"error":"actor of type "public" not allowed"}"#
        ));
        assert!(!looks_signed_out(
            r#"rollingUsage: { usagePercent: 5, resetInSec: 18000 }"#
        ));
    }

    #[test]
    fn opencode_explicit_null_forms() {
        assert!(is_explicit_null("null"));
        assert!(is_explicit_null("NULL"));
        assert!(is_explicit_null(
            r#";0x41;((self.$R=self.$R||{})["server-fn:18cb"]=[],null)"#
        ));
        assert!(!is_explicit_null(
            r#"($R=>$R[0]={customerID:"cus_x",balance:0})(self.$R))"#
        ));
    }

    #[test]
    fn opencode_nonfinite_percent_drops_window() {
        // NaN percent must never become a served value.
        let json = r#"{"rollingUsage":{"usagePercent":50,"resetInSec":100},"weeklyUsage":{"usagePercent":"NaN","resetInSec":200}}"#;
        let usage = parse_opencode_go(json);
        // "NaN" string is not a JSON number: as_f64 fails → weekly dropped,
        // rolling kept.
        let usage = usage.unwrap();
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].label, "5h");
    }

    #[test]
    fn opencode_parse_workspace_ids_sorted_deduped() {
        let ids = parse_workspace_ids(r#"id:"wrk_b",id:"wrk_a",id:"wrk_b""#);
        assert_eq!(ids, vec!["wrk_a", "wrk_b"]);
    }

    #[test]
    fn opencode_null_record_before_real_block_is_skipped() {
        // A plan-less record (`monthlyUsage:null`) ahead of the real window
        // must not poison parsing: every occurrence is scanned.
        let text = "monthlyUsage:null,rollingUsage: { usagePercent: 5, resetInSec: 100 },monthlyUsage: { usagePercent: 3, resetInSec: 300 }";
        let usage = parse_opencode_go(text).unwrap();
        assert_eq!(usage.windows.len(), 2);
        let monthly = usage.windows.iter().find(|w| w.label == "30d").unwrap();
        assert_eq!(monthly.used_percent, Some(3.0));
        assert_eq!(monthly.reset_secs, Some(300));
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
