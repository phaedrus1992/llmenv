//! Permission rules of the named presets, shared by the capability merge (#3336 offset).

use crate::config::{PermissionPreset, PermissionRule};

fn bash_rule(pattern: &str) -> PermissionRule {
    PermissionRule {
        tool: "Bash".to_string(),
        pattern: Some(pattern.to_string()),
        ..Default::default()
    }
}

/// The curated `allow` rules a [`PermissionPreset`] expands to (#975).
///
/// A tool ends up here only if this project's own bundled rules
/// (`examples/config-llmenv-dir/bundles/base/AGENTS.md`'s "CLI Tools" table)
/// name it as the recommended read-only replacement for a common shell
/// command, or #975 named it directly as a safe read-only `git`/`ls`
/// invocation. `git status`/`diff`/`log`/`show`/`blame` and `ls` get both a
/// bare form and a `"<cmd> *"` glob — the bare form is their dominant
/// invocation, unlike e.g. `cargo`, which is never run with zero arguments.
///
/// `rg`, `ast-grep`, and `shfmt` each have a flag that turns a "read-only"
/// invocation into arbitrary command execution or an in-place file write
/// (`rg --pre`/`--hostname-bin`, `ast-grep -U`, `shfmt -w`) — verified
/// against each tool's own `--help`. A blanket `"<tool> *"` allow can't
/// exclude just those flags, so [`safe_readonly_deny_rules_for`] denies them
/// specifically; deny wins over allow for every adapter this preset
/// targets. Verified this is actually sufficient for each of these three
/// (no clustering path exists to hide the flag from the deny glob): `rg`'s
/// dangerous flags have no short forms at all; `ast-grep`'s only other
/// boolean short flag (`-i`) is mutually exclusive with `-U`; `shfmt`'s Go
/// `flag` package doesn't support short-flag clustering at all
/// (`shfmt -sw file` is rejected outright).
///
/// `fd` is deliberately **not** in this list despite being named directly by
/// #975 and the CLI-tools table: `fd -x`/`-X`/`--exec` has the same class of
/// escape, but `fd` (like most clap-based Rust CLIs) supports GNU-style
/// short-flag clustering, and has ~11 other boolean short flags any of
/// which can precede `-x`/`-X` in a cluster (`fd -Lx cmd .` runs `cmd`
/// despite denying `*-x*` — verified locally, `-x` never appears as its own
/// substring). Enumerating every clustering prefix as a `deny` glob is
/// combinatorially infeasible and still fragile against a future flag. See
/// #1219 for revisiting this once there's a genuinely robust fix.
///
/// This is a denylist of the flags found during #975's review, not a proof
/// that no other escape exists on the tools that *are* listed — a future
/// flag on any of them could reopen the same class of gap.
pub(super) fn safe_readonly_allow_rules_for(preset: PermissionPreset) -> Vec<PermissionRule> {
    match preset {
        PermissionPreset::SafeReadonly => vec![
            bash_rule("rg *"),
            bash_rule("ast-grep *"),
            bash_rule("shellcheck *"),
            bash_rule("shfmt *"),
            bash_rule("git status"),
            bash_rule("git status *"),
            bash_rule("git diff"),
            bash_rule("git diff *"),
            bash_rule("git log"),
            bash_rule("git log *"),
            bash_rule("git show"),
            bash_rule("git show *"),
            bash_rule("git blame"),
            bash_rule("git blame *"),
            bash_rule("ls"),
            bash_rule("ls *"),
        ],
    }
}

/// The `deny` rules that close the escapes documented on
/// [`safe_readonly_allow_rules_for`] — always expanded alongside it, never
/// independently, since a deny with no matching allow is a no-op.
pub(super) fn safe_readonly_deny_rules_for(preset: PermissionPreset) -> Vec<PermissionRule> {
    match preset {
        PermissionPreset::SafeReadonly => vec![
            bash_rule("rg *--pre*"),
            bash_rule("rg *--hostname-bin*"),
            bash_rule("ast-grep *-U*"),
            bash_rule("ast-grep *--update-all*"),
            bash_rule("shfmt *-w*"),
            bash_rule("shfmt *--write*"),
        ],
    }
}
