# Changelog

All notable changes to aiproxy will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed
- **Docs corrected against shipped behavior** — the README claimed `openai-codex` had no model discovery and logged in by device code (discovery is implicit for that kind; browser paste-back is the default and device code the fallback), that discovery offered only `supported_in_api: true` entries (the picker applies that filter only outside ChatGPT mode — applying it is what emptied the catalog), that the login lived at `{config-dir}/openai-codex-oauth-state.json` (it is `{state-dir}/openai-codex-oauth-{name}.json`), that pi configures the extension in `models.json` (it is `aiproxy.json`; `models.json` is not read), that `docker-push.sh` takes a version argument (it reads `Cargo.toml`), and that multi-subscription ids look like `go-alice/<model>` (they are `kind=name`, e.g. `opencode-go=go-alice/grok-4.6`). The extension README said metadata came from `models-store.json` and that thinking defaults to `high` with a `models.json` override — metadata comes from the pi.dev catalog (store as fallback) and thinking levels ride that metadata. `surface:` (and its wire-name spelling `chat`) is now documented in the README, not only in the example config.
- **`/reload` is now `/models`** — the page lists every upstream and the models it offers (one collapsible section each, click to show/hide) instead of only per-upstream counts, and keeps the **Reload models** button. `GET /api/models` serves the same data as JSON without probing; `POST /api/reload` probes first and returns the same shape plus `reloaded_at_ms`. A provider whose probe failed keeps its entry, flagged, with the probe's error under it. `/reload` is gone (no redirect).

