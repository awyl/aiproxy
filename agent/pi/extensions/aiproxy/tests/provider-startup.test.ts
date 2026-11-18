import { existsSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { afterEach, expect, it, vi } from "vitest";
import { registerProxyProvider } from "../provider.ts";

vi.mock("node:fs", () => ({
  existsSync: vi.fn(),
  readFileSync: vi.fn(),
  statSync: vi.fn(),
  mkdirSync: vi.fn(),
  writeFileSync: vi.fn(),
}));

const config = { base: "http://proxy.test/v1", apiKey: "fixture" };
const rows = (data: unknown) => ({ ok: true, json: async () => data });
const models = rows({ data: [{ id: "opencode-go/demo", surface: "chat" }] });
const metadata = rows([{ id: "demo", contextWindow: 99_000 }]);

afterEach(() => {
  vi.resetAllMocks();
  vi.unstubAllGlobals();
});

it("starts proxy discovery without waiting on pi.dev metadata", async () => {
  let release!: (value: typeof metadata) => void;
  const pending = new Promise<typeof metadata>((resolve) => { release = resolve; });
  const fetcher = vi.fn((url: string) => url.startsWith(config.base) ? Promise.resolve(models) : pending);
  const pi = { registerProvider: vi.fn(), on: vi.fn() };
  const done = registerProxyProvider(pi as never, config, { fetchImpl: fetcher as never });
  try {
    expect(fetcher).toHaveBeenCalledWith(config.base + "/models", expect.anything());
  } finally {
    release(metadata);
    await done;
  }
});

it("registers with fresh disk metadata while refreshing that cache in the background", async () => {
  vi.mocked(existsSync).mockImplementation((path) => String(path).endsWith("/aiproxy-models.json"));
  vi.mocked(statSync).mockReturnValue({ mtimeMs: Date.now() } as never);
  vi.mocked(readFileSync).mockReturnValue(JSON.stringify({ "opencode-go/demo": { contextWindow: 42_000 } }));
  let release!: (value: typeof metadata) => void;
  const pending = new Promise<typeof metadata>((resolve) => { release = resolve; });
  vi.stubGlobal("fetch", vi.fn((url: string) => url.startsWith(config.base) ? Promise.resolve(models) : pending));
  const pi = { registerProvider: vi.fn(), on: vi.fn() };
  const done = registerProxyProvider(pi as never, config);
  try {
    await vi.waitFor(() => expect(pi.registerProvider).toHaveBeenCalled(), { timeout: 300 });
    expect(pi.registerProvider.mock.calls[0]?.[1].models[0].contextWindow).toBe(42_000);
    expect(writeFileSync).not.toHaveBeenCalled();
  } finally {
    release(metadata);
    await done;
  }
  await vi.waitFor(() => expect(writeFileSync).toHaveBeenCalledWith(
    expect.stringContaining("aiproxy-models.json"), expect.stringContaining("99000")));
});
