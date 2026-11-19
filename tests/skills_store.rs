use aiproxy::skills::{Action, SkillsStore};

const BODY: &str = "---
name: example
description: Example workflow
---
first canary
";
const EDIT: &str = "---
name: example
description: Example workflow
---
second canary
";

fn save(store: &SkillsStore, content: &str, base: Option<u64>, draft: Option<u64>) -> u64 {
    store
        .apply(
            "example",
            Action::SaveTextDraft {
                content: content.into(),
                note: Some("private note".into()),
                base_revision: base,
                expected_draft_revision: draft,
            },
        )
        .unwrap()
        .skills["example"]
        .draft
        .as_ref()
        .unwrap()
        .id
}

fn publish(store: &SkillsStore, draft_revision: u64, base_revision: Option<u64>) -> u64 {
    store
        .apply(
            "example",
            Action::Publish {
                draft_revision,
                base_revision,
            },
        )
        .unwrap()
        .skills["example"]
        .published
        .as_ref()
        .unwrap()
        .id
}

fn enable(store: &SkillsStore, revision: u64) {
    store
        .apply(
            "example",
            Action::SetEnabled {
                revision,
                enabled: true,
            },
        )
        .unwrap();
}

#[test]
fn skills_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aiproxy-skills.json");
    let store = SkillsStore::open(path.clone()).unwrap();
    let draft = save(&store, BODY, None, None);
    assert!(store.catalog().is_empty());
    let initial = publish(&store, draft, None);
    assert!(store.load("example").is_err());
    enable(&store, initial);
    assert_eq!(store.catalog()[0].name, "example");
    assert_eq!(store.load("example").unwrap().content, BODY);
    let draft = save(&store, EDIT, Some(initial), None);
    assert_eq!(store.load("example").unwrap().content, BODY);
    let latest = publish(&store, draft, Some(initial));
    assert_eq!(store.load("example").unwrap().content, EDIT);
    let pending = save(&store, BODY, Some(latest), None);
    drop(store);
    let store = SkillsStore::open(path).unwrap();
    assert_eq!(
        store.load("example").unwrap().note.as_deref(),
        Some("private note")
    );
    let rolled = store
        .apply("example", Action::Rollback { revision: latest })
        .unwrap();
    let restored = rolled.skills["example"].published.as_ref().unwrap().id;
    assert!(restored > pending);
    assert_eq!(store.load("example").unwrap().content, BODY);
    assert!(
        store
            .apply(
                "example",
                Action::Publish {
                    draft_revision: pending,
                    base_revision: Some(latest)
                }
            )
            .is_err()
    );
    store
        .apply(
            "example",
            Action::Reject {
                draft_revision: pending,
            },
        )
        .unwrap();
    store
        .apply(
            "example",
            Action::SetEnabled {
                revision: restored,
                enabled: false,
            },
        )
        .unwrap();
    assert!(store.bundle("example", restored).is_err());
}

#[test]
fn skills_reject_stale() {
    let dir = tempfile::tempdir().unwrap();
    let store = SkillsStore::open(dir.path().join("state.json")).unwrap();
    let first = save(&store, BODY, None, None);
    let replacement = save(&store, EDIT, None, Some(first));
    assert!(
        store
            .apply(
                "example",
                Action::Publish {
                    draft_revision: first,
                    base_revision: None
                }
            )
            .is_err()
    );
    assert!(
        store
            .apply(
                "example",
                Action::Reject {
                    draft_revision: first
                }
            )
            .is_err()
    );
    assert_eq!(
        store.admin_snapshot().skills["example"]
            .draft
            .as_ref()
            .unwrap()
            .id,
        replacement
    );
}

#[test]
fn skills_remove_recreate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let store = SkillsStore::open(path.clone()).unwrap();
    let draft = save(&store, BODY, None, None);
    let published = publish(&store, draft, None);
    enable(&store, published);
    let draft = save(&store, EDIT, Some(published), None);
    let newest = publish(&store, draft, Some(published));
    let pending = save(&store, BODY, Some(newest), None);
    store
        .apply(
            "example",
            Action::Remove {
                revision: Some(newest),
                draft_revision: Some(pending),
            },
        )
        .unwrap();
    assert!(store.load("example").is_err());
    let disk = std::fs::read_to_string(&path).unwrap();
    assert!(!disk.contains("example"));
    let store = SkillsStore::open(path).unwrap();
    assert!(store.admin_snapshot().skills.is_empty());
    assert!(save(&store, BODY, None, None) > pending);
}

#[test]
fn skills_preserve_state_on_failed_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let store = SkillsStore::open(path.clone()).unwrap();
    std::fs::create_dir(&path).unwrap();
    let before = store.admin_snapshot();
    assert!(
        store
            .apply(
                "example",
                Action::SaveTextDraft {
                    content: BODY.into(),
                    note: None,
                    base_revision: None,
                    expected_draft_revision: None
                }
            )
            .is_err()
    );
    assert_eq!(before, store.admin_snapshot());
}

#[test]
fn skills_invalid_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    std::fs::write(&path, "{garbled").unwrap();
    assert!(SkillsStore::open(path.clone()).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "{garbled");
}
