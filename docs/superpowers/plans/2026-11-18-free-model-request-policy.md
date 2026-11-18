# Free-only request enforcement

**Goal:** The existing free-only setting also refuses inference requests for models excluded by that policy.

**Design:** Keep raw discovery and alias management intact. Evaluate the resolved provider and model against the latest catalog and existing provider scope after authentication, before surface validation or forwarding. In free-only mode only confirmed-free OpenRouter/NVIDIA models may be used; unknown, absent, static/unverified and aliased blocked targets return HTTP 403 in the route's error schema. Other providers and the off state retain current routing. Existing in-flight requests are not cancelled. Catalog prices are evidence, not a billing guarantee.

**Plan (inline):**
- [x] Add a local HTTP fixture recording forwarded requests; reproduce direct and alias bypasses on chat, Responses and Messages, including unknown models and NVIDIA.
- [x] Implement the shared policy check using the existing classification and resolved alias target, without rebuilding the catalog on each request.
- [x] Prove free models, nonrestricted providers and disabled mode still relay; prove toggle and restart enforce the saved policy, discovery failure revokes permission and authentication still precedes policy.
- [x] Update README, models page copy and YAML comments; verify all tests, format, Clippy and types; commit locally only when done. No version bump, merge or push.

Evidence: Before the fix, the paid-model request test got 200 instead of 403. After the fix, all 24 denied direct/alias requests across the three routes return 403 and the fixture records zero inference calls. Full verification: 309 Rust passed (1 ignored), 105 Pi passed, Clippy and TypeScript clean.
