# Crate Coupling Cycle Resolution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans
> to implement this plan task-by-task (global policy overrides
> subagent-driven-development — see dev-sprint's handoff notes for issue
> #1462). Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Do not execute this plan as part of the dev-sprint run that produced
> it.** Issue #1462 is design-only — its acceptance criteria are the design
> doc and this plan, not the relocations themselves. A future, separate
> dev-sprint run picks this plan up via `nbl-dev:ship-issue`.

**Goal:** Break all 7 circular internal-module dependencies confirmed in the
design doc by relocating 9 groups of misplaced symbols to their natural
lower-layer home, with zero behavior change.

**Architecture:** Pure code motion — no new abstractions, no trait
inversions. Each task cuts one or more symbols (a constant, a struct, or a
small set of stateless functions) from their current module and pastes them
into an existing lower-layer crate (`llmenv-util`, `llmenv-mcp`,
`llmenv-scope`) or the `src/` module that is their natural conceptual owner
(`materialize`, `memory`, `session_log`), or into one new module
(`crate::bundle_select`). Every call site across the tree is updated to the
new path. No task changes what any function does — only where it lives.

**Tech Stack:** Rust (edition 2024), existing workspace crates only. Two
crates gain one new dependency each (both already pinned at the workspace
level): `llmenv-mcp` gains `serde_json` and `url`; `llmenv-util` gains
`sha2`.

**Spec:** `docs/superpowers/specs/2026-08-27-crate-coupling-cycle-resolution-design.md`

## Global Constraints

- `unsafe_code = "forbid"` (workspace lint) — none of these moves need
  `unsafe`; if a task seems to need it, stop and re-read the design.
- `unwrap_used = "deny"`, `expect_used = "deny"`, `panic = "deny"` (workspace
  lint) — moved code must compile clean under these; none of the code being
  moved uses `unwrap`/`expect`/`panic!` today (verified per-task below), so
  no rewriting should be needed, only relocation.
- `cargo fmt` after every file edit, before staging.
- Every task ends green on `cargo build --workspace`,
  `cargo test --workspace`, and
  `cargo clippy --all-targets --all-features -- -D warnings`.
- No behavior change in any task — every existing test for a moved symbol
  moves with it and must still pass verbatim (only `use`/`crate::` paths in
  the test module change, never assertions).
- Commit after each task (9 commits total, matching the design's
  sequencing).

---

### Task 1: Move `update_len_prefixed` into `llmenv-util`

**Files:**
- Modify: `crates/llmenv-util/Cargo.toml` (add `sha2` dependency)
- Modify: `crates/llmenv-util/src/lib.rs` (add function)
- Modify: `src/materialize/cache.rs:197-200` (delete function, update 15
  internal call sites to the new path, add `use` import)
- Modify: `src/merge/mod.rs:202` (update the one call site)

**Interfaces:**
- Produces: `llmenv_util::update_len_prefixed(h: &mut sha2::Sha256, data: &[u8])`

- [ ] **Step 1: Add `sha2` to `llmenv-util`'s dependencies**

In `crates/llmenv-util/Cargo.toml`, under `[dependencies]`, add:

```toml
sha2 = { workspace = true }
```

- [ ] **Step 2: Move the function into `llmenv-util`**

Append to `crates/llmenv-util/src/lib.rs`:

```rust
use sha2::Sha256;

/// Shared length-prefix hashing convention: length-prefix every field before
/// its bytes so concatenation can't ambiguate boundaries. Used by
/// `materialize::cache::hash_manifest` and `merge::merge_signature` (#920).
pub fn update_len_prefixed(h: &mut Sha256, data: &[u8]) {
    h.update((data.len() as u64).to_le_bytes());
    h.update(data);
}
```

- [ ] **Step 3: Delete the original and redirect its 15 internal callers**

In `src/materialize/cache.rs`, delete the `update_len_prefixed` function
(lines 197-200, the `pub(crate) fn update_len_prefixed` block only — leave
`hash_native_capability_map` and everything else untouched). Add near the
top of the file:

```rust
use llmenv_util::update_len_prefixed;
```

Every existing call site in this file (`update_len_prefixed(&mut h, ...)` /
`update_len_prefixed(h, ...)`, currently 15 occurrences) already calls it
unqualified — they resolve to the new `use` import unchanged. No call-site
edits needed beyond the import.

- [ ] **Step 4: Redirect `merge`'s one call site**

In `src/merge/mod.rs:202`, change:

```rust
use crate::materialize::cache::update_len_prefixed;
```

to:

```rust
use llmenv_util::update_len_prefixed;
```

- [ ] **Step 5: Build and test**

Run: `cargo build --workspace && cargo test -p llmenv-util -p llmenv --lib materialize::cache:: merge::`
Expected: PASS, no warnings.

- [ ] **Step 6: Verify the cycle is broken**

Run: `rg -l "crate::materialize" src/merge/ --type rust`
Expected: no output (previously matched `src/merge/mod.rs`).

- [ ] **Step 7: Format and commit**

```bash
cargo fmt
git add crates/llmenv-util/Cargo.toml crates/llmenv-util/src/lib.rs \
  src/materialize/cache.rs src/merge/mod.rs Cargo.lock
git commit -m "refactor: move update_len_prefixed into llmenv-util"
```

---

### Task 2: Move `should_use_color`, `paint`, and `doctor_warning` into `llmenv-util`

**Files:**
- Modify: `crates/llmenv-util/Cargo.toml` (add `anstyle` dependency)
- Modify: `crates/llmenv-util/src/lib.rs` (add functions + tests)
- Modify: `src/cli/style.rs:1-14,35-73,90-98` (delete `paint`,
  `should_use_color`, `should_use_color_with_env`, `doctor_warning`, and
  their tests, re-export all four for existing `cli` callers)
- Modify: `src/hook_run/mod.rs:490` (update the one call site)

**Interfaces:**
- Produces: `llmenv_util::should_use_color(mode: Option<ColorMode>, is_tty: bool) -> bool`
  where `ColorMode` stays defined in `src/cli/style.rs` (it is not part of
  this cycle — `hook_run` calls `should_use_color(None, false)` and never
  names `ColorMode` itself, so the type does not need to move).
- Produces: `llmenv_util::paint(text: &str, color: anstyle::AnsiColor, use_color: bool) -> String`
  and `llmenv_util::doctor_warning(use_color: bool) -> String`. These move
  too, ahead of when they're strictly needed, because Task 6 discovered that
  `materialize`'s new stale-check function needs `doctor_warning` without
  pulling `cli` back in as a dependency (which would undo Task 6's fix) —
  `doctor_warning` depends on the private `paint` helper, so both move
  together. The rest of `cli::style`'s doctor/marker functions
  (`active_marker`, `doctor_pass`, `doctor_fail`, `doctor_info`, etc.) stay
  in `cli::style` and switch to calling `llmenv_util::paint` instead of the
  local (now-deleted) private copy — they are not part of any cycle and do
  not need to move themselves.

- [ ] **Step 1: Add `anstyle` to `llmenv-util`'s dependencies**

In `crates/llmenv-util/Cargo.toml`, under `[dependencies]`, add:

```toml
anstyle = { workspace = true }
```

- [ ] **Step 2: Move `paint`, `should_use_color`, and `doctor_warning`, plus their tests, into `llmenv-util`**

Append to `crates/llmenv-util/src/lib.rs`:

```rust
use anstyle::{AnsiColor, Color, Style};

/// Wrap text in an ANSI style when `use_color` is set, else return it plain.
/// `pub`, not `pub(crate)`: `cli::style`'s remaining doctor/marker functions
/// call this from a different crate after this move.
pub fn paint(text: &str, color: AnsiColor, use_color: bool) -> String {
    if use_color {
        let style = Style::new().fg_color(Some(Color::Ansi(color)));
        format!("{style}{text}{style:#}")
    } else {
        text.to_string()
    }
}

/// Format a doctor "warning" symbol (⚠) with optional yellow color.
pub fn doctor_warning(use_color: bool) -> String {
    paint("⚠", AnsiColor::Yellow, use_color)
}

/// Color mode: auto-detect, always on, or always off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Auto-detect based on stdout TTY and NO_COLOR env var
    Auto,
    /// Force colors on
    Always,
    /// Force colors off
    Never,
}

/// Determine whether to emit colors based on flags, env vars, and TTY state.
pub fn should_use_color(mode: Option<ColorMode>, is_tty: bool) -> bool {
    should_use_color_with_env(mode, is_tty, &|name| std::env::var(name).ok())
}

fn should_use_color_with_env<F>(mode: Option<ColorMode>, is_tty: bool, get_env: &F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    let effective_mode = mode.unwrap_or(ColorMode::Auto);
    match effective_mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            if get_env("NO_COLOR").is_some() {
                return false;
            }
            if get_env("CLICOLOR_FORCE")
                .filter(|v| !v.is_empty())
                .is_some()
            {
                return true;
            }
            is_tty
        }
    }
}

#[cfg(test)]
mod color_tests {
    use super::*;

    #[test]
    fn test_should_use_color_always_mode() {
        assert!(should_use_color(Some(ColorMode::Always), false));
        assert!(should_use_color(Some(ColorMode::Always), true));
    }

    #[test]
    fn test_should_use_color_never_mode() {
        assert!(!should_use_color(Some(ColorMode::Never), false));
        assert!(!should_use_color(Some(ColorMode::Never), true));
    }

    #[test]
    fn test_should_use_color_auto_respects_tty() {
        assert!(!should_use_color(Some(ColorMode::Auto), false));
    }

    #[test]
    fn test_should_use_color_auto_with_tty_isolated() {
        let no_env = |_name: &str| -> Option<String> { None };
        assert!(!should_use_color_with_env(Some(ColorMode::Auto), false, &no_env));
        assert!(should_use_color_with_env(Some(ColorMode::Auto), true, &no_env));
    }

    #[test]
    fn test_should_use_color_no_color_overrides() {
        let no_color_env = |name: &str| -> Option<String> {
            match name {
                "NO_COLOR" => Some("1".to_string()),
                _ => None,
            }
        };
        assert!(!should_use_color_with_env(Some(ColorMode::Auto), true, &no_color_env));
    }

    #[test]
    fn test_should_use_color_no_color_empty_string() {
        let no_color_empty_env = |name: &str| -> Option<String> {
            match name {
                "NO_COLOR" => Some(String::new()),
                _ => None,
            }
        };
        assert!(!should_use_color_with_env(Some(ColorMode::Auto), true, &no_color_empty_env));
    }

    #[test]
    fn test_should_use_color_clicolor_force_overrides() {
        let force_env = |name: &str| -> Option<String> {
            match name {
                "CLICOLOR_FORCE" => Some("1".to_string()),
                _ => None,
            }
        };
        assert!(should_use_color_with_env(Some(ColorMode::Auto), false, &force_env));
    }

    #[test]
    fn test_should_use_color_clicolor_force_empty_string_does_not_force() {
        let empty_force_env = |name: &str| -> Option<String> {
            match name {
                "CLICOLOR_FORCE" => Some(String::new()),
                _ => None,
            }
        };
        assert!(!should_use_color_with_env(Some(ColorMode::Auto), false, &empty_force_env));
    }

    #[test]
    fn test_should_use_color_no_color_takes_precedence_over_clicolor_force() {
        let both_env = |name: &str| -> Option<String> {
            match name {
                "NO_COLOR" => Some("1".to_string()),
                "CLICOLOR_FORCE" => Some("1".to_string()),
                _ => None,
            }
        };
        assert!(!should_use_color_with_env(Some(ColorMode::Auto), true, &both_env));
    }
}
```

- [ ] **Step 3: Delete the originals from `cli::style` and re-export**

In `src/cli/style.rs`, delete the private `paint` function (lines 6-14),
`should_use_color` and `should_use_color_with_env` (lines 27-73), and
`doctor_warning` (the `paint("⚠", AnsiColor::Yellow, use_color)` one-liner,
around line 96). Delete their tests from the `#[cfg(test)] mod tests` block:
the 9 `test_should_use_color_*` tests. `doctor_warning` has no dedicated
test of its own — it's exercised only via the combined
`test_marker_functions_*` tests, which also cover `active_marker`,
`inactive_annotation`, `orphan_annotation`, `doctor_pass`, `doctor_fail`;
those tests stay in `cli/style.rs` unchanged since those functions aren't
moving, but each `assert_eq!`/`assert!` line touching `doctor_warning(...)`
specifically needs a matching case added to `llmenv-util`'s own test module
instead — check `test_marker_functions_plain_when_no_color`,
`test_marker_functions_colored_contain_escape_codes`, and
`test_marker_functions_preserve_glyph_under_color` for their
`doctor_warning` assertions and port just those into a
`doctor_warning`-specific test in `llmenv-util`, e.g.:

```rust
#[test]
fn doctor_warning_plain_when_no_color() {
    assert_eq!(doctor_warning(false), "⚠");
}

#[test]
fn doctor_warning_colored_contains_escape_code() {
    assert!(doctor_warning(true).contains('\u{1b}'));
}
```

leaving the remaining (non-`doctor_warning`) assertions in
`cli/style.rs`'s existing `test_marker_functions_*` tests untouched.

Every remaining function in `cli/style.rs` that called the now-deleted
private `paint` (`active_marker`, `inactive_annotation`,
`orphan_annotation`, `doctor_pass`, `doctor_fail`) needs its call site
changed from `paint(...)` to `llmenv_util::paint(...)` — but `paint` was
`pub(crate)`-invisible outside `llmenv-util` in Step 2's definition above;
change that definition to `pub(crate) fn paint` is wrong (it must be
reachable from `cli`, a different crate) — make it `pub fn paint` in
`llmenv-util` instead, then update those 5 call sites in `cli/style.rs` to
`llmenv_util::paint(...)`.

Add near the top of `cli/style.rs`, after the `use anstyle::...` line:

```rust
pub use llmenv_util::{doctor_warning, should_use_color};
```

`ColorMode` stays defined in this file exactly as-is — only `paint`,
`should_use_color`, `should_use_color_with_env`, and `doctor_warning` move.

- [ ] **Step 4: Redirect `hook_run`'s one call site**

`src/hook_run/mod.rs:490` already calls `crate::cli::should_use_color(None, false)`
— unchanged, since `cli::style` now re-exports it under the same path. No
edit needed here.

- [ ] **Step 5: Build and test**

Run: `cargo build --workspace && cargo test -p llmenv-util -p llmenv --lib cli::style::`
Expected: PASS.

- [ ] **Step 6: Format and commit**

```bash
cargo fmt
git add crates/llmenv-util/Cargo.toml crates/llmenv-util/src/lib.rs \
  src/cli/style.rs Cargo.lock
git commit -m "refactor: move should_use_color, paint, doctor_warning into llmenv-util"
```

---

### Task 3: Move `mcp_client` module + `INDEX_REPOSITORY_TOOL` into `llmenv-mcp`

**Files:**
- Modify: `crates/llmenv-mcp/Cargo.toml` (add `serde_json`, `url`
  dependencies)
- Create: `crates/llmenv-mcp/src/mcp_client.rs` (moved from
  `src/hook_run/mcp_client.rs`)
- Modify: `crates/llmenv-mcp/src/lib.rs` (declare `pub mod mcp_client;`)
- Delete: `src/hook_run/mcp_client.rs`
- Modify: `src/hook_run/cbm_index_guard.rs:29` (delete constant, it moves to
  `llmenv-mcp`)
- Modify: `crates/llmenv-mcp/src/resolve.rs` (add `INDEX_REPOSITORY_TOOL`)
- Modify 9 call sites: `src/consolidation/mod.rs:29`,
  `src/memory/prune.rs:25`, `src/memory/mod.rs:14`,
  `src/throttle/backend.rs:206,208`, `src/hook_run/detached_store.rs:9`,
  `src/session_log/detached.rs:17`, `src/session_log/dispatch.rs:6`,
  `src/hook_run/action.rs:7`, `src/hook_run/detached_consolidation.rs:9`
- Modify 2 call sites for the constant:
  `src/adapter/claude_code.rs:1485,3779`,
  `src/hook_run/cbm_index_guard.rs:36,73,89,105`

**Interfaces:**
- Produces: `llmenv_mcp::mcp_client::McpHttpClient`,
  `llmenv_mcp::mcp_client::validate_url_production`,
  `llmenv_mcp::mcp_client::SsrfPolicy` (same names/signatures as today,
  new crate path)
- Produces: `llmenv_mcp::resolve::INDEX_REPOSITORY_TOOL: &str`

`src/hook_run/mcp_client.rs` has zero internal `crate::` dependencies today
(only `std::net`, `std::sync::mpsc`, `std::thread`, `std::time::Duration`,
`anyhow`, `serde_json`, `url`) — this is a verbatim file move, not a rewrite.

- [ ] **Step 1: Add dependencies to `llmenv-mcp`**

In `crates/llmenv-mcp/Cargo.toml`, under `[dependencies]`, add:

```toml
serde_json = { workspace = true }
url = { workspace = true }
```

- [ ] **Step 2: Move the file verbatim**

```bash
git mv src/hook_run/mcp_client.rs crates/llmenv-mcp/src/mcp_client.rs
```

No content changes needed inside the file — it has no `crate::` imports to
fix.

- [ ] **Step 3: Declare the module in `llmenv-mcp`**

In `crates/llmenv-mcp/src/lib.rs`, add (alongside the existing `pub mod proxy;`
and `pub mod resolve;`):

```rust
pub mod mcp_client;
```

- [ ] **Step 4: Move `INDEX_REPOSITORY_TOOL` into `llmenv-mcp::resolve`**

In `crates/llmenv-mcp/src/resolve.rs`, add near `CODEBASE_MEMORY_MCP_NAME`:

```rust
/// Tool name for the codebase-memory MCP's repository-indexing tool, as it
/// appears in a hook's `tool_name` field.
pub const INDEX_REPOSITORY_TOOL: &str = "mcp__codebase-memory-mcp__index_repository";
```

In `src/hook_run/cbm_index_guard.rs`, delete line 29
(`pub(crate) const INDEX_REPOSITORY_TOOL: &str = ...`) and add near the top:

```rust
use llmenv_mcp::resolve::INDEX_REPOSITORY_TOOL;
```

The 4 existing unqualified uses of `INDEX_REPOSITORY_TOOL` in this file
(lines 36, 73, 89, 105) resolve to the import unchanged.

- [ ] **Step 5: Redirect the 2 `adapter` call sites**

In `src/adapter/claude_code.rs:1485,3779`, change
`crate::hook_run::cbm_index_guard::INDEX_REPOSITORY_TOOL` to
`llmenv_mcp::resolve::INDEX_REPOSITORY_TOOL`.

- [ ] **Step 6: Redirect the 9 `mcp_client` call sites**

In each of `src/consolidation/mod.rs:29`, `src/memory/prune.rs:25`,
`src/memory/mod.rs:14`, `src/hook_run/detached_store.rs:9`,
`src/session_log/detached.rs:17`, `src/session_log/dispatch.rs:6`,
`src/hook_run/action.rs:7`, `src/hook_run/detached_consolidation.rs:9`,
change:

```rust
use crate::hook_run::mcp_client::McpHttpClient;
```

to:

```rust
use llmenv_mcp::mcp_client::McpHttpClient;
```

In `src/throttle/backend.rs:206,208`, change
`crate::hook_run::mcp_client::validate_url_production` /
`crate::hook_run::mcp_client::SsrfPolicy` to
`llmenv_mcp::mcp_client::validate_url_production` /
`llmenv_mcp::mcp_client::SsrfPolicy`.

- [ ] **Step 7: Build and test**

Run: `cargo build --workspace && cargo test -p llmenv-mcp && cargo test -p llmenv --lib consolidation:: memory:: throttle:: hook_run:: session_log::`
Expected: PASS. `mcp_client`'s own test module (lines 531-1143 of the
original file, now inside `crates/llmenv-mcp/src/mcp_client.rs`) moved with
it verbatim and must pass unchanged.

- [ ] **Step 8: Verify the cycles are broken**

Run:
```bash
rg -l "crate::hook_run" src/adapter/ --type rust
rg -l "crate::hook_run" src/consolidation/ --type rust
rg -l "hook_run::mcp_client" src/ --type rust
```
Expected: first two produce no output (previously matched
`src/adapter/claude_code.rs` and `src/consolidation/mod.rs`); third produces
no output anywhere in the tree.

- [ ] **Step 9: Format and commit**

```bash
cargo fmt
git add -A
git commit -m "refactor: move mcp_client and INDEX_REPOSITORY_TOOL into llmenv-mcp"
```

---

### Task 4: Move `bundle_keyword`/`tag_keyword` into `llmenv-scope`

**Files:**
- Modify: `crates/llmenv-scope/src/lib.rs` (add constants + functions + tests)
- Modify: `src/hook_run/action.rs:12-33` (delete both functions and their
  prefix constants, add `use`)
- Modify: `src/session_log/scope_header.rs:6` (update the one call site)

**Interfaces:**
- Produces: `llmenv_scope::TAG_KEYWORD_PREFIX: &str`,
  `llmenv_scope::tag_keyword(tag: &str) -> String`,
  `llmenv_scope::BUNDLE_KEYWORD_PREFIX: &str`,
  `llmenv_scope::bundle_keyword(bundle: &str) -> String`

- [ ] **Step 1: Move both constants and functions into `llmenv-scope`**

Append to `crates/llmenv-scope/src/lib.rs`:

```rust
/// The keyword prefix under which tag-scoped memory is stored and recalled.
/// A memory written for tag `work-vpn` carries keyword `llmenv-tag:work-vpn`;
/// recalling that keyword (project-unfiltered) surfaces it from any project.
pub const TAG_KEYWORD_PREFIX: &str = "llmenv-tag:";

/// The `llmenv-tag:<tag>` keyword for a tag. The tag is assumed
/// pre-validated so it contains no recall-query metacharacters.
#[must_use]
pub fn tag_keyword(tag: &str) -> String {
    format!("{TAG_KEYWORD_PREFIX}{tag}")
}

/// The keyword prefix under which bundle-scoped memory is stored and
/// recalled. A memory written for bundle `base` carries keyword
/// `llmenv-bundle:base`; recalling that keyword (project-unfiltered)
/// surfaces it from any project.
pub const BUNDLE_KEYWORD_PREFIX: &str = "llmenv-bundle:";

/// The `llmenv-bundle:<bundle>` keyword for a bundle. The bundle name is
/// assumed pre-validated so it contains no recall-query metacharacters.
#[must_use]
pub fn bundle_keyword(bundle: &str) -> String {
    format!("{BUNDLE_KEYWORD_PREFIX}{bundle}")
}

#[cfg(test)]
mod keyword_tests {
    use super::*;

    #[test]
    fn tag_keyword_prefixes_tag() {
        assert_eq!(tag_keyword("work-vpn"), "llmenv-tag:work-vpn");
        assert_eq!(tag_keyword("rust"), "llmenv-tag:rust");
    }

    #[test]
    fn bundle_keyword_prefixes_bundle() {
        assert_eq!(bundle_keyword("base"), "llmenv-bundle:base");
    }
}
```

- [ ] **Step 2: Delete the originals from `hook_run::action`**

In `src/hook_run/action.rs`, delete `TAG_KEYWORD_PREFIX`, `tag_keyword`,
`BUNDLE_KEYWORD_PREFIX`, and `bundle_keyword` (lines 12-33). Add near the top
of the file, alongside the existing `use crate::hook_run::mcp_client::...`
import (now `use llmenv_mcp::mcp_client::McpHttpClient;` after Task 3):

```rust
pub use llmenv_scope::{bundle_keyword, tag_keyword};
```

`pub use` (not plain `use`) because other code in `hook_run` reaches these
via `crate::hook_run::action::{bundle_keyword, tag_keyword}` — re-exporting
keeps that path valid without hunting down every internal caller.

Check whether `action.rs`'s own test module (search
`rg -n "tag_keyword|bundle_keyword" src/hook_run/action.rs`) has tests
duplicating the two moved above; if so, delete those specific test functions
from `action.rs` (the equivalent coverage now lives in `llmenv-scope`) —
leave every other test in that file untouched.

