# Runtime Cleanup Implementation Plan

> Execute inline with focused tests and one final branch verification.

**Goal:** Implement all five audit findings without changing public wire contracts, routing, model policy, or config. The documented usage-credential behavior and intended refresh/cache timing are the exceptions to preserving observed behavior.

**Architecture:** Keep existing module and trait boundaries. Reuse startup IDs and catalog snapshots, start independent fetches together, and keep synchronous ONNX inference off runtime workers.

- [x] Pin API-key selection and startup probe count with a local OpenCode Go subprocess fixture. Use the configured inference key and the precomputed provider ID. Delay scheduled discovery until the next interval.
- [x] Reuse a single sorted catalog for `GET /v1/models` and alias lookup; preserve sort order, duplicate handling, alias availability, eligibility, and response fields. Verify the existing alias and free-model tests.
- [x] Prove model fetching starts without waiting for metadata and production uses a fresh cache while refreshing it in the background. Fix the ESM filesystem import for cache writes.
- [x] Prove real embedding inference yields on a single-thread runtime with the blocking pool occupied. Retain one job at a time per model, results, and existing output shape.
- [x] Remove the unused usage trait method and attribution helper, and narrow the four Pi exports. No new traits or dependencies.
- [x] Verify full suites (304 Rust passed, 1 ignored; 105 Pi passed), formatting, Clippy and TypeScript. Commit locally without merging or pushing.
