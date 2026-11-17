# Model aliases

## Intent and agreed behavior
Provide stable role names such as aiproxy/default/fast, default/free, and default/planner. Users create/edit/delete mappings through a web page and API; mappings survive a restart. The proxy receives default/<name>, resolves it to an actual upstream/model, and retains existing provider authentication, subscription checks, wire-surface validation, model-only byte patching, and existing response handling. The pi extension keeps the alias as the selected/sent ID while inheriting the target's catalog metadata and provider-specific behavior. The user approved this approach in conversation.

## Approaches
- Server aliases plus target-aware discovery (recommended and accepted): shared across clients, centrally editable, no protocol conversion.
- Client-only aliases: smaller server change but does not meet central web management.
- Protocol translation: unnecessary and explicitly excluded.

## Proposed implementation details
- Dedicated src/aliases.rs module for durable mappings and management handlers; no provider implementation changes.
- Names are default/<name>, where name contains only ASCII letters, digits, dot, underscore, or hyphen; reject empty, dot, and dot-dot names. Targets must be real currently cataloged inference models with known surfaces; no alias chains or embeddings.
- Persist a JSON object of alias-to-target pairs as aiproxy-aliases.json. Resolve directory by nonempty AIPROXY_RUNTIME_DIR, then /runtime if it exists, then the config file's directory. Do not use the Codex-specific override. Save through a same-directory temporary file and rename, serialized across concurrent updates; update memory only after durable save. Missing file means empty aliases; unreadable/corrupt file fails startup rather than silently discarding mappings.
- GET /aliases serves an editor integrated with existing navigation. GET /api/aliases returns mappings, target availability/surface, and state path. PUT /api/aliases replaces the full map, enabling create/edit/delete in one atomic operation. Authentication uses the global proxy token, not subscription tokens. When completely unauthenticated operation is configured, keep that mode; with subscription-only auth but no global token, management is denied. The editor accepts a password-style admin-token field, kept only in memory.
- Reserve default as the alias namespace; detect any configured provider-prefix collision at initialization.
- Retain persisted mappings whose targets become unavailable, show them as unavailable in the editor, omit them from /v1/models, and reject requests to them. Existing unavailable mappings may be retained or deleted when editing others, but new/retargeted mappings require an available target.
- /v1/models adds alias entries with id, target, surface and ordinary model fields. Ordinary entries remain unchanged.
- Registry owns the alias store so existing AppState fixtures need no new field. Resolve aliases before returning the final provider/model pair; every request still uses existing authorization and surface checks.
- pi's ProxyModel gains optional target. Registration, metadata lookup, attribution headers, and existing model-specific stream cleanup use the resolved target where appropriate, while model identity and routing remain the alias. Unknown target metadata uses existing fallback behavior, never invented limits.
- No example aliases are installed automatically: the supplied model names may not be available on this installation.

## Non-goals and limits
No automatic pi refresh in this change; explain it after implementation. No provider/API translation, new upstream integrations, fallback/load balancing, response model rewriting, dependency additions, version bump, commits, tags, or pushes. Normal direct model IDs keep current behavior. An already-running client may have stale capabilities after a mapping change; reload pi after saves, especially across wire surfaces.

## Verification
Tests must first demonstrate missing behavior, then cover restart durability, write failure retaining old state, invalid/cyclic/noncatalog targets, concurrent saves, auth failures, unavailable targets after refresh, unchanged normal routing, aliases across all three wire surfaces, subscription gates, byte-faithful model replacement, extension metadata/header/cleanup inheritance, and editor save/delete/error behavior. Final checks include Rust tests, fmt, clippy, extension tests/typecheck, diagnostics, and local mock-backed HTTP evidence. Real provider availability is not assumed.
