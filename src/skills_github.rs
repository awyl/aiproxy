//! Public GitHub repository imports through the pinned official skills library.
use crate::skills::{Files, SkillsError, Source, parse_metadata, valid_name, validate_files};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ImportError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Upstream(String),
    #[error(
        "Repository imports require Node.js 22.20+, git, and the skills-import helper with its dependencies. Run npm ci --ignore-scripts in skills-import."
    )]
    Unavailable,
    #[error("another skill discovery, import or update is running")]
    Busy,
    #[error(transparent)]
    Skill(#[from] SkillsError),
}
#[derive(Clone)]
pub(crate) struct Github {
    helper: PathBuf,
    gate: Arc<Semaphore>,
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Choice {
    pub name: String,
    pub description: String,
    pub path: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Discovery {
    pub commit: String,
    pub skills: Vec<Choice>,
}
pub(crate) struct Imported {
    pub name: String,
    pub files: Files,
    pub source: Source,
}
#[derive(Deserialize)]
struct Copied {
    name: String,
    files: Files,
    commit: String,
}
fn slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn folder(s: &str) -> bool {
    s == "." || (s.len() <= 512 && s.split('/').all(slug))
}
fn sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn repository(url: &str) -> Result<Source, ImportError> {
    let invalid =
        ImportError::Invalid("Enter a public HTTPS GitHub repository URL, not a skill-folder URL.");
    let path = url.strip_prefix("https://github.com/").ok_or(invalid)?;
    let (owner, repo) = path
        .trim_end_matches('/')
        .split_once('/')
        .ok_or(ImportError::Invalid(
            "expected https://github.com/OWNER/REPO",
        ))?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if !slug(owner) || !slug(repo) {
        return Err(ImportError::Invalid("invalid GitHub repository URL"));
    }
    Ok(Source {
        owner: owner.into(),
        repo: repo.into(),
        folder: ".".into(),
        tracked_ref: None,
        tracks_default: true,
        commit: String::new(),
    })
}

impl Github {
    pub fn new() -> Result<Self, std::io::Error> {
        let installed = std::env::current_exe()?
            .parent()
            .unwrap()
            .join("../share/aiproxy/skills-import/helper.mjs");
        let helper = if installed.is_file() {
            installed
        } else {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills-import/helper.mjs")
        };
        Ok(Self {
            helper,
            gate: Arc::new(Semaphore::new(1)),
        })
    }

    pub async fn discover(&self, url: &str) -> Result<Discovery, ImportError> {
        let source = repository(url)?;
        let result: Discovery = self.run(json!({"action":"discover","url":format!("https://github.com/{}/{}",source.owner,source.repo)})).await?;
        let mut seen = BTreeSet::new();
        if !sha(&result.commit)
            || result.skills.is_empty()
            || result.skills.len() > 256
            || result.skills.iter().any(|s| {
                !valid_name(&s.name)
                    || !folder(&s.path)
                    || s.description.len() > 16384
                    || !seen.insert((&s.name, &s.path))
            })
        {
            return Err(ImportError::Upstream(
                "Invalid discovered skill metadata.".into(),
            ));
        }
        Ok(result)
    }

    pub async fn import(
        &self,
        url: &str,
        commit: &str,
        path: &str,
        name: &str,
    ) -> Result<Imported, ImportError> {
        let mut source = repository(url)?;
        if !sha(commit) || !folder(path) || !valid_name(name) {
            return Err(ImportError::Invalid(
                "Invalid skill selection; use Find skills again.",
            ));
        }
        source.folder = path.into();
        source.commit = commit.into();
        self.copy(source, name, false).await
    }

    pub async fn fetch(&self, source: Source, name: &str) -> Result<Imported, ImportError> {
        if !source.tracks_default && source.tracked_ref.is_none() {
            return Err(ImportError::Invalid(
                "This skill is commit-pinned and has no moving update ref.",
            ));
        }
        self.copy(source, name, true).await
    }

    async fn copy(
        &self,
        mut source: Source,
        name: &str,
        update: bool,
    ) -> Result<Imported, ImportError> {
        if !slug(&source.owner)
            || !slug(&source.repo)
            || !folder(&source.folder)
            || !valid_name(name)
            || !sha(&source.commit)
            || source
                .tracked_ref
                .as_ref()
                .is_some_and(|r| r.len() > 256 || !r.split('/').all(slug))
        {
            return Err(ImportError::Invalid("invalid saved skill source"));
        }
        let copied: Copied = self
            .run(json!({
                "action":if update {"update"} else {"import"},
                "url":format!("https://github.com/{}/{}",source.owner,source.repo),
                "commit":source.commit, "ref":source.tracked_ref,
                "path":source.folder, "name":name,
            }))
            .await?;
        if copied.name != name
            || !sha(&copied.commit)
            || (!update && copied.commit != source.commit)
        {
            return Err(ImportError::Upstream(
                "Skill name or commit did not match the selection.".into(),
            ));
        }
        validate_files(&copied.files)?;
        let markdown = copied
            .files
            .get("SKILL.md")
            .ok_or(ImportError::Invalid("missing SKILL.md"))?
            .bytes()?;
        if parse_metadata(
            std::str::from_utf8(&markdown)
                .map_err(|_| ImportError::Invalid("SKILL.md must be UTF-8"))?,
        )?
        .name
            != name
        {
            return Err(ImportError::Invalid("upstream changed the skill name"));
        }
        source.commit = copied.commit;
        Ok(Imported {
            name: name.into(),
            files: copied.files,
            source,
        })
    }

    async fn run<T: DeserializeOwned>(&self, input: Value) -> Result<T, ImportError> {
        let _permit = self.gate.try_acquire().map_err(|_| ImportError::Busy)?;
        let dir = tempfile::tempdir().map_err(|_| ImportError::Unavailable)?;
        let path = std::env::join_paths(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .filter(|entry| entry.is_absolute()),
        )
        .map_err(|_| ImportError::Unavailable)?;
        let mut command = Command::new("node");
        command
            .arg(&self.helper)
            .current_dir(dir.path())
            .env_clear()
            .env("PATH", path)
            .env("HOME", dir.path())
            .env("TMPDIR", dir.path())
            .env("DISABLE_TELEMETRY", "1")
            .env("DO_NOT_TRACK", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            // Disable inherited hooks, credentials and submodule initialization.
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", "/dev/null")
            .env("GIT_CONFIG_KEY_1", "submodule.active")
            .env("GIT_CONFIG_VALUE_1", ":(exclude)**")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for key in [
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "NO_PROXY",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|_| ImportError::Unavailable)?;
        #[cfg(unix)]
        let _group = ProcessGroup(child.id().unwrap() as i32);
        let operation = async {
            let bytes = serde_json::to_vec(&input).unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(&bytes)
                .await
                .map_err(|_| ImportError::Unavailable)?;
            let mut stdout = child.stdout.take().unwrap().take(4 * 1024 * 1024 + 1);
            let mut output = Vec::new();
            stdout
                .read_to_end(&mut output)
                .await
                .map_err(|_| ImportError::Unavailable)?;
            if output.len() > 4 * 1024 * 1024 {
                return Err(ImportError::Upstream(
                    "Importer response exceeds the size limit.".into(),
                ));
            }
            let status = child.wait().await.map_err(|_| ImportError::Unavailable)?;
            if !status.success() {
                let error: Value =
                    serde_json::from_slice(&output).map_err(|_| ImportError::Unavailable)?;
                return Err(ImportError::Upstream(
                    error
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("Repository import failed.")
                        .chars()
                        .take(1024)
                        .collect(),
                ));
            }
            serde_json::from_slice(&output)
                .map_err(|_| ImportError::Upstream("Invalid importer response.".into()))
        };
        tokio::time::timeout(Duration::from_secs(120), operation)
            .await
            .map_err(|_| {
                ImportError::Upstream("Repository operation timed out after 120 seconds.".into())
            })?
    }
}

#[cfg(unix)]
struct ProcessGroup(i32);
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Each helper has its own process group. Also stop git descendants on cancellation.
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{Action, SkillsStore};
    use axum::{Router, body::Body, http::Request};
    use tower::ServiceExt;

    const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn fixture() -> (tempfile::TempDir, Github) {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("helper.mjs");
        std::fs::write(dir.path().join("mode"), "old").unwrap();
        std::fs::write(&helper,r#"
import {readFile,writeFile,access} from 'node:fs/promises';
import {spawn} from 'node:child_process';
const root=new URL('.',import.meta.url);
let text=''; for await(const chunk of process.stdin) text+=chunk;
const input=JSON.parse(text);
await writeFile(new URL('request.json',root),JSON.stringify(input));
let mode=(await readFile(new URL('mode',root),'utf8')).trim();
if(mode==='pause'){
 const descendant=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'});
 await writeFile(new URL('descendant',root),String(descendant.pid));
 await writeFile(new URL('started',root),String(process.pid));
 while(true){try{await access(new URL('resume',root));break;}catch{}await new Promise(r=>setTimeout(r,10));}
 descendant.kill('SIGTERM');
 mode='delete';
}
if(mode==='invalid'){console.log('{}');process.exit(0);}
if(mode==='flood'){await new Promise(done=>process.stdout.write('x'.repeat(5*1024*1024),done));process.exit(0);}
const commit='a'.repeat(40);
if(input.action==='discover'){
 console.log(JSON.stringify({commit,skills:[{name:'example',description:'Example',path:'skill'}]}));
}else{
 const data=Buffer.from('---\nname: example\ndescription: Example\n---\nBODY-TEST\n').toString('base64');
 const files={'SKILL.md':{data,executable:false,blob:null}};
 if(mode==='old') files['whole-app.md']={data:Buffer.from('Companion').toString('base64'),executable:false,blob:null};
 console.log(JSON.stringify({name:'example',commit,files}));
}
"#).unwrap();
        let importer = Github {
            helper,
            gate: Arc::new(Semaphore::new(1)),
        };
        (dir, importer)
    }
    async fn request(app: &Router, path: &str, body: Value) -> (u16, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::post(path)
                    .header("authorization", "Bearer admin")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    async fn started(dir: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    #[test]
    fn only_public_repository_urls_and_safe_paths_are_accepted() {
        for url in [
            "file:///etc",
            "http://github.com/a/b",
            "https://user:token@github.com/a/b",
            "https://github.com/a/b/tree/main/skill",
            "https://evil.test/a/b",
        ] {
            assert!(repository(url).is_err(), "{url}");
        }
        assert_eq!(
            repository("https://github.com/test/repo.git/")
                .unwrap()
                .repo,
            "repo"
        );
        assert!(!folder("../secret"));
        assert!(!folder("folder/../secret"));
        assert!(folder("."));
    }
    #[tokio::test]
    async fn discovery_import_update_and_open_draft_race() {
        let (dir, github) = fixture();
        let store = Arc::new(SkillsStore::open(dir.path().join("skills.json")).unwrap());
        let app = crate::dashboard_auth::DashboardAuth::new(Some("admin".into())).api(
            crate::skills_api::router(
                store.clone(),
                crate::skills_mcp::DraftTokens::default(),
                github,
            ),
        );
        let (status, found) = request(
            &app,
            "/api/skills/discover",
            json!({"url":"https://github.com/test/repo"}),
        )
        .await;
        assert_eq!(status, 200, "{found}");
        assert_eq!(found["commit"], COMMIT);
        assert_eq!(found["skills"][0]["path"], "skill");
        assert!(!found.to_string().contains("BODY-TEST"));
        let (status,snapshot)=request(&app,"/api/skills/import",json!({"url":"https://github.com/test/repo","commit":COMMIT,"path":"skill","name":"example","note":"Check local tools"})).await;
        assert_eq!(status, 200, "{snapshot}");
        assert_eq!(
            snapshot["skills"]["example"]["draft"]["note"],
            "Check local tools"
        );
        let draft = snapshot["skills"]["example"]["draft"]["id"]
            .as_u64()
            .unwrap();
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
        let check = json!({"revision":revision});
        let (status, body) = request(&app, "/api/skills/example/check-update", check.clone()).await;
        assert_eq!(status, 200);
        assert_eq!(body["message"], "No skill changes.");
        std::fs::write(dir.path().join("mode"), "invalid").unwrap();
        assert_eq!(
            request(&app, "/api/skills/example/check-update", check.clone())
                .await
                .0,
            502
        );
        assert!(store.admin_snapshot().skills["example"].draft.is_none());
        std::fs::write(dir.path().join("mode"), "pause").unwrap();
        let (app2, check2) = (app.clone(), check.clone());
        let pending = tokio::spawn(async move {
            request(&app2, "/api/skills/example/check-update", check2).await
        });
        started(dir.path()).await;
        let agent = store
            .save_agent_draft(
                "example",
                "---\nname: example\ndescription: Example\n---\nAgent edit".into(),
                Some(revision),
                None,
            )
            .unwrap();
        std::fs::write(dir.path().join("resume"), "").unwrap();
        assert_eq!(pending.await.unwrap().0, 409);
        assert_eq!(
            store.admin_snapshot().skills["example"]
                .draft
                .as_ref()
                .unwrap()
                .id,
            agent
        );
        store
            .apply(
                "example",
                Action::Reject {
                    draft_revision: agent,
                },
            )
            .unwrap();
        std::fs::write(dir.path().join("mode"), "delete").unwrap();
        let (status, _) = request(&app, "/api/skills/example/check-update", check).await;
        assert_eq!(status, 200);
        let snapshot = store.admin_snapshot();
        let draft = snapshot.skills["example"].draft.as_ref().unwrap();
        assert_eq!(draft.note.as_deref(), Some("Check local tools"));
        assert!(!draft.files.contains_key("whole-app.md"));
        assert!(store.read_file("example", revision, "whole-app.md").is_ok());
    }
    #[tokio::test]
    async fn legacy_sources_keep_their_tracked_branch_and_imports_verify_the_commit() {
        let (dir, github) = fixture();
        let source = Source {
            owner: "test".into(),
            repo: "repo".into(),
            folder: "skill".into(),
            tracked_ref: Some("feature/port".into()),
            tracks_default: false,
            commit: COMMIT.into(),
        };
        let fetched = github.fetch(source.clone(), "example").await.unwrap();
        assert_eq!(fetched.source, source);
        let request: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("request.json")).unwrap())
                .unwrap();
        assert_eq!(request["ref"], "feature/port");
        assert_eq!(request["path"], "skill");
        assert!(
            github
                .import(
                    "https://github.com/test/repo",
                    &"b".repeat(40),
                    "skill",
                    "example"
                )
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn oversized_helper_output_is_rejected_and_job_slot_is_released() {
        let (dir, github) = fixture();
        std::fs::write(dir.path().join("mode"), "flood").unwrap();
        let error = github
            .discover("https://github.com/test/repo")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("size limit"), "{error}");
        std::fs::write(dir.path().join("mode"), "old").unwrap();
        assert!(
            github
                .discover("https://github.com/test/repo")
                .await
                .is_ok()
        );
    }
    #[tokio::test]
    async fn unavailable_helper_fails_only_repository_operations() {
        let github = Github {
            helper: "/missing/aiproxy/skills-helper.mjs".into(),
            gate: Arc::new(Semaphore::new(1)),
        };
        assert!(matches!(
            github.discover("https://github.com/test/repo").await,
            Err(ImportError::Unavailable)
        ));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_import_kills_helper_and_descendants_and_releases_job_slot() {
        let (dir, github) = fixture();
        std::fs::write(dir.path().join("mode"), "pause").unwrap();
        let worker = github.clone();
        let pending =
            tokio::spawn(async move { worker.discover("https://github.com/test/repo").await });
        started(dir.path()).await;
        let pid: i32 = std::fs::read_to_string(dir.path().join("started"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(matches!(
            github.discover("https://github.com/test/repo").await,
            Err(ImportError::Busy)
        ));
        let descendant: i32 = std::fs::read_to_string(dir.path().join("descendant"))
            .unwrap()
            .parse()
            .unwrap();
        pending.abort();
        let _ = pending.await;
        let stopped = tokio::time::timeout(Duration::from_secs(5), async {
            for pid in [pid, descendant] {
                loop {
                    let running = unsafe { libc::kill(pid, 0) } == 0;
                    // A killed orphan can briefly remain a zombie until the host reaps it.
                    let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                        .is_ok_and(|stat| stat.split_whitespace().nth(2) == Some("Z"));
                    if !running || zombie {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        })
        .await;
        unsafe {
            libc::kill(descendant, libc::SIGKILL);
        }
        stopped.unwrap();
        std::fs::write(dir.path().join("mode"), "old").unwrap();
        assert!(
            github
                .discover("https://github.com/test/repo")
                .await
                .is_ok()
        );
    }
}
