//! OpenAI Codex (ChatGPT subscription) provider — Responses surface only.
//!
//! Requests are relayed to `{base}/codex/responses` with the Codex request
//! shape applied (see `crate::codex_oauth::transform_codex_body`) and the
//! Codex auth/identity headers; upstream SSE bytes are relayed verbatim.

use crate::codex_oauth::{self, CodexError, CodexModel, CodexTokenManager};
use crate::config::UpstreamConfig;
use crate::provider::{
    Event, Model, ModelSurface, Provider, ProviderError, ProviderStream, RequestContext,
};
use axum::body::Bytes;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct OpenAiCodexProvider {
    pub id: String,
    base_url: String,
    models: Vec<String>,
    discover: bool,
    manager: Arc<CodexTokenManager>,
    client: Client,
}

impl OpenAiCodexProvider {
    pub fn new(cfg: &UpstreamConfig, id: &str, manager: Arc<CodexTokenManager>) -> Self {
        Self::with_base_url(id, &cfg.effective_base_url(), cfg.models.clone(), manager)
            .with_discovery(cfg.discover)
    }

    pub fn with_base_url(
        id: &str,
        base_url: &str,
        models: Vec<String>,
        manager: Arc<CodexTokenManager>,
    ) -> Self {
        Self {
            id: id.to_string(),
            base_url: base_url.to_string(),
            models,
            discover: false,
            manager,
            client: crate::providers::default_http_client(),
        }
    }

    /// Probe the Codex model catalog instead of serving `models:` verbatim.
    /// The configured list stays as the fallback for a failed or impossible
    /// probe (offline, logged out, upstream error).
    pub fn with_discovery(mut self, discover: bool) -> Self {
        self.discover = discover;
        self
    }

    fn url(&self) -> String {
        codex_oauth::codex_responses_url(&self.base_url)
    }

    fn models_url(&self) -> String {
        format!(
            "{}?client_version={}",
            codex_oauth::codex_models_url(&self.base_url),
            codex_oauth::client_version()
        )
    }

