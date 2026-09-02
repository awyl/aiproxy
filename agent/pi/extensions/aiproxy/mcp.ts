/**
 * aiproxy MCP registration: connects to the gateway's MCP multiplexer
 * (streamable-HTTP) and exposes its tools as native pi tools — no mcp.json.
 */
export interface McpToolDef {
  name: string;
  description?: string;
  inputSchema?: Record<string, unknown>;
}

interface McpToolResult {
  content?: Array<{ type: string; text?: string }>;
  isError?: boolean;
}

export interface McpClientLike {
  listTools(): Promise<{ tools: McpToolDef[] }>;
  callTool(params: { name: string; arguments?: Record<string, unknown> }, discriminator?: undefined, options?: { signal?: AbortSignal }): Promise<McpToolResult>;
  close(): Promise<void>;
}

export type McpClientFactory = (url: string, headers: Record<string, string>) => Promise<McpClientLike>;

/** X-MCP-Servers header value: names (and optional per-server tokens) passthrough. */
export function mcpHeader(mcpServers?: string | string[]): string | undefined {
  if (!mcpServers) return undefined;
  const v = Array.isArray(mcpServers) ? mcpServers.join(",") : mcpServers.trim();
  return v.length > 0 ? v : undefined;
}

/** Default factory: MCP SDK streamable-HTTP client. */
export const defaultClientFactory: McpClientFactory = async (url, headers) => {
  const { Client } = await import("@modelcontextprotocol/sdk/client/index.js");
  const { StreamableHTTPClientTransport } = await import("@modelcontextprotocol/sdk/client/streamableHttp.js");
  const transport = new StreamableHTTPClientTransport(new URL(url), { requestInit: { headers } });
  const client = new Client({ name: "pi-aiproxy", version: "0.2.6" });
  await client.connect(transport);
  return client as unknown as McpClientLike;
};

export interface McpDeps {
  base: string;
  apiKey: string;
  token?: string;
  mcpServers?: string | string[];
}

function textOf(result: McpToolResult): string {
  const parts = (result.content ?? [])
    .filter((c) => c.type === "text" && typeof c.text === "string")
    .map((c) => c.text as string);
  return parts.join("\n");
}

export async function registerMcpTools(
  pi: import("@earendil-works/pi-coding-agent").ExtensionAPI,
  deps: McpDeps,
  factory: McpClientFactory = defaultClientFactory,
): Promise<void> {
  const header = mcpHeader(deps.mcpServers);
  if (!header) return;

  const headers: Record<string, string> = {
    "X-MCP-Servers": header,
    ...(deps.token ? { Authorization: `Bearer ${deps.token}` } : {}),
  };
  const url = `${deps.base.replace(/\/v1\/?$/, "")}/mcp`;

  let client: McpClientLike;
  try {
    client = await factory(url, headers);
    const { tools } = await client.listTools();
    for (const t of tools) {
      const def = {
        name: t.name,
        label: t.name,
        description: t.description ?? t.name,
        ...(t.inputSchema ? { parameters: t.inputSchema } : {}),
        async execute(
          _toolCallId: string,
          args: Record<string, unknown>,
          signal?: AbortSignal,
        ) {
          const result = await client.callTool({ name: t.name, arguments: args }, undefined, { signal });
          const text = textOf(result);
          if (result.isError) throw new Error(text || `MCP tool ${t.name} failed`);
          return {
            content: [{ type: "text", text: text || "(no output)" }],
            details: {},
          };
        },
      } as Parameters<typeof pi.registerTool>[0];
      pi.registerTool(def);
    }
    console.log(`[aiproxy] registered ${tools.length} MCP tool(s) via ${header}`);
  } catch (err) {
    console.warn(
      `[aiproxy] MCP unavailable at ${url} (${header}): ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}
