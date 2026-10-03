# Issue #2345 — doctor accounts for `autoCompactEnabled` and `autoCompactWindow`

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2345
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** bug fix (doctor gives wrong advice when a settings key changes what the env var does)
- **Related:** #2145 (the same fix for `bashOutputMaxChars` and the cache TTL)

This is a spec, not a plan.

## Problem

Doctor's token-efficiency section recommends `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=50` whenever the variable is unset, and warns above 70.
Two Claude Code settings change what that variable does:

- `autoCompactEnabled: false` turns automatic compaction off, so the percentage has nothing to act on and the "set it to 50" advice is wrong.
- `autoCompactWindow` sets the window the percentage applies to, which changes what "50" means.

Doctor reads neither.

## Claude Code facts (settings reference, env-vars reference, model-config page, fetched 2026-10-02; binary 2.1.288)

| Fact | Detail |
| --- | --- |
| `autoCompactEnabled` | boolean, any settings file; `false` turns automatic compaction off; default unset (on) |
| `autoCompactWindow` | number of tokens from `100000` to `1000000`, or `"auto"`; any settings file; `/autocompact` saves per-model values under `modelSettings` (per-model override since 2.1.288, takes precedence over the top-level key in the same file) |
| The percentage override | "the percentage (1-100) of the auto-compact window at which auto-compaction triggers"; it cannot raise the threshold (values above the default are ignored); it applies only in sessions that compact before the model's context limit |
| Which sessions compact early | models with a native 1M window compact at about 967K by default; 200K models compact at the limit unless a window is set; cloud sessions compact as they approach the limit |
| Variable name | the docs spell it `CLAUDE_CODE_AUTOCOMPACT_PCT_OVERRIDE`; the 2.1.288 binary contains `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` (8 hits) and the `CLAUDE_CODE_` form zero times. llmenv uses the name the binary reads |
| Fork note | the binary has a log line saying a fork ignores the override (issue text); not relevant to doctor's advice |

