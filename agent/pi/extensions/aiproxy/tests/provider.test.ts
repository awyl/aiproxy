import { describe, it, expect, vi, beforeEach } from "vitest";
import { registerProxyProvider, attributionHeaders, loadCatalog, SURFACE_API, fromCatalog } from "../provider.ts";

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

  it("registers every subscription of one kind, catalog-matched by kind", async () => {
    // Two openai-codex upstreams: ids carry the subscription suffix, the catalog
    // key does not — both must resolve to the same metadata and stay distinct
    // model ids for pi.
    const catalog = new Map([
      ["openai-codex/gpt-5.6-sol", { api: "openai-codex-responses", contextWindow: 272_000 }],
    ]);
    const { pi } = await registerWith(
      {
        ok: true,
        json: async () => ({
          data: [
            { id: "openai-codex=alice/gpt-5.6-sol", surface: "responses" },
            { id: "openai-codex=bob/gpt-5.6-sol", surface: "responses" },
          ],
        }),
      },
      catalog,
    );
    const cfg = pi.registerProvider.mock.calls[0][1];
    expect(cfg.models).toHaveLength(2);
    for (const id of ["openai-codex=alice/gpt-5.6-sol", "openai-codex=bob/gpt-5.6-sol"]) {
      const m = cfg.models.find((x: { id: string }) => x.id === id);
      expect(m, `${id} must register`).toBeTruthy();
      expect(m.contextWindow).toBe(272_000);
      expect(m.api).toBe("openai-responses");
    }
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

  it("fetches the openai-codex catalog under pi's own provider id", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(piDevResponse);
    await loadCatalog(["openai-codex"], fetchImpl as unknown as typeof fetch);
    // no kind→catalog aliasing: the proxy kind is already pi's provider id
    expect(fetchImpl).toHaveBeenCalledWith(
      expect.stringContaining("pi.dev/api/models/providers/openai-codex"),
      expect.anything(),
    );
  });

  it("codex models stream over the responses surface", () => {
    expect(SURFACE_API["responses"]).toBe("openai-responses");
    const model = fromCatalog("openai-codex/gpt-5.6-sol", {}, "responses");
    expect(model.api).toBe("openai-responses");
    expect(model.baseUrl).toBeUndefined();
  });

  it("never hands pi a chatgpt-specific driver for a proxied codex model", () => {
    // pi.dev's openai-codex catalog advertises api "openai-codex-responses",
    // which talks to chatgpt.com with its own OAuth — through aiproxy the
    // generic responses wire is the correct one.
    const model = fromCatalog(
      "openai-codex/gpt-5.6-sol",
      { api: "openai-codex-responses" },
    );
    expect(model.api).toBe("openai-responses");
  });
});


describe("model aliases", () => {
  it("keeps alias identity while inheriting target metadata and generic Responses wire", async () => {
    const meta = { name:"Real planner", api:"openai-codex-responses", reasoning:true,
      input:["text","image"], contextWindow:272_000, maxTokens:32_000,
      thinkingLevelMap:{high:"high"}, cost:{input:1,output:2,cacheRead:0.1,cacheWrite:0},
      headers:{"x-test":"target"}, compat:{supportsStore:false} };
    const { pi } = await registerWith({ok:true,json:async()=>({data:[
      {id:"default/planner",target:"openai-codex=alice/gpt-sol",surface:"responses",display_name:"default/planner"}
    ]})}, new Map([["openai-codex/gpt-sol",meta]]));
    const model = pi.registerProvider.mock.calls[0][1].models[0];
    expect(model).toMatchObject({...meta, id:"default/planner",name:"default/planner",api:"openai-responses"});
    expect(model.baseUrl).toBeUndefined();
  });

  it("preserves nested target paths and the messages wire base", async () => {
    const { pi } = await registerWith({ok:true,json:async()=>({data:[
      {id:"default/writer",target:"openrouter/anthropic/claude",surface:"messages"}
    ]})}, new Map([["openrouter/anthropic/claude",{reasoning:true,contextWindow:200_000}]]));
    expect(pi.registerProvider.mock.calls[0][1].models[0]).toMatchObject({
      id:"default/writer", name:"default/writer", reasoning:true,contextWindow:200_000,
      api:"anthropic-messages",baseUrl:"http://p:9999"
    });
  });

  it("uses existing fallback metadata when a target is not in pi's catalog", async () => {
    const { pi } = await registerWith({ok:true,json:async()=>({data:[
      {id:"default/unknown",target:"openai-codex/new-model",surface:"responses"}
    ]})});
    expect(pi.registerProvider.mock.calls[0][1].models[0]).toMatchObject({
      id:"default/unknown", name:"default/unknown",api:"openai-responses",reasoning:false,contextWindow:128_000
    });
  });

  it("attaches target provider session headers while preserving explicit headers", async () => {
    const { pi } = await registerWith({ok:true,json:async()=>({data:[
      {id:"default/fast",target:"opencode-go=alice/deepseek",surface:"chat"}
    ]})});
    const hook = pi.on.mock.calls.find(([name]) => name === "before_provider_headers")![1];
    const headers = {"x-opencode-client":"explicit"} as Record<string,string>;
    hook({headers},{model:{provider:"aiproxy",id:"default/fast"},sessionManager:{getSessionId:()=>"session-123"}});
    expect(headers).toEqual({"x-opencode-session":"session-123","x-opencode-client":"explicit"});
    const unrelated: Record<string,string> = {};
    hook({headers:unrelated},{model:{provider:"another",id:"default/fast"}});
    expect(unrelated).toEqual({});
  });
});

