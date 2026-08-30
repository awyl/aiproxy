use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::Response,
    Router,
};

/// Constant-time string comparison (no early length-scoped short-circuit on
/// content: length mismatch short-circuits, which is acceptable here).
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    for i in 0..ab.len() {
        diff |= ab[i] ^ bb[i];
    }
    diff == 0
}

/// Wrap a router with bearer-token auth. `None` disables auth entirely.
pub fn apply_auth(router: Router, expected: Option<String>) -> Router {
    router.layer(from_fn_with_state(expected, auth_check))
}

async fn auth_check(State(expected): State<Option<String>>, req: Request, next: Next) -> Response {
    let Some(expected) = expected else {
        return next.run(req).await;
    };
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match header {
        Some(provided) if constant_time_eq(provided, &expected) => next.run(req).await,
        _ => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::from("unauthorized"))
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn router_with(expected: Option<String>) -> Router {
        apply_auth(Router::new().route("/ok", get(|| async { "ok" })), expected)
    }

    #[test]
    fn constant_time_eq_cases() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }

    #[tokio::test]
    async fn auth_required_rejects_and_accepts() {
        let app = router_with(Some("sekrit".into()));
        let no_hdr = app
            .clone()
            .oneshot(Request::builder().uri("/ok").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(no_hdr.status(), StatusCode::UNAUTHORIZED);

        let wrong = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ok")
                    .header("authorization", "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let right = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ok")
                    .header("authorization", "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(right.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_disabled_passes_any_request() {
        let app = router_with(None);
        let resp = app
            .oneshot(Request::builder().uri("/ok").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}