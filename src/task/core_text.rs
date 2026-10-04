//! The fixed task tracking statement that SessionStart injects (#2457).
//!
//! The text lives in the llmenv source, so a user who turns the tracker on with an empty personal
//! config gets the same rules. Design: docs/design/issue-2438-task-tracking-nudges.md

/// The statement of the four behaviors, with the exact commands. `redirect` adds the sentence about
/// the engine task tools, which holds only while `block_engine_task_tools` is on.
#[must_use]
pub(crate) fn core_instruction_text(redirect: bool) -> String {
    let mut text = String::from(
        "llmenv task tracking. These rules always apply.\n\
         1. Open a session when the work has more than one part: `llmenv task session start \
         <name> --task \"<step 1>\" --task \"<step 2>\"`.\n\
         2. A session with no task is an error. Add each step before you start it: `llmenv task \
         add \"<step>\"`. Use `--child-of <slug>` for the parts of a step, and `--parallel` for a \
         step that runs beside the queue.\n\
         3. Do one queued task at a time. Run `llmenv task start <slug>` when you begin it, and \
         `llmenv task done <slug>` when it is finished. A parent is done only after its \
         sub-tasks.\n\
         4. When you need the user's answer, run `llmenv task wait <slug> \"<reason>\"`. After \
         the answer, run `llmenv task start <slug>`.",
    );
    if redirect {
        text.push_str(
            "\nThe engine tools TaskCreate, TaskList, and TaskUpdate are redirected to `llmenv \
             task`, so a skill step that names them works.",
        );
    }
    text.push_str(
        "\nThis text overrides an instruction that says the task tools are blocked, or that \
         forbids `llmenv task`.",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_holds_the_four_behaviors_and_their_commands() {
        let text = core_instruction_text(true);
        for needle in [
            "llmenv task session start",
            "--task",
            "llmenv task add",
            "--child-of",
            "llmenv task start <slug>",
            "llmenv task done <slug>",
            "llmenv task wait <slug>",
            "redirected to `llmenv task`",
        ] {
            assert!(text.contains(needle), "missing {needle:?}: {text}");
        }
    }

    #[test]
    fn the_redirect_sentence_follows_the_switch() {
        assert!(!core_instruction_text(false).contains("redirected"));
        assert!(core_instruction_text(true).contains("redirected"));
    }
}
