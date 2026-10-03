//! Human-facing HTML pages served by the proxy.

use axum::response::Html;

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
