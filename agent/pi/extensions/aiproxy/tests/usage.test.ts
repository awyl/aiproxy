import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { describe, expect, it, vi, afterEach } from "vitest";
import { registerUsage } from "../usage.ts";

afterEach(() => vi.unstubAllGlobals());

describe("usage widget", () => {
  it("matches full provider ID for named subscriptions", async () => {
    const handlers = new Map<string, (...args: unknown[]) => unknown>();
    const pi = {
      on(event: string, handler: (...args: unknown[]) => unknown) {
        handlers.set(event, handler);
      },
    } as unknown as ExtensionAPI;
    const widgets: Array<string[] | undefined> = [];
    const ctx = {
      ui: { setWidget: (_id: string, lines?: string[]) => widgets.push(lines) },
    } as unknown as ExtensionContext;
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify([
      {
        provider: "openai-codex=alice",
        windows: [{ label: "5h", used_percent: 42, reset_secs: 3600 }],
        pools: [],
        updated_at: 1_800_000_000_000,
      },
    ]), { status: 200, headers: { "content-type": "application/json" } })));

    registerUsage(pi, { base: "http://proxy" });
    handlers.get("before_provider_headers")?.({}, {
      model: { provider: "aiproxy", id: "openai-codex=alice/gpt-6.1-sol" },
    });
    await handlers.get("agent_start")?.({}, ctx);

    expect(widgets).toEqual([["[aiproxy] 5h 42% (1h)"]]);
  });
});
