# MCP Skills Implementation Plan

> Historical plan: draft transport authorization is superseded by `2026-11-20-skills-draft-argument.md`; create and edit use the `draft_token` tool argument, not a bearer header.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking. Project rules override the skill's intermediate commits and any automatic delegation.

**Goal:** Review, publish, lazily load, update, and permanently remove full skill folders.

**Architecture:** A single versioned registry backs a dedicated MCP route, the dashboard API, and an authenticated bundle download used by a local CLI. GitHub imports stage snapshots in that registry; only publishing changes what agents see.

**Tech Stack:** Rust, Axum, rmcp, reqwest, serde, dashboard HTML/JS, existing jsdom tests.

**Spec:** [MCP skills design](../specs/2026-11-19-mcp-skills-design.md)

## Global constraints

- Work on feat/skills-registry, never main. No pushes or intermediate commits.
- No execution, installs, or MCP registration on the proxy; no startup sync on clients.
- No state or routes with skills disabled. Persist enabled state in aiproxy-skills.json using the existing runtime path convention.
- Read and draft authority are separate from dashboard authority. Do not log secrets, skill bodies, or notes.

## Review focus

- Deletion and recreation must not reuse revision IDs (Task 1).
- Disable or publish between load and file retrieval must not return mismatched content (Task 2).
- The browser must not render imported instructions or claim success on stale/failed writes (Task 3).
- A draft opened during a GitHub check must not be overwritten (Task 4).
- A malformed file list must not escape or partially populate the output directory (Task 5).

### Task 1: Registry and persistence

**Files:** Create src/skills.rs; register in src/lib.rs. Tests in src/skills.rs.

**Interface:** SkillsStore::open(path: PathBuf) -> io::Result<SkillsStore>; lock-protected store shared as Arc<SkillsStore>. Expose admin_snapshot(), catalog(), load(name), read_file(name, revision, path), bundle(name, revision), and apply(action: Action) -> Result<AdminSnapshot, SkillsError>. Use u64 revision IDs. Store one pending and two published snapshots per skill; store a registry-wide next ID in the state. Snapshots carry BTreeMap<String, SkillFile>, note, parsed metadata, and optional source; SkillFile holds base64 bytes and an executable flag. GitHub snapshots additionally record blob IDs and commit. Public catalog omits files, note, and source; load emits name, revision, content, note and companion paths.

- [ ] Add tests named skills_lifecycle, skills_reject_stale, skills_remove_recreate, skills_preserve_state_on_failed_save, and skills_invalid_state. Assert pending changes never leak, initial publish is disabled, changes preserve the toggle, exact-draft rejection works, rollback allocates a new ID and leaves pending drafts stale, and purge removes all content across restart. On a failed rename, compare the before/after admin snapshots. Test a separate successful restart with open draft, toggle, note and rollback history.
- [ ] Run cargo test skills_ and record the failing assertions (or compile errors for missing interfaces).
- [ ] Implement registry transitions against a candidate copy, using aliases::persist_json before swapping live state. Require a matching base and draft ID for save/publish, draft ID for reject, and current published ID for toggle/rollback. Remove checks both current IDs. Cap skills at 64, note at 8 KiB, files per snapshot at 128, file at 512 KiB, raw bytes per snapshot at 2 MiB and serialized state at 32 MiB. Enforce caps both after reading and before writing, including aggregate encoded state. Require a frontmatter identifier of at most 64 characters matching [a-z0-9]+(-[a-z0-9]+)* and a bounded nonempty description; expose only name, description, license and compatibility from supported frontmatter fields. Validate portable paths including prefix conflicts, case-fold collisions and Windows device names; reject unsafe path components and do not normalize incoming paths. Disabled snapshots remain available to admins only.
- [ ] Run cargo test skills_ and record passing results.

### Task 2: Config, MCP discovery, and agent authorization

**Files:** src/config.rs, src/server.rs, src/lib.rs; create src/skills_mcp.rs and tests/skills_mcp.rs.

**Interface:** No skills-specific YAML; the server builds a shared SkillsStore at aliases::runtime_state_path and mounts /mcp/skills. Factor the existing allowed-host computation out of src/mcp.rs so both call it; use rmcp StreamableHttpService with existing stateless transport settings.

- [ ] Add skills_mcp_wire and skills_mcp_auth integration tests that initialize with Streamable HTTP, inspect tools/list and call all four tools by exact names in the spec. Assert hidden fixture markers never appear in initialize, tools/list or catalog. Load returns the body and approved note; old revision reads after publish conflict; after disable or purge, reads fail. Assert anonymous reads work and only a temporary draft bearer can submit drafts. The draft bearer cannot replace source/note/assets or publish. Add config tests for missing admin token and reserved backend name.
- [ ] Run cargo test --test skills_mcp and record failure.
- [ ] Implement the route. Check the dashboard-issued draft bearer on each draft tool call without blocking reads, and return tool errors for denied writes, unavailable skills, and stale revisions. For individual binary reads, return base64 with explicit encoding; return UTF-8 companion text as text. Set no-store on all skill routes.
- [ ] Run cargo test skills and cargo test --test skills_mcp.

### Task 3: Dashboard and exact-revision review

**Files:** src/pages.rs, src/server.rs, src/lib.rs; create src/skills_api.rs, src/skills_page.html, tests/skills_api.rs, and agent/pi/extensions/aiproxy/tests/skills_page.test.ts.

**Interface:** GET /skills is an inert page shell. GET /api/skills is the admin snapshot; POST /api/skills/{name}/actions takes a tagged Action: save_text_draft, publish, reject, set_enabled, rollback, remove. Draft saves carry content, note and expected base/draft IDs. Reject targets the exact draft; remove uses administrator confirmation plus IDs from the page. Respond 400 invalid, 401 missing/wrong credential, 404 unavailable, 409 conflict, 500 persistence failure; return the fresh snapshot on success.

