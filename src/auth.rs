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

/// Auth state carried by the middleware: the global token (if any) plus every
/// subscription token. A request passes when its bearer token matches any of
/// them — so one token both authenticates and identifies a subscription.
#[derive(Debug, Clone, Default)]
pub struct AuthState {
    pub global: Option<String>,
    pub subscription_tokens: Vec<String>,
}

impl AuthState {
    pub fn accepts(&self, provided: &str) -> bool {
        self.global
            .as_deref()
            .is_some_and(|g| constant_time_eq(provided, g))
            || self
                .subscription_tokens
                .iter()
                .any(|s| constant_time_eq(provided, s))
    }
}

/// Wrap a router with bearer-token auth. `global: None` + no subscription
/// tokens disables auth entirely. The request's bearer token (if any) is
/// exposed to handlers via `Extension<Option<String>>`.
pub fn apply_auth<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    state: AuthState,
) -> Router<S> {
    router.layer(from_fn_with_state(state, auth_check))
}

pub fn auth_state(global: Option<String>, subscription_tokens: &[String]) -> AuthState {
    AuthState {
        global,
        subscription_tokens: subscription_tokens.to_vec(),
    }
}

async fn auth_check(State(state): State<AuthState>, mut req: Request, next: Next) -> Response {
    // Capture the request's bearer token so downstream handlers can apply
    // per-upstream subscription gates.
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.to_string());
    req.extensions_mut().insert(provided.clone());

    if state.global.is_none() && state.subscription_tokens.is_empty() {
        return next.run(req).await; // auth disabled
    }
    match provided.as_deref() {
        Some(provided) if state.accepts(provided) => next.run(req).await,
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
        apply_auth(
            Router::new().route("/ok", get(|| async { "ok" })),
            auth_state(expected, &[]),
        )
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

    #[tokio::test]
    async fn middleware_exposes_bearer_token_to_handlers() {
        use axum::extract::Extension;
        let app = apply_auth(
            Router::new().route("/tok", get(|Extension(t): Extension<Option<String>>| async move {
                t.unwrap_or_default()
            })),
            auth_state(Some("sekrit".into()), &[]),
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/tok")
                    .header("authorization", "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"sekrit");
    }
}