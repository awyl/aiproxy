# On-demand skills in aiproxy

## Goal

Manage shared **skill folders** in the dashboard. Pi and Claude Code discover them through MCP and fetch instructions or companion files only as needed. Agents can propose text edits, but nothing is distributed before dashboard review. V1 does not mirror skills at startup or provide native slash commands.

## Agent flow

Use a built-in Streamable HTTP server at `/mcp/skills`, separate from the `/mcp` multiplexer. Use stock client MCP configuration with no token for reads and a dashboard-issued temporary bearer token for drafts; no Pi extension or Claude Code hook. Anyone who can reach the proxy can read enabled, published skills. Reuse the existing allowed-host policy and reserve the MCP backend name `skills`.

- `list_skills()` returns enabled published skills' names, descriptions, supported frontmatter metadata, and revision IDs. No bodies, notes, or file contents in initialization, tool descriptions, or catalog output. Server instructions tell the agent when to consult the catalog.
- `load_skill(name)` checks live state and returns the current published revision, original `SKILL.md`, optional **Note to agents**, and companion file paths. If a publication happened after listing, the load gets the new revision.
- `read_skill_file(name, revision, path)` reads one companion file from the exact revision returned by load. It rechecks live availability. It never substitutes a newer file; if the loaded revision is no longer current, reload the skill. In particular, `rust-anything`'s `whole-app.md` is fetched only when needed. Unpublished, disabled, and unknown names are unavailable; retained history is for admin rollback only.
- For skills that need a local folder, `aiproxy skills fetch <name> --revision <id> --output <new-directory>` fetches the approved bundle without credentials, preserves paths and executable bits, and never runs anything. This is an explicit local shell action, not a side effect of reading `SKILL.md`. The agent may run a bundled script locally under its normal tool permissions after fetching it.
- `save_skill_draft(name, content, base_revision, expected_draft_revision)` requires a valid draft token on the MCP request. If one is missing, expired, or revoked, instruct the agent to ask the user for a fresh one and send it as an MCP bearer credential, never as a tool argument. Agents can create a text-only skill or edit `SKILL.md`; they cannot replace companion files, the note, or the GitHub source. Keep one open draft per skill; reject stale base or draft IDs. A disabled skill can be edited from the dashboard without granting the agent access to its published contents.

## Dashboard

Add **Skills** to the existing page frame. Require the global token for *all* admin API requests, including reads. No skills-specific YAML is needed. The page is always visible; without a nonempty global token distinct from subscription tokens, admin requests are denied. Inference/subscription tokens grant no draft or dashboard permissions. Keep the global token out of agent configurations. Return private state with `Cache-Control: no-store`.

Admins can paste a new skill, edit `SKILL.md` or its note, or stage a GitHub folder. Show the base and draft, including changed, added and deleted files and changes to executable bits. Display all text as inert text, never rendered Markdown or HTML. Publish/reject must target the exact draft reviewed; publish also checks the base revision. Publishing a new skill leaves it disabled. Publishing an edit preserves the toggle. Only published skills can be enabled. Retain one previous published version for rollback; rollback publishes it as a new revision, preserves the toggle, and leaves any open draft stale rather than silently rebasing it.

- **Disable** immediately makes the skill unavailable to new loads, file reads, and bundle downloads; leave state intact.
- **Remove** requires explicit confirmation and permanently deletes the skill, including source, draft, published files and rollback revision. No trash or restore. Neither action can retract in-flight responses, previously downloaded files, or instructions already in context. Removal never uninstalls an external CLI or MCP server.

### Temporary draft access

An admin manually generates a random draft-only bearer token from **Temporary draft access** in the dashboard. Only one token is active at a time; a new one replaces it. Tokens expire after 15 minutes, can be revoked from the dashboard, and do not survive a restart. Return the secret only on generation; never include it in the registry snapshot or persist it. The dashboard shows the generated token for sharing, and clears it on expiry, successful revocation or a change to the global-token field. Drafts still need dashboard approval.

### Note to agents

An optional, dashboard-editable note travels with the approved revision, separately from upstream `SKILL.md`. Review note changes like other changes, including note-only edits. Keep it private until publish; return it with load, not with list. Carry the published note forward on GitHub updates so the reviewer can adjust compatibility guidance.

For `rust-anything`, use its MCP tools if available, otherwise check `rust-anything --version` locally. If a CLI install or upgrade is needed, show the upstream command and ask permission before running it locally (e.g. `cargo install rust-anything` for a Cargo install). The user can do it themselves. MCP registration or changes to agent settings require a separate decision. Publication doesn't grant permission to install tools. aiproxy never runs skills, setup commands, or scripts, or configures their external dependencies.

### Import and update

For v1, accept public GitHub skill folder URLs and track an explicit ref (or the repo's default branch). A commit-pinned URL can be installed, but updating from a moving ref requires choosing that ref explicitly. Resolve the ref once to a commit; fetch the folder at that commit. Save the commit and source with the candidate revision. Never clone repositories, extract upstream archives, or follow symlinks/submodules. Import regular files only, preserving executable bits. Show binary file changes by metadata (path, size and Git blob ID); don't execute or render them.

**Check for update** is manual. Fetch the saved source; if the folder content and executable bits are unchanged, report no skill changes and leave the current revision alone. Otherwise stage a draft. Don't overwrite an open draft, including one created during the fetch. Review updates, including file deletions, before publishing; the old revision remains served until then. For `rust-anything` this includes both `SKILL.md` and `whole-app.md`. Updating the remote files does not update the agent's installed CLI or MCP server.

## Storage and validation

Persist versioned state in `aiproxy-skills.json` via the runtime path convention (`AIPROXY_RUNTIME_DIR`, then `/runtime` if present, then the config directory). Use the existing atomic JSON writer, serialize mutations, and expose new state only after a successful write. Missing means empty; invalid/unreadable state fails startup rather than silently replacing it. Multiple instances sharing one state file are unsupported.

Validate skill names, UTF-8 `SKILL.md` with matching frontmatter name and nonempty description, relative file paths and all size limits before accepting a draft. Every write checks the relevant revisions; IDs are not reused after deletion. Limit file count, bytes per file/revision, total registry size and concurrent import work. Materialize only into a new output directory; prevalidate all paths and never overwrite or follow existing local files. The extractor and import accept no dot components, absolute paths, path separators inside components, or portable filename collisions. Trust vetted instructions; approval does not sandbox a script that a local agent later runs.

## Acceptance

- [ ] Inspect raw MCP initialize/list/catalog/load; a body-only marker and the private note appear only after load, and a companion marker appears only after a companion or bundle fetch.
- [ ] Exercise credentials, exact-draft review, approval, enable/disable, stale reads, conflicting writes, rollback, purge, and restart durability. Force a save failure and verify no change becomes visible.
- [ ] Import the public `rust-anything` folder, then simulate an upstream update including a changed or deleted companion file. Verify note carry-forward, failed checks leaving state untouched, and the old publication remaining live until review.
- [ ] Fetch a fixture with a script and assets to a local workspace; verify preserved paths and permissions without execution on fetch. Reject unsafe path inputs without writing output.
- [ ] Smoke-test stock Pi and Claude Code against the real endpoint. Report unavailable clients as untested.
