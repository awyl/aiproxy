use aiproxy::{config::Config, server};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use tower::ServiceExt;

async fn app(dir: &tempfile::TempDir, token: &str) -> Router {
    let yaml = format!(
        "bind: 127.0.0.1:0
{token}
upstreams: [{{kind: openai, models: [demo]}}]"
    );
    server::build(
        Config::from_yaml(&yaml).unwrap(),
        dir.path().join("aiproxy.yaml"),
    )
    .await
    .unwrap()
    .1
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "proxy.test");
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn one_login_protects_all_pages_and_management_and_sign_out_revokes_it() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, "token: fixture-global-secret").await;
    let setup = send(&app, "GET", "/setup?provider=openai-codex%3Dalice", &[], "").await;
    assert_eq!(
        setup.headers()[header::LOCATION],
        "/dashboard/login?next=%2Fsetup%3Fprovider%3Dopenai-codex%253Dalice"
    );
    let pages = ["/usage", "/models", "/aliases", "/setup", "/skills"];
    for page in pages {
        let res = send(&app, "GET", page, &[], "").await;
        assert_eq!(res.status(), 303, "{page} must require dashboard login");
        assert!(
            res.headers()[header::LOCATION]
                .to_str()
                .unwrap()
                .starts_with("/dashboard/login")
        );
    }
    for (method, path) in [
        ("GET", "/api/models"),
        ("GET", "/api/aliases"),
        ("GET", "/api/codex/providers"),
        ("GET", "/api/codex/status"),
        ("POST", "/api/codex/start"),
        ("POST", "/api/codex/complete"),
        ("POST", "/api/reload"),
        ("GET", "/api/skills"),
        ("POST", "/api/skills/draft-token"),
    ] {
        assert_eq!(
            send(&app, method, path, &[], "").await.status(),
            401,
            "{path}"
        );
    }
    let form = [
        ("content-type", "application/x-www-form-urlencoded"),
        ("origin", "https://proxy.test"),
    ];
    for password in ["wrong", "restricted-key"] {
        let res = send(
            &app,
            "POST",
            "/dashboard/login",
            &form,
            &format!("password={password}"),
        )
        .await;
        assert_eq!(res.status(), 401);
        assert!(!res.headers().contains_key(header::SET_COOKIE));
    }
    let res = send(
        &app,
        "POST",
        "/dashboard/login",
        &form,
        "password=fixture-global-secret&next=%2Fskills",
    )
    .await;
    assert_eq!(res.status(), 303);
    assert_eq!(res.headers()[header::LOCATION], "/skills");
    let set = res.headers()[header::SET_COOKIE].to_str().unwrap();
    for attribute in ["HttpOnly", "SameSite=Strict", "Secure"] {
        assert!(set.contains(attribute));
    }
    assert!(!set.contains("fixture-global-secret"));
    let cookie = set.split(';').next().unwrap();
    for page in pages {
        let res = send(&app, "GET", page, &[("cookie", cookie)], "").await;
        assert_eq!(res.status(), 200, "{page}");
        let html = String::from_utf8(to_bytes(res.into_body(), 1_000_000).await.unwrap().to_vec())
            .unwrap();
        assert!(!html.contains(r#"type="password""#), "{page}");
        assert!(html.contains("Sign out"));
    }
    assert_eq!(
        send(
            &app,
            "PUT",
            "/api/aliases",
            &[
                ("cookie", cookie),
                ("content-type", "application/json"),
                ("origin", "https://proxy.test")
            ],
            r#"{"aliases":{}}"#
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/skills/draft-token",
            &[("cookie", cookie), ("origin", "https://other.test")],
            ""
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/skills/draft-token",
            &[("cookie", cookie)],
            ""
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        send(&app, "GET", "/v1/models", &[("cookie", cookie)], "")
            .await
            .status(),
        401
    );
    assert_eq!(
        send(
            &app,
            "GET",
            "/v1/models",
            &[("authorization", "Bearer fixture-global-secret")],
            ""
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/dashboard/logout",
            &[("cookie", cookie), ("origin", "https://proxy.test")],
            ""
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        send(&app, "GET", "/api/models", &[("cookie", cookie)], "")
            .await
            .status(),
        401
    );
    assert_eq!(
        send(
            &app,
            "GET",
            "/api/models",
            &[("authorization", "Bearer fixture-global-secret")],
            ""
        )
        .await
        .status(),
        200
    );
}

#[tokio::test]
async fn no_global_token_keeps_dashboard_locked_without_stopping_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, "").await;
    assert_eq!(
        send(&app, "GET", "/dashboard/login", &[], "")
            .await
            .status(),
        503
    );
    assert_eq!(
        send(&app, "POST", "/api/reload", &[], "").await.status(),
        401
    );
    assert_eq!(
        send(&app, "GET", "/healthz", &[], "").await.status(),
        StatusCode::OK
    );
    assert_eq!(send(&app, "GET", "/v1/usage", &[], "").await.status(), 200);
}

#[tokio::test]
async fn bare_dashboard_address_reaches_password_form() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir, "token: fixture-global-secret").await;
    let mut path = "/".to_owned();
    for _ in 0..3 {
        let response = send(&app, "GET", &path, &[], "").await;
        if response.status().is_redirection() {
            path = response.headers()[header::LOCATION]
                .to_str()
                .unwrap()
                .to_owned();
            continue;
        }
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        let html = String::from_utf8(
            to_bytes(response.into_body(), 100_000)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("Sign in to aiproxy"));
        assert!(html.contains(r#"type="password""#));
        return;
    }
    panic!("dashboard did not reach sign-in form");
}