describe("automatic provider refresh", () => {
  it("updates target capabilities, wire driver and header attribution without duplicate hooks", async () => {
    vi.useFakeTimers();
    const pi = stubPi();
    const initial = [{id:"default/fast",target:"opencode-go/old",surface:"chat"}];
    const changed = [{id:"default/fast",target:"openai-codex/new",surface:"responses"}];
    const fetchImpl = vi.fn()
      .mockResolvedValueOnce({ok:true,json:async()=>({data:initial})})
      .mockResolvedValue({ok:true,json:async()=>({data:changed})});
    try {
      await registerProxyProvider(pi as never,{base:"http://p/v1",apiKey:"k",token:"k"},{
        fetchImpl:fetchImpl as never,
        catalog:new Map([["openai-codex/new",{reasoning:true,contextWindow:272_000,maxTokens:32_000}]]),
      });
      const ctx = {isIdle:()=>true,model:{provider:"aiproxy",id:"default/fast"},hasUI:true,ui:{notify:vi.fn()},sessionManager:{getSessionId:()=>"s"}};
      const headerHook = pi.on.mock.calls.find(([name])=>name==="before_provider_headers")![1];
      const before: Record<string,string> = {};
      headerHook({headers:before},ctx);
      expect(before["x-opencode-session"]).toBe("s");
      await pi.on.mock.calls.find(([name])=>name==="session_start")?.[1]({},ctx);
      await vi.advanceTimersByTimeAsync(60_000);
      expect(pi.registerProvider).toHaveBeenCalledTimes(2);
      const model = pi.registerProvider.mock.calls[1][1].models[0];
      expect(model).toMatchObject({id:"default/fast",api:"openai-responses",reasoning:true,contextWindow:272_000,maxTokens:32_000});
      const after: Record<string,string> = {};
      headerHook({headers:after},ctx);
      expect(after).toEqual({});
      expect(pi.on.mock.calls.filter(([name])=>name==="before_provider_headers")).toHaveLength(1);
      expect(fetchImpl.mock.calls[1][1]).toMatchObject({headers:{Authorization:"Bearer k"},signal:expect.any(AbortSignal)});
      await pi.on.mock.calls.find(([name])=>name==="session_shutdown")?.[1]({},ctx);
    } finally { vi.clearAllTimers();vi.useRealTimers(); }
  });

  it("retains working provider models on a malformed refresh response",async()=>{
    vi.useFakeTimers();
    const pi=stubPi();
    const fetchImpl=vi.fn().mockResolvedValueOnce(modelsResponse).mockResolvedValue({ok:true,json:async()=>({data:[{}]})});
    const ctx={isIdle:()=>true,hasUI:true,ui:{notify:vi.fn()}};
    try {
      await registerProxyProvider(pi as never,{base:"http://p/v1",apiKey:""},{fetchImpl:fetchImpl as never,catalog:new Map()});
      await pi.on.mock.calls.find(([name])=>name==="session_start")?.[1]({},ctx);
      await vi.advanceTimersByTimeAsync(60_000);
      expect(pi.registerProvider).toHaveBeenCalledTimes(1);
      expect(ctx.ui.notify).toHaveBeenCalledWith(expect.stringContaining("keeping the last catalog"),"warning");
      await pi.on.mock.calls.find(([name])=>name==="session_shutdown")?.[1]({},ctx);
    } finally {vi.clearAllTimers();vi.useRealTimers();}
  });
});
