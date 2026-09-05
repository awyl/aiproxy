# Usage Tracking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Per-provider usage tracking from upstream rate-limit headers, exposed via `GET /v1/usage`, displayed in the pi extension status bar.

**Architecture:** In-memory `UsageTracker` shared across all providers. Each provider extracts rate-limit response headers after every upstream call and updates the tracker. A new axum endpoint exposes the data. Extension fetches periodically and shows a status line.

**Tech Stack:** Rust (axum, reqwest, tokio, serde), TypeScript (extension)

**Spec:** User requirement: per-provider usage from the get-go, status bar shows current provider with usage %, reset time, weekly reset. Follows insula's per-account, per-resource-window model.

## Global Constraints

- aiproxy is stateless — usage data lives in memory only, lost on restart
- No new dependencies beyond what's already in Cargo.toml
- TDD: failing test first, green, committed per task
- Follow existing code patterns (Provider trait, axum handlers, extension structure)
- Rate limits are per-account (credential), not per-model — track per upstream kind
- Multiple windows per provider (5h + weekly, requests + tokens) — capture all upstream exposes
- `primary` slot is shortest window, NOT most constrained — API returns all windows, consumer takes max

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/usage.rs` | Create | UsageTracker, RateLimitSnapshot, per-provider rate windows |
| `src/api/mod.rs` | Modify | Mount `GET /v1/usage` route |
| `src/server.rs` | Modify | Create UsageTracker, pass to AppState |
| `src/provider.rs` | Modify | Add `usage_tracker: Option<UsageTracker>` to RequestContext |
| `src/providers/openai.rs` | Modify | Extract OpenAI rate-limit headers |
| `src/providers/anthropic.rs` | Modify | Extract Anthropic rate-limit headers |
| `src/providers/go.rs` | Modify | Extract go upstream rate-limit headers |
| `src/lib.rs` | Modify | Register `pub mod usage` |
| `tests/usage_e2e.rs` | Create | End-to-end usage endpoint test |
| `agent/pi/extensions/aiproxy/provider.ts` | Modify | Fetch `/v1/usage`, expose for status bar |
| `CHANGELOG.md` | Modify | Document new feature |

---

## Task 1: UsageTracker core + tests

**Files:**
- Create: `src/usage.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `UsageTracker` (Arc-wrapped, cloneable), `RateWindow` (per resource), `ProviderUsage` (JSON shape), `UsageSnapshot`

- [ ] **Step 1: Write the failing test**

