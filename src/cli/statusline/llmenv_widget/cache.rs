//! The `cache` widget: the prunable cache size, humanized or raw.

use super::humanize::humanize_bytes;
use crate::cli::statusline::data::StatusData;

pub(super) fn render_cache(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(cache) = &data.cache else {
        return String::new();
    };
    // `None` means the size could not be read. The marker keeps the field visible.
    // A partial total is a floor, so "at least" keeps it from reading as exact (#3069).
    let humanized = cache.prunable_bytes.map_or_else(
        || UNKNOWN_MARKER.to_string(),
        |bytes| {
            if cache.prunable_partial {
                format!("at least {}", humanize_bytes(bytes))
            } else {
                humanize_bytes(bytes)
            }
        },
    );
    let raw = cache
        .prunable_bytes
        .map_or_else(|| UNKNOWN_MARKER.to_string(), |bytes| bytes.to_string());
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{prunable}");
    format
        .replace("{prunable}", &humanized)
        .replace("{prunable_raw}", &raw)
}

/// Shown in place of a size that could not be read.
pub(super) const UNKNOWN_MARKER: &str = "?";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::statusline::data::CacheData;
    use proptest::prelude::*;

    fn data(prunable_bytes: Option<u64>, prunable_partial: bool) -> StatusData {
        StatusData {
            cache: Some(CacheData {
                prunable_bytes,
                prunable_partial,
            }),
            ..Default::default()
        }
    }

    proptest! {
        /// A custom format gets each placeholder replaced and keeps every other byte. An unknown
        /// size shows the marker in both placeholders, and a partial size reads as a floor (#3064).
        #[test]
        fn a_format_replaces_only_its_placeholders(
            bytes in proptest::option::of(any::<u64>()),
            partial in any::<bool>(),
            parts in proptest::collection::vec(
                prop_oneof![
                    Just("{prunable}".to_string()),
                    Just("{prunable_raw}".to_string()),
                    "[a-z :()]{0,6}",
                ],
                0..6,
            ),
        ) {
            let format: String = parts.concat();
            let cfg = llmenv_config::WidgetConfig {
                format: Some(format.clone()),
                ..Default::default()
            };
            let (human, raw) = match bytes {
                None => (UNKNOWN_MARKER.to_string(), UNKNOWN_MARKER.to_string()),
                Some(b) if partial => (format!("at least {}", humanize_bytes(b)), b.to_string()),
                Some(b) => (humanize_bytes(b), b.to_string()),
            };
            let want = format.replace("{prunable}", &human).replace("{prunable_raw}", &raw);
            prop_assert_eq!(render_cache(&data(bytes, partial), Some(&cfg)), want);
            prop_assert_eq!(render_cache(&data(bytes, partial), None), human);
        }
    }

    #[test]
    fn no_cache_data_renders_nothing() {
        assert_eq!(render_cache(&StatusData::default(), None), "");
    }
}
