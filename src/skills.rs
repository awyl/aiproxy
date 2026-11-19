use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read},
    path::PathBuf,
    sync::Mutex,
};

pub const MAX_FILE: usize = 512 * 1024;
pub const MAX_BUNDLE: usize = 2 * 1024 * 1024;
pub const MAX_FILES: usize = 128;
const MAX_STATE: usize = 32 * 1024 * 1024;
const MAX_SKILLS: usize = 64;
pub type Files = BTreeMap<String, SkillFile>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillFile {
    pub data: String,
    pub executable: bool,
    pub blob: Option<String>,
}
impl SkillFile {
    pub fn new(bytes: &[u8], executable: bool, blob: Option<String>) -> Self {
        Self {
            data: STANDARD.encode(bytes),
            executable,
            blob,
        }
    }
    pub fn bytes(&self) -> Result<Vec<u8>, SkillsError> {
        if self.data.len() > MAX_FILE.div_ceil(3) * 4 {
            return Err(invalid("file too large"));
        }
        let bytes = STANDARD
            .decode(&self.data)
            .map_err(|_| invalid("invalid base64"))?;
        if bytes.len() > MAX_FILE {
            return Err(invalid("file too large"));
        }
        Ok(bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub owner: String,
    pub repo: String,
    pub folder: String,
    pub tracked_ref: Option<String>,
    #[serde(default)]
    pub tracks_default: bool,
    pub commit: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: u64,
    pub base_revision: Option<u64>,
    pub files: Files,
    pub note: Option<String>,
    pub metadata: Metadata,
    pub source: Option<Source>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub enabled: bool,
    pub draft: Option<Snapshot>,
    pub published: Option<Snapshot>,
    pub previous: Option<Snapshot>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSnapshot {
    version: u8,
    next_id: u64,
    pub skills: BTreeMap<String, Skill>,
}
#[derive(Debug, Serialize)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub revision: u64,
}
#[derive(Debug, Serialize)]
pub struct LoadedSkill {
    pub name: String,
    pub revision: u64,
    pub content: String,
    pub note: Option<String>,
    pub companion_paths: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Bundle {
    pub revision: u64,
    #[serde(deserialize_with = "unique_files")]
    pub files: Files,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    SaveTextDraft {
        content: String,
        note: Option<String>,
        base_revision: Option<u64>,
        expected_draft_revision: Option<u64>,
    },
    Publish {
        draft_revision: u64,
        base_revision: Option<u64>,
    },
    Reject {
        draft_revision: u64,
    },
    SetEnabled {
        revision: u64,
        enabled: bool,
    },
    Rollback {
        revision: u64,
    },
    Remove {
        revision: Option<u64>,
        draft_revision: Option<u64>,
    },
}
#[derive(Debug, thiserror::Error)]
pub enum SkillsError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("skill or file not available")]
    Unavailable,
    #[error("skill changed; reload and try again")]
    Conflict,
    #[error("could not persist skill state")]
    Persist(#[source] io::Error),
}
use SkillsError::Invalid as invalid;

pub struct SkillsStore {
    path: PathBuf,
    state: Mutex<AdminSnapshot>,
}
impl SkillsStore {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let state = match File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take((MAX_STATE + 1) as u64).read_to_end(&mut bytes)?;
                if bytes.len() > MAX_STATE {
                    return Err(crate::aliases::invalid("skills state too large"));
                }
                let state = serde_json::from_slice(&bytes)?;
                validate_state(&state).map_err(|e| crate::aliases::invalid(e.to_string()))?;
                state
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => AdminSnapshot {
                version: 1,
                next_id: 1,
                skills: BTreeMap::new(),
            },
            Err(e) => return Err(e),
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }
    pub fn admin_snapshot(&self) -> AdminSnapshot {
        self.state.lock().unwrap().clone()
    }
    pub fn catalog(&self) -> Vec<CatalogEntry> {
        self.state
            .lock()
            .unwrap()
            .skills
            .values()
            .filter_map(|s| {
                let p = s.published.as_ref().filter(|_| s.enabled)?;
                let Metadata {
                    name,
                    description,
                    license,
                    compatibility,
                } = p.metadata.clone();
                Some(CatalogEntry {
                    name,
                    description,
                    license,
                    compatibility,
                    revision: p.id,
                })
            })
            .collect()
    }
    pub fn load(&self, name: &str) -> Result<LoadedSkill, SkillsError> {
        let state = self.state.lock().unwrap();
        let p = current(&state, name, None)?;
        Ok(LoadedSkill {
            name: name.into(),
            revision: p.id,
            content: markdown(&p.files)?,
            note: p.note.clone(),
            companion_paths: p
                .files
                .keys()
                .filter(|s| *s != "SKILL.md")
                .cloned()
                .collect(),
        })
    }
    pub fn read_file(
        &self,
        name: &str,
        revision: u64,
        path: &str,
    ) -> Result<SkillFile, SkillsError> {
        let state = self.state.lock().unwrap();
        current(&state, name, Some(revision))?
            .files
            .get(path)
            .cloned()
            .ok_or(SkillsError::Unavailable)
    }
    pub fn bundle(&self, name: &str, revision: u64) -> Result<Bundle, SkillsError> {
        let state = self.state.lock().unwrap();
        let p = current(&state, name, Some(revision))?;
        Ok(Bundle {
            revision,
            files: p.files.clone(),
        })
    }
    pub fn apply(&self, name: &str, action: Action) -> Result<AdminSnapshot, SkillsError> {
        self.change(name, |state| {
            let id = state.next_id;
            if let Action::SaveTextDraft {
                content,
                note,
                base_revision,
                expected_draft_revision,
            } = action
            {
                return stage_text(
                    state,
                    name,
                    content,
                    Some(note),
                    base_revision,
                    expected_draft_revision,
                );
            }
            let skill = state.skills.get_mut(name).ok_or(SkillsError::Unavailable)?;
            let published = skill.published.as_ref().map(|p| p.id);
            let draft = skill.draft.as_ref().map(|p| p.id);
            match action {
                Action::SaveTextDraft { .. } => unreachable!(),
                Action::Publish {
                    draft_revision,
                    base_revision,
                } => {
                    let mut next = skill
                        .draft
                        .as_ref()
                        .filter(|d| {
                            d.id == draft_revision
                                && d.base_revision == base_revision
                                && published == base_revision
                        })
                        .ok_or(SkillsError::Conflict)?
                        .clone();
                    next.id = id;
                    skill.previous = skill.published.replace(next);
                    skill.draft = None;
                }
                Action::Reject { draft_revision } => {
                    check(Some(draft_revision), draft)?;
                    skill.draft = None;
                    if published.is_none() {
                        state.skills.remove(name);
                    }
                }
                Action::SetEnabled { revision, enabled } => {
                    check(Some(revision), published)?;
                    skill.enabled = enabled;
                }
                Action::Rollback { revision } => {
                    check(Some(revision), published)?;
                    let mut p = skill.previous.take().ok_or(SkillsError::Unavailable)?;
                    p.id = id;
                    p.base_revision = published;
                    skill.previous = skill.published.replace(p);
                }
                Action::Remove {
                    revision,
                    draft_revision,
                } => {
                    check(revision, published)?;
                    check(draft_revision, draft)?;
                    state.skills.remove(name);
                }
            }
            Ok(())
        })
    }
    pub fn save_agent_draft(
        &self,
        name: &str,
        content: String,
        base: Option<u64>,
        draft: Option<u64>,
    ) -> Result<u64, SkillsError> {
        let state = self.change(name, |state| {
            if state
                .skills
                .get(name)
                .is_some_and(|s| s.published.is_some() && !s.enabled)
            {
                return Err(SkillsError::Unavailable);
            }
            stage_text(state, name, content, None, base, draft)
        })?;
        Ok(state.skills[name].draft.as_ref().unwrap().id)
    }
    pub fn stage_import(
        &self,
        name: &str,
        files: Files,
        source: Source,
        base: Option<u64>,
    ) -> Result<AdminSnapshot, SkillsError> {
        self.change(name, |state| {
            let id = state.next_id;
            let skill = draft_target(state, name, base, None)?;
            let note = skill.published.as_ref().and_then(|p| p.note.clone());
            skill.draft = Some(snapshot(name, id, base, files, note, Some(source))?);
            Ok(())
        })
    }
    fn change(
        &self,
        name: &str,
        operation: impl FnOnce(&mut AdminSnapshot) -> Result<(), SkillsError>,
    ) -> Result<AdminSnapshot, SkillsError> {
        if !valid_name(name) {
            return Err(invalid("invalid skill name"));
        }
        let mut live = self.state.lock().unwrap();
        let mut candidate = live.clone();
        operation(&mut candidate)?;
        candidate.next_id = candidate
            .next_id
            .checked_add(1)
            .ok_or(invalid("revision IDs exhausted"))?;
        if serde_json::to_vec_pretty(&candidate)
            .map_err(|e| SkillsError::Persist(e.into()))?
            .len()
            > MAX_STATE
        {
            return Err(invalid("skills registry is full"));
        }
        crate::aliases::persist_json(&self.path, &candidate).map_err(SkillsError::Persist)?;
        *live = candidate.clone();
        Ok(candidate)
    }
}
fn check(expected: Option<u64>, actual: Option<u64>) -> Result<(), SkillsError> {
    if expected == actual {
        Ok(())
    } else {
        Err(SkillsError::Conflict)
    }
}
fn current<'a>(
    state: &'a AdminSnapshot,
    name: &str,
    revision: Option<u64>,
) -> Result<&'a Snapshot, SkillsError> {
    let p = state
        .skills
        .get(name)
        .filter(|s| s.enabled)
        .and_then(|s| s.published.as_ref())
        .ok_or(SkillsError::Unavailable)?;
    if revision.is_some_and(|r| r != p.id) {
        return Err(SkillsError::Conflict);
    }
    Ok(p)
}
fn draft_target<'a>(
    state: &'a mut AdminSnapshot,
    name: &str,
    base: Option<u64>,
    draft: Option<u64>,
) -> Result<&'a mut Skill, SkillsError> {
    if !state.skills.contains_key(name) && state.skills.len() >= MAX_SKILLS {
        return Err(invalid("skills registry is full"));
    }
    let s = state.skills.entry(name.into()).or_default();
    check(base, s.published.as_ref().map(|p| p.id))?;
    check(draft, s.draft.as_ref().map(|d| d.id))?;
    if s.draft.as_ref().is_some_and(|d| d.base_revision != base) {
        return Err(SkillsError::Conflict);
    }
    Ok(s)
}
fn stage_text(
    state: &mut AdminSnapshot,
    name: &str,
    content: String,
    note: Option<Option<String>>,
    base: Option<u64>,
    draft: Option<u64>,
) -> Result<(), SkillsError> {
    let id = state.next_id;
    let skill = draft_target(state, name, base, draft)?;
    let existing = skill.draft.as_ref().or(skill.published.as_ref());
    let mut files = existing.map(|p| p.files.clone()).unwrap_or_default();
    if files
        .get("SKILL.md")
        .map(|f| f.bytes())
        .transpose()?
        .as_deref()
        != Some(content.as_bytes())
    {
        let executable = files.get("SKILL.md").is_some_and(|f| f.executable);
        files.insert(
            "SKILL.md".into(),
            SkillFile::new(content.as_bytes(), executable, None),
        );
    }
    let source = existing.and_then(|p| p.source.clone());
    let note = note.unwrap_or_else(|| existing.and_then(|p| p.note.clone()));
    skill.draft = Some(snapshot(name, id, base, files, note, source)?);
    Ok(())
}
fn snapshot(
    name: &str,
    id: u64,
    base_revision: Option<u64>,
    files: Files,
    note: Option<String>,
    source: Option<Source>,
) -> Result<Snapshot, SkillsError> {
    let metadata = validate_snapshot(name, &files, note.as_deref())?;
    Ok(Snapshot {
        id,
        base_revision,
        files,
        note,
        metadata,
        source,
    })
}
fn validate_snapshot(
    name: &str,
    files: &Files,
    note: Option<&str>,
) -> Result<Metadata, SkillsError> {
    validate_files(files)?;
    if note.is_some_and(|s| s.len() > 8192) {
        return Err(invalid("note too large"));
    }
    let metadata = parse_metadata(&markdown(files)?)?;
    if metadata.name != name {
        return Err(invalid("frontmatter name must match skill name"));
    }
    Ok(metadata)
}
fn validate_state(state: &AdminSnapshot) -> Result<(), SkillsError> {
    if state.version != 1 || state.next_id == 0 || state.skills.len() > MAX_SKILLS {
        return Err(invalid("invalid skills registry"));
    }
    let mut ids = BTreeSet::new();
    for (name, skill) in &state.skills {
        if (skill.enabled || skill.previous.is_some() || skill.draft.is_none())
            && skill.published.is_none()
        {
            return Err(invalid("invalid skill state"));
        }
        for p in [&skill.draft, &skill.published, &skill.previous]
            .into_iter()
            .flatten()
        {
            if p.id == 0
                || p.id >= state.next_id
                || !ids.insert(p.id)
                || p.base_revision
                    .is_some_and(|base| base == 0 || base >= p.id)
            {
                return Err(invalid("invalid revision id"));
            }
            if validate_snapshot(name, &p.files, p.note.as_deref())? != p.metadata {
                return Err(invalid("invalid skill metadata"));
            }
        }
    }
    Ok(())
}
pub(crate) fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && name.split('-').all(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}
fn markdown(files: &Files) -> Result<String, SkillsError> {
    let file = files
        .get("SKILL.md")
        .ok_or(invalid("SKILL.md is required"))?;
    String::from_utf8(file.bytes()?).map_err(|_| invalid("SKILL.md must be UTF-8"))
}
pub fn parse_metadata(content: &str) -> Result<Metadata, SkillsError> {
    let mut lines = content.lines();
    if lines.next() != Some("---") {
        return Err(invalid("expected SKILL.md frontmatter"));
    }
    let mut yaml = String::new();
    for line in lines {
        if line == "---" {
            let m: Metadata =
                serde_yaml::from_str(&yaml).map_err(|_| invalid("invalid SKILL.md frontmatter"))?;
            if !valid_name(&m.name)
                || m.description.trim().is_empty()
                || m.description.len() > 1024
                || [&m.license, &m.compatibility]
                    .into_iter()
                    .flatten()
                    .any(|s| s.len() > 1024)
            {
                return Err(invalid("invalid skill metadata"));
            }
            return Ok(m);
        }
        if yaml.len() + line.len() + 1 > 8192 {
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    Err(invalid("unterminated or oversized frontmatter"))
}
pub fn validate_files(files: &Files) -> Result<(), SkillsError> {
    if files.len() > MAX_FILES {
        return Err(invalid("too many files"));
    }
    let mut components = BTreeMap::new();
    let mut total = 0;
    for (path, file) in files {
        if path.len() > 512 {
            return Err(invalid("path too long"));
        }
        let mut prefix = String::new();
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            let stem = part
                .split('.')
                .next()
                .unwrap_or_default()
                .trim_end()
                .to_ascii_lowercase();
            let device = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
                || ["com", "lpt"].iter().any(|p| {
                    stem.strip_prefix(p)
                        .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
                });
            if part.is_empty()
                || part.len() > 128
                || part.ends_with(['.', ' '])
                || device
                || !part
                    .bytes()
                    .all(|c| (c.is_ascii_graphic() || c == b' ') && !b"\\:<>\"|?*".contains(&c))
            {
                return Err(invalid("unsafe file path"));
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let entry = (prefix.clone(), parts.peek().is_none());
            if components
                .insert(prefix.to_ascii_lowercase(), entry.clone())
                .is_some_and(|old| old != entry)
            {
                return Err(invalid("file path collision"));
            }
        }
        total += file.bytes()?.len();
        if total > MAX_BUNDLE {
            return Err(invalid("bundle too large"));
        }
    }
    Ok(())
}

fn unique_files<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Files, D::Error> {
    struct FileMap;
    impl<'de> serde::de::Visitor<'de> for FileMap {
        type Value = Files;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an object of unique file paths")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Files, A::Error> {
            let mut files = Files::new();
            while let Some((path, file)) = map.next_entry()? {
                if files.len() >= MAX_FILES || files.insert(path, file).is_some() {
                    return Err(serde::de::Error::custom("duplicate path or too many files"));
                }
            }
            Ok(files)
        }
    }
    de.deserialize_map(FileMap)
}
