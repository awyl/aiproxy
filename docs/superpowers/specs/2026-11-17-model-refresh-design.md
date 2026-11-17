# Automatic pi model refresh

## Intent
Refresh aiproxy model aliases and their target-derived capabilities without requiring /reload. User requested implementation; inline execution remains selected. No commits or pushes.

## Approach
Poll the existing /v1/models endpoint; no new proxy endpoint or event transport. Native dynamic pi.registerProvider reuses the current registration path and refreshes the selected model by provider/id (verified in installed pi 0.84.4 agent-session.js:1970-1979 and 2084-2086). Native refreshModels with a whole-registry refresh would also refresh unrelated providers; a server push transport adds unnecessary machinery.

## Proposed behavior
Default fixed interval 60 seconds (15 seconds available if the user chooses it); ten-second request timeout, auth header as at startup, no overlapping requests. Validate catalog structure before applying. Compare normalized relevant discovery records so reordered or unchanged responses do not trigger registration. Update target lookup and registered models together using existing pi catalog metadata, without fetching pi.dev each minute. Poll for model/alias additions, removals, target, display-name and surface changes. Only publish while ctx.isIdle(); hold pending data while active and apply on agent_settled. Re-register only aiproxy, keeping the same selected alias and existing metadata/header/cleanup behavior. Deleted current selection gets a warning, never a silent fallback. Failures retain current models and are reported once per outage, recovery clears the warning latch. Clear interval and abort in-flight request on session_shutdown; stale callbacks must not register after shutdown. No user config option, new dependency, version bump, server/provider changes, or broader metadata refresh from pi.dev.

## Tests
Fake timers prove cadence, unchanged/reordered no-op, target/surface metadata/header updates, addition/removal, idle deferral and immediate settle application, failure/malformed response retention and recovery, no overlap, request timeout, shutdown cancellation and restart timer cleanup. At least one SDK-backed integration proves re-registration refreshes selected model identity/capabilities rather than only verifying mock calls. Extension tests/typecheck and unchanged Rust tests are final checks. Document behavior and replace manual-reload instructions.