    /// `GET {base}/codex/models` with the Codex auth/identity headers.
    /// `None` when there are no usable credentials — discovery is skipped
    /// rather than failing the whole refresh.
    async fn discover_models(&self) -> Result<Vec<CodexModel>, ProviderError> {
        let access = match self.manager.access().await {
            Ok(access) => access,
            Err(e) => return Err(codex_err(e)),
        };
        let account_id = codex_oauth::account_id_from_token(&access)
            .ok_or_else(|| codex_err(CodexError::LoggedOut))?;
        let resp = self
            .client
            .get(self.models_url())
            .headers(codex_oauth::codex_models_headers(&access, &account_id))
            .send()
            .await
            .map_err(|e| ProviderError::Transport(format!("codex catalog: {e}")))?;
        let status = resp.status();
        let body = resp.bytes().await.unwrap_or_default();
        if !status.is_success() {
            let parsed: Value = serde_json::from_slice(&body).unwrap_or_else(
                |_| json!({"error": {"message": "codex catalog request failed", "type": "upstream_error"}}),
            );
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body: parsed,
            });
        }
        let catalog = codex_oauth::parse_models_catalog(&body).map_err(codex_err)?;
        if catalog.models.is_empty() {
            // Silence here is what made a logged-in upstream look broken: the
            // probe answered, so nothing failed, and "0 models" said nothing
            // about why. Report the skip reasons instead.
            tracing::warn!(
                provider = %self.id,
                total = catalog.total,
                hidden = catalog.hidden,
                not_in_api = catalog.not_in_api,
                no_slug = catalog.no_slug,
                body = %String::from_utf8_lossy(&body[..body.len().min(400)]),
                "codex catalog probe returned nothing offerable"
            );
            return Err(ProviderError::Transport(format!(
                "codex catalog: {}",
                catalog.empty_reason()
            )));
        }
        Ok(catalog.models)
    }

    fn static_models(&self) -> Vec<Model> {
        self.models
            .iter()
            .map(|m| Model {
                id: m.clone(),
                display_name: None,
                created_at: None,
                surface: ModelSurface::Responses,
            })
            .collect()
    }

    /// One upstream attempt: fresh access token, Codex headers, transformed body.
    async fn send(
        &self,
        body: &Bytes,
        session_id: Option<&str>,
    ) -> Result<reqwest::Response, ProviderError> {
        let access = self.manager.access().await.map_err(codex_err)?;
        let account_id = codex_oauth::account_id_from_token(&access)
            .ok_or_else(|| codex_err(CodexError::LoggedOut))?;
        self.client
            .post(self.url())
            .headers(codex_oauth::codex_headers(&access, &account_id, session_id))
            .body(body.clone())
            .send()
            .await
            .map_err(|e| ProviderError::Transport(format!("codex upstream: {e}")))
    }

    async fn relay(resp: reqwest::Response) -> Result<ProviderStream, ProviderError> {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let body: Value = serde_json::from_str(&text)
                .unwrap_or_else(|_| json!({"error": {"message": text, "type": "upstream_error"}}));
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let stream = resp.bytes_stream().map(|chunk| match chunk {
            Ok(b) => Ok(Event(b)),
            Err(e) => Err(ProviderError::Transport(e.to_string())),
        });
        Ok(Box::new(stream))
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiCodexProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn surface_of(&self, _model: &str) -> ModelSurface {
        ModelSurface::Responses
    }

    async fn list_models(&self) -> Result<Vec<Model>, ProviderError> {
        if !self.discover {
            return Ok(self.static_models());
        }
        match self.discover_models().await {
            Ok(models) => Ok(models
                .into_iter()
                .map(|m| Model {
                    id: m.slug,
                    display_name: m.display_name,
                    created_at: None,
                    surface: ModelSurface::Responses,
                })
                .collect()),
            // Keep serving the configured catalog when the probe cannot answer
            // (logged out, upstream error, malformed payload)…
            Err(e) if !self.models.is_empty() => {
                tracing::warn!(provider = %self.id, error = ?e, "codex catalog probe failed; using the configured models");
                Ok(self.static_models())
            }
            // …and only surface the failure when there is nothing to fall back to.
            Err(e) => Err(e),
        }
    }

    async fn chat_completions(
        &self,
        _req: Bytes,
        _ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::Http {
            status: 400,
            body: json!({"error": {
                "message": "openai-codex upstream serves the Responses surface only — POST /v1/responses",
                "type": "invalid_request_error"
            }}),
        })
    }

    async fn messages(
        &self,
        _req: Bytes,
        _ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::Http {
            status: 400,
            body: json!({"error": {
                "message": "openai-codex upstream serves the Responses surface only — POST /v1/responses",
                "type": "invalid_request_error"
            }}),
        })
    }

    async fn responses(
        &self,
        req: Bytes,
        _ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        let codex_request = codex_oauth::transform_codex_body(req).map_err(codex_err)?;
        let session_id = codex_request.session_id.clone();
        let resp = self
            .send(&codex_request.body, session_id.as_deref())
            .await?;
        // A 401 means the access token went stale between refresh windows:
        // force one refresh and retry once, then surface whatever comes back.
        let resp = if resp.status().as_u16() == 401 {
            self.manager.force_refresh().await.map_err(codex_err)?;
            self.send(&codex_request.body, session_id.as_deref())
                .await?
        } else {
            resp
        };
        Self::relay(resp).await
    }
}

