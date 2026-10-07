> **⚠️ Warning: Built with AI. Use at your own risk.**

# aiproxy

Unified LLM proxy — set up once, every agent connects through it. No per-agent API key management.

Single endpoint serves OpenAI `/v1/*`, Anthropic `/v1/messages`, and OpenAI Responses `/v1/responses` wire formats. Models are auto-discovered or statically listed. MCP servers hosted at `/mcp/<name>`. Local CPU embeddings via fastembed (ONNX).

## Quick start

```bash
# 1. Clone and build
git clone https://github.com/awyl/aiproxy.git && cd aiproxy
cargo build --release

# 2. Configure
cp aiproxy.yaml.example aiproxy.yaml
# Edit aiproxy.yaml — add your upstreams and keys

# 3. Set API keys
export AIPROXY_TOKEN=your-proxy-secret
export OPENCODE_GO_API_KEY=your-go-key

# 4. Run
./target/release/aiproxy --config aiproxy.yaml
```

## Supported upstream kinds

| Kind | Wire format | Default base URL | Discovery | Notes |
|------|-------------|-----------------|-----------|-------|
| `opencode-go` | OpenAI + Anthropic + Responses | `opencode.ai/zen/go/v1` | Public, keyless | Routes per-model across 3 surfaces |
| `openai` | OpenAI chat completions | `api.openai.com/v1` | Keyed | Generic OpenAI-compatible gateway |
| `anthropic` | Anthropic messages | `api.anthropic.com/v1` | Keyed | Generic Anthropic-compatible gateway |
| `minimax` | OpenAI chat completions | `api.minimax.io/v1` | Keyed | Token Plan or pay-as-you-go |
| `zai` | OpenAI chat completions | `api.z.ai/api/coding/paas/v4` | Keyed | GLM Coding Plan |
| `openrouter` | OpenAI chat completions | `openrouter.ai/api/v1` | Public, keyless | 396+ models aggregated |
| `nvidia` | OpenAI chat completions | `integrate.api.nvidia.com/v1` | Public, keyless | NIM cloud; self-hosted via `base_url` |
| `openai-codex` | OpenAI Responses | `chatgpt.com/backend-api` | Implicit (needs the login) | **ChatGPT Plus/Pro subscription** — OAuth login at `/setup`, no API key |

Agent-facing model ids are always `<provider-id>/<model-id>`, e.g. `opencode-go/grok-4.6`.

Provider IDs follow a scheme:
- **1 upstream of kind** → ID = kind name (e.g. `opencode-go`)
- **2+ upstreams of kind** → ID = kind=name (e.g. `opencode-go=alice`)

`name` is optional when only 1 upstream of a kind; required for multiples (must be unique within the kind).

## Docker

```bash
# Build (reads the version from Cargo.toml, tags :<version> + :latest)
DOCKER_USER=yourhubuser ./docker-push.sh

# Run
docker run -d \
  -v ./aiproxy.yaml:/etc/aiproxy/aiproxy.yaml:ro \
  -v aiproxy-runtime:/runtime \
  -v aiproxy-models:/models \
  -e AIPROXY_TOKEN=secret \
  -e OPENCODE_GO_API_KEY=... \
  -p 8080:8080 \
  yourhubuser/aiproxy:latest
```

`/runtime` is where OAuth logins live (`openai-codex-oauth-{name}.json`). Mount it, or
mount the config *directory* instead of the single file: mounting only
`aiproxy.yaml` leaves `/etc/aiproxy` an anonymous volume that is recreated empty with the
container, so a login stored next to the config file would not survive a container
recreate. Override the location with `AIPROXY_CODEX_STATE_DIR` (Codex only) or
`AIPROXY_RUNTIME_DIR` (shared).

The image includes Node.js (`npx`), Python/uv (`uvx`) for MCP servers, and ONNX Runtime (via fastembed) for local embeddings. Embedding models auto-download to `/models` on first use.

## Configuration

Config lives in a single YAML file. Keys are **never** stored in the config — they reference env vars by name.

