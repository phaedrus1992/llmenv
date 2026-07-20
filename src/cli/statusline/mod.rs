//! `llmenv statusline` — first-class statusline renderer. See
//! `docs/superpowers/specs/2026-07-15-statusline-design.md`.

pub(crate) mod data;
mod icons;
mod llmenv_widget;
mod peak;
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

/// Strip C0 (`\x00`-`\x1F`, `\x7F`) and C1 (`\u{80}`-`\u{9F}`) control
/// characters from **untrusted free-text** (engine JSON fields, filesystem
/// paths/basenames, git branch names, PR URLs, tag/backend names). A stray
/// escape sequence (e.g. from a directory or branch name) would otherwise be
/// emitted verbatim into the user's terminal — an ANSI-injection vector.
///
/// This is applied by each widget at the point it interpolates untrusted data,
/// **not** by [`finish`], so that widgets can still emit their own *trusted*
/// escapes (OSC 8 hyperlinks, per-cell bar colors). Every widget that renders
/// external string data must route it through here.
pub(super) fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// Wrap `text` in an OSC 8 terminal hyperlink to `url`. Callers must pass a
/// URL that already passed [`valid_url`] (scheme-checked, control-char-free) —
/// the OSC 8 payload is otherwise an escape-injection vector.
pub(super) fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Whether `url` is safe to embed in an OSC 8 hyperlink: `http`/`https` only,
/// and no control characters (which would break out of the escape sequence).
pub(super) fn valid_url(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && !url.chars().any(char::is_control)
}

/// Apply per-widget truncation + style. Shared by every widget render path
/// (engine-sourced in `widget.rs`, llmenv-sourced in `llmenv_widget.rs`) —
/// hoisted here so the two modules don't each carry a byte-for-byte-identical
/// private copy.
///
/// `raw` must already have untrusted free-text sanitized by the widget (see
/// [`sanitize`]); `finish` deliberately does **not** strip control chars, so a
/// widget can emit trusted escapes (OSC 8 links, colored bars). `max_len`
/// truncation counts characters including any such escapes — set it only on
/// plain-text widgets.
pub(super) fn finish(
    name: &str,
    raw: String,
    cfg: Option<&llmenv_config::WidgetConfig>,
    dyn_style: Option<&str>,
    use_color: bool,
) -> String {
    let truncated = match cfg.and_then(|c| c.max_len) {
        Some(max) => truncate_ellipsis(&raw, max),
        None => raw,
    };
    // Style precedence: an explicit per-widget `style` wins — including
    // `""`/`"none"`, which `apply_style` renders as plain, so it's the
    // per-widget colour opt-out. Else a widget-supplied dynamic style (e.g.
    // threshold coloring). Else the widget's static default colour.
    let style = cfg
        .and_then(|c| c.style.as_deref())
        .or(dyn_style)
        .unwrap_or_else(|| default_style(name));
    apply_style(&truncated, style, use_color)
}

