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

function loadConfig(): {
  base: string;
  apiKey: string;
  token?: string;
  mcpServers?: string | string[];
} {
  const candidates = [
    join(homedir(), ".pi/agent/aiproxy.json"),
    join(process.cwd(), ".pi/aiproxy.json"),
  ];
  for (const path of candidates) {
    try {
      if (!existsSync(path)) continue;
      const raw = readFileSync(path, "utf8")
        .replace(/\/\*[\s\S]*?\*\//g, "")
        .replace(/(^|[^:])\/\/.*$/gm, "$1");
      const parsed = JSON.parse(raw) as {
        baseUrl?: string;
        apiKey?: string;
        mcpServers?: string | string[];
      };
      const base = parsed.baseUrl?.trim() || "http://127.0.0.1:8080/v1";
      const apiKey = parsed.apiKey?.trim() || "";
      const token = apiKey.startsWith("$")
        ? process.env[apiKey.slice(1)]?.trim()
        : apiKey || undefined;
      return { base, apiKey, token, mcpServers: parsed.mcpServers };
    } catch { /* fall through */ }
  }
  return { base: "http://127.0.0.1:8080/v1", apiKey: "" };
}

export default async function (pi: ExtensionAPI) {
  const cfg = loadConfig();
  await registerProxyProvider(pi, cfg);
  await registerMcpTools(pi, cfg);
}
