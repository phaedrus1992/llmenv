//! The `cache` widget: the prunable cache size, humanized or raw.

use super::humanize::humanize_bytes;
use crate::cli::statusline::UNKNOWN_MARKER;
use crate::cli::statusline::data::StatusData;

pub(super) fn render_cache(data: &StatusData, cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let Some(cache) = &data.cache else {
        return String::new();
    };
    // `None` means the size could not be read. The marker keeps the field visible.
    // A partial total is a floor, so "at least" keeps it from reading as exact. The count tells
    // the user how many entries to look at (#3069, #3110).
    let humanized = cache.prunable_bytes.map_or_else(
        || UNKNOWN_MARKER.to_string(),
        |bytes| match cache.unreadable {
            0 => humanize_bytes(bytes),
            n => format!("at least {} ({n} unreadable)", humanize_bytes(bytes)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::statusline::data::CacheData;
    use proptest::prelude::*;

    fn data(prunable_bytes: Option<u64>, unreadable: u64) -> StatusData {
        StatusData {
            cache: Some(CacheData {
                prunable_bytes,
                unreadable,
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
            unreadable in 0u64..5,
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
                Some(b) if unreadable > 0 => (
                    format!("at least {} ({unreadable} unreadable)", humanize_bytes(b)),
                    b.to_string(),
                ),
                Some(b) => (humanize_bytes(b), b.to_string()),
            };
            // The oracle maps each generated part, so it does not share the replace chain.
            let want: String = parts
                .iter()
                .map(|part| match part.as_str() {
                    "{prunable}" => human.as_str(),
                    "{prunable_raw}" => raw.as_str(),
                    literal => literal,
                })
                .collect();
            prop_assert_eq!(render_cache(&data(bytes, unreadable), Some(&cfg)), want);
            prop_assert_eq!(render_cache(&data(bytes, unreadable), None), human);
        }
    }

    #[test]
    fn no_cache_data_renders_nothing() {
        assert_eq!(render_cache(&StatusData::default(), None), "");
    }
}
