pub use llmenv_paths::{
    config_dir, config_path, cwd_under_prefix, expand_tilde, has_parent_component,
    is_unsafe_join_target, is_valid_short_name, read_dir_optional, state_dir, write_owner_only,
    write_owner_only_atomic,
};

/// File name used for SessionEnd dedup across hook run and memory CLI.
/// Shared between `hook_run` and `memory` modules — must not drift.
pub const HOOK_STORE_CHUNK: &str = "hook_store_chunk";
