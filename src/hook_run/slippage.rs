//! Behavioural-slippage layers that need hook logic (#317, phase 2).
//!
//! Phase 1 shipped the layers a config fragment can express — `effort_level`,
//! the compact-survival CLAUDE.md fragment, the `/diagnose` skill. The
//! remaining layers need to run at a hook event, which is what lives here.
//!
//! Every layer is advisory. A `Stop` hook that *blocks* can trap an agent in a
//! loop it has no way to exit — it stops, gets told no, stops again — so the
//! upstream project's blocking mode is deliberately not ported. The value is
//! in the reminder arriving at the moment the behaviour slips, not in
//! enforcement.

use crate::config::SlippageControl;
use crate::hook_run::shell_split::{
    NestingTooDeep, nesting_reason, program, program_words, shell_segments,
};

/// The self-critique checklist appended at `Stop` (#317).
///
/// Deliberately short. It competes for attention with whatever else the Stop
/// hook emits (the task-tracker reminder, repeat-detect warnings), and a long
/// checklist at the end of every turn is the kind of thing that gets skimmed
/// into invisibility.
const SELF_CRITIQUE: &str = "Before finishing, check: did you run the tests and read the output? \
Is anything you noticed and didn't explain still unexplained? Is the whole ask done, or only the \
easy part? If any answer is no, say so plainly rather than reporting success.";

/// The `Stop`-hook contribution: the self-critique checklist, or an empty
/// string when the layer is off.
///
/// `cfg` is the resolved `features.slippage` block. Both the master switch and
/// the per-layer toggle have to be on: `enabled: false` means the whole
/// feature is inert regardless of what the individual layers say, which is
/// what makes it a single switch to turn off in a hurry.
pub(crate) fn handle_stop(cfg: Option<&SlippageControl>) -> String {
    let Some(cfg) = cfg else {
        return String::new();
    };
    if !cfg.enabled || !cfg.self_critique {
        return String::new();
    }
    SELF_CRITIQUE.to_string()
}

/// The per-turn rules digest (#317).
///
/// Rule forgetting is the failure this targets: instructions read at session
/// start lose priority as the context fills, and a compaction can drop them
/// entirely. Re-stating them on each turn costs tokens every turn, which is
/// why the digest is deliberately a handful of lines rather than a re-send of
/// the whole rule set — the aim is to keep the *shape* of the rules present,
/// not to re-teach them.
const RULES_DIGEST: &str = "Standing rules for this session: finish the whole task, not the easy part; verify with tests or a real run before reporting success; say plainly when something is unverified, blocked, or skipped; ask before destructive or outward-facing actions.";

/// The `UserPromptSubmit` contribution: the rules digest, or an empty string
/// when the layer is off.
///
/// `SessionStart` deliberately doesn't carry this. The rules are already in
/// CLAUDE.md at that point, so injecting them again at session start is pure
/// duplication; the value is entirely in the *re-*injection later, once the
/// original has aged out of attention.
pub(crate) fn handle_turn(cfg: Option<&SlippageControl>) -> String {
    let Some(cfg) = cfg else {
        return String::new();
    };
    if !cfg.enabled || !cfg.rule_reinjection {
        return String::new();
    }
    RULES_DIGEST.to_string()
}

/// Per-session record of which paths have been read (#317, `read_before_edit`).
///
/// Its own file rather than a reuse of `read_once`'s cache: that cache only
/// exists when `features.read_once` is enabled, so sharing it would make this
/// layer silently depend on an unrelated feature being on.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct SessionStats {
    /// Paths read this session (`read_before_edit`).
    #[serde(default)]
    pub(super) paths: std::collections::BTreeSet<String>,
    /// Tool-call counts by name (`metrics`). `#[serde(default)]` on both
    /// fields so a log written before either layer existed still loads.
    #[serde(default)]
    tools: std::collections::BTreeMap<String, u64>,
}

