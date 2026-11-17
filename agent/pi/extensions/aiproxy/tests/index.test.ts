import { describe, it, expect, vi, beforeAll, afterAll } from "vitest";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const h = vi.hoisted(() => ({ home: "" }));
vi.mock("node:os", async (importOriginal) => {
  const actual = await importOriginal();
  return { ...(actual as object), homedir: () => h.home };
});

let home = "";
beforeAll(() => {
  home = mkdtempSync(join(tmpdir(), "aiproxy-ext-"));
  h.home = home;
  mkdirSync(join(home, ".pi", "agent"), { recursive: true });
  writeFileSync(
    join(home, ".pi", "agent", "aiproxy.json"),
    JSON.stringify({ baseUrl: "http://cfg:9999/v1", apiKey: "cfgtok" }),
  );
});

afterAll(() => {
  rmSync(home, { recursive: true, force: true });
});

describe("index (glue)", () => {
  it("sets AIPROXY_TOKEN env from config and registers both halves", async () => {
    const before = process.env.AIPROXY_TOKEN;
    // The factory registers the provider with its own fetch, which would reach
    // out to the configured baseUrl — a real network call that made this test
    // flaky (5s timeout) whenever the suite ran in parallel.
    const realFetch = globalThis.fetch;
    globalThis.fetch = (async () =>
      new Response(JSON.stringify({ data: [] }), {
        status: 200,
        headers: { "content-type": "application/json" },
      })) as typeof fetch;
    try {
      const { default: factory } = await import("../index.ts");
      const pi = { registerTool: vi.fn(), registerProvider: vi.fn(), on: vi.fn() };
      await factory(pi as never);
      expect(process.env.AIPROXY_TOKEN).toBe("cfgtok");
      expect(pi.registerProvider).toHaveBeenCalledTimes(1);
      expect(pi.registerTool).not.toHaveBeenCalled(); // no mcpServers configured
    } finally {
      globalThis.fetch = realFetch;
      if (before === undefined) delete process.env.AIPROXY_TOKEN;
      else process.env.AIPROXY_TOKEN = before;
    }
  });

  it("wires alias target routing into the usage widget", async () => {
    const before = process.env.AIPROXY_TOKEN;
    const realFetch = globalThis.fetch;
    globalThis.fetch = (async (url: string | URL | Request) => {
      const body = String(url).endsWith("/usage") ? [
        { provider: "openai-codex=bob", windows: [{ label: "5h", used_percent: 84 }] },
        { provider: "openai-codex=alice", windows: [{ label: "5h", used_percent: 42 }] },
      ] : { data: [{ id: "default/planner", target: "openai-codex=alice/gpt-6.1-sol", surface: "responses" }] };
      return new Response(JSON.stringify(body), { headers: { "content-type": "application/json" } });
    }) as typeof fetch;
    try {
      const { default: factory } = await import("../index.ts");
      const pi = { registerTool: vi.fn(), registerProvider: vi.fn(), on: vi.fn() };
      await factory(pi as never);
      const setWidget = vi.fn();
      const ctx = {
        model: { provider: "aiproxy", id: "default/planner" },
        ui: { setWidget },
      };
      for (const [event, handler] of pi.on.mock.calls) {
        if (event === "before_provider_headers") await handler({ headers: {} }, ctx);
      }
      for (const [event, handler] of pi.on.mock.calls) {
        if (event === "agent_start") await handler({}, ctx);
      }
      expect(setWidget).toHaveBeenCalledWith("aiproxy-usage", ["[aiproxy] 5h 42%"], { placement: "belowEditor" });
    } finally {
      globalThis.fetch = realFetch;
      if (before === undefined) delete process.env.AIPROXY_TOKEN;
      else process.env.AIPROXY_TOKEN = before;
    }
  });

  it("does not touch the env when no config exists", async () => {
    h.home = join(home, "does-not-exist");
    try {
      vi.resetModules();
      const { default: factory } = await import("../index.ts");
      const pi = { registerTool: vi.fn(), registerProvider: vi.fn(), on: vi.fn() };
      delete process.env.AIPROXY_TOKEN;
      await factory(pi as never);
      expect(process.env.AIPROXY_TOKEN).toBeUndefined();
      expect(pi.registerProvider).toHaveBeenCalledTimes(1);
    } finally {
      h.home = home;
      vi.resetModules();
    }
  });
});
