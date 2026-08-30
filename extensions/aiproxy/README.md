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

Environment:

- `AIPROXY_URL` — gateway base URL, e.g. `http://host.containers.internal:8080/v1` (default `http://localhost:8080/v1`; trailing `/` stripped)
- `AIPROXY_TOKEN` — shared gateway token (default: unset → gateway likely auth-disabled)

## Enable

```bash
pi -e ./extensions/aiproxy
```

or add to your pi settings' extension list. Restart not required if added
via `/extensions` live UI.

Then `/models` → select `aiproxy/opencode-go/mimo-v2.5` (the full prefixed id).

## Notes

- Cost/contextWindow/maxTokens are conservative defaults (`0` cost, 128K /
  16K). Override per model in your `models.json` / provider `models` config —
  pi composes those above registered models.
- `reasoning` is auto-enabled for known extended-thinking ids
  (`qwen3.x*`, `glm-4.x`, `deepseek-r1` patterns); everything else defaults to
  non-reasoning. Override via models.json.
- `Input` is limited to `["text"]` by default — Go multi-modal models exist
  (`mimo-v2-omni`); flipping to include `"image"` per model is safe if you
  tested it.
- If the gateway is unreachable at startup, the extension warns and registers
  zero models — pi still starts.

## Typecheck

```bash
npm run typecheck
```