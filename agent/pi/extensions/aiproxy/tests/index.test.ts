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
    try {
      const { default: factory } = await import("../index.ts");
      const pi = { registerTool: vi.fn(), registerProvider: vi.fn() };
      await factory(pi as never);
      expect(process.env.AIPROXY_TOKEN).toBe("cfgtok");
      expect(pi.registerProvider).toHaveBeenCalledTimes(1);
      expect(pi.registerTool).not.toHaveBeenCalled(); // no mcpServers configured
    } finally {
      if (before === undefined) delete process.env.AIPROXY_TOKEN;
      else process.env.AIPROXY_TOKEN = before;
    }
  });

  it("does not touch the env when no config exists", async () => {
    h.home = join(home, "does-not-exist");
    try {
      vi.resetModules();
      const { default: factory } = await import("../index.ts");
      const pi = { registerTool: vi.fn(), registerProvider: vi.fn() };
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