/// Default ANSI style per widget, so a statusline with no per-widget `style:`
/// still renders with readable colour instead of monochrome. Opt out per
/// widget with `style: none` (or any unknown/empty style) or globally with
/// `statusline.style.color: false`. Unknown widget names get no colour.
fn default_style(name: &str) -> &'static str {
    match name {
        "model" => "bold cyan",
        "folder" => "blue",
        "branch" | "icm" => "magenta",
        "pr" => "bold magenta",
        "context_pct" | "progress_bar" | "config_stale" | "usage_5h" | "usage_7d" => "yellow",
        "plugins" | "mcps" => "green",
        "peak" => "bold yellow",
        "throttle" => "bold red",
        "budget" | "tokens" | "duration" | "cache_pct" | "cache" | "scopes" | "session_log" => {
            "dim"
        }
        _ => "",
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
    // Global colour opt-out: `statusline.style.color: false` forces plain text
    // for every widget, layered on top of the runtime `--color`/TTY gate.
    let use_color = use_color && cfg.style.color;
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
        assert!(out.contains("Opus"));
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
    fn renders_llmenv_widgets_from_real_data_file() {
        let config = llmenv_config::Config {
            statusline: Some(StatuslineConfig {
                rows: vec![
                    "{scopes} {plugins} {mcps} {icm} {cache} {config_stale} {throttle} {session_log}"
                        .to_string(),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json");
        std::fs::write(
            &data_path,
            serde_json::json!({
                "$schema": "llmenv-status-v1",
                "v": 1,
                "ts": "2026-07-17T00:00:00Z",
                "scopes": { "tags": ["dev", "rust"] },
                "plugins": { "total": 3, "errors": 0 },
                "mcps": { "total": 2, "errors": 0 },
                "icm": { "memories": 10, "concepts": 4 },
                "cache": { "prunable_bytes": 2048 },
                "config_stale": true,
                "throttle": { "backend": "icm", "cooldown_secs": 12 },
                "session_log": 5
            })
            .to_string(),
        )
        .unwrap();

        let stdin = b"{}";
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false).unwrap();

        assert!(out.contains("dev · rust"), "scopes widget: {out}");
        assert!(out.contains("🔌 3"), "plugins widget: {out}");
        assert!(out.contains("MCP 2"), "mcps widget: {out}");
        assert!(out.contains("🧠 10"), "icm widget: {out}");
        assert!(out.contains("2 KB"), "cache widget: {out}");
        assert!(out.contains("icm: 12s"), "throttle widget: {out}");
        assert!(out.contains('5'), "session_log widget: {out}");
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

    #[test]
    fn sanitize_removes_c0_and_c1_but_keeps_printable_and_newline_tab() {
        let input = "a\x1bb\u{9b}c\nd\te";
        assert_eq!(super::sanitize(input), "abc\nd\te");
    }

    #[test]
    fn finish_does_not_strip_control_chars_widgets_sanitize_their_own_data() {
        // finish no longer strips — it trusts the widget to have sanitized
        // untrusted data (so widgets can emit their own escapes). A widget's
        // own trusted content passes through unchanged.
        let out = finish("model", "Op\x1bus".to_string(), None, None, false);
        assert_eq!(out, "Op\x1bus");
    }

    #[test]
    fn finish_applies_default_color_when_no_style_configured() {
        // model's default style is "bold cyan" -> SGR codes 1;36.
        let out = finish("model", "Opus".to_string(), None, None, true);
        assert_eq!(out, "\x1b[1;36mOpus\x1b[0m");
    }

    #[test]
    fn finish_per_widget_style_none_opts_out_of_default_color() {
        let cfg = llmenv_config::WidgetConfig {
            style: Some("none".to_string()),
            ..Default::default()
        };
        let out = finish("model", "Opus".to_string(), Some(&cfg), None, true);
        assert_eq!(
            out, "Opus",
            "explicit style:none must suppress the default colour"
        );
    }

    #[test]
    fn global_color_off_forces_plain_even_with_color_flag() {
        let config = llmenv_config::Config {
            statusline: Some(StatuslineConfig {
                rows: vec!["{model}".to_string()],
                style: llmenv_config::StatuslineStyle {
                    color: false,
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        let stdin = br#"{"model":{"display_name":"Opus 4.8"}}"#;
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json");
        // use_color=true at the call site, but style.color=false must win.
        let out = run_statusline(&config, &data_path, &mut &stdin[..], true).unwrap();
        assert!(
            !out.contains('\u{1b}'),
            "global color off must emit no ANSI: {out:?}"
        );
    }

    #[test]
    fn engine_sourced_control_chars_are_stripped_end_to_end() {
        let config = llmenv_config::Config {
            statusline: Some(StatuslineConfig {
                rows: vec!["{branch}".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        // A branch name embedding a raw ANSI escape (git's own check-ref-format
        // would reject this in a real ref, but the widget must not trust that
        // upstream invariant — engine-sourced JSON is a separate trust boundary).
        let stdin = b"{\"branch\": {\"name\": \"feature\\u001b[31mBAD\"}}";
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("llmenv-status.json");
        let out = run_statusline(&config, &data_path, &mut &stdin[..], false).unwrap();
        assert!(
            !out.contains('\u{1b}'),
            "escape char leaked into output: {out:?}"
        );
        assert_eq!(out.trim_end(), "\u{1f33f} feature[31mBAD");
    }
}