```yaml
bind: 127.0.0.1:8080                # or 0.0.0.0:8080 for all interfaces
token_env: AIPROXY_TOKEN             # bearer auth; omit both token_env/token = no auth
model_refresh_secs: 0                # 0 = fetch once at startup; >0 = periodic refresh

upstreams:
  - kind: opencode-go              # name optional — provider ID = "opencode-go"
    api_key_env: OPENCODE_GO_API_KEY
    discover: true                   # public catalog, safe to enable

  - kind: minimax                   # provider ID = "minimax"
    api_key_env: MINIMAX_API_KEY
    models: [MiniMax-M3]             # static list, no probing

mcp:
  servers:
    - name: searxng
      command: npx
      args: ["-y", "mcp-searxng"]
      env:
        SEARXNG_URL: "http://localhost:8888"
    - name: github
      url: https://api.githubcopilot.com/mcp/
      api_key_env: GITHUB_TOKEN
      token_env: MCP_GITHUB_TOKEN    # per-server auth (optional, falls back to global)

embeddings:
  idle_ttl_secs: 3600
  models:
    - id: nomic-embed-text-v1.5
      model: NomicEmbedTextV15
```

### Discovery

- `models: [...]` — static list, never probed (recommended for keyed upstreams)
- `discover: true` — probe `GET <base_url>/models` at startup/refresh (for
  `openai-codex`: `GET <base_url>/codex/models`, authenticated with the subscription)
- Neither — empty catalog; requests still route, agents see nothing in `/v1/models`
- `surface: chat | messages | responses` — for a static list, which wire format those
  models are served on (this is what `/v1/models` reports and what clients route by).
  Without it a static entry is catalog-only: listed, but not streamable. On
  `opencode-go` the surface comes from `surface_map_url` / the builtin table instead,
  and `endpoint_by_model:` overrides a single model.

OpenCode Go, OpenRouter, and NVIDIA have **public/keyless** catalogs — `discover: true` is safe. MiniMax, Z.AI, and others require a valid API key. `openai-codex` discovery needs the ChatGPT login (run `/setup` first); before that it serves whatever `models:` lists.

Discovery runs once at startup and then every `model_refresh_secs` (default `0` = startup only), so an upstream that was unreachable at boot keeps an empty catalog. Open **`/models`** to see every upstream and the models it offers — click one to expand it — and press **Reload models** to re-run discovery. `GET /api/models` returns the same data as JSON without probing anything; `POST /api/reload` probes first and returns the same shape plus `reloaded_at_ms`. One entry per upstream: `{id, count, models: [{id, surface}], error}`. A login to `openai-codex` triggers discovery automatically.

### ChatGPT / Codex subscription (`openai-codex`)

Runs Codex models on a ChatGPT Plus/Pro subscription instead of an API key:

```yaml
upstreams:
  - kind: openai-codex          # provider ID = "openai-codex"
    models: [gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna, gpt-5.5]
```

- **No key, no `api_key_env`/`token_env`** — setting either is a config error. Auth is
the ChatGPT OAuth login (see *Where the login is stored* below).
- **Log in at `http://<proxy>/setup`** — browser login by default (PKCE, like pi's own
Codex login): the page opens `auth.openai.com`, and when you authorize, the browser is
sent to `http://localhost:1455/auth/callback?code=…&state=…`. **Nothing listens on that
port**, so the browser shows a connection error — that is expected. Copy the whole URL
out of the address bar and paste it into the page (`POST /api/codex/complete`), which
works for a remote or containerized proxy and never fights the Codex CLI for 1455. A
**Use device code** button runs the headless flow instead (enter the code at
`auth.openai.com/codex/device`, proxy polls for you). Tokens are refreshed in the
background (and on a `401`, once, mid-request).
- **Responses surface only**: agents call `POST /v1/responses` with
`openai-codex/<model>`; `/v1/chat/completions` and `/v1/messages` are rejected for these
models. The proxy applies the Codex request shape (forces `store: false`, `stream: true`,
default `instructions`, `include: ["reasoning.encrypted_content"]`, `text.verbosity: "low"`,
drops `max_output_tokens`) and sends `originator: pi` plus your account id.
- **Model discovery** — probes the Codex catalog
(`GET {base}/codex/models?client_version=X.Y.Z`, same OAuth headers as requests) on every
model-refresh tick. Offered entries are the ones the picker itself shows:
`visibility: list` (in ChatGPT mode every model is visible, so `supported_in_api` is
*not* applied — applying it is what silently dropped the subscription-only models).
Hidden entries such as `codex-auto-review` are skipped. A
`models:` list is optional; when present it is the fallback if the probe cannot answer
(logged out, offline, upstream error). Without either, the catalog stays empty until login.
- Not logged in yet? Requests fail `502` with a hint to open `/setup`.

