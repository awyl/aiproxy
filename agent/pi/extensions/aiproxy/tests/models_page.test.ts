// Regression tests for the `/models` page script, run against the real HTML the
// proxy serves (`src/models_page.html`) in a jsdom document.
//
// The page exists to answer "why is this model missing?", so it has to render
// every upstream — including the ones with no models and the ones whose probe
// failed — and it must not lose what the reader expanded when they press
// Reload (that re-render is where a UI like this usually collapses).
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { JSDOM } from "jsdom";
import { describe, expect, it } from "vitest";

const here = dirname(fileURLToPath(import.meta.url));
const PAGE = readFileSync(resolve(here, "../../../../../src/models_page.html"), "utf8");

interface Provider {
  id: string;
  count: number;
  models: { id: string; surface: string }[];
  error: string | null;
}

const CATALOG: Provider[] = [
  {
    id: "openai-codex",
    count: 2,
    models: [
      { id: "gpt-5.6-sol", surface: "responses" },
      { id: "gpt-5.5", surface: "responses" },
    ],
    error: null,
  },
  { id: "opencode-go", count: 0, models: [], error: null },
  { id: "nvidia", count: 0, models: [], error: "Transport(\"catalog probe failed: 401\")" },
];

const RELOADED: Provider[] = [
  {
    id: "openai-codex",
    count: 3,
    models: [
      { id: "gpt-5.6-sol", surface: "responses" },
      { id: "gpt-5.5", surface: "responses" },
      { id: "gpt-6.1-sol", surface: "responses" },
    ],
    error: null,
  },
  { id: "opencode-go", count: 0, models: [], error: null },
  { id: "nvidia", count: 0, models: [], error: null },
];

/** jsdom document with a stubbed fetch: GET /api/models returns `catalog`,
 * POST /api/reload returns `reloaded`. */
async function page(catalog = CATALOG, reloaded = RELOADED) {
  const calls: string[] = [];
  const dom = new JSDOM(PAGE, {
    runScripts: "dangerously",
    url: "http://127.0.0.1:8080/models",
    beforeParse(window) {
      window.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        calls.push((init?.method ?? "GET") + " " + url);
        const body = init?.method === "POST" ? reloaded : catalog;
        return new Response(JSON.stringify({ total: 3, providers: body }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }) as typeof window.fetch;
    },
  });
  await settle(dom);
  return { dom, calls };
}

/** Let the page's initial async render finish. */
async function settle(dom: JSDOM) {
  for (let i = 0; i < 10; i++) {
    await new Promise((r) => setTimeout(r, 0));
  }
  void dom;
}

function sections(dom: JSDOM): HTMLDetailsElement[] {
  return [...dom.window.document.querySelectorAll("details.provider")] as HTMLDetailsElement[];
}

function section(dom: JSDOM, id: string): HTMLDetailsElement {
  const el = sections(dom).find((d) => d.dataset.id === id);
  if (!el) throw new Error(`no section for ${id}`);
  return el;
}

/** What the reader did: expand a section (browser fires `toggle`). */
function expand(dom: JSDOM, id: string) {
  const el = section(dom, id);
  el.open = true;
  el.dispatchEvent(new dom.window.Event("toggle"));
}

async function reload(dom: JSDOM) {
  const button = dom.window.document.getElementById("go") as HTMLButtonElement;
  button.dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
  await settle(dom);
}

describe("models page", () => {
  it("lists every upstream, with its model count and models", async () => {
    const { dom } = await page();
    expect(sections(dom).map((d) => d.dataset.id)).toEqual([
      "openai-codex",
      "opencode-go",
      "nvidia",
    ]);
    const codex = section(dom, "openai-codex");
    expect(codex.querySelector("summary")?.textContent).toContain("2 models");
    const listed = [...codex.querySelectorAll("li")].map((li) => li.textContent);
    expect(listed).toEqual(["gpt-5.6-solresponses", "gpt-5.5responses"]);
    // collapsed until clicked
    expect(codex.open).toBe(false);
    expand(dom, "openai-codex");
    expect(section(dom, "openai-codex").open).toBe(true);
  });

  it("keeps a zero-model upstream visible and shows why its probe failed", async () => {
    const { dom } = await page();
    const empty = section(dom, "opencode-go");
    expect(empty.querySelector("summary")?.textContent).toContain("0 models");
    expect(empty.querySelectorAll("li").length).toBe(0);

    const failed = section(dom, "nvidia");
    expect(failed.querySelector("summary")?.textContent).toContain("probe failed");
    expect(failed.textContent).toContain("401");
  });

  it("reloads on the button and keeps the expanded sections expanded", async () => {
    const { dom, calls } = await page();
    expand(dom, "openai-codex");
    await reload(dom);

    expect(calls).toContain("POST /api/reload");
    const codex = section(dom, "openai-codex");
    expect(codex.open).toBe(true);
    expect(codex.querySelector("summary")?.textContent).toContain("3 models");
    // a section the reader never opened stays collapsed
    expect(section(dom, "opencode-go").open).toBe(false);
    // and the recovered upstream no longer claims a failure
    expect(section(dom, "nvidia").textContent).not.toContain("probe failed");
  });

  it("reads the current catalog on load, without probing", async () => {
    const { calls } = await page();
    expect(calls).toEqual(["GET /api/models"]);
  });
});
