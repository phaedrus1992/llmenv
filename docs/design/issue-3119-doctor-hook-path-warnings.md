# Issue #3119 — doctor warns about a build-folder, temp-folder, or deleted hook binary

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/3119
- **Milestone:** `v3.14.0`
- **Base branch:** `release/3.x`
- **Code:** `src/cli/doctor/hook_command.rs`
- **Type:** new doctor warnings, tests, docs

## Problem

Since #3081, every engine's hook table names the running llmenv binary by its absolute path.
The path comes from `std::env::current_exe()` through `program_name` (`src/adapter/hook_program.rs:20`).
If you run `llmenv regenerate` from a build folder, such as `target/debug/llmenv`, every later hook runs that file.
Whoever can write to that folder then controls the hooks.
A temp folder has the same risk.
On Linux, a binary that was replaced or removed reads back with the suffix ` (deleted)`, so the hook path names a file that is gone.

## Decision

Owner decision on #3119 (2026-11-03), option B:

> Keep `current_exe` in the hook table. `llmenv doctor` warns when the hook path is under a `target/` folder, under a temp folder, or ends in ` (deleted)`. Acceptance: one doctor test for each of the three cases. The hook table does not change.

## What does not change

- `hook_command`, `program_name`, and `fallback_message` in `src/adapter/hook_program.rs`.
- The bare-`llmenv` fallback when the path is not UTF-8 or unknown. Issue #3060 owns that decision.
- Every hook table that an adapter writes.

## Design

All new code goes in `src/cli/doctor/hook_command.rs`.

### Risk kinds

```rust
/// A reason the binary that hook commands name is not safe to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookPathRisk {
    /// The path has a `target` folder in it, which is where cargo builds.
    BuildFolder,
    /// The path is under a temp folder.
    TempFolder,
    /// The path ends in ` (deleted)`: Linux reports this for a binary that was removed or replaced.
    Deleted,
}
```

### The pure check

```rust
const DELETED_SUFFIX: &str = " (deleted)";

/// The risks of `exe`, the path that hook commands name. `temp_roots` are the temp folders to
/// test against. The result is in the order Deleted, BuildFolder, TempFolder, and has no repeats.
fn hook_path_risks(exe: &Path, temp_roots: &[PathBuf]) -> Vec<HookPathRisk>
```

Rules, in this order:

1. **Deleted.** Take `exe.to_string_lossy()`. If it ends with `DELETED_SUFFIX`, push `Deleted`, and use the text without the suffix as the path for rules 2 and 3. Otherwise use `exe` as it is.
2. **BuildFolder.** If any component of the path's parent folder equals the OS string `target`, push `BuildFolder`. Compare whole components with `Path::components`, never a substring. So `/home/u/targets/llmenv` and `/home/u/my-target/llmenv` do not match.
3. **TempFolder.** Build the candidate list: the path, plus its `std::fs::canonicalize` result when that succeeds. Build the root list: each entry of `temp_roots`, plus its `canonicalize` result when that succeeds. If any candidate `starts_with` any root (use `Path::starts_with`, which compares whole components), push `TempFolder`.

The canonical forms matter on macOS, where `/tmp` is a link to `/private/tmp`.
A path that does not exist, such as a deleted binary, has no canonical form, so only its raw form is compared.

### The temp roots

```rust
/// The temp folders that a hook binary must not live under.
fn temp_roots() -> Vec<PathBuf>
```

Return these, in this order, without repeats:

1. `std::env::temp_dir()`.
2. `/tmp`, `/private/tmp`, and `/var/tmp`.
3. `/private/var/folders` (the root of the per-user temp folders on macOS).
4. `<state_dir>/tmp`, when `crate::paths::state_dir()` succeeds. llmenv sets `TMPDIR` to this folder inside a session (`src/adapter/temp_dir.rs:13`), so `temp_dir()` alone can miss the system folders.

### The messages

```rust
fn risk_message(risk: HookPathRisk, exe: &Path) -> String
```

Use these texts, with `{path}` as `exe.display()`:

- `BuildFolder`: `Hook commands name {path}, which is in a cargo target folder. A later build replaces it, and anyone who can write that folder controls every hook. Fix: install llmenv, then run llmenv regenerate with the installed binary.`
- `TempFolder`: `Hook commands name {path}, which is in a temp folder. A temp sweep can remove it, and anyone who can write that folder controls every hook. Fix: install llmenv, then run llmenv regenerate with the installed binary.`
- `Deleted`: `Hook commands name {path}, but that binary was removed or replaced after it started. Fix: run llmenv regenerate with the installed binary.`

