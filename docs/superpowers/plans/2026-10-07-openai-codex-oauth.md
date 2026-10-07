# OpenAI Codex (ChatGPT subscription) implementation plan

Spec: `docs/superpowers/specs/2026-10-07-openai-codex-oauth-design.md`
Branch: `feat/openai-codex` · TDD: failing test → green → commit per task. No pushes.

Reference (replicate exactly): `pi-ai/dist/auth/oauth/openai-codex.js`,
`pi-ai/dist/auth/oauth/device-code.js`, `pi-ai/dist/api/openai-codex-responses.js`.

## Task 1 — Constants + pure helpers (`src/codex_oauth.rs`, new)

`pub mod codex_oauth;` in `lib.rs`.
- Constants: `CLIENT_ID`, `AUTH_BASE_URL`, `TOKEN_URL`, `DEVICE_USER_CODE_URL`,
  `DEVICE_TOKEN_URL`, `DEVICE_VERIFICATION_URI`, `DEVICE_REDIRECT_URI`, `SCOPE`,
  `JWT_CLAIM_PATH`, `DEFAULT_CODEX_BASE_URL`, `DEVICE_CODE_TIMEOUT_SECS` (900),
  `MIN_POLL_INTERVAL_MS` (1000), `DEFAULT_POLL_INTERVAL_SECS` (5),
  `SLOW_DOWN_INCREMENT_MS` (5000).
- `account_id_from_token(&str) -> Option<String>` — base64url JWT payload → claim path.
- `codex_responses_url(base: &str) -> String` — join rule from the spec.
- `pi_user_agent() -> String` — `pi (<os> <release>; <arch>)` (`/proc/sys/kernel/osrelease`,
  fallback `pi (<os>)`).
- `transform_codex_body(Bytes) -> Result<Bytes, CodexError>` — spec §Request path.
- `codex_headers(access, account_id, session_id: Option<&str>) -> HeaderMap` +
  `session_id_from_body(&Value) -> Option<String>`.

Tests: JWT vectors (valid, 2-part, bad base64, missing claim), URL join matrix,
transform cases, `0600`-free pure assertions.

## Task 2 — Device-code + token exchange client

`start_device_flow(&Client, base) -> DeviceFlow {device_auth_id, user_code,
interval_secs}`, `poll_device_flow(&Client, &DeviceFlow) -> PollStatus`
(`Pending | SlowDown {interval_secs} | Complete {authorization_code, code_verifier} |
Failed(String)`), `exchange_code(...) -> Tokens`, `refresh_tokens(...) -> Tokens`.

Tests against a hand-rolled axum mock auth server: request shapes (JSON start/poll,
form exchange/refresh), numeric-string interval, 404 start, pending via 403/404 and via
`deviceauth_authorization_pending`, `slow_down`, complete, exchange missing-field error,
refresh omitting `refresh_token`.

## Task 3 — `CodexTokenManager`

State file next to the config path, `0600`. `CodexTokenManager::new(state_path, token_url)`:
`access()`, `force_refresh()`, `account_id()`, `reload()`, `background_tick()`,
`poll_until_complete()` (used by `/setup`).

Tests: rotation persisted before return, single-flight (two concurrent `access()` with an
expired token → one HTTP call, via a counting mock), invalid_grant latch, relogin
adoption, margin, backoff, owner-only perms.

## Task 4 — `OpenAiCodexProvider` (`src/providers/openai_codex.rs`, new)

`surface_of` → `Responses` for every model; `list_models` → static config list;
`chat_completions`/`messages` → `ProviderError::Http{400}` hint; `responses()` → access
token, headers, transform, POST, 401 → `force_refresh()` + retry once, `LoggedOut` →
502 with `/setup` hint, else relay SSE bytes verbatim (shared `stream_sse` helper in
`provider.rs` if a second user appears).

Tests: mock upstream asserting the full wire header set + transformed body; 401 retry
count; logged-out 502; invalid JSON 400; catalog surface.

