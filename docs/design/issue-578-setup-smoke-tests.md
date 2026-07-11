# Issue #578 — Expand smoke-test coverage for `llmenv setup` CLI flags

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/578
- **Milestone:** Small Projects
- **Type:** Test-only — no production code changes expected
- **Difficulty:** Easy. Pattern to copy already exists in `tests/smoke_suite.rs`.

## Problem

`llmenv setup` has four flags (`--no-launch`, `--rescan`, `--path`, `--repo`)
covered only by inline unit tests in `src/cli/setup.rs`. There are no
integration smoke tests that run the compiled binary end-to-end, so a
regression in arg parsing, flag wiring, or process-level behavior (exit
codes, filesystem side effects) would not be caught.

## What to build

A new integration test file `tests/setup_smoke.rs` that invokes the `llmenv`
binary as a subprocess. **Do not** extend `tests/smoke_suite.rs`; a separate
file keeps `cargo test setup_smoke` targetable.

### Prerequisites already in place — reuse, don't re-add

- `assert_cmd` is already a dev-dependency (see `Cargo.toml`) and is used in
  `tests/smoke_suite.rs`.
- `tests/smoke_suite.rs` has the canonical pattern: a tempdir per test that
  doubles as the config dir, passed via `.env("LLMENV_CONFIG_DIR", config_dir)`
  (see around `tests/smoke_suite.rs:112`). Copy that helper style — read the
  top ~120 lines of `smoke_suite.rs` before writing anything.

### Test cases (one `#[test]` fn each)

| # | Test | Setup | Command | Assert |
|---|------|-------|---------|--------|
| 1 | `setup_no_launch_creates_config` | empty tempdir | `llmenv setup --no-launch` with `LLMENV_CONFIG_DIR=<tmp>` | exit 0; expected config files exist in tempdir |
| 2 | `setup_custom_path` | empty tempdir | `llmenv setup --path <tmp> --no-launch` (no env var) | exit 0; files created under `<tmp>` |
| 3 | `setup_repo_flag_non_interactive` | empty tempdir | `llmenv setup --repo <url> --no-launch` | exit 0; repo URL recorded in the created config |
| 4 | `setup_rescan_on_existing` | run test-1 setup first, record file mtimes/contents | `llmenv setup --rescan --no-launch` | exit 0; pre-existing files not overwritten (contents unchanged) |
| 5 | `setup_rescan_on_empty_dir` | empty tempdir | `llmenv setup --rescan --no-launch` | non-zero exit; stderr mentions running setup first |
| 6 | `setup_missing_config_dir` | point `LLMENV_CONFIG_DIR` at a nonexistent path, no `--path` | `llmenv setup --no-launch` | graceful error or success-with-creation — assert **no panic** (stderr must not contain `panicked`) |
| 7 | `setup_non_interactive_no_flags` | empty tempdir, stdin closed (`.write_stdin("")` or null stdin) | `llmenv setup` | does not hang (assert_cmd runs to completion); exits with clear message rather than panic |

For test 3, use a dummy URL like `https://example.com/user/llmenv-config.git`
— the test must not hit the network. If `run_setup` tries to clone, check
what `--no-launch` + `--rescan` unit tests in `src/cli/setup.rs` do to avoid
network and mirror that; if cloning is unavoidable for `--repo`, assert only
on the recorded URL/error path and note it in a comment.

### How to find "expected files"

Read the unit tests at the bottom of `src/cli/setup.rs` (23 tests, tempdir
isolated) — they name the exact files `run_setup()` creates. Assert on the
same set. Do not guess file names.

## Step-by-step

1. Read `tests/smoke_suite.rs` (helper pattern) and the `#[cfg(test)]` module
   in `src/cli/setup.rs` (expected files, error strings).
2. Create `tests/setup_smoke.rs` with a small local helper
   (tempdir + `assert_cmd::Command::cargo_bin("llmenv")` + env) and the seven
   tests above.
3. Run `cargo test --test setup_smoke`.
4. Verify one test fails when it should: temporarily break an assertion
   (e.g. assert a bogus file exists), confirm failure, restore.
5. Run `cargo clippy --all-targets --all-features -- -D warnings` and
   `cargo fmt`.

## Acceptance criteria

- [ ] `tests/setup_smoke.rs` exists with all 7 scenarios; all pass.
- [ ] No test touches the real user config (every test isolates via tempdir
      + `LLMENV_CONFIG_DIR` or `--path`).
- [ ] No test requires network access.
- [ ] No new dependencies added.
- [ ] Clippy/fmt clean.
- [ ] No CHANGELOG entry (test-only, not user-facing).

## Out of scope

- Testing the interactive prompt flow itself (readline interaction).
- Testing engine handoff/launch (`--no-launch` is used everywhere).
