// Regression tests for the `/setup` page script, run against the real HTML the
// proxy serves (`src/setup_page.html`) in a jsdom document.
//
// These exist because the page re-renders its card from `GET /api/codex/status`
// on a 2s tick: an implementation that swaps `innerHTML` wholesale destroys
// anything the user typed or pasted, which silently ate pasted callback URLs.
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { JSDOM } from "jsdom";
import { describe, expect, it } from "vitest";

const here = dirname(fileURLToPath(import.meta.url));
const PAGE = readFileSync(resolve(here, "../../../../../src/setup_page.html"), "utf8");

const REDIRECT = "http://localhost:1455/auth/callback?code=ac_1&state=st_1";

/** Waiting-for-paste status: nothing listens on the callback port, so the page
 * always asks the user for the redirect URL. */
const AUTHORIZING = {
  provider: "openai-codex",
  state: "authorizing",
  method: "browser",
  auth_url: "https://auth.openai.com/oauth/authorize?x=1",
  redirect_uri: "http://localhost:1455/auth/callback",
  expires_in: 900,
  state_path: "/srv/aiproxy/openai-codex-oauth-state.json",
  state_file: { exists: false },
};

interface Posted {
  url: string;
  body: unknown;
}

/** jsdom document with a stubbed fetch; returns the DOM plus what was POSTed.
 * `subs` stands in for GET /api/codex/providers (the multi-subscription list). */
