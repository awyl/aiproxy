> **⚠️ Warning: Built with AI. Use at your own risk.**

# aiproxy

Unified LLM proxy — set up once, every agent connects through it. No per-agent API key management.

Single endpoint serves OpenAI `/v1/*`, Anthropic `/v1/messages`, and OpenAI Responses `/v1/responses` wire formats. Models are auto-discovered or statically listed. MCP servers hosted at `/mcp/<name>`. Local CPU embeddings via llama.cpp.

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
export MINIMAX_API_KEY=your-minimax-key

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

Agent-facing model ids are always `<upstream-name>/<model-id>`, e.g. `opencode-go/mimo-v2.5`.

## Docker

```bash
# Build
DOCKER_USER=yourhubuser ./docker-push.sh 0.1.0

# Run
docker run -d \
  -v ./aiproxy.yaml:/etc/aiproxy/aiproxy.yaml \
  -e AIPROXY_TOKEN=secret \
  -e OPENCODE_GO_API_KEY=... \
  -p 8080:8080 \
  yourhubuser/aiproxy:0.1.0
```

The image includes Node.js (`npx`) and Python/uv (`uvx`) for MCP servers, plus `llama-server` for local embeddings.

## Configuration

Config lives in a single YAML file. Keys are **never** stored in the config — they reference env vars by name.

```yaml
bind: 127.0.0.1:8080                # or 0.0.0.0:8080 for all interfaces
token_env: AIPROXY_TOKEN             # bearer auth; omit both token_env/token = no auth
model_refresh_secs: 0                # 0 = fetch once at startup; >0 = periodic refresh

upstreams:
  - name: opencode-go
    kind: opencode-go
    api_key_env: OPENCODE_GO_API_KEY
    discover: true                   # public catalog, safe to enable

  - name: minimax
    kind: minimax
    api_key_env: MINIMAX_API_KEY
    models: [MiniMax-M3]             # static list, no probing

mcp:
  servers:
    - name: searxng
      command: npx
      args: ["-y", "mcp-searxng"]
      env:
        SEARXNG_URL: "http://localhost:8888"

embeddings:
  llama_bin: llama-server
  idle_ttl_secs: 3600
  models:
    - id: nomic-embed-text-v1.5
      model_file: /models/nomic-embed-text-v1.5.Q8_0.gguf
      port: 18081
```

### Discovery

- `models: [...]` — static list, never probed (recommended for keyed upstreams)
- `discover: true` — probe `GET <base_url>/models` at startup/refresh
- Neither — empty catalog; requests still route, agents see nothing in `/v1/models`

OpenCode Go, OpenRouter, and NVIDIA have **public/keyless** catalogs — `discover: true` is safe. MiniMax, Z.AI, and others require a valid API key.

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
    api_key_env: GO_BOB_TOKEN
    token_env: GO_BOB_TOKEN
```

Model ids become `go-alice/mimo-v2.5` and `go-bob/mimo-v2.5`. Alice can't use Bob's models.

## MCP hosting

aiproxy hosts MCP servers via the [pi-mcp-extension](https://www.npmjs.com/package/pi-mcp-extension) or any MCP client. Two transport types:

- **stdio**: proxy spawns a child process (`command` + `args` + `env`) and exposes it at `/mcp/<name>`
- **streamable-http**: proxy connects to a remote MCP server (`url`) and relays

```yaml
mcp:
  servers:
    - name: filesystem
      command: npx
      args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    - name: github
      url: https://api.githubcopilot.com/mcp/
      api_key_env: GITHUB_TOKEN
```

Clients connect at `http://<host>:8080/mcp/<name>` with the same bearer token as the proxy.

## Local embeddings

CPU-only embedding via fastembed (ONNX). Models auto-download from HuggingFace on first request and unload after idle timeout — only one model resident at a time.

```yaml
embeddings:
  idle_ttl_secs: 3600
  models:
    - id: nomic-embed-text-v1.5
      model: NomicEmbedTextV15
```

Models download automatically on first use — no manual GGUF management needed.

Exposed as `embeddings-local/<model-id>` in the catalog (surface: `embedding`). Standard `POST /v1/embeddings` endpoint.

## Connecting pi

Install the pi aiproxy extension:

```bash
# Project-scoped (recommended)
pi install ./agent/pi/extensions/aiproxy -l

# Or global
cp -r agent/pi/extensions/aiproxy ~/.pi/agent/extensions/aiproxy
```

Configure in `~/.pi/agent/models.json`:

```json
{
  "providers": {
    "aiproxy": {
      "baseUrl": "http://127.0.0.1:8080/v1",
      "apiKey": "$AIPROXY_TOKEN"
    }
  }
}
```

Models auto-register from `/v1/models`. Wire format (openai-completions / anthropic-messages / openai-responses) is set per-model based on the upstream's surface. Thinking defaults to `high` for all models; override per-model via `modelOverrides` in models.json.

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
- MCP `allowed_hosts` defaults to `[localhost, 127.0.0.1, ::1]`; add hostnames for container-to-host connections

## License

MIT
