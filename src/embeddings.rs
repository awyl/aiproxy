//! Local embeddings — on-demand llama-server children behind the fake
//! `embeddings-local` provider.
//!
//! Lifecycle: a child is spawned on first request for a model (single-flight),
//! kept for reuse, and killed by the idle reaper after `idle_ttl_secs` with no
//! traffic. Children are independent OS processes on 127.0.0.1:<port>; the
//! proxy relays `POST /v1/embeddings` to the right port.

use std::time::{Duration, Instant};
use serde_json::Value;
use crate::config::EmbeddingsConfig;
use tokio::process::Command;

/// How long to wait for a child to serve /health == 200 (llama.cpp answers 503
/// while loading, so poll).
const HEALTH_POLL_STEP: Duration = Duration::from_millis(500);
const HEALTH_MAX_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("unknown embedding model: {0}")]
    UnknownModel(String),
    #[error("embedding backend '{0}' failed to start: {1}")]
    SpawnFailed(String, String),
    #[error("embedding backend '{0}' not ready: {1}")]
    NotReady(String, String),
    #[error("embedding call failed ({0}): {1}")]
    Http(u16, String),
    #[error("embedding call failed: {0}")]
    Transport(String),
}

#[derive(Debug)]
pub struct EmbeddingSlot {
    pub id: String,
    pub port: u16,
    model_file: String,
    state: tokio::sync::Mutex<SlotState>,
}

#[derive(Debug)]
enum SlotState {
    Idle,
    /// Spawned child, single-flight: the slot mutex is held across spawn +
    /// health wait, so concurrent callers serialize and reuse the child.
    Live {
        child: tokio::process::Child,
        last_used: Instant,
    },
}

impl EmbeddingSlot {
    fn new(id: String, model_file: String, port: u16) -> Self {
        Self {
            id,
            port,
            model_file,
            state: tokio::sync::Mutex::new(SlotState::Idle),
        }
    }
}

#[derive(Debug)]
pub struct EmbeddingManager {
    llama_bin: String,
    idle_ttl: Duration,
    slots: Vec<EmbeddingSlot>,
}

/// Kill every running child synchronously on drop (also covers panic
/// unwinding — tokio's Child does not kill on drop, unlike std's).
impl Drop for EmbeddingManager {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            if let Ok(mut st) = slot.state.try_lock() {
                if let SlotState::Live { child, .. } = &mut *st {
                    let _ = child.start_kill();
                }
            }
        }
    }
}

impl EmbeddingManager {
    pub fn new(cfg: &EmbeddingsConfig) -> Self {
        let slots = cfg
            .models
            .iter()
            .enumerate()
            .map(|(i, m)| EmbeddingSlot::new(m.id.clone(), m.model_file.clone(), cfg.port_for(i)))
            .collect();
        Self {
            llama_bin: cfg.llama_bin.clone(),
            idle_ttl: Duration::from_secs(cfg.idle_ttl_secs),
            slots,
        }
    }

    /// Unprefixed embedding model ids for the /v1/models catalog.
    pub fn model_ids(&self) -> Vec<String> {
        self.slots.iter().map(|s| s.id.clone()).collect()
    }

    fn slot(&self, id: &str) -> Option<&EmbeddingSlot> {
        self.slots.iter().find(|s| s.id == id)
    }

