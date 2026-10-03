# Issue #2343 — nextest flags a plugin-json test as leaky under full load

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2343
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** test infrastructure

This is a spec, not a plan.

## Problem

`cargo nextest run --workspace --all-features` reports `adapter::claude_code::tests::generate_installed_plugins_json_succeeds_on_absent_file` as `LEAK` in two full runs in a row, at about test 43 of 3,076.
Run alone three times, it passes with no `LEAK`.
The function under test and its helper start no subprocess.
The likely cause is nextest's timing-based leak detection (default leak timeout 100 ms) under heavy parallel load early in the run.
That is not confirmed.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `.config/nextest.toml` defines a `cli-subprocess` test group (`max-threads = 4`) for binaries that spawn llmenv with a timeout; it sets no `leak-timeout` and no `leak-timeout-result` | `.config/nextest.toml` |
| CI runs `cargo nextest run --workspace --test-threads "$CARGO_BUILD_JOBS"` with the default profile | `.github/workflows/ci.yml` |
| nextest's default `leak-timeout` is 100 ms; `LEAK` is reported but counts as a pass unless `leak-timeout-result = "fail"` is set (nextest 0.9.84 or later) | nextest docs |
| The test exists in the Claude Code adapter's test module; `generate_installed_plugins_json` and `external_plugin` do file I/O only | `src/adapter/claude_code/mod.rs` |

## Decisions

1. **Confirm the cause before changing config.**
   Run the full suite three times with `--leak-timeout 500ms`.
   If `LEAK` disappears, the cause is the timing heuristic.
   If it stays, find the handle: run the single test under `lsof -p` (or `fuser`) at exit, and check whether the test opens a file it does not drop before returning (an unflushed `File` in a `tempfile::TempDir` still counts as a child-held handle only if a child exists; so also check for an inherited stdout or stderr pipe from a `Command` elsewhere in the same binary).
2. **If timing, set a project-wide `leak-timeout` with the reason**, in `.config/nextest.toml` under `[profile.default]`: `leak-timeout = "500ms"`, with a comment that names this issue, the machine load, and that the value is five times the default.
3. **Make a real leak fail in CI.**
   Add `leak-timeout-result = "fail"` under a `[profile.ci]` that inherits default, and switch the CI command to `--profile ci`.
   Do this only after three clean full runs locally with the new timeout, so CI does not turn red on day one.
   Check the pinned nextest version in CI supports the key; bump if needed and note it.
4. **If a real handle is found, fix the test (or the code) and leave the timeout alone.**
   The doc then records which handle it was.

## Design

### `.config/nextest.toml`

```toml
[profile.default]
# nextest's 100 ms leak window flags a pure file-I/O test under full parallel
# load (#2343); 500 ms keeps the detector for real leaks without the false flag.
leak-timeout = "500ms"

[profile.ci]
# A real leaked handle is a bug; CI must not pass it silently.
leak-timeout-result = "fail"
```

### `.github/workflows/ci.yml`

- `cargo nextest run --workspace --profile ci --test-threads "$CARGO_BUILD_JOBS"`.

### Docs

- `website/docs/maintainers.md` (or wherever the test workflow is described): one sentence on the leak timeout and the CI profile. No changelog entry (test-only).

## Tests

1. Three full `cargo nextest run --workspace --all-features` runs with the new config: zero `LEAK` lines.
2. A deliberate leak (a temporary test that spawns `sleep 5` without waiting) fails under `--profile ci` and passes under the default profile; remove the test before merging and record the result in the PR.
3. CI green on the PR.

## Acceptance criteria

1. The named test no longer reports `LEAK` under full load (three runs).
2. CI uses a profile where a leak fails the run.
3. The config comment names the reason and the issue.

## Out of scope

- Changing the `cli-subprocess` group or its thread cap.
- Investigating other tests unless they show `LEAK` in the three runs (file an issue per test if they do).
