//! Stable keys for recall records, used to tell a sent record from a new one.

#[cfg(test)]
pub(crate) use llmenv_util::hash::HASH_HEX_CHARS;
pub(crate) use llmenv_util::hash::hash_prefix;

/// A stable key for one recall record: whitespace layout does not change it.
pub(crate) fn record_hash(record: &str) -> String {
    let normalized = record.split_whitespace().collect::<Vec<_>>().join(" ");
    hash_prefix(normalized.as_bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use proptest::prelude::*;

    use super::{HASH_HEX_CHARS, record_hash};

    #[test]
    fn a_key_has_the_fixed_length_of_hex_digits() {
        let key = record_hash("[t] one record");

        assert_eq!(key.len(), HASH_HEX_CHARS);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn an_empty_record_still_has_a_key() {
        assert_eq!(record_hash("").len(), HASH_HEX_CHARS);
    }

    proptest! {
        /// Whitespace layout never changes the key, so the same words always dedupe.
        #[test]
        fn whitespace_layout_never_changes_the_key(
            words in proptest::collection::vec("[a-z]{1,6}", 0..8),
            gap in "[ \t\n]{1,4}",
        ) {
            let single = words.join(" ");
            let spaced = words.join(&gap);

            prop_assert_eq!(record_hash(&spaced), record_hash(&single));
        }
    }
}
