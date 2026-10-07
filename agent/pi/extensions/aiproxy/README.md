# pi-extension-aiproxy

Pi provider that fronts an [aiproxy](../..) gateway. One provider entry; models
are auto-discovered from the gateway at startup, split across the three wire
surfaces the proxy speaks.

Model ids stay prefixed (`opencode-go/grok-4.6`); the proxy routes by prefix
and strips it. Provider IDs follow a scheme:

- 1 upstream of kind → ID = kind name (e.g. `opencode-go`)
- 2+ upstreams of kind → ID = kind=name (e.g. `opencode-go=alice`)

`api` is derived per model from the proxy's `surface` field:

| proxy surface | pi api |
|---|---|
| `chat` | `openai-completions` |
| `messages` | `anthropic-messages` |
| `responses` | `openai-responses` |

## Setup

```bash
npm install          # pull pi-ai + pi-coding-agent for types (edit-time only)
```

No extension env vars, no dependency on the proxy's yaml. The extension reads
its connection settings from its **own per-machine config file**
(`aiproxy.json` — user-level `~/.pi/agent/aiproxy.json`, or project
`.pi/aiproxy.json`):

```json
{
  "baseUrl": "http://127.0.0.1:8080/v1",
  "apiKey": "$AIPROXY_TOKEN",
  "mcpServers": "searxng,ctx7,grep"
}
```

- `baseUrl` → where the proxy listens (default `http://127.0.0.1:8080/v1`;
  put the proxy's real host here when it runs on another machine)
- `apiKey` → bearer token; `$ENV` interpolation or a literal. Secret keeps
  living in env.
- `mcpServers` (optional) → comma-separated MCP multiplexer selection
  (`X-MCP-Servers` header value; per-server tokens in `name:token` form pass
  through). Omit to stay model-provider only.

### Precedence

The global `~/.pi/agent/aiproxy.json` is the **default**; a project-level
`.pi/aiproxy.json` **overrides individual fields** (per-field merge, like
pi's mcp.json) — e.g. set just `apiKey` per project and inherit `baseUrl`.

`aiproxy.yaml` remains the **proxy server's own config** — the extension never
touches it. With no `aiproxy.json` the extension falls back to localhost + no
key.

## MCP tools

With `mcpServers` set, the extension connects to the proxy's `/mcp`
multiplexer and registers its tools as **native pi tools** (`searxng__search`,
`ctx7__docs`, ...) — no `mcp.json` needed. It does not touch the
pi-mcp-extension; if you also configure the multiplexer there you'll see the
tools twice (both work; pick one home).

The MCP SDK (`@modelcontextprotocol/sdk`) is a runtime dependency — installed
automatically by `pi install` (git package), but for loose-copy installs run
`npm i @modelcontextprotocol/sdk` next to the extension files.

Thinking levels come from the same catalog metadata (`thinkingLevelMap`), so
whatever pi knows about a model applies through the proxy. There is no local
override file: `models.json` is not read.

## Install

Install as a pi package from git (recommended):

```bash
pi install git:github.com/awyl/aiproxy@v0.4.0
```

Clones to `~/.pi/agent/git/github.com/awyl/aiproxy` and loads the extension.
Move to a newer release with `pi install git:github.com/awyl/aiproxy@vX.Y.Z`;
`pi update --extensions` reconciles the clone to the pinned ref.

Or try it without installing:

```bash
pi -e git:github.com/awyl/aiproxy
```

Or for a local checkout, add `./agent/pi/extensions/aiproxy` to your settings'
`packages` list (or `pi install ./agent/pi/extensions/aiproxy -l`).
Then `/models` → select `aiproxy/opencode-go/grok-4.6`.

## Notes

- Model metadata (contextWindow, maxTokens, reasoning, thinkingLevelMap, cost,
  compat) is resolved per `provider/modelId` from the **pi.dev catalog API**
  (`/api/models/providers/<kind>`), with pi's local model store
  (`models-store.json`) as fallback — so context sizes match what pi knows about
  each model instead of defaulting to 128k. The resolved catalog is cached and
  refreshed in the background.
- `Input` defaults to `["text"]` — flip to include `"image"` per model if tested.
- If the gateway is unreachable at startup, the extension warns and registers
  zero models — pi still starts.

## Multi-subscription gateways

With 2+ upstreams of a kind the catalog contains models from every one of them
(`opencode-go=alice/*`, `opencode-go=bob/*`, `openai-codex=alice/*`, …). This
extension registers all of them — nothing to configure, the ids carry the
subscription.

Who may use which is the proxy's business: with per-upstream `token_env`, a
request's bearer token both authenticates and locks it to that upstream's models
(prefixes the token does not own are rejected), so each user sets their own
`AIPROXY_TOKEN`. `openai-codex` subscriptions take no token — they are logged in
at the proxy's `/setup` page and are shared by everyone holding the proxy token.

## Typecheck

```bash
npm run typecheck
```