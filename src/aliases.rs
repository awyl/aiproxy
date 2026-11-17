//! Durable, centrally managed model aliases. Provider implementations stay untouched.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct AliasStore {
    path: Option<PathBuf>,
    mappings: Mutex<BTreeMap<String, String>>,
}

impl AliasStore {
    pub fn memory() -> Self {
        Self::default()
    }

    pub fn load(path: PathBuf) -> io::Result<Self> {
        let map = match fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        validate_structure(&map)?;
        Ok(Self {
            path: Some(path),
            mappings: Mutex::new(map),
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn snapshot(&self) -> BTreeMap<String, String> {
        self.mappings.lock().unwrap().clone()
    }

    pub fn replace(&self, map: BTreeMap<String, String>) -> io::Result<()> {
        self.replace_checked(map, |_, _| Ok(()))
    }

    pub(crate) fn replace_checked(
        &self,
        map: BTreeMap<String, String>,
        validate: impl FnOnce(&BTreeMap<String, String>, &BTreeMap<String, String>) -> io::Result<()>,
    ) -> io::Result<()> {
        validate_structure(&map)?;
        let mut active = self.mappings.lock().unwrap();
        validate(&active, &map)?;
        if let Some(path) = &self.path {
            persist(path, &map)?;
        }
        *active = map;
        Ok(())
    }
}

fn persist(path: &Path, map: &BTreeMap<String, String>) -> io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".aiproxy-aliases-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(map)?)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        // The rename has committed; a directory-sync error cannot be rolled back.
        if let Err(e) = fs::File::open(parent).and_then(|dir| dir.sync_all()) {
            tracing::error!(path = %path.display(), "alias directory sync failed after save: {e}");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub(crate) fn validate_structure(map: &BTreeMap<String, String>) -> io::Result<()> {
    for (id, target) in map {
        let name = id.strip_prefix("default/").unwrap_or("");
        if name.is_empty()
            || matches!(name, "." | "..")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(invalid(format!(
                "invalid alias '{id}'; use default/<name> with letters, digits, dot, underscore or hyphen"
            )));
        }
        let Some((provider, model)) = target.split_once('/') else {
            return Err(invalid(format!(
                "invalid target '{target}'; use upstream/model"
            )));
        };
        if provider.is_empty()
            || provider == "default"
            || provider == "embeddings-local"
            || model.is_empty()
        {
            return Err(invalid(format!(
                "alias '{id}' must target a real inference model, not '{target}'"
            )));
        }
    }
    Ok(())
}

pub(crate) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub fn alias_state_path(config_path: &Path) -> PathBuf {
    let runtime = std::env::var_os("AIPROXY_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let config = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    pick_alias_dir(runtime, Path::new("/runtime").is_dir(), config).join("aiproxy-aliases.json")
}

fn pick_alias_dir(runtime: Option<PathBuf>, exists: bool, config: &Path) -> PathBuf {
    runtime
        .or_else(|| exists.then(|| PathBuf::from("/runtime")))
        .unwrap_or_else(|| config.to_path_buf())
}

type ApiError = (axum::http::StatusCode, axum::Json<serde_json::Value>);
type ApiResult = Result<(axum::http::HeaderMap, axum::Json<serde_json::Value>), ApiError>;

fn api_error(status: axum::http::StatusCode, message: impl ToString) -> ApiError {
    (
        status,
        axum::Json(serde_json::json!({"error":{"message":message.to_string()}})),
    )
}

fn authorize(
    state: &crate::api::AppState,
    headers: &axum::http::HeaderMap,
) -> Result<(), ApiError> {
    use axum::http::StatusCode;
    if let Some(expected) = &state.token {
        let provided = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));
        if provided.is_some_and(|s| crate::auth::constant_time_eq(s, expected)) {
            return Ok(());
        }
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "global proxy token required to manage aliases",
        ));
    }
    if !state.subscriptions.is_empty() {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "configure a global proxy token to manage aliases; subscription tokens are not administrators",
        ));
    }
    Ok(())
}

