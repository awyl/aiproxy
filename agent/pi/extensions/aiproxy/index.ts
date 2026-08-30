/**
 * aiproxy provider extension for pi.
 *
 * Fronts an aiproxy gateway: one provider entry, models discovered from the
 * gateway at startup. All configuration comes from the gateway's own yaml
 * (default `./aiproxy.yaml`), so there are NO extension env vars — env vars
 * are reserved for secrets (upstream keys, tokens) referenced by name in the
 * yaml, e.g. `token_env: AIPROXY_TOKEN`.
 *
 *   bind: 127.0.0.1:8080   (extension derives http://127.0.0.1:8080/v1)
 *   token_env: AIPROXY_TOKEN   (or token: <literal>; secret stays in env)
 *   thinking: high          (default thinking level: off|low|medium|high|max)
 *
 * The proxy serves three wire surfaces on one host; per-model `api`:
 *   surface "chat"      -> openai-completions
 *   surface "messages"  -> anthropic-messages
 *   surface "responses" -> openai-responses
 */

import { readFileSync } from "node:fs";
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

/** Minimal yaml scrape for the handful of keys the extension needs. */
function readYaml(path: string): Record<string, string> {
  const text = readFileSync(path, "utf8");
  const out: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const m = /^\s*([A-Za-z_][\w-]*)\s*:\s*(.*?)\s*$/.exec(line);
    if (m && !m[2].startsWith("#")) out[m[1]] = m[2];
  }
  return out;
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

function loadConfig(): { base: string; token?: string; apiKey: string; thinking: Thinking } {
  const yaml = readYaml("aiproxy.yaml");

  // bind -> base URL (http://host:port/v1, empty host = localhost)
  const [host, port] = (yaml.bind ?? "127.0.0.1:8080").split(":");
  const base = `http://${host || "localhost"}:${(port ?? "8080").trim() || "8080"}/v1`;

  // token: literal wins over token_env (env var NAME holding the secret).
  // apiKey must end up identical at request time: reference the same env var
  // (pi interpolates `$NAME` from the environment) or pass the literal.
  const tokenEnv = yaml.token_env?.trim();
  const token = yaml.token?.trim() || (tokenEnv ? process.env[tokenEnv]?.trim() : undefined);
  const apiKey = tokenEnv ? `$${tokenEnv}` : token ?? "";

  const thinking = (yaml.thinking ?? "high").trim().toLowerCase() as Thinking;
  return { base, token, apiKey, thinking: ["off", "low", "medium", "high", "max"].includes(thinking) ? thinking : "high" };
}

export default async function (pi: ExtensionAPI) {
  const { base, token, apiKey, thinking } = loadConfig();

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
        "           no aiproxy models registered — check aiproxy.yaml bind/token and that the gateway is running.",
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