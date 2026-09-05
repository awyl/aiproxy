/**
 * Usage widget: fetches per-provider billing windows from aiproxy
 * and displays them in a TUI widget below the editor.
 *
 * Format: 🟢 5h 1% (4h 30m) │ 7d 74% (6d 12h)
 */
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

interface UsageWindow {
  label?: string;
  used_percent?: number;
  reset_secs?: number;
}

interface ProviderUsage {
  provider: string;
  windows: UsageWindow[];
}

interface UsageConfig {
  base: string;
  token?: string;
}

function formatDuration(secs: number): string {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return h > 0 ? `${d}d ${h}h` : `${d}d`;
  if (h > 0) return m > 0 ? `${h}h ${m}m` : `${h}h`;
  return `${m}m`;
}

function formatUsage(usage: ProviderUsage[]): string {
  if (usage.length === 0) return "";
  const seen = new Set<string>();
  const parts: string[] = [];
  for (const u of usage) {
    for (const w of u.windows) {
      if (w.used_percent == null) continue;
      const label = w.label || "";
      if (seen.has(label)) continue;
      seen.add(label);
      const pct = w.used_percent.toFixed(0);
      const reset = w.reset_secs ? ` (${formatDuration(w.reset_secs)})` : "";
      parts.push(`${label} ${pct}%${reset}`);
    }
  }
  if (parts.length === 0) return "";
  return parts.join(" │ ");
}

/**
 * Register usage tracking with pi. Handles:
 * - Tracking current provider from model id
 * - Fetching usage on first agent turn
 * - Refreshing every 60s
 * - Displaying in a widget below the editor
 */
export function registerUsage(
  pi: ExtensionAPI,
  cfg: UsageConfig,
): void {
  let timer: ReturnType<typeof setInterval> | null = null;
  let lastSummary = "";
  let currentProvider = "";

  // Track current provider from model id (e.g. "opencode-go/mimo-v2.5" → "opencode-go")
  pi.on("before_provider_headers", (_event, ctx) => {
    const model = ctx.model;
    if (!model || model.provider !== "aiproxy") return;
    const id = model.id || "";
    const slash = id.indexOf("/");
    if (slash > 0) {
      const prefix = id.substring(0, slash);
      // Strip =suffix for multi-subscription (opencode-go=alice → opencode-go)
      currentProvider = prefix.includes("=") ? prefix.split("=")[0] : prefix;
    }
  });

  async function refreshWidget(ctx: ExtensionContext): Promise<void> {
    try {
      const res = await fetch(`${cfg.base}/usage`, {
        headers: cfg.token ? { Authorization: `Bearer ${cfg.token}` } : undefined,
      });
      if (!res.ok) return;
      const allUsage: ProviderUsage[] = await res.json();
      // Filter to current provider once known; show all until first request
      const usage = currentProvider
        ? allUsage.filter((u) => u.provider === currentProvider)
        : allUsage;
      const summary = formatUsage(usage);
      if (summary !== lastSummary) {
        lastSummary = summary;
        ctx.ui.setWidget(
          "aiproxy-usage",
          summary ? [`[aiproxy] ${summary}`] : undefined,
          { placement: "belowEditor" },
        );
      }
    } catch { }
  }

  // Set up refresh interval on session start
  pi.on("session_start", async (_event, ctx) => {
    if (timer) clearInterval(timer);
    timer = setInterval(() => refreshWidget(ctx), 60_000);
  });

  // Fetch usage on first agent turn (ensures TUI is fully initialized)
  let initialFetchDone = false;
  pi.on("agent_start", async (_event, ctx) => {
    if (!initialFetchDone) {
      initialFetchDone = true;
      await refreshWidget(ctx);
    }
  });

  // Cleanup on shutdown
  pi.on("session_shutdown", () => {
    if (timer) {
      clearInterval(timer);
      timer = null;
    }
  });
}
