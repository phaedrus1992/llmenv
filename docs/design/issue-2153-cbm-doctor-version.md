# Issue #2153 — doctor warns on codebase-memory-mcp older than 0.11.0

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2153
- **Milestone:** `v3.11.2`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** bug fix (doctor)

This is a spec, not a plan.

## Problem

`llmenv doctor` prints the installed codebase-memory-mcp (cbm) version but has no minimum.
After #2151 and #2152, llmenv's permission tiers and model guidance assume cbm 0.11.0 tools (`get_file_outline`, `compare_graphs`).
On an older cbm, the model is told about tools that do not exist.

The first index after an upgrade from 0.10.8 or earlier is a full rebuild (index format change), and llmenv starts that index at every `SessionStart`.
Users should know before it happens.

**Not a problem (correction to the original issue).**
The issue said doctor's "`codebase-memory-mcp update` only reports" label is stale.
It is correct: in a release build of cbm 0.11.0, `cbm_cmd_update` (`src/cli/cli.c` near line 13424, `#ifndef CBM_CLI_ENABLE_TEST_API`) prints the install command and installs nothing.
Keep `UpdatePath::Reports("codebase-memory-mcp update")` unchanged.

## Verified locations (release/3.x)

| Fact | Location |
| --- | --- |
| `DEPENDENT_TOOLS` with `("codebase-memory-mcp", UpdatePath::Reports("codebase-memory-mcp update"))` | `src/cli/doctor.rs`, above `tool_version` |
| `tool_version(bin)` runs `<bin> --version`; `parse_version_line` returns the last whitespace token of the first line if it contains a digit | `src/cli/doctor.rs` near lines 257 to 285 |
| `run_doctor_dependent_tools` prints `{pass} <bin> <version> — check for updates with \`<cmd>\`` | `src/cli/doctor.rs` line 290 |
| cbm prints `codebase-memory-mcp 0.10.8` for `--version` | observed |
| No `semver` crate in the root `Cargo.toml` | `Cargo.toml` |

## Design

### Version floor

Add in `src/cli/doctor.rs`:

```rust
/// Oldest codebase-memory-mcp whose tool set matches llmenv's tier table and
/// model guidance (get_file_outline, compare_graphs).
const CBM_MIN_VERSION: (u64, u64, u64) = (0, 11, 0);

/// Parse `X.Y.Z` with an optional leading `v` and an optional `-suffix` or
/// `+suffix` on Z. `None` when the text does not have that shape.
fn parse_semver_triple(text: &str) -> Option<(u64, u64, u64)>
```

Parsing rules (match the shape, then check the meaning):

1. Remove one leading `v`.
2. Cut the text at the first `-` or `+`.
3. Split on `.`; there must be exactly three parts.
4. Each part is 1 to 9 ASCII digits (`[0-9]`, not `char::is_numeric`), parsed as `u64`.
5. Anything else returns `None`.

Do not add the `semver` crate for one comparison.

### Output

In `run_doctor_dependent_tools`, for `codebase-memory-mcp` only:

- Version parses and is below `CBM_MIN_VERSION`: print the tool line with the `{warn}` marker instead of `{pass}`, then one more line:
  `{warn} codebase-memory-mcp <version> is older than 0.11.0; llmenv's guidance names tools it lacks (get_file_outline, compare_graphs). The first index after upgrading rebuilds each project once.`
- Version parses and is at or above the floor: unchanged.
- Version unknown or does not parse: unchanged (`{info}` for unknown).

`run_doctor_dependent_tools` gets a warning marker from `super::doctor_warning(use_color)` (`src/cli/style.rs` line 96).

## Tests

1. `parse_semver_triple`: `0.11.0`, `v0.10.8`, `0.11.0-rc.1`, `1.2.3+build`, `0.11`, `0.11.0.1`, `0.x.0`, `０.11.0` (full-width digit), empty.
2. Floor comparison: `0.10.8` below, `0.11.0` equal, `0.12.0` above, `1.0.0` above.
3. The warning line text for a below-floor version (factor line building into a pure function so the test needs no binary).

## Acceptance criteria

1. With cbm 0.10.8 installed, `llmenv doctor` shows the warning pair.
2. With cbm 0.11.0, the line is unchanged from today.
3. Changelog `Added`: doctor warns when codebase-memory-mcp is older than 0.11.0.

## Out of scope

- Running `codebase-memory-mcp update` for the user. It only prints a command.
- A floor for `icm`.
