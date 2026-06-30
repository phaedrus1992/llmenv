//! Session logging: a single `SessionLogEvent` stream that fans out to two
//! independent sinks — a local JSONL file and ICM's transcript store via the
//! ICM MCP. See `docs/superpowers/specs/2026-06-30-icm-transcript-session-logging-design.md`.

pub mod event;

pub use event::{EventKind, EventScope, SessionLogEvent};
