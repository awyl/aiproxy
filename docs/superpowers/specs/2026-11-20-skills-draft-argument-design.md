# One-call skills drafts

Approved in conversation: save_skill_draft accepts a temporary draft_token argument for both creating and editing skills. Replace, rather than retain, skills-specific bearer and X-MCP-Servers per-server-token authorization. External MCP server auth is unchanged.

## Contract
- save_skill_draft(draft_token, name, content, base_revision?, expected_draft_revision?) works through /mcp and /mcp/skills without configuring client credentials.
- Missing, invalid, expired, rotated or revoked tokens fail with actionable instructions. Dashboard/global tokens do not grant draft access.
- Existing 15-minute expiry, dashboard issuance/revocation, and revision conflict checks stay unchanged. Edits are drafts only; human publication remains separate.
- Tokens are not persisted with skills, echoed in responses, or explicitly logged by aiproxy. They can appear in client tool-call histories; use HTTPS remotely.
- Remove old skills bearer/per-server token guidance, handler parameters and credential extraction. No new auth tool, session state, client configuration or dependencies.

## Approach
A credential argument is usable by ordinary MCP tools in one call. Transport-only credentials require client reconfiguration or custom HTTP; a separate authorization tool would add a second call and session state.

## Mandatory draft reading (approved follow-up)
Add read_skill_draft(draft_token, name) on both MCP transports. It returns the live draft SKILL.md, name, draft_revision and base_revision. It must not fall back to published content or expose drafts through public tools. Require the same temporary credential as draft writes, with the same expiry/revocation behavior. Read before editing and save with the returned IDs; a concurrent edit must cause a conflict. Keep the existing disabled-published-skill agent access restriction. No read receipts, client state, extra credential, or transport-auth fallback.