**Where the login is stored.** `{state-dir}/openai-codex-oauth-{name}.json`, mode `0600`,
where `{name}` is the upstream's `name:` (or `openai-codex` when it has none) — the same
convention as the old opencode-go cookie files, so the file itself says who logged in.
The state dir is resolved in this order: `AIPROXY_CODEX_STATE_DIR` →
`AIPROXY_RUNTIME_DIR` → `/runtime` when it exists → the config file's directory.
`/api/codex/status` reports the exact path
(`state_path`) and whether a file is really there (`state_file`), `/setup` shows the same
line, and startup logs `codex credentials path=… logged_in=…` — check that first when a
login seems to vanish after a restart. A successful login also re-runs discovery on its
own, so the models appear without a restart.

**`client_version` matters.** The catalog `GET` sends a `client_version`, and the
backend hides models newer than that version — with no error, just an empty list.
aiproxy therefore sends a recent Codex CLI version (`CODEX_CLIENT_VERSION` in
`src/codex_oauth.rs`), **not** its own. Verified live: no parameter → `400`;
`0.4.0` → `200 {"models":[]}`; `0.161.0` → `200`, 10 models.

Bump it per upstream in the config, or globally in the environment:

```yaml
- kind: openai-codex
  name: nyccom
  client_version: "0.161.0"   # optional; wins over AIPROXY_CODEX_CLIENT_VERSION
```

Resolution order: `client_version:` → `AIPROXY_CODEX_CLIENT_VERSION` → built-in
default. `client_version:` is only valid on the `openai-codex` kind.

**More than one subscription.** Give each upstream a `name:` — ids become
`openai-codex=<name>`, each with its own login, state file and model prefix:

```yaml
upstreams:
  - { kind: openai-codex, name: alice, models: [gpt-5.6-sol] }
  - { kind: openai-codex, name: bob,   models: [gpt-5.6-sol] }
```

`/setup` then lists both and links to one page per subscription (`/setup?provider=openai-codex%3Dalice`);
the JSON APIs take the same `?provider=` parameter, and `GET /api/codex/providers` lists
every subscription with its `logged_in` flag and state path. Agent-facing ids are
`openai-codex=alice/gpt-5.6-sol` and `openai-codex=bob/gpt-5.6-sol`.

Plan usage/limits for the subscription are not fetched (deferred).

### Multi-subscription

Each upstream can have its own bearer token via `token_env`. The token both authenticates and locks the request to that upstream's models.

```yaml
upstreams:
  - name: go-alice
    kind: opencode-go
    api_key_env: GO_ALICE_KEY
    token_env: GO_ALICE_TOKEN
  - name: go-bob
    kind: opencode-go
    api_key_env: GO_BOB_KEY
    token_env: GO_BOB_TOKEN
```

Model ids become `opencode-go=go-alice/grok-4.6` and `opencode-go=go-bob/grok-4.6` (the
`kind=name` form — see *Provider IDs* above). Alice can't use Bob's models.

## MCP hosting

