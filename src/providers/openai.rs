use crate::config::UpstreamConfig;
use crate::provider::{
    Event, Model, ModelSurface, Provider, ProviderError, ProviderStream, RequestContext,
};
use futures::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    pub id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    client: Client,
}

impl OpenAiProvider {
    pub fn new(cfg: &UpstreamConfig, id: &str) -> Self {
        Self::new_with_key(cfg, id, cfg.api_key())
    }

    pub fn new_with_key(cfg: &UpstreamConfig, id: &str, api_key: Option<String>) -> Self {
        Self {
            id: id.to_string(),
            base_url: cfg.effective_base_url(),
            api_key,
            client: crate::providers::default_http_client(),
        }
    }

    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => builder.bearer_auth(k),
            None => builder,
        }
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn surface_of(&self, _model: &str) -> ModelSurface {
        ModelSurface::ChatCompletions
    }

    async fn list_models(&self) -> Result<Vec<Model>, ProviderError> {
        let resp = self
            .authed(self.client.get(format!("{}/models", self.base_url)))
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body,
            });
        }
        Ok(body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|m| Model {
                        id: m["id"].as_str().unwrap_or_default().to_string(),
                        display_name: None,
                        created_at: m["created"].as_u64().map(|c| c.to_string()),
                        surface: ModelSurface::ChatCompletions,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn chat_completions(
        &self,
        req: Value,
        ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        let resp = self
            .authed(
                self.client
                    .post(format!("{}/chat/completions", self.base_url)),
            )
            .json(&req)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(format!("upstream {}: {e}", ctx.model)))?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp
                .json()
                .await
                .unwrap_or_else(|_| json!({"error": {"message": "upstream error"}}));
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

    async fn messages(
        &self,
        _req: Value,
        _ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::Transport(
            "anthropic messages surface is not supported by openai-kind gateway".into(),
        ))
    }

    async fn responses(
        &self,
        _req: Value,
        _ctx: &RequestContext,
    ) -> Result<ProviderStream, ProviderError> {
        Err(ProviderError::Transport(
            "openai responses surface is not supported by openai-kind gateway".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::RequestContext;
    use crate::providers::test_mock_upstream::{Capture, SharedCapture, mock_openai_server};
    use futures::StreamExt;
    use serde_json::json;
    use std::sync::Arc;

    async fn spawn_mock() -> (SharedCapture, String) {
        let state = Arc::new(Capture::default());
        let app = mock_openai_server(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (state, format!("http://{addr}"))
    }

    fn provider(base: &str, key: Option<&str>) -> OpenAiProvider {
        let cfg = crate::config::UpstreamConfig {
            discover: false,
            token_env: None,
            surface: None,
            name: Some("mock".into()),
            kind: crate::config::UpstreamKind::Openai,
            base_url: Some(base.into()),
            api_key_env: None,
            models: vec![],
            endpoint_by_model: Default::default(),
            surface_map_url: None,
        };
        OpenAiProvider::new_with_key(&cfg, "mock", key.map(String::from))
    }

    #[tokio::test]
    async fn list_models_parses_upstream() {
        let (_state, base) = spawn_mock().await;
        let p = provider(&format!("{base}/v1"), None);
        let models = p.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-4o");
        assert_eq!(models[0].created_at.as_deref(), Some("1720000000"));
    }

    #[tokio::test]
    async fn chat_completions_sends_key_and_verbatim_body() {
        let (state, base) = spawn_mock().await;
        let p = provider(&format!("{base}/v1"), Some("sk-test"));
        let req = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]});
        let mut stream = p
            .chat_completions(
                req,
                &RequestContext {
                    model: "gpt-4o".into(),
                },
            )
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            out.extend_from_slice(&ev.unwrap().0);
        }
        assert_eq!(
            String::from_utf8_lossy(&out),
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: [DONE]\n\n"
        );
        let headers = state.headers.lock().unwrap();
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer sk-test")
        );
        let body = state.body.lock().unwrap().clone().unwrap();
        assert_eq!(body["model"], "gpt-4o");
    }

    #[tokio::test]
    async fn chat_completions_maps_upstream_error() {
        // base WITHOUT /v1 -> posts to {base}/chat/completions -> mock returns 500 JSON
        let (_state, base) = spawn_mock().await;
        let p = provider(&base, None);
        let err = match p
            .chat_completions(
                json!({"model": "gpt-4o"}),
                &RequestContext {
                    model: "gpt-4o".into(),
                },
            )
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("expected upstream error"),
        };
        match err {
            ProviderError::Http { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body["error"]["message"], "upstream boom");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn surface_is_chat_completions_only() {
        let (_state, base) = spawn_mock().await;
        let p = provider(&format!("{base}/v1"), None);
        assert_eq!(p.surface_of("gpt-4o"), ModelSurface::ChatCompletions);
        let err = p
            .messages(
                json!({"model": "gpt-4o"}),
                &RequestContext {
                    model: "gpt-4o".into(),
                },
            )
            .await
            .err()
            .expect("expected unsupported-surface error");
        assert!(matches!(err, ProviderError::Transport(_)));
        let err2 = p
            .responses(
                json!({"model": "gpt-4o"}),
                &RequestContext {
                    model: "gpt-4o".into(),
                },
            )
            .await
            .err()
            .expect("expected unsupported-surface error");
        assert!(matches!(err2, ProviderError::Transport(_)));
    }
}
