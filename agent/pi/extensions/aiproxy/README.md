# pi-extension-aiproxy

Pi provider that fronts an [aiproxy](../..) gateway. One provider entry; models
are auto-discovered from the gateway at startup, split across the three wire
surfaces the proxy speaks.

Model ids stay prefixed (`opencode-go/mimo-v2.5`); the proxy routes by prefix
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
  "apiKey": "$AIPROXY_TOKEN"
}
```

- `baseUrl` → where the proxy listens (default `http://127.0.0.1:8080/v1`;
  put the proxy's real host here when it runs on another machine)
- `apiKey` → bearer token; `$ENV` interpolation or a literal. Secret keeps
  living in env.

`aiproxy.yaml` remains the **proxy server's own config** — the extension never
touches it. With no `aiproxy.json` the extension falls back to localhost + no
key.

Default thinking level is `high` for every model; override per model with
`models.json` `modelOverrides` (e.g. `{"providers":{"aiproxy": {"modelOverrides":
{"aiproxy/opencode-go/mimo-v2.5": {"thinkingLevelMap": {...}}}}}}`).

## Install

Persistent (adds to project settings, `.pi/settings.json`):

```bash
pi install ./agent/pi/extensions/aiproxy -l
pi list          # should show it under project packages
```

Ephemeral (this session only):

```bash
pi -e ./agent/pi/extensions/aiproxy
```

Or add `./agent/pi/extensions/aiproxy` to your settings' `packages` list
manually. Then `/models` → select `aiproxy/opencode-go/mimo-v2.5`.

## Notes

- Model metadata (contextWindow, maxTokens, reasoning, cost, etc.) is resolved
  from pi's live model store (`models-store.json`) so context sizes match what
  pi knows about each model (e.g. mimo-v2.5 gets 1M, not 128k defaults).
- `Input` defaults to `["text"]` — flip to include `"image"` per model if tested.
- If the gateway is unreachable at startup, the extension warns and registers
  zero models — pi still starts.

## Multi-subscription gateways

With per-upstream `token_env` (aiproxy multi-subscription), the catalog
contains models from every subscription (`opencode-go=alice/*`, `opencode-go=bob/*`).
This extension registers all of them; each user picks their subscription by model
prefix and their own `AIPROXY_TOKEN` identifies them (requests for prefixes
they don't own are rejected by the proxy). One extension, no per-user config
beyond each person's token env.

## Typecheck

```bash
npm run typecheck
```