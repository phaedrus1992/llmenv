# Issue #2143 — stale and invalid Claude model IDs

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2143
- **Milestone:** `v3.11.2`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** bug fix

This is a spec, not a plan.

## Problem

1. The `anthropic-api` consolidation backend defaults to model `claude-sonnet-5-20250624`.
   That ID does not match any published model ID.
   The published Sonnet 5 ID is `claude-sonnet-5`.
   With no `ANTHROPIC_MODEL` set, every consolidation call through this backend gets an API error.
2. The same backend reads `ANTHROPIC_MODEL`.
   Claude Code reads the same variable and accepts aliases such as `opus`, `sonnet` or `sonnet[1m]`.
   A user who sets `ANTHROPIC_MODEL=opus` for Claude Code sends `"model": "opus"` to the raw Messages API, which rejects it.
3. `llmenv doctor` says the `CLAUDE_CODE_SUBAGENT_MODEL` default is `claude-sonnet-4-6`.
   llmenv cannot know the engine's default, and the value is out of date.
4. Docs and the example config show old model IDs.

## Current model IDs (2026-09)

| Model | ID |
| --- | --- |
| Opus 5.5 | `claude-opus-5-5` |
| Sonnet 5 | `claude-sonnet-5` |
| Fable 5.1 | `claude-fable-5-1` |
| Haiku 4.5 | `claude-haiku-4-5` |

## Verified locations (release/3.x)

| Location | Today | Change |
| --- | --- | --- |
| `src/consolidation/mod.rs:38` `DEFAULT_MODEL` | `claude-sonnet-5-20250624` | `claude-sonnet-5` |
| `src/consolidation/mod.rs:10` module doc | says `ANTHROPIC_MODEL` is required | say it is optional, and name the default |
| `src/consolidation/mod.rs:251` model lookup | `ANTHROPIC_MODEL` or default | see "Model selection" below |
| `src/cli/doctor.rs:83` | `not set (default: claude-sonnet-4-6)` | `not set (Claude Code picks the model per agent)` |
| `examples/config-llmenv-dir/config.yaml:270` | `CLAUDE_CODE_SUBAGENT_MODEL: "claude-sonnet-4-6"` | `"inherit"` with a one-line comment: `inherit` keeps each agent's own `model:` frontmatter |
| `website/docs/engines.md:234` | `model: claude-opus-4-5` | `model: claude-opus-5-5` |
| `website/docs/configuration.md:285` | `claude-haiku-4-5` | no change; Haiku 4.5 is current |

Do not change test fixtures that use model IDs as plain data:
`crates/llmenv-config/src/schema.rs` (`default_models_map_yaml_roundtrip`), `validate.rs`, `src/adapter/crush.rs`, `src/adapter/opencode.rs`, `src/merge/capabilities.rs`, `src/cli/statusline/widget.rs`.
They test parsing and pass-through, not model validity.
Do not change `docs/superpowers/plans/` or `docs/reference/config-updates.md`; they are historical records.

## Model selection (consolidation `anthropic-api` backend)

Rules, in order:

1. If `ANTHROPIC_MODEL` is set and its value starts with `claude-`, use it.
2. If `ANTHROPIC_MODEL` is set and does not start with `claude-`, do not use it.
   Log one `tracing::warn!`: `consolidation: ignoring ANTHROPIC_MODEL="<value>": the Messages API needs a full model ID such as claude-sonnet-5, not a Claude Code alias. Using <DEFAULT_MODEL>.`
3. Otherwise use `DEFAULT_MODEL`.

Put this in one pure function: `fn resolve_api_model(env_value: Option<&str>) -> (String, Option<String>)`.
It returns the model and an optional warning text.
The caller logs the warning.
This keeps the function testable without environment variables.

Do not add a new config key for the consolidation model.
A user who needs another model sets `ANTHROPIC_MODEL` to a full ID.

The `claude-cli` backend (`call_claude`) runs `claude -p` without `--model` and is not changed.

## Tests

1. `DEFAULT_MODEL == "claude-sonnet-5"` (a guard test; its failure message names this spec).
2. `resolve_api_model` table: `None`; `Some("claude-opus-5-5")`; `Some("opus")`; `Some("sonnet[1m]")`; `Some("")`. The empty string counts as "not a full ID" and warns.
3. `doctor` output test (if the doctor env checks already have tests): the not-set line contains `Claude Code picks the model per agent` and no model ID.

## Acceptance criteria

1. `git grep -n 'claude-sonnet-5-20250624\|claude-sonnet-4-6'` on the branch returns no hits outside the changelog.
2. A consolidation run with `consolidation.backend: anthropic-api`, a valid key and no `ANTHROPIC_MODEL` reaches the API and stores rules.
3. The same run with `ANTHROPIC_MODEL=opus` logs the warning and succeeds with the default model.
4. Changelog entry under `Fixed` in the 3.x changelog.
5. `website/docs/` has no section on post-session consolidation today (only a troubleshooting bullet).
   Add one to the memory docs page (the page that documents `features.memory`).
   It documents `consolidation.enabled` (default `false`), `consolidation.backend` (`claude-cli` default, or `anthropic-api`), the `ANTHROPIC_API_KEY` requirement for `anthropic-api`, the default model `claude-sonnet-5`, and the alias rule.
   Read the field docs on the consolidation config struct in `crates/llmenv-config/src/schema.rs` (near line 1242) for the full field list and document every field there.
   Tag the section with the version that first shipped consolidation (find it with `git log -S 'consolidation.backend' --reverse` and `git tag --contains`), and add `(model default changed in v3.11.2)`.

## Out of scope

- Replacing the hand-written Messages API client with an SDK.
- A config key for the consolidation model.
