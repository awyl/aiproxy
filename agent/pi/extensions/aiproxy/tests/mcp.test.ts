import { describe, it, expect, vi } from "vitest";
import { registerMcpTools, mcpHeader } from "../mcp.ts";
import type { McpClientLike } from "../mcp.ts";

function stubPi() {
  return { registerTool: vi.fn(), registerProvider: vi.fn() };
}

function stubClient(tools: Array<Record<string, unknown>>, callResult?: Record<string, unknown>): McpClientLike {
  return {
    listTools: async () => ({ tools }) as never,
    callTool: async () => callResult ?? { content: [{ type: "text", text: "ok" }] },
    close: async () => {},
  };
}

const baseDeps = { base: "http://p:9999/v1", apiKey: "k", token: "k", mcpServers: "searxng,ctx7" };

describe("mcpHeader", () => {
  it("returns undefined for missing config", () => {
    expect(mcpHeader(undefined)).toBeUndefined();
    expect(mcpHeader("")).toBeUndefined();
  });

  it("passes a string through verbatim (token syntax stays usable)", () => {
    expect(mcpHeader("searxng:tok,ctx7")).toBe("searxng:tok,ctx7");
  });

  it("joins an array of names", () => {
    expect(mcpHeader(["searxng", "ctx7", "grep"])).toBe("searxng,ctx7,grep");
  });
});

describe("registerMcpTools", () => {
  it("registers one pi tool per MCP tool with name and description", async () => {
    const pi = stubPi();
    const client = stubClient([
      { name: "searxng__search", description: "Web search", inputSchema: { type: "object", properties: { q: { type: "string" } } } },
      { name: "ctx7__docs", description: "Doc lookup" },
    ]);
    await registerMcpTools(pi as never, baseDeps, async () => client);

    expect(pi.registerTool).toHaveBeenCalledTimes(2);
    const t1 = pi.registerTool.mock.calls[0][0];
    expect(t1.name).toBe("searxng__search");
    expect(t1.description).toBe("Web search");
    expect(t1.parameters).toEqual({ type: "object", properties: { q: { type: "string" } } });
    const t2 = pi.registerTool.mock.calls[1][0];
    expect(t2.name).toBe("ctx7__docs");
    expect(t2.description).toBe("Doc lookup");
  });

  it("execute forwards arguments to callTool and maps text content", async () => {
    const pi = stubPi();
    const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: "result text" }] });
    const client: McpClientLike = {
      listTools: async () => ({ tools: [{ name: "t__x", description: "d", inputSchema: { type: "object" } }] }) as never,
      callTool: callTool,
      close: async () => {},
    };
    await registerMcpTools(pi as never, baseDeps, async () => client);

    const tool = pi.registerTool.mock.calls[0][0];
    const out = await tool.execute("id1", { q: "hello" }, undefined as never, undefined as never, {} as never);
    expect(callTool).toHaveBeenCalledWith({ name: "t__x", arguments: { q: "hello" } }, undefined, { signal: undefined });
    expect(out).toEqual({ content: [{ type: "text", text: "result text" }], details: {} });
  });

  it("execute throws when the MCP call reports isError", async () => {
    const pi = stubPi();
    const client = stubClient(
      [{ name: "t__x", description: "d" }],
      { content: [{ type: "text", text: "boom" }], isError: true },
    );
    await registerMcpTools(pi as never, baseDeps, async () => client);

    const tool = pi.registerTool.mock.calls[0][0];
    await expect(tool.execute("id", {}, undefined as never, undefined as never, {} as never)).rejects.toThrow("boom");
  });

  it("registers nothing and does not throw when the gateway is unreachable", async () => {
    const pi = stubPi();
    await registerMcpTools(pi as never, baseDeps, async () => {
      throw new Error("ECONNREFUSED");
    });
    expect(pi.registerTool).not.toHaveBeenCalled();
  });

  it("skips entirely when mcpServers is not configured", async () => {
    const pi = stubPi();
    const factory = vi.fn();
    await registerMcpTools(pi as never, { ...baseDeps, mcpServers: undefined }, factory);
    expect(factory).not.toHaveBeenCalled();
    expect(pi.registerTool).not.toHaveBeenCalled();
  });
});
