# pi-extension-aiproxy

Pi provider that fronts an [aiproxy](../..) gateway. One provider entry; models
are auto-discovered from the gateway at startup, split across the three wire
surfaces the proxy speaks.

Model ids stay prefixed (`opencode-go/mimo-v2.5`); the proxy routes by prefix
and strips it. `api` is derived per model from the proxy's `surface` field:

| proxy surface | pi api |
|---|---|
| `chat` | `openai-completions` |
| `messages` | `anthropic-messages` |
| `responses` | `openai-responses` |

## Setup

```bash
npm install          # pull pi-ai + pi-coding-agent for types
```

No extension env vars. Everything comes from the proxy's `aiproxy.yaml`
(next to where you run pi, or a symlink to it):

- `bind` → base URL (`http://host:port/v1`, default `127.0.0.1:8080`)
- `token` / `token_env` → the bearer token (secret still lives in env, the
  yaml only names it)
- `thinking` → default thinking level for all models (`off|low|medium|high|max`,
  default `high`)

The env file holds only secrets (API keys, tokens); the yaml holds all
configuration.

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

- Cost/contextWindow/maxTokens are conservative defaults (`0` cost, 128K /
  16K). Override per model in your `models.json` / provider `models` config —
  pi composes those above registered models.
- Thinking default is `high` for every model (`thinking:` in aiproxy.yaml);
  only that level is enabled in each model's `thinkingLevelMap`. Chat-surface
  models receive `reasoning_effort` — models that reject it need
  `thinking: off` or a per-model override.
- `Input` is limited to `["text"]` by default — Go multi-modal models exist
  (`mimo-v2-omni`); flipping to include `"image"` per model is safe if you
  tested it.
- If the gateway is unreachable at startup, the extension warns and registers
  zero models — pi still starts.

## Multi-subscription gateways

With per-upstream `token_env` (aiproxy multi-subscription), the catalog
contains models from every subscription (`go-alice/*`, `go-bob/*`). This
extension registers all of them; each user picks their subscription by model
prefix and their own `AIPROXY_TOKEN` identifies them (requests for prefixes
they don't own are rejected by the proxy). One extension, no per-user config
beyond each person's token env.

## Typecheck

```bash
npm run typecheck
```