# Issue #527 — Provider/model config: merge rules

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/527 (part of #508)
- **Milestone:** Large Projects
- **Type:** Feature
- **Difficulty:** Moderate, fully specified. **Blocked** until #526 (schema
  types) lands.

## Authoritative spec

**Task 3** of `docs/superpowers/plans/2026-07-01-provider-model-config.md`
(lines ~499–769) — exact code and test content. Follow it test-first. The
merged #526 types win over the plan if names drifted.

## Scope summary

Two additions to the merge layer:

1. **`model_providers` accumulation** in `merge_capabilities`
   (`src/merge/capabilities.rs`): concat across contributors + dedup exact
   duplicates — the *same* pattern already used for `lsp`/`mcp`/`skills`.
   Deliberately **no** override-by-id logic at merge time; override-by-id
   falls out of CrushAdapter's render-into-a-map-keyed-by-id step (#528).
2. **`resolve_default_models()`** — new function modeled directly on the
   existing `resolve_env()` in the same module:
   - per-role-key resolution, highest precedence wins;
   - two contributors at the *same* precedence disagreeing on the same role
     is a hard error;
   - a conflict on one role must not affect resolution of other roles.

## Steps

1. Read `merge_capabilities` and `resolve_env` in
   `src/merge/capabilities.rs` first; mirror their structure and error
   types exactly.
2. Write the failing tests from plan Task 3 (concat+dedup; per-role
   precedence; same-precedence conflict errors; conflict isolation between
   roles).
3. Implement; keep functions ≤100 lines / complexity ≤8 (split helpers if
   needed, as `resolve_env` likely already demonstrates).
4. Run the merge suites: `cargo test --test merge --test merge_proptest`
   plus the full workspace suite.
5. `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`.

## Acceptance criteria

- [ ] `model_providers` concatenates across contributors and dedups exact
      duplicates.
- [ ] `default_models` resolves independently per role; same-precedence
      conflict on one role errors, other roles unaffected.
- [ ] Error message on conflict names the role and both contributors
      (fail fast with context).
- [ ] No regressions in existing `lsp`/`mcp`/`env` merge tests.
- [ ] No CHANGELOG entry yet (#530 covers the stack).

## Out of scope

- Adapter rendering (#528) — no adapter files touched here.
