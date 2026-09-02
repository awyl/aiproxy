# Changelog

All notable changes to aiproxy will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.2.8] - 2026-09-04

### Added
- **Thinking-stream cleanup for MiniMax M3** — two-layer fix for M3's duplicate thinking emission:
  - Layer 1 (proxy): `<think>` tags stripped from SSE response stream — all clients see clean text, zero overhead for non-M3 models (`api::strip_think_tags`, 4 tests).
  - Layer 2 (extension): `thinking.ts` merges consecutive pi-ai thinking blocks into one, suppressing prefix re-streams when M3 alternates `reasoning_content`/`reasoning` fields. Registered as `aiproxy-clean/minimax/MiniMax-M3` — opt in via `/model` (9 ThinkScanner tests + 4 cleanStream tests).

## [0.2.7] - 2026-09-04

### Fixed
- **Byte-faithful request relay** — chat/messages/responses handlers now relay the client's raw request body byte-for-byte, patching only the top-level `model` id (prefix strip). Previously axum parsed into `serde_json::Value` and re-serialized, alphabetizing object keys and reformatting numbers — breaking upstream passive prompt caching (e.g. MiniMax M3: 95%+ cache hit → 0% via proxy). All upstreams unaffected (same bytes in, same bytes out); the `api::body::replace_model_field` scanner is byte-level and tested independently.

## [0.2.6] - 2026-09-03

### Added
- **pi package install** — the repo is now a pi package (`pi install git:github.com/awyl/aiproxy@v0.2.6`); no more manual copying of the extension file.
- **Native MCP tools in the extension** — new `mcpServers` key in `aiproxy.json` (e.g. `"searxng,ctx7,grep"`) connects the extension to the proxy's `/mcp` multiplexer and registers its tools as native pi tools; no mcp.json needed. Split into `index.ts` (glue) + `provider.ts` + `mcp.ts`; 19 vitest tests.

### Changed
- **aiproxy extension** — own config file: `~/.pi/agent/aiproxy.json` (or project `.pi/aiproxy.json`) with `baseUrl`/`apiKey`. No longer reads `models.json`; `{ "providers": {} }` there is now enough. Falls back to `http://127.0.0.1:8080/v1` + no key when the file is absent.
- **aiproxy extension** — config precedence: global file is the default; project `.pi/aiproxy.json` overrides individual fields (per-field merge, like mcp.json).

## [0.2.5] - 2025-09-02

### Fixed
- **aiproxy extension** — two wire-routing bugs: (1) proxy-advertised surface is now fully authoritative over pi's catalog `api` (chat→openai-completions added to the map), so models whose native provider speaks a different wire than the proxy (e.g. `minimax/MiniMax-M3`: pi says anthropic-messages, proxy serves OpenAI chat) route correctly. (2) anthropic-messages models now get a per-model `baseUrl` with the trailing `/v1` stripped — pi's Anthropic client appends `/v1/messages` itself, so the provider base produced `/v1/v1/messages` → 404 (no body) on every messages-surface model.

### Added
- **MCP multiplexer** — single `/mcp` endpoint aggregates multiple MCP servers. Use `X-MCP-Servers` header to select servers and pass per-server tokens (e.g. `X-MCP-Servers: searxng:tok,ctx7`). Auth check on every `tools/list` and `tools/call` call, not just connection. Tools namespaced as `<server>__<tool>`.

### Changed
- **MCP per-server auth** — `token`/`token_env` on MCP servers now works with both individual `/mcp/<name>` endpoints and the new `/mcp` multiplexer.
- **aiproxy extension** sets `AIPROXY_TOKEN` env var from provider apiKey, so MCP and other tools can use it without repeating config.

## [0.2.4] - 2025-09-02

### Changed
- **Provider ID scheme** — `name` is now optional (defaults to `kind`). Single upstream of kind → ID = kind name (e.g. `opencode-go`). Multiple upstreams of kind → ID = kind=name (e.g. `opencode-go=alice`). Name uniqueness is per-kind, not global.
- **Extension catalog lookup** — strips subscription suffix (`=name`) before matching in pi's model store, so multi-subscription models get correct metadata.

