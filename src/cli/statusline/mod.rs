//! `llmenv statusline` — first-class statusline renderer. See
//! `docs/superpowers/specs/2026-07-15-statusline-design.md`.

mod data;
mod template;

#[expect(
    unused_imports,
    reason = "consumed by statusline widget rendering and orchestrator, wired up in a follow-up task"
)]
pub use data::StatusData;
#[expect(
    unused_imports,
    reason = "consumed by statusline orchestrator, wired up in a follow-up task"
)]
pub use template::{TemplateToken, parse_template};