/// Codex errors → the API layer's provider error shape.
fn codex_err(err: CodexError) -> ProviderError {
    match err {
        CodexError::LoggedOut => ProviderError::Http {
            status: 502,
            body: json!({"error": {
                "message": err.to_string(),
                "type": "upstream_error"
            }}),
        },
        CodexError::Http { status, body } => ProviderError::Http { status, body },
        CodexError::InvalidJson(message) => ProviderError::Http {
            status: 400,
            body: json!({"error": {
                "message": message,
                "type": "invalid_request_error"
            }}),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_oauth::Tokens;
    use axum::Router;
    use axum::http::Uri;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SSE: &str =
        "event: response.output_text.delta\ndata: {\"delta\":\"hel\"}\n\ndata: [DONE]\n\n";

    #[derive(Default)]
    struct Upstream {
        calls: AtomicUsize,
        seen: Mutex<Vec<(HashMap<String, String>, Value)>>,
    }

    /// Codex-shaped mock upstream: `statuses[n]` is the status of call n (the
    /// last entry repeats). Captures headers + parsed body per call.
    async fn spawn_codex(state: Arc<Upstream>, statuses: Vec<u16>) -> String {
        let app = Router::new().route(
            "/codex/responses",
            post(move |headers: HeaderMap, body: String| {
                let state = state.clone();
                let statuses = statuses.clone();
                async move {
                    let n = state.calls.fetch_add(1, Ordering::SeqCst);
                    let seen_headers = headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                        .collect();
                    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    state.seen.lock().unwrap().push((seen_headers, parsed));
                    let status = statuses.get(n).copied().unwrap_or(200);
                    if status == 200 {
                        Response::builder()
                            .status(200)
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from(SSE))
                            .unwrap()
                    } else {
                        (
                            StatusCode::from_u16(status).unwrap(),
                            axum::Json(json!({"error": {"message": "nope"}})),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// Token endpoint mock returning a token with an account claim.
    async fn spawn_tokens(access: &'static str, calls: Arc<AtomicUsize>) -> String {
        let app = Router::new().route(
            "/oauth/token",
            post(move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let payload = json!({
                        "https://api.openai.com/auth": {"chatgpt_account_id": "acct_1"}
                    });
                    let header = "eyJhbGciOiJub25lIn0";
                    let payload_b64 = base64_url(json!(payload).to_string().as_bytes());
                    let token = format!("{header}.{payload_b64}.sig");
                    axum::Json(json!({
                        "access_token": format!("{access}|{token}"),
                        "refresh_token": "rt_new",
                        "expires_in": 86_400
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn base64_url(input: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(A[(n >> 18) as usize & 63] as char);
            out.push(A[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                out.push(A[(n >> 6) as usize & 63] as char);
            }
            if chunk.len() > 2 {
                out.push(A[n as usize & 63] as char);
            }
        }
        out
    }

    fn access_token_with_account(account: &str) -> String {
        let payload = json!({"https://api.openai.com/auth": {"chatgpt_account_id": account}});
        format!(
            "eyJhbGciOiJub25lIn0.{}.sig",
            base64_url(payload.to_string().as_bytes())
        )
    }

    /// Provider + tempdir guard so the state file outlives the provider borrow.
    struct Fixture {
        provider: OpenAiCodexProvider,
        _dir: tempfile::TempDir,
    }

    async fn fixture(token_base: &str, upstream_base: &str, access_prefix: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openai-codex-oauth-state.json");
        let manager = Arc::new(CodexTokenManager::new(
            &path,
            &format!("{token_base}/oauth/token"),
        ));
        manager
            .store_tokens(Tokens {
                access: format!("{access_prefix}|{}", access_token_with_account("acct_1")),
                refresh: "rt_1".into(),
                expires_at_ms: crate::codex_oauth::now_ms() + 24 * 3600 * 1000,
            })
            .await
            .unwrap();
        Fixture {
            provider: OpenAiCodexProvider::with_base_url(
                "openai-codex",
                upstream_base,
                vec!["gpt-5.6-sol".into()],
                manager,
            ),
            _dir: dir,
        }
    }

    /// Model-catalog mock: `GET /codex/models`, captures headers + query.
    #[derive(Default)]
    struct Catalog {
        calls: AtomicUsize,
        headers: Mutex<Vec<HashMap<String, String>>>,
        queries: Mutex<Vec<String>>,
    }

    async fn spawn_catalog(status: u16, body: Value) -> (String, Arc<Catalog>) {
        let state = Arc::new(Catalog::default());
        let handler_state = state.clone();
        let app = Router::new().route(
            "/codex/models",
            get(move |uri: Uri, headers: HeaderMap| {
                let state = handler_state.clone();
                let body = body.clone();
                async move {
                    state.calls.fetch_add(1, Ordering::SeqCst);
                    state.headers.lock().unwrap().push(
                        headers
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                            .collect(),
                    );
                    state
                        .queries
                        .lock()
                        .unwrap()
                        .push(uri.query().unwrap_or_default().to_string());
                    if status == 200 {
                        axum::Json(body).into_response()
                    } else {
                        (
                            StatusCode::from_u16(status).unwrap(),
                            axum::Json(json!({"detail": "Unauthorized"})),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), state)
    }

    /// Provider over `base` with a token manager (logged in unless told not to).
    async fn provider_with(
        base: &str,
        models: Vec<String>,
        discover: bool,
        logged_in: bool,
    ) -> (OpenAiCodexProvider, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openai-codex-oauth-state.json");
        let manager = Arc::new(CodexTokenManager::new(
            &path,
            "http://127.0.0.1:1/oauth/token",
        ));
        if logged_in {
            manager
                .store_tokens(Tokens {
                    access: access_token_with_account("acct_1"),
                    refresh: "rt_1".into(),
                    expires_at_ms: crate::codex_oauth::now_ms() + 24 * 3600 * 1000,
                })
                .await
                .unwrap();
        }
        (
            OpenAiCodexProvider::with_base_url("openai-codex", base, models, manager)
                .with_discovery(discover),
            dir,
        )
    }

    fn catalog_body() -> Value {
        json!({"models": [
            {"slug": "gpt-5.6-sol", "display_name": "GPT-5.6-Sol", "visibility": "list",
             "supported_in_api": true, "context_window": 272000, "priority": 1},
            {"slug": "gpt-daybreak-red-latest", "display_name": "Daybreak Red",
             "visibility": "hide", "supported_in_api": true},
            {"slug": "codex-auto-review", "display_name": "Codex Auto Review",
             "visibility": "hide", "supported_in_api": true},
            {"slug": "gpt-5.5", "display_name": "GPT-5.5", "visibility": "list",
             "supported_in_api": true}
        ]})
    }

    #[tokio::test]
    async fn discovery_probes_the_codex_catalog_with_auth_and_client_version() {
        let (base, catalog) = spawn_catalog(200, catalog_body()).await;
        let (provider, _dir) =
            provider_with(&base, vec!["static-fallback".into()], true, true).await;

        let models = provider.list_models().await.unwrap();
        assert_eq!(
            models
                .iter()
                .map(|m| (m.id.as_str(), m.display_name.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("gpt-5.6-sol", Some("GPT-5.6-Sol")),
                ("gpt-5.5", Some("GPT-5.5")),
            ],
            "hidden models are not offered"
        );
        assert!(models.iter().all(|m| m.surface == ModelSurface::Responses));
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 1);

        // the CLI's own query parameter and the Codex auth/identity headers
        let query = catalog.queries.lock().unwrap()[0].clone();
        assert_eq!(
            query,
            format!("client_version={}", crate::codex_oauth::client_version()),
            "got {query}"
        );
        let headers = catalog.headers.lock().unwrap()[0].clone();
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {}", access_token_with_account("acct_1")).as_str())
        );
        assert_eq!(
            headers.get("chatgpt-account-id").map(String::as_str),
            Some("acct_1")
        );
        assert_eq!(headers.get("originator").map(String::as_str), Some("pi"));
        assert_eq!(
            headers.get("accept").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(headers.get("openai-beta"), None);
    }

    #[tokio::test]
    async fn discovery_failure_falls_back_to_the_static_list() {
        let (base, catalog) = spawn_catalog(401, json!({})).await;
        let (provider, _dir) = provider_with(&base, vec!["gpt-5.6-sol".into()], true, true).await;
        let models = provider.list_models().await.unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gpt-5.6-sol"],
            "a failed probe keeps the configured catalog"
        );
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_catalog_with_nothing_offerable_says_why() {
        // A real account can list models that are all hidden or not API-capable.
        // The probe used to report success with 0 models, which reads as a broken
        // login; the reason must reach the caller (and /reload).
        let (base, _catalog) = spawn_catalog(
            200,
            json!({"models": [
                {"slug": "codex-auto-review", "visibility": "hide", "supported_in_api": true},
                {"slug": "gpt-5.6-sol", "visibility": "list", "supported_in_api": false},
            ]}),
        )
        .await;
        let (provider, _dir) = provider_with(&base, vec![], true, true).await;
        let err = provider.list_models().await.unwrap_err();
        let text = format!("{err:?}");
        assert!(
            text.contains("0 offerable")
                && text.contains("1 hidden")
                && text.contains("1 not supported in api"),
            "the empty catalog must explain itself, got {text}"
        );
        // and with a static list configured, that list still serves
        let (provider, _dir) = provider_with(&base, vec!["gpt-5.6-sol".into()], true, true).await;
        assert_eq!(
            provider
                .list_models()
                .await
                .unwrap()
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-5.6-sol"]
        );
    }

    #[tokio::test]
    async fn discovery_failure_without_a_static_list_is_an_error() {
        let (base, _catalog) = spawn_catalog(500, json!({})).await;
        let (provider, _dir) = provider_with(&base, vec![], true, true).await;
        let err = provider.list_models().await.unwrap_err();
        assert!(
            matches!(&err, ProviderError::Http { status, .. } if *status == 500),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn logged_out_discovery_falls_back_to_the_static_list() {
        let (base, catalog) = spawn_catalog(200, catalog_body()).await;
        let (provider, _dir) = provider_with(&base, vec!["gpt-5.6-sol".into()], true, false).await;
        let models = provider.list_models().await.unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gpt-5.6-sol"],
            "no credentials: no request, keep the configured catalog"
        );
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn discovery_off_never_probes() {
        let (base, catalog) = spawn_catalog(200, catalog_body()).await;
        let (provider, _dir) = provider_with(&base, vec!["gpt-5.6-sol".into()], false, true).await;
        let models = provider.list_models().await.unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["gpt-5.6-sol"]
        );
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 0);
    }

    fn expect_err(result: Result<ProviderStream, ProviderError>) -> ProviderError {
        match result {
            Ok(_) => panic!("expected a provider error"),
            Err(e) => e,
        }
    }

    fn ctx() -> RequestContext {
        RequestContext {
            model: "gpt-5.6-sol".into(),
            ..Default::default()
        }
    }

    fn response_body(session: Option<&str>) -> Bytes {
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [{"role": "user", "content": "hi"}],
            "max_output_tokens": 4096,
            "store": true
        });
        if let Some(session) = session {
            body["prompt_cache_key"] = json!(session);
        }
        Bytes::from(body.to_string())
    }

    #[tokio::test]
    async fn responses_relays_sse_and_sends_codex_wire_shape() {
        let upstream = Arc::new(Upstream::default());
        let base = spawn_codex(upstream.clone(), vec![200]).await;
        let token_calls = Arc::new(AtomicUsize::new(0));
        let token_base = spawn_tokens("at_1", token_calls.clone()).await;
        let fx = fixture(&token_base, &base, "at_1").await;

        let mut stream = fx
            .provider
            .responses(response_body(Some("sess-7")), &ctx())
            .await
            .unwrap();
        let mut relayed = Vec::new();
        while let Some(chunk) = stream.next().await {
            relayed.extend_from_slice(&chunk.unwrap().0);
        }
        assert_eq!(String::from_utf8_lossy(&relayed), SSE);

        let seen = upstream.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (headers, body) = &seen[0];
        assert!(
            headers
                .get("authorization")
                .is_some_and(|v| v.starts_with("Bearer at_1|")),
            "got {:?}",
            headers.get("authorization")
        );
        assert_eq!(
            headers.get("chatgpt-account-id").map(String::as_str),
            Some("acct_1")
        );
        assert_eq!(headers.get("originator").map(String::as_str), Some("pi"));
        assert_eq!(
            headers.get("openai-beta").map(String::as_str),
            Some("responses=experimental")
        );
        assert_eq!(
            headers.get("accept").map(String::as_str),
            Some("text/event-stream")
        );
        assert_eq!(
            headers.get("session-id").map(String::as_str),
            Some("sess-7")
        );
        assert_eq!(
            headers.get("x-client-request-id").map(String::as_str),
            Some("sess-7")
        );
        assert!(
            headers
                .get("user-agent")
                .is_some_and(|ua| ua.starts_with("pi (")),
            "got {:?}",
            headers.get("user-agent")
        );
        // body transform applied on the wire
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "You are a helpful assistant.");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["input"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn responses_refreshes_once_and_retries_on_401() {
        let upstream = Arc::new(Upstream::default());
        let base = spawn_codex(upstream.clone(), vec![401, 200]).await;
        let token_calls = Arc::new(AtomicUsize::new(0));
        let token_base = spawn_tokens("at_refreshed", token_calls.clone()).await;
        let fx = fixture(&token_base, &base, "at_stale").await;

        let mut stream = fx
            .provider
            .responses(response_body(None), &ctx())
            .await
            .unwrap();
        let mut relayed = Vec::new();
        while let Some(chunk) = stream.next().await {
            relayed.extend_from_slice(&chunk.unwrap().0);
        }
        assert_eq!(String::from_utf8_lossy(&relayed), SSE);
        assert_eq!(upstream.calls.load(Ordering::SeqCst), 2, "one retry");
        assert_eq!(token_calls.load(Ordering::SeqCst), 1, "one forced refresh");
        let seen = upstream.seen.lock().unwrap();
        assert!(
            seen[0]
                .0
                .get("authorization")
                .is_some_and(|v| v.starts_with("Bearer at_stale|")),
            "got {:?}",
            seen[0].0.get("authorization")
        );
        assert!(
            seen[1]
                .0
                .get("authorization")
                .is_some_and(|v| v.starts_with("Bearer at_refreshed|")),
            "retry uses the refreshed token: {:?}",
            seen[1].0.get("authorization")
        );
    }

    #[tokio::test]
    async fn responses_without_credentials_is_502_with_setup_hint() {
        let upstream = Arc::new(Upstream::default());
        let base = spawn_codex(upstream.clone(), vec![200]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openai-codex-oauth-state.json");
        let manager = Arc::new(CodexTokenManager::new(
            &path,
            "http://127.0.0.1:1/oauth/token",
        ));
        let provider = OpenAiCodexProvider::with_base_url(
            "openai-codex",
            &base,
            vec!["gpt-5.6-sol".into()],
            manager,
        );
        let err = expect_err(provider.responses(response_body(None), &ctx()).await);
        match err {
            ProviderError::Http { status, body } => {
                assert_eq!(status, 502);
                assert!(
                    body["error"]["message"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("/setup"),
                    "got {body}"
                );
            }
            other => panic!("expected Http, got {other:?}"),
        }
        assert_eq!(
            upstream.calls.load(Ordering::SeqCst),
            0,
            "never hit upstream"
        );
    }

    #[tokio::test]
    async fn upstream_error_body_is_relayed_as_provider_error() {
        let upstream = Arc::new(Upstream::default());
        let base = spawn_codex(upstream.clone(), vec![429]).await;
        let token_calls = Arc::new(AtomicUsize::new(0));
        let token_base = spawn_tokens("at_1", token_calls).await;
        let fx = fixture(&token_base, &base, "at_1").await;
        let err = expect_err(fx.provider.responses(response_body(None), &ctx()).await);
        match err {
            ProviderError::Http { status, body } => {
                assert_eq!(status, 429);
                assert_eq!(body["error"]["message"], "nope");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_json_body_is_400() {
        let fx = fixture("http://127.0.0.1:1", "http://127.0.0.1:1", "at").await;
        let err = expect_err(
            fx.provider
                .responses(Bytes::from_static(b"{nope"), &ctx())
                .await,
        );
        match err {
            ProviderError::Http { status, .. } => assert_eq!(status, 400),
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn surface_and_catalog_are_responses_only() {
        let fx = fixture("http://127.0.0.1:1", "http://127.0.0.1:1", "at").await;
        assert_eq!(
            fx.provider.surface_of("gpt-5.6-sol"),
            ModelSurface::Responses
        );
        assert_eq!(fx.provider.id(), "openai-codex");
        let models = fx.provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5.6-sol");
        assert_eq!(models[0].surface, ModelSurface::Responses);
    }

    #[tokio::test]
    async fn chat_and_messages_are_rejected_with_a_hint() {
        let fx = fixture("http://127.0.0.1:1", "http://127.0.0.1:1", "at").await;
        for err in [
            expect_err(
                fx.provider
                    .chat_completions(response_body(None), &ctx())
                    .await,
            ),
            expect_err(fx.provider.messages(response_body(None), &ctx()).await),
        ] {
            match err {
                ProviderError::Http { status, body } => {
                    assert_eq!(status, 400);
                    assert!(
                        body["error"]["message"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("/v1/responses"),
                        "got {body}"
                    );
                }
                other => panic!("expected Http, got {other:?}"),
            }
        }
    }

    #[test]
    fn access_token_fixture_account_is_parsed() {
        let token = access_token_with_account("acct_1");
        assert_eq!(
            codex_oauth::account_id_from_token(&token).as_deref(),
            Some("acct_1")
        );
    }
}
