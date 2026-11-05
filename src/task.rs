pub use llmenv_task::{
    Completed, DisplayRow, NewTask, ParentSpec, Placement, SessionChoice, Task, TaskEdit,
    TaskState, Tracking, add_task, add_task_for_session, add_task_with, block_task, complete_task,
    complete_task_by, current_wip_title, delete_task, display_rows, edit_task, filter_by_state,
    filter_tasks_for_project, last_task_hint, list_tasks, load_task, note_task, render_task_list,
    reopen_task, reopen_tasks, resolve_current_task, resolve_identifier, resolve_next_task,
    session_start_reminder, start_task, stop_hook_reminder, tasks_dir, tracking, try_list_tasks,
    wait_task,
};

pub mod core_text {
    pub use llmenv_task::core_text::core_instruction_text;
}

pub mod ownership {
    pub use llmenv_task::ownership::{
        Caller, OverrideNote, caller_session, ensure_session_is_ours, ensure_task_is_ours,
    };
}

pub mod project {
    pub use llmenv_task::project::current_tag;
}

pub mod resume {
    pub use llmenv_task::resume::{MISSING_CONTEXT_NUDGE, ResumeContext, git_branch};
}

pub mod visibility {
    pub use llmenv_task::visibility::owner_visible;
}

pub mod session {
    pub use llmenv_task::session::{
        EngineIdentity, PickError, Session, SessionSummary, SessionSummaryTask, StartDecision,
        StartOutcome, StartRequest, delete_tasks_in_session, finish_session, idle_display,
        list_sessions_strict, load_existing_session, open_sessions_strict, pick_open_session,
        session_summary, session_summary_with_agent, start_session, start_session_as,
        touch_last_activity, try_session_progress, update_resume,
    };
}
