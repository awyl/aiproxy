# aiproxy Design

- Date: 2026-08-30
- Status: draft, pending user review
- Classification: architectural (new project)

## 1. Overview

aiproxy is a single-host LLM proxy that centralizes upstream provider credentials and
MCP servers. Agents (opencode first, more later) connect to one URL, send one shared
bearer token, and get access to every configured upstream LLM provider and MCP server.
Individual agents never configure API keys. The flagship upstream is
[OpenCode Go](https://opencode.ai/go) (`https://opencode.ai/zen/go/v1`), which serves
different models over different wire surfaces (chat-completions, messages, responses);
aiproxy resolves each model's surface from a built-in table and routes accordingly.

## 2. Goals / Non-Goals

### Goals

- Single setup: one config file + env vars on the proxy host; agents just point at the proxy URL.
- Expose three agent-facing surfaces: OpenAI chat-completions (`/v1/chat/completions`),
  OpenAI Responses (`/v1/responses`), Anthropic messages (`/v1/messages`).
- Model auto-discovery: proxy lists models from every upstream and serves the merged,
  provider-prefixed catalog over HTTP. No hand-maintained model list.
- Provider-prefixed routing: agent requests `opencode-go/kimi-k3` → proxy strips the prefix,
  forwards to the `opencode-go` upstream with the real key, on that model's native wire
  surface (known per model from the built-in Go surface table + config overrides).
- Centrally host MCP servers: proxy launches (stdio) or connects to (remote HTTP) each
  configured MCP server and re-exposes it as `/mcp/<name>` (streamable HTTP).
- Rust, single crate, small modules, module boundaries expressed as traits.

### Non-Goals (v1)

- No per-agent tokens, no usage accounting/audit. Single shared token.
- No semantic re-encoding of request/response payloads. Streaming is SSE byte relay.
- No config UI or admin API. Config file + env vars only.
- No model translation/aliasing beyond provider prefixing.
- No TLS termination (deploy behind a reverse proxy if needed). Plain HTTP.
- No horizontal scaling, no multi-node state.

## 3. Architecture

Single Rust crate `aiproxy` (edition 2024), one binary with internal modules:

```
src/
├── main.rs          — entry: parse CLI, load config, init tracing, run server
├── lib.rs           — crate root, re-exports for tests/integration
├── config.rs        — Config/Upstream/McpServer structs, YAML parse, env-key resolution
├── auth.rs          — shared bearer-token auth middleware
├── provider.rs      — Provider trait + Model/Event/UpstreamError types (the core interface)
├── providers/
│   ├── mod.rs       — registry: build/validate providers from UpstreamConfig
│   ├── openai.rs    — OpenAI-compatible gateway impl (kind `openai`, custom base URLs)
│   ├── anthropic.rs — Anthropic gateway impl (kind `anthropic`, custom base URLs)
│   └── go.rs        — OpenCode Go impl (kind `opencode-go`): per-model surface routing
├── discovery.rs     — model registry: prefixed catalog, refresh interval loop
├── api/
│   ├── mod.rs       — shared helpers: prefix strip, SSE relay, error JSON translation
│   ├── openai.rs    — OpenAI routes (/v1/models, /v1/chat/completions, /v1/responses)
│   └── anthropic.rs — Anthropic routes (/v1/models, /v1/messages)
├── mcp.rs           — MCP host: spawn/connect configured servers, expose per-name endpoints
└── server.rs        — axum Router assembly, AppState, healthz
```

Dependency direction: `server → api → provider;  server → mcp;  provider ← upstream impls;
discovery → provider`. No module depends on `server`.

### Key stack

- HTTP: `axum` 0.8 + `tokio`
- Upstream calls: `reqwest` (streaming)
- MCP: `rmcp` 3.1.4 (streamable HTTP server transport + stdio/HTTP clients)
- Serialization: `serde` / `serde_json` / `serde_yaml`
- Observability: `tracing` / `tracing-subscriber`
- CLI: `clap` (`--config`, `--port`)
- Errors: `thiserror` for library errors, `anyhow` optional at entry
- Futures: `futures` / `async-trait`

## 4. Core Interface

Everything plugs into one trait. New provider = new impl, no router changes.

```rust
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// Stable id used in config (`name:` field) and in model IDs ("opencode-go/").
    fn id(&self) -> &str;

    /// Which wire surface this provider serves a given model id on. Lets the
    /// API layer reject mismatched routes before forwarding (400 with a hint).
    fn surface_of(&self, model: &str) -> ModelSurface;

    /// Auto-discovery: fetch this upstream's model list.
    async fn list_models(&self) -> Result<Vec<Model>, ProviderError>;

    /// OpenAI chat-completions surface (request JSON passthrough).
    async fn chat_completions(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<Event, ProviderError>>, ProviderError>;

    /// Anthropic messages surface (request JSON passthrough).
    async fn messages(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<Event, ProviderError>>, ProviderError>;

    /// OpenAI Responses surface (request JSON passthrough).
    async fn responses(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<Event, ProviderError>>, ProviderError>;
}
```

Shared types (`provider.rs`):

- `enum ModelSurface { ChatCompletions, Messages, Responses, Unknown }` — the three
  agent-facing wire formats plus Unknown (dead/static models).
- `Model { id, display_name: Option<String>, created_at: Option<String>, surface: ModelSurface }` —
  catalog entry; `id` gets prefixed, `surface` records the model's native wire surface.
- `Event` — one unit of streamed content. v1: opaque raw bytes per upstream response
  chunk (SSE passthrough). The name exists so a semantic layer can be added later
  without changing the trait shape.
- `ProviderError { Http { status: u16, body: Value }, Transport(String) }` — upstream
  HTTP status + raw error JSON, or a transport failure; the API layer translates to
  the surface it serves.
- `RequestContext { model: String }` — the model id as sent upstream (prefix already
  stripped by the API layer); keeps the trait free of axum types.

### Upstream impls

- `OpenAiProvider` (kind `openai`): OpenAI-compatible gateway for custom base URLs
  (`https://api.openai.com/v1` default), `Authorization: Bearer <key>`. Serves the
  chat-completions surface only (`surface_of` → `ChatCompletions`); `messages` and
  `responses` return an unsupported-surface error.
- `AnthropicProvider` (kind `anthropic`): Anthropic gateway (`x-api-key` +
  `anthropic-version` headers), default base `https://api.anthropic.com/v1`. Serves
  messages only (`surface_of` → `Messages`).
- `OpencodeGoProvider` (kind `opencode-go`): OpenCode Go, default base
  `https://opencode.ai/zen/go/v1`. `surface_of` resolves per model, priority:
  (1) config `endpoint_by_model` overrides, (2) a runtime surface map parsed on
  refresh from `surface_map_url` (default `https://opencode.ai/docs/go` —
  the endpoints table, so new models are picked up automatically), (3) a built-in
  snapshot table for offline fallback, (4) `ChatCompletions` for unknown models.
  Auth: `Authorization: Bearer <key>` on `/chat/completions` and `/responses`;
  `x-api-key` + `anthropic-version` on `/messages` (Go's Anthropic surface).

### OpenCode Go surface table (built-in, from opencode.ai/docs/go, 2026-08-30)

| Surface | Models |
|---|---|
| `Responses` | `grok-4.6`, `gpt-5.6-luna`, `muse-spark-1.2-contributor` |
| `Messages` | `minimax-m3`, `minimax-m2.7`, `minimax-m2.5`, `qwen3.8-max`, `qwen3.8-flash`, `qwen3.7-max`, `qwen3.7-plus`, `qwen3.6-plus` |
| `ChatCompletions` (default) | everything else: `glm-*`, `kimi-*`, `deepseek-*`, `mimo-*`, `longcat-2.0`, `hy3`, `hy4-preview` |

## 5. Configuration

Default path `./aiproxy.yaml`, override via `--config` or `AIPROXY_CONFIG`.
`--port` / `AIPROXY_PORT` override the config port.

```yaml
port: 8080
token_env: AIPROXY_TOKEN        # or token: "xyz" (literal). Omit both = auth disabled.
model_refresh_secs: 1800        # discovery interval; 0 = fetch once at startup

upstreams:
  - name: opencode-go           # OpenCode Go — flagship upstream
    kind: opencode-go           # opencode-go | openai | anthropic
    api_key_env: OPENCODE_GO_API_KEY
    # endpoint_by_model:          # optional overrides for models outside the
    #   qwen3.9-x: messages       # surface table (default: chat/completions)
    # surface_map_url: https://opencode.ai/docs/go   # runtime surface source (default)

  - name: openai                # becomes id + model prefix "openai/..."
    kind: openai                # any OpenAI-compatible gateway (custom base_url)
    base_url: https://api.openai.com/v1   # optional, kind has a default
    api_key_env: OPENAI_API_KEY           # optional upstream: no key

  - name: no-list-endpoint
    kind: openai
    base_url: http://gateway.local/v1
    models: [gpt-4o, llama-3.3-70b]       # static list fallback; discovery skipped when present

  - name: anthropic
    kind: anthropic
    api_key_env: ANTHROPIC_API_KEY

  - name: local
    kind: openai
    base_url: http://localhost:11434/v1   # keyless local model server

mcp:
  servers:
    - name: filesystem
      command: npx                      # stdio transport
      args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
      env: {}                            # optional extra env for the child

    - name: github
      url: https://api.githubcopilot.com/mcp/   # remote HTTP transport
      api_key_env: GITHUB_TOKEN
```

Rules:

- `api_key_env` names an env var holding the upstream key. Literal keys never appear in
  the YAML. Missing env var = provider marked unhealthy, refused with a clear error.
- Model IDs exposed are always `{upstream name}/{model id}` from discovery, e.g.
  `opencode-go/kimi-k3`, `openai/gpt-4o`, `local/qwen3:14b`. OpenCode itself uses
  exactly this `opencode-go/<model-id>` shape when talking to Go directly, so agent
  config maps 1:1.
- YAML validation at startup: duplicate upstream names, empty configs, bad kinds →
  hard error with a precise message.

## 6. Wire Surface

All routes except `/healthz` require `Authorization: Bearer <token>` when a token is
configured. Token comparison is constant-time.

| Route | Method | Purpose |
|---|---|---|
| `/healthz` | GET | liveness; 200 with `ok` |
| `/v1/models` | GET | merged prefixed model catalog |
| `/v1/chat/completions` | POST | OpenAI chat completions, SSE stream |
| `/v1/responses` | POST | OpenAI Responses surface, SSE stream |
| `/v1/messages` | POST | Anthropic messages, SSE stream |
| `/mcp/<name>` | GET/POST | streamable HTTP MCP endpoint for that server |

`count_tokens` deliberately omitted: no agent in scope needs it, and the trait exposes no
method for it. Add later if a client requires it.

### Model routing

1. Extract `model` field from request; must contain `/` with a prefix matching a
   configured upstream name. `anthropic/claude-sonnet-4` → upstream `anthropic`.
2. Strip the `{prefix}/` before forwarding; upstream sees its native id.
3. Unknown/unprefixed model → `400` listing valid prefixes and the requested id.
4. Upstream name maps to its `Provider` impl via the registry. No globbing, no
   fallback routing — explicit prefix is the whole rule.
5. **Surface check:** the route the agent hit must match `surface_of(model)`.
   `/v1/chat/completions` requires `ChatCompletions`, `/v1/responses` requires
   `Responses`, `/v1/messages` requires `Messages`. Mismatch → `400` naming the
   model's correct surface. (On Go: `opencode-go/grok-4.6` via `/v1/chat/completions`
   is rejected with "use /v1/responses".)

### Streaming

- Upstream responses relayed byte-for-byte as SSE to the client. No promise queues,
  no buffering beyond the transport chunk. Tool calls, reasoning fields, usage blocks
  pass through untouched (fidelity over normalization).
- All three surfaces return `data:`-framed SSE from their upstreams, which is what the
  OpenAI and Anthropic protocols use.

### Errors

- Upstream non-2xx with JSON body → wrapped in the response schema of the API the
  client hit (OpenAI `{error: {message, type, code}}` or Anthropic `{type: "error",
  error: {type, message}}`), status preserved.
- Proxy-local errors (bad model prefix, auth failure, upstream unavailable) formatted
  the same way per surface.

## 7. Model Discovery

- On startup and every `model_refresh_secs`, poll `Provider::list_models` for all
  upstreams in parallel with a per-upstream timeout (10s default).
- Merge into the catalog with prefixed ids; drop upstreams that fail on refresh
  (keep last-known catalog or empty) and log.
- `/v1/models` serves the merged list; ids always prefixed so agent config never
  changes when upstreams are added/removed.
- Anthropic upstream discovery uses `GET /v1/models`; OpenAI-compatible and
  opencode-go use `GET /v1/models` too (Go's catalog is public: standard OpenAI
  `{data:[{id,object,created,owned_by}]}` shape — no per-model surface field, so
  surfaces come from the runtime `surface_map_url` fetch with builtin-table fallback).
- Upstreams with a static `models:` list in config skip discovery entirely and serve
  that list prefixed (for gateways without a listing endpoint); their surface is
  `Unknown`, so they serve catalog only, never streams.

## 8. MCP Hosting

- Each `mcp.servers` entry becomes a backend:
  - `command`/`args`/`env` → stdio backend (spawn child, keep alive, restart on exit).
  - `url` → remote streamable-HTTP backend, connecting with the configured key.
- One axum service per server mounted at `/mcp/<name>`, running `rmcp`'s streamable
  HTTP server transport (single path answering both GET SSE and POST JSON/SSE,
  session cookies per MCP spec 2025-06-18).
- Same shared bearer token guards all `/mcp/*` routes.
- Backend spawn/connect failures surface as an explicit startup error for that server.
- v1 scope: no tool namespace prefixing, no aggregation endpoint (per earlier
  decision: one endpoint per server).

## 9. Security Model

- Single shared bearer token, constant-time compare. No token = auth disabled
  (explicit operator choice, warn in logs).
- Upstream keys live in env vars only, referenced by `api_key_env` name.
- Proxy never logs request/response bodies, only model ids, upstream names, status,
  and durations.
- No TLS in v1: documented recommendation is a reverse proxy (e.g. Caddy) in front.

## 10. Testing

- Unit: config parsing (valid/invalid/dup names, env resolution), model prefix
  strip/route logic, error translation per surface, constant-time token check.
- Provider-layer: `MockProvider` implementing the trait in tests; router tests drive
  the full request → catalog → relay path against the mock.
- Upstream-contract tests: a tiny in-crate axum mock server speaking OpenAI, Anthropic,
  and OpenCode Go shapes; assert outbound header/key/prefix-strip, per-surface routing
  (Go: same model id forwarded to `/chat/completions`, `/messages`, or `/responses`
  depending on surface), and SSE relay integrity (including mid-stream chunk breaks).
- Surface-table tests: built-in Go table resolves every documented model id to the
  right surface; `endpoint_by_model` config overrides it; unknown model defaults to
  `ChatCompletions`.
- MCP: spawn the stdio backend against `@modelcontextprotocol/server-everything`
  (or a minimal fixture server), connect a real rmcp client, run list-tools + one
  tool call through `/mcp/<name>`.
- Integration: `WiremockUpstream` + one end-to-end test: start server, POST chat
  completion, assert SSE frames and error-path frames.

## 11. Deliverable Shape

- Cargo project at repo root, `aiproxy.yaml` example committed, `.gitignore` for
  `.env`, README with quickstart (run proxy, configure opencode with
  `http://localhost:8080/v1` + shared token + `opencode-go/<model>` ids).
- `cargo build` + `cargo test` green.