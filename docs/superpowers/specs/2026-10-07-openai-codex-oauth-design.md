# OpenAI Codex (ChatGPT subscription) support for aiproxy

Date: 2026-10-07
Status: approved design
Branch: `feat/openai-codex` (off `main` — standalone, no dependency on `feat/anthropic-oauth`)

## Goal

Let aiproxy front a **ChatGPT Plus/Pro subscription** (Codex OAuth, not an API key) so
every agent pointing at aiproxy can run Codex models. The proxy owns login, token
refresh, and the Codex request shape; clients speak ordinary OpenAI Responses.

Reference implementation (read byte-for-byte, replicated exactly — no guessing):
`@earendil-works/pi-ai/dist/auth/oauth/openai-codex.js`,
`.../auth/oauth/device-code.js`, `.../api/openai-codex-responses.js`.

## Decisions (user-approved)

1. **Standalone off `main`.** No reuse of `feat/anthropic-oauth` (`src/oauth.rs`,
   `/setup` page). Codex gets its own module, manager, state file, and page.
2. **Login = device code via the `/setup` page.** The proxy runs the poll loop, so the
   page can be closed and the flow still completes; works headless/remote (no
   `localhost:1455` callback).
3. **Responses surface only.** The Codex backend is a Responses endpoint; chat/messages
   requests for Codex models are rejected before forwarding.
4. **Plan usage/limits deferred.** Not in this task.
5. **Kind name `openai-codex`** — identical to pi's provider id, pi.dev catalog provider
   name, and `~/.pi/agent/models-store.json` key, so the pi extension needs no
   kind→catalog aliasing (one string added to `UPSTREAM_KINDS`).

## Config & state

```yaml
upstreams:
  - kind: openai-codex
    name: chatgpt-max          # optional; provider id stays "openai-codex" when single
    models:                    # required — no discovery endpoint exists
      - gpt-5.6-sol
      - gpt-5.6-terra
      - gpt-5.6-luna
      - gpt-5.5
      - gpt-5.3-codex-spark
```

- Default base URL: `https://chatgpt.com/backend-api`; full request URL resolved as
  `.../codex/responses` (join rule below).
- Auth is intrinsic to the kind: `api_key_env` / `token_env` on an `openai-codex`
  upstream → config error at startup. `oauth: true` is not a flag here.
- `discover: true` → ignored with a warning (no model-discovery endpoint).
- Static `models:` entries carry surface `responses`.
- Tokens live in `{config-dir}/openai-codex-oauth-state.json` (respects `--config`),
  perms `0600`: `{"access": "...", "refresh": "...", "expires_at": <ms>}`.
- Missing state file → startup fine; first request fails 502 with
  `OpenAI Codex not logged in — open /setup`.
- Test hook: `AIPROXY_CODEX_AUTH_BASE_URL` and `AIPROXY_CODEX_TOKEN_URL` override the
  auth base / token endpoint (mock servers in tests).

## Login (device code)

Constants: `CLIENT_ID = app_EMoamEEZ73f0CkXaXp7hrann`,
`AUTH_BASE_URL = https://auth.openai.com`,
`DEVICE_USER_CODE_URL = {AUTH_BASE_URL}/api/accounts/deviceauth/usercode`,
`DEVICE_TOKEN_URL = {AUTH_BASE_URL}/api/accounts/deviceauth/token`,
`DEVICE_VERIFICATION_URI = {AUTH_BASE_URL}/codex/device`,
`DEVICE_REDIRECT_URI = {AUTH_BASE_URL}/deviceauth/callback`,
`TOKEN_URL = {AUTH_BASE_URL}/oauth/token`,
`SCOPE = "openid profile email offline_access"`,
`JWT_CLAIM_PATH = "https://api.openai.com/auth"`,
timeout `15 min`, minimum interval `1 s`, default interval `5 s` (RFC 8628), slow_down
`+5 s` (or the server-reported interval when present).

1. Start: `POST DEVICE_USER_CODE_URL`, JSON `{"client_id": CLIENT_ID}` →
   `{device_auth_id, user_code, interval}` (`interval` may be a numeric string). `404`
   → "device code login is not enabled for this server".
2. Page shows `user_code` + link to `DEVICE_VERIFICATION_URI`; proxy polls in the
   background.
3. Poll: `POST DEVICE_TOKEN_URL`, JSON `{device_auth_id, user_code}`. `200` →
   `{authorization_code, code_verifier}` (both required). `403`/`404` → pending; error
   code `deviceauth_authorization_pending` → pending; `slow_down` → widen interval;
   anything else → failed.
4. Exchange: `POST TOKEN_URL`, `Content-Type: application/x-www-form-urlencoded`,
   body `grant_type=authorization_code, client_id, code, code_verifier,
   redirect_uri=DEVICE_REDIRECT_URI` → `{access_token, refresh_token, expires_in}`
   (all three required; `expires_in` must be a number).
5. Write state file (`0600`), mark the flow complete.

## Token manager (`src/codex_oauth.rs`)

`CodexTokenManager { state_path, http: Client, inner: tokio::sync::Mutex<ManagerInner> }`

- `access() -> Result<String>`: memory fast path; else load file; if expired or inside
  the margin, single-flight refresh (re-check under the mutex) and return.
- **Rotation safety**: refresh response may omit `refresh_token` → keep the old one.
  Persist the new refresh token to disk **before** returning the access token.
- **Refresh**: `POST TOKEN_URL`, form-urlencoded
  `grant_type=refresh_token, refresh_token, client_id` (no scope).
