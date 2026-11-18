# Per-upstream free-model policy implementation plan

**Spec:** [Per-upstream free-model policy](../specs/2026-11-18-per-upstream-free-model-policy.md)

- [x] Add failing tests for independent named upstreams, Go enforcement, legacy migration and the per-upstream UI.
- [x] Extend `Provider` with `free_only_default() -> Option<bool>`; forward through wrappers. In `server.rs` bind supported instances to stable persisted keys and resolve YAML defaults.
- [x] Store per-upstream overrides in the existing state file; load legacy state without rewriting until a successful change. Use one effective snapshot for listing and check the resolved target before inference.
- [x] Expose each supported provider's effective `free_models_only` value on `GET /api/models`; update one instance via `PUT /api/models` with `{"provider":"id","free_models_only":true}`. Render controls from server metadata and leave unsupported providers without a toggle.
- [x] Verify policy, migration, failed persistence, restart, and full Rust/extension suites; update user docs. No push or merge.
