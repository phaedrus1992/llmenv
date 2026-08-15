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
struct SessionStats {
    /// Paths read this session (`read_before_edit`).
    #[serde(default)]
    paths: std::collections::BTreeSet<String>,
    /// Tool-call counts by name (`metrics`). `#[serde(default)]` on both
    /// fields so a log written before either layer existed still loads.
    #[serde(default)]
    tools: std::collections::BTreeMap<String, u64>,
}

fn stats_path(state_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    state_dir
        .join("slippage")
        .join(format!("{session_id}.json"))
}

fn load_stats(state_dir: &std::path::Path, session_id: &str) -> SessionStats {
    // Fail-soft throughout: an unreadable or corrupt log means "nothing known
    // to have been read", which allows the write. The alternative — denying on
    // a corrupt state file — would wedge the agent over a bookkeeping error.
    std::fs::read_to_string(stats_path(state_dir, session_id))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

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
    let Some(session_id) = session_id else {
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
            let mut stats = load_stats(state_dir, session_id);
            stats.paths.insert(path_key(path));
            save_stats(state_dir, session_id, &stats);
            String::new()
        }
        "Write" => {
            if !std::path::Path::new(path).exists() {
                return String::new();
            }
            if load_stats(state_dir, session_id)
                .paths
                .contains(&path_key(path))
            {
                return String::new();
            }
            format!(
                "__DENY__:`{path}` already exists and you haven't read it in this session.                  Write replaces the whole file, so read it first and confirm what you'd be                  discarding — this fires most often after a compaction dropped the earlier read."
            )
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
fn path_key(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Persist `stats` for `session_id`, best-effort.
///
/// Opportunistically drops session files older than a week, the way
/// `read_once`'s cache does — without it the directory grows one file per
/// session forever.
fn save_stats(state_dir: &std::path::Path, session_id: &str, stats: &SessionStats) {
    super::session_state::prune_stale_json_files(&state_dir.join("slippage"), 7);
    let Ok(json) = serde_json::to_string(stats) else {
        return;
    };
    if let Err(e) =
        crate::paths::write_owner_only_atomic(&stats_path(state_dir, session_id), json.as_bytes())
    {
        tracing::debug!("slippage: could not record session stats: {e}");
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
    if !cfg.enabled || !cfg.metrics {
        return;
    }
    let (Some(session_id), Some(tool)) = (
        session_id,
        payload.get("tool_name").and_then(serde_json::Value::as_str),
    ) else {
        return;
    };
    let mut stats = load_stats(state_dir, session_id);
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
    let stats = load_stats(state_dir, session_id?);
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

/// Whether `command` starts a modifying operation, checking each top-level
/// segment so a read-only head doesn't wave through what follows it.
fn is_modifying(command: &str) -> bool {
    command
        .split(['\n', ';', '|'])
        .flat_map(|seg| seg.split("&&"))
        .flat_map(|seg| seg.split("||"))
        .any(|segment| {
            let mut words = segment.split_whitespace().skip_while(|w| w.contains('='));
            let head = words.next().unwrap_or_default();
            let head = head.rsplit('/').next().unwrap_or(head);
            if head == "sudo" {
                return words
                    .next()
                    .is_some_and(|next| MODIFYING_COMMANDS.contains(&next));
            }
            if head == "git" {
                // Only the ones that leave the machine or rewrite history.
                return words
                    .next()
                    .is_some_and(|sub| matches!(sub, "push" | "reset" | "rebase" | "clean"));
            }
            MODIFYING_COMMANDS.contains(&head)
        })
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
        let modifying = payload
            .get("tool_input")
            .and_then(|v| v.get("command"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_modifying);
        if modifying {
            return "__DENY__:this command changes something and you haven't said what you're \
                    doing yet. Explain the change and why in text first, then run it."
                .to_string();
        }
    }

    String::new()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, self_critique: bool) -> SlippageControl {
        SlippageControl {
            enabled,
            self_critique,
            ..Default::default()
        }
    }

    #[test]
    fn emits_the_checklist_when_the_layer_is_on() {
        let out = handle_stop(Some(&cfg(true, true)));
        assert!(out.contains("tests"), "{out}");
        assert!(
            !out.starts_with("__DENY__:"),
            "the layer is advisory, never blocking"
        );
    }

    // The master switch has to win over the per-layer default, or "turn this
    // feature off" wouldn't actually turn it off — `self_critique` defaults to
    // true, so an inert `enabled: false` is the only thing standing between a
    // user who disabled the feature and a checklist on every turn.
    #[test]
    fn master_switch_off_silences_a_layer_that_is_on() {
        assert_eq!(handle_stop(Some(&cfg(false, true))), "");
    }

    #[test]
    fn layer_off_silences_it_while_the_feature_stays_on() {
        assert_eq!(handle_stop(Some(&cfg(true, false))), "");
    }

    #[test]
    fn absent_config_is_silent() {
        assert_eq!(handle_stop(None), "");
    }

    #[test]
    fn default_config_is_silent_because_the_feature_is_opt_in() {
        assert_eq!(handle_stop(Some(&SlippageControl::default())), "");
    }
    #[test]
    fn turn_digest_follows_its_own_toggle_and_the_master_switch() {
        let on = SlippageControl {
            enabled: true,
            rule_reinjection: true,
            ..Default::default()
        };
        assert!(handle_turn(Some(&on)).contains("whole task"));

        let layer_off = SlippageControl {
            rule_reinjection: false,
            ..on.clone()
        };
        assert_eq!(handle_turn(Some(&layer_off)), "");

        let master_off = SlippageControl {
            enabled: false,
            ..on
        };
        assert_eq!(handle_turn(Some(&master_off)), "");
        assert_eq!(handle_turn(None), "");
        assert_eq!(handle_turn(Some(&SlippageControl::default())), "");
    }

    // The digest is re-sent on every turn, so its size is a recurring cost
    // rather than a one-off. Keeping it short is the design, and a cap makes
    // that explicit instead of leaving it to whoever edits the string next.
    #[test]
    fn turn_digest_stays_small_enough_to_repeat_every_turn() {
        let text = handle_turn(Some(&SlippageControl {
            enabled: true,
            rule_reinjection: true,
            ..Default::default()
        }));
        assert!(
            text.len() < 600,
            "the per-turn digest is {} bytes; it is sent on every turn",
            text.len()
        );
    }
    fn on() -> SlippageControl {
        SlippageControl {
            enabled: true,
            read_before_edit: true,
            ..Default::default()
        }
    }

    fn payload(tool: &str, path: &std::path::Path) -> serde_json::Value {
        serde_json::json!({
            "tool_name": tool,
            "tool_input": { "file_path": path.to_str().unwrap() },
        })
    }

    #[test]
    fn denies_writing_over_an_existing_file_that_was_never_read() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("existing.txt");
        std::fs::write(&file, "important").unwrap();

        let out = handle_pre_tool_use(
            Some(&on()),
            &payload("Write", &file),
            Some("s1"),
            state.path(),
        );
        assert!(out.starts_with("__DENY__:"), "{out}");
        assert!(out.contains("existing.txt"), "{out}");
    }

    #[test]
    fn allows_the_write_once_the_file_has_been_read() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("existing.txt");
        std::fs::write(&file, "important").unwrap();

        handle_pre_tool_use(
            Some(&on()),
            &payload("Read", &file),
            Some("s1"),
            state.path(),
        );
        assert_eq!(
            handle_pre_tool_use(
                Some(&on()),
                &payload("Write", &file),
                Some("s1"),
                state.path()
            ),
            ""
        );
    }

    // Creating a file is not a mistake. Denying it would make the layer refuse
    // ordinary work, which is how a guard gets switched off entirely.
    #[test]
    fn allows_writing_a_file_that_does_not_exist_yet() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        assert_eq!(
            handle_pre_tool_use(
                Some(&on()),
                &payload("Write", &work.path().join("new.txt")),
                Some("s1"),
                state.path()
            ),
            ""
        );
    }

    // The log is per session, so one session's read must not vouch for
    // another's write — sessions run concurrently.
    #[test]
    fn a_read_in_one_session_does_not_authorise_a_write_in_another() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("shared.txt");
        std::fs::write(&file, "x").unwrap();

        handle_pre_tool_use(
            Some(&on()),
            &payload("Read", &file),
            Some("a"),
            state.path(),
        );
        assert!(
            handle_pre_tool_use(
                Some(&on()),
                &payload("Write", &file),
                Some("b"),
                state.path()
            )
            .starts_with("__DENY__:")
        );
    }

    #[test]
    fn edit_is_left_to_claude_codes_own_guard() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("existing.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            handle_pre_tool_use(
                Some(&on()),
                &payload("Edit", &file),
                Some("s1"),
                state.path()
            ),
            "",
            "Edit already requires a prior read; duplicating that only adds false denials"
        );
    }

    #[test]
    fn a_corrupt_log_allows_the_write_rather_than_wedging_the_agent() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        std::fs::create_dir_all(state.path().join("slippage")).unwrap();
        std::fs::write(state.path().join("slippage/s1.json"), b"{not json").unwrap();

        // Fail-soft means the deny still fires (nothing is known to have been
        // read) but nothing panics and the file is rewritten cleanly on the
        // next Read.
        let out = handle_pre_tool_use(
            Some(&on()),
            &payload("Write", &file),
            Some("s1"),
            state.path(),
        );
        assert!(out.starts_with("__DENY__:"), "{out}");
        handle_pre_tool_use(
            Some(&on()),
            &payload("Read", &file),
            Some("s1"),
            state.path(),
        );
        assert_eq!(
            handle_pre_tool_use(
                Some(&on()),
                &payload("Write", &file),
                Some("s1"),
                state.path()
            ),
            "",
            "a corrupt log must not be permanently poisoned"
        );
    }

    #[test]
    fn write_guard_follows_the_master_switch_and_its_own_toggle() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let p = payload("Write", &file);

        let master_off = SlippageControl {
            enabled: false,
            ..on()
        };
        assert_eq!(
            handle_pre_tool_use(Some(&master_off), &p, Some("s1"), state.path()),
            ""
        );
        let layer_off = SlippageControl {
            read_before_edit: false,
            ..on()
        };
        assert_eq!(
            handle_pre_tool_use(Some(&layer_off), &p, Some("s1"), state.path()),
            ""
        );
        assert_eq!(handle_pre_tool_use(None, &p, Some("s1"), state.path()), "");
        // No session id: nothing to scope the log to, so the layer stands down
        // rather than sharing one log across concurrent sessions.
        assert_eq!(handle_pre_tool_use(Some(&on()), &p, None, state.path()), "");
    }
    #[test]
    fn metrics_summarise_the_read_to_edit_ratio() {
        let state = tempfile::tempdir().unwrap();
        let cfg = SlippageControl {
            enabled: true,
            metrics: true,
            ..Default::default()
        };
        for tool in ["Read", "Read", "Read", "Read", "Edit", "Write"] {
            handle_post_tool_use(
                Some(&cfg),
                &serde_json::json!({ "tool_name": tool }),
                Some("s1"),
                state.path(),
            );
        }
        let summary = session_metrics_summary(Some(&cfg), Some("s1"), state.path()).unwrap();
        assert!(summary.contains("6 calls"), "{summary}");
        assert!(summary.contains("4 reads"), "{summary}");
        assert!(summary.contains("2 edits"), "{summary}");
        assert!(summary.contains("2.0 reads per edit"), "{summary}");
    }

    #[test]
    fn metrics_report_no_edits_rather_than_dividing_by_zero() {
        let state = tempfile::tempdir().unwrap();
        let cfg = SlippageControl {
            enabled: true,
            metrics: true,
            ..Default::default()
        };
        handle_post_tool_use(
            Some(&cfg),
            &serde_json::json!({ "tool_name": "Read" }),
            Some("s1"),
            state.path(),
        );
        let summary = session_metrics_summary(Some(&cfg), Some("s1"), state.path()).unwrap();
        assert!(summary.contains("no edits"), "{summary}");
    }

    #[test]
    fn metrics_are_silent_with_nothing_counted_or_the_layer_off() {
        let state = tempfile::tempdir().unwrap();
        let on = SlippageControl {
            enabled: true,
            metrics: true,
            ..Default::default()
        };
        assert!(session_metrics_summary(Some(&on), Some("empty"), state.path()).is_none());
        assert!(session_metrics_summary(None, Some("s1"), state.path()).is_none());
        assert!(session_metrics_summary(Some(&on), None, state.path()).is_none());

        let off = SlippageControl {
            metrics: false,
            ..on
        };
        handle_post_tool_use(
            Some(&off),
            &serde_json::json!({ "tool_name": "Read" }),
            Some("s2"),
            state.path(),
        );
        assert!(session_metrics_summary(Some(&off), Some("s2"), state.path()).is_none());
    }

    // The two layers share one file, so counting must not discard the read
    // log and vice versa.
    #[test]
    fn metrics_and_the_read_log_coexist_in_one_state_file() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let cfg = SlippageControl {
            enabled: true,
            metrics: true,
            read_before_edit: true,
            ..Default::default()
        };

        handle_pre_tool_use(
            Some(&cfg),
            &payload("Read", &file),
            Some("s1"),
            state.path(),
        );
        handle_post_tool_use(
            Some(&cfg),
            &serde_json::json!({ "tool_name": "Read" }),
            Some("s1"),
            state.path(),
        );

        assert!(
            session_metrics_summary(Some(&cfg), Some("s1"), state.path()).is_some(),
            "the count survived the read being recorded"
        );
        assert_eq!(
            handle_pre_tool_use(
                Some(&cfg),
                &payload("Write", &file),
                Some("s1"),
                state.path()
            ),
            "",
            "the read record survived the count being written"
        );
    }
    fn scan_cfg(answer: bool, explain: bool) -> SlippageControl {
        SlippageControl {
            enabled: true,
            answer_before_act: answer,
            explain_before_act: explain,
            ..Default::default()
        }
    }

    fn transcript_with(lines: &[serde_json::Value]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(file.path(), body).unwrap();
        file
    }

    fn user_line(text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        })
    }

    fn assistant_line(text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] },
        })
    }

    fn bash_payload(file: &tempfile::NamedTempFile, command: &str) -> serde_json::Value {
        serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": command },
            "transcript_path": file.path().to_str().unwrap(),
        })
    }

    #[test]
    fn answer_before_act_denies_while_a_question_is_open() {
        let file = transcript_with(&[user_line("should we roll this back?")]);
        let out = handle_transcript_scan(
            Some(&scan_cfg(true, false)),
            &bash_payload(&file, "git status"),
        );
        assert!(out.starts_with("__DENY__:"), "{out}");
        assert!(out.contains("answered"), "{out}");
    }

    #[test]
    fn answer_before_act_allows_once_the_question_is_answered() {
        let file = transcript_with(&[
            user_line("should we roll this back?"),
            assistant_line("No — the failure is unrelated."),
        ]);
        assert_eq!(
            handle_transcript_scan(
                Some(&scan_cfg(true, false)),
                &bash_payload(&file, "git status")
            ),
            ""
        );
    }

    #[test]
    fn explain_before_act_only_fires_for_modifying_commands() {
        let file = transcript_with(&[user_line("clean up the branch")]);
        assert_eq!(
            handle_transcript_scan(Some(&scan_cfg(false, true)), &bash_payload(&file, "ls -la")),
            "",
            "a read-only command needs no narration"
        );
        assert!(
            handle_transcript_scan(
                Some(&scan_cfg(false, true)),
                &bash_payload(&file, "chmod 777 build")
            )
            .starts_with("__DENY__:")
        );
    }

    #[test]
    fn modifying_detection_looks_at_every_segment_and_through_sudo_and_git() {
        assert!(
            is_modifying("ls && truncate -s 0 log"),
            "a later segment counts"
        );
        assert!(is_modifying("sudo systemctl restart nginx"));
        assert!(is_modifying("git push --force"));
        assert!(
            is_modifying("/bin/chmod 600 file"),
            "an absolute path is the same command"
        );
        assert!(
            is_modifying("FOO=1 docker run x"),
            "env prefixes don't hide it"
        );
        assert!(
            !is_modifying("git status"),
            "read-only git is not modifying"
        );
        assert!(
            !is_modifying("grep -r chmod ."),
            "a command name as an argument is not a command"
        );
    }

    #[test]
    fn transcript_layers_fail_open_without_a_usable_transcript() {
        let no_path = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "chmod 777 x" },
        });
        assert_eq!(
            handle_transcript_scan(Some(&scan_cfg(true, true)), &no_path),
            ""
        );

        let missing = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "chmod 777 x" },
            "transcript_path": "/nonexistent/llmenv/t.jsonl",
        });
        assert_eq!(
            handle_transcript_scan(Some(&scan_cfg(true, true)), &missing),
            ""
        );
    }

    #[test]
    fn transcript_layers_are_off_by_default() {
        let file = transcript_with(&[user_line("are you sure?")]);
        assert_eq!(
            handle_transcript_scan(
                Some(&SlippageControl {
                    enabled: true,
                    ..Default::default()
                }),
                &bash_payload(&file, "chmod 777 x")
            ),
            "",
            "both layers are opt-in even with the feature on"
        );
    }
    // A read recorded under one spelling and a write checked under another
    // would deny a write to a file that *was* read — the false positive most
    // likely to get the layer switched off.
    #[test]
    fn a_read_by_one_spelling_authorises_a_write_by_another() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();

        let nested = work.path().join("sub/..").join("f.txt");
        std::fs::create_dir_all(work.path().join("sub")).unwrap();

        handle_pre_tool_use(
            Some(&on()),
            &payload("Read", &nested),
            Some("s1"),
            state.path(),
        );
        assert_eq!(
            handle_pre_tool_use(
                Some(&on()),
                &payload("Write", &file),
                Some("s1"),
                state.path()
            ),
            "",
            "the same file by a different spelling must count as read"
        );
    }
}
