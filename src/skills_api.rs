use crate::{
    skills::{Action, AdminSnapshot, SkillsError, SkillsStore},
    skills_mcp::DraftTokens,
};
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use std::sync::Arc;

pub(crate) fn router(
    store: Arc<SkillsStore>,
    draft_tokens: DraftTokens,
    github: crate::skills_github::Github,
) -> Router {
    Router::new()
        .route("/api/skills", get(snapshot))
        .route(
            "/api/skills/draft-token",
            post(issue_draft_token).delete(revoke_draft_token),
        )
        .route("/api/skills/{name}/actions", post(apply))
        .route("/api/skills/discover", post(discover))
        .route("/api/skills/import", post(import))
        .route("/api/skills/{name}/check-update", post(check_update))
        .layer(Extension(github))
        .layer(Extension(draft_tokens))
        .with_state(store)
}
async fn issue_draft_token(Extension(tokens): Extension<DraftTokens>) -> Response {
    match tokens.issue() {
        Ok(grant) => Json(grant).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"could not generate draft token"})),
        )
            .into_response(),
    }
}
async fn revoke_draft_token(Extension(tokens): Extension<DraftTokens>) -> StatusCode {
    tokens.revoke();
    StatusCode::NO_CONTENT
}
async fn snapshot(State(store): State<Arc<SkillsStore>>) -> Json<AdminSnapshot> {
    Json(store.admin_snapshot())
}
async fn apply(
    State(store): State<Arc<SkillsStore>>,
    Path(name): Path<String>,
    Json(action): Json<Action>,
) -> Result<Json<AdminSnapshot>, ApiError> {
    let result = tokio::task::spawn_blocking(move || store.apply(&name, action))
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "skill write failed".into(),
            )
        })?;
    Ok(Json(result?))
}
struct ApiError(StatusCode, String);
impl From<SkillsError> for ApiError {
    fn from(error: SkillsError) -> Self {
        let status = match error {
            SkillsError::Invalid(_) => StatusCode::BAD_REQUEST,
            SkillsError::Unavailable => StatusCode::NOT_FOUND,
            SkillsError::Conflict => StatusCode::CONFLICT,
            SkillsError::Persist(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, error.to_string())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}

use crate::skills_github::{Github, ImportError, Imported};
use serde::Deserialize;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportRequest {
    url: String,
    commit: String,
    path: String,
    name: String,
    note: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRequest {
    revision: u64,
}
impl From<ImportError> for ApiError {
    fn from(error: ImportError) -> Self {
        let status = match error {
            ImportError::Skill(e) => return e.into(),
            ImportError::Upstream(_) => StatusCode::BAD_GATEWAY,
            ImportError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ImportError::Busy => StatusCode::CONFLICT,
            ImportError::Invalid(_) => StatusCode::BAD_REQUEST,
        };
        Self(status, error.to_string())
    }
}
async fn stage(
    store: Arc<SkillsStore>,
    imported: Imported,
    base: Option<u64>,
    note: Option<String>,
) -> Result<Json<AdminSnapshot>, ApiError> {
    let result = tokio::task::spawn_blocking(move || {
        store.stage_import(&imported.name, imported.files, imported.source, base, note)
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "skill write failed".into(),
        )
    })?;
    Ok(Json(result?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoverRequest {
    url: String,
}
async fn discover(
    Extension(github): Extension<Github>,
    Json(req): Json<DiscoverRequest>,
) -> Result<Json<crate::skills_github::Discovery>, ApiError> {
    Ok(Json(github.discover(&req.url).await?))
}
async fn import(
    State(store): State<Arc<SkillsStore>>,
    Extension(github): Extension<Github>,
    Json(req): Json<ImportRequest>,
) -> Result<Json<AdminSnapshot>, ApiError> {
    let imported = github
        .import(&req.url, &req.commit, &req.path, &req.name)
        .await?;
    stage(store, imported, None, req.note).await
}
async fn check_update(
    State(store): State<Arc<SkillsStore>>,
    Extension(github): Extension<Github>,
    Path(name): Path<String>,
    Json(req): Json<CheckRequest>,
) -> Result<Response, ApiError> {
    let before = store.admin_snapshot();
    let skill = before.skills.get(&name).ok_or(SkillsError::Unavailable)?;
    let published = skill.published.as_ref().ok_or(SkillsError::Unavailable)?;
    if published.id != req.revision || skill.draft.is_some() {
        return Err(SkillsError::Conflict.into());
    }
    let source = published
        .source
        .clone()
        .ok_or(ImportError::Invalid("skill has no GitHub source"))?;
    if source.tracked_ref.is_none() && !source.tracks_default {
        return Err(ImportError::Invalid(
            "this import is pinned; remove and re-import with a tracked ref to update",
        )
        .into());
    }
    let imported = github.fetch(source.clone(), &name).await?;
    if name != imported.name {
        return Err(ImportError::Invalid("upstream changed the skill name").into());
    }
    if published.files.len() == imported.files.len()
        && imported.files.iter().all(|(path, file)| {
            published
                .files
                .get(path)
                .is_some_and(|old| old.data == file.data && old.executable == file.executable)
        })
    {
        let now = store.admin_snapshot();
        if now.skills.get(&name).is_none_or(|s| {
            s.draft.is_some() || s.published.as_ref().map(|p| p.id) != Some(req.revision)
        }) {
            return Err(SkillsError::Conflict.into());
        }
        return Ok(
            Json(json!({"skills":now.skills,"message":"No skill changes."})).into_response(),
        );
    }
    Ok(stage(store, imported, Some(req.revision), None)
        .await?
        .into_response())
}