### Added
- **Fail-fast validation** — 2+ upstreams of same kind with 2+ missing names → config error at startup.

## [0.2.3] - 2025-09-02

### Changed
- **Pi extension: model metadata from pi's model store** — extension reads `models-store.json` for all model attributes (contextWindow, maxTokens, reasoning, thinkingLevelMap, input, cost, headers, compat) instead of hardcoding. Models show correct context sizes (e.g. mimo-v2.5 1M, deepseek-v4-pro 1M) instead of 128k defaults.

## [0.2.2] - 2025-09-01

### Added
- **Per-MCP-server auth tokens** — each MCP server can now specify its own `token` or `token_env`, falling back to the global token when unset. Precedence: `token_env` > `token` > global.

### Changed
- **Embeddings: fastembed replaces llama-server** — in-process ONNX via fastembed crate; no more child processes. Models auto-download from HuggingFace and unload after idle timeout.
- **fastembed uses rustls** — removed openssl-sys dependency for simpler Docker builds.
- **Docker: Debian trixie for runtime** — Alpine blocked by ort-sys lacking musl prebuilts; Debian trixie (glibc 2.40) satisfies ONNX Runtime requirements.
- **Docker: BuildKit cache mounts** for cargo registry; `apt-get cargo` replaces rustup in builder to avoid layer shadowing.

### Fixed
- **fastembed observability** — model load `elapsed_ms`, embed request/completion debug logging.
- **Real integration tests** — download, load, and embed all 3 models (AllMiniLML6V2, NomicEmbedTextV15, BGESmallENV15) with concurrency-safe cache serialization.
- **BGE-small-en-v1.5 dimensions** — corrected expected from 512 to 384.

## [0.2.1] - 2025-08-28

### Added
- **Docker support** — Dockerfile (Debian trixie, llama.cpp build, Node.js for npx MCP servers), `.dockerignore`, `docker-push.sh` for versioned Docker Hub pushes.
- **DNS rebinding protection** — `mcp.allowed_hosts` config for streamable-HTTP MCP servers; bind host always added automatically.

### Fixed
- **MCP session mode** — disabled legacy session mode to fix "Session not found" errors.
- **MCP allowed_hosts** — derived from bind config automatically.

## [0.2.0] - 2025-08-25

### Added
- **Embeddings subsystem** — `embeddings-local` fake provider with per-model idle TTL, `/v1/embeddings` relay endpoint, model auto-download from HuggingFace.
- **MCP host proxying** — stdio and remote streamable-HTTP backends via `/mcp/<server>`.
- **Per-upstream multi-subscription routing** — `token_env` + `token=identity` for sharing upstream keys or isolating subscriptions.
- **Upstream kinds**: `minimax` (api.minimax.io), `zai` (GLM Coding Plan), `openrouter` (aggregator), `nvidia` (NIM cloud).
- **Model catalog** — `/v1/models` exposes per-model wire surface + display name, drives pi model selection.
- **Runtime surface discovery** — `surface_map_url` docs-table parse on discovery cadence; builtin snapshot as fallback.
- **`bind` config** — `host:port` replaces bare `port`, default `127.0.0.1:8080`.
- **`discover` opt-in** — no startup probing of keyless upstreams unless `discover: true`.

### Changed
- **Go surface-map TTL removed** — surfaces ride the discovery refresh cadence (one TTL).
- **Discovery opt-in** — opencode-go also requires `discover: true`.

## [0.1.0] - 2025-08-20

### Added
- Initial release: Provider trait, shared types, bearer token auth middleware.
- OpenAI-compatible gateway provider with streaming relay.
- Anthropic gateway provider with streaming relay.
- OpenCode Go provider with auto surface discovery.
- Model registry with parallel discovery.
- OpenAI API routes (`/chat/completions`, `/responses`).
- Anthropic API routes (`/messages`).
- Server assembly, CLI, healthz endpoint.