    /// Ensure the child for `id` is running (spawn on demand), then relay an
    /// OpenAI `/v1/embeddings` request and return the upstream JSON verbatim.
    pub async fn embed(&self, id: &str, req: &Value) -> Result<Value, EmbedError> {
        let slot = self
            .slot(id)
            .ok_or_else(|| EmbedError::UnknownModel(id.to_string()))?;
        let port = {
            let mut st = slot.state.lock().await;
            self.ensure_spawned(slot, &mut st).await?;
            slot.port
        };
        let url = format!("http://127.0.0.1:{port}/v1/embeddings");
        let resp = reqwest::Client::new()
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| EmbedError::Transport(format!("{url}: {e}")))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| EmbedError::Transport(format!("bad response body: {e}")))?;
        if !status.is_success() {
            return Err(EmbedError::Http(status.as_u16(), body.to_string()));
        }
        Ok(body)
    }

    /// Spawn the child if not Live. Single-flight: the caller holds the slot
    /// lock; spawn + health wait happen under it.
    async fn ensure_spawned(
        &self,
        slot: &EmbeddingSlot,
        st: &mut SlotState,
    ) -> Result<(), EmbedError> {
        match st {
            SlotState::Live { last_used, .. } => {
                *last_used = Instant::now();
                Ok(())
            }
            SlotState::Idle => {
                let mut child = Command::new(&self.llama_bin)
                    .args([
                        "-m",
                        &slot.model_file,
                        "--embeddings",
                        "--host",
                        "127.0.0.1",
                        "--port",
                        &slot.port.to_string(),
                    ])
                    // Spawn errors surface through the health poll; don't hold
                    // the child's stdout pipe open (would block the proxy on a
                    // chatty child).
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| EmbedError::SpawnFailed(slot.id.clone(), e.to_string()))?;
                match wait_healthy(slot.port, &slot.id).await {
                    Ok(()) => {
                        *st = SlotState::Live {
                            child,
                            last_used: Instant::now(),
                        };
                        Ok(())
                    }
                    Err(e) => {
                        let _ = child.kill().await;
                        Err(e)
                    }
                }
            }
        }
    }

    /// Kill every running child immediately (shutdown / test cleanup).
    pub async fn shutdown_all(&self) {
        for slot in &self.slots {
            let mut st = slot.state.lock().await;
            if let SlotState::Live { child, .. } = &mut *st {
                let _ = child.kill().await;
                let _ = child.wait().await;
                *st = SlotState::Idle;
            }
        }
    }

    /// One reaper pass: kill + drop children idle longer than the TTL.
    /// Called from the reaper task at an interval; public so tests drive it.
    pub async fn reaper_round(&self) {
        for slot in &self.slots {
            let mut st = slot.state.lock().await;
            if let SlotState::Live { child, last_used } = &mut *st {
                if last_used.elapsed() >= self.idle_ttl {
                    let _ = child.kill().await;
                    let _ = child.wait().await; // reap zombie
                    *st = SlotState::Idle;
                }
            }
        }
    }
}

