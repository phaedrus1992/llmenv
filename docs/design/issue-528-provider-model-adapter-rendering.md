# Issue #528 — Provider/model config: adapter probe + CrushAdapter rendering

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/528 (part of #508)
- **Milestone:** Large Projects
- **Type:** Feature (includes one research sub-task)
- **Difficulty:** Moderate-hard — the only #508 sub-issue with an external
  unknown (catwalk field names). **Blocked** until #526 and #527 land.

## Authoritative spec

**Tasks 4–6** of `docs/superpowers/plans/2026-07-01-provider-model-config.md`
(Task 4: lines ~770–867, Task 5: ~868–917, Task 6: ~918–1141). Exact code,
research steps, and test content live there. Merged #526/#527 code wins over
the plan if names drifted.

## Scope summary

1. **Trait probe** (`src/adapter/mod.rs`): add
   `AgentAdapter::supports_model_providers()` — default/`false` for
   `ClaudeCodeAdapter`, `true` for `CrushAdapter`. Extend the existing
   `registry_adapter_trait_probes` test.
2. **Research (Task 5) — do this before writing render code:** confirm
   `catwalk.Model`'s exact JSON field names. catwalk is an external,
   unvendored Go module — use `go doc` against the module or clone it to
   `~/git/reference/catwalk` and read the struct tags. **Do not guess field
   names from memory.** Record the confirmed mapping in a code comment on
   the render function.
3. **Rendering** (`src/adapter/crush.rs`, inside
   `CrushAdapter::materialize()`):
   - `model_providers` → a `providers` JSON map keyed by provider id;
   - `default_models` → a `models` JSON map;
   - follow the exact map-insert-by-key pattern `render_lsp` already uses.
     Inserting by id into a map is what gives override-by-id semantics for
     free (later contributors overwrite earlier ones) — no merge-time
     override logic exists or should be added.
4. **ClaudeCode no-op guarantee:** `ClaudeCodeAdapter::materialize()` output
   must be byte-identical whether or not `model_providers`/`default_models`
   are populated. Write the test proving it (plan Self-Review gap-fix,
   folded into Task 4's test step).

## Steps

1. Do the catwalk research first (Task 5); the JSON field names gate
   everything in Task 6.
2. Write failing tests from plan Tasks 4 and 6: trait probe values; provider
   written under `providers.<id>`; empty `model_providers` omits the key
   entirely; `default_models` written under `models`; ClaudeCode
   byte-identical no-op.
3. Implement probe, then `render_model_providers` /
   `render_default_models` / wiring into `materialize()`, mirroring
   `render_lsp`'s structure and error handling.
4. Run adapter suites: `cargo test --test claude_code_adapter` plus crush
   unit tests and full workspace suite; check `tests/snapshots/` for any
   snapshot tests that need regenerating (inspect diffs — only
   crush-related snapshots may change).
5. `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`.

## Acceptance criteria

- [ ] `registry_adapter_trait_probes` extended and passing.
- [ ] Crush rendering tests pass: provider rendered, empty list omitted,
      default model rendered (plan Task 6 test set).
- [ ] Catwalk field mapping confirmed from source/`go doc`, documented in a
      comment with the upstream reference.
- [ ] ClaudeCodeAdapter byte-identical no-op test passes.
- [ ] No CHANGELOG entry yet (#530 covers the stack).

## Out of scope

- Property tests (#529) and docs (#530).
- Any other adapter (opencode etc.) — probe stays `false` for them.