- [ ] **Step 3: Redirect `session_log`'s one call site**

`src/session_log/scope_header.rs:6` currently has:

```rust
use crate::hook_run::action::{bundle_keyword, tag_keyword};
```

Change to:

```rust
use llmenv_scope::{bundle_keyword, tag_keyword};
```

- [ ] **Step 4: Build and test**

Run: `cargo build --workspace && cargo test -p llmenv-scope && cargo test -p llmenv --lib hook_run::action:: session_log::scope_header::`
Expected: PASS.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt
git add crates/llmenv-scope/src/lib.rs src/hook_run/action.rs src/session_log/scope_header.rs
git commit -m "refactor: move bundle_keyword/tag_keyword into llmenv-scope"
```

---

### Task 5: Extract `crate::bundle_select` out of `cli`

**Files:**
- Create: `src/bundle_select.rs`
- Modify: `src/main.rs` or `src/lib.rs` (wherever top-level `mod` decls
  live — add `mod bundle_select;`; run
  `rg -n "^mod cli;" src/*.rs` to find the right file before editing)
- Modify: `src/cli/mod.rs:3085-3155,4389-4456` (delete the 5 functions and
  their tests, add `use`)
- Modify: `src/hook_run/mod.rs` (update call sites once known — see Step 4)

**Interfaces:**
- Produces:
  `bundle_select::build_bundle_refs(config_dir: &Path, active: &ActiveScopes, firing: &[&Bundle]) -> Vec<crate::merge::BundleRef>`
  `bundle_select::marker_enabled_bundle_names(active: &ActiveScopes) -> HashSet<String>`
  `bundle_select::marker_disabled_bundle_names(active: &ActiveScopes) -> HashSet<String>`
  `bundle_select::tag_or_marker_selected(bundle: &Bundle, active: &ActiveScopes, manually_enabled: &HashSet<String>) -> bool`
  `bundle_select::firing_bundles<'a>(bundles: &'a [Bundle], active: &ActiveScopes, tag_filter: Option<&str>) -> Vec<&'a Bundle>`

`BundleRef` is **not** a new type — it already exists at
`src/merge/mod.rs:16` (`pub struct BundleRef { pub name: String, pub path: PathBuf, pub precedence: u8 }`,
identical shape). `bundle_select::build_bundle_refs` constructs and returns
that existing type; it does not define its own.

- [ ] **Step 1: Create `src/bundle_select.rs`**

```rust
//! Bundle/marker selection logic shared by `cli` (rendering, doctor) and
//! `hook_run` (live memory-endpoint resolution) — factored out so the two
//! callers' notion of "which bundles are active" can't drift apart (#1141,
//! #1125).

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use crate::config::Bundle;
use crate::merge::BundleRef;
use crate::scope::ActiveScopes;

pub fn build_bundle_refs(
    config_dir: &Path,
    active: &ActiveScopes,
    firing: &[&Bundle],
) -> Vec<BundleRef> {
    const PRECEDENCE: &[&str] = &["network", "host", "user", "content", "project"];

    let bundles_dir = config_dir.join("bundles");
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut refs: Vec<BundleRef> = Vec::new();

    let push_ref =
        |name: &str, precedence: u8, refs: &mut Vec<BundleRef>, seen: &mut BTreeSet<String>| {
            if seen.contains(name) {
                return;
            }
            if crate::paths::is_unsafe_join_target(name) {
                tracing::warn!("rejecting bundle name with traversal/absolute path: {name}");
                return;
            }
            let path = bundles_dir.join(name);
            if !path.exists() {
                tracing::warn!(
                    "bundle '{}' has no content directory at {}; \
                     skipping (tag-only bundle, or missing/deleted directory)",
                    name,
                    path.display()
                );
                return;
            }
            seen.insert(name.to_owned());
            refs.push(BundleRef {
                name: name.to_owned(),
                path,
                precedence,
            });
        };

    for (tier, kind) in PRECEDENCE.iter().enumerate() {
        let precedence = u8::try_from(PRECEDENCE.len() - tier).unwrap_or(u8::MAX);
        let kind_tags: BTreeSet<&str> = active
            .scopes
            .iter()
            .filter(|s| s.kind == *kind)
            .flat_map(|s| s.tags.iter().map(String::as_str))
            .collect();
        for bundle in firing {
            if bundle.when.iter().any(|t| kind_tags.contains(t.as_str())) {
                push_ref(&bundle.name, precedence, &mut refs, &mut seen);
            }
        }
    }
    for bundle in firing {
        push_ref(&bundle.name, 0, &mut refs, &mut seen);
    }
    refs
}

/// Bundle names any active scope enables via marker `enable_bundles`.
pub fn marker_enabled_bundle_names(active: &ActiveScopes) -> HashSet<String> {
    active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().cloned())
        .collect()
}

/// Bundle names any active scope disables via marker `disable_bundles`
/// (#194).
pub fn marker_disabled_bundle_names(active: &ActiveScopes) -> HashSet<String> {
    active
        .scopes
        .iter()
        .flat_map(|s| s.disable_bundles.iter().cloned())
        .collect()
}

/// Whether `bundle` would be selected by tag intersection or explicit
/// `enable_bundles`, ignoring `disable_bundles` entirely.
pub fn tag_or_marker_selected(
    bundle: &Bundle,
    active: &ActiveScopes,
    manually_enabled: &HashSet<String>,
) -> bool {
    bundle.when.iter().any(|bt| active.tags.contains(bt)) || manually_enabled.contains(&bundle.name)
}

/// Compute the bundles that fire for `active`: tag intersection OR
/// `enable_bundles`, minus anything any scope disables via `disable_bundles`
/// (#194). `tag_filter` (the CLI `--tag` flag) additionally gates a
/// bundle's `when` list when present.
pub fn firing_bundles<'a>(
    bundles: &'a [Bundle],
    active: &ActiveScopes,
    tag_filter: Option<&str>,
) -> Vec<&'a Bundle> {
    let manually_enabled = marker_enabled_bundle_names(active);
    let disabled = marker_disabled_bundle_names(active);
    bundles
        .iter()
        .filter(|b| !disabled.contains(&b.name))
        .filter(|b| tag_filter.is_none_or(|t| b.when.iter().any(|w| w == t)))
        .filter(|b| tag_or_marker_selected(b, active, &manually_enabled))
        .collect()
}
```

- [ ] **Step 2: Register the module**

Find where top-level modules are declared:
`rg -n "^mod cli;" src/*.rs`. Add `mod bundle_select;` next to it, in the
same file.

- [ ] **Step 3: Delete the originals from `cli` and re-export**

In `src/cli/mod.rs`, delete `build_bundle_refs` (lines 3085-3155),
`marker_enabled_bundle_names` (4389-4395), `marker_disabled_bundle_names`
(4403-4412), `tag_or_marker_selected` (4422-4428), and `firing_bundles`
(4443-4456). Move their existing `#[cfg(test)]` tests (search
`rg -n "fn firing_bundles_|fn build_bundle_refs_" src/cli/mod.rs` for the
full list — includes `firing_bundles_tag_matched_bundle_fires`,
`firing_bundles_manually_enabled_bundle_fires_without_matching_tag`,
`build_bundle_refs_orders_by_scope_precedence`,
`build_bundle_refs_content_scope_is_not_lowest_rank`,
`build_bundle_refs_unmatched_enable_bundles_falls_to_catch_all`,
`firing_bundles_disable_suppresses_tag_matched_bundle`,
`firing_bundles_disable_suppresses_manually_enabled_bundle`,
`firing_bundles_disable_does_not_affect_unrelated_bundles`,
`firing_bundles_tag_filter_still_applies_alongside_disable`) into a new
`#[cfg(test)] mod tests` block at the bottom of `src/bundle_select.rs`,
verbatim.

Add near the top of `src/cli/mod.rs`:

```rust
pub(crate) use crate::bundle_select::{
    build_bundle_refs, firing_bundles, marker_disabled_bundle_names,
    marker_enabled_bundle_names, tag_or_marker_selected, BundleRef,
};
```

(`pub(crate) use`, not plain `use` — `cli`'s own other internal code calls
these unqualified today, and the re-export keeps that valid.)

- [ ] **Step 4: Redirect `hook_run`'s call site**

At this point in the sequence, `memory_url` is still in
`src/hook_run/mod.rs` (Task 7 moves it later) and is the only `hook_run`
caller of this cluster — it calls `crate::cli::firing_bundles(&config.bundle, active, None)`
and `crate::cli::build_bundle_refs(config_dir, active, &firing)` (confirmed:
`rg -n "cli::firing_bundles|cli::build_bundle_refs" src/hook_run/mod.rs`
returns exactly these two lines inside `memory_url`). Change both to
`crate::bundle_select::firing_bundles(...)` /
`crate::bundle_select::build_bundle_refs(...)`. When Task 7 later moves
`memory_url` into `memory`, it carries this already-correct call along with
it — no further change needed there.

- [ ] **Step 5: Build and test**

Run: `cargo build --workspace && cargo test -p llmenv --lib bundle_select:: cli::`
Expected: PASS.

- [ ] **Step 6: Format and commit**

```bash
cargo fmt
git add -A
git commit -m "refactor: extract crate::bundle_select out of cli"
```

---

### Task 6: Move `StatusData` schema + `StaleStatus`/`stale_status` into `materialize`; add `materialize::report_if_stale`

**Files:**
- Modify: `src/materialize/status_data.rs` (add the schema types from
  `cli/statusline/data.rs`, add `StaleStatus`, `stale_status`,
  `report_if_stale`)
- Delete or gut: `src/cli/statusline/data.rs` (re-export only, see Step 2)
- Modify: `src/cli/mod.rs:34-78,2667-2742` (delete `StaleStatus`,
  `stale_status`; rewrite `run_check_stale`'s non-auto-fix branch to
  delegate to `materialize::report_if_stale`; add `use`)
- Modify: `src/cli/statusline/mod.rs:193` (update the one direct construction
  site if it names `crate::cli::statusline::data::StatusData` explicitly —
  check `rg -n "statusline::data::" src/cli/`)
- Modify: `src/hook_run/mod.rs:490-491` (update the 2 call sites)

**Interfaces:**
- Produces: `materialize::status_data::StatusData` (and sibling types
  `ScopesData`, `CountData`, `IcmData`, `ThrottleData`, `CacheData`,
  `TasksData` — the whole schema, unchanged field-for-field)
- Produces: `materialize::StaleStatus`,
  `materialize::stale_status(booted: Option<&str>, current: &str) -> StaleStatus`,
  `materialize::report_if_stale(use_color: bool) -> anyhow::Result<()>`

**Design correction found during planning:** the original design doc said
"move `run_check_stale` into `materialize`," but `run_check_stale`'s
`auto_fix: true` branch calls `build_and_materialize` with
`ClaudeCodeAdapter` (`src/cli/mod.rs:2`, `use crate::adapter::claude_code::ClaudeCodeAdapter`)
and a `cli`-local `MaterializeContext` struct (`src/cli/mod.rs:1853`).
Moving the whole function into `materialize` would make `materialize`
depend on `adapter`, which — combined with `adapter`'s existing legitimate
one-way dependency on `materialize` (Task 9) — recreates the exact
`adapter ↔ materialize` cycle Task 9 just broke. `hook_run`'s only call
site (`src/hook_run/mod.rs:491`) always passes `auto_fix=false`
(`crate::cli::run_check_stale(use_color, false)`) and `run_check_stale`
has no other caller besides `cli/mod.rs:740`'s own subcommand dispatch
(verified: `rg -n "run_check_stale" src/ --type rust` returns exactly these
two call sites plus the function's own definition). So only the
non-auto-fix "detect drift, print a warning" logic needs to move — the
auto-fix orchestration stays in `cli`, where its `adapter` dependency is
unproblematic (`adapter` never depends back on `cli`, confirmed in the
design doc's re-verification).

`src/materialize/status_data.rs` already imports the whole
`cli::statusline::data` schema (`use crate::cli::statusline::data::{...}` at
line 17) — this task merges the two files' type definitions into one,
resolving the cycle by making `materialize` the sole owner.

- [ ] **Step 1: Read both files in full before merging**

Run:
```bash
cat src/materialize/status_data.rs
cat src/cli/statusline/data.rs
```
Confirm the full list of types in `data.rs` (seen so far: `StatusData`,
`ScopesData`, `CountData`, `IcmData`, `ThrottleData`, `CacheData`,
`TasksData` — verify no others exist) and how `status_data.rs` currently
imports them (`use crate::cli::statusline::data::{...}` at line 17).

- [ ] **Step 2: Move the schema into `materialize::status_data`**

Paste every type definition from `src/cli/statusline/data.rs` (all `#[derive(...)]`
structs — `StatusData` and its siblings) directly into
`src/materialize/status_data.rs`, replacing the `use crate::cli::statusline::data::{...}`
import line with the pasted definitions. Keep every `#[derive(...)]`
attribute and doc comment unchanged — these are pure data types, no logic to
adapt.

Replace the body of `src/cli/statusline/data.rs` with a re-export, keeping
the file's own doc comment:

```rust
//! `llmenv-status.json` — llmenv-sourced stats consumed by the statusline
//! renderer. Pure parsing only: no scope resolution, no MCP calls, no
//! business logic. All fields written once at data-file-write time by
//! `src/materialize/status_data.rs`, which is also where these types are
//! now defined.

pub use crate::materialize::status_data::{
    CacheData, CountData, IcmData, ScopesData, StatusData, TasksData, ThrottleData,
};
```

(Adjust the type list to match whatever Step 1 actually found — this is the
expected set based on the struct fields already seen, not a guess to leave
unchecked.)

- [ ] **Step 3: Move `StaleStatus`/`stale_status`/`run_check_stale` into `materialize`**

In `src/materialize/status_data.rs` (or a new `src/materialize/stale.rs` if
`status_data.rs` is already large — check its line count with `wc -l`; if
over ~400 lines, create the new file and add `mod stale;` /
`pub use stale::*;` in `src/materialize/mod.rs` instead), add:

```rust
/// Compares the content hash an agent booted with against a freshly
/// computed current hash — used by `check-stale` and by `hook_run`'s
/// `SessionStart` drift check so the two can't disagree about whether
/// config drifted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleStatus {
    /// Booted hash matches the current one — the session is up to date.
    Fresh,
    /// Config drifted since the agent booted; the user should restart.
    Stale { booted: String, current: String },
    /// No booted hash to compare against (llmenv didn't boot this agent, or
    /// the booted folder predates the manifest dotfile).
    Unknown,
}

impl StaleStatus {
    /// True only when the booted config no longer matches the current one.
    #[must_use]
    pub fn is_drift(&self) -> bool {
        matches!(self, StaleStatus::Stale { .. })
    }
}

/// Compare the content hash the agent booted with against the freshly
/// computed current hash. `booted` is the `content_hash` read from the
/// booted folder's manifest dotfile; `None` when the agent wasn't booted by
/// llmenv or the booted folder has no manifest.
#[must_use]
pub fn stale_status(booted: Option<&str>, current: &str) -> StaleStatus {
    match booted {
        None => StaleStatus::Unknown,
        Some(b) if b == current => StaleStatus::Fresh,
        Some(b) => StaleStatus::Stale {
            booted: b.to_string(),
            current: current.to_string(),
        },
    }
}
```

Add `materialize::report_if_stale`, covering only the non-auto-fix branch —
the full original `run_check_stale` body (`src/cli/mod.rs:2667-2742`) is:

```rust
pub(crate) fn run_check_stale(use_color: bool, auto_fix: bool) -> anyhow::Result<()> {
    let booted = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .and_then(|dir| {
            crate::materialize::manifest::CacheManifest::read(&dir)
                .ok()
                .flatten()
                .map(|m| m.content_hash)
        });

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let config_dir = paths::config_dir()?;

    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let firing: Vec<&Bundle> = firing_bundles(&config.bundle, &active, None);

    let current = match build_manifest(&config, &config_dir, &active, &firing, false)? {
        Some((manifest, _)) => crate::materialize::cache::hash_manifest(&manifest)?,
        None => {
            return Ok(());
        }
    };

    match stale_status(booted.as_deref(), &current) {
        StaleStatus::Stale { .. } => {
            if auto_fix {
                let materialize_ctx = MaterializeContext {
                    config: &config,
                    config_dir: &config_dir,
                    active: &active,
                    firing: &firing,
                };
                match build_and_materialize(&ClaudeCodeAdapter, materialize_ctx, false) {
                    Ok(Some((cache_path, _))) => {
                        eprintln!("✓ Config refreshed at {}", cache_path.display());
                    }
                    Ok(None) => {
                        eprintln!("✓ Config up-to-date (no content directory)");
                    }
                    Err(e) => return Err(e).context("auto-fix: re-materialization failed"),
                }
            } else {
                let warn = doctor_warning(use_color);
                eprintln!(
                    "{warn} llmenv config changed in place; restart your agent to load it. \
                     (Bundles, MCP wiring, or plugin paths changed since this session started.)"
                );
            }
        }
        StaleStatus::Fresh => {}
        StaleStatus::Unknown => {
            tracing::debug!(
                "check-stale: no booted manifest hash to compare against; \
                 drift detection skipped (current hash would be {current})"
            );
        }
    }
    Ok(())
}
```

Everything up through computing `current` and calling `stale_status`, plus
the `Fresh`/`Unknown`/non-auto-fix-`Stale` arms, has no dependency on
`adapter`. Only the `if auto_fix { ... }` branch does. Split it: add to
`materialize` (in `status_data.rs` or the new `stale.rs` per the file-size
check above):

```rust
/// Detect config drift (booted vs. current content hash) and print a
/// warning to stderr if drifted. The auto-fix path (re-materializing via a
/// specific adapter) stays in `cli::run_check_stale`, which owns the
/// `adapter` dependency that auto-fix needs — `materialize` must not depend
/// on `adapter` (see this task's design-correction note above).
pub fn report_if_stale(use_color: bool) -> anyhow::Result<()> {
    let booted = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .and_then(|dir| {
            crate::materialize::manifest::CacheManifest::read(&dir)
                .ok()
                .flatten()
                .map(|m| m.content_hash)
        });

    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let config_dir = crate::paths::config_dir()?;

    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let firing = crate::bundle_select::firing_bundles(&config.bundle, &active, None);

    let current = match crate::materialize::build_manifest(&config, &config_dir, &active, &firing, false)? {
        Some((manifest, _)) => crate::materialize::cache::hash_manifest(&manifest)?,
        None => return Ok(()),
    };

    match stale_status(booted.as_deref(), &current) {
        StaleStatus::Stale { .. } => {
            let warn = llmenv_util::doctor_warning(use_color);
            eprintln!(
                "{warn} llmenv config changed in place; restart your agent to load it. \
                 (Bundles, MCP wiring, or plugin paths changed since this session started.)"
            );
        }
        StaleStatus::Fresh => {}
        StaleStatus::Unknown => {
            tracing::debug!(
                "check-stale: no booted manifest hash to compare against; \
                 drift detection skipped (current hash would be {current})"
            );
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Delete `StaleStatus`/`stale_status` from `cli`, rewrite `run_check_stale` to delegate**

In `src/cli/mod.rs`, delete `StaleStatus` (lines 34-43), its `impl` block
(45-51), and `stale_status` (56-67). Add near the top:

```rust
pub(crate) use crate::materialize::{stale_status, StaleStatus};
```

Replace the body of `run_check_stale` (lines 2667-2742) with:

```rust
pub(crate) fn run_check_stale(use_color: bool, auto_fix: bool) -> anyhow::Result<()> {
    if !auto_fix {
        return crate::materialize::report_if_stale(use_color);
    }

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let config_dir = paths::config_dir()?;

    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let firing: Vec<&Bundle> = firing_bundles(&config.bundle, &active, None);

    let current = match build_manifest(&config, &config_dir, &active, &firing, false)? {
        Some((manifest, _)) => crate::materialize::cache::hash_manifest(&manifest)?,
        None => return Ok(()),
    };

    let booted = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .and_then(|dir| {
            crate::materialize::manifest::CacheManifest::read(&dir)
                .ok()
                .flatten()
                .map(|m| m.content_hash)
        });

    if let StaleStatus::Stale { .. } = stale_status(booted.as_deref(), &current) {
        let materialize_ctx = MaterializeContext {
            config: &config,
            config_dir: &config_dir,
            active: &active,
            firing: &firing,
        };
        match build_and_materialize(&ClaudeCodeAdapter, materialize_ctx, false) {
            Ok(Some((cache_path, _))) => {
                eprintln!("✓ Config refreshed at {}", cache_path.display());
            }
            Ok(None) => {
                eprintln!("✓ Config up-to-date (no content directory)");
            }
            Err(e) => return Err(e).context("auto-fix: re-materialization failed"),
        }
    }
    Ok(())
}
```

(`firing_bundles`/`build_manifest` here already resolve to
`crate::bundle_select::firing_bundles` / `crate::materialize::build_manifest`
via Task 5's re-export and `cli`'s existing `use` of `materialize`'s
`build_manifest` — no new imports needed if those `use` lines already exist
in this file; add them if `cargo build` reports them missing.)

- [ ] **Step 5: Redirect `hook_run`'s call site**

`src/hook_run/mod.rs:491` currently calls
`crate::cli::run_check_stale(use_color, false)`. Change to
`crate::materialize::report_if_stale(use_color)` (drop the now-redundant
`false` argument — `report_if_stale` has no `auto_fix` parameter, since
`hook_run` never used that branch).

- [ ] **Step 6: Check `cli/statusline/mod.rs` for a direct path reference**

Run `rg -n "statusline::data::" src/cli/`. If any call site names
`crate::cli::statusline::data::StatusData` (or a sibling type) explicitly
rather than through a bare `StatusData` import, it still resolves correctly
through Step 2's re-export — no change required unless the lint step below
flags an unused-import warning, in which case redirect that one import to
`crate::materialize::status_data::StatusData` directly.

- [ ] **Step 7: Build and test**

Run: `cargo build --workspace && cargo test -p llmenv --lib materialize:: cli::`
Expected: PASS.

- [ ] **Step 8: Verify the cycles are broken**

Run:
```bash
rg -l "crate::cli" src/materialize/ --type rust
```
Expected: no output (previously matched `src/materialize/status_data.rs`).

- [ ] **Step 9: Format and commit**

```bash
cargo fmt
git add -A
git commit -m "refactor: move StatusData schema and stale-check into materialize"
```

---

### Task 7: Move the `memory_url` cluster into `memory`

**This task's scope grew during planning.** `memory_url` is not a standalone
function — it sits in a contiguous, mutually-dependent cluster with
`MemoryEndpoint` (a large, heavily-tested enum), `classify_missing_memory`,
`annotate_resolve_error`, `suppressed_bundle_capabilities`, and
`suppressed_memory_bundles`. All six must move together (verified: every
top-level item in `src/hook_run/mod.rs` lines 1729-2128 was enumerated with
`rg -n "^(pub\(crate\) )?(fn|enum|struct|impl) " src/hook_run/mod.rs`, then
each item's callers were checked individually). One item in that same line
range — `ResolvedMemoryClient`/`resolve_memory_client` (lines 1821-1883) —
is **not** part of this cluster (its only callers, at
`src/hook_run/mod.rs:1114` and a test at `:3160`, are unrelated hook-dispatch
code) and stays in `hook_run` untouched.

**Files:**
- Modify: `src/memory/mod.rs` (add the 6-item cluster below)
- Modify: `src/hook_run/mod.rs` (delete the same 6 items, lines
  1729-1820 and 1884-2128 — leaving the 1821-1883 `ResolvedMemoryClient`
  carve-out in place; extract and move their tests, scattered inside the
  file's single shared `#[cfg(test)] mod tests` block starting at line 2529)
- Modify: `src/memory/mod.rs:31`, `src/memory/prune.rs:88` (already call
  `crate::hook_run::memory_url` — update to a local/unqualified call)
- Modify: `src/session_log/detached.rs:113` (update the call site)
- Modify: `src/cli/doctor.rs:301` (`crate::hook_run::suppressed_memory_bundles`
  → `crate::memory::suppressed_memory_bundles` — this external caller was
  missed in the design doc's original per-cycle table; it does not
  reintroduce a cycle, since `cli` already depends on `hook_run` one-way
  and gaining an additional one-way dependency on `memory` is harmless, but
  it must still be updated or the build breaks)

**Interfaces:**
- Produces: `memory::memory_url(config: &crate::config::Config, config_dir: &Path, active: &crate::scope::ActiveScopes) -> anyhow::Result<MemoryEndpoint>`
  (same signature as today)
- Produces: `memory::MemoryEndpoint` (enum + `impl`, `pub` — was
  `pub(crate)`, since external crates don't exist here but `memory_url`'s
  callers outside `hook_run` need it visible from their own modules)
- Produces: `memory::suppressed_memory_bundles(config: &crate::config::Config, config_dir: &Path, active: &crate::scope::ActiveScopes) -> Vec<String>`
  (signature per its existing external call site at `cli/doctor.rs:301` —
  read the exact return type there before finalizing if it differs)

- [ ] **Step 1: Confirm the exact boundaries before cutting**

Run:
```bash
sed -n '1729,2128p' src/hook_run/mod.rs
```
Confirm it contains exactly: `MemoryEndpoint` enum (1729-1758), its `impl`
(1759-1820), `memory_url` (1884-1969), `classify_missing_memory`
(1970-2014), `annotate_resolve_error` (2015-2048),
`suppressed_bundle_capabilities` (2049-2087), `suppressed_memory_bundles`
(2088-2120), `resolve_bundle_memory_host` (2121-2128) — with
`ResolvedMemoryClient`/`resolve_memory_client` (1821-1883) sitting in the
middle, to be skipped. If the actual content differs from this (line
numbers drift after Tasks 1-6's edits touch other parts of the file — none
of those tasks touch this range, but re-verify rather than assume), adjust
the cut to match reality, not this description.

- [ ] **Step 2: Move the 6-item cluster into `memory`, skipping the `ResolvedMemoryClient` carve-out**

Paste the two contiguous chunks (1729-1820, then 1884-2128) into
`src/memory/mod.rs`, in that order, as one block. Within the pasted code,
make these signature changes:
- `pub(crate) enum MemoryEndpoint` → `pub enum MemoryEndpoint`
- `pub(crate) fn memory_url` → `pub fn memory_url`
- `pub(crate) fn suppressed_memory_bundles` → `pub fn suppressed_memory_bundles`
- `classify_missing_memory`, `annotate_resolve_error`,
  `suppressed_bundle_capabilities`, `resolve_bundle_memory_host` stay
  private (`fn`, no `pub`) — none of them are called from outside this
  cluster.

Within the pasted `memory_url` body, change:

```rust
let firing = crate::cli::firing_bundles(&config.bundle, active, None);
let bundle_refs = crate::cli::build_bundle_refs(config_dir, active, &firing);
```

to:

```rust
let firing = crate::bundle_select::firing_bundles(&config.bundle, active, None);
let bundle_refs = crate::bundle_select::build_bundle_refs(config_dir, active, &firing);
```

(the pre-Task-5 path — Task 5 already redirected this exact call site once;
if Task 5 already landed before this task runs, the pasted code will
already read `crate::bundle_select::*` and this sub-step is a no-op, just
confirm it via `grep`.)

Any other `crate::hook_run::*` reference inside the pasted block (e.g. a
call to `resolve_mcps`, `ResolvedKind`, `MEMORY_MCP_NAME` from
`crate::mcp::resolve`, or `crate::config::*` types) needs no change — those
already resolve correctly with an absolute `crate::` path regardless of
which module the calling code lives in.

- [ ] **Step 3: Delete the moved cluster from `hook_run`, leaving the carve-out**

Delete lines 1729-1820 and 1884-2128 from `src/hook_run/mod.rs` (leaving
1821-1883, `ResolvedMemoryClient`/`resolve_memory_client`, exactly as-is).
Run `rg -n "MemoryEndpoint|memory_url|classify_missing_memory|annotate_resolve_error|suppressed_bundle_capabilities|suppressed_memory_bundles" src/hook_run/mod.rs`
afterward — every remaining match outside the (now-deleted) production
range should be either a doc-comment cross-reference (fine to leave, or
update to `memory::` for accuracy) or a test in the shared `mod tests`
block (handled in Step 4).

- [ ] **Step 4: Find and relocate the cluster's tests**

The single `#[cfg(test)] mod tests` block starts at line 2529 (post-Step-3
line numbers will shift — re-locate it with
`rg -n "^mod tests" src/hook_run/mod.rs` after Step 3) and contains
thousands of lines covering all of `hook_run`, not just this cluster. Find
every test referencing the moved items:

```bash
rg -n "fn \w*(memory_url|MemoryEndpoint|classify_missing_memory|suppressed_memory_bundles|suppressed_bundle_capabilities|annotate_resolve_error)\w*" src/hook_run/mod.rs
```

(known from design research so far: `memory_url_uses_persisted_cache_when_key_matches`,
plus everything the earlier `MemoryEndpoint`/`classify_missing_memory`
searches surfaced around lines 2938, 3008, 3252, 3344, 3389, 3416, 3444,
3502, 3558, 3680, 4775, 4786, 4797, 4863-4952 in the pre-Step-3 file — this
list is a floor, not a ceiling; the `rg` command above is the authoritative
source, run it fresh rather than trusting this enumeration). For each match,
read the full test function, cut it from `hook_run/mod.rs`'s test module,
and paste it into a new `#[cfg(test)] mod tests` block in `src/memory/mod.rs`
(create one if `memory/mod.rs` doesn't already have one). A test that
constructs `MemoryEndpoint` variants via helpers defined elsewhere in
`hook_run`'s test module (fixture builders, mock configs) needs those
fixtures moved or duplicated alongside it — check for `fn make_test_config`-
style helpers shared across many unrelated tests; if a fixture is used by
both moved and non-moved tests, duplicate it into `memory`'s test module
rather than trying to share one copy across modules (test-only code, not
subject to the same DRY pressure as production code).

- [ ] **Step 5: Redirect the 4 external call sites**

- `src/memory/mod.rs:31` and `src/memory/prune.rs:88`: currently
  `crate::hook_run::memory_url(...)` — since the function now lives in
  `memory` itself, change to a local call (`memory_url(...)` if in the same
  file as the new definition, or `crate::memory::memory_url(...)` if in
  `prune.rs`).
- `src/session_log/detached.rs:113`: change
  `crate::hook_run::memory_url(&config, config_dir, &active)?` to
  `crate::memory::memory_url(&config, config_dir, &active)?`.
- `src/cli/doctor.rs:301`: change
  `crate::hook_run::suppressed_memory_bundles(config, config_dir, active)`
  to `crate::memory::suppressed_memory_bundles(config, config_dir, active)`.

- [ ] **Step 6: Build and test**

Run: `cargo build --workspace && cargo test -p llmenv --lib memory:: hook_run:: session_log::detached:: cli::doctor::`
Expected: PASS — every relocated test passes unchanged (assertions
untouched, only `use`/`crate::` paths differ).

- [ ] **Step 7: Verify no stray references remain**

Run: `rg -n "hook_run::memory_url|hook_run::MemoryEndpoint|hook_run::suppressed_memory_bundles" src/ --type rust`
Expected: no output.

- [ ] **Step 8: Format and commit**

```bash
cargo fmt
git add -A
git commit -m "refactor: move memory_url and its resolution cluster into memory"
```

---

### Task 8: Move `redirect_stderr_to_detached_log`/`detached_child_log_path`/`redirect_stderr_to_bounded_log` into `session_log`

**Files:**
- Modify: `src/session_log/mod.rs` or a new `src/session_log/detached_log.rs`
  (add the 3 functions + their tests)
- Modify: `src/hook_run/mod.rs:2369-2416` (delete all 3, update the
  in-file caller at line ~2477, add `use`)
- Modify: `src/session_log/detached.rs:63-65` (update the call site)

**Interfaces:**
- Produces: `session_log::redirect_stderr_to_bounded_log(cmd: &mut std::process::Command, log_path: &Path, dir_mode: llmenv_mcp::proxy::LogDirMode, context: &str)`
  (`pub(crate)`, since only `hook_run` and `session_log` itself call it)
- Produces: `session_log::redirect_stderr_to_detached_log(cmd: &mut std::process::Command, log_path: impl FnOnce() -> anyhow::Result<PathBuf>)`
- Produces: `session_log::detached_child_log_path() -> anyhow::Result<PathBuf>`

`redirect_stderr_to_bounded_log` has a second caller inside `hook_run/mod.rs`
itself (around line 2477, found during design research) beyond
`redirect_stderr_to_detached_log` — that caller must be redirected too, not
just `session_log`'s.

- [ ] **Step 1: Read all 3 functions and their tests in full**

Run:
```bash
sed -n '2345,2420p' src/hook_run/mod.rs
rg -n "fn redirect_stderr_to_bounded_log_captures_child_stderr|fn detached_child_log_path_is_named_under_the_state_dir|fn redirect_stderr_to_detached_log_writes_to_the_resolved_path" -A20 src/hook_run/mod.rs
```

- [ ] **Step 2: Move the 3 functions into `session_log`**

Create `src/session_log/detached_log.rs`:

```rust
//! Detached-process stderr redirection, shared by `hook_run`'s subprocess
//! spawns and `session_log`'s own detached record/store paths.

const BOUNDED_LOG_MAX_BYTES: u64 = 1 << 19; // 512 KiB

pub(crate) fn detached_child_log_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::paths::state_dir()?.join("detached-hook.log"))
}

pub(crate) fn redirect_stderr_to_bounded_log(
    cmd: &mut std::process::Command,
    log_path: &std::path::Path,
    dir_mode: llmenv_mcp::proxy::LogDirMode,
    context: &str,
) {
    cmd.stderr(std::process::Stdio::null());
    match llmenv_mcp::proxy::open_bounded_log(log_path, BOUNDED_LOG_MAX_BYTES, dir_mode) {
        Ok(file) => {
            cmd.stderr(std::process::Stdio::from(file));
        }
        Err(e) => {
            tracing::debug!("{context}: log unavailable ({e:#}), stderr discarded");
        }
    }
}

pub(crate) fn redirect_stderr_to_detached_log(
    cmd: &mut std::process::Command,
    log_path: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) {
    match log_path() {
        Ok(path) => redirect_stderr_to_bounded_log(
            cmd,
            &path,
            llmenv_mcp::proxy::LogDirMode::OwnerOnly,
            "detached child",
        ),
        Err(e) => {
            cmd.stderr(std::process::Stdio::null());
            tracing::debug!("detached child: cannot resolve log path ({e:#}), stderr discarded");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // #1133: the detached memory children were spawned with
    // `stderr(Stdio::null())`, so nothing they reported could reach anyone —
    // including the `tracing` events meant to compensate, whose sink is that
    // same discarded stderr.
    #[test]
    fn redirect_stderr_to_bounded_log_captures_child_stderr() {
        let dir = tempfile::tempdir().expect("test");
        let log = dir.path().join("detached-hook.log");
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("echo boom >&2");
        redirect_stderr_to_bounded_log(
            &mut cmd,
            &log,
            llmenv_mcp::proxy::LogDirMode::OwnerOnly,
            "test",
        );

        assert!(cmd.status().expect("test").success());
        let body = std::fs::read_to_string(&log)
            .expect("a detached child's stderr must reach a file, not /dev/null");
        assert!(body.contains("boom"), "stderr not captured: {body}");
    }

    // Pins the shared log name: the three detached children, the docs, and any
    // operator told where to look must all agree on one path.
    #[test]
    fn detached_child_log_path_is_named_under_the_state_dir() {
        let path = detached_child_log_path().expect("test");
        assert_eq!(
            path.file_name().and_then(std::ffi::OsStr::to_str),
            Some("detached-hook.log")
        );
        assert!(path.starts_with(crate::paths::state_dir().expect("test")));
    }

    /// End-to-end coverage for `redirect_stderr_to_detached_log` itself, not
    /// just the `redirect_stderr_to_bounded_log` helper it delegates to
    /// (`redirect_stderr_to_bounded_log_captures_child_stderr` above already
    /// covers that) — this calls the exact same function signature real
    /// callers do, with an injected path resolver instead of the real
    /// `detached_child_log_path`, so it's the one test that would catch this
    /// function's own body being replaced wholesale.
    #[test]
    fn redirect_stderr_to_detached_log_writes_to_the_resolved_path() {
        let dir = tempfile::tempdir().expect("test");
        let log_path = dir.path().join("detached-hook.log");
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("echo boom >&2");
        redirect_stderr_to_detached_log(&mut cmd, || Ok(log_path.clone()));

        assert!(cmd.status().expect("test").success());
        let body = std::fs::read_to_string(&log_path)
            .expect("a detached child's stderr must reach the resolved log path");
        assert!(body.contains("boom"), "stderr not captured: {body}");
    }
}
```

The three tests reference `llmenv_mcp::proxy::LogDirMode` (adjusted above
from the original `crate::mcp::proxy::LogDirMode` — `mcp_client`'s own
cross-module use of `crate::mcp::proxy` already established this is the
`llmenv-mcp` crate's `proxy` module) and otherwise use only `std`/`tempfile`,
no `hook_run`-specific fixtures.

`BOUNDED_LOG_MAX_BYTES` (verified: `src/hook_run/mod.rs:2338`) has exactly
one caller, `redirect_stderr_to_bounded_log` itself (`:2376`) — no other
`hook_run` code references it, so it moves wholesale with no re-export
needed back into `hook_run`.

Add `mod detached_log;` and
`pub(crate) use detached_log::{detached_child_log_path, redirect_stderr_to_bounded_log, redirect_stderr_to_detached_log};`
to `src/session_log/mod.rs`.

- [ ] **Step 3: Delete the originals from `hook_run` and redirect its own caller**

Delete all 3 functions from `src/hook_run/mod.rs` (lines 2345-2416 per
Step 1's range) and their 3 tests. Find the other in-file caller of
`redirect_stderr_to_bounded_log` (around line 2477) and change it to
`crate::session_log::redirect_stderr_to_bounded_log`.

- [ ] **Step 4: Redirect `session_log::detached`'s call site**

`src/session_log/detached.rs:63-65` currently calls
`crate::hook_run::redirect_stderr_to_detached_log` and
`crate::hook_run::detached_child_log_path`. Since both now live in the same
crate module tree (`session_log::detached_log`), change to unqualified
calls (`redirect_stderr_to_detached_log(...)`,
`detached_child_log_path`) via `use super::detached_log::{...}` or the
module path that fits how `session_log`'s submodules already reference each
other — check an existing sibling import in `detached.rs` for the house
style before choosing.

- [ ] **Step 5: Build and test**

Run: `cargo build --workspace && cargo test -p llmenv --lib session_log:: hook_run::`
Expected: PASS.

- [ ] **Step 6: Verify the cycle is broken**

Run: `rg -l "crate::hook_run" src/session_log/ --type rust`
Expected: no output (previously matched multiple files in this cycle's
design research — `bundle_keyword`/`tag_keyword` cleared in Task 4,
`McpHttpClient` cleared in Task 3, this task clears the remainder).

- [ ] **Step 7: Format and commit**

```bash
cargo fmt
git add -A
git commit -m "refactor: move detached stderr-redirect helpers into session_log"
```

---

### Task 9: Redirect `materialize::inherit`'s `create_dir_owner_only` calls to `paths`

**Files:**
- Modify: `src/materialize/inherit.rs:99,851`

**Interfaces:**
- Consumes: `crate::paths::create_dir_owner_only(dir: &Path) -> anyhow::Result<()>`
  (already exists — `adapter::skills::create_dir_owner_only` is itself a
  2-line wrapper around this exact function, per the design doc)

No new symbol is produced; this task only changes which existing function
two call sites reach.

- [ ] **Step 1: Redirect the 2 call sites**

In `src/materialize/inherit.rs:99` and `:851`, change
`crate::adapter::skills::create_dir_owner_only(...)` to
`crate::paths::create_dir_owner_only(...)`. Check
`src/adapter/skills.rs:21-24`'s exact wrapper signature first
(`pub(crate) fn create_dir_owner_only(dir: &Path) -> anyhow::Result<()>`) to
confirm the underlying `paths` function has the identical signature (it
does — the wrapper does nothing but forward the call and wrap the error
message; re-check the wrapped error message text at
`src/adapter/skills.rs:23` — `"failed to create dir {}: {e}"` — and decide
whether `materialize`'s 2 call sites relied on that exact wording anywhere,
e.g. in a test asserting on error text; if so, wrap the `paths` call the
same way inline rather than silently losing the context, using
`.map_err(|e| anyhow::anyhow!("failed to create dir {}: {e}", dir.display()))`).

- [ ] **Step 2: Build and test**

Run: `cargo build --workspace && cargo test -p llmenv --lib materialize::inherit::`
Expected: PASS.

- [ ] **Step 3: Verify the cycle is broken**

Run: `rg -l "crate::adapter" src/materialize/ --type rust`
Expected: no output (previously matched `src/materialize/inherit.rs`).

- [ ] **Step 4: Format and commit**

```bash
cargo fmt
git add src/materialize/inherit.rs
git commit -m "refactor: use paths::create_dir_owner_only directly in materialize"
```

---

## Final Verification (after all 9 tasks)

- [ ] **Re-run the full audit for all 7 cycles**

```bash
for pair in "adapter hook_run" "adapter materialize" "cli hook_run" \
            "cli materialize" "materialize merge" "consolidation hook_run" \
            "hook_run session_log"; do
  read -r a b <<< "$pair"
  echo "-- $a <-> $b --"
  echo "fwd ($a -> $b):"; rg -l "crate::$b" "src/$a/" --type rust
  echo "bwd ($b -> $a):"; rg -l "crate::$a" "src/$b/" --type rust
done
```

Expected: every pair shows at most one direction with matches (the
"heavy, kept" direction named in the design doc's per-cycle table), never
both.

- [ ] **Cross-check with `cargo-modules`**

```bash
cargo modules dependencies --package llmenv --no-fns --no-types --no-traits --no-externs --max-depth 3
```

Confirm no circular edge remains among `adapter`, `cli`, `hook_run`,
`materialize`, `merge`, `consolidation`, `session_log`.

- [ ] **Full workspace check**

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

Expected: all green.

- [ ] **Update `AGENTS.md` and `website/docs/` crate references if any exist**

Run `rg -n "llmenv-config\`, \`llmenv-paths\`, \`llmenv-git\`, \`llmenv-util\`" AGENTS.md website/docs/`
— if any doc enumerates the workspace crate list, it does not need updating
by this plan (`llmenv-scope`/`llmenv-mcp`/`llmenv-util` already existed
before this work; no new crates are created here), but check anyway since
this was flagged as a concern in the original #1339 issue scope.

- [ ] **Close out #1462**

Comment on #1462 confirming the design (already merged) and this plan are
complete, link the merged PR(s), and note that `adapter`/`cli`/`hook_run`/
`materialize`/`merge`/`consolidation`/`session_log` are now independently
extractable — file the follow-up extraction issues per the design doc's
"Follow-up issues" section, referencing #1459-#1461's pattern.