/// Poll `GET /health` until 200 (llama.cpp returns 503 while loading).
async fn wait_healthy(port: u16, id: &str) -> Result<(), EmbedError> {
    let url = format!("http://127.0.0.1:{port}/health");
    let client = reqwest::Client::new();
    let deadline = Instant::now() + HEALTH_MAX_WAIT;
    while Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(_) => {} // 503: still loading
            Err(_) => {}
        }
        tokio::time::sleep(HEALTH_POLL_STEP).await;
    }
    Err(EmbedError::NotReady(
        id.to_string(),
        "child did not serve /health == 200 within 30s".into(),
    ))
}#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use crate::config::{EmbeddingModelConfig, EmbeddingsConfig};
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One fake llama-server bin for the whole test binary: written + chmod'd
    /// ONCE (executing a just-written script races overlayfs copy-up ->
    /// ETXTBSY). Per-test output files derive from the `-m` file path
    /// (`<model_dir>/<id>.pid|.count`), so parallel tests keep separate state.
    fn fake_llama_bin() -> &'static str {
        static BIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        BIN.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("aiproxy-fake-llama-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let py = dir.join("fake_llama.py");
            fs::write(
                &py,
                r#"import http.server, json, sys, os
port=int(sys.argv[1]); pidf=sys.argv[2]; cntf=sys.argv[3]
open(pidf,"w").write(str(os.getpid()))
open(cntf,"a").write("x")
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path=="/health":
            b=b'{"status":"ok"}'; self.send_response(200); self.send_header("Content-Length",str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_POST(self):
        n=int(self.headers.get("Content-Length",0)); body=json.loads(self.rfile.read(n))
        out={"object":"list","model":body.get("model","test"),"data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],"usage":{"prompt_tokens":1,"total_tokens":1}}
        b=json.dumps(out).encode(); self.send_response(200); self.send_header("Content-Type","application/json"); self.send_header("Content-Length",str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self,*a): pass
http.server.HTTPServer(("127.0.0.1",port),H).serve_forever()
"#,
            )
            .unwrap();
            let sh = dir.join("fake_llama.sh");
            fs::write(
                &sh,
                r#"#!/bin/bash
# fake llama-server: parse -m <file> and --port <N> from manager args
MODEL=""; PORT=""
while [ $# -gt 0 ]; do
  case "$1" in
    -m) MODEL="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    *) shift ;;
  esac
done
ID=$(basename "$MODEL" .gguf)
OUT=$(dirname "$MODEL")
SDIR=$(dirname "$0")
exec python3 "$SDIR/fake_llama.py" "$PORT" "$OUT/$ID.pid" "$OUT/$ID.count"
"#,
            )
            .unwrap();
            let mut perm = fs::metadata(&sh).unwrap().permissions();
            use std::os::unix::fs::PermissionsExt;
            perm.set_mode(0o755);
            fs::set_permissions(&sh, perm).unwrap();
            sh.to_str().unwrap().to_string()
        })
    }

    /// Manager over the fake bin; one model per `id`. Model file paths live in
    /// `dir` (need not exist) so pid/count files land in the test's dir.
    pub fn manager_with_fake(
        dir: &std::path::Path,
        port_cell: Arc<AtomicUsize>,
        ttl_secs: u64,
        ids: &[&str],
    ) -> EmbeddingManager {
        let models: Vec<EmbeddingModelConfig> = ids
            .iter()
            .map(|id| EmbeddingModelConfig {
                id: (*id).to_string(),
                model_file: dir.join(format!("{id}.gguf")).to_str().unwrap().to_string(),
                port: Some(port_cell.fetch_add(1, Ordering::SeqCst) as u16),
            })
            .collect();
        let cfg = EmbeddingsConfig {
            llama_bin: fake_llama_bin().to_string(),
            idle_ttl_secs: ttl_secs,
            models,
        };
        EmbeddingManager::new(&cfg)
    }

    pub fn spawn_count(dir: &std::path::Path, id: &str) -> usize {
        fs::read_to_string(dir.join(format!("{id}.count")))
            .map(|s| s.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use crate::config::{EmbeddingModelConfig, EmbeddingsConfig};
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn spawns_on_demand_once_and_relays() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager_with_fake(dir.path(), Arc::new(AtomicUsize::new(19010)), 3600, &["nomic"]);
        let req = serde_json::json!({ "model": "nomic", "input": "hello" });
        let out = mgr.embed("nomic", &req).await.expect("embed ok");
        assert_eq!(out["data"][0]["embedding"], serde_json::json!([0.1, 0.2]));
        assert_eq!(spawn_count(dir.path(), "nomic"), 1, "exactly one spawn");

        // second call reuses the running child — no new spawn
        let first_pid = fs::read_to_string(dir.path().join("nomic.pid")).unwrap();
        let _ = mgr.embed("nomic", &req).await.expect("embed ok 2");
        assert_eq!(spawn_count(dir.path(), "nomic"), 1, "no second spawn");
        assert_eq!(
            fs::read_to_string(dir.path().join("nomic.pid")).unwrap(),
            first_pid
        );
        mgr.shutdown_all().await; // cleanup — never leave orphans behind
    }

    #[tokio::test]
    async fn idle_reaper_kills_and_frees_the_port() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager_with_fake(dir.path(), Arc::new(AtomicUsize::new(19030)), 1, &["nomic"]);
        let req = serde_json::json!({ "model": "nomic", "input": "hi" });
        let _ = mgr.embed("nomic", &req).await.expect("embed ok");
        let first_pid = fs::read_to_string(dir.path().join("nomic.pid")).unwrap();
        assert_eq!(spawn_count(dir.path(), "nomic"), 1);

        // reaper pass until the child is gone (bounded wait — robust under load)
        let mut killed = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            mgr.reaper_round().await;
            if !std::path::Path::new(&format!("/proc/{first_pid}")).exists() {
                killed = true;
                break;
            }
        }
        assert!(killed, "idle child must be killed by the reaper");

        // port freed -> next request respawns (count increments, new pid)
        let _ = mgr.embed("nomic", &req).await.expect("embed ok 2");
        assert_eq!(spawn_count(dir.path(), "nomic"), 2, "child must be respawned after reap");
        assert_ne!(
            fs::read_to_string(dir.path().join("nomic.pid")).unwrap(),
            first_pid
        );
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn traffic_within_ttl_keeps_child_alive() {
        // generous ttl; traffic at ~600ms keeps last_used well inside it
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager_with_fake(dir.path(), Arc::new(AtomicUsize::new(19040)), 10, &["nomic"]);
        let req = serde_json::json!({ "model": "nomic", "input": "hi" });
        let _ = mgr.embed("nomic", &req).await.expect("embed 1");
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let _ = mgr.embed("nomic", &req).await.expect("embed 2"); // refreshes last_used
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        mgr.reaper_round().await;

        let _ = mgr.embed("nomic", &req).await.expect("embed 3");
        assert_eq!(spawn_count(dir.path(), "nomic"), 1, "recent traffic must keep the child");
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn unknown_model_is_rejected() {
        let mgr = EmbeddingManager::new(&EmbeddingsConfig::default());
        let err = mgr
            .embed("nope", &serde_json::json!({ "model": "nope" }))
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::UnknownModel(_)));
    }

    #[tokio::test]
    async fn spawn_failure_is_reported_with_binary() {
        let cfg = EmbeddingsConfig {
            llama_bin: "/nonexistent/llama-server".into(),
            idle_ttl_secs: 3600,
            models: vec![EmbeddingModelConfig {
                id: "x".into(),
                model_file: "/m/x.gguf".into(),
                port: Some(19020),
            }],
        };
        let mgr = EmbeddingManager::new(&cfg);
        let err = mgr
            .embed("x", &serde_json::json!({ "model": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::SpawnFailed(..)), "got {err:?}");
    }
}