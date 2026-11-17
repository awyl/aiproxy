# Model Aliases Implementation Plan

**Status:** Complete. Automatic refresh was implemented in the follow-up [refresh plan](2026-11-17-model-refresh.md). Original no-release/no-commit constraints below describe the implementation phase; the user authorized the 0.4.3 bump and local commit on 2026-11-17. No push is authorized.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Centrally managed durable default/<name> model aliases, usable through all existing inference surfaces and the pi extension.

**Architecture:** A dedicated aliases module owns durable mappings. ModelRegistry resolves them before existing routing checks; discovery carries target identity so pi can inherit capabilities without changing the selected model ID.

**Tech Stack:** Existing Rust/axum/tokio/serde_json; existing TypeScript/vitest/jsdom. No new dependencies.

**Spec:** docs/superpowers/specs/2026-11-16-model-aliases-design.md

## Global Constraints
- No git push ever. No commits until the job is done and separately authorized.
- No provider integration changes or Anthropic OAuth work.
- No protocol conversion, automatic pi refresh, dependency additions, or release/version bump.
- Names: default/<name>. State file: aiproxy-aliases.json.
- Directory: AIPROXY_RUNTIME_DIR > existing /runtime > config parent.
- Persist first, publish in-memory state second; retain old state on errors.
- Global-token-only management when auth is configured; subscription tokens do not grant management access.

## Review Focus
- A target disappears after discovery: retain mapping, advertise unavailable in management, omit inference listing, reject routing.
- A failing save must not report success or change active mappings; concurrent saves leave disk and memory consistent.
- Alias identity must not bypass subscription authentication or select the wrong wire driver.
- Pi attribution and stream cleanup must follow targets, including named subscriptions and nested OpenRouter model IDs.
- Editor text is untrusted: use textContent/DOM construction, never inject names/errors/tokens into HTML.

### Task 1: Durable store and registry resolution

**Files:** Create src/aliases.rs; modify src/lib.rs, src/discovery.rs, src/server.rs; tests in aliases/discovery and tests/e2e.rs.

**Interfaces:**
- AliasStore::load(path: PathBuf) -> io::Result<Self>; AliasStore::memory() -> Self for existing registry constructors.
- AliasStore::snapshot() -> BTreeMap<String, String>.
- AliasStore::replace(map: BTreeMap<String, String>) -> io::Result<()> serializes save and publication.
- alias_state_path(config_path: &Path) -> PathBuf; pure directory-precedence helper tested without environment mutation.
- ModelRegistry::with_aliases(providers: Vec<Arc<dyn Provider>>, aliases: AliasStore) -> Self; existing new delegates with an empty memory store.
- Registry alias validation and available alias catalog helpers shared by management and /v1/models; resolve returns final real provider/model tuple.

- [x] Add failing store tests: absent file empty; save/load restart equals map; corrupt file errors; failing write preserves old map; concurrent writes leave reloaded map equal to active map; path precedence.
- [x] Add failing registry tests: direct IDs unchanged; aliases resolve chat/messages/responses targets; invalid names, collisions, alias targets, embeddings and unknown targets rejected; disappeared targets unavailable but retained; unchanged unavailable entries can survive unrelated edits.
- [x] Run cargo test --lib aliases and cargo test --lib discovery; record expected failures.
- [x] Implement the store using existing filesystem/serde primitives, atomic same-directory save and serialized updates; wire registry and startup load with visible state path.
- [x] Run the same tests; verify all pass.

### Task 2: Management API, inference listing and web editor

**Files:** Modify src/aliases.rs, src/server.rs, src/api/openai.rs, src/pages.rs; create src/aliases_page.html and agent/pi/extensions/aiproxy/tests/aliases_page.test.ts; extend existing routing tests and tests/e2e.rs.

**Interfaces:**
- GET /api/aliases -> { aliases: [{ id, target, surface, available }], state_path }.
- PUT /api/aliases accepts { aliases: { "default/fast": "provider/model" } }, validates the complete update, persists, and returns the same response shape.
- GET /aliases -> shared-frame editor. Catalog options come from existing /api/models; token stays in memory.
- /v1/models alias entries include target and inherited surface; normal entries are unchanged.

- [x] Write failing API tests for global token success, wrong/missing/subscription token rejection, fully auth-disabled mode, subscription-only fail-closed mode, invalid updates, and failed-save response with old mappings intact.
- [x] Write failing inference tests across chat/messages/responses: target resolution, wrong-surface 400, subscription denial, unchanged bytes except top-level model, unavailable target rejection.
- [x] Write failing jsdom tests for listing, target selection, add/edit/delete/full-map save, failed request preserving edits, unavailable target visibility, token header handling, and hostile text rendered literally.
- [x] Run targeted Rust tests and npm test -- aliases_page.test.ts; capture failures before implementation.
- [x] Implement routes, validation, listing, editor and navigation using current page conventions. Reuse existing auth primitives without allowing subscription tokens to administer mappings.
- [x] Run targeted tests; confirm persistence by rebuilding the server against the same temporary directory and checking GET/list/routing after restart.

### Task 3: Target-aware pi metadata and behavior

**Files:** Modify agent/pi/extensions/aiproxy/provider.ts and tests/provider.test.ts; existing thinking tests if needed. No pi core edits.

**Interfaces:**
- ProxyModel.target?: string. Build alias-to-target lookup once per registration.
- Lookup ID is target ?? id; registered/sent ID remains id. Strip named-subscription suffix only from target provider kind, preserve the full remaining model path.
- Existing SURFACE_API/API_ALIASES mapping remains authoritative. Header attribution and THINK_CLEAN_MODELS checks follow the target.

- [x] Read relevant installed pi extension docs and existing source before changing SDK-facing code.
- [x] Add failing tests: alias retains ID/name; target capabilities including reasoning/input/limits/compat copied; responses and messages drivers preserved; named subscriptions and nested target paths; target metadata absent fallback; attribution headers and existing stream cleanup follow target; normal models unchanged.
- [x] Run npm test -- provider.test.ts; record expected failures.
- [x] Implement target-aware lookup and behavior without changing native provider code or adding polling/hooks.
- [x] Run npm test and npm run typecheck in agent/pi/extensions/aiproxy.

### Task 4: Verification and user documentation

**Files:** README.md and agent/pi/extensions/aiproxy/README.md; plan checkboxes.

- [x] Document web/API use, exact file naming/directory precedence, management auth, restart persistence and reload requirement. Explain server discovery refresh versus pi registration refresh; automatic refresh is not implemented.
- [x] Run cargo fmt --check, cargo test, cargo clippy --all-targets -- -D warnings; distinguish any hardware/network-gated tests from actual passes.
- [x] Run npm test and npm run typecheck in agent/pi/extensions/aiproxy; inspect diagnostics with aft_inspect.
- [x] Run a local mock-backed HTTP smoke test; record raw alias save/list, wrong-auth rejection, successful routed upstream model and restart persistence. Do not call actual providers or expose credentials.
- [x] Review diff and git status; report exact evidence, reload limitation and automatic-refresh explanation. Do not commit or push.