Re-check the variable name against the binary whenever Claude Code is bumped; if the `CLAUDE_CODE_` form appears, doctor should accept both and recommend the documented one.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `run_doctor_token_efficiency(use_color, pass, warn, cm_enabled, native_claude_env, native_claude_settings)` holds the inline `match get("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE")`: pass at ≤ 70, warn above, warn on non-numeric, warn when unset with "recommend 50" | `src/cli/doctor.rs` |
| `bashOutputMaxChars` (#2145) reads `native_claude_settings.get("bashOutputMaxChars")` from the merged manifest's `native.claude_code`, then `print_check(bash_output_check(setting, env_value), …)`; the setting wins | `src/cli/doctor.rs` |
| `effective_token_efficiency_var(native_claude_env, key)`: process environment wins, else `native.claude_code.env[key]` | `src/cli/doctor.rs` |
| `native_claude_settings` is `None` when `build_manifest` returns `None` (no bundle fired) | `src/cli/doctor.rs` |
| Rendered files are read only by `report_retired_claude_settings` through `rendered_claude_dir` and `read_rendered_json` (`settings.json` and `.claude.json` under `CLAUDE_CONFIG_DIR`) | `src/cli/doctor.rs` |
| Tests: `bash_output_max_chars_wins_over_the_env_var`, `bash_output_max_chars_non_numeric_warns_with_the_value`, `bash_output_falls_back_to_the_env_var` call `bash_output_check` with a `serde_yaml::Value` and an `Option<String>` | `src/cli/doctor.rs` tests |
| Nothing in the repo mentions `autoCompactEnabled` or `autoCompactWindow` | repo |
| Docs: the token-efficiency bullet list in `website/docs/commands.md` `## doctor`; the v3.12.0 token-efficiency note in `troubleshooting.md` | docs |

## Decisions

1. **Extract the check into a pure function** `autocompact_check(settings: Option<&Value>, override_value: Option<String>) -> (CheckLevel, String)`, the same shape as `bash_output_check`, and print it with `print_check`.
2. **Settings come from the merged manifest** (`native.claude_code`), as `bashOutputMaxChars` does.
   That is what llmenv renders; keys a user sets in a project `.claude/settings.json` are outside llmenv's view and the message says so when it matters (decision 5).
   Also read `modelSettings.*.autoCompactWindow`: if any model has one, treat the window as set.
3. **`autoCompactEnabled: false` wins.**
   Output `{info} autoCompactEnabled is false; automatic compaction is off and CLAUDE_AUTOCOMPACT_PCT_OVERRIDE has no effect`.
   No recommendation.
   If the override is also set, add `; the override can be removed`.
4. **A window changes the message, not the level.**
   With `autoCompactWindow` set (top-level or per model): `{pass|warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=<pct> of autoCompactWindow <window>` with the same ≤ 70 rule; when unset: `{warn} … not set (recommend 50; applies to autoCompactWindow <window>)`.
   `"auto"` prints as `auto`.
5. **The unset-override warning gains one clause**: `(only matters in sessions that compact before the model's limit)`, so a 200K-model user is not told to fix something that does nothing for them.
6. **Keep the 70 and 50 numbers**, with a named constant each and a comment that says why (PreCompact cleanup needs headroom; see the slippage and PreCompact docs).

## Design

### `src/cli/doctor.rs`

```rust
const AUTOCOMPACT_OVERRIDE_VAR: &str = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE";
/// Above this, PreCompact hooks have too little room to run before the cut.
const AUTOCOMPACT_PCT_MAX_RECOMMENDED: u32 = 70;
const AUTOCOMPACT_PCT_RECOMMENDED: u32 = 50;

fn autocompact_window(settings: Option<&Value>) -> Option<String>  // top-level or any modelSettings entry
fn autocompact_check(settings: Option<&Value>, override_value: Option<String>) -> (CheckLevel, String)
```

`run_doctor_token_efficiency` replaces the inline `match` with
`print_check(autocompact_check(native_claude_settings, get(AUTOCOMPACT_OVERRIDE_VAR)), pass, warn, info)`.
If that function is near the 100-line limit, move the whole autocompact block into a sibling helper.

### Docs

- `website/docs/commands.md` `## doctor`, token-efficiency bullets: the autocompact line now reads `autoCompactEnabled` and `autoCompactWindow`, `(changed in v3.12.0)`.
- `website/docs/troubleshooting.md`: one sentence that with `autoCompactEnabled: false` doctor reports info and recommends nothing.
- Changelog `Fixed`: doctor no longer recommends an autocompact percentage when automatic compaction is off, and names the window the percentage applies to.

## Tests

Table-driven over `(autoCompactEnabled, autoCompactWindow, modelSettings window, override)`:

1. enabled unset, window unset, override unset → warn, message contains `recommend 50` and `compact before the model's limit`.
2. enabled unset, override `50` → pass.
3. enabled unset, override `85` → warn with `≤70`.
4. enabled unset, override `abc` → warn, non-numeric, value in message.
5. enabled `false`, override unset → info, no `recommend`.
6. enabled `false`, override `50` → info, `can be removed`.
7. window `200000`, override `50` → pass, message contains `autoCompactWindow 200000`.
8. window `"auto"`, override unset → warn, message contains `auto`.
9. only `modelSettings.claude-opus-5.autoCompactWindow: 400000` → treated as set.
10. `settings == None` behaves as all unset.
11. Property test: the level is `Info` whenever `autoCompactEnabled` is `false`, for any other inputs.

## Acceptance criteria

1. With `native.claude_code.autoCompactEnabled: false` in config, `llmenv doctor` prints an info line and no recommendation.
2. With `autoCompactWindow: 200000` and the override unset, the warning names the window.
3. Existing behavior unchanged when neither key is set.
4. Changelog entry; commands and troubleshooting docs updated.

## Out of scope

- Reading project-level `.claude/settings.json` files outside `CLAUDE_CONFIG_DIR`.
- Validating `autoCompactWindow` values (Claude Code does that).
- `MAX_MCP_OUTPUT_TOKENS` and `CLAUDE_CODE_SUBAGENT_MODEL`, confirmed to have no overriding setting.
