//! Session logging: a single `SessionLogEvent` stream that fans out to two
//! independent sinks — a local JSONL file and ICM's transcript store via the
//! ICM MCP. See `docs/superpowers/specs/2026-06-30-icm-transcript-session-logging-design.md`.

pub mod dispatch;
pub mod event;
pub mod file_sink;
pub mod scope_header;
pub mod state;
pub mod tracing_layer;
pub mod transcript;

pub use event::{EventKind, EventScope, SessionLogEvent, now_rfc3339};
pub use file_sink::{FileSink, default_file_path, default_file_path_string};
pub use scope_header::{ScopeContext, scope_header_content, scope_metadata_json};
pub use tracing_layer::FileLogLayer;
