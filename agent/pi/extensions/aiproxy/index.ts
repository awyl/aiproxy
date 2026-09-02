/**
 * aiproxy provider extension for pi.
 *
 * Fronts an aiproxy gateway: one provider entry, models discovered from the
 * gateway at startup. Configuration lives in PI's own per-machine config
 * (`models.json`, user-level `~/.pi/agent/models.json` or project `.pi/`),
 * not in the proxy's yaml — the proxy and pi can run on different machines.
 *
 * Model metadata (contextWindow, maxTokens, reasoning, cost, etc.) is resolved
 * from pi's live model store (models-store.json) so context sizes match what pi
 * knows about each model rather than hardcoded defaults.
 */
import { readFileSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

interface ProxyModel {
  id: string;
  display_name?: string | null;
  surface?: string;
}

// ── config ──────────────────────────────────────────────────────────────────

function loadConfig(): { base: string; apiKey: string; token?: string } {
  const candidates = [
    join(homedir(), ".pi/agent/models.json"),
    join(process.cwd(), ".pi/models.json"),
  ];
  for (const path of candidates) {
    try {
      const raw = readFileSync(path, "utf8")
        .replace(/\/\*[\s\S]*?\*\//g, "")
        .replace(/(^|[^:])\/\/.*$/gm, "$1");
      const parsed = JSON.parse(raw) as {
        providers?: Record<string, { baseUrl?: string; apiKey?: string }>;
      };
      const e = parsed.providers?.["aiproxy"];
      if (e) {
        const base = e.baseUrl?.trim() || "http://127.0.0.1:8080/v1";
        const apiKey = e.apiKey?.trim() || "";
        const token = apiKey.startsWith("$")
          ? process.env[apiKey.slice(1)]?.trim()
          : apiKey || undefined;
        return { base, apiKey, token };
      }
    } catch { /* fall through */ }
  }
  return { base: "http://127.0.0.1:8080/v1", apiKey: "" };
}

// ── model catalog ───────────────────────────────────────────────────────────

/** Map proxy surface name → pi api id. */
const SURFACE_API: Record<string, string> = {
  messages: "anthropic-messages",
  responses: "openai-responses",
};

function loadCatalog(): Map<string, Record<string, unknown>> {
  const candidates = [
    join(homedir(), ".pi/agent/models-store.json"),
    join(process.cwd(), ".pi/models-store.json"),
  ];
  for (const p of candidates) {
    try {
      if (!existsSync(p)) continue;
      const raw = JSON.parse(readFileSync(p, "utf8")) as Record<
        string,
        { models?: Array<Record<string, unknown>> }
      >;
      const map = new Map<string, Record<string, unknown>>();
      for (const [provider, data] of Object.entries(raw)) {
        for (const m of data.models ?? []) {
          map.set(`${provider}/${m.id}`, m);
        }
      }
      return map;
    } catch { /* try next */ }
  }
  return new Map();
}

/**
 * Strip subscription suffix from provider ID for catalog lookup.
 * "opencode-go=alice" → "opencode-go", "opencode-go" → "opencode-go"
 */
function stripSubSuffix(id: string): string {
  const eq = id.indexOf('=');
  return eq >= 0 ? id.slice(0, eq) : id;
}

/** Build a model entry from catalog metadata. */
function fromCatalog(
  id: string,
  meta: Record<string, unknown>,
  fallbackSurface?: string,
) {
  return {
    id,
    name: (meta.name as string) ?? id,
    api: (meta.api as string) ?? SURFACE_API[fallbackSurface ?? ""] ?? "openai-completions",
    reasoning: (meta.reasoning as boolean) ?? false,
    thinkingLevelMap: (meta.thinkingLevelMap as Record<string, string | null>) ?? undefined,
    input: (meta.input as ("text" | "image")[]) ?? ["text"],
    cost: (meta.cost as { input: number; output: number; cacheRead: number; cacheWrite: number }) ?? { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    contextWindow: (meta.contextWindow as number) ?? 128_000,
    maxTokens: (meta.maxTokens as number) ?? 16_384,
    headers: (meta.headers as Record<string, string>) ?? undefined,
    compat: meta.compat as any,
  };
}

// ── extension entry ─────────────────────────────────────────────────────────

export default async function (pi: ExtensionAPI) {
  const { base, apiKey, token } = loadConfig();

  // Expose token as env so MCP and other tools can use it without repeating
  if (token) {
    process.env.AIPROXY_TOKEN = token;
  }

  // Discover models from the proxy
  let models: ProxyModel[] = [];
  try {
    const res = await fetch(`${base}/models`, {
      headers: token ? { Authorization: `Bearer ${token}` } : undefined,
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    models = ((await res.json()) as { data: ProxyModel[] }).data ?? [];
  } catch (err) {
    console.warn(
      `[aiproxy] could not fetch ${base}/models: ${err instanceof Error ? err.message : String(err)}\n` +
      "           no models registered — check models.json and that the gateway is running.",
    );
  }

  const catalog = loadCatalog();
  if (catalog.size > 0) {
    console.log(`[aiproxy] loaded ${catalog.size} model(s) from pi model store`);
  }

  pi.registerProvider("aiproxy", {
    name: "aiproxy",
    baseUrl: base,
    apiKey,
    authHeader: true,
    api: "openai-completions",
    models: models.map((m) => {
      // Strip subscription suffix (e.g. "opencode-go=alice" → "opencode-go")
      // then look up in catalog by provider/modelId
      const provider = stripSubSuffix(m.id.split('/')[0] ?? '');
      const modelId = m.id.split('/').slice(1).join('/');
      const catalogKey = `${provider}/${modelId}`;
      const meta = catalog.get(catalogKey);
      return meta ? fromCatalog(m.id, meta, m.surface) : {
        id: m.id,
        name: m.display_name ?? m.id,
        api: SURFACE_API[m.surface ?? ""] ?? "openai-completions",
        reasoning: false,
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 128_000,
        maxTokens: 16_384,
      };
    }),
  });

  if (models.length > 0) {
    console.log(`[aiproxy] registered ${models.length} model(s) from ${base}`);
  }
}
