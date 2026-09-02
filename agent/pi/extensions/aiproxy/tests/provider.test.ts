import { describe, it, expect, vi } from "vitest";
import { registerProxyProvider } from "../provider.ts";

function stubPi() {
  return { registerTool: vi.fn(), registerProvider: vi.fn() };
}

const modelsResponse = {
  ok: true,
  json: async () => ({
    data: [
      { id: "minimax/MiniMax-M3", surface: "chat" },
      { id: "opencode-go/minimax-m3", surface: "messages" },
      { id: "opencode-go/mimo-v2.5", surface: "chat", display_name: "MiMo" },
    ],
  }),
};

async function registerWith(modelsResponseBody: unknown, catalog = new Map()) {
  const pi = stubPi();
  const fetchImpl = vi.fn().mockResolvedValue(modelsResponseBody);
  await registerProxyProvider(
    pi as never,
    { base: "http://p:9999/v1", apiKey: "k", token: "k" },
    { catalog, fetchImpl: fetchImpl as unknown as typeof fetch },
  );
  return { pi, fetchImpl };
}

describe("registerProxyProvider", () => {
  it("registers provider with configured baseUrl and models from the gateway", async () => {
    const { pi, fetchImpl } = await registerWith(modelsResponse);
    expect(fetchImpl).toHaveBeenCalledWith("http://p:9999/v1/models", expect.objectContaining({ headers: { Authorization: "Bearer k" } }));
    expect(pi.registerProvider).toHaveBeenCalledTimes(1);
    const cfg = pi.registerProvider.mock.calls[0][1];
    expect(cfg.baseUrl).toBe("http://p:9999/v1");
    expect(cfg.models).toHaveLength(3);
  });

  it("proxy surface chat wins over catalog api and needs no baseUrl override", async () => {
    const catalog = new Map([["minimax/MiniMax-M3", { api: "anthropic-messages", contextWindow: 1_048_576 }]]);
    const { pi } = await registerWith(modelsResponse, catalog);
    const m = pi.registerProvider.mock.calls[0][1].models.find((x: { id: string }) => x.id === "minimax/MiniMax-M3");
    expect(m.api).toBe("openai-completions");
    expect(m.baseUrl).toBeUndefined();
    expect(m.contextWindow).toBe(1_048_576);
  });

  it("messages-surface models get anthropic api with /v1 stripped from baseUrl", async () => {
    const { pi } = await registerWith(modelsResponse);
    const cfg = pi.registerProvider.mock.calls[0][1];
    const m = cfg.models.find((x: { id: string }) => x.id === "opencode-go/minimax-m3");
    expect(m.api).toBe("anthropic-messages");
    expect(m.baseUrl).toBe("http://p:9999");
  });

  it("falls back to display_name and openai-completions for unknown models without catalog", async () => {
    const { pi } = await registerWith(modelsResponse);
    const m = pi.registerProvider.mock.calls[0][1].models.find((x: { id: string }) => x.id === "opencode-go/mimo-v2.5");
    expect(m.api).toBe("openai-completions");
    expect(m.name).toBe("MiMo");
    expect(m.baseUrl).toBeUndefined();
  });

  it("registers zero models and does not throw when the gateway is unreachable", async () => {
    const pi = stubPi();
    const fetchImpl = vi.fn().mockRejectedValue(new Error("ECONNREFUSED"));
    await registerProxyProvider(
      pi as never,
      { base: "http://p:9999/v1", apiKey: "", token: undefined },
      { catalog: new Map(), fetchImpl: fetchImpl as unknown as typeof fetch },
    );
    const cfg = pi.registerProvider.mock.calls[0][1];
    expect(cfg.models).toHaveLength(0);
  });

  it("exposes the token as AIPROXY_TOKEN env for MCP and tools", async () => {
    const before = process.env.AIPROXY_TOKEN;
    try {
      await registerWith(modelsResponse);
      expect(process.env.AIPROXY_TOKEN).toBe("k");
    } finally {
      if (before === undefined) delete process.env.AIPROXY_TOKEN;
      else process.env.AIPROXY_TOKEN = before;
    }
  });
});