```rust
// src/usage.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_is_empty() {
        let t = UsageTracker::new();
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn update_and_snapshot_one_window() {
        let t = UsageTracker::new();
        t.update("openai", RateLimitSnapshot {
            windows: vec![
                RateWindow {
                    resource: "requests".into(),
                    limit: Some(100),
                    remaining: Some(65),
                    reset_secs: Some(1800),
                    used_count: None,
                    total_count: None,
                },
            ],
        });
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1);
        let u = &snap[0];
        assert_eq!(u.provider, "openai");
        assert_eq!(u.windows.len(), 1);
        assert_eq!(u.windows[0].resource, "requests");
        assert_eq!(u.windows[0].limit, Some(100));
        assert_eq!(u.windows[0].remaining, Some(65));
        // used_percent computed: (100-65)/100 * 100 = 35%
        assert!((u.windows[0].used_percent - 35.0).abs() < 0.1);
    }

    #[test]
    fn multiple_windows_per_provider() {
        let t = UsageTracker::new();
        t.update("openai", RateLimitSnapshot {
            windows: vec![
                RateWindow { resource: "requests".into(), limit: Some(100), remaining: Some(50), reset_secs: Some(1800), used_count: None, total_count: None },
                RateWindow { resource: "tokens".into(), limit: Some(1_000_000), remaining: Some(400_000), reset_secs: Some(3600), used_count: None, total_count: None },
            ],
        });
        let snap = t.snapshot();
        let u = &snap[0];
        assert_eq!(u.windows.len(), 2);
        // tokens: (1_000_000 - 400_000) / 1_000_000 * 100 = 60%
        let tokens = u.windows.iter().find(|w| w.resource == "tokens").unwrap();
        assert!((tokens.used_percent - 60.0).abs() < 0.1);
    }

    #[test]
    fn per_provider_updates_dont_clobber() {
        let t = UsageTracker::new();
        t.update("openai", RateLimitSnapshot { windows: vec![RateWindow { resource: "requests".into(), limit: Some(100), remaining: Some(50), reset_secs: None, used_count: None, total_count: None }] });
        t.update("anthropic", RateLimitSnapshot { windows: vec![RateWindow { resource: "requests".into(), limit: Some(200), remaining: Some(180), reset_secs: None, used_count: None, total_count: None }] });
        let snap = t.snapshot();
        assert_eq!(snap.len(), 2);
        let o = snap.iter().find(|u| u.provider == "openai").unwrap();
        assert_eq!(o.windows[0].remaining, Some(50));
        let a = snap.iter().find(|u| u.provider == "anthropic").unwrap();
        assert_eq!(a.windows[0].remaining, Some(180));
    }

    #[test]
    fn partial_limit_still_records() {
        let t = UsageTracker::new();
        t.update("openai", RateLimitSnapshot {
            windows: vec![RateWindow { resource: "requests".into(), limit: Some(100), remaining: None, reset_secs: None, used_count: None, total_count: None }],
        });
        let snap = t.snapshot();
        let w = &snap[0].windows[0];
        assert_eq!(w.limit, Some(100));
        assert_eq!(w.remaining, None);
        // no crash, used_percent handles None remaining
    }

    #[test]
    fn no_limit_is_none_used_percent() {
        let t = UsageTracker::new();
        t.update("openai", RateLimitSnapshot {
            windows: vec![RateWindow { resource: "requests".into(), limit: None, remaining: None, reset_secs: None, used_count: None, total_count: None }],
        });
        let snap = t.snapshot();
        assert!(snap[0].windows[0].used_percent.is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test usage -- --nocapture`
Expected: FAIL — module `usage` not found

- [ ] **Step 3: Implement UsageTracker**

```rust
// src/usage.rs
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use serde::Serialize;

/// A single rate-limit window (requests, tokens, or any other resource).
/// Mirrors insula's RateWindow concept: one resource, one window, one reset.
#[derive(Debug, Clone, Default)]
pub struct RateWindowInput {
    pub resource: String,       // "requests", "tokens", etc.
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub reset_secs: Option<u64>,
    pub used_count: Option<u64>,
    pub total_count: Option<u64>,
}

/// Snapshot of all rate-limit windows for one upstream kind.
#[derive(Debug, Clone, Default)]
pub struct RateLimitSnapshot {
    pub windows: Vec<RateWindowInput>,
}

/// Serialized rate window in the API response.
#[derive(Debug, Clone, Serialize)]
pub struct RateWindow {
    pub resource: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<u64>,
}

/// One provider's usage snapshot in the API response.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderUsage {
    pub provider: String,
    pub windows: Vec<RateWindow>,
    pub updated_at: u64, // epoch millis
}

/// Thread-safe in-memory usage tracker.
#[derive(Clone)]
pub struct UsageTracker {
    inner: Arc<RwLock<HashMap<String, (RateLimitSnapshot, u64)>>>,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(HashMap::new())) }
    }

    pub async fn update(&self, provider: &str, snapshot: RateLimitSnapshot) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.inner.write().await.insert(provider.to_string(), (snapshot, now));
    }

    pub async fn snapshot(&self) -> Vec<ProviderUsage> {
        self.inner.read().await.iter().map(|(provider, (snap, ts))| {
            ProviderUsage {
                provider: provider.clone(),
                windows: snap.windows.iter().map(|w| {
                    let used_percent = match (w.limit, w.remaining) {
                        (Some(limit), Some(remaining)) if limit > 0 => {
                            Some(((limit - remaining) as f64 / limit as f64) * 100.0)
                        }
                        _ => None,
                    };
                    RateWindow {
                        resource: w.resource.clone(),
                        limit: w.limit,
                        remaining: w.remaining,
                        used_percent,
                        reset_secs: w.reset_secs,
                        used_count: w.used_count,
                        total_count: w.total_count,
                    }
                }).collect(),
                updated_at: *ts,
            }
        }).collect()
    }
}
```