### Added
- **`client_version:` on an `openai-codex` upstream** — sets the `client_version` sent to the Codex model catalog, for when a model is missing because the backend hides models newer than the client. Resolution order: this setting → `AIPROXY_CODEX_CLIENT_VERSION` → built-in default (a recent Codex CLI version). Rejected on any other kind.
- **Multiple ChatGPT subscriptions** — several `openai-codex` upstreams now work end to end. Give each a `name:` and the ids become `openai-codex=<name>` (`openai-codex=alice/gpt-5.6-sol`), each with its own state file, login flow and discovery. `/setup` lists them and links to one page per subscription, every `/api/codex/*` route takes `?provider=<id>`, and the new `GET /api/codex/providers` returns each subscription with its `logged_in` flag and state path. Fixed while testing this: `POST /api/codex/complete` read the provider from the JSON body only, so **Finish login was broken as soon as a second subscription existed** — the page puts the id in the query string (`?provider=`), which the handler ignored.
- **Codex OAuth state file is named after the upstream** — `openai-codex-oauth-{name}.json`, where `{name}` is the upstream's `name:` (`openai-codex-oauth-alice.json`) or `openai-codex` when it is unnamed (`openai-codex-oauth-openai-codex.json`). Same convention as the old opencode-go cookie files (`opencode-cookie_{name}`), so the file says who logged in instead of repeating a provider id (`openai-codex=alice-oauth-state.json`) into a path. A configured name is sanitized to `[A-Za-z0-9._-]` before use as a path component.
- **Codex OAuth state lives in the runtime dir** — `openai-codex-oauth-{name}.json` (`0600`, see above), resolved as `AIPROXY_CODEX_STATE_DIR` → `AIPROXY_RUNTIME_DIR` → `/runtime` when it exists → the config file's directory. The old location (next to the config file) was fragile: the documented `docker run` mounts only the file (`-v ./aiproxy.yaml:/etc/aiproxy/aiproxy.yaml:ro`), leaving `/etc/aiproxy` an anonymous volume that is recreated empty with the container, so the login vanished on a container recreate. No migration from the old location: a state file next to the config file is ignored, so log in again after upgrading. The README docker example now mounts `aiproxy-runtime:/runtime`.
- **`/models` page + `POST /api/reload`** — re-run model discovery on demand and see what each upstream answered: one collapsible section per upstream with its model count and models, or the probe's error. Discovery otherwise runs once at startup and then every `model_refresh_secs` (default `0` = startup only), so an upstream that was unreachable at boot — an `openai-codex` upstream that was not logged in yet, an upstream down at boot — served an empty catalog until the proxy was restarted, with the reason only in the log. The endpoint returns `{reloaded_at_ms, total, providers: [{id, count, models, error}]}`; the registry gained `refresh_report()` (the per-provider outcome `refresh()` used to log and discard).
- **A successful `openai-codex` login re-runs discovery** — both paths (paste-back `POST /api/codex/complete` and the device-code poll loop) now spawn one discovery round after the tokens are stored, so the Codex catalog appears without a restart. The login response does not wait on upstream probes.
- **Credential location is visible** — `GET /api/codex/status` reports `state_path` and `state_file` (`exists`, `size`, `mtime_ms`, `mode`), the `/setup` page shows the same line, and startup logs `codex credentials path=… logged_in=…`. "The login vanished after a restart" is almost always the state file sitting in a directory that did not survive the restart (only the config file mounted, `--config` pointing at a copy): the path follows the config file's directory.
- **`openai-codex` upstream kind — ChatGPT Plus/Pro subscription** — run Codex models on a ChatGPT subscription instead of an API key. OAuth login from the new `/setup` page: **browser (PKCE) by default**, matching pi's own Codex login — the authorize URL uses `redirect_uri=http://localhost:1455/auth/callback` and **nothing listens there** — the browser lands on a connection error and the user pastes that URL back (`POST /api/codex/complete`), which also works for a remote or containerized proxy and never contends with the Codex CLI for 1455. A **device-code fallback** (`method=device`) asks for a code at `auth.openai.com/codex/device` and polls proxy-side, so the page can be closed mid-flow; `fresh=true` issues a new code without waiting out the old flow (`GET /api/codex/status`). Tokens live in the state dir as `openai-codex-oauth-{name}.json` (mode `0600`), refresh single-flight in the background (10-min tick, 60-min margin, 1→32-min backoff), persist a rotated refresh token before the access token is used, latch on `invalid_grant` (and adopt a state file rewritten by a later login), and retry once after a mid-request `401`. Requests go to `{base}/codex/responses` — `https://chatgpt.com/backend-api` by default — with the Codex request shape applied (force `store: false`/`stream: true`, default `instructions`, ensure `include: ["reasoning.encrypted_content"]`, default `text.verbosity: "low"`, drop `max_output_tokens`) and Codex identity headers (`Authorization: Bearer`, `chatgpt-account-id` from the JWT claim, `originator: pi`, `OpenAI-Beta: responses=experimental`, `session-id`/`x-client-request-id` from `prompt_cache_key`). Responses surface only: `/v1/chat/completions` and `/v1/messages` are rejected for these models; not-logged-in requests fail `502` with a hint to open `/setup`. **Model discovery** via `discover: true`: `GET {base_url}/codex/models?client_version=X.Y.Z` with the subscription's OAuth headers, offering only `visibility: list` + `supported_in_api: true` entries (hidden ones like `codex-auto-review` are skipped); `models:` becomes the fallback for a failed or impossible probe instead of the only source, and `AIPROXY_CODEX_CLIENT_VERSION` overrides the reported client version. No API-key fields (setting them is a config error). The pi extension knows the kind for pi.dev catalog lookup (no kind→provider aliasing — the kind name is already pi's provider id).

### Fixed
- **Dead code and duplicated test scaffolding removed** (net −193 lines, no behavior change). Dead: `CodexTokenManager::reload` (never called — relogin adoption is `adopt_relogin`), `codex_oauth::parse_models_response` (a test-only wrapper over `parse_models_catalog`), `OcGoUsageMeter.status` (parsed and never read), and the `#[allow(dead_code)]` on a test lock guard (the field is `_lock` now). Duplicated: `base64_url` was copied into four test modules while `codex_oauth::base64_url_encode` already did exactly that; `api::{openai,anthropic}::test_state` built the same fixture twice (`api::testutil::state_with`); `openai_codex`'s two rejected routes returned the same literal error twice (`responses_only()`); `setup.rs` hand-rolled eight oneshot request blocks (`post_empty`/`get_json`); `thinking.test.ts` repeated its terminal event (`done()`).

- **A static upstream advertised `surface: unknown` in `/v1/models`** — `StaticProvider::list_models` hardcoded `Unknown` while `surface_of` returned the configured surface, so a `models:`-only upstream with `surface: responses` told clients (pi reads this to pick the wire API) that its surface was unknown. The catalog entry now carries the configured surface and agrees with routing.
- **`surface:` now accepts the wire name** — `surface: chat` was rejected with ``unknown variant `chat`, expected one of `chatcompletions`, …`` even though `chat` is what `/v1/models` reports and what `endpoint_by_model` takes. Both spellings parse now.
- **`openai-codex` discovery sent its own version as `client_version`, so the backend returned an empty catalog** — the Codex models endpoint filters on client version (each model carries `minimal_client_version`). Measured live against a real subscription: no parameter → `400 Bad Request`; `client_version=0.4.0` (aiproxy's version) → `200 {"models":[]}`; `client_version=0.161.0` (Codex CLI) → `200` with 10 models; `999.0.0` → 10 models. The default is now a Codex CLI version constant (`CODEX_CLIENT_VERSION`), overridable with `AIPROXY_CODEX_CLIENT_VERSION`, and an empty catalog reports the version it used.
- **`openai-codex` discovery offered 0 models on a logged-in subscription** — the catalog parse required `supported_in_api: true`, but the Codex picker applies that filter only outside ChatGPT mode ("In ChatGPT mode, all models are visible", `codex-rs/protocol/src/openai_models.rs:1007`); the picker rule is `visibility == "list"` alone (`:967`). Since this upstream *is* the ChatGPT subscription, every ChatGPT-only model was dropped and `/models` showed `openai-codex: 0 models` with no error — a probe that answered successfully. Now `supported_in_api: false` models are offered (and counted as `chatgpt_only`), and a probe that genuinely offers nothing returns an error naming the counts (`upstream listed N models, 0 offerable (H hidden, S without a slug)`) instead of silent success.
- **`/setup` paste box no longer eats a pasted callback URL** — the 2s status poll rebuilt the card with `innerHTML = …`, which destroyed the input (and its focus) mid-paste, so "Finish login" posted an empty code. The poll now skips a status it already rendered, and the paste value lives outside the DOM so it survives any re-render; a failed paste-back shows the error and the next tick restores the login card. The page HTML moved to `src/setup_page.html` and now has regression tests that drive the real page in jsdom (`agent/pi/extensions/aiproxy/tests/setup_page.test.ts`).

### Removed
- **Codex loopback callback listener** — `/setup` browser login no longer binds `127.0.0.1:1455`. It bought one-click login only when the browser ran on the proxy's machine with 1455 free, and cost a whole listener lifecycle: a second start reported "Port 1455 is busy" while the port was held by the proxy's *own* previous flow, a pasted code left the port bound until the 15-minute timeout, and a shutdown could be lost before the listener's first poll. Paste-back is now the only path (the page says the browser will fail to load). Gone with it: `spawn_callback_listener`, the per-flow shutdown plumbing, `AppState.codex_callback_port`/`codex_listeners`, `CodexOptions.callback_port`, and the `AIPROXY_CODEX_CALLBACK_PORT` env hook — `redirect_uri` is always the fixed `http://localhost:1455/auth/callback`.
- **opencode-go cookie plumbing** — the `/setup` page, `POST /api/cookie`, `GET /api/cookie/status` and `GET /api/upstreams` are gone, along with the cookie read/write/list helpers, `AppState.cookie_path`, `AppState.upstream_names`, and the now-unused `config_path` parameter threaded through `server::build`/`run`. Nothing read `/runtime/opencode-cookie_*` after the usage fetcher moved to the Zen usage API, so the page was still advertising "Cookie saved successfully" for a write no code consumed. `/usage` is unchanged; `src/setup.rs` became `src/pages.rs`.

## [0.3.2] - 2026-10-02

### Changed
- **opencode-go usage uses the official Zen usage API** — `GET <base_url>/usage` (`https://opencode.ai/zen/go/v1/usage`), authenticated with the same inference API key the upstream already uses for model traffic (`Authorization: Bearer`). It is a supported route in opencode's inference proxy (`packages/console/app/src/lib/inference-proxy.ts`: `"GET /zen/go/v1/usage": "/go/v1/usage"`) and returns `{usage: {rolling, weekly, monthly}}` with percent-scale `percent` and RFC3339 `resetsAt` → 5h / 7d / 30d windows.

### Fixed
- **opencode-go usage no longer needs a browser session cookie** — the usage path drops the `/setup`-pasted opencode.ai cookie entirely. The original scrape (`GET /_server?id=<hash>` → `GET /workspace/{id}/go` + regex) was retired by opencode.ai's console rebuild, and its replacement (`GET /console/api/go/status`) is a console-UI route that rejects session cookies — so every fetch reported "opencode session expired" against a perfectly valid session. No cookie, no session, no expiry, no HTML, no micro-cent arithmetic. `FetcherConfig.cookie_path` is gone; opencode-go now takes `api_key` + `base_url` like every other billing fetcher.

## [0.3.1] - 2026-09-08

### Fixed
- **Usage sub-1% display** — opencode-go windows under 1% (e.g. 0.3%) no longer show 100x (30%). `normalize_percent` treated any 0..=1 value as a 0-1 fraction and scaled it; upstream values are always percent-scale, so it now clamps only — consistent with zai/minimax/header paths which never rescale.
- **MCP shared backend cache** — `/mcp` multiplexer now shares the per-server backend cache with `/mcp/<name>` routes: one backend session per server instead of spawning a fresh backend on every multiplexed request.

## [0.3.0] - 2026-09-05

### Added
- **Usage tracking** — `GET /v1/usage` returns per-provider rate-limit data captured from upstream response headers. Supports requests + tokens windows with used_percent, remaining, reset_secs. In-memory only (lost on restart). Extension shows the most-pressured provider on startup and every 60s: `[aiproxy] opencode-go 68% (reset 32m)`.

### Changed
- **Extension: pi.dev catalog fetch** — `loadCatalog()` now fetches model metadata from `pi.dev/api/models/providers/{kind}` (same API pi's built-in providers use) instead of reading pi's `models-store.json`. Results cached to `~/.pi/agent/aiproxy-models.json` (4h TTL). On startup: cache first, background refresh. No dependency on pi's refresh cycle for extension-registered providers.
- **Extension: provider attribution mirror** — `attributionHeaders()` replicates pi's `provider-attribution.js` via the `before_provider_headers` hook. Covers opencode-go session headers (`x-opencode-session` + `x-opencode-client`). Openrouter/nvidia attribution gated off (mirrors pi's telemetry gate).

## [0.2.9] - 2026-09-05

### Fixed
- **Faithful header relay** — client request headers now reach upstreams verbatim (`x-opencode-session`, `x-opencode-client`, custom `x-*`, user-agent, …). Previously the gateways built fresh requests with only auth + content-type, silently dropping `x-opencode-session` — breaking OpenCode's conversation→backend affinity (prompt-cache warmth; some Go backends 400 without it). Stripped only what must be: aiproxy-owned headers (`authorization`, `x-api-key`, `content-type`, `anthropic-version` — reqwest appends, never replaces) and hop-by-hop/transport-managed fields (`host`, `content-length`, `connection`, `keep-alive`, `transfer-encoding`, `upgrade`, `expect`, `accept-encoding`) per RFC 9110. Body relay unchanged: byte-for-byte, only the model id patched.

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
