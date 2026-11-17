//! The `icm` widget: the memory count and the concept count from the ICM stats.

use crate::cli::statusline::UNKNOWN_MARKER;
use crate::cli::statusline::data::StatusData;

pub(super) fn render_icm(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("🧠 {memories}");
    let Some(icm) = &data.icm else {
        // A failed query keeps the format and shows the marker. No backend shows nothing (#3745).
        return if data.icm_failed {
            crate::cli::statusline::fill_unknown(
                cfg,
                "🧠 {memories}",
                &["{memories}", "{concepts}"],
            )
        } else {
            String::new()
        };
    };
    format
        .replace("{memories}", &icm.memories.to_string())
        .replace(
            "{concepts}",
            &icm.concepts
                .map_or_else(|| UNKNOWN_MARKER.to_owned(), |n| n.to_string()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::statusline::data::IcmData;

    #[test]
    fn render_icm_honors_custom_format() {
        let data = StatusData {
            icm: Some(IcmData {
                memories: 10,
                concepts: Some(4),
            }),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{memories}c{concepts}".to_string()),
            ..Default::default()
        };
        let out = render_icm(&data, Some(&cfg));
        assert_eq!(out, "10c4");
    }

    #[test]
    fn render_icm_shows_unknown_marker_when_concepts_is_unknown() {
        let data = StatusData {
            icm: Some(IcmData {
                memories: 10,
                concepts: None,
            }),
            ..Default::default()
        };
        let cfg = llmenv_config::WidgetConfig {
            format: Some("{memories}c{concepts}".to_string()),
            ..Default::default()
        };
        let out = render_icm(&data, Some(&cfg));
        assert_eq!(out, format!("10c{UNKNOWN_MARKER}"));
    }

    #[test]
    fn render_icm_keeps_its_format_and_shows_the_marker_when_the_query_failed() {
        let data = StatusData {
            icm_failed: true,
            ..Default::default()
        };
        assert_eq!(render_icm(&data, None), format!("🧠 {UNKNOWN_MARKER}"));
    }

    #[test]
    fn render_icm_is_empty_when_no_backend_exists() {
        assert_eq!(render_icm(&StatusData::default(), None), "");
    }
}
