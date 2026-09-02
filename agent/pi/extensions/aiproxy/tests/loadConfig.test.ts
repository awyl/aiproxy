import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const h = vi.hoisted(() => ({ home: "" }));
vi.mock("node:os", async (importOriginal) => {
  const actual = await importOriginal();
  return { ...(actual as object), homedir: () => h.home };
});

let home = "";
let proj = "";
beforeEach(() => {
  home = mkdtempSync(join(tmpdir(), "aiproxy-gl-"));
  proj = mkdtempSync(join(tmpdir(), "aiproxy-pr-"));
  h.home = home;
});

afterEach(() => {
  rmSync(home, { recursive: true, force: true });
  rmSync(proj, { recursive: true, force: true });
});

async function load() {
  const { loadConfig } = await import("../index.ts");
  return loadConfig(proj);
}

function writeGlobal(cfg: object) {
  mkdirSync(join(home, ".pi", "agent"), { recursive: true });
  writeFileSync(join(home, ".pi", "agent", "aiproxy.json"), JSON.stringify(cfg));
}

function writeProject(cfg: object) {
  mkdirSync(join(proj, ".pi"), { recursive: true });
  writeFileSync(join(proj, ".pi", "aiproxy.json"), JSON.stringify(cfg));
}

describe("loadConfig precedence", () => {
  it("uses the global file as default", async () => {
    writeGlobal({ baseUrl: "http://global:8080/v1", apiKey: "gtok", mcpServers: "grep" });
    const cfg = await load();
    expect(cfg.base).toBe("http://global:8080/v1");
    expect(cfg.apiKey).toBe("gtok");
    expect(cfg.mcpServers).toBe("grep");
  });

  it("project overrides individual fields, inherits the rest from global", async () => {
    writeGlobal({ baseUrl: "http://global:8080/v1", apiKey: "gtok", mcpServers: "grep" });
    writeProject({ apiKey: "$PROJ_TOKEN" });
    process.env.PROJ_TOKEN = "resolved";
    try {
      const cfg = await load();
      expect(cfg.base).toBe("http://global:8080/v1"); // inherited
      expect(cfg.apiKey).toBe("$PROJ_TOKEN"); // overridden
      expect(cfg.token).toBe("resolved"); // $ENV resolved after merge
      expect(cfg.mcpServers).toBe("grep"); // inherited
    } finally {
      delete process.env.PROJ_TOKEN;
    }
  });

  it("project-only config works without a global file", async () => {
    writeProject({ baseUrl: "http://proj:8080/v1" });
    const cfg = await load();
    expect(cfg.base).toBe("http://proj:8080/v1");
    expect(cfg.apiKey).toBe("");
  });

  it("falls back to defaults when neither exists", async () => {
    const cfg = await load();
    expect(cfg.base).toBe("http://127.0.0.1:8080/v1");
    expect(cfg.apiKey).toBe("");
    expect(cfg.mcpServers).toBeUndefined();
  });
});
