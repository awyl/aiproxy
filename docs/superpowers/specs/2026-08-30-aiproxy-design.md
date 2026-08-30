# aiproxy Design

- Date: 2026-08-30
- Status: draft, pending user review
- Classification: architectural (new project)

## 1. Overview

aiproxy is a single-host LLM proxy that centralizes upstream provider credentials and
MCP servers. Agents (opencode-go first, more later) connect to one URL, send one shared
bearer token, and get access to every configured upstream LLM provider and MCP server.
Individual agents never configure API keys.

## 2. Goals / Non-Goals

### Goals

- Single setup: one config file + env vars on the proxy host; agents just point at the proxy URL.
- Expose both OpenAI-compatible (`/v1/*`) and Anthropic-compatible (`/v1/messages`) APIs.
- Model auto-discovery: proxy lists models from every upstream and serves the merged,
  provider-prefixed catalog over HTTP. No hand-maintained model list.
- Provider-prefixed routing: agent requests `openai/gpt-4o` → proxy strips the prefix,
  forwards to the matching upstream with the real key.
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
│   ├── openai.rs    — OpenAI-compatible upstream impl
│   └── anthropic.rs — Anthropic upstream impl
├── discovery.rs     — model registry: prefixed catalog, refresh interval loop
├── api/
│   ├── mod.rs       — shared helpers: prefix strip, SSE relay, error JSON translation
│   ├── openai.rs    — OpenAI-compatible routes (/v1/models, /v1/chat/completions)
│   └── anthropic.rs — Anthropic-compatible routes (/v1/models, /v1/messages)
├── mcp.rs           — MCP host: spawn/connect configured servers, expose per-name endpoints
└── server.rs        — axum Router assembly, AppState, healthz
```

Dependency direction: `server → api → provider;  server → mcp;  provider ← upstream impls;
discovery → provider`. No module depends on `server`.

### Key stack

- HTTP: `axum` 0.8 + `tokio`
- Upstream calls: `reqwest` (streaming)
- MCP: `rmcp` 0.x (streamable HTTP server transport + stdio/HTTP clients)
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
    /// Stable id used in config (`name:` field) and in model IDs ("openai/").
    fn id(&self) -> &str;

    /// Auto-discovery: fetch this upstream's model list.
    async fn list_models(&self) -> Result<Vec<Model>, ProviderError>;

    /// OpenAI-compatible chat completions (request JSON passthrough).
    async fn chat_completions(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<Event, ProviderError>>, ProviderError>;

    /// Anthropic messages (request JSON passthrough).
    async fn messages(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<Event, ProviderError>>, ProviderError>;
}
```

Shared types (`provider.rs`):

- `Model { id, display_name: Option<String>, created_at: Option<String> }` — minimal
  catalog entry; `id` is what gets prefixed.
- `Event` — one unit of streamed content. v1: opaque raw bytes per upstream response
  chunk (SSE passthrough). The name exists so a semantic layer can be added later
  without changing the trait shape.
- `ProviderError { status: Option<u16>, body: Value }` — carries the upstream HTTP
  status and raw error JSON so the API layer can translate to the wire format it serves.
- `RequestContext` — upstream key, target model id (prefixed as received), per-request
  deadline; keeps the trait free of axum types.

### Upstream impls

- `OpenAiProvider` (kind `openai`): speaks OpenAI wire format to `base_url`
  (`https://api.openai.com/v1` default), `Authorization: Bearer <key>`. Both
  `chat_completions` and `messages` map to the same upstream endpoint shape
  (`/chat/completions` vs `/messages` on the provider's base).
- `AnthropicProvider` (kind `anthropic`): speaks Anthropic wire format
  (`x-api-key` + `anthropic-version` headers), default base
  `https://api.anthropic.com/v1`.

## 5. Configuration

Default path `./aiproxy.yaml`, override via `--config` or `AIPROXY_CONFIG`.
`--port` / `AIPROXY_PORT` override the config port.

```yaml
port: 8080
token_env: AIPROXY_TOKEN        # or token: "xyz" (literal). Omit both = auth disabled.
model_refresh_secs: 1800        # discovery interval; 0 = fetch once at startup

upstreams:
  - name: openai                # becomes id + model prefix "openai/..."
    kind: openai                # openai | anthropic
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
  `openai/gpt-4o`, `local/qwen3:14b`.
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

### Streaming

- Upstream responses relayed byte-for-byte as SSE to the client. No promise queues,
  no buffering beyond the transport chunk. Tool calls, reasoning fields, usage blocks
  pass through untouched (fidelity over normalization).
- Both `chat_completions` and `messages` return `data:`-framed SSE from the respective
  upstreams, which is what both agent-facing protocols use.

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
- Anthropic upstream discovery uses `GET /v1/models`; OpenAI-compatible uses
  `GET /v1/models` too. Upstreams with a static `models:` list in config skip
  discovery entirely and serve that list prefixed (for gateways without a listing
  endpoint).

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
- Upstream-contract tests: a tiny in-crate axum mock server speaking OpenAI and
  Anthropic shapes; assert outbound header/key/prefix-strip and SSE relay integrity
  (including mid-stream chunk breaks).
- MCP: spawn the stdio backend against `@modelcontextprotocol/server-everything`
  (or a minimal fixture server), connect a real rmcp client, run list-tools + one
  tool call through `/mcp/<name>`.
- Integration: `WiremockUpstream` + one end-to-end test: start server, POST chat
  completion, assert SSE frames and error-path frames.

## 11. Deliverable Shape

- Cargo project at repo root, `aiproxy.yaml` example committed, `.gitignore` for
  `.env`, README with quickstart (run proxy, configure opencode-go custom provider
  with `http://localhost:8080/v1` + shared token + prefixed model ids).
- `cargo build` + `cargo test` green.