- Background refresh: task in `server.rs`, wakes every 10 min (+ ≤60 s jitter),
  refreshes when `expires_at - now < 60 min`. Failures back off 1 min → 32 min.
- `invalid_grant` (400/401 on refresh): latch logged-out (clear memory, keep file), all
  later requests fail 502 with the login hint. A state file rewritten by a later login
  is adopted (`reload()` / adopt-on-check) so logging in again while running works.
- `account_id()`: extracted from the access-token JWT claim
  `https://api.openai.com/auth` → `chatgpt_account_id`; missing → logged-out error.
- Non-Codex upstreams never touch this module.

## Request path (`/v1/responses`)

URL: strip trailing slashes from base; if it ends with `/codex/responses` use as-is; if
it ends with `/codex` append `/responses`; else append `/codex/responses`.

Headers (owned by the provider; the client's own copies are stripped like every other
upstream):

| Header | Value |
| --- | --- |
| `Authorization` | `Bearer <access token>` |
| `chatgpt-account-id` | JWT claim value |
| `originator` | `pi` |
| `User-Agent` | `pi (<os> <release>; <arch>)` — mirrors pi's `getPiUserAgent()`; `/proc/sys/kernel/osrelease` on Linux, `pi (<os>)` fallback |
| `OpenAI-Beta` | `responses=experimental` |
| `accept` | `text/event-stream` |
| `content-type` | `application/json` |
| `session-id`, `x-client-request-id` | client's `prompt_cache_key` when present |

Body transform (parse `Bytes` → `Value`, keep every other field, re-serialize):

1. Force `store: false`, `stream: true`.
2. `instructions`: default `"You are a helpful assistant."` when absent or empty.
3. `include`: ensure `"reasoning.encrypted_content"` is present (append if missing).
4. `text`: default `{"verbosity": "low"}` when absent.
5. Default `tool_choice: "auto"`, `parallel_tool_calls: true`, `prompt_cache_key` from
   the session id when absent.
6. **Drop `max_output_tokens`** (the reference client never sends it).
7. Invalid JSON → 400 `invalid_request_error` (client bug).

Response: non-2xx → typed `ProviderError::Http`; 2xx → SSE bytes relayed verbatim.
`401` from upstream → `force_refresh()` then retry **once**. `LoggedOut` → 502 with
`OpenAI Codex not logged in — open /setup`.

Non-goals: `Content-Encoding: zstd` body compression (the backend accepts plain JSON —
the reference falls back to it), the WebSocket transport, plan-usage fetcher,
chat→responses translation, browser-callback login, multi-account.

## `/setup` page (unauthenticated, like `/usage`)

- `GET /setup` — HTML: per-`openai-codex`-upstream status (logged in / logged out /
  pending), a "Connect ChatGPT" button, the `user_code` and verification link while
  pending, auto-refresh while pending.
- `POST /api/codex/start` → `{user_code, verification_uri, interval, expires_in}`;
  starts a proxy-side poll task (idempotent: a live flow is reused).
- `GET /api/codex/status` → `{state: "idle"|"pending"|"logged_in"|"failed", user_code?,
  verification_uri?, message?}`.
- Poll state lives in `AppState` (`Arc<Mutex<HashMap<provider_id, PendingCodexFlow>>>`).

## Errors

| Situation | Behavior |
| --- | --- |
| No state file / logged out | 502 upstream_error, message contains `/setup` |
| Device code 404 | `/api/codex/start` 502, "device code login not enabled" |
| Poll expires (15 min) | status `failed`, message "Device flow timed out" |
| Refresh network error | backoff; request fails 502 fast |
| `invalid_grant` | logged-out latch, 502 with hint |
| Upstream 401 mid-session | one force-refresh + retry, else surface 401 |
| Body not valid JSON | 400 invalid_request_error |

## Testing (TDD)

1. Pure helpers: JWT account-id (incl. malformed/short tokens), URL join matrix, body
   transform (forced fields, `max_output_tokens` dropped, client fields preserved,
   invalid JSON), header set.
2. Device client against a mock auth server: start (numeric-string interval), poll
   pending/403/slow_down/complete, exchange success + missing-field failure.
3. Token manager (tempdir state files, mock token server): rotation persisted before
   use, missing `refresh_token` keeps the old one, two concurrent `access()` on an
   expired token → exactly one refresh, `invalid_grant` latch + relogin adoption,
   margin refresh, backoff growth, `0600` perms.
4. Provider against a mock Codex upstream: wire assertions (Bearer, `chatgpt-account-id`,
   `originator`, `OpenAI-Beta`, session headers, transformed body), 401 → one refresh +
   retry, logged-out 502, chat/messages → 400 surface mismatch.
5. Wiring: config validation matrix, manager creation from `--config` path, background
   refresh spawn, provider build arm.
6. `/setup` handlers: start/status state machine, unknown provider 400, idempotent start.
7. e2e: full server + mock auth server + mock Codex upstream — device login through
   `/api/codex/*`, then a streamed `/v1/responses` call with `aiproxy` bearer token,
   byte-identical SSE relayed; api-key upstream regression untouched.
8. Docs: README, `aiproxy.yaml.example`, CHANGELOG, version bump, extension
   `UPSTREAM_KINDS`, then **live smoke**: real device-code login + one streamed request;
   routing proven by the Codex backend's own auth/error surfacing through the proxy.

## Open items

- Whether the Codex backend tolerates any other field pi's Responses client sends
  (`max_output_tokens` is the only suspect today, and the transform drops it) — resolved
  by the live smoke in Task 8.
- `User-Agent` kernel-release detail may need widening if the backend ever gates on it.
