# Repository skill picker (approved direction)

Replace the custom GitHub folder importer with the official skills@1.9.0 Node API.
Dashboard flow: repository URL -> Find skills -> select name, description and folder -> Note to agents -> Import as draft.
Show a persistent reminder: hook-dependent skills need local installation; sharing does not register hooks, tools or MCP servers.
Use Node >=22.20 and git. Runtime package is installed at build/deploy time, never through npx on a dashboard request.

## Boundaries
- Keep public GitHub HTTPS repositories as the supported input scope for this replacement. Reject local paths, credentials, non-HTTPS and non-GitHub sources before starting the helper.
- Discovery resolves repository HEAD to a full commit using git ls-remote, then calls resolveSource at that commit. Return names, descriptions, paths and commit; expand duplicate names into path-specific choices.
- Import reopens only the chosen folder at that exact commit, uses universal/copy, disables telemetry and lock files, rejects symlinks and duplicates. Never run bundled code.
- Rust retains existing file/name/size checks, draft/publish/enabled gates, concurrent-draft checks and Note to agents. Keep existing Source storage compatible so existing imported skills still update.
- Updates resolve the saved moving ref (or default HEAD), fetch only the saved skill folder and stage a reviewed draft. No automatic publishing.
- Helper execution uses argument arrays, bounded input/output, one import/discovery job at a time, process timeouts and temporary-directory cleanup.
- Deployment includes the helper and locked package. The rest of aiproxy must still start without Node/helper installed; only repository operations fail with an actionable error.

Ruling: this change replaces GitHub import mechanics, not the supported-host policy. Adding arbitrary git hosts/local sources would expand network and filesystem access and is out of scope.