- [ ] Add tests skills_api_exact_review and skills_api_admin_only. jsdom: render a note with HTML-like payload using textContent, exercise stale publish and forced 500 without fake success, assert explicit confirmation before remove, and assert changed and deleted files appear in review.
- [ ] Run cargo test --test skills_api and the focused skills_page.test.ts using the existing package's test runner; record failure.
- [ ] Implement API authorization independently of aliases' optional-token behavior and add Skills to the nav when configured. Review shows old/new plain text for Markdown and notes, added/removed/modified paths, and metadata for changed binaries. Preserve the form on a failed save. Do not silently resubmit against a newer revision.
- [ ] Rerun API and dashboard tests. Check the dashboard only loads the shell before auth.

### Task 4: GitHub import and explicit update checks

**Files:** Create src/skills_github.rs; extend skills_api.rs, skills_page.html and their tests.

**Interface:** POST /api/skills/import takes folder URL and optional tracked ref; POST /api/skills/{name}/check-update takes current published revision. Both require global auth. GitHub source holds owner, repo, folder, tracked ref (default branch if absent; no moving target for a commit-pinned import unless explicitly set), and resolved commit. Use the existing GitHub endpoint probe below and deterministic fixture responses for tests.

- [ ] Add skills_github_pins_snapshot and skills_github_update_race tests: assert two checked-in files at pinned SHAs, note carried forward, identical bundle does not stage, file deletion stages without replacing published files, and upstream failure leaves state unchanged. Verify simultaneous author save causes 409. Exercise rejected redirects, oversize bodies, incomplete trees, symlinks, and submodules.
- [ ] Run cargo test skills_github and record failure.
- [ ] Parse only HTTPS github.com folder URLs and construct requests only to https://api.github.com using escaped segments. Require explicit ref field for ambiguous slash-containing refs. Resolve /repos/{owner}/{repo}/commits/{ref} once (use commits?per_page=1 for default branch), descend nonrecursive git trees to the folder, then fetch that tree recursively and require truncated=false. Fetch blob SHA with Accept: application/vnd.github.raw+json. Never use arbitrary upstream URLs, HTTP redirects, checkout, or archives. Limit response bytes and concurrency before and during streaming, not just after decoding. Check no open draft before remote I/O and again under lock with expected IDs when staging. Compare file paths, raw bytes, executable bits; retain published note. Report upstream errors without altering state. Show the saved source and commit in dashboard review.
- [ ] Run cargo test skills_github and dashboard tests. Probe the real public rust-anything folder without publishing anything to a user's server.

### Task 5: Local bundle fetch and end-to-end checks

**Files:** src/main.rs, src/skills_api.rs, src/lib.rs; create src/skills_fetch.rs and tests/skills_fetch.rs; update README.md with dashboard, auth and stock MCP setup.

**Interface:** GET /api/skills/{name}/bundle?revision=ID requires live availability but no bearer. Returns revision and file map, including SKILL.md. aiproxy skills fetch NAME --revision ID --output NEW_DIRECTORY --base-url URL materializes bytes without credentials, and prints the directory path. Accept HTTPS and loopback HTTP; do not follow redirects. Use existing CLI conventions if they already provide an equivalent base URL flag.

- [ ] Add skills_fetch_layout and skills_fetch_bad_paths: fetch script plus asset and assert contents, exec bit on Unix, and no execution marker. Assert rejection for traversal, absolute paths, duplicates, file/dir prefix collisions, existing or linked output, revision mismatch, disabled and removed skills. Assert failure leaves preexisting paths untouched; do not write a file before validating the entire manifest.
- [ ] Run cargo test --test skills_fetch and record failure.
- [ ] Implement the public live bundle route and bounded client decoder. Share path validation with the store, create output root exclusively, create files exclusively under it, preserve regular exec bit on Unix, and clean up the new output on recoverable write failure. Never execute any files. Document runtime token separation, first-use MCP, and local fetch with a script fixture.
- [ ] Run cargo fmt --check, cargo test --locked, the extension test suite, and git diff --check. Test Pi and Claude Code if installed; show raw initialize/catalog/load/file results with canary markers and redact credentials. On a temporary proxy demonstrate install, approve, enable, import update review, disable, purge, and restart. Report absent clients as untested. No push.

## Upstream evidence used to design import

Live GitHub REST responses for erkamyaman/rust-anything: commit f486865e6d9714137908a457638ecd7fadb8bbd6 returned tree d67acd629b35aac88d6b8f1e108ef2b1d2f3d55d. Under skills/rust-anything that tree returned SKILL.md (12,045 bytes) and whole-app.md (15,630 bytes), both mode 100644. A raw blob GET for whole-app.md returned exactly 15,630 bytes. The separate default-branch commit listing returned 9029714832faedc9bec43e0e6ae5179b0ef9f073 at probe time.

## Verification record

- Stock Pi 1.5.2 connected to a temporary proxy and listed all four skill tools. Claude Code was not installed in the test environment.
- Live GitHub import of rust-anything resolved 9029714832faedc9bec43e0e6ae5179b0ef9f073 and staged SKILL.md and whole-app.md. MCP returned an empty catalog before publication. After publish and enable it listed metadata, loaded SKILL.md with only the companion path, and returned whole-app.md on a separate request. The local CLI materialized both files. The unchanged upstream reported "No skill changes." Enable survived restart; disable blocked MCP load and bundle fetch; removal survived restart with an empty registry.
- The deterministic GitHub update fixture verifies that a file deletion stages for review without changing published files, carries forward the approved note, and cannot overwrite a draft created while the fetch is in flight. It also verifies a truncated upstream tree does not stage any changes.
