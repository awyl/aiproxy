use aiproxy::{config::Config, server};
use axum::{body::Body, http::Request};
use tower::ServiceExt;

#[tokio::test]
async fn discovery_requires_dashboard_auth_and_rejects_unsafe_sources() {
    let dir = tempfile::tempdir().unwrap();
    let (_,app)=server::build(Config::from_yaml("bind: 127.0.0.1:0\ntoken: admin-secret\nupstreams: [{kind: openai, models: [example]}]").unwrap(),dir.path().join("config.yaml")).await.unwrap();
    for (token, expected) in [
        (None, 401),
        (Some("wrong"), 401),
        (Some("admin-secret"), 400),
    ] {
        let mut request =
            Request::post("/api/skills/discover").header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(
                request
                    .body(Body::from(r#"{"url":"file:///etc"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
}