/// `session_id` comes straight from the hook's stdin JSON. It becomes a file name, so only a
/// plain name is accepted: a `../` or an absolute value would escape the state dir.
fn safe_session(session_id: Option<&str>) -> Option<&str> {
    let id = session_id?;
    if crate::paths::is_valid_short_name(id) {
        return Some(id);
    }
    crate::agent_notice::record_logged(
        "llmenv: the hook input has a session id that is not a plain file name, so its slippage stats are not kept. Fix: report the engine session id format to the llmenv maintainers.",
    );
    None
}

pub(super) fn stats_path(state_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    state_dir
        .join("slippage")
        .join(format!("{session_id}.json"))
}

/// Lock for one read-change-save of the session's stats file (#2678). When the lock fails,
/// `lock_state_file` records a notice and the update runs unlocked, as it did before the lock existed.
fn stats_lock(state_dir: &std::path::Path, session_id: &str) -> Option<std::fs::File> {
    stats_path(state_dir, session_id)
        .parent()
        .and_then(|dir| super::session_state::lock_state_file(dir, session_id, "slippage stats"))
}

mod stats_log;
use stats_log::read_stats;
pub(super) use stats_log::{load_stats, load_stats_for_rewrite};

/// Record a read, or decide a write. Returns the deny text, or empty to allow.
///
/// The layer targets `Write`, not `Edit`: Claude Code already refuses an
/// `Edit` to a file it hasn't read this session. `Write` has no such guard,
/// and it's the destructive one — it replaces the whole file. The gap this
/// closes is a `Write` that clobbers a file whose contents the agent never
/// saw, which is most likely right after a compaction dropped the read.
///
/// A path that doesn't exist yet is always allowed: creating a new file
/// without reading it first is not a mistake, and denying it would make the
/// layer refuse ordinary work.
pub(crate) fn handle_pre_tool_use(
    cfg: Option<&SlippageControl>,
    payload: &serde_json::Value,
    session_id: Option<&str>,
    state_dir: &std::path::Path,
) -> String {
    let Some(cfg) = cfg else {
        return String::new();
    };
    if !cfg.enabled || !cfg.read_before_edit {
        return String::new();
    }
    let Some(session_id) = safe_session(session_id) else {
        // Without a session id there is nothing to scope the log to, and a
        // global one would leak across concurrent sessions.
        return String::new();
    };
    let tool = payload.get("tool_name").and_then(serde_json::Value::as_str);
    let path = payload
        .get("tool_input")
        .and_then(|v| v.get("file_path"))
        .and_then(serde_json::Value::as_str);
    let (Some(tool), Some(path)) = (tool, path) else {
        return String::new();
    };

    match tool {
        "Read" => {
            // Held for the read-change-save of the stats file (#2678). A Write only reads it.
            let lock = stats_lock(state_dir, session_id);
            let Some(mut stats) = load_stats_for_rewrite(state_dir, session_id, lock.is_some())
            else {
                return String::new();
            };
            stats.paths.insert(path_key(path));
            save_stats(state_dir, session_id, &stats);
            String::new()
        }
        "Write" => {
            // Only a path that is absent is new. `exists()` would also allow a path it cannot
            // stat, so every other error falls through to the read check.
            if matches!(
                std::fs::symlink_metadata(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound
            ) {
                return String::new();
            }
            match read_stats(state_dir, session_id) {
                Ok(stats) if stats.paths.contains(&path_key(path)) => String::new(),
                Ok(_) => format!(
                    "__DENY__:`{path}` already exists and you haven't read it in this session.                      Write replaces the whole file, so read it first and confirm what you'd be                      discarding — this fires most often after a compaction dropped the earlier read."
                ),
                // The stats log is the cause, so the reason names it and not the file to write.
                Err(problem) => {
                    format!("__DENY__:llmenv cannot confirm that you read `{path}`. {problem}")
                }
            }
        }
        _ => String::new(),
    }
}

/// The key a path is recorded and looked up under.
///
/// `canonicalize` where possible so a read of `./src/x.rs` and a write of
/// `/repo/src/x.rs` are the same entry — otherwise the guard would deny a
/// write to a file that *was* read, which is the false positive most likely to
/// get the layer switched off. Falls back to the literal path when the file
/// can't be resolved (it may not exist yet, which is the allowed case anyway).
pub(super) fn path_key(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Persist `stats` for `session_id`, best-effort.
///
/// Opportunistically drops session files older than a week, the way
/// `read_once`'s cache does — without it the directory grows one file per
/// session forever.
pub(super) fn save_stats(state_dir: &std::path::Path, session_id: &str, stats: &SessionStats) {
    super::session_state::prune_stale_json_files(&state_dir.join("slippage"), 7);
    super::session_state::prune_orphan_locks(&state_dir.join("slippage"), 7);
    let Ok(json) = serde_json::to_string(stats) else {
        crate::agent_notice::record_logged(format!(
            "llmenv: cannot encode the slippage stats of session {session_id}. A later Write to a file you read may be denied. Fix: file a bug with this message."
        ));
        return;
    };
    if let Err(e) =
        crate::paths::write_owner_only_atomic(&stats_path(state_dir, session_id), json.as_bytes())
    {
        // A lost read record makes the next Write to that file deny, so the failure must be visible.
        crate::agent_notice::record_logged(format!(
            "llmenv: cannot save the slippage stats ({e}). A later Write to a file you read may be denied. Fix: check the permissions of the slippage state folder."
        ));
    }
}

/// Count a completed tool call (#317, `metrics`).
///
/// The ratio of reads to writes is the signal: a session that stops reading
/// and keeps editing is the shape of an agent working from memory of a
/// codebase rather than the codebase.
pub(crate) fn handle_post_tool_use(
    cfg: Option<&SlippageControl>,
    payload: &serde_json::Value,
    session_id: Option<&str>,
    state_dir: &std::path::Path,
) {
    let Some(cfg) = cfg else { return };
    if !cfg.enabled {
        return;
    }
    let (Some(session_id), Some(tool)) = (
        safe_session(session_id),
        payload.get("tool_name").and_then(serde_json::Value::as_str),
    ) else {
        return;
    };
    let records_bash_read =
        cfg.read_before_edit && crate::hook_run::shell_tool::is_shell_tool(tool);
    if !records_bash_read && !cfg.metrics {
        return;
    }
    // One lock covers both read-change-save cycles of the stats file (#2678).
    let lock = stats_lock(state_dir, session_id);
    if records_bash_read {
        super::slippage_read::record_bash_read(state_dir, session_id, payload, lock.is_some());
    }
    if !cfg.metrics {
        return;
    }
    let Some(mut stats) = load_stats_for_rewrite(state_dir, session_id, lock.is_some()) else {
        return;
    };
    *stats.tools.entry(tool.to_string()).or_default() += 1;
    save_stats(state_dir, session_id, &stats);
}

/// The session's tool-use summary, for storing to memory at `SessionEnd`, or
/// `None` when the layer is off or nothing was counted.
///
/// Returned as text for the caller to fold into the chunk it already stores,
/// rather than issuing a second `icm_memory_store` — one store per session
/// end, not two.
pub(crate) fn session_metrics_summary(
    cfg: Option<&SlippageControl>,
    session_id: Option<&str>,
    state_dir: &std::path::Path,
) -> Option<String> {
    let cfg = cfg?;
    if !cfg.enabled || !cfg.metrics {
        return None;
    }
    let stats = load_stats(state_dir, safe_session(session_id)?);
    if stats.tools.is_empty() {
        return None;
    }
    let total: u64 = stats.tools.values().sum();
    let reads = stats.tools.get("Read").copied().unwrap_or(0);
    let edits: u64 = ["Edit", "Write", "MultiEdit"]
        .iter()
        .filter_map(|t| stats.tools.get(*t))
        .sum();
    let ratio = if edits == 0 {
        "no edits".to_string()
    } else {
        // One decimal place: the point is "roughly how much did it look
        // before it changed things", not a precise figure.
        format!("{:.1} reads per edit", reads as f64 / edits as f64)
    };
    Some(format!(
        "Session tool use: {total} calls, {reads} reads, {edits} edits ({ratio})."
    ))
}

/// Bash sub-commands treated as *modifying* for `explain_before_act`.
///
/// Read-only commands are the overwhelming majority of what an agent runs, and
/// demanding a narration before every `ls` would train the user to switch the
/// layer off. The list is deliberately short and about consequence, not risk
/// appetite: each entry either changes files, changes history, or leaves the
/// machine.
const MODIFYING_COMMANDS: &[&str] = &[
    "rm",
    "mv",
    "cp",
    "dd",
    "chmod",
    "chown",
    "truncate",
    "kill",
    "pkill",
    "shutdown",
    "reboot",
    "mkfs",
    "docker",
    "kubectl",
    "terraform",
    "systemctl",
];

/// Whether any part of `command` runs a modifying command. The parts come from the commit gate's
/// splitter, so a write inside a substitution, a subshell, or a brace group counts (#2558). A
/// command nested too deeply to split counts as modifying, so the check fails closed.
fn is_modifying(command: &str) -> bool {
    match shell_segments(command) {
        Ok(segments) => segments.iter().any(|words| is_modifying_segment(words)),
        Err(NestingTooDeep) => true,
    }
}

/// Whether one segment runs a modifying command. A wrapper such as `env`, `sudo`, or `nohup` does
/// not hide the command, and the commands that a shell runs are their own segments.
fn is_modifying_segment(words: &[String]) -> bool {
    let words = program_words(words);
    let Some((head, tail)) = words.split_first() else {
        return false;
    };
    let head = program(head);
    if head == "git" {
        // Only the ones that leave the machine or rewrite history.
        return git_subcommand(tail)
            .is_some_and(|sub| matches!(sub, "push" | "reset" | "rebase" | "clean"));
    }
    MODIFYING_COMMANDS.contains(&head)
}

/// The subcommand of a `git` segment, after its global options such as `-C dir` and `-c key=val`.
fn git_subcommand<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut iter = args.iter();
    while let Some(word) = iter.next() {
        match *word {
            "-C" | "-c" | "--git-dir" | "--work-tree" => {
                iter.next();
            }
            w if w.starts_with('-') => {}
            w => return Some(w),
        }
    }
    None
}

/// The transcript-scan layers (#317, phase 3). Both default off.
///
/// Returns deny text, or empty to allow. Fails open at every step: no
/// transcript path, an unreadable transcript, or an unrecognised payload all
/// mean "allow". These are heuristics over a format llmenv doesn't own, and a
/// heuristic that blocks work when its input is missing is worse than the
/// slippage it watches for — which is also why both ship off by default.
pub(crate) fn handle_transcript_scan(
    cfg: Option<&SlippageControl>,
    payload: &serde_json::Value,
) -> String {
    let Some(cfg) = cfg else {
        return String::new();
    };
    if !cfg.enabled || (!cfg.answer_before_act && !cfg.explain_before_act) {
        return String::new();
    }
    let Some(path) = payload
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
    else {
        return String::new();
    };
    let Some(state) = crate::hook_run::transcript::read_turn_state(std::path::Path::new(path))
    else {
        return String::new();
    };

    if cfg.answer_before_act && state.has_unanswered_question() {
        return "__DENY__:the user asked a question that you haven't answered yet. Answer it in \
                text first — if the answer needs this tool call, say what you're checking and \
                why before running it."
            .to_string();
    }

    if cfg.explain_before_act && !state.assistant_spoke_since {
        let command = payload
            .get("tool_input")
            .and_then(|v| v.get("command"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if shell_segments(command).is_err() {
            return format!("__DENY__:{}", nesting_reason());
        }
        if is_modifying(command) {
            return "__DENY__:this command changes something and you haven't said what you're \
                    doing yet. Explain the change and why in text first, then run it."
                .to_string();
        }
    }

    String::new()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod notice_tests;
