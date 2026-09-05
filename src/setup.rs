use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing;

use crate::api::AppState;

/// Resolve the base cookie directory. Defaults to /runtime (sibling of /models in Docker).
/// Override with AIPROXY_RUNTIME_DIR env var.
pub fn cookie_dir(_config_path: &Path) -> PathBuf {
    std::env::var("AIPROXY_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/runtime"))
}

/// Resolve cookie path for a specific provider.
pub fn cookie_path(config_path: &Path, provider_name: &str) -> PathBuf {
    cookie_dir(config_path).join(format!("opencode-cookie_{provider_name}"))
}

/// List all stored cookie files and return (provider_name, path) pairs.
pub fn list_cookies(config_path: &Path) -> Vec<(String, PathBuf)> {
    let dir = cookie_dir(config_path);
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(provider) = name.strip_prefix("opencode-cookie_")
                && !provider.is_empty()
            {
                result.push((provider.to_string(), entry.path()));
            }
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

/// Read the stored cookie (if any).
pub fn read_cookie(path: &Path) -> Option<String> {
    let data = std::fs::read_to_string(path).ok()?;
    let trimmed = data.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Write the cookie to disk with restricted permissions.
pub fn write_cookie(path: &Path, cookie: &str) -> Result<(), String> {
    let trimmed = cookie.trim();
    if trimmed.is_empty() {
        return Err("cookie must not be empty".into());
    }
    // Validate basic cookie format: should contain =
    if !trimmed.contains('=') {
        return Err("cookie must be in name=value format (e.g. auth=xxx)".into());
    }
    std::fs::write(path, trimmed).map_err(|e| format!("write failed: {e}"))?;
    // Restrict permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!("opencode cookie stored");
    Ok(())
}

// ── Setup page handler ─────────────────────────────────────────────────

pub async fn setup_page() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>aiproxy — Setup</title>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
         max-width: 640px; margin: 2rem auto; padding: 0 1rem; color: #1a1a1a; }
  h1 { font-size: 1.5rem; margin-bottom: 0.5rem; }
  p.sub { color: #666; margin-bottom: 1.5rem; font-size: 0.9rem; }
  label { display: block; font-weight: 600; margin-bottom: 0.3rem; }
  select, textarea { width: 100%; font-size: 0.85rem; padding: 0.5rem;
             border: 1px solid #ccc; border-radius: 4px; }
  textarea { height: 80px; font-family: monospace; resize: vertical; }
  .hint { font-size: 0.8rem; color: #888; margin: 0.3rem 0 1rem; }
  button { background: #2563eb; color: #fff; border: none; padding: 0.6rem 1.2rem;
           border-radius: 4px; cursor: pointer; font-size: 0.9rem; }
  button:hover { background: #1d4ed8; }
  .msg { margin-top: 1rem; padding: 0.7rem; border-radius: 4px; font-size: 0.85rem; }
  .ok { background: #dcfce7; color: #166534; }
  .err { background: #fef2f2; color: #991b1b; }
  hr { margin: 2rem 0; border: none; border-top: 1px solid #eee; }
  h2 { font-size: 1.1rem; margin-bottom: 0.5rem; }
  .status { font-size: 0.85rem; color: #666; }
  .status .row { padding: 0.3rem 0; }
</style>
</head>
<body>
<h1>aiproxy Setup</h1>
<p class="sub">Configure browser cookies for opencode-go usage tracking.</p>

<h2>OpenCode Cookie</h2>
<p class="hint">
  Open <a href="https://opencode.ai" target="blank">opencode.ai</a> in Chrome, open DevTools (F12),
  go to Application → Cookies, and copy the value of the <code>auth</code> cookie.
  Then paste <code>auth=&lt;value&gt;</code> below.
</p>
<label for="provider">Upstream name</label>
<select id="provider"><option value="opencode-go">opencode-go</option></select>
<div class="hint">Select the upstream to configure. For <code>opencode-go=alice</code>, select <code>opencode-go=alice</code>.</div>
<label for="cookie">Cookie header</label>
<textarea id="cookie" placeholder="auth=eyJhbGci..."></textarea>
<div class="hint">Format: <code>name=value</code> (e.g. <code>auth=xxx</code>)</div>
<button onclick="submit()">Save</button>
<div id="msg"></div>

<hr>

<h2>Status</h2>
<div id="status">Loading...</div>

<script>
async function loadUpstreams() {
  try {
    const [upRes, statusRes] = await Promise.all([
      fetch('/api/upstreams'),
      fetch('/api/cookie/status')
    ]);
    const upData = await upRes.json();
    const statusData = await statusRes.json();
    const stored = new Set((statusData.cookies || []).filter(c => c.stored).map(c => c.provider));
    const sel = document.getElementById('provider');
    sel.innerHTML = '';
    for (const u of upData.upstreams || []) {
      const opt = document.createElement('option');
      opt.value = u.name;
      opt.textContent = stored.has(u.name) ? `✅ ${u.name}` : u.name;
      sel.appendChild(opt);
    }
  } catch(e) {}
}
async function loadStatus() {
  try {
    const r = await fetch('/api/cookie/status');
    const d = await r.json();
    const el = document.getElementById('status');
    if (!d.cookies || d.cookies.length === 0) {
      el.innerHTML = '❌ No cookies stored';
      return;
    }
    el.innerHTML = d.cookies.map(c => {
      if (!c.stored) return '<div class="row">❌ <b>' + c.provider + '</b>: not set</div>';
      const t = new Date(c.saved_at * 1000).toLocaleString();
      return '<div class="row">✅ <b>' + c.provider + '</b>: stored (' + t + ')</div>';
    }).join('');
  } catch(e) {
    document.getElementById('status').textContent = 'Error checking status';
  }
}
async function submit() {
  const cookie = document.getElementById('cookie').value.trim();
  const provider = document.getElementById('provider').value;
  const msg = document.getElementById('msg');
  if (!cookie) { msg.className='msg err'; msg.textContent='Enter a cookie value'; return; }
  if (!provider) { msg.className='msg err'; msg.textContent='Select an upstream'; return; }
  try {
    const r = await fetch('/api/cookie', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({cookie, provider})
    });
    const d = await r.json();
    if (r.ok) {
      msg.className='msg ok'; msg.textContent=d.message;
      document.getElementById('cookie').value='';
      loadStatus();
    } else {
      msg.className='msg err'; msg.textContent=d.error || 'Failed';
    }
  } catch(e) {
    msg.className='msg err'; msg.textContent='Network error';
  }
}
loadUpstreams();
loadStatus();
</script>
</body>
</html>"#,
    )
}

// ── Usage page handler ──────────────────────────────────────────────

pub async fn usage_page() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>aiproxy — Usage</title>
<meta http-equiv="refresh" content="60">
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
         max-width: 800px; margin: 2rem auto; padding: 0 1rem; color: #1a1a1a; }
  h1 { font-size: 1.5rem; margin-bottom: 0.5rem; }
  p.sub { color: #666; margin-bottom: 1.5rem; font-size: 0.9rem; }
  .provider { border: 1px solid #e5e7eb; border-radius: 8px; padding: 1rem; margin-bottom: 1rem; }
  .provider h2 { font-size: 1.1rem; margin-bottom: 0.5rem; }
  .window { display: flex; justify-content: space-between; align-items: center;
            padding: 0.5rem 0; border-bottom: 1px solid #f3f4f6; }
  .window:last-child { border-bottom: none; }
  .window-label { font-weight: 600; }
  .window-pct { font-size: 1.2rem; font-weight: 700; }
  .window-reset { color: #666; font-size: 0.85rem; }
  .pct-high { color: #dc2626; }
  .pct-med { color: #d97706; }
  .pct-low { color: #16a34a; }
  .empty { color: #999; font-style: italic; padding: 2rem; text-align: center; }
  .updated { color: #999; font-size: 0.8rem; margin-top: 1rem; }
</style>
</head>
<body>
<h1>aiproxy Usage</h1>
<p class="sub">Per-provider billing window usage. Auto-refreshes every 60s.</p>
<div id="data">Loading...</div>
<div class="updated" id="updated"></div>
<script>
async function load() {
  try {
    const r = await fetch('/v1/usage');
    if (!r.ok) throw new Error('HTTP ' + r.status);
    const data = await r.json();
    const el = document.getElementById('data');
    if (!data.length) { el.innerHTML = '<div class="empty">No usage data yet. Make a request first.</div>'; return; }
    el.innerHTML = data.map(p => {
      const wins = (p.windows || []).map(w => {
        const pct = w.used_percent != null ? w.used_percent.toFixed(1) : '?';
        const cls = pct > 80 ? 'pct-high' : pct > 50 ? 'pct-med' : 'pct-low';
        const reset = w.reset_secs ? formatDur(w.reset_secs) : '';
        return `<div class="window"><span class="window-label">${w.label || 'unknown'}</span><span class="window-pct ${cls}">${pct}%</span><span class="window-reset">${reset ? 'resets in ' + reset : ''}</span></div>`;
      }).join('');
      return `<div class="provider"><h2>${p.provider}</h2>${wins || '<div class="empty">No windows</div>'}</div>`;
    }).join('');
    document.getElementById('updated').textContent = 'Updated: ' + new Date().toLocaleTimeString();
  } catch(e) {
    document.getElementById('data').innerHTML = '<div class="empty">Error: ' + e.message + '</div>';
  }
}
function formatDur(s) {
  const d = Math.floor(s/86400), h = Math.floor((s%86400)/3600), m = Math.floor((s%3600)/60);
  if (d>0) return h>0 ? d+'d '+h+'h' : d+'d';
  if (h>0) return m>0 ? h+'h '+m+'m' : h+'h';
  return m+'m';
}
load();
</script>
</body>
</html>"#,
    )
}

// ── Cookie API handlers ────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CookieInput {
    pub cookie: String,
    pub provider: String,
}

#[derive(Serialize)]
pub struct CookieStatus {
    pub stored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<u64>,
}

pub async fn upstreams_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let upstreams: Vec<serde_json::Value> = state
        .upstream_names
        .iter()
        .filter(|name| name.starts_with("opencode-go"))
        .map(|name| serde_json::json!({ "name": name }))
        .collect();
    Json(serde_json::json!({ "upstreams": upstreams }))
}

pub async fn set_cookie(
    State(state): State<AppState>,
    Json(input): Json<CookieInput>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let path = cookie_path(&state.cookie_path, &input.provider);
    match write_cookie(&path, &input.cookie) {
        Ok(()) => Ok(Json(serde_json::json!({
            "message": "Cookie saved successfully"
        }))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )),
    }
}

pub async fn cookie_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cookies = list_cookies(&state.cookie_path);
    let entries: Vec<serde_json::Value> = cookies
        .iter()
        .map(|(name, path)| {
            let stored = read_cookie(path).is_some();
            let saved_at = if stored {
                std::fs::metadata(path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
            } else {
                None
            };
            serde_json::json!({
                "provider": name,
                "stored": stored,
                "saved_at": saved_at,
            })
        })
        .collect();
    Json(serde_json::json!({ "cookies": entries }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn cookie_path_is_in_runtime_dir() {
        let config = Path::new("/etc/aiproxy/aiproxy.yaml");
        assert_eq!(
            cookie_path(config, "opencode-go"),
            PathBuf::from("/runtime/opencode-cookie_opencode-go")
        );
    }

    #[test]
    fn cookie_path_env_override() {
        // Can't safely test env var override in parallel tests;
        // just verify the default behavior works.
        let config = Path::new("aiproxy.yaml");
        assert!(cookie_path(config, "opencode-go").ends_with("opencode-cookie_opencode-go"));
    }

    #[test]
    fn cookie_path_multi_sub() {
        let config = Path::new("/etc/aiproxy/aiproxy.yaml");
        assert_eq!(
            cookie_path(config, "opencode-go=alice"),
            PathBuf::from("/runtime/opencode-cookie_opencode-go=alice")
        );
    }

    #[test]
    fn write_and_read_cookie() {
        let dir = std::env::temp_dir().join(format!("aiproxy-cookie-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode-cookie");

        write_cookie(&path, "auth=abc123").unwrap();
        assert_eq!(read_cookie(&path).as_deref(), Some("auth=abc123"));

        // Overwrite
        write_cookie(&path, "auth=newvalue").unwrap();
        assert_eq!(read_cookie(&path).as_deref(), Some("auth=newvalue"));

        fs::remove_file(&path).unwrap();
        fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn read_missing_cookie_returns_none() {
        let path = PathBuf::from("/nonexistent/opencode-cookie");
        assert!(read_cookie(&path).is_none());
    }

    #[test]
    fn empty_cookie_is_rejected() {
        let dir = std::env::temp_dir().join(format!("aiproxy-cookie-empty-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode-cookie");

        assert!(write_cookie(&path, "").is_err());
        assert!(write_cookie(&path, "  ").is_err());

        fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn cookie_without_equals_is_rejected() {
        let dir = std::env::temp_dir().join(format!("aiproxy-cookie-noeq-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode-cookie");

        assert!(write_cookie(&path, "justtext").is_err());

        fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn cookie_is_trimmed() {
        let dir = std::env::temp_dir().join(format!("aiproxy-cookie-trim-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode-cookie");

        write_cookie(&path, "  auth=abc  \n").unwrap();
        assert_eq!(read_cookie(&path).as_deref(), Some("auth=abc"));

        fs::remove_file(&path).unwrap();
        fs::remove_dir(&dir).unwrap();
    }
}
