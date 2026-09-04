import { describe, it, expect, vi, beforeEach } from "vitest";
import { registerProxyProvider, attributionHeaders, loadCatalog } from "../provider.ts";

function stubPi() {
  return { registerTool: vi.fn(), registerProvider: vi.fn(), on: vi.fn() };
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
});

describe("attributionHeaders — mirrors pi provider-attribution.js", () => {
  it("opencode-go: session + client headers from sessionId", () => {
    expect(attributionHeaders("opencode-go", "sess-1")).toEqual({
      "x-opencode-session": "sess-1",
      "x-opencode-client": "pi",
    });
  });

  it("opencode-go: no sessionId → no headers (mirrors getSessionHeaders gate)", () => {
    expect(attributionHeaders("opencode-go", undefined)).toBeUndefined();
  });

  it("openrouter/nvidia: telemetry-gated headers not sent (mirrors pi gate)", () => {
    expect(attributionHeaders("openrouter", undefined)).toBeUndefined();
    expect(attributionHeaders("nvidia", undefined)).toBeUndefined();
  });

  it("kinds without attribution → undefined", () => {
    expect(attributionHeaders("minimax", "sess-1")).toBeUndefined();
    expect(attributionHeaders("zai", "sess-1")).toBeUndefined();
    expect(attributionHeaders("openai", "sess-1")).toBeUndefined();
    expect(attributionHeaders("anthropic", "sess-1")).toBeUndefined();
  });
});

describe("before_provider_headers wiring", () => {
  it("registers the hook and injects opencode session headers for aiproxy models", async () => {
    const { pi } = await registerWith(modelsResponse);
    expect(pi.on).toHaveBeenCalledWith("before_provider_headers", expect.any(Function));
    const hook = pi.on.mock.calls.find((c) => c[0] === "before_provider_headers")![1];
    const headers: Record<string, string | null> = {};
    hook(
      { type: "before_provider_headers", headers },
      {
        model: { provider: "aiproxy", id: "opencode-go/mimo-v2.5" },
        sessionManager: { getSessionId: () => "sess-42" },
      },
    );
    expect(headers["x-opencode-session"]).toBe("sess-42");
    expect(headers["x-opencode-client"]).toBe("pi");
  });

  it("ignores non-aiproxy models and never overwrites existing headers", async () => {
    const { pi } = await registerWith(modelsResponse);
    const hook = pi.on.mock.calls.find((c) => c[0] === "before_provider_headers")![1];
    // builtin provider: untouched
    const h1: Record<string, string | null> = {};
    hook(
      { type: "before_provider_headers", headers: h1 },
      {
        model: { provider: "opencode-go", id: "mimo-v2.5" },
        sessionManager: { getSessionId: () => "sess-42" },
      },
    );
    expect(Object.keys(h1)).toHaveLength(0);
    // existing value wins
    const h2: Record<string, string | null> = { "x-opencode-session": "pi-set-this" };
    hook(
      { type: "before_provider_headers", headers: h2 },
      {
        model: { provider: "aiproxy", id: "opencode-go/mimo-v2.5" },
        sessionManager: { getSessionId: () => "ours" },
      },
    );
    expect(h2["x-opencode-session"]).toBe("pi-set-this");
  });
});

describe("loadCatalog — fetches from pi.dev and caches", () => {
  const piDevResponse = {
    ok: true,
    json: async () => ({
      "mimo-v2.5": {
        id: "mimo-v2.5",
        name: "MiMo",
        contextWindow: 1_000_000,
        maxTokens: 32_000,
        reasoning: true,
      },
    }),
  };

  it("fetches from pi.dev for each upstream kind and merges results", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(piDevResponse);
    const catalog = await loadCatalog(["opencode-go"], fetchImpl as unknown as typeof fetch);
    expect(catalog.get("opencode-go/mimo-v2.5")).toMatchObject({
      contextWindow: 1_000_000,
      reasoning: true,
    });
    expect(fetchImpl).toHaveBeenCalledWith(
      expect.stringContaining("pi.dev/api/models/providers/opencode-go"),
      expect.anything(),
    );
  });

  it("degrades gracefully when pi.dev is unreachable", async () => {
    const fetchImpl = vi.fn().mockRejectedValue(new Error("ENOTFOUND"));
    const catalog = await loadCatalog(["opencode-go"], fetchImpl as unknown as typeof fetch);
    // no crash, just empty (or from models-store.json)
    expect(catalog).toBeInstanceOf(Map);
  });

  it("merges multiple upstream kinds in parallel", async () => {
    const kinds = ["opencode-go", "minimax"];
    const fetchImpl = vi.fn().mockResolvedValue(piDevResponse);
    const catalog = await loadCatalog(kinds, fetchImpl as unknown as typeof fetch);
    // Both kinds fetched
    expect(fetchImpl).toHaveBeenCalledTimes(2);
    expect(catalog.has("opencode-go/mimo-v2.5")).toBe(true);
    expect(catalog.has("minimax/mimo-v2.5")).toBe(true);
  });
});
