//! `llmenv-status.json` — llmenv-sourced stats consumed by the statusline
//! renderer. Pure parsing only: no scope resolution, no MCP calls, no
//! business logic. All fields written once at data-file-write time by
//! `src/materialize/status_data.rs`, which is also where these types are
//! now defined.

pub use crate::materialize::status_data::{ScopesData, StatusData};
