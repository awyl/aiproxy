import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { afterEach, describe, expect, it, vi } from "vitest";
import { registerProxyProvider, type ProxyModel } from "../provider.ts";
import { registerUsage } from "../usage.ts";

afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllTimers();
  vi.useRealTimers();
});

const usage = [
  { provider: "openai-codex=bob", windows: [{ label: "5h", used_percent: 84 }], pools: [], updated_at: 1_800_000_000_000 },
  { provider: "openai-codex=alice", windows: [{ label: "5h", used_percent: 42, reset_secs: 3600 }], pools: [], updated_at: 1_800_000_000_000 },
  { provider: "opencode-go=team", windows: [{ label: "5h", used_percent: 7 }], pools: [], updated_at: 1_800_000_000_000 },
];

async function harness(id: string, initialModels: ProxyModel[]) {
  let models = initialModels;
  const handlers = new Map<string, Array<(...args: any[]) => unknown>>();
  const pi = {
    registerProvider: vi.fn(),
    on(event: string, handler: (...args: any[]) => unknown) {
      handlers.set(event, [...(handlers.get(event) ?? []), handler]);
    },
  } as unknown as ExtensionAPI;
  const widgets: Array<string[] | undefined> = [];
  const ctx = {
    model: { provider: "aiproxy", id },
    isIdle: () => true,
    hasUI: true,
    ui: {
      setWidget: (_id: string, lines?: string[]) => widgets.push(lines),
      notify: vi.fn(),
    },
  };
  const fetchImpl = vi.fn(async (url: string | URL | Request) => {
    const body = String(url).endsWith("/models") ? { data: models } : usage;
    return new Response(JSON.stringify(body), { headers: { "content-type": "application/json" } });
  });
  vi.stubGlobal("fetch", fetchImpl);
  const cfg = { base: "http://proxy/v1", apiKey: "" };
  const targetOf = await registerProxyProvider(pi, cfg, { catalog: new Map(), fetchImpl: fetchImpl as typeof fetch });
  registerUsage(pi, cfg, targetOf);
  async function emit(event: string) {
    for (const handler of handlers.get(event) ?? []) await handler({}, ctx as unknown as ExtensionContext);
  }
  return { widgets, ctx, emit, setModels: (next: ProxyModel[]) => { models = next; } };
}

describe("usage widget", () => {
  it("matches full provider ID for named subscriptions", async () => {
    const h = await harness("openai-codex=alice/gpt-6.1-sol", []);
    await h.emit("before_provider_headers");
    await h.emit("agent_start");
    expect(h.widgets).toEqual([["[aiproxy] 5h 42% (1h)"]]);
  });

  it("shows only the alias target subscription's usage", async () => {
    const h = await harness("default/planner", [
      { id: "default/planner", target: "openai-codex=alice/gpt-6.1-sol", surface: "responses" },
    ]);
    await h.emit("before_provider_headers");
    await h.emit("agent_start");
    expect(h.widgets).toEqual([["[aiproxy] 5h 42% (1h)"]]);
  });

  it("selects the target's usage on the first turn before request headers fire", async () => {
    const h = await harness("default/planner", [
      { id: "default/planner", target: "openai-codex=alice/gpt-6.1-sol", surface: "responses" },
    ]);
    await h.emit("agent_start");
    expect(h.widgets).toEqual([["[aiproxy] 5h 42% (1h)"]]);
  });

  it("follows a refreshed alias target without another provider request", async () => {
    vi.useFakeTimers();
    const h = await harness("default/planner", [
      { id: "default/planner", target: "openai-codex=alice/gpt-6.1-sol", surface: "responses" },
    ]);
    await h.emit("session_start");
    await h.emit("before_provider_headers");
    await h.emit("agent_start");
    h.setModels([{ id: "default/planner", target: "opencode-go=team/mimo-v2.5", surface: "chat" }]);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(h.widgets.at(-1)).toEqual(["[aiproxy] 5h 7%"]);
    await h.emit("session_shutdown");
  });
});
