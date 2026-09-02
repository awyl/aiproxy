/**
 * aiproxy provider registration: one provider entry fronting the gateway,
 * models discovered from the proxy at startup, metadata resolved from pi's
 * live model store (models-store.json).
 */
import { readFileSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { Model, Context, SimpleStreamOptions } from "@earendil-works/pi-ai";
import { getApiProvider } from "@earendil-works/pi-ai/compat";
import { cleanStream } from "./thinking.ts";

export interface ProxyModel {
  id: string;
  display_name?: string | null;
  surface?: string;
}

/** Models that need thinking-stream cleanup (M3 on openai-completions). */
const THINK_CLEAN_MODELS = new Set(["minimax/MiniMax-M3"]);

/** Map proxy surface name → pi api id. The proxy's surface is authoritative. */
export const SURFACE_API: Record<string, string> = {
  chat: "openai-completions",
  messages: "anthropic-messages",
  responses: "openai-responses",
};

/**
 * pi-ai's anthropic client appends `/v1/messages` to the base URL (the Anthropic
 * SDK convention), while aiproxy mounts the messages route at `/v1/messages`.
 * The provider baseUrl ends in `/v1`, so messages-wire models need the root.
 */
export function wireBaseUrl(api: string, base: string): string | undefined {
  return api === "anthropic-messages" ? base.replace(/\/v1\/?$/, "") : undefined;
}

export function loadCatalog(): Map<string, Record<string, unknown>> {
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
export function stripSubSuffix(id: string): string {
  const eq = id.indexOf('=');
  return eq >= 0 ? id.slice(0, eq) : id;
}

/** Build a model entry from catalog metadata. */
export function fromCatalog(
  id: string,
  meta: Record<string, unknown>,
  surface?: string,
  base = "http://127.0.0.1:8080/v1",
) {
  const api = SURFACE_API[surface ?? ""] ?? (meta.api as string) ?? "openai-completions";
  const msgBase = wireBaseUrl(api, base);
  return {
    id,
    name: (meta.name as string) ?? id,
    api,
    ...(msgBase ? { baseUrl: msgBase } : {}),
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

export interface ProxyProviderConfig {
  base: string;
  apiKey: string;
  token?: string;
}

export interface ProxyProviderOptions {
  catalog?: Map<string, Record<string, unknown>>;
  fetchImpl?: typeof fetch;
}

export async function registerProxyProvider(
  pi: import("@earendil-works/pi-coding-agent").ExtensionAPI,
  cfg: ProxyProviderConfig,
  opts: ProxyProviderOptions = {},
): Promise<void> {
  const { base, apiKey, token } = cfg;

  const fetchImpl = opts.fetchImpl ?? fetch;
  const catalog = opts.catalog ?? loadCatalog();
  if (catalog.size > 0) {
    console.log(`[aiproxy] loaded ${catalog.size} model(s) from pi model store`);
  }

  // Discover models from the proxy
  let models: ProxyModel[] = [];
  try {
    const res = await fetchImpl(`${base}/models`, {
      headers: token ? { Authorization: `Bearer ${token}` } : undefined,
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    models = ((await res.json()) as { data: ProxyModel[] }).data ?? [];
  } catch (err) {
    console.warn(
      `[aiproxy] could not fetch ${base}/models: ${err instanceof Error ? err.message : String(err)}\n` +
      "           no models registered — check aiproxy.json and that the gateway is running.",
    );
  }

  pi.registerProvider("aiproxy", {
    name: "aiproxy",
    baseUrl: base,
    apiKey,
    authHeader: true,
    api: "openai-completions",
    streamSimple: (model: Model<any>, context: Context, options?: SimpleStreamOptions) => {
      const driver = getApiProvider("openai-completions");
      if (!driver) throw new Error("openai-completions api provider not registered");
      const baseStream = driver.streamSimple({ ...model, api: "openai-completions" }, context, options);
      // MiniMax M3 emits thinking twice (reasoning fields + inline <think>); clean it
      if (THINK_CLEAN_MODELS.has(model.id)) return cleanStream(baseStream);
      return baseStream;
    },
    models: models.map((m) => {
      // Strip subscription suffix (e.g. "opencode-go=alice" → "opencode-go")
      // then look up in catalog by provider/modelId
      const provider = stripSubSuffix(m.id.split('/')[0] ?? '');
      const modelId = m.id.split('/').slice(1).join('/');
      const meta = catalog.get(`${provider}/${modelId}`);
      if (meta) return fromCatalog(m.id, meta, m.surface, base);
      const api = SURFACE_API[m.surface ?? ""] ?? "openai-completions";
      const msgBase = wireBaseUrl(api, base);
      return {
        id: m.id,
        name: m.display_name ?? m.id,
        api,
        ...(msgBase ? { baseUrl: msgBase } : {}),
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
