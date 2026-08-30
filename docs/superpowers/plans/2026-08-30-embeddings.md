# aiproxy local embeddings — fake provider, on-demand llama-server children

Date: 2026-08-30 · Status: draft plan (awaiting approval)

## Goal

Serve local CPU embeddings through aiproxy under one fake provider
(`embeddings-local`). Three model options ship default; backends are
llama-server child processes spawned **on demand**, killed after **1h idle**
(separate processes, no shared state). Surface: OpenAI `POST /v1/embeddings`
relay, one port for everything.

## Decisions (user-confirmed)

1. Fake upstream lives in **aiproxy** — new virtual kind whose catalog groups
   the embedding models; agents see `embeddings-local/*` in `/v1/models`.
2. **Proxy spawns llama-server** child processes per model (not in-process
   ONNX, not pre-running servers).
3. aiproxy gets a **`/v1/embeddings` relay surface**.
4. Lifecycle: spawn on first request, kill + release RAM after `idle_ttl_secs`
   (default 3600) with no traffic. Reaper thread separate from request
   handling; children are independent OS processes.

## Config

```yaml
embeddings:
  llama_bin: llama-server            # path or bare name (PATH)
  idle_ttl_secs: 3600                # kill child after 1h no traffic (default 3600)
  models:
    - id: nomic-embed-text-v1.5      # proxied id: embeddings-local/nomic-embed-text-v1.5
      model_file: /models/nomic-embed-text-v1.5.Q8_0.gguf
      port: 18081                    # localhost port for the child
    - id: all-MiniLM-L6-v2
      model_file: /models/all-MiniLM-L6-v2.Q8_0.gguf
      port: 18082
    - id: bge-small-en-v1.5
      model_file: /models/bge-small-en-v1.5.Q8_0.gguf
      port: 18083
```

`models:` absent → no embedding options exposed. Runtime shape follows the
OpenAI `/v1/embeddings` protocol (request `{model, input}` → `{data:[{embedding,
index, object}], model, usage}`). Responses are JSON (not SSE) — relayed
byte-for-byte.

## Architecture

- `src/config.rs` — `EmbeddingsConfig` + per-model validation (port in range,
  unique ids, file set). No `deny_unknown_fields` on the top level (extension
  keys like `thinking:` must keep parsing).
- `src/embeddings.rs` — `EmbeddingManager`:
  - `models: HashMap<String, Slot>`; `Slot { port, cmd, proc: Mutex<Option<Child>>, last_used: Mutex<Instant>, spawned_ok }`
  - `ensure_spawned(id)`: if not running, `tokio::process::Command(llama_bin) -m file --embeddings --host 127.0.0.1 --port N` (stdout/stderr piped to log), poll `GET /health` until 200 (max ~30s), touch `last_used`; already running → no second spawn (single-flight via slot state).
  - `embed(id, req: Value) -> Result<Value>`: ensure spawned → forward
    `POST 127.0.0.1:{port}/v1/embeddings` (keep `input` verbatim) → return
    upstream JSON body as-is; non-2xx → map to proxy error with status.
  - `reaper()`: tokio loop, every 60s scan slots; `last_used.elapsed() >
    idle_ttl_secs` → kill child (`Child::kill`), reap, clear slot. Failure to
    kill logs + retried next pass.
- `src/api/openai.rs` (or new `src/api/embeddings.rs`) — `POST /v1/embeddings`:
  strip `embeddings-local/` prefix on `model`, 400 if model unknown or wrong
  prefix, route to `EmbeddingManager::embed`. Mounted alongside chat routes.
- Catalog merge — `/v1/models`: after `registry.refresh()`, append
  `embeddings-local/<id>` entries (surface `embedding`, new `ModelSurface`
  variant used only for display; chat routers never see them). Embedding
  models come from config, never from discovery.
- `src/server.rs` — build `EmbeddingManager`, spawn reaper task; AppState
  carries it behind `Arc`.

## Model defaults

| option | GGUF (approx) | RSS | notes |
|---|---|---|---|
| `nomic-embed-text-v1.5` | Q8_0 ≈ 146MB | ~450MB | 768d, 8192 ctx, top pick |
| `all-MiniLM-L6-v2` | Q8 ≈ 30MB | ~250MB | 384d, English, smallest |
| `bge-small-en-v1.5` | Q8 ≈ 40MB | ~260MB | 384d, better MTEB, 512 ctx |

Budget note: only the requested model is resident; 1GB holds any single one.

## TDD tasks

1. **Config**: parse `embeddings:` block (ids, model_file, ports, llama_bin,
   idle_ttl default 3600); reject dup ids / bad ports. Tests first, red→green,
   commit.
2. **EmbeddingManager spawn-on-demand**: inject a fake `llama-server` (test
   script binding its port, serving `/health` + `/v1/embeddings` echo).
   Assert: first request spawns exactly one child, concurrent/second request
   reuses it (no second spawn).
3. **Idle reaper**: ttl = 1s, wait for reaper pass, assert child process
   exited (pid file / exit marker) and slot cleared; traffic within ttl keeps
   it alive.
4. **`/v1/embeddings` relay + catalog**: route test with fake backend —
   prefix strip, unknown-model 400, response relayed byte-identical;
   `/v1/models` includes `embeddings-local/*` with surface `embedding`.
5. **Server wiring + docs**: manager + reaper in server.rs, spawn failure →
   500 with named binary hint; example yaml block, README section (how to
   obtain GGUF files, llama-server install), no new env vars.

Each task ends all-green + committed on `feat/aiproxy`.

## Non-goals (v1)

- No in-process ONNX (fastembed-rs) — llama-server only.
- No batching/queueing across models beyond llama-server defaults.
- No auth on the child ports (loopback only; proxy gate is the boundary).
- `/v1/embeddings` only — no rerank relay.