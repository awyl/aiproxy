# aiproxy

A single-host LLM proxy that centralizes upstream credentials and MCP servers.
Agents connect to one URL with one shared token and get every configured LLM
provider and MCP server — no per-agent API keys.

The flagship upstream is [OpenCode Go](https://opencode.ai/go): one $10/mo
subscription, model ids + wire surfaces auto-discovered (chat-completions,
messages, or Responses per model, resolved at runtime).

## What it exposes

| Route | Purpose |
|---|---|
| `GET /v1/models` | merged, prefixed model catalog |
| `POST /v1/chat/completions` | OpenAI chat-completions surface (SSE) |
| `POST /v1/responses` | OpenAI Responses surface (SSE) |
| `POST /v1/messages` | Anthropic messages surface (SSE) |
| `/mcp/<name>` | streamable-HTTP MCP endpoint per configured server |
| `GET /healthz` | liveness |

Model ids are always `{upstream name}/{model id}` — e.g. `opencode-go/kimi-k3`,
`opencode-go/grok-4.6`, `openai/gpt-4o`. The proxy strips the prefix, picks the
upstream, checks the model's wire surface, and relays upstream SSE bytes
verbatim.

## Discovery

Only OpenCode Go auto-discovers unconditionally (its catalog fetch is public,
keyless). Every other upstream is catalog-only unless you opt in:

| Config | Behavior |
|---|---|
| `models: [a, b]` | static catalog, never probed (recommended) |
| `discover: true` | probes `GET <base_url>/models` at startup + each refresh |
| neither | empty catalog — requests still route, agents see no models |

`discover: true` needs a reachable, keyed upstream — otherwise expect
per-refresh warnings in the log (e.g. `401` against `api.openai.com` or
`Transport` against an idle `localhost:11434` Ollama).

## Quickstart

```bash
cargo build --release
cp aiproxy.yaml.example aiproxy.yaml
cp .env.example .env
# edit .env: set AIPROXY_TOKEN + OPENCODE_GO_API_KEY
export $(grep -v '^#' .env | xargs)   # or use a dotenv loader
./target/release/aiproxy --config aiproxy.yaml
```

`aiproxy --help` for flags (`--config`, `--port`).

## OpenCode setup

Point opencode at the proxy with custom providers. Because Go serves different
models over three wire formats, use one provider entry per AI SDK package — the
proxy exposes all three surfaces on the same host:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "aiproxy-chat": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "aiproxy (chat)",
      "options": { "baseURL": "http://localhost:8080/v1", "apiKey": "<shared token>" },
      "models": { "opencode-go/kimi-k3": {}, "opencode-go/glm-5.3-flash": {} }
    },
    "aiproxy-messages": {
      "npm": "@ai-sdk/anthropic",
      "name": "aiproxy (messages)",
      "options": { "baseURL": "http://localhost:8080/v1", "apiKey": "<shared token>" },
      "models": { "opencode-go/minimax-m3": {}, "opencode-go/qwen3.7-plus": {} }
    },
    "aiproxy-responses": {
      "npm": "@ai-sdk/openai",
      "name": "aiproxy (responses)",
      "options": { "baseURL": "http://localhost:8080/v1", "apiKey": "<shared token>" },
      "models": { "opencode-go/grok-4.6": {}, "opencode-go/gpt-5.6-luna": {} }
    }
  }
}
```

Model ids must match the catalog served by `GET /v1/models` — run `curl
http://localhost:8080/v1/models` to see the current list, and check the
[Go docs](https://opencode.ai/docs/go) endpoints table for which surface a model
uses. Any OpenAI-compatible client (Ollama, LM Studio, arbitrary SDKs) can point
at `http://<host>:8080/v1` the same way.

## MCP

Each `mcp.servers` entry is available to clients at
`http://<host>:8080/mcp/<name>`, guarded by the shared token (`Authorization:
Bearer ...`). stdio entries spawn a child process; `url` entries connect to a
remote streamable-HTTP server.

## Security

- No TLS in v1 — put a reverse proxy (e.g. Caddy) in front for anything public.
- Upstream keys live in env vars only (`api_key_env`); never in the YAML.
- No token configured = auth disabled (the proxy warns at startup).

## Architecture

Single crate, small modules, module boundaries as traits:

```
config.rs       YAML schema + validation + env-key resolution
auth.rs         shared bearer-token middleware (constant-time compare)
provider.rs     Provider trait + Model/ModelSurface/Event — the core interface
providers/
  openai.rs     OpenAI-compatible gateway (kind "openai")
  anthropic.rs  Anthropic gateway (kind "anthropic")
  go.rs         OpenCode Go (kind "opencode-go", per-model surface routing)
discovery.rs    model registry: parallel refresh, prefixed catalog
api/
  mod.rs        SSE relay + per-surface error translation
  openai.rs     /v1/models, /v1/chat/completions, /v1/responses
  anthropic.rs  /v1/messages
mcp.rs          MCP host: stdio + remote backends, lazy reconnect
server.rs       assembly: providers -> registry -> routers -> listener
```

## Development

```bash
cargo test
cargo run -- --help
```