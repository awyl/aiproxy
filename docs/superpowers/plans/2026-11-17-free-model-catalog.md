# Free Model Catalog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Flag confirmed free models and publish only those from OpenRouter and NVIDIA by default.

**Architecture:** Discovery stamps free status onto model records. A registry policy filters the agent list without altering provider routing. The browser edits the effective policy through a global-token-only write that persists independently of YAML.

**Tech Stack:** Rust, Axum, Tokio, reqwest, serde, existing HTML/JS.
**Spec:** `docs/superpowers/specs/2026-11-17-free-model-catalog-design.md`

## Constraints

Feature branch only; no remote push. Commit only after completion. No paid requests, no pi.dev dependency, no new runtime dependency. Do not change other providers' visibility or the transport for inference.

## Review focus

- Free-only mode must reject missing, dynamic, or nonzero OpenRouter prices.
- A partial or changed NVIDIA catalog must not mark any model free.
- A named upstream and an alias must not escape filtering.
- Failed discovery must not publish stale free labels for restricted providers.
- Subscription authorization and failed persistence must not change the setting.

## Task 1: Classify and filter

- [x] Write HTTP integration tests that distinguish OpenRouter zero-price, paid, missing-price and dynamic-price entries, with YAML omitted and with explicit opt-out. Verify RED before changing product code.
- [x] Add a shared model classification flag, `Config.free_models_only`, and apply the policy at the listing boundary (including aliases). Keep raw catalog and alias validation independent of listing policy.
- [x] Write NVIDIA tests against the observed hydrated `searchResult` schema and paginated fixture HTTP responses. Verify RED. Use the public filtered URL and `&page=2` (41 results, 24+17 verified live). Validate page metadata, flag, unique IDs, and results total before joining exact publisher/name IDs to the inference catalog. Never send a key to the website.
- [x] Run focused tests. Verify upstream failures do not retain free assertions.

## Task 2: Persist and expose the toggle

- [x] Write RED tests for global-token authorization, persisted override over YAML after restart, and failed persistence.
- [x] Add `PUT /api/models` (JSON `{"free_models_only":false}`) and return effective policy with catalog responses. Use existing runtime-dir conventions and atomic save to `aiproxy-model-visibility.json`; update memory only after save.
- [x] Wire the `/models` toggle and badges to backend data. Keep the alias editor's catalog complete and do not touch the pi extension.
- [x] Run focused tests and document YAML, refresh, and the billing boundary.

## Task 3: Verify

- [x] Run format, full tests, diagnostics, and review the connected diff. Verify live read-only catalog results without mutating the deployed proxy.
- [x] Confirm feature branch status and leave main and remotes alone.