async function page(status: Record<string, unknown>, subs?: unknown[]) {
  const posted: Posted[] = [];
  const dom = new JSDOM(PAGE, {
    runScripts: "dangerously",
    url: "http://127.0.0.1:8080/setup",
    beforeParse(window) {
      window.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (init?.method === "POST") {
          posted.push({ url, body: JSON.parse(String(init.body)) });
          return new Response(JSON.stringify({ state: "logged_in" }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        const body = url.startsWith("/api/codex/providers")
          ? { providers: subs ?? [] }
          : status;
        return new Response(JSON.stringify(body), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }) as typeof window.fetch;
    },
  });
  await settle(dom);
  return { dom, posted };
}

const TWO_SUBS = [
  {
    id: "openai-codex=alice",
    logged_in: true,
    state_path: "/runtime/openai-codex=alice-oauth-state.json",
    state_file: { exists: true, size: 300 },
  },
  {
    id: "openai-codex=bob",
    logged_in: false,
    state_path: "/runtime/openai-codex=bob-oauth-state.json",
    state_file: { exists: false },
  },
];

/** Let the page's initial async render finish. */
async function settle(dom: JSDOM) {
  for (let i = 0; i < 10; i++) {
    await new Promise((r) => setTimeout(r, 0));
  }
  void dom;
}

/** Text of the rendered card. `body.textContent` would also match the inline
 * `<script>` source, which makes "does the page say X" assertions vacuous. */
function cardText(dom: JSDOM): string {
  const card = dom.window.document.querySelector(".card");
  if (!card) throw new Error("no card rendered");
  return (card.textContent ?? "").replace(/\s+/g, " ").trim();
}

function pasteBox(dom: JSDOM): HTMLInputElement {
  const el = dom.window.document.getElementById("paste");
  if (!(el instanceof dom.window.HTMLInputElement)) {
    throw new Error("no paste box in the rendered card");
  }
  return el;
}

/** The page's own 2s status tick. */
async function tick(dom: JSDOM) {
  await (dom.window as unknown as { refresh: () => Promise<void> }).refresh();
  await settle(dom);
}

describe("setup page", () => {
  it("asks for the pasted redirect URL", async () => {
    const { dom } = await page(AUTHORIZING);
    expect(cardText(dom)).toContain("http://localhost:1455/auth/callback");
    expect(cardText(dom)).toContain("connection error");
    expect(pasteBox(dom).value).toBe("");
  });

  it("keeps a pasted callback URL across a status refresh", async () => {
    const { dom } = await page(AUTHORIZING);
    pasteBox(dom).value = REDIRECT;

    await tick(dom);

    expect(pasteBox(dom).value).toBe(REDIRECT);
  });

  it("keeps the paste box focused while the user is typing into it", async () => {
    const { dom } = await page(AUTHORIZING);
    const box = pasteBox(dom);
    box.focus();

    await tick(dom);

    expect(dom.window.document.activeElement?.id).toBe("paste");
  });

  it("submits the pasted URL to /api/codex/complete", async () => {
    const { dom, posted } = await page(AUTHORIZING);
    pasteBox(dom).value = REDIRECT;
    await tick(dom);

    await (
      dom.window as unknown as { finish: () => Promise<void> }
    ).finish();
    await settle(dom);

    expect(posted).toEqual([
      { url: "/api/codex/complete", body: { input: REDIRECT } },
    ]);
    expect(cardText(dom)).toContain("Connected");
  });

  it("recovers the real card after a failed paste-back", async () => {
    const { dom } = await page(AUTHORIZING);
    pasteBox(dom).value = "garbage";
    // the proxy rejects it: /api/codex/complete answers 400
    dom.window.fetch = (async () =>
      new Response(JSON.stringify({ error: { message: "state mismatch" } }), {
        status: 400,
        headers: { "content-type": "application/json" },
      })) as unknown as typeof dom.window.fetch;

    await (dom.window as unknown as { finish: () => Promise<void> }).finish();
    await settle(dom);
    expect(cardText(dom)).toContain("state mismatch");

    // next status tick shows the login card again, not the stale error
    dom.window.fetch = (async () =>
      new Response(JSON.stringify(AUTHORIZING), {
        status: 200,
        headers: { "content-type": "application/json" },
      })) as unknown as typeof dom.window.fetch;
    await tick(dom);

    expect(cardText(dom)).toContain("Waiting for the browser login");
    expect(cardText(dom)).not.toContain("state mismatch");
  });

  it("shows where credentials are looked for", async () => {
    const { dom } = await page(AUTHORIZING);
    expect(cardText(dom)).toContain("/srv/aiproxy/openai-codex-oauth-state.json");
    expect(cardText(dom)).toContain("no credentials file");
  });

  it("points at /models once connected", async () => {
    const { dom } = await page({
      provider: "openai-codex",
      state: "logged_in",
      state_path: "/srv/aiproxy/openai-codex-oauth-state.json",
      state_file: { exists: true, size: 412 },
    });
    expect(cardText(dom)).toContain("Connected");
    expect(cardText(dom)).toContain("credentials file present (412 bytes)");
    const link = dom.window.document.querySelector(".card a");
    expect(link?.getAttribute("href")).toBe("/models");
  });

  it("offers a picker when the proxy serves several subscriptions", async () => {
    const { dom } = await page(AUTHORIZING, TWO_SUBS);
    const text = cardText(dom);
    expect(text).toContain("2 openai-codex upstreams");
    const links = [...dom.window.document.querySelectorAll(".card a")];
    const hrefs = links.map((a) => a.getAttribute("href"));
    // one link per subscription, carrying the provider id (encoded — the id
    // contains "=")
    expect(hrefs).toContain("?provider=openai-codex%3Dalice");
    expect(hrefs).toContain("?provider=openai-codex%3Dbob");
    expect(text).toContain("connected");
    expect(text).toContain("not logged in");
  });

  it("shows the login card, not the picker, with a single subscription", async () => {
    const { dom } = await page(AUTHORIZING, [TWO_SUBS[0]]);
    expect(cardText(dom)).toContain("Waiting for the browser login");
    expect(cardText(dom)).not.toContain("upstreams");
  });

  it("does not re-render an unchanged status into the DOM", async () => {
    const { dom } = await page(AUTHORIZING);
    const before = dom.window.document.querySelector(".card");
    await tick(dom);
    expect(dom.window.document.querySelector(".card")).toBe(before);
  });
});
