//! `llmenv statusline` — first-class statusline renderer. See
//! `docs/superpowers/specs/2026-07-15-statusline-design.md`.

mod data;
mod icons;
mod llmenv_widget;
mod template;
mod widget;

use crate::cli::style::{apply_style, truncate_ellipsis};
use std::io::Read;

pub use data::StatusData;
pub use icons::resolve_icons;
pub use llmenv_widget::render_llmenv_widget;
pub use template::{TemplateToken, parse_template};
pub use widget::{EngineData, render_engine_widget};

const DEFAULT_ROW: &str = "{model} │ {folder} │ {branch} │ {context_pct} │ {budget}";

/// Apply per-widget truncation + style. Shared by every widget render path
/// (engine-sourced in `widget.rs`, llmenv-sourced in `llmenv_widget.rs`) —
/// hoisted here so the two modules don't each carry a byte-for-byte-identical
/// private copy.
pub(super) fn finish(
    raw: String,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    let truncated = match cfg.and_then(|c| c.max_len) {
        Some(max) => truncate_ellipsis(&raw, max),
        None => raw,
    };
    match cfg.and_then(|c| c.style.as_deref()) {
        Some(style) => apply_style(&truncated, style, use_color),
        None => truncated,
    }
}

/// Full render pipeline: stdin (engine JSON) + data file (llmenv stats) +
/// config (`statusline:` section) → ANSI rows, one `\n`-terminated line per
/// configured row. Never returns `Err` for "no data" conditions (missing data
/// file, malformed stdin, unknown widget names) — only for a genuine I/O
/// failure reading stdin itself. See the design doc's "Renderer contract".
///
/// # Errors
///
/// Returns an error if `stdin` cannot be read (not for malformed JSON on it,
/// which degrades to an empty [`EngineData`] instead).
pub fn run_statusline(
    config: &llmenv_config::Config,
    data_path: &std::path::Path,
    stdin: &mut impl Read,
    use_color: bool,
) -> anyhow::Result<String> {
    let mut stdin_buf = String::new();
    stdin.read_to_string(&mut stdin_buf)?;
    let engine_data: EngineData = serde_json::from_str(&stdin_buf).unwrap_or_default();
    let status_data = StatusData::load(data_path);

    let cfg = config.statusline.clone().unwrap_or_default();
    let rows: Vec<String> = if cfg.rows.is_empty() {
        vec![DEFAULT_ROW.to_string()]
    } else {
        cfg.rows.clone()
    };
    let icons = resolve_icons(cfg.style.icon_set, &cfg.icons);

    let mut out = String::new();
    for row in &rows {
        let tokens = parse_template(row);
        let mut rendered_any = false;
        let mut line = String::new();
        for token in tokens {
            match token {
                TemplateToken::Literal(text) => line.push_str(&text),
                // `truncate` (the `{name:t}` shorthand) is deliberately
                // unused here: per the design doc it's "redundant with
                // max_len on the widget definition" — truncation already
                // applies whenever the widget's config sets `max_len`,
                // regardless of this flag.
                TemplateToken::Widget { name, truncate: _ } => {
                    let widget_cfg = cfg.widgets.get(&name);
                    let value = render_engine_widget(&name, &engine_data, widget_cfg, use_color)
                        .or_else(|| {
                            render_llmenv_widget(&name, &status_data, widget_cfg, &icons, use_color)
                        })
                        .unwrap_or_default();
                    if !value.is_empty() {
                        rendered_any = true;
                    }
                    line.push_str(&value);
                }
            }
        }
        // No orphaned separators: a row whose only content is literal
        // separator text (all widgets empty) still needs *some* output per
        // the design doc, but must not display bare separators with no
        // data. Render an empty line for that row instead of the
        // separator-only text.
        if rendered_any {
            out.push_str(&line);
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use llmenv_config::StatuslineConfig;

    #[test]
    fn renders_default_single_row_when_config_absent() {
        let config = llmenv_config::Config::default();
        let stdin = br#"{"model": {"display_name": "Claude Opus 4.8"}}"#;
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json"); // missing file
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false).unwrap();
        assert!(out.contains("Claude Opus 4.8"));
        assert!(out.contains(" │ "));
    }

    #[test]
    fn renders_configured_rows() {
        let config = llmenv_config::Config {
            statusline: Some(StatuslineConfig {
                rows: vec!["{model}".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let stdin = br#"{"model": {"display_name": "GPT-Z"}}"#;
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json");
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false).unwrap();
        assert_eq!(out.trim_end(), "GPT-Z");
    }

    #[test]
    fn missing_data_file_still_renders_engine_widgets() {
        let config = llmenv_config::Config {
            statusline: Some(StatuslineConfig {
                rows: vec!["{model} {plugins}".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let stdin = br#"{"model": {"display_name": "GPT-Z"}}"#;
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("does-not-exist.json");
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false).unwrap();
        assert!(out.contains("GPT-Z"));
    }

    #[test]
    fn malformed_stdin_renders_engine_widgets_empty_not_error() {
        let config = llmenv_config::Config::default();
        let stdin = b"not json";
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json");
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false);
        assert!(out.is_ok(), "malformed stdin must degrade, not error");
    }

    #[test]
    fn all_widgets_empty_yields_empty_row() {
        let config = llmenv_config::Config {
            statusline: Some(StatuslineConfig {
                rows: vec!["{model}".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let stdin = b"{}";
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json");
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false).unwrap();
        assert_eq!(out, "\n");
    }

    #[test]
    fn unknown_widget_name_in_template_renders_empty() {
        let config = llmenv_config::Config {
            statusline: Some(StatuslineConfig {
                rows: vec!["{bogus_widget}".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let out = run_statusline(
            &config,
            std::path::Path::new("/nonexistent"),
            &mut &b""[..],
            false,
        )
        .unwrap();
        assert_eq!(out, "\n");
    }
}
