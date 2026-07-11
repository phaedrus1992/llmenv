# Issue #530 — Provider/model config: user-facing docs + changelog

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/530 (part of #508)
- **Milestone:** Large Projects
- **Type:** Documentation only
- **Difficulty:** Easy — but **blocked** until #526, #527, #528, #529 have all
  landed. This issue documents shipped behavior, not the plan.

## Authoritative spec

The full step-by-step instructions are **Task 8** of
`docs/superpowers/plans/2026-07-01-provider-model-config.md` (lines ~1230–1270).
Follow that task exactly. This design doc only adds ordering and acceptance
context; where the two disagree, the plan doc wins — except that the docs must
describe the code **as actually merged**, which wins over both.

## Scope summary

1. Find where `lsp:`/`mcp:` config keys are documented
   (`grep -rln "lsp:" docs/ README.md | grep -v superpowers`) and add
   `model_providers` / `default_models` sections **in the same format** —
   field tables/YAML examples matching the existing style, not a new format.
2. Document the full field list of `ModelProvider`, `ModelSource`,
   `ModelCost`, `ModelRef` **as they exist in the merged code**
   (`crates/llmenv-config/src/schema.rs`) — re-verify field names against the
   code, since the plan may have drifted during implementation of #526–#528.
   Include a minimal example (the Ollama round-trip example from Task 1 of
   the plan works).
3. Document `default_models` role-map shape with both `large` and `small`.
4. Invoke the `keepachangelog` skill to add an `[Unreleased]` entry for the
   new capability (new config keys + CrushAdapter rendering support). Per
   `AGENTS.md`, when editing `CHANGELOG.md` also reconcile against the older
   release line for missing forward-merged fixes.

## Acceptance criteria

- [ ] Docs cover every field of `ModelProvider`/`ModelSource`/`ModelCost`/
      `ModelRef` as merged, with a minimal example, in the same doc/format
      where `lsp`/`mcp` live.
- [ ] `default_models` documented with a two-role example.
- [ ] `CHANGELOG.md` has an `[Unreleased]` entry (keepachangelog format).
- [ ] `cargo test --test docs_sync` and `--test readme_links` still pass
      (repo has doc-consistency tests).

## Out of scope

- Any code or schema change. If docs reveal a code bug, file an issue.