- [ ] **Step 4: Register module in lib.rs**

Add `pub mod usage;` to `src/lib.rs`.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test usage`
Expected: PASS (all 6 tests)

- [ ] **Step 6: Commit**

---

## Task 2: Add UsageTracker to RequestContext + AppState

**Files:**
- Modify: `src/provider.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/server.rs`

**Interfaces:**
- Consumes: `UsageTracker` from server startup
- Produces: tracker available in all handler contexts

- [ ] **Step 1: Add field to RequestContext**

```rust
// src/provider.rs — add to RequestContext struct
pub struct RequestContext {
    pub model: String,
    pub client_headers: axum::http::HeaderMap,
    pub usage_tracker: Option<crate::usage::UsageTracker>,
}
```

Update all construction sites to pass `None` initially (will be `Some(tracker)` after Task 3).

- [ ] **Step 2: Add to AppState**

```rust
// src/api/mod.rs — add to AppState
pub struct AppState {
    pub registry: Arc<ModelRegistry>,
    pub embeddings: Arc<EmbeddingManager>,
    pub token: Option<String>,
    pub subscriptions: std::collections::HashMap<String, Option<String>>,
    pub usage: crate::usage::UsageTracker,
}
```

- [ ] **Step 3: Create tracker in server startup**

```rust
// src/server.rs — in build_with_port
let usage = crate::usage::UsageTracker::new();
// Pass to AppState and handlers
```

- [ ] **Step 4: Pass tracker through handler → provider**

In each handler (chat_completions, messages, responses, embeddings), construct `RequestContext` with `usage_tracker: Some(state.usage.clone())`.

- [ ] **Step 5: Run tests, commit**

---

## Task 3: Header extraction per provider

**Files:**
- Modify: `src/providers/openai.rs`
- Modify: `src/providers/anthropic.rs`
- Modify: `src/providers/go.rs`

**Interfaces:**
- Consumes: `UsageTracker` from `RequestContext`
- Produces: Rate-limit headers extracted after each upstream response

- [ ] **Step 1: Write failing test for OpenAI extraction**

```rust
#[tokio::test]
async fn openai_extracts_rate_limit_headers() {
    // Modify mock to return x-ratelimit-limit-requests: 100,
    // x-ratelimit-remaining-requests: 65, x-ratelimit-reset-requests: 1800
    // Call chat_completions, verify tracker receives them
}
```

- [ ] **Step 2: Implement extraction helper**

```rust
// In each provider, after resp.status() check, before creating byte stream:

