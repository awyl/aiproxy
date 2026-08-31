/**
 * aiproxy provider extension for pi.
 *
 * Fronts an aiproxy gateway: one provider entry, models discovered from the
 * gateway at startup. Configuration lives in PI's own per-machine config
 * (`models.json`, user-level `~/.pi/agent/models.json` or project `.pi/`),
 * not in the proxy's yaml — the proxy and pi can run on different machines.
 *
 *   {
 *     "providers": {
 *       "aiproxy": {
 *         "baseUrl": "http://127.0.0.1:8080/v1",   // remote proxy: use its host
 *         "apiKey": "$AIPROXY_TOKEN"               // $ENV interpolation, literal, or omit
 *       }
 *     }
 *   }
 *
 * `aiproxy.yaml` is purely the PROXY SERVER's config — the extension never
 * reads it. Defaults (no models.json entry): http://127.0.0.1:8080/v1, no key.
 *
 * The proxy serves three wire surfaces on one host; per-model `api`:
 *   surface "chat"      -> openai-completions
 *   surface "messages"  -> anthropic-messages
 *   surface "responses" -> openai-responses
 */

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type Surface = "chat" | "messages" | "responses" | "unknown";
type Thinking = "off" | "low" | "medium" | "high" | "max";

interface ProxyModel {
  id: string;
  display_name?: string | null;
  surface?: Surface;
}

interface ModelsResponse {
  data: ProxyModel[];
}

/**
 * Read the aiproxy provider entry from PI's models.json (user-level
 * `~/.pi/agent/models.json`). Same file pi composes over registered providers;
 * we read it directly so discovery + registration agree with pi's auth.
 * Returns the base URL and the apiKey config value (may use $ENV syntax — pi
 * interpolates at request time; we pass it through for registration).
 */
function loadConfig(): {
  base: string;
  apiKey: string;
  token?: string;
  thinking: Thinking;
} {
  const candidates = [
    join(homedir(), ".pi/agent/models.json"),
    join(process.cwd(), ".pi/models.json"),
  ];
  let entry: { baseUrl?: string; apiKey?: string } | undefined;
  for (const path of candidates) {
    try {
      const raw = readFileSync(path, "utf8")
        .replace(/\/\*[\s\S]*?\*\//g, "") // strip block comments
        .replace(/(^|[^:])\/\/.*$/gm, "$1"); // strip line comments (keep URLs)
      const parsed = JSON.parse(raw) as { providers?: Record<string, { baseUrl?: string; apiKey?: string }> };
      entry = parsed.providers?.["aiproxy"];
      if (entry) break;
    } catch {
      // missing or unparseable — fall through to defaults
    }
  }

  const baseUrl = entry?.baseUrl?.trim() || "http://127.0.0.1:8080/v1";
  const apiKey = entry?.apiKey?.trim() || "";
  const token = apiKey.startsWith("$")
    ? process.env[apiKey.slice(1)]?.trim()
    : apiKey || undefined; // token for the discovery fetch

  // default thinking level (per-model override via models.json stays possible)
  return { base: baseUrl, apiKey, token, thinking: "high" };
}

/** Map proxy surface to pi Api id; unknown surfaces default to chat. */
function apiForSurface(surface: Surface | undefined): "openai-completions" | "anthropic-messages" | "openai-responses" {
  if (surface === "messages") return "anthropic-messages";
  if (surface === "responses") return "openai-responses";
  return "openai-completions";
}

/**
 * One default thinking level for every model: the chosen level maps to itself
 * (pi level name == provider effort value), everything else hidden. "off"
 * registers non-reasoning models.
 */
function thinkingFor(level: Thinking): { reasoning: boolean; thinkingLevelMap?: Record<string, string | null> } {
  if (level === "off") return { reasoning: false };
  const thinkingLevelMap: Record<string, string | null> = { minimal: null, low: null, medium: null, high: null, xhigh: null, max: null };
  thinkingLevelMap[level] = level;
  return { reasoning: true, thinkingLevelMap };
}

export default async function (pi: ExtensionAPI) {
  const { base, apiKey, token, thinking } = loadConfig();

  let models: ProxyModel[] = [];
  try {
    const res = await fetch(`${base}/models`, {
      headers: token ? { Authorization: `Bearer ${token}` } : undefined,
    });
    if (!res.ok) throw new Error(`GET ${base}/models -> HTTP ${res.status}`);
    models = ((await res.json()) as ModelsResponse).data ?? [];
  } catch (error) {
    console.warn(
      `[aiproxy] could not fetch ${base}/models: ${error instanceof Error ? error.message : String(error)}\n` +
        "           no aiproxy models registered — check models.json providers.aiproxy and that the gateway is running.",
    );
  }

  pi.registerProvider("aiproxy", {
    name: "aiproxy",
    baseUrl: base,
    apiKey,
    authHeader: true,
    api: "openai-completions", // provider default; per-model api below wins
    models: models.map((m) => ({
      id: m.id,
      name: m.display_name ?? m.id,
      api: apiForSurface(m.surface),
      ...thinkingFor(thinking),
      input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      contextWindow: 128000,
      maxTokens: 16384,
    })),
  });

  if (models.length > 0) {
    console.log(`[aiproxy] registered ${models.length} model(s) from ${base} (keystone: ${models[0].id})`);
  }
}