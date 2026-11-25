//! The size cap of the llmenv notices in a hook output (#3357).
//!
//! Claude Code cuts the text that a hook returns at a fixed number of characters. A cut in the
//! middle of a notice leaves half a sentence, and Claude Code 2.1.314 made a very large PreCompact
//! output fail or repeat compaction. The notices are the part of a hook output with no size
//! limit of its own, so they are shortened here, to whole lines. The notices of a hook run are
//! one per line, so a cut keeps whole notices.

/// The number of characters at which Claude Code cuts a hook output.
const CLAUDE_CODE_CUT_CHARS: usize = 10_000;

/// The most characters that a hook output may hold. The margin leaves room for the line that
/// reports the omission, and for the header of the memory block.
const MAX_CONTEXT_CHARS: usize = CLAUDE_CODE_CUT_CHARS - 500;

/// The characters of the blank line that separates the notices from the memory block.
const SEPARATOR_CHARS: usize = 2;

/// The length of `text` in UTF-16 units, which is how Claude Code counts characters.
pub(crate) fn context_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The line that replaces the notices that did not fit.
fn omission_line(left_out: usize) -> String {
    format!(
        "llmenv: {left_out} more line(s) of notices were left out, because Claude Code cuts a hook output \
         at {CLAUDE_CODE_CUT_CHARS} characters. Fix: run `llmenv doctor`, and read the llmenv log."
    )
}

/// Shortens `notes` so that the notices, and `memory_chars` characters of memory after them, fit
/// in [`MAX_CONTEXT_CHARS`]. The first notices stay whole, one per line. One line counts the rest.
/// With no room for even that line, the notices are dropped and the drop is logged, and the
/// memory keeps its place.
pub(crate) fn fit_notes(notes: &str, memory_chars: usize) -> String {
    let reserved = if memory_chars > 0 {
        memory_chars + SEPARATOR_CHARS
    } else {
        0
    };
    let room = MAX_CONTEXT_CHARS.saturating_sub(reserved);
    if context_len(notes) <= room {
        return notes.to_string();
    }
    let lines: Vec<&str> = notes.lines().collect();
    let mut kept = String::new();
    let mut used = 0;
    for (index, line) in lines.iter().enumerate() {
        let next = used + context_len(line) + 1;
        let omission = context_len(&omission_line(lines.len() - index)) + 1;
        if next + omission > room {
            return close_with_omission(kept, lines.len() - index, used, room);
        }
        kept.push_str(line);
        kept.push('\n');
        used = next;
    }
    kept.trim_end().to_string()
}

/// `kept` plus the omission line for `left_out` notices, when that line fits in `room`.
fn close_with_omission(mut kept: String, left_out: usize, used: usize, room: usize) -> String {
    let omission = omission_line(left_out);
    if used + context_len(&omission) > room {
        tracing::error!(
            left_out,
            room,
            "the notices of a hook output are dropped, because the memory block fills the cap"
        );
        return String::new();
    }
    kept.push_str(&omission);
    kept
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn notices(count: usize, width: usize) -> String {
        (0..count)
            .map(|i| format!("llmenv: notice {i} {}", "x".repeat(width)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn notices_that_fit_are_kept_unchanged() {
        let notes = notices(5, 100);
        assert_eq!(fit_notes(&notes, 0), notes);
        assert_eq!(fit_notes("", 8_000), "");
    }

    #[test]
    fn a_large_set_keeps_whole_first_notices_and_counts_the_rest() {
        let notes = notices(400, 100);
        let out = fit_notes(&notes, 0);
        assert!(
            context_len(&out) <= MAX_CONTEXT_CHARS,
            "{}",
            context_len(&out)
        );
        assert!(out.starts_with("llmenv: notice 0 "), "{out}");
        let last = out.lines().last().unwrap();
        assert!(
            last.contains("more line(s) of notices were left out"),
            "{last}"
        );
        assert!(last.contains("llmenv doctor"), "{last}");
        // Every kept notice is whole: it ends with its padding.
        for line in out.lines().filter(|l| l.starts_with("llmenv: notice")) {
            assert!(line.ends_with(&"x".repeat(100)), "{line}");
        }
        let kept = out.lines().count() - 1;
        let counted: usize = last
            .trim_start_matches("llmenv: ")
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(kept + counted, 400);
    }

    #[test]
    fn memory_keeps_its_room_and_the_notices_shrink() {
        let notes = notices(100, 100);
        let out = fit_notes(&notes, 8_000);
        assert!(context_len(&out) + 8_000 + SEPARATOR_CHARS <= MAX_CONTEXT_CHARS);
    }

    #[test]
    fn memory_that_fills_the_cap_leaves_no_notices() {
        assert_eq!(fit_notes(&notices(3, 50), MAX_CONTEXT_CHARS), "");
    }

    proptest! {
        /// Whatever the notices and the memory size, the result plus the memory fits the cap,
        /// and a result that is not empty ends on a whole line.
        #[test]
        fn the_result_always_fits(
            count in 0usize..300,
            width in 0usize..200,
            memory in 0usize..12_000,
        ) {
            let out = fit_notes(&notices(count, width), memory);
            let reserved = if memory > 0 { memory + SEPARATOR_CHARS } else { 0 };
            if reserved <= MAX_CONTEXT_CHARS {
                prop_assert!(context_len(&out) + reserved <= MAX_CONTEXT_CHARS);
            } else {
                prop_assert_eq!(out, "");
            }
        }

        /// Notices that fit are never changed.
        #[test]
        fn a_fitting_set_is_unchanged(count in 0usize..20, width in 0usize..100) {
            let notes = notices(count, width);
            prop_assert_eq!(fit_notes(&notes, 0), notes);
        }
    }
}
