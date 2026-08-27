//! Session logging: a single `SessionLogEvent` stream that fans out to two
//! independent sinks — a local JSONL file and ICM's transcript store via the
//! ICM MCP. See `docs/superpowers/specs/2026-06-30-icm-transcript-session-logging-design.md`.

pub(crate) mod detached;
pub(crate) mod detached_log;
pub(crate) mod dispatch;
pub mod event;
pub mod file_sink;
pub(crate) mod reaper;
pub mod scope_header;
pub(crate) mod state;
pub mod tracing_layer;
pub(crate) mod transcript;

pub(crate) use detached_log::{
    detached_child_log_path, redirect_stderr_to_bounded_log, redirect_stderr_to_detached_log,
};
pub use file_sink::{FileSink, default_file_path};
pub(crate) use reaper::reap_session_log;
pub(crate) use scope_header::{ScopeContext, scope_header_content, scope_metadata_json};
pub use tracing_layer::FileLogLayer;
