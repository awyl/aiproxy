use axum::{
    Form, Json, Router,
    extract::{Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    middleware::{Next, from_fn_with_state},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const COOKIE: &str = "aiproxy_dashboard";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_SESSIONS: usize = 256;

#[derive(Clone, Default)]
pub(crate) struct DashboardAuth {
    password: Option<Arc<str>>,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
}

impl DashboardAuth {
    pub(crate) fn new(password: Option<String>) -> Self {
        Self {
            password: password.map(Arc::from),
            ..Self::default()
        }
    }

    fn accepts(&self, password: &str) -> bool {
        self.password
            .as_deref()
            .is_some_and(|expected| crate::auth::constant_time_eq(password, expected))
    }

    fn signed_in(&self, headers: &HeaderMap) -> bool {
        session_id(headers).is_some_and(|id| {
            self.sessions
                .lock()
                .unwrap()
                .get(id)
                .is_some_and(|expiry| *expiry > Instant::now())
        })
    }

    pub(crate) fn pages<S: Clone + Send + Sync + 'static>(&self, router: Router<S>) -> Router<S> {
        router.route_layer(from_fn_with_state(self.clone(), protect_page))
    }

    pub(crate) fn api<S: Clone + Send + Sync + 'static>(&self, router: Router<S>) -> Router<S> {
        router.route_layer(from_fn_with_state(self.clone(), protect_api))
    }

    pub(crate) fn routes(self) -> Router {
        Router::new()
            .route("/dashboard/login", get(login_page).post(login))
            .route("/dashboard/logout", post(logout))
            .with_state(self)
    }
}

fn session_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|v| v.trim().split_once('='))
        .find_map(|(name, value)| (name == COOKIE).then_some(value))
}

fn uncached(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn return_path(path: &str) -> &str {
    match path
        .parse::<axum::http::uri::PathAndQuery>()
        .ok()
        .as_ref()
        .map(|p| p.path())
    {
        Some("/usage" | "/models" | "/aliases" | "/setup" | "/skills") => path,
        _ => "/usage",
    }
}

async fn protect_page(State(auth): State<DashboardAuth>, req: Request, next: Next) -> Response {
    if auth.signed_in(req.headers()) {
        uncached(next.run(req).await)
    } else {
        let mut login = reqwest::Url::parse("http://local/dashboard/login").unwrap();
        let target = req.uri().path_and_query().map_or("/usage", |p| p.as_str());
        login
            .query_pairs_mut()
            .append_pair("next", return_path(target));
        uncached(
            Redirect::to(&format!("{}?{}", login.path(), login.query().unwrap())).into_response(),
        )
    }
}

async fn protect_api(State(auth): State<DashboardAuth>, req: Request, next: Next) -> Response {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let response = if bearer.is_some_and(|v| auth.accepts(v)) {
        next.run(req).await
    } else if !auth.signed_in(req.headers()) {
        let message = if auth.password.is_none() {
            "no global token configured for dashboard access"
        } else {
            "dashboard sign-in required"
        };
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": message}})),
        )
            .into_response()
    } else if !matches!(*req.method(), Method::GET | Method::HEAD)
        && !same_origin(req.headers(), req.uri())
    {
        StatusCode::FORBIDDEN.into_response()
    } else {
        next.run(req).await
    };
    uncached(response)
}

// SameSite=Strict excludes cross-site cookies, but same-site subdomains are
// still different origins. Reject browser writes from both.
fn same_origin(headers: &HeaderMap, uri: &Uri) -> bool {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|v| v != "same-origin")
    {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| uri.authority().map(|a| a.as_str()));
    origin
        .to_str()
        .ok()
        .and_then(|o| o.parse::<Uri>().ok())
        .is_some_and(|o| {
            matches!(o.scheme_str(), Some("http" | "https"))
                && o.authority()
                    .zip(host)
                    .is_some_and(|(given, expected)| given.as_str().eq_ignore_ascii_case(expected))
        })
}

#[derive(Deserialize)]
struct ReturnTo {
    #[serde(default)]
    next: String,
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
    #[serde(default)]
    next: String,
}

fn login_html(status: StatusCode, next: &str, message: &str) -> Response {
    let form = if status == StatusCode::SERVICE_UNAVAILABLE {
        String::new()
    } else {
        format!(
            r#"<form method="post" action="/dashboard/login"><input type="hidden" name="next" value="{}"><label>Password <input type="password" name="password" autocomplete="current-password" required autofocus></label> <button type="submit">Sign in</button></form>"#,
            return_path(next)
                .replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('<', "&lt;")
        )
    };
    uncached((status, Html(format!(r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>aiproxy sign in</title><style>body{{font:16px system-ui;max-width:28rem;margin:5rem auto;padding:0 1rem}}input,button{{font:inherit}}</style></head><body><h1>Sign in to aiproxy</h1><p>{message}</p>{form}</body></html>"#))).into_response())
}

const LOCKED: &str = "Dashboard locked. Configure a global proxy token (token or token_env) distinct from any subscription token, then restart aiproxy.";

async fn login_page(
    State(auth): State<DashboardAuth>,
    Query(target): Query<ReturnTo>,
    headers: HeaderMap,
) -> Response {
    if auth.password.is_none() {
        login_html(StatusCode::SERVICE_UNAVAILABLE, "", LOCKED)
    } else if auth.signed_in(&headers) {
        uncached(Redirect::to(return_path(&target.next)).into_response())
    } else {
        login_html(
            StatusCode::OK,
            &target.next,
            "Use your global proxy token as the password.",
        )
    }
}

async fn login(
    State(auth): State<DashboardAuth>,
    uri: Uri,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if !same_origin(&headers, &uri) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if auth.password.is_none() {
        return login_html(StatusCode::SERVICE_UNAVAILABLE, "", LOCKED);
    }
    if !auth.accepts(&form.password) {
        return login_html(StatusCode::UNAUTHORIZED, &form.next, "Incorrect password.");
    }
    let mut bytes = [0; 32];
    if getrandom::fill(&mut bytes).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let id = URL_SAFE_NO_PAD.encode(bytes);
    let now = Instant::now();
    {
        let mut sessions = auth.sessions.lock().unwrap();
        sessions.retain(|_, expiry| *expiry > now);
        if let Some(id) = session_id(&headers) {
            sessions.remove(id);
        }
        if sessions.len() >= MAX_SESSIONS {
            if let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, expiry)| *expiry)
                .map(|(id, _)| id.clone())
            {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(id.clone(), now + SESSION_TTL);
    }
    redirect_with_cookie(
        return_path(&form.next),
        &id,
        SESSION_TTL.as_secs(),
        &headers,
        &uri,
    )
}

async fn logout(State(auth): State<DashboardAuth>, uri: Uri, headers: HeaderMap) -> Response {
    if !same_origin(&headers, &uri) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(id) = session_id(&headers) {
        auth.sessions.lock().unwrap().remove(id);
    }
    redirect_with_cookie("/dashboard/login", "", 0, &headers, &uri)
}

fn redirect_with_cookie(
    path: &str,
    value: &str,
    max_age: u64,
    headers: &HeaderMap,
    uri: &Uri,
) -> Response {
    // Origin on a same-origin browser form reflects HTTPS even when TLS is
    // terminated by a reverse proxy. Allow local/plain HTTP without Secure.
    let secure = uri.scheme_str() == Some("https")
        || headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|o| o.starts_with("https://"));
    let mut response = Redirect::to(path).into_response();
    let flags = if secure { "; Secure" } else { "" };
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{flags}"
        ))
        .unwrap(),
    );
    uncached(response)
}
