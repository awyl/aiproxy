# Per-upstream free-model policy

## Behavior

Replace the global **Free only** checkbox with one in each supported upstream's section on `/models`. Two named upstreams of the same kind can be set independently. Keep one global-token field; only that token can change policy. An update targets exactly one configured upstream, applies immediately after a successful save, and survives restart. Failed saves leave the old value in effect.

Use the same effective policy for `/v1/models` and the three inference routes. If enabled, list and route only models that the upstream catalog confirms are free. Block paid, unknown and unverified targets locally with HTTP 403, including aliases; enforce subscription authorization first. A discovery failure continues to revoke prior free claims. If disabled, preserve ordinary routing, listing and free badges. Do not cancel in-flight requests or alter the alias store. No agent query flag.

## Configuration and migration

Set `free_models_only: true|false` on the individual entry in `upstreams:`. Reject it on providers that do not advertise support. Defaults: on for OpenRouter and NVIDIA, off for OpenCode Go. Go's free signal is its advertised zero prices, not 'included in the subscription'.

Read the existing top-level YAML setting and the existing boolean state file for compatibility, applying them only to OpenRouter and NVIDIA. Precedence: saved per-upstream setting > legacy saved setting for OpenRouter/NVIDIA > explicit per-upstream YAML > legacy top-level YAML for OpenRouter/NVIDIA > provider default. On the next successful save, persist independent overrides in the same `aiproxy-model-visibility.json` state file; do not rewrite YAML. Store overrides by stable upstream identity (kind and configured name, or the kind when unnamed), so adding a second named account does not move or lose the first named account's setting. The old global write payload is not supported once the control is split.

## Implementation boundary

Extend the existing Rust `Provider` trait with an optional free-only capability and its default. OpenRouter, NVIDIA and OpenCode Go opt in; the discovery wrapper preserves the capability. Each adapter keeps supplying its existing free classification in `Model.free`. The server exposes capability and effective settings for each upstream in the management catalog. The UI renders controls from that metadata and does not know which provider kinds support it. New providers such as Groq only need to opt in and supply a verified free-model signal; no Groq integration is part of this change.

## Verification

Use local fixtures to verify independent settings for two named upstreams of one kind, default behavior for all three kinds, legacy YAML and state migration, persisted override precedence, global-token enforcement, and a failed save. Assert catalog and HTTP 403 agree for direct and alias requests, with no upstream inference call on denial. Test that a capable provider shows a switch while an unsupported provider does not, and that toggling one does not change another.
