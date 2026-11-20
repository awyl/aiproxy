# Repository Skill Picker Implementation Plan

> Required skill: executing-plans. Execute inline; no delegated workers or commits without user approval.

**Goal:** Replace GitHub-folder input with repository discovery and a skill picker.
**Architecture:** A locked Node helper calls skills@1.9.0; Rust validates inputs/results and stages existing registry drafts.
**Tech stack:** Rust/axum, Node >=22.20, git, skills@1.9.0, node:test, existing vitest/jsdom.
**Spec:** docs/superpowers/specs/2026-11-19-skills-library-import.md

## Global constraints
Preserve anonymous published reads, dashboard-only management, draft tokens and exact-revision approval.
Keep existing Source JSON and installed skills compatible. No global skills install. No npx at request time.
No push, no automatic commit. Use existing feature branch.

## Review focus
- Duplicate names must select by folder, not silently choose first.
- Changing the URL invalidates search results; late discovery responses cannot authorize the new input.
- Missing runtime/helper produces an actionable error but does not prevent server startup.
- Timeouts, process cancellation and output limits must not leak child jobs or fill disk.
- Updates preserve notes and reject races/open drafts; old persisted sources still work.

## Tasks
- [x] Add locked helper package and failing node tests: remote commit pin, path selection, scripts not run, symlinks/duplicates rejected, unsafe sources, cleanup.
- [x] Replace custom GitHub downloader with bounded subprocess bridge; test API discovery, source compatibility, stage/update races and invalid outputs.
- [x] Add picker and hooks reminder test-first; invalidate results on URL change, preserve note, retain compact list and lazy reviews.
- [x] Ship Node/git/helper in Docker, document standalone setup and update design.
- [x] Run complete Rust/JS/typecheck suites; live import rust-anything and verify companion-file/on-demand behavior.

## Verification ledger
- Node helper tests: 4 passed, including opt-in live rust-anything discovery/import at a pinned commit.
- Rust: 336 passed, 1 ignored. Dashboard/Pi: 112 passed; type-check passed.
- Live temporary proxy: three rust-anything folder choices, exact-commit import, both files preserved, note delivered through MCP, unchanged update, companion read and disable gate verified.
- Evidence: target/logs/skills-picker-live.json (ignored runtime artifact).
- Docker build not run: neither docker nor podman is installed.
- Strict clippy is blocked by pre-existing lints in dashboard_auth.rs, mcp.rs and tests/skills_mcp.rs. These files were not changed.
- Upstream contract correction: commit is source.source.commit; a scoped source can still report repository-relative skill paths.
- Ruling: filter names outside existing registry naming rules rather than failing discovery for an entire mixed-name repository. Documented in README.
- Ruling: Git hooks/config/credentials and submodule initialization are disabled in the child environment; only absolute PATH entries are passed. Entire helper process groups are killed on cancellation (Unix).
