# Issue #529 — Provider/model config: property tests for CrushAdapter rendering

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/529 (part of #508)
- **Milestone:** Large Projects
- **Type:** Test-only
- **Difficulty:** Easy — literal test code is already written in the plan.
  **Blocked** until #528 (adapter rendering) lands.

## Authoritative spec

**Task 7** of `docs/superpowers/plans/2026-07-01-provider-model-config.md`
(lines ~1142–1226) contains the complete, literal test code to add. Follow it
verbatim, with one caveat: if the merged #528 code renamed
`render_model_providers` / `render_default_models` or changed their
signatures, adapt the tests to the merged names — the merged code wins over
the plan.

## Scope summary

Add three proptest cases to the existing `proptest! { ... }` block in
`src/adapter/crush.rs`, alongside `prop_render_lsp_keys_match_non_disabled_servers`:

1. `prop_render_model_providers_keys_match_non_disabled` — output key set ==
   non-disabled provider ids exactly.
2. `prop_render_model_providers_no_panic` — arbitrary id/base_url/api_key
   strings never panic.
3. `prop_render_default_models_no_panic` — arbitrary role/provider/model
   strings never panic.

## Steps

1. Read the existing `proptest!` block in `src/adapter/crush.rs` to confirm
   module paths (`super::` vs crate paths) and the `ModelProvider` field
   names as merged.
2. Copy the three tests from plan Task 7 Step 1, adjusting names only if the
   merged code differs.
3. `cargo test --lib prop_render_model_providers prop_render_default_models`
   — expect 3 passes at default case count.
4. `cargo fmt` + `cargo clippy --all-targets --all-features -- -D warnings`.

## Acceptance criteria

- [ ] All three property tests pass at proptest's default case count.
- [ ] No new dependencies (proptest is already a dev-dependency).
- [ ] Tests live inside the existing `proptest!` block, matching the
      `prop_render_lsp_*` pattern.
- [ ] No CHANGELOG entry (test-only).
