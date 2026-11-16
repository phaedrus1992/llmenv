//! The `icm` widget: the memory count and the concept count from the ICM stats.

use crate::cli::statusline::UNKNOWN_MARKER;
use crate::cli::statusline::data::StatusData;

pub(super) fn render_icm(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(icm) = &data.icm else {
        return String::new();
    };
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("🧠 {memories}");
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
}
