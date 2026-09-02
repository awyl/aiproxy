/**
 * aiproxy extension for pi.
 *
 * Glues the two halves of the gateway integration together:
 *  - provider.ts — model provider (one entry, models discovered from the proxy)
 *  - mcp.ts      — native pi tools backed by the proxy's MCP multiplexer
 *
 * Configuration lives in pi's own per-machine config file
 * (`~/.pi/agent/aiproxy.json` or project `.pi/aiproxy.json`), not in the
 * proxy's yaml — the proxy and pi can run on different machines.
 */
import { readFileSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { registerProxyProvider } from "./provider.ts";
import { registerMcpTools } from "./mcp.ts";

type RawConfig = {
  baseUrl?: string;
  apiKey?: string;
  mcpServers?: string | string[];
};

function readConfigFile(path: string): RawConfig | undefined {
  try {
    if (!existsSync(path)) return undefined;
    const raw = readFileSync(path, "utf8")
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .replace(/(^|[^:])\/\/.*$/gm, "$1");
    return JSON.parse(raw) as RawConfig;
  } catch {
    return undefined;
  }
}

/**
 * Global ~/.pi/agent/aiproxy.json is the default; project .pi/aiproxy.json
 * overrides individual fields (per-field merge, like pi's mcp.json).
 */
export function loadConfig(cwd = process.cwd()): {
  base: string;
  apiKey: string;
  token?: string;
  mcpServers?: string | string[];
} {
  const global = readConfigFile(join(homedir(), ".pi/agent/aiproxy.json")) ?? {};
  const project = readConfigFile(join(cwd, ".pi/aiproxy.json")) ?? {};
  const merged = { ...global, ...project };

  const base = merged.baseUrl?.trim() || "http://127.0.0.1:8080/v1";
  const apiKey = merged.apiKey?.trim() || "";
  const token = apiKey.startsWith("$")
    ? process.env[apiKey.slice(1)]?.trim()
    : apiKey || undefined;
  return { base, apiKey, token, mcpServers: merged.mcpServers };
}

export default async function (pi: ExtensionAPI) {
  const cfg = loadConfig();

  // Expose token as env so MCP and other tools can use it without repeating
  if (cfg.token) {
    process.env.AIPROXY_TOKEN = cfg.token;
  }

  await registerProxyProvider(pi, cfg);
  await registerMcpTools(pi, cfg);
}
