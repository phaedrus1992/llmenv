//! llmenv — a scope-aware environment manager for AI coding agents.
//!
//! **This library target is an implementation detail of the `llmenv` binary,
//! not a supported API.** The crate is published so the binary can be installed
//! from crates.io; the module tree exists to serve `main.rs` and the test
//! suite, and items are narrowed to the smallest visibility that satisfies
//! those two consumers. Anything reachable here can change or disappear in a
//! patch release without notice.
//!
//! `cargo hawk` enforces that narrowing (see `hawk.toml` and
//! `scripts/hawk-check.sh`). The four `crates/llmenv-*` support crates are the
//! opposite case — they are published as libraries and their public API is an
//! external boundary hawk is told not to narrow.

pub mod adapter;
pub(crate) mod agent_notice;
pub use agent_notice::record_detached_job_failure;
pub mod auth;
pub(crate) mod cache_trace;
pub mod cli;
pub mod config;
pub(crate) mod consolidation;
pub(crate) mod env_var;
pub(crate) mod export_trace;
pub mod git;
pub mod hook_run;
pub mod icm;
pub mod materialize;
mod materialize_trace;
pub mod mcp;
mod mcp_trace;
pub mod memory;
pub mod merge;
pub(crate) mod onboarding;
pub mod panic_hook;
pub(crate) mod path_lookup;
pub mod paths;
pub mod plugins;
pub(crate) mod process;
pub(crate) mod proxy_note;
pub mod scope;
pub mod session_log;
pub mod sync;
pub mod task;
#[cfg(test)]
mod test_fixtures;
#[cfg(test)]
pub(crate) mod test_log_capture;
pub(crate) mod throttle;
pub(crate) mod trace;
pub mod util;
