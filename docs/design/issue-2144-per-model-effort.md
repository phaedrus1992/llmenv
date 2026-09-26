# Issue #2144 — per-model effort level for Claude Code

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2144
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** bug fix with a new config field

This is a spec, not a plan.

## Problem

llmenv renders `capabilities.effort_level` as a top-level `effortLevel` key in `$CLAUDE_CONFIG_DIR/settings.json`.
Claude Code treats that file as the **user** settings file.

Claude Code's documentation (settings reference, `effortLevel`, fetched 2026-09-26) says:

> In your user settings file, `~/.claude/settings.json`, this key is the older form `/effort` wrote before it saved levels per model, and it keeps applying where it applied before, on Opus 5, Fable 5.1, and earlier models.
> Opus 5.5 and models released after it ignore it and start at their own default until you save a level for them, which `/effort` writes under `modelSettings`.

So on Opus 5.5, the current default Opus model, llmenv's `effort_level` does nothing.
The slippage-control feature (#317) has the same defect: `slippage.effort_level` flows into the same scalar.

Two smaller defects:

1. `effort_level` accepts any string. Claude Code accepts only `low`, `medium`, `high`, `xhigh` in settings. `max` is session-only (through `CLAUDE_CODE_EFFORT_LEVEL`), and `ultracode` has its own key.
2. There is no way to cap effort (`maxEffortLevel`).

## Claude Code facts (docs, fetched 2026-09-26)

| Fact | Source |
| --- | --- |
| `modelSettings` type: object mapping a model name to `{ "effortLevel"?: "low"\|"medium"\|"high"\|"xhigh", "maxEffortLevel"?: "low"\|"medium"\|"high"\|"xhigh"\|"max" }` | settings reference, `modelSettings` |
| Entries are keyed by canonical model ID (for example `claude-opus-5-5`); Claude Code matches aliases, date suffixes, `[1m]` and provider IDs to that entry | same |
| Within one file, a model's `modelSettings` entry beats top-level `effortLevel` | same |
| `/effort` in an interactive session writes `modelSettings.<model>.effortLevel` into the user settings file | same |
| `maxEffortLevel` `"max"` means no cap; requires Claude Code 2.1.267 or later | settings reference, `maxEffortLevel` |
| `modelSettings` requires Claude Code 2.1.251 or later | settings reference, `modelSettings` |
| `CLAUDE_CODE_EFFORT_LEVEL` and `--effort` beat every settings key | model-config, "Adjust effort level" |
| Opus 5.5 defaults to `medium`; most other models default to `high`; Opus 4.7 defaults to `xhigh` | same |

Under llmenv, `/effort` writes into llmenv's rendered `settings.json`, because that file is the user settings file.
llmenv must therefore merge into `modelSettings` and must not overwrite entries it did not write.

## Verified locations (release/3.x)

| Fact | Location |
| --- | --- |
| `Capabilities.effort_level: Option<String>`, unvalidated | `crates/llmenv-config/src/schema.rs` near line 534 |
| `SlippageControl.effort_level: Option<String>` | same file near line 1146 |
| Slippage value propagates when no higher-precedence `effort_level` is set | `src/merge/mod.rs` near line 138 |
| `effort_level` is a known bundle key | `BUNDLE_YAML_KNOWN_KEYS`, `src/merge/mod.rs` near line 241 |
| Renders `settings.insert("effortLevel", …)` | `src/adapter/claude_code.rs`, render function near the `advisorSize` insert |
| `effortLevel` is in `LLMENV_OWNED_SETTINGS_KEYS` | `src/adapter/claude_code.rs` |
| Pattern for tracking what llmenv wrote into a shared file | `.claude.json.llmenv-owned`, `CLAUDE_JSON_OWNED_SERVERS_FILE`, `src/adapter/claude_code.rs` near line 606 |
| Per-key map merge with equal-precedence conflict as a hard error | `resolve_default_models`, `src/merge/capabilities.rs` near line 468 |

## Config

```yaml
capabilities:
  # Default for every model. See "Rendering" for how it reaches models that ignore the top-level key.
  effort_level: high
  # Per-model overrides. Keys are canonical Claude model IDs.
  model_effort:
    claude-opus-5-5:
      effort_level: xhigh
      max_effort_level: xhigh
    claude-fable-5-1:
      max_effort_level: high
```

New field on `Capabilities`: `model_effort: BTreeMap<String, ModelEffort>`, `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]`.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEffort {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_effort_level: Option<String>,
}
```

Use `String`, not an enum, to match the existing `effort_level` field and its native-override style; validation (below) enforces the values.
Add `model_effort` to `BUNDLE_YAML_KNOWN_KEYS`.
`Capabilities` derives `Debug, Clone, Deserialize, Serialize, Default, PartialEq` on `release/3.x` and has no `JsonSchema` derive, so `ModelEffort` does not need one either.

### Validation (`crates/llmenv-config/src/validate.rs`)

New `ValidateError` variants, each with a message that names the field, the bad value, and the allowed values:

1. `capabilities.effort_level` and `slippage.effort_level`: one of `low`, `medium`, `high`, `xhigh`.
   For `max`, the message adds: `max applies to one session only; set CLAUDE_CODE_EFFORT_LEVEL=max in native.claude_code.env instead.`
   For `ultracode`, the message adds: `set native.claude_code.ultracode: true instead.`
2. `model_effort.<id>.effort_level`: same set as item 1.
3. `model_effort.<id>.max_effort_level`: one of `low`, `medium`, `high`, `xhigh`, `max`.
4. `model_effort.<id>` with neither field set: error, `set effort_level, max_effort_level, or both`.
5. `model_effort` key: must start with `claude-`, and must not be one of the aliases `default`, `best`, `fable`, `sonnet`, `opus`, `haiku`, `opusplan`, or contain `[`.
   The message says: `use the canonical model ID, such as claude-opus-5-5; Claude Code matches aliases to that entry itself.`

Validation runs on the merged result and on each contributor, the same way the existing capability checks run.
The current `effort_level` accepts any string, so a config with an invalid value that loads today fails after this change.
That is intended; the changelog entry says so.

### Merge

`model_effort` merges per model ID, like `resolve_default_models`: the highest-precedence contributor wins for a model ID; two contributors at the same precedence that disagree on the same model ID are a hard error with both names.
Inside one model ID the two fields are not merged separately: the winning contributor's whole `ModelEffort` value wins.

## Rendering (Claude Code adapter)

### Constant

```rust
/// Models that ignore a top-level effortLevel in user settings (Claude Code
/// docs, `effortLevel`). Add each new Claude model here when it ships.
const PER_MODEL_EFFORT_MODELS: &[&str] = &["claude-opus-5-5"];
```

### What llmenv writes

Build the set of entries llmenv manages, `managed: BTreeMap<String, Map>`:

1. Start empty.
2. If the merged `effort_level` is set: write top-level `effortLevel` (as today, for Opus 5, Fable 5.1 and earlier), and for each ID in `PER_MODEL_EFFORT_MODELS`, set `managed[id].effortLevel = effort_level`.
3. For each `model_effort` entry: set `managed[id].effortLevel` if `effort_level` is set, and `managed[id].maxEffortLevel` if `max_effort_level` is set. A `model_effort` entry replaces the fields from step 2 for that ID.

### Merge into the file

`modelSettings` is shared with Claude Code's own `/effort`.
Do not add it to `LLMENV_OWNED_SETTINGS_KEYS`.
Use a companion file, `settings.json.llmenv-owned-model-settings`, next to `settings.json`, with the same write and permission rules as `.claude.json.llmenv-owned`.
It holds a JSON object: the exact `managed` map llmenv wrote on the previous render.

On each render, with `current` = `modelSettings` from the existing `settings.json` (or `{}`), and `prev` = the companion file (or `{}`):

1. For each ID in `prev` but not in `managed`: if `current[id]` equals `prev[id]` exactly, remove `current[id]`. Otherwise the user changed it with `/effort`; leave it.
2. For each ID in `managed`: for each field llmenv manages for that ID, set `current[id][field]`. Keep any other field in `current[id]`.
3. For each field llmenv managed last time for an ID it still manages but no longer sets: remove that field from `current[id]` if its value equals `prev[id][field]`.
4. Remove an ID whose object is empty after steps 1 to 3.
5. Write `modelSettings` only if `current` is not empty; else remove the key.
6. Write the companion file with `managed`, or delete it if `managed` is empty.

A config value for a model wins over an earlier `/effort` save for that model at the next render.
State this in the docs: to keep a level picked with `/effort`, do not also set it in llmenv config.

The render must not read the companion file when `settings.json` does not exist (first render); treat both as empty.

### Slippage

No change to slippage code: its value already reaches `capabilities.effort_level`, and the rendering above makes it reach Opus 5.5.

### Other adapters

Crush and opencode do not read `effort_level` today. They ignore `model_effort` too. Do not add rendering for them.

## Tests

1. Validation table: every allowed and rejected value for each of the five rules, including `max` and `ultracode` messages.
2. Merge: higher precedence wins per ID; equal precedence conflict is an error naming both contributors; equal precedence with equal values is fine.
3. Render, fresh file: `effort_level: high` gives top-level `effortLevel: high` and `modelSettings.claude-opus-5-5.effortLevel: high`; companion file matches.
4. Render keeps a foreign entry: existing `modelSettings.claude-sonnet-5.effortLevel: low` written by `/effort` survives.
5. Render removes its own stale entry: remove `model_effort.claude-fable-5-1` from config; the entry goes if unchanged, stays if the user changed it.
6. Render keeps user fields: `/effort` added `effortLevel` to an entry where llmenv manages only `maxEffortLevel`; both survive.
7. Property test: rendering twice with the same config gives a byte-identical `settings.json` (idempotence).
8. Slippage: `slippage.effort_level: xhigh` with no `effort_level` gives `modelSettings.claude-opus-5-5.effortLevel: xhigh`.

## Acceptance criteria

1. With `capabilities.effort_level: high` and Opus 5.5 as the model, the Claude Code session header says "with high effort".
2. `/effort low` in a session, then `llmenv regenerate`, keeps `low` for a model llmenv does not manage.
3. Changelog `Fixed`: `effort_level` now reaches Opus 5.5. `Added`: `capabilities.model_effort`. `Changed`: invalid `effort_level` values now fail validation.
4. `website/docs/` capabilities page documents `effort_level` (with the Opus 5.5 note), `model_effort`, the allowed values, the `/effort` interaction, and the `PER_MODEL_EFFORT_MODELS` maintenance note. Tag `model_effort` `(added in v3.12.0)` and `effort_level` `(changed in v3.12.0)`.
5. `release/3.x` has no generated config schema file, so no schema output needs regenerating.

## Out of scope

- Setting `CLAUDE_CODE_EFFORT_LEVEL` from llmenv. It overrides `/effort` and `--effort`, which is too strong for a default.
- The `ultracode` key. Users can set it through `native.claude_code`.