fn extract_openai_limits(resp: &reqwest::Response) -> RateLimitSnapshot {
    let get = |name: &str| -> Option<u64> {
        resp.headers().get(name)?.to_str().ok()?.parse().ok()
    };
    let mut windows = Vec::new();
    if let (Some(limit), Some(remaining)) = (get("x-ratelimit-limit-requests"), get("x-ratelimit-remaining-requests")) {
        windows.push(RateWindowInput {
            resource: "requests".into(),
            limit: Some(limit),
            remaining: Some(remaining),
            reset_secs: get("x-ratelimit-reset-requests"),
            ..Default::default()
        });
    }
    if let (Some(limit), Some(remaining)) = (get("x-ratelimit-limit-tokens"), get("x-ratelimit-remaining-tokens")) {
        windows.push(RateWindowInput {
            resource: "tokens".into(),
            limit: Some(limit),
            remaining: Some(remaining),
            reset_secs: get("x-ratelimit-reset-tokens"),
            ..Default::default()
        });
    }
    RateLimitSnapshot { windows }
}
```

- [ ] **Step 3: Wire into OpenAI provider**

After `resp.status()` success, before `resp.bytes_stream()`:
```rust
if let Some(tracker) = &ctx.usage_tracker {
    let snapshot = extract_openai_limits(&resp);
    if !snapshot.windows.is_empty() {
        tracker.update("openai", snapshot).await;
    }
}
```

- [ ] **Step 4: Implement Anthropic extraction**

Anthropic headers: `anthropic-ratelimit-requests-limit`, `anthropic-ratelimit-requests-remaining`, `anthropic-ratelimit-requests-reset` (reset is ISO timestamp — convert to secs).

- [ ] **Step 5: Implement Go extraction**

Go provider handles multiple upstream kinds. Extract headers generically, key by the upstream kind (from model prefix).

- [ ] **Step 6: Run all tests, commit**

---

## Task 4: `GET /v1/usage` endpoint

**Files:**
- Modify: `src/api/mod.rs`
- Create: `tests/usage_e2e.rs`

**Interfaces:**
- Consumes: `UsageTracker` from `AppState`
- Produces: `GET /v1/usage` → `Json<Vec<ProviderUsage>>`

- [ ] **Step 1: Write failing e2e test**

```rust
#[tokio::test]
async fn usage_endpoint_returns_empty_when_no_traffic() {
    // Start daemon, GET /v1/authed/usage (with bearer), expect []
}
```

- [ ] **Step 2: Mount route**

```rust
// In openai_router (where /v1/models lives):
.route("/usage", get(usage_handler))
```

- [ ] **Step 3: Implement handler**

```rust
async fn usage_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.usage.snapshot().await)
}
```

- [ ] **Step 4: Require auth for /v1/usage**

Usage data is sensitive — gate behind the same auth as other endpoints.

- [ ] **Step 5: Run tests, commit**

---

## Task 5: Extension status bar display

**Files:**
- Modify: `agent/pi/extensions/aiproxy/provider.ts`

**Interfaces:**
- Consumes: `GET {baseUrl}/usage` from aiproxy
- Produces: Status bar line on startup + periodic refresh

- [ ] **Step 1: Add usage fetch function**

```typescript
interface RateWindow {
  resource: string;
  limit?: number;
  remaining?: number;
  used_percent?: number;
  reset_secs?: number;
}

interface ProviderUsage {
  provider: string;
  windows: RateWindow[];
  updated_at: number;
}

async function fetchUsage(base: string, token?: string): Promise<ProviderUsage[]> {
  try {
    const res = await fetch(`${base}/usage`, {
      headers: token ? { Authorization: `Bearer ${token}` } : undefined,
    });
    if (!res.ok) return [];
    return await res.json();
  } catch { return []; }
}
```

- [ ] **Step 2: Show active provider usage**

Show only the provider currently being used, determined by model prefix (`opencode-go/mimo-v2.5` → `opencode-go`).

```typescript
// Store latest usage snapshot
let lastUsage: Map<string, ProviderUsage> = new Map();

// After fetching usage, build lookup
for (const u of usage) lastUsage.set(u.provider, u);

// On startup, show first available provider
const first = usage[0];
if (first) {
  const maxWindow = first.windows.reduce((max, w) =>
    (w.used_percent ?? 0) > (max.used_percent ?? 0) ? w : max,
  first.windows[0]);
  if (maxWindow) {
    const pct = maxWindow.used_percent?.toFixed(0) ?? "?";
    const reset = maxWindow.reset_secs ? ` reset ${formatDuration(maxWindow.reset_secs)}` : "";
    console.log(`[aiproxy] ${first.provider} ${pct}%${reset}`);
  }
}
```

When a request comes in for a model like `opencode-go/mimo-v2.5`, the active provider is `opencode-go`. The proxy updates the tracker for that provider after the response. The extension reads and shows only that provider's usage.

- [ ] **Step 3: Periodic refresh (1 min)**

```typescript
setInterval(async () => {
  const usage = await fetchUsage(base, token);
  // Store in module-level var for status bar access
}, 60_000);
```

- [ ] **Step 4: Typecheck, tests, commit**

---

## Task 6: Documentation + version bump

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `README.md`
- Modify: `Cargo.toml`
- Modify: `agent/pi/extensions/aiproxy/package.json`

- [ ] **Step 1: Update CHANGELOG.md**

```
### Added
- **Usage tracking** — `GET /v1/usage` returns per-provider rate-limit data
  captured from upstream response headers (requests + tokens windows).
  In-memory only (lost on restart). Extension shows usage summary on startup.
```

- [ ] **Step 2: Bump version to 0.3.0**

- [ ] **Step 3: Update README.md**

- [ ] **Step 4: Run full test suite, clippy, fmt**

- [ ] **Step 5: Commit**