aiproxy hosts MCP servers via the [pi-mcp-extension](https://www.npmjs.com/package/pi-mcp-extension) or any MCP client. Two transport types:

- **stdio**: proxy spawns a child process (`command` + `args` + `env`) and exposes it at `/mcp/<name>`
- **streamable-http**: proxy connects to a remote MCP server (`url`) and relays

Each MCP server can optionally require its own auth token via `token` (literal) or `token_env` (env var name). Precedence: per-server `token_env` > per-server `token` > global token. When no token is set on a server, clients authenticate with the global proxy token.

```yaml
mcp:
  servers:
    - name: filesystem
      command: npx
      args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    - name: github
      url: https://api.githubcopilot.com/mcp/
      api_key_env: GITHUB_TOKEN
      token_env: MCP_GITHUB_TOKEN   # this server requires its own token
    - name: searxng
      command: npx
      args: ["-y", "mcp-searxng"]
      # no token → uses global proxy token
```

Clients connect at `http://<host>:8080/mcp/<name>` with the appropriate bearer token.

### MCP multiplexer

A single `/mcp` endpoint aggregates multiple MCP servers. Use the `X-MCP-Servers` header to select which servers to include and authenticate per-server:

```
X-MCP-Servers: searxng:searxng_token,ctx7,grep:grep_token
```

Format: `name:token` or just `name`:
- `name:token` — use provided token for auth
- `name` — fallback to `Authorization: Bearer <token>` header

If the token matches the server's effective token (`token_env` > `token` > global), that server's tools are included. Tools are namespaced as `<server>__<tool>` (e.g. `searxng__search`).

```
POST /mcp
Headers:
  Authorization: Bearer <global-token>
  X-MCP-Servers: searxng:tok_a,ctx7
Body: {"jsonrpc":"2.0","method":"tools/list","id":1}
```

No `X-MCP-Servers` header → include all servers (auth via `Authorization` header).

Individual `/mcp/<name>` endpoints remain available for backward compatibility.

## Local embeddings

CPU-only embedding via fastembed (ONNX). Models auto-download from HuggingFace on first request and unload after idle timeout — only the requested model is resident, keeping memory low.

```yaml
embeddings:
  idle_ttl_secs: 3600
  models:
    - id: nomic-embed-text-v1.5    # proxied id: embeddings-local/nomic-embed-text-v1.5
      model: NomicEmbedTextV15
    - id: all-MiniLM-L6-v2        # proxied id: embeddings-local/all-MiniLM-L6-v2
      model: AllMiniLML6V2
    - id: bge-small-en-v1.5       # proxied id: embeddings-local/bge-small-en-v1.5
      model: BGESmallENV15
```

Models download automatically on first use — no manual GGUF management needed.

Exposed as `embeddings-local/<model-id>` in the catalog (surface: `embedding`). Standard `POST /v1/embeddings` endpoint.

## Usage tracking

`GET /v1/usage` returns per-provider rate-limit data captured from upstream response headers:

```json
[
  {
    "provider": "opencode-go",
    "windows": [
      { "resource": "requests", "limit": 100, "remaining": 65, "used_percent": 35.0, "reset_secs": 1800 },
      { "resource": "tokens", "limit": 1000000, "remaining": 400000, "used_percent": 60.0, "reset_secs": 3600 }
    ],
    "updated_at": 1725547200000
  }
]
```

In-memory only (lost on restart). Auth-gated like other `/v1/*` endpoints.

Sources differ per upstream: OpenAI-style gateways report rate-limit headers;
minimax/zai/openrouter have billing endpoints; **opencode-go** calls the official
Zen usage route (`GET <base_url>/usage` → `opencode.ai/zen/go/v1/usage`)
authenticated with the same inference API key the upstream already uses, and
reports its 5h / 7d / 30d subscription windows as percent. No browser cookie,
session or HTML scraping is involved.

The pi extension shows the most-pressured provider on startup and every 60s:
```
[aiproxy] opencode-go 60% (reset 1h)
```

## Connecting pi

Install the pi aiproxy extension as a pi package:

```bash
pi install git:github.com/awyl/aiproxy@v0.4.0
```

For a local checkout, `pi install ./agent/pi/extensions/aiproxy -l` (project-scoped).
Don't also keep a hand-copied extension under `~/.pi/agent/extensions/` — it would load
twice.

Configure in `~/.pi/agent/aiproxy.json` (a project-level `.pi/aiproxy.json` overrides
individual fields):

```json
{
  "baseUrl": "http://127.0.0.1:8080/v1",
  "apiKey": "$AIPROXY_TOKEN",
  "mcpServers": "searxng,ctx7,grep"
}
```

`mcpServers` is optional (it registers the proxy's MCP multiplexer tools). This file is
the extension's own — the proxy's `aiproxy.yaml` is never read by pi, and neither is
`models.json`.

Models auto-register from the proxy's `/v1/models`. Wire format
(openai-completions / anthropic-messages / openai-responses) is set per-model from the
`surface` the proxy reports, and metadata (context window, max tokens, reasoning,
thinking levels, cost) is resolved from the pi.dev model catalog, with pi's local
`models-store.json` as fallback.

## CLI

```
aiproxy --config <path>        # run with config file
aiproxy --port <port>          # override bind port
aiproxy --help                 # show options
```

## Security notes

- No TLS in v1 — put a reverse proxy (nginx, caddy) in front for anything public
- API keys live in env vars only; the config references them by name
- Bearer token auth; omit `token_env`/`token` for unauthenticated mode
- MCP servers support per-server auth tokens (falls back to global)
- MCP `allowed_hosts` defaults to `[localhost, 127.0.0.1, ::1]`; add hostnames for container-to-host connections

## License

MIT
