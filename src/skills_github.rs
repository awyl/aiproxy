#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};
    use serde_json::json;
    const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TREE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const FOLDER: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const MARKDOWN: &str = "dddddddddddddddddddddddddddddddddddddddd";
    const COMPANION: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    #[tokio::test]
    async fn skills_github_update_race() {
        use crate::skills::{Action, SkillsStore};
        use std::sync::atomic::{AtomicU8, Ordering};
        use tokio::sync::Notify;

        const MD: &str = "---\nname: example\ndescription: Example\n---\nBODY-TEST\n";
        let mode = Arc::new(AtomicU8::new(0));
        let started = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let fixture = Router::new()
            .route("/repos/test/repo/commits/main", get(|| async { Json(json!({"sha":COMMIT,"commit":{"tree":{"sha":TREE}}})) }))
            .route(&format!("/repos/test/repo/git/trees/{TREE}"), get(|| async { Json(json!({"truncated":false,"tree":[{"path":"skill","mode":"040000","type":"tree","sha":FOLDER}]})) }))
            .route(&format!("/repos/test/repo/git/trees/{FOLDER}"), get({
                let (mode, started, resume) = (mode.clone(), started.clone(), resume.clone());
                move || { let (mode, started, resume) = (mode.clone(), started.clone(), resume.clone()); async move {
                    let mode = mode.load(Ordering::SeqCst);
                    if mode == 2 { started.notify_one(); resume.notified().await; }
                    let mut tree = vec![json!({"path":"SKILL.md","mode":"100644","type":"blob","sha":MARKDOWN,"size":MD.len()})];
                    if mode == 0 { tree.push(json!({"path":"whole-app.md","mode":"100644","type":"blob","sha":COMPANION,"size":4})); }
                    Json(json!({"truncated":mode == 3,"tree":tree}))
                }}
            }))
            .route(&format!("/repos/test/repo/git/blobs/{MARKDOWN}"), get(|| async { MD }))
            .route(&format!("/repos/test/repo/git/blobs/{COMPANION}"), get(|| async { "test" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let github = Github::new_at(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        )
        .unwrap();
        let upstream = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SkillsStore::open(dir.path().join("skills.json")).unwrap());
        let app = crate::skills_api::router(
            store.clone(),
            crate::skills_mcp::Credentials {
                admin: "admin".into(),
                read: "reader".into(),
                author: None,
            },
            github,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/skills", listener.local_addr().unwrap());
        let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let imported = http
            .post(format!("{base}/import"))
            .bearer_auth("admin")
            .json(&json!({"url":"https://github.com/test/repo/tree/main/skill"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(imported.status(), 200);
        let draft = store.admin_snapshot().skills["example"]
            .draft
            .as_ref()
            .unwrap()
            .id;
        let edited = store
            .apply(
                "example",
                Action::SaveTextDraft {
                    content: MD.into(),
                    note: Some("local setup".into()),
                    base_revision: None,
                    expected_draft_revision: Some(draft),
                },
            )
            .unwrap();
        let draft = edited.skills["example"].draft.as_ref().unwrap().id;
        let published = store
            .apply(
                "example",
                Action::Publish {
                    draft_revision: draft,
                    base_revision: None,
                },
            )
            .unwrap();
        let revision = published.skills["example"].published.as_ref().unwrap().id;
        store
            .apply(
                "example",
                Action::SetEnabled {
                    revision,
                    enabled: true,
                },
            )
            .unwrap();
        let check = || {
            http.post(format!("{base}/example/check-update"))
                .bearer_auth("admin")
                .json(&json!({"revision":revision}))
        };
        let unchanged: serde_json::Value = check()
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(unchanged["message"], "No skill changes.");
        mode.store(3, Ordering::SeqCst);
        assert_eq!(check().send().await.unwrap().status(), 502);
        assert!(store.admin_snapshot().skills["example"].draft.is_none());
        mode.store(2, Ordering::SeqCst);
        let request = check();
        let update = tokio::spawn(async move { request.send().await.unwrap() });
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        let agent_draft = store
            .save_agent_draft(
                "example",
                MD.replace("BODY-TEST", "AGENT-DRAFT"),
                Some(revision),
                None,
            )
            .unwrap();
        resume.notify_one();
        assert_eq!(update.await.unwrap().status(), 409);
        assert_eq!(
            store.admin_snapshot().skills["example"]
                .draft
                .as_ref()
                .unwrap()
                .id,
            agent_draft
        );
        store
            .apply(
                "example",
                Action::Reject {
                    draft_revision: agent_draft,
                },
            )
            .unwrap();
        mode.store(1, Ordering::SeqCst);
        check().send().await.unwrap().error_for_status().unwrap();
        let snapshot = store.admin_snapshot();
        let update = snapshot.skills["example"].draft.as_ref().unwrap();
        assert_eq!(update.note.as_deref(), Some("local setup"));
        assert!(!update.files.contains_key("whole-app.md"));
        assert_eq!(
            store
                .read_file("example", revision, "whole-app.md")
                .unwrap()
                .bytes()
                .unwrap(),
            b"test"
        );
        proxy.abort();
        upstream.abort();
    }

    #[tokio::test]
    async fn skills_github_pins_snapshot() {
        let app = Router::new()
            .route("/repos/test/repo/commits/main", get(|| async {Json(json!({"sha":COMMIT,"commit":{"tree":{"sha":TREE}}}))}))
            .route(&format!("/repos/test/repo/git/trees/{TREE}"), get(|| async {Json(json!({"truncated":false,"tree":[{"path":"skill","mode":"040000","type":"tree","sha":FOLDER}]}))}))
            .route(&format!("/repos/test/repo/git/trees/{FOLDER}"), get(|axum::extract::RawQuery(q):axum::extract::RawQuery| async move {
                assert_eq!(q.as_deref(),Some("recursive=1"));
                Json(json!({"truncated":false,"tree":[{"path":"SKILL.md","mode":"100644","type":"blob","sha":MARKDOWN,"size":53},{"path":"whole-app.md","mode":"100755","type":"blob","sha":COMPANION,"size":4}]}))
            }))
            .route(&format!("/repos/test/repo/git/blobs/{MARKDOWN}"), get(|| async {"---
name: example
description: Example
---
BODY-TEST
"}))
            .route(&format!("/repos/test/repo/git/blobs/{COMPANION}"), get(|| async {"test"}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let github = Github::new_at(base.parse().unwrap()).unwrap();
        let target = Target::parse("https://github.com/test/repo/tree/main/skill", None).unwrap();
        let import = github.fetch(target.source, target.resolve).await.unwrap();
        assert_eq!(import.source.commit, COMMIT);
        assert_eq!(import.name, "example");
        assert!(import.files["whole-app.md"].executable);
        assert_eq!(import.files["whole-app.md"].bytes().unwrap(), b"test");
        task.abort();
    }
}

use crate::skills::{
    Files, MAX_BUNDLE, MAX_FILE, MAX_FILES, SkillFile, SkillsError, Source, parse_metadata,
    validate_files,
};
use reqwest::{Client, Url};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ImportError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("GitHub returned an incomplete or invalid snapshot, or is unavailable")]
    Upstream,
    #[error("another GitHub import or update is running")]
    Busy,
    #[error(transparent)]
    Skill(#[from] SkillsError),
}
pub(crate) struct Target {
    pub source: Source,
    pub resolve: Option<String>,
}
fn slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn git_ref(s: &str) -> bool {
    s.len() <= 256 && s.split('/').all(slug)
}
fn sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
impl Target {
    pub fn parse(url: &str, tracked_ref: Option<String>) -> Result<Self, ImportError> {
        let invalid = ImportError::Invalid(
            "expected a public https://github.com/OWNER/REPO/tree/REF/FOLDER URL",
        );
        let path = url.strip_prefix("https://github.com/").ok_or_else(|| {
            ImportError::Invalid("only public https://github.com folder URLs are supported")
        })?;
        if path.len() > 1024 {
            return Err(invalid);
        }
        let mut parts = path.trim_end_matches('/').splitn(4, '/');
        let (Some(owner), Some(repo), Some("tree"), Some(tail)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid);
        };
        let (short_ref, _) = tail
            .split_once('/')
            .ok_or_else(|| ImportError::Invalid("a skill folder is required"))?;
        if !slug(owner)
            || !slug(repo)
            || !slug(short_ref)
            || tracked_ref.as_deref().is_some_and(|r| !git_ref(r))
        {
            return Err(invalid);
        }
        let url_ref = tracked_ref
            .as_deref()
            .filter(|r| {
                r.contains('/') && tail.strip_prefix(*r).is_some_and(|s| s.starts_with('/'))
            })
            .unwrap_or(short_ref);
        let folder = &tail[url_ref.len() + 1..];
        if folder.len() > 512 || !folder.split('/').all(slug) {
            return Err(invalid);
        }
        let pinned = sha(url_ref);
        let resolve = if url_ref == "HEAD" {
            tracked_ref.clone()
        } else {
            Some(url_ref.into())
        };
        let tracks_default = url_ref == "HEAD" && tracked_ref.is_none();
        let inferred_ref = (!pinned && url_ref != "HEAD").then(|| url_ref.to_owned());
        let tracked_ref = tracked_ref.or(inferred_ref);
        Ok(Self {
            source: Source {
                owner: owner.into(),
                repo: repo.into(),
                folder: folder.into(),
                tracks_default,
                tracked_ref,
                commit: String::new(),
            },
            resolve,
        })
    }
}
#[derive(Clone)]
pub(crate) struct Github {
    client: Client,
    api: Url,
    gate: Arc<Semaphore>,
}
pub(crate) struct Imported {
    pub name: String,
    pub files: Files,
    pub source: Source,
}
#[derive(Deserialize)]
struct Commit {
    sha: String,
    commit: CommitObject,
}
#[derive(Deserialize)]
struct CommitObject {
    tree: ObjectId,
}
#[derive(Deserialize)]
struct ObjectId {
    sha: String,
}
#[derive(Deserialize)]
struct Tree {
    truncated: bool,
    tree: Vec<Entry>,
}
#[derive(Deserialize)]
struct Entry {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
    size: Option<usize>,
}
impl Github {
    pub fn new() -> Result<Self, reqwest::Error> {
        Self::new_at(Url::parse("https://api.github.com").unwrap())
    }
    fn new_at(api: Url) -> Result<Self, reqwest::Error> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(12))
            .user_agent("aiproxy-skills")
            .build()?;
        Ok(Self {
            client,
            api,
            gate: Arc::new(Semaphore::new(1)),
        })
    }
    async fn get(
        &self,
        source: &Source,
        segments: &[&str],
        query: Option<(&str, &str)>,
        raw: bool,
        limit: usize,
    ) -> Result<Vec<u8>, ImportError> {
        let mut url = self.api.clone();
        url.path_segments_mut()
            .unwrap()
            .extend(["repos", &source.owner, &source.repo])
            .extend(segments.iter().copied());
        if let Some((k, v)) = query {
            url.query_pairs_mut().append_pair(k, v);
        }
        let response = self
            .client
            .get(url)
            .header(
                "Accept",
                if raw {
                    "application/vnd.github.raw+json"
                } else {
                    "application/vnd.github+json"
                },
            )
            .send()
            .await
            .map_err(|_| ImportError::Upstream)?;
        bounded_body(response, limit)
            .await
            .ok_or(ImportError::Upstream)
    }
    async fn tree(
        &self,
        source: &Source,
        id: &str,
        recursive: bool,
    ) -> Result<Vec<Entry>, ImportError> {
        if !sha(id) {
            return Err(ImportError::Upstream);
        }
        let bytes = self
            .get(
                source,
                &["git", "trees", id],
                recursive.then_some(("recursive", "1")),
                false,
                2 * 1024 * 1024,
            )
            .await?;
        let tree: Tree = serde_json::from_slice(&bytes).map_err(|_| ImportError::Upstream)?;
        if tree.truncated {
            return Err(ImportError::Upstream);
        }
        Ok(tree.tree)
    }
    pub async fn fetch(
        &self,
        source: Source,
        resolve: Option<String>,
    ) -> Result<Imported, ImportError> {
        let _permit = self.gate.try_acquire().map_err(|_| ImportError::Busy)?;
        tokio::time::timeout(Duration::from_secs(45), self.fetch_inner(source, resolve))
            .await
            .map_err(|_| ImportError::Upstream)?
    }
    async fn fetch_inner(
        &self,
        mut source: Source,
        resolve: Option<String>,
    ) -> Result<Imported, ImportError> {
        if !slug(&source.owner)
            || !slug(&source.repo)
            || !source.folder.split('/').all(slug)
            || resolve.as_deref().is_some_and(|r| !git_ref(r))
        {
            return Err(ImportError::Invalid("invalid saved GitHub source"));
        }
        let commit: Commit = if let Some(r) = resolve.as_deref() {
            let bytes = self
                .get(&source, &["commits", r], None, false, 1024 * 1024)
                .await?;
            serde_json::from_slice(&bytes).map_err(|_| ImportError::Upstream)?
        } else {
            let bytes = self
                .get(
                    &source,
                    &["commits"],
                    Some(("per_page", "1")),
                    false,
                    1024 * 1024,
                )
                .await?;
            serde_json::from_slice::<Vec<Commit>>(&bytes)
                .map_err(|_| ImportError::Upstream)?
                .into_iter()
                .next()
                .ok_or(ImportError::Upstream)?
        };
        if !sha(&commit.sha)
            || resolve
                .as_deref()
                .is_some_and(|r| sha(r) && !r.eq_ignore_ascii_case(&commit.sha))
        {
            return Err(ImportError::Upstream);
        }
        let mut tree = commit.commit.tree.sha;
        for component in source.folder.split('/') {
            tree = self
                .tree(&source, &tree, false)
                .await?
                .into_iter()
                .find(|e| e.path == component && e.kind == "tree" && e.mode == "040000")
                .ok_or(ImportError::Invalid(
                    "skill folder not found at resolved commit",
                ))?
                .sha;
        }
        let entries = self.tree(&source, &tree, true).await?;
        let mut files = Files::new();
        let mut total = 0;
        for entry in entries {
            if entry.kind == "tree" && entry.mode == "040000" {
                continue;
            }
            if entry.kind != "blob"
                || !matches!(entry.mode.as_str(), "100644" | "100755")
                || !sha(&entry.sha)
            {
                return Err(ImportError::Invalid(
                    "skill folder must contain only regular files (no symlinks or submodules)",
                ));
            }
            let size = entry
                .size
                .filter(|&n| n <= MAX_FILE)
                .ok_or(ImportError::Invalid("invalid GitHub file size"))?;
            total += size;
            if total > MAX_BUNDLE || files.len() >= MAX_FILES || files.contains_key(&entry.path) {
                return Err(ImportError::Invalid(
                    "skill folder exceeds limits or has duplicate paths",
                ));
            }
            let bytes = self
                .get(&source, &["git", "blobs", &entry.sha], None, true, size)
                .await?;
            if bytes.len() != size {
                return Err(ImportError::Upstream);
            }
            files.insert(
                entry.path,
                SkillFile::new(&bytes, entry.mode == "100755", Some(entry.sha)),
            );
        }
        validate_files(&files)?;
        let text = files
            .get("SKILL.md")
            .ok_or(ImportError::Invalid("folder must contain SKILL.md"))?
            .bytes()?;
        let name = parse_metadata(
            std::str::from_utf8(&text)
                .map_err(|_| ImportError::Invalid("SKILL.md must be UTF-8"))?,
        )?
        .name;
        source.commit = commit.sha;
        Ok(Imported {
            name,
            files,
            source,
        })
    }
}

pub(crate) async fn bounded_body(mut response: reqwest::Response, limit: usize) -> Option<Vec<u8>> {
    if !response.status().is_success()
        || response.content_length().is_some_and(|n| n > limit as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > limit {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}
