# Free model catalog

## Goal

Agents should discover only models advertised as free on OpenRouter and NVIDIA by default. Other providers, existing inference routes, and OpenAI client URLs stay unchanged. This is a listing policy, not a spending limit or a guarantee of account access.

## Approach

Use a server-wide YAML default and a persistent `/models` toggle. A query parameter was rejected because existing clients should work unchanged. A page-only toggle would not change agent discovery. Store the override separately rather than rewriting a possibly read-only YAML mount.

- Top-level `free_models_only: true` is the default when omitted. It applies only to OpenRouter and NVIDIA, including named upstreams of those kinds.
- With the toggle on, `/v1/models` lists only confirmed free models from those two providers. Aliases inherit their target's listing visibility. With it off, all discovered models are listed. `/models` shows the effective toggle and badges confirmed models.
- Only the global proxy token can change the setting, following alias-management authorization. Persist an override atomically in `aiproxy-model-visibility.json` in the existing runtime state directory. A saved override takes precedence over YAML; without one, YAML applies. A failed write does not change the live setting. Do not rewrite YAML or alter inference routing.

## Evidence used for badges

- **OpenRouter:** use the upstream model record and its advertised prices. Require zero prompt and completion prices and no nonzero, dynamic, or unparseable advertised charges. Do not rely solely on the `:free` suffix.
- **NVIDIA:** intersect exact provider/model IDs from the inference catalog with NVIDIA's public **Free Endpoint** listings. First prove that a read-only backend fetch can retrieve the complete filtered result, including pagination. The first unfiltered HTML page is not sufficient. An unmatched, incomplete, or failed classification is unknown, not free or paid.
- Refresh classification on the existing discovery cadence (startup, manual reload, or periodically when `model_refresh_secs` is enabled). Never broaden the free-only list on an upstream failure. Do not use pi.dev, UI cookies, or inference requests for classification.

## Acceptance

Verify both sources with fixtures and live read-only catalog checks. Test default and opt-out behavior, unknown classifications, aliases, unaffected providers, unauthorized writes, persistence across restart, and failure to persist without a live state change.