fn aliases_response(
    registry: &crate::discovery::ModelRegistry,
) -> (axum::http::HeaderMap, axum::Json<serde_json::Value>) {
    let available = registry.alias_models();
    let aliases: Vec<_> = registry.aliases().snapshot().into_iter().map(|(id, target)| {
        let surface = available.iter().find(|(m, t)| m.id == id && *t == target).map(|(m, _)| m.surface.as_str());
        serde_json::json!({"id":id,"target":target,"surface":surface,"available":surface.is_some()})
    }).collect();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    (
        headers,
        axum::Json(serde_json::json!({
            "aliases":aliases,
            "state_path":registry.aliases().path().map(|p| p.display().to_string()),
        })),
    )
}

pub async fn aliases_api(
    axum::extract::State(state): axum::extract::State<crate::api::AppState>,
    headers: axum::http::HeaderMap,
) -> ApiResult {
    authorize(&state, &headers)?;
    Ok(aliases_response(&state.registry))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveAliases {
    aliases: BTreeMap<String, String>,
}

pub async fn save_aliases(
    axum::extract::State(state): axum::extract::State<crate::api::AppState>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<SaveAliases>,
) -> ApiResult {
    authorize(&state, &headers)?;
    let registry = state.registry.clone();
    tokio::task::spawn_blocking(move || registry.replace_aliases(body.aliases))
        .await
        .map_err(|e| api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?
        .map_err(|e| {
            let status = if e.kind() == io::ErrorKind::InvalidInput {
                axum::http::StatusCode::BAD_REQUEST
            } else {
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            };
            api_error(status, e)
        })?;
    Ok(aliases_response(&state.registry))
}

pub async fn aliases_page() -> axum::response::Html<String> {
    crate::pages::with_page_frame(include_str!("aliases_page.html"), "/aliases")
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::api::AppState;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn state() -> AppState {
        let embeddings = std::sync::Arc::new(crate::embeddings::EmbeddingManager::new(
            &crate::config::EmbeddingsConfig::default(),
        ));
        crate::api::testutil::state_with(embeddings).await
    }

    async fn management(
        state: AppState,
        token: Option<&str>,
        method: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let app = Router::new()
            .route("/api/aliases", get(aliases_api).put(save_aliases))
            .with_state(state);
        let mut req = Request::builder()
            .method(method)
            .uri("/api/aliases")
            .header("content-type", "application/json");
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn aliases_management_requires_global_not_subscription_token() {
        let mut state = state().await;
        state.token = Some("admin".into());
        state
            .subscriptions
            .insert("openai".into(), Some("subscriber".into()));
        for method in ["GET", "PUT"] {
            for token in [None, Some("wrong"), Some("subscriber")] {
                assert_eq!(
                    management(state.clone(), token, method, json!({"aliases":{}}))
                        .await
                        .0,
                    StatusCode::UNAUTHORIZED
                );
            }
            assert_eq!(
                management(state.clone(), Some("admin"), method, json!({"aliases":{}}))
                    .await
                    .0,
                StatusCode::OK
            );
        }
    }

    #[tokio::test]
    async fn aliases_management_auth_disabled_and_subscription_only_modes() {
        let mut state = state().await;
        state.token = None;
        state.subscriptions.clear();
        assert_eq!(
            management(state.clone(), None, "PUT", json!({"aliases":{}}))
                .await
                .0,
            StatusCode::OK
        );
        state.subscriptions.insert("openai".into(), None);
        assert_eq!(
            management(state, None, "PUT", json!({"aliases":{}}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn aliases_management_invalid_update_keeps_old_mapping() {
        let state = state().await;
        state
            .registry
            .replace_aliases(BTreeMap::from([(
                "default/fast".into(),
                "openai/gpt-4o".into(),
            )]))
            .unwrap();
        let (status, body) = management(
            state.clone(),
            state.token.as_deref(),
            "PUT",
            json!({"aliases":{"default/fast":"default/free"}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("real inference")
        );
        assert_eq!(
            state.registry.resolve("default/fast"),
            Some(("openai".into(), "gpt-4o".into()))
        );
    }

    #[tokio::test]
    async fn aliases_routes_enforce_surfaces_and_resolved_subscription() {
        let mut state = state().await;
        state
            .registry
            .replace_aliases(BTreeMap::from([
                ("default/fast".into(), "openai/gpt-4o".into()),
                ("default/writer".into(), "anthropic/claude-sonnet-4".into()),
                ("default/planner".into(), "opencode-go/grok-4.6".into()),
            ]))
            .unwrap();
        state
            .subscriptions
            .insert("openai".into(), Some("owner".into()));
        let app =
            crate::api::openai::openai_router_with_subs(Some("tok".into()), &["owner".into()])
                .merge(crate::api::anthropic::anthropic_router_with_subs(
                    Some("tok".into()),
                    &["owner".into()],
                ))
                .with_state(state);
        for (path, alias, token, want) in [
            ("/v1/chat/completions", "default/fast", "owner", 200),
            ("/v1/chat/completions", "default/fast", "tok", 401),
            ("/v1/messages", "default/writer", "tok", 200),
            ("/v1/responses", "default/planner", "tok", 200),
            ("/v1/chat/completions", "default/planner", "tok", 400),
            ("/v1/messages", "default/fast", "owner", 400),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"model":alias,"messages":[],"input":[]}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), want, "{path} {alias}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::ModelRegistry;
    use crate::provider::{ModelSurface, Provider, testutil::MockProvider};
    use std::sync::Arc;

    fn mappings(target: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("default/fast".into(), target.into())])
    }

    #[test]
    fn aliases_save_load_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aiproxy-aliases.json");
        let store = AliasStore::load(path.clone()).unwrap();
        assert!(store.snapshot().is_empty());
        store.replace(mappings("openai/gpt-4o")).unwrap();
        assert_eq!(
            AliasStore::load(path).unwrap().snapshot(),
            mappings("openai/gpt-4o")
        );
    }

    #[test]
    fn aliases_failed_save_retains_active_and_persisted_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/aliases.json");
        let store = AliasStore::load(path.clone()).unwrap();
        store.replace(mappings("openai/old")).unwrap();
        // A non-directory parent makes the next save fail even as root.
        std::fs::rename(path.parent().unwrap(), dir.path().join("saved")).unwrap();
        std::fs::write(path.parent().unwrap(), b"blocked").unwrap();
        assert!(store.replace(mappings("openai/new")).is_err());
        assert_eq!(store.snapshot(), mappings("openai/old"));
        assert_eq!(
            AliasStore::load(dir.path().join("saved/aliases.json"))
                .unwrap()
                .snapshot(),
            mappings("openai/old")
        );
    }

    #[test]
    fn aliases_corrupt_or_structurally_invalid_state_is_not_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aliases.json");
        for bad in [
            "broken",
            "[]",
            r#"{"default/fast":"default/free"}"#,
            r#"{"../x":"openai/m"}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(AliasStore::load(path.clone()).is_err(), "{bad}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
        }
    }

    #[test]
    fn aliases_concurrent_writes_keep_disk_and_memory_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aliases.json");
        let store = Arc::new(AliasStore::load(path.clone()).unwrap());
        let tasks: Vec<_> = (0..12)
            .map(|i| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .replace(mappings(&format!("openai/model-{i}")))
                        .unwrap()
                })
            })
            .collect();
        for task in tasks {
            task.join().unwrap();
        }
        assert_eq!(AliasStore::load(path).unwrap().snapshot(), store.snapshot());
    }

    #[test]
    fn aliases_directory_precedence_ignores_codex_override() {
        let config = Path::new("/config");
        assert_eq!(
            pick_alias_dir(Some("/custom".into()), true, config),
            PathBuf::from("/custom")
        );
        assert_eq!(
            pick_alias_dir(None, true, config),
            PathBuf::from("/runtime")
        );
        assert_eq!(
            pick_alias_dir(None, false, config),
            PathBuf::from("/config")
        );
    }

    fn providers() -> Vec<Arc<dyn Provider>> {
        vec![
            Arc::new(MockProvider::with_surface(
                "openai",
                vec!["gpt-4o".into()],
                ModelSurface::ChatCompletions,
            )),
            Arc::new(MockProvider::with_surface(
                "anthropic",
                vec!["claude".into()],
                ModelSurface::Messages,
            )),
            Arc::new(MockProvider::with_surface(
                "openai-codex=alice",
                vec!["gpt-sol".into()],
                ModelSurface::Responses,
            )),
            Arc::new(MockProvider::with_surface(
                "static",
                vec!["unknown".into()],
                ModelSurface::Unknown,
            )),
        ]
    }

    #[tokio::test]
    async fn aliases_resolve_all_surfaces_and_keep_normal_resolution() {
        let registry = ModelRegistry::new(providers());
        registry.refresh().await;
        let map = BTreeMap::from([
            ("default/fast".into(), "openai/gpt-4o".into()),
            ("default/writer".into(), "anthropic/claude".into()),
            (
                "default/planner".into(),
                "openai-codex=alice/gpt-sol".into(),
            ),
        ]);
        registry.replace_aliases(map).unwrap();
        assert_eq!(
            registry.resolve("default/fast"),
            Some(("openai".into(), "gpt-4o".into()))
        );
        assert_eq!(
            registry.resolve("default/writer"),
            Some(("anthropic".into(), "claude".into()))
        );
        assert_eq!(
            registry.resolve("default/planner"),
            Some(("openai-codex=alice".into(), "gpt-sol".into()))
        );
        assert_eq!(
            registry.resolve("openai/arbitrary"),
            Some(("openai".into(), "arbitrary".into()))
        );
        let models = registry.alias_models();
        let planner = models
            .iter()
            .find(|(m, _)| m.id == "default/planner")
            .unwrap();
        assert_eq!(planner.0.surface, ModelSurface::Responses);
        assert_eq!(planner.1, "openai-codex=alice/gpt-sol");
    }

    #[tokio::test]
    async fn aliases_reject_invalid_names_targets_and_chains_without_changing_state() {
        let registry = ModelRegistry::new(providers());
        registry.refresh().await;
        registry.replace_aliases(mappings("openai/gpt-4o")).unwrap();
        for name in [
            "default/",
            "default/.",
            "default/..",
            "default/a/b",
            "default/a b",
            "openai/fast",
            "default/é",
        ] {
            assert!(
                registry
                    .replace_aliases(BTreeMap::from([(name.into(), "openai/gpt-4o".into())]))
                    .is_err(),
                "{name}"
            );
        }
        for target in [
            "default/free",
            "missing/m",
            "openai/missing",
            "embeddings-local/m",
            "static/unknown",
            "openai/",
        ] {
            assert!(
                registry.replace_aliases(mappings(target)).is_err(),
                "{target}"
            );
        }
        assert_eq!(registry.aliases().snapshot(), mappings("openai/gpt-4o"));
    }

    #[tokio::test]
    async fn aliases_unavailable_targets_retained_but_not_offered_or_routed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aliases.json");
        let store = AliasStore::load(path.clone()).unwrap();
        store.replace(mappings("openai/removed")).unwrap();
        let registry = ModelRegistry::with_aliases(providers(), store).unwrap();
        registry.refresh().await;
        assert!(registry.alias_models().is_empty());
        assert_eq!(registry.resolve("default/fast"), None);
        let mut map = mappings("openai/removed");
        map.insert("default/writer".into(), "anthropic/claude".into());
        registry.replace_aliases(map).unwrap();
        assert_eq!(registry.alias_models().len(), 1);
        assert_eq!(
            registry.aliases().snapshot().get("default/fast").unwrap(),
            "openai/removed"
        );
    }

    #[test]
    fn aliases_reserved_namespace_collision_is_rejected() {
        let providers: Vec<Arc<dyn Provider>> =
            vec![Arc::new(MockProvider::new("default", vec![]))];
        assert!(ModelRegistry::with_aliases(providers, AliasStore::memory()).is_err());
    }
}
