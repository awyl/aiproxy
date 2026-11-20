# Temporary Skill Draft Tokens Implementation Plan

> Historical plan: its bearer-only draft transport and exclusion from tool arguments are superseded by `2026-11-20-skills-draft-argument.md`. Issuance, lifetime, rotation and revocation remain unchanged.

> **For agentic workers:** Use superpowers:executing-plans. Follow the checklist inline on feat/skills-registry.

**Goal:** Replace the environment-based author token with a dashboard-issued draft-only credential.

**Architecture:** Share one in-memory grant between admin and MCP routes. Use an OS-random bearer secret and a monotonic 15-minute deadline. A new grant replaces the old one; revocation or restart clears it. Keep reads public and all admin routes global-token only.

**Spec:** Approved dashboard flow in the conversation; update docs/superpowers/specs/2026-11-19-mcp-skills-design.md to match.

- [x] Add failing HTTP coverage for authorization, issuance, rotation, revocation, and unchanged public reads; add UI coverage for secret display and clearing.
- [x] Remove author_token_env; wire POST/DELETE /api/skills/draft-token and verify the bearer on each MCP draft call. Test the expiry boundary without waiting.
- [x] Add dashboard controls and agent instructions. Keep the secret out of snapshots, storage, and tool arguments.
- [x] Run Rust and dashboard suites, typecheck, and formatting checks. Do not push or move the release tag.
