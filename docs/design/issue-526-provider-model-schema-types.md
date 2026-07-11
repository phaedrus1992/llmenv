# Issue #526 — Provider/model config: schema types + validation

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/526 (part of #508)
- **Milestone:** Large Projects
- **Type:** Feature (foundation for #527/#528/#529/#530)
- **Difficulty:** Moderate but fully specified — the plan contains exact code
  and tests. **First** of the #508 sub-issues; nothing blocks it.

## Authoritative spec

**Tasks 1–2** of `docs/superpowers/plans/2026-07-01-provider-model-config.md`
(Task 1: lines ~41–296, Task 2: lines ~297–498). The plan has exact type
definitions, file:line targets, and TDD test content. Follow it task-by-task,
test-first. Also read the plan's **Global Constraints** section (lines
~20–40) before starting.

## Scope summary

1. **Types** (`crates/llmenv-config/src/schema.rs`): add `ModelProvider`,
   `ModelSource`, `ModelCost`, `ModelRef`, plus
   `Capabilities.model_providers: Vec<ModelProvider>` and
   `Capabilities.default_models: BTreeMap<String, ModelRef>` fields.
2. **Derive change:** drop `Eq` from `Capabilities` (keep `PartialEq`) —
   `ModelCost` carries `f64` fields, which are not `Eq`. Already confirmed
   nothing requires `Capabilities: Eq`; if the compiler finds a consumer that
   does, fix that consumer to use `PartialEq`, don't hack around it.
3. **Validation** in the config crate's `validate()`: reject
   - empty or duplicate provider `id`,
   - empty or duplicate model `id` *within* a provider,
   - empty role, provider, or model strings in `default_models`.

## Steps

1. Write the failing round-trip and validation tests from plan Tasks 1–2
   first (YAML round-trip for `ModelProvider`/`ModelSource`/`default_models`;
   the validation rejection cases).
2. Add the types and fields; make serde attributes match the plan
   (defaults/skip-serializing so existing configs without these keys still
   parse and re-serialize byte-identically).
3. Implement the validation rules.
4. Run the **full** `llmenv-config` suite plus the workspace suite —
   the `Eq` removal is the regression risk to watch.
5. `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`.

## Acceptance criteria

- [ ] YAML round-trip tests pass for `ModelProvider`/`ModelSource`/
      `default_models`.
- [ ] `validate()` rejects duplicate/empty ids per plan Task 2's tests.
- [ ] Full `llmenv-config` + workspace test suites pass; no `Eq`-removal
      fallout.
- [ ] Existing configs (fixtures under `tests/fixtures/`) still parse.
- [ ] No CHANGELOG entry yet — #530 handles docs+changelog once the whole
      #508 stack lands.

## Out of scope

- Merge behavior (#527), adapter rendering (#528), property tests (#529),
  docs (#530).
