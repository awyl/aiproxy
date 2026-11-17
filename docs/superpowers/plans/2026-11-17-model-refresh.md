# Automatic Model Refresh Implementation Plan

**Status:** Complete. The user authorized the 0.4.3 bump and local commit on 2026-11-17 after implementation; the implementation-phase constraints below remain historical. No push is authorized.

> **For agentic workers:** Use superpowers:executing-plans inline; no delegation authorized.

**Goal:** Refresh aiproxy aliases every 60 seconds without /reload.
**Architecture:** A focused model-refresh.ts owns lifecycle/polling; provider.ts reuses one registration builder for startup and changed catalogs. Re-registering via pi updates the selected model using the native SDK behavior.
**Tech Stack:** Existing TypeScript, pi 0.84.4, vitest. No new dependencies.
**Spec:** docs/superpowers/specs/2026-11-17-model-refresh-design.md

## Global Constraints
No commits or pushes, dependencies, version bumps, proxy/provider changes, or pi.dev polling. Cadence 60_000 ms, timeout 10_000 ms. Preserve selection; no silent fallback. Inline execution approved.

## Review Focus
Shutdown during fetch cannot publish stale data; busy-to-idle transitions cannot drop pending updates; reordering cannot cause rebuilds; malformed data cannot erase working models; native current selection really receives new capabilities.

### Task 1: Poll lifecycle — complete
Files: create model-refresh.ts and tests/model-refresh.test.ts under agent/pi/extensions/aiproxy.
Interface: registerModelRefresh(pi, initialModels, fetchModels(signal), applyModels(models)); initialModels and callback use ProxyModel[]. Poll starts at session_start, applies on idle or agent_settled, aborts/cleans on shutdown. Normalize/fingerprint discovery fields id,target,surface,display_name; reject malformed models or duplicate IDs before publishing. One outage warning per failure period.
- [x] Write failing fake-timer tests for cadence, timeout, non-overlap, reordered/no-op responses, idle deferral, failure retention/recovery, additions/deletions, missing selected alias warning, shutdown/stale fetch cancellation and repeated session_start.
- [x] Run tests and observe failures; implement the smallest lifecycle controller; run tests again.

### Task 2: Provider integration
Files: provider.ts, tests/provider.test.ts, SDK-backed tests/provider-refresh.test.ts.
Interface: shared applyModels updates target lookup and pi registration synchronously; fetchModels validates the response and forwards auth and AbortSignal. Existing before_provider_headers registered once. registerModelRefresh called once after startup registration.
- [x] Write failing tests proving new target metadata/driver/headers, validation, and unchanged model behavior. Prove selected model refresh with actual installed AgentSession SDK, not only mock registration calls.
- [x] Refactor current registration into a reusable closure, integrate polling without adding duplicate hooks, then run focused tests and typecheck.

### Task 3: Verification and docs
Files: README.md, extension README.md, aliases_page.html reload hint, related jsdom assertions.
- [x] Update docs/UI: automatic refresh within 60s while idle; /reload still forces an immediate refresh. Document outage retention and missing-selection warning.
- [x] Self-review lifecycle race conditions and real SDK source contract. Run npm test, npm run typecheck, cargo fmt --check, cargo test, cargo clippy --all-targets -- -D warnings, git diff --check, and aft_inspect. Record evidence and limits; no deployment/commit/push.
