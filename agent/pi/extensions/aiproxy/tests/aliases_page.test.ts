import { readFileSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { JSDOM } from "jsdom";
import { describe, expect, it } from "vitest";

const PAGE = readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "../../../../../src/aliases_page.html"), "utf8");
const original = [{ id: "default/fast", target: "openai/m", surface: "chat", available: true }];
const catalog = { providers: [{ id: "openai", models: [{ id: "m", surface: "chat" }, { id: "new", surface: "chat" }] }] };
async function settle() { await new Promise(r => setTimeout(r, 15)); }

async function page(aliases = original, failSave = false) {
  let saved: Record<string, string> | undefined;
  let lastAuthorization: string | undefined;
  const dom = new JSDOM(PAGE, {
    runScripts: "dangerously", url: "http://proxy/aliases",
    beforeParse(w) {
      w.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
        lastAuthorization = (init?.headers as Record<string,string> | undefined)?.Authorization;
        if (String(input) === "/api/models") return new Response(JSON.stringify(catalog));
        if (init?.method === "PUT") {
          saved = JSON.parse(String(init.body)).aliases;
          if (failSave) return new Response(JSON.stringify({error:{message:"disk full"}}), { status: 500 });
          aliases = Object.entries(saved!).map(([id,target]) => ({ id, target, available: true, surface: "chat" }));
        }
        return new Response(JSON.stringify({ aliases, state_path: "/runtime/aiproxy-aliases.json" }));
      }) as typeof w.fetch;
    },
  });
  await settle();
  const doc = dom.window.document;
  const click = async (id: string) => { (doc.getElementById(id) as HTMLButtonElement).click(); await settle(); };
  return { dom, doc, click, saved: () => saved, authorization: () => lastAuthorization };
}

describe("alias editor", () => {
  it("lists mappings, target choices, and the durable file path", async () => {
    const p = await page();
    expect(p.doc.querySelectorAll("tr[data-alias]").length).toBe(1);
    expect((p.doc.querySelector('[aria-label="Alias name"]') as HTMLInputElement).value).toBe("fast");
    expect([...p.doc.querySelectorAll("select option")].map(x => (x as HTMLOptionElement).value)).toContain("openai/new");
    expect(p.doc.body.textContent).toContain("/runtime/aiproxy-aliases.json");
    p.dom.window.close();
  });

  it("creates, edits and deletes mappings using a full-map save", async () => {
    const p = await page();
    expect(p.doc.querySelectorAll("tr[data-alias]").length).toBe(1);
    (p.doc.querySelector("select") as HTMLSelectElement).value = "openai/new";
    await p.click("add");
    const rows = p.doc.querySelectorAll("tr[data-alias]");
    (rows[1].querySelector("input") as HTMLInputElement).value = "planner";
    (rows[1].querySelector("select") as HTMLSelectElement).value = "openai/m";
    await p.click("save");
    expect(p.saved()).toEqual({ "default/fast":"openai/new", "default/planner":"openai/m" });
    (p.doc.querySelector('button[data-delete]') as HTMLButtonElement).click();
    await p.click("save");
    expect(p.saved()).toEqual({ "default/planner":"openai/m" });
    p.dom.window.close();
  });

  it("keeps unsaved edits visible when saving fails", async () => {
    const p = await page(original, true);
    expect(p.doc.querySelectorAll("tr[data-alias]").length).toBe(1);
    (p.doc.querySelector("select") as HTMLSelectElement).value = "openai/new";
    await p.click("save");
    expect(p.doc.getElementById("status")?.textContent).toContain("disk full");
    expect((p.doc.querySelector("select") as HTMLSelectElement).value).toBe("openai/new");
    p.dom.window.close();
  });

  it("retains unavailable targets so another alias can be edited", async () => {
    const p = await page([{ id:"default/fast", target:"openai/removed", surface: null as never, available:false }]);
    expect(p.doc.body.textContent).toContain("unavailable");
    expect((p.doc.querySelector("select") as HTMLSelectElement).value).toBe("openai/removed");
    p.dom.window.close();
  });

  it("renders hostile server text literally, without executing HTML", async () => {
    const target = '<img src=x onerror="window.pwned=true">';
    const p = await page([{ id:"default/fast", target, surface:null as never, available:false }]);
    expect(p.doc.body.textContent).toContain(target);
    expect(p.doc.querySelector("img")).toBeNull();
    p.dom.window.close();
  });

  it("saves without a per-page token or authorization header", async () => {
    const p = await page();
    expect(p.doc.querySelector('input[type="password"]')).toBeNull();
    await p.click("save");
    expect(p.saved()).toEqual({"default/fast":"openai/m"});
    expect(p.authorization()).toBeUndefined();
    expect(p.dom.window.localStorage.length).toBe(0);
    expect(p.dom.window.sessionStorage.length).toBe(0);
    p.dom.window.close();
  });

  it("rejects duplicate names instead of silently dropping a row", async () => {
    const p = await page();
    expect(p.doc.querySelectorAll("tr[data-alias]").length).toBe(1);
    await p.click("add");
    (p.doc.querySelectorAll('tr[data-alias] input')[1] as HTMLInputElement).value = "fast";
    await p.click("save");
    expect(p.saved()).toBeUndefined();
    expect(p.doc.getElementById("status")?.textContent).toContain("Duplicate");
    p.dom.window.close();
  });
});
