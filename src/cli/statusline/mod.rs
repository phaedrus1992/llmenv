//! `llmenv statusline` — first-class statusline renderer. See
//! `docs/superpowers/specs/2026-07-15-statusline-design.md`.

mod data;
mod icons;
mod llmenv_widget;
mod template;
mod widget;

#[expect(
    unused_imports,
    reason = "consumed by statusline widget rendering and orchestrator, wired up in a follow-up task"
)]
pub use data::StatusData;
#[expect(
    unused_imports,
    reason = "consumed by statusline orchestrator, wired up in a follow-up task"
)]
pub use icons::resolve_icons;
#[expect(
    unused_imports,
    reason = "consumed by statusline orchestrator, wired up in a follow-up task"
)]
pub use llmenv_widget::render_llmenv_widget;
#[expect(
    unused_imports,
    reason = "consumed by statusline orchestrator, wired up in a follow-up task"
)]
pub use template::{TemplateToken, parse_template};
#[expect(
    unused_imports,
    reason = "consumed by statusline orchestrator, wired up in a follow-up task"
)]
pub use widget::{EngineData, render_engine_widget};
