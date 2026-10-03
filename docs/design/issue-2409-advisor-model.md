# Issue #2409 — replace `capabilities.advisor_size` with `advisor_model`

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2409
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** bug fix with a breaking config change (a field is renamed; the old name becomes a validation error)
- **Supersedes:** #2328 (its premise, a set of accepted size values, does not hold)
- **Decision owner:** ranger chose "replace with `advisor_model`" over "remove" on 2026-10-02

This is a spec, not a plan.

## Problem

`capabilities.advisor_size` renders `advisorSize` into `settings.json`.
Claude Code has no such setting.
The 2.1.288 binary contains `advisorModel` 90 times and `advisorSize` zero times.
The value is ignored with no message, so a user who sets it gets nothing.

## Claude Code facts (settings reference and advisor page, fetched 2026-10-02)

| Fact | Detail |
| --- | --- |
| Key | `advisorModel`, a string, in any settings file (user, project, local, managed) |
| Values | one of the aliases `fable`, `opus`, `sonnet` (resolve to Claude Code's current default version of that family), or a full model id such as `claude-opus-5-5` |
| Default | unset, which turns the advisor off |
| Overrides | `--advisor <model>` for one session; `/advisor <model>` saves to user settings; `/advisor off` unsets; `CLAUDE_CODE_DISABLE_ADVISOR_TOOL=1` turns the tool off and ignores the setting |
| Pairing | the advisor must rank at or above the main model; Claude Code checks before sending, the API checks again. llmenv does not need to validate pairing |
| Availability | Anthropic API only; needs feature-flag fetching (off when `DISABLE_TELEMETRY` is set) |

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `Capabilities.advisor_size: Option<String>`, doc comment "small/medium/large", no validation, counted in `Capabilities::is_empty()` | `crates/llmenv-config/src/schema.rs` |
| Merge: `highest_precedence(contributors, "advisor_size", …)`; test `advisor_size_scalar_resolution` | `src/merge/capabilities.rs` |
| `advisor_size` is in `BUNDLE_YAML_KNOWN_KEYS` | `src/merge/mod.rs` |
| Render: `settings.insert("advisorSize", …)` right after the `effortLevel` insert, under the "#221: render first-class capability fields" comment | `src/adapter/claude_code/mod.rs` |
| `LLMENV_OWNED_SETTINGS_KEYS: [&str; 12]` includes `advisorSize`; the array length is part of the type | `src/adapter/claude_code/mod.rs` |
| Tests that name it: `manifest_with_native_override`, `native_null_removes_a_rendered_settings_key` (loops over `autoMemoryEnabled`, `effortLevel`, `advisorSize`), `OVERRIDABLE_SETTINGS_KEYS: [&str; 4]`, `arb_merged_manifest`; regression seeds in `proptest-regressions/adapter/claude_code.txt` and `crates/llmenv-config/proptest-regressions/validate.txt` | adapter and config tests |
| History: added as `advisor_model`, renamed to `advisor_size` in the #221/#296 work; the merge drop was fixed later | `git log -S advisorSize` |
| Retired-key table: `const RETIRED: &[Retired]` built from `row(SettingsKey, name, no_effect, since, replacement)`; `scan()` checks top-level `settings.json` keys; the `table_shape` test enforces no duplicates and a replacement for each deprecated row | `src/adapter/claude_code/retired.rs` |
| Validation lives in `impl Config { fn validate() }` with `ValidateError` (thiserror); `validate_effort` in `effort.rs` is the model for a small value check | `crates/llmenv-config/src/validate.rs`, `crates/llmenv-config/src/effort.rs` |
| `Capabilities` is not `deny_unknown_fields`, so an unknown `advisor_size` would be dropped silently unless handled | `crates/llmenv-config/src/schema.rs` |
| Docs: `capabilities:` table in `website/docs/configuration.md` does not list `advisor_size`; the changelogs mention it | docs |

## Decisions

1. **Rename the field to `advisor_model`** and render `advisorModel`.
   The field name follows the Claude Code key, as `effort_level` → `effortLevel` does.
2. **Validate shape, not meaning.**
   Accept exactly `fable`, `opus`, `sonnet`, or a string that matches `claude-[a-z0-9]+(-[a-z0-9]+)*`.
   Reject anything else with the field name, the value, and the accepted forms.
   Do not encode the pairing table; Claude Code and the API own it and it changes every release.
3. **`advisor_size` becomes a hard error with the fix.**
   Keep a hidden `advisor_size: Option<String>` on `Capabilities` marked `#[serde(default, skip_serializing)]` only so `validate()` can see it and say `capabilities.advisor_size was removed; set capabilities.advisor_model to fable, opus, sonnet, or a model id`.
   Do not merge or render it.
   Remove the hidden field in the next major.
4. **Old rendered settings get a doctor warning.**
   Add `advisorSize` to `RETIRED` as a removed key with `since` the llmenv version (not a Claude Code version) and the replacement `advisorModel`.
   `scan()` then warns on a stale `settings.json` until the user regenerates.
5. **`LLMENV_OWNED_SETTINGS_KEYS` swaps the name**, so `advisorModel` is owned and a `native.claude_code.advisorModel: null` can remove it, and the old `advisorSize` key is cleared from a rendered file on the next render (check how the owned-keys cleanup treats a key that is no longer in the list; if it would leave `advisorSize` behind, add a one-off removal).
6. **Changelog has both a `Changed` and a `Removed` entry**, and the configuration docs gain the field with `(added in v3.12.0; replaces advisor_size)`.

## Design

### Schema (`crates/llmenv-config/src/schema.rs`)

- `pub advisor_model: Option<String>` with a doc comment that lists the accepted forms and links the Claude Code advisor page.
- `Capabilities::is_empty()` counts `advisor_model` instead of `advisor_size`.
- The hidden `advisor_size` per decision 3.

### Validation (`crates/llmenv-config/src/validate.rs`)

- New `ValidateError` variants: `AdvisorModelInvalid { value }` and `AdvisorSizeRemoved`.
- `validate_advisor_model(&str) -> Result<(), ValidateError>` in a small module next to `effort.rs` (`advisor.rs`): alias set, then the shape regex with `fullmatch` semantics (anchor the pattern).
- Called from `validate()` for the top-level capabilities and from the bundle validation path for bundle-level capabilities.

### Merge (`src/merge/capabilities.rs`, `src/merge/mod.rs`)

- Rename the `highest_precedence` key and the known-keys entry.
- Rename the test.

### Adapter (`src/adapter/claude_code/mod.rs`)

- Render `advisorModel` from `advisor_model` at the same spot.
- Update `LLMENV_OWNED_SETTINGS_KEYS` (same length, one name changes) and `OVERRIDABLE_SETTINGS_KEYS`.
- Regenerate or delete the affected proptest regression seeds (they encode the struct shape).

### Retired table (`src/adapter/claude_code/retired.rs`)

- One new row for `advisorSize` in the removed group, note "llmenv wrote this key by mistake before v3.12.0; regenerate to replace it with `advisorModel`".

### Docs

- `website/docs/configuration.md`, capabilities table: `advisor_model`, accepted values, the version tag, a link to the Claude Code advisor page, and one sentence that `--advisor` and `/advisor` override per session.
- `website/docs/troubleshooting.md`, retired settings: the `advisorSize` warning and the fix (`llmenv regenerate`).
- Changelog: `Changed`: `capabilities.advisor_size` is now `capabilities.advisor_model` and renders Claude Code's `advisorModel`. `Removed`: `advisor_size` (it rendered a key Claude Code never read); a config that still sets it fails validation with the fix.

## Tests

1. Validation table: `fable`, `opus`, `sonnet`, `claude-opus-5-5`, `claude-sonnet-5-5` accepted; `small`, `Opus`, `claude-`, `claude-opus-5-5 ` (trailing space), `gpt-5`, empty string rejected with the value in the message.
2. `advisor_size: medium` in config fails validation naming `advisor_model`.
3. Merge precedence test renamed and passing; a bundle and the top level both setting `advisor_model` resolve by precedence, and an equal-precedence conflict errors as the other scalar capabilities do.
4. Render: `advisor_model: opus` gives `settings.json` with `advisorModel: "opus"` and no `advisorSize`; unset renders neither.
5. `native_null_removes_a_rendered_settings_key` loops over the new key.
6. Owned-keys cleanup: a rendered `settings.json` that holds a stale `advisorSize` loses it on the next render.
7. Retired scan: `settings.json` with `advisorSize` produces one warning naming `advisorModel`; `table_shape` still passes.
8. Property test: any string matching the model-id shape is accepted and any string with a character outside `[a-z0-9-]` is rejected.

## Acceptance criteria

1. `capabilities.advisor_model: opus` renders `"advisorModel": "opus"` and Claude Code shows the advisor notification at session start.
2. `capabilities.advisor_size: medium` fails `llmenv regenerate` with a message that names `advisor_model`.
3. A pre-upgrade rendered config makes `llmenv doctor` warn about `advisorSize` until regenerated.
4. Changelog entries, configuration and troubleshooting docs updated.
5. #2328 is closed as superseded with a comment pointing here.

## Out of scope

- Validating advisor and main-model pairing.
- An `--advisor` flag passthrough or a per-model advisor in `model_effort`.
- Crush and opencode (no advisor concept).