### Wiring in `run_doctor_hook_command`

`run_doctor_hook_command(use_color: bool)` is at `src/cli/doctor/hook_command.rs:8`.
Keep its existing `program_name` check, and add the risk check:

1. Call `std::env::current_exe()` once, and keep the `io::Result<PathBuf>`.
2. Call `program_name` with that result. `program_name` takes ownership, so clone the `PathBuf` first, or call `current_exe()` a second time.
3. If `program_name` returns `Err(reason)`, print the existing warning with `fallback_message(reason)`, as today. Do not run the risk check.
4. If it returns `Ok(_)`, compute `hook_path_risks(&exe, &temp_roots())`.
   - No risks: print the existing pass line, `Hook commands name llmenv by the path of the running binary`.
   - One or more risks: print one warning line for each risk with `risk_message`, using the `doctor_warning` prefix. Do not print the pass line.

Print with `eprintln!`, as the existing check does.

## Tests

Add these tests to the `tests` module of `src/cli/doctor/hook_command.rs`.
Each test passes paths that do not exist, so `canonicalize` fails and only the raw paths are compared.
Use `PathBuf::from` with literal paths.

1. `a_binary_in_a_cargo_target_folder_is_a_build_folder_risk`
   - `exe` = `/home/u/llmenv/target/debug/llmenv`, roots = `[/tmp]`.
   - Assert the result equals `vec![HookPathRisk::BuildFolder]`.
2. `a_binary_in_a_temp_folder_is_a_temp_folder_risk`
   - `exe` = `/tmp/build-1234/llmenv`, roots = `[/tmp]`.
   - Assert the result equals `vec![HookPathRisk::TempFolder]`.
3. `a_deleted_binary_is_a_deleted_risk`
   - `exe` = `/usr/local/bin/llmenv (deleted)`, roots = `[/tmp]`.
   - Assert the result equals `vec![HookPathRisk::Deleted]`.
4. `an_installed_binary_has_no_risk`
   - `exe` = `/home/u/.cargo/bin/llmenv`, roots = `[/tmp]`.
   - Assert the result is empty.
5. `a_folder_named_like_target_is_not_a_build_folder`
   - `exe` = `/home/u/targets/llmenv`, roots = `[/tmp]`.
   - Assert the result is empty.
6. `a_deleted_build_binary_has_both_risks`
   - `exe` = `/home/u/p/target/release/llmenv (deleted)`, roots = `[/tmp]`.
   - Assert the result equals `vec![HookPathRisk::Deleted, HookPathRisk::BuildFolder]`.
7. `each_risk_message_names_the_path_and_the_fix`
   - For each of the three risks, assert `risk_message(risk, Path::new("/x/llmenv"))` contains `/x/llmenv` and `Fix:`.

Tests 1 to 3 are the acceptance tests from the decision.

## Docs

Edit `website/docs/commands.md`, section `## doctor` (starts around line 1181).
Add one bullet to the list of checks:

```markdown
- Hook commands: warns when hook commands name a binary in a cargo `target` folder or a temp folder, or a binary that was removed after it started (added in v3.14.0).
```

Edit `website/docs/troubleshooting.md`, section `## A program is reported as not found` (around lines 223-226).
After the sentence about the bare-name fallback, add:

```markdown
`llmenv doctor` also warns when the hook path is in a cargo `target` folder or a temp folder, or names a binary that was removed. Install llmenv, then run `llmenv regenerate` with the installed binary (added in v3.14.0).
```

## Changelog

Add one entry under `## [Unreleased]` → `### Added` in `CHANGELOG-3.md`:

```markdown
- Warn in `llmenv doctor` when hook commands name a binary in a cargo `target` folder or a temp folder, or a binary that was removed after it started. [feat:doctor] [#3119]
```

Take the tag from the commit that adds the check.
Then run `scripts/sync-changelog-doc.sh` and commit `website/docs/changelog.md` with it.

## Spec-writer calls

- `CARGO_TARGET_DIR` is not read. The decision names a `target/` folder only, and a custom target folder is rare.
- `<state_dir>/tmp` is in the temp roots, because llmenv points `TMPDIR` there inside a session.
- A deleted path is also tested for the other two risks, using the path without the suffix.
