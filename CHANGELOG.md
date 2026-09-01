# Changelog

All notable changes to aiproxy will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
