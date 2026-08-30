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

Discovery is opt-in for every upstream, OpenCode Go included. Its catalog
fetch is keyless/public, so enabling it there is low-risk — that's the
standard setup.

| Config | Behavior |
|---|---|
| `models: [a, b]` | static catalog, never probed (recommended) |
| `discover: true` | probes `GET <base_url>/models` at startup + each refresh |
| neither | empty catalog — requests still route, agents see no models |

`discover: true` needs a reachable, keyed upstream — otherwise expect
per-refresh warnings in the log (e.g. `401` against `api.openai.com` or
`Transport` against an idle `localhost:11434` Ollama). With the default
`model_refresh_secs: 0` the fetch happens once at startup; set it to a
positive value (e.g. `1800`) for periodic re-discovery.

One cadence for everything: surfaces ride the same discovery pass. A go
`surface_map_url` table re-fetch happens on every `registry.refresh()`
(startup + each `model_refresh_secs` tick) — no independent TTL, no special
treatment. Failures keep the last-known map and log a warning.

## Local embeddings (fake provider)

`POST /v1/embeddings` relays to on-demand local llama-server children behind the
fake provider `embeddings-local` — no API key, no cloud:

```yaml
embeddings:
  llama_bin: llama-server   # llama.cpp binary (install: https://github.com/ggml-org/llama.cpp)
  idle_ttl_secs: 3600       # kill a child after 1h with no traffic (spawn on first request)
  models:
    - { id: nomic-embed-text-v1.5, model_file: /models/nomic-embed-text-v1.5.Q8_0.gguf }
    - { id: all-MiniLM-L6-v2,       model_file: /models/all-MiniLM-L6-v2.Q8_0.gguf }
    - { id: bge-small-en-v1.5,      model_file: /models/bge-small-en-v1.5.Q8_0.gguf }
```

Each model runs as its own child process on 127.0.0.1 (default ports 18081+,
override per model with `port:`). Only the requested model loads into RAM, so
the configured ones fit well under 1GB each (nomic Q8 ≈ 150MB weights,
MiniLM ≈ 30MB, bge-small ≈ 40MB — single-resident tops out ~450MB). GGUF
files: download from Hugging Face (e.g. `nomic-ai/nomic-embed-text-v1.5-GGUF`).

Call it OpenAI-style:

```bash
curl http://localhost:8080/v1/embeddings -H "Authorization: Bearer $AIPROXY_TOKEN" \
  -d '{"model": "embeddings-local/nomic-embed-text-v1.5", "input": "some text"}'
```

Catalog: `/v1/models` lists `embeddings-local/*` (surface `embedding`) — chat
routers reject these ids with a surface hint.

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

## Network

Binds `0.0.0.0` (all interfaces). No TLS in v1 — put a reverse proxy (e.g. Caddy) in front for anything beyond a trusted LAN.

## Multi-subscription routing

Give each upstream its own `token_env`; that token both authenticates and pins
the requester to that subscription:

```yaml
upstreams:
  - name: go-alice
    kind: opencode-go
    api_key_env: GO_ALICE_KEY
    token_env: GO_ALICE_TOKEN    # alice's bearer token
    discover: true
  - name: go-bob
    kind: opencode-go
    api_key_env: GO_BOB_KEY
    token_env: GO_BOB_TOKEN
    discover: true
```

Global auth accepts the global token **or** any subscription token. Requests
for a model prefix you don't own → `401`. Model ids are `go-alice/mimo-v2.5`,
`go-bob/mimo-v2.5` — pick a subscription by prefix. Proxy-level keys stay
per-upstream (`api_key_env`), so each subscription's quota is separate.

## Network & Security

- Binds the address in `bind` (default `127.0.0.1:8080`); use
  `bind: 0.0.0.0:8080` for LAN/containers.
- No TLS in v1 — put a reverse proxy (e.g. Caddy) in front for anything public.
- Upstream keys live in env vars only (`api_key_env`); never in the YAML.
- No token configured = auth disabled (the proxy warns at startup).

## Architecture

Single crate, small modules, module boundaries as traits:

```
config.rs       YAML schema + validation + env-key resolution (bind, token_env)
auth.rs         token middleware: global OR subscription tokens, constant-time
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