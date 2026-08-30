/**
 * aiproxy provider extension for pi.
 *
 * Fronts an aiproxy gateway (https://github.com/.../aiproxy): one provider
 * entry, models discovered from GET /v1/models at startup. The proxy serves
 * three wire surfaces on one host, so per-model `api` is derived from the
 * proxy's per-model `surface` field:
 *
 *   surface "chat"      -> openai-completions
 *   surface "messages"  -> anthropic-messages
 *   surface "responses" -> openai-responses
 *
 * Setup:
 *   AIPROXY_URL=http://host:8080/v1   (default http://localhost:8080/v1)
 *   AIPROXY_TOKEN=<shared token>       (default: nothing -> proxy runs auth-disabled)
 *   AIPROXY_THINKING=high              (default "high"; off|low|medium|high|max)
 *     default thinking level for every model. "off" registers models as
 *     non-reasoning; otherwise all models get a thinkingLevelMap where the
 *     chosen level is enabled and the rest are hidden.
 *
 * Enable via `pi -e ./extensions/aiproxy` or add the path to settings.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type Surface = "chat" | "messages" | "responses" | "unknown";

interface ProxyModel {
  id: string;
  display_name?: string | null;
  surface?: Surface;
  created?: string | null;
}

interface ModelsResponse {
  data: ProxyModel[];
}

const DEFAULT_URL = "http://localhost:8080/v1";

async function fetchModels(base: string, token?: string): Promise<ProxyModel[]> {
  const url = `${base}/models`;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 10_000);
  try {
    const res = await fetch(url, {
      signal: controller.signal,
      headers: token ? { Authorization: `Bearer ${token}` } : undefined,
    });
    if (!res.ok) {
      throw new Error(`GET ${url} -> HTTP ${res.status}`);
    }
    const payload = (await res.json()) as ModelsResponse;
    return Array.isArray(payload.data) ? payload.data : [];
  } finally {
    clearTimeout(timer);
  }
}

/** Map proxy surface to pi Api id; unknown surfaces default to chat. */
function apiForSurface(surface: Surface | undefined): "openai-completions" | "anthropic-messages" | "openai-responses" {
  switch (surface) {
    case "messages":
      return "anthropic-messages";
    case "responses":
      return "openai-responses";
    case "chat":
    case "unknown":
    default:
      return "openai-completions";
  }
}

type ThinkingLevel = "off" | "low" | "medium" | "high" | "max";

/**
 * Build a thinkingLevelMap for all models from one default level.
 * The chosen level maps to itself (pi level name == provider effort value for
 * openai-completions); everything else is hidden (null). "off" disables
 * thinking entirely (reasoning: false, no map).
 */
function thinkingFor(level: ThinkingLevel): {
  reasoning: boolean;
  thinkingLevelMap?: Record<string, string | null>;
} {
  if (level === "off") {
    return { reasoning: false };
  }
  const map: Record<string, string | null> = {
    minimal: null,
    low: null,
    medium: null,
    high: null,
    xhigh: null,
    max: null,
  };
  map[level] = level;
  return { reasoning: true, thinkingLevelMap: map };
}

export default async function (pi: ExtensionAPI) {
  const base = (process.env.AIPROXY_URL ?? DEFAULT_URL).replace(/\/+$/, "");
  const tokenEnv = process.env.AIPROXY_TOKEN_ENV ?? "AIPROXY_TOKEN";
  const token = process.env[tokenEnv]?.trim();
  const apiKey = token ? `$${tokenEnv}` : "$AIPROXY_TOKEN";

  const requested = (process.env.AIPROXY_THINKING ?? "high").trim().toLowerCase();
  const thinkingLevel: ThinkingLevel =
    requested === "off" || requested === "low" || requested === "medium" || requested === "high" || requested === "max"
      ? requested
      : "high";
  if (requested !== thinkingLevel) {
    console.warn(
      `[aiproxy] AIPROXY_THINKING='${requested}' invalid (off|low|medium|high|max); using 'high'`,
    );
  }

  let models: ProxyModel[] = [];
  try {
    models = await fetchModels(base, token);
  } catch (error) {
    console.warn(
      `[aiproxy] could not fetch ${base}/models: ${error instanceof Error ? error.message : String(error)}\n` +
        "           no aiproxy models registered — check AIPROXY_URL and that the gateway is running.",
    );
  }

  pi.registerProvider("aiproxy", {
    name: "aiproxy",
    baseUrl: base,
    apiKey,
    authHeader: true,
    api: "openai-completions", // provider default; per-model api below wins
    models: models.map((m) => {
      const { reasoning, thinkingLevelMap } = thinkingFor(thinkingLevel);
      return {
        id: m.id,
        name: m.display_name ?? m.id,
        api: apiForSurface(m.surface),
        reasoning,
        thinkingLevelMap,
        input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 128000,
        maxTokens: 16384,
      };
    }),
  });

  if (models.length > 0) {
    console.log(
      `[aiproxy] registered ${models.length} model(s) from ${base} (keystone: ${models[0].id})`,
    );
  }
}