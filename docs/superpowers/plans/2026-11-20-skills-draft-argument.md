# One-call Skills Drafts Implementation Plan

> For agentic workers: use executing-plans, inline, with TDD. No delegation, commits, merges or pushes without permission.

**Goal:** ordinary MCP calls create and edit drafts using draft_token.
**Spec:** ../specs/2026-11-20-skills-draft-argument-design.md
**Architecture:** one shared skills handler checks the argument; remove skills-only transport credential forwarding.
**Stack:** existing Rust, Axum, rmcp and serde; no new dependencies.

## Constraints and review focus
Keep external MCP auth, dashboard approval, expiry/rotation/revocation and conflict semantics. Test wrong argument with a valid legacy header, editing conflicts, token non-disclosure, and both transports.

## Task 1: Replace draft authentication
Files: src/skills_mcp.rs, src/mcp_multiplexer.rs, tests/skills_mcp.rs, README.md, current skills spec and relevant old plans.
- [x] Add a two-transport regression covering the advertised argument, create/edit, rejected legacy auth, wrong/expired/revoked token, stale revisions, and no credential in persisted skill state or responses.
- [x] Run cargo test --test skills_mcp; observe failure against the old implementation.
- [x] Add draft_token to Draft/tool schema and validate it in the shared handler; remove token parameters/header extraction and skills-specific multiplexer forwarding.
- [x] Migrate existing auth tests to the argument; keep negative legacy-auth tests. Update README, tool instructions and specs; mark obsolete plans superseded.
- [x] Run focused tests, then review cleanup, callers and allocations.
- [x] Run full Rust and dashboard/Pi/helper suites, typecheck, strict Clippy, fmt and a live temporary proxy create/edit flow.

Progress: plan/spec recorded after approval. Existing checkout isolated on feat/skills-draft-token-argument. No review delegation authorized; final review will be inline.

## Verification and review
- RED: two new transport regressions failed on the original code: argument credentials were denied; bearer-only credentials incorrectly saved a draft.
- GREEN: both routes create and edit via draft_token; existing auth tests now use the argument; explicit legacy-auth rejection cases remain.
- Expiry is tested in the shared handler, rotation/revocation on real HTTP routes, and stale draft/published revisions prevent overwrites. Token non-disclosure and persisted content are checked.
- Cleanup: one shared credential check, no fallback credential paths, removed header extraction and token forwarding for skills. External MCP auth is untouched. The Arc clone for spawn_blocking is required for owned task lifetime.
- Full verification: 339 Rust passed (1 ignored), 112 dashboard/Pi passed, 5 helper tests passed including live import; strict Clippy, formatting and typecheck passed.
- A stock MCP SDK client connected without credentials to both /mcp and /mcp/skills, inspected the required argument, created and edited drafts, and confirmed stale/revoked calls fail. Drafts remained unpublished and disabled.
- Live evidence: target/logs/skills-draft-argument-live.json. The initial probe attempted to parse the empty DELETE 204 response as JSON; only the probe was corrected.
- Final review: inline; no delegation authorized. No new dependencies, client settings or extra authorization tool. Changes left uncommitted.

## Follow-up: mandatory draft read (approved)
- [x] Test real draft read/edit/save through both transports, exact content and IDs, public non-disclosure and rejected/rotated/revoked/expired credentials.
- [x] Implement a targeted store reader, shared draft credential check and new read_skill_draft tool. Update discovery expectations and require read-before-edit in tool/server instructions and documentation.
- [x] Verify live SDK read/edit/save with no configured credentials; run full suites, Clippy fixes, then fmt and final checks. Leave uncommitted.

Follow-up verification: the new read regression failed with unknown skills tool before implementation. Both transports now pass authenticated read/edit/save/read-back, exact content and revision IDs, concurrent/stale save rejection, no-draft/no-published-fallback, disabled-publication blocking, and token rotation/revocation. Expiry is tested against a real existing draft in the shared handler. A stock SDK client used the returned live content and IDs on both endpoints without transport credentials. 340 Rust passed (1 ignored), 112 dashboard/Pi and 5 helper tests passed; Clippy, then fmt, formatting check and typecheck passed. AFT diagnostics are unavailable (no Rust LSP producer); Cargo tests and strict Clippy are the compiler-backed verification. This verifies the temporary proxy fixture, not the user's live clean-up draft, which requires deployment and MCP reload before the new tool is available.

Pre-commit recheck: the full parallel rerun hit OS process/thread exhaustion (WouldBlock / EAGAIN); dashboard tests timed out and the helper could not spawn git. A sequential Rust retry reached 305 passing tests but timed out before finishing. No application code was changed to bypass these failures. Fresh focused verification passed all 5 HTTP skills MCP tests and 3 shared-handler tests, strict all-target Clippy and formatting check. The earlier complete successful run remains 340 Rust (1 ignored), 112 dashboard/Pi and 5 helper tests; do not treat the pre-commit full rerun as passing. After deployment the real read_skill_draft call returned clean-up draft 8 directly, confirming the requested logic-permission rule and Rust Clippy/fmt order without rewriting it.