## Task 5 — Config + wiring

- `UpstreamKind::OpenAiCodex` (`as_str` = `openai-codex`, default base URL).
- Validation: `api_key_env`/`token_env` rejected on this kind; `discover: true` warns.
- `server.rs`/`main.rs`: thread `config_path` through `build`/`run`/`run_with_port`.
- `providers/mod.rs`: build arm + `create_codex_managers(cfg, config_path)` keyed by
  provider id, honouring `AIPROXY_CODEX_TOKEN_URL`.
- `server.rs`: spawn background refresh per manager; put managers + pending-flow map in
  `AppState`.

Tests: config matrix, manager path creation, provider build arm, existing tests updated
for the new signatures.

## Task 6 — `/setup` page + `/api/codex/*`

`src/setup.rs` (new): `setup_page()`, `codex_start` (browser PKCE by default, `method=device`
fallback, `fresh=true` for a new flow), `codex_complete` (paste-back), `codex_status`;
loopback callback listener on 1455; per-flow `flow_id` so stale poll/timeout loops cannot
clobber a newer flow. Routes registered outside the auth layer.

Tests: browser start binds loopback and returns the authorize URL, real callback request
finishes the login, state mismatch rejected, paste-back (URL / `code#state` / bare code),
busy-port fallback, unknown method 400, device start + proxy-side completion, idempotent
start, `fresh` issues a new code.

## Task 7 — e2e + extension

- `tests/codex_e2e.rs`: mock auth server + mock Codex upstream + real `aiproxy::server`
  build; device login via `/api/codex/*`; then `/v1/responses` with `codex/<model>`
  returns the upstream SSE bytes verbatim; unauthenticated request rejected; api-key
  upstream path unchanged.
- `agent/pi/extensions/aiproxy/provider.ts`: add `"openai-codex"` to `UPSTREAM_KINDS`;
  extend `provider.test.ts` for a `responses`-surface model.

## Task 8 — Docs, version bump, live smoke

README section + example config + CHANGELOG + version bump (Cargo.toml, both
package.json), `cargo clippy --all-targets`, `cargo fmt`, extension tests.
Then the live smoke: real login through `/setup` (browser first, device fallback), one
streamed `/v1/responses` call, routing proven by the Codex backend's own error/auth
behaviour relayed through the proxy. **Skipped by the user's choice** — the real authorize
URL and loopback listener were produced, but no real token exchange or upstream call was
made; live routing stays unverified.

## Completion criteria

`cargo test` green, clippy clean, fmt clean, extension tests green, docs synced, live
smoke recorded, one commit per task on `feat/openai-codex`.

## Revision — loopback callback listener removed

Task 6 shipped the browser flow with a loopback callback listener on 1455. It was removed
after three defects surfaced in testing (all real, all in the listener lifecycle): a second
`start` reported "Port 1455 is busy" while the port was held by the proxy's *own* previous
flow; a successful paste-back held the port until the 15-minute timeout, blocking the Codex
CLI's own login; and a shutdown notified before the listener's first poll was lost
(`notify_waiters` has no permit). Fixing all three left the machinery buying one-click
login only for a browser on the proxy's machine with 1455 free — the paste-back path covers
every deployment, including remote and containerized ones.

Deleted: `spawn_callback_listener` + the `GET /auth/callback` handler, `bind_callback`,
`release_callback_listener`, `CodexListeners`, `AppState.codex_callback_port`,
`CodexOptions.callback_port`, `AIPROXY_CODEX_CALLBACK_PORT`, the `callback_listening` flow
field, and the tests that exercised them. `build_browser_flow()` now takes no port and
always builds `http://localhost:1455/auth/callback`. `/setup` always shows the paste box and
says the browser will fail to load the callback page. `tests/codex_e2e.rs` finishes the
browser login through `POST /api/codex/complete` instead of a real callback request.
