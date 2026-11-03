use serde::{Deserialize, Serialize};

/// In-engine task tracker (#231): a file-based task store with CLI commands,
/// injected context, and lifecycle-hook ordering enforcement. Off by default
/// — disabled means zero materialized-output change and zero hook cost.
///
/// `Default` is hand-written, not derived: `block_engine_task_tools` defaults
/// to `true`, so a blind `#[derive(Default)]` (which would give `false`) would
/// silently disagree with what an absent/partial config actually resolves to.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskTracker {
    /// Whether the task tracker's CLAUDE.md fragment and lifecycle hooks are
    /// active. The `llmenv task` CLI subcommands work regardless of this flag
    /// — it only gates the injected-context and hook-reminder side effects.
    #[serde(default)]
    pub enabled: bool,
    /// Whether enabling the tracker also auto-injects a `PreToolUse` block
    /// hook on the engine's own built-in task tools (Claude Code's
    /// `TaskCreate`/`TaskList`/`TaskUpdate`), redirecting them to `llmenv
    /// task` (#980, #985). Default `true`. Set `false` to keep the tracker's
    /// CLAUDE.md fragment and reminders while still allowing the native tools
    /// through — e.g. for genuine multi-agent teammate coordination that
    /// isn't solo step tracking.
    #[serde(default = "default_block_engine_task_tools")]
    pub block_engine_task_tools: bool,
    /// Whether the tracker nudges the agent while it works (#2456): the reminder after a
    /// workflow skill starts, the nudge after several tool calls with no task, and the reminder
    /// to park a task when the agent asks the user a question. Default `true`.
    #[serde(default = "default_block_engine_task_tools")]
    pub nudges: bool,
    /// Whether the first `git commit` or `gh pr create` of a session with no task in progress is
    /// denied once, with the `llmenv task` commands to run first (#2456). Default `true`.
    #[serde(default = "default_block_engine_task_tools")]
    pub enforce_commit: bool,
    /// Skills that start a multi-step workflow and trigger the one-time reminder. Absent means
    /// `dev-sprint`, `ship-issue`, `pre-pr-review`, `executing-plans`, and `writing-plans`. A
    /// plugin prefix (`nbl-dev:ship-issue`) is ignored when matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_skills: Option<Vec<String>>,
    /// Mutating tool calls with no task before the first nudge. Absent means 8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nudge_after: Option<u32>,
    /// Mutating tool calls between later nudges. Absent means 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nudge_every: Option<u32>,
    /// Turn ends with a task in progress and tasks queued behind it, between two Stop reminders
    /// of that state (#3318). Absent means 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_nudge_every: Option<u32>,
}

fn default_block_engine_task_tools() -> bool {
    true
}

impl Default for TaskTracker {
    fn default() -> Self {
        Self {
            enabled: false,
            block_engine_task_tools: default_block_engine_task_tools(),
            nudges: default_block_engine_task_tools(),
            enforce_commit: default_block_engine_task_tools(),
            workflow_skills: None,
            nudge_after: None,
            nudge_every: None,
            stop_nudge_every: None,
        }
    }
}
