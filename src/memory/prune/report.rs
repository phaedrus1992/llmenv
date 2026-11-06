//! The summary that `llmenv memory prune` prints. Only the command prints it: the `auto_prune`
//! pass of `llmenv export` must keep stdout for the shell (#2947).

use super::PruneResult;

/// Print the summary of one prune pass to stdout.
#[expect(
    clippy::print_stdout,
    reason = "the summary is the output of the prune command"
)]
pub(super) fn print(result: &PruneResult) {
    for line in summary_lines(result) {
        println!("{line}");
    }
}

/// The lines of the summary, without a line ending. Kept apart from [`print`] so a test reads
/// the text without capturing stdout.
fn summary_lines(result: &PruneResult) -> Vec<String> {
    let (title, medium, low) = if result.dry_run {
        (
            "Prune dry-run — no changes made.",
            "medium (candidate):  ",
            "low (candidate):     ",
        )
    } else {
        (
            "Prune complete.",
            "medium (pruned):     ",
            "low (pruned):        ",
        )
    };
    let mut lines = vec![
        title.to_owned(),
        format!("  Total records:       {}", result.total),
        format!("  critical (kept):     {}", result.critical),
        format!("  high (kept):         {}", result.high),
        format!("  {medium}{}", result.medium),
        format!("  {low}{}", result.low),
    ];
    if result.unknown > 0 {
        lines.push(format!(
            "  unknown (kept):      {} (an importance that prune does not know)",
            result.unknown
        ));
    }
    if result.dry_run {
        lines.extend(dry_run_lines(result));
    } else {
        lines.push(format!(
            "  Forgot:              {} record(s)",
            result.forgotten
        ));
        if result.forgotten != result.candidates {
            lines.push(format!(
                "  Skipped:             {} record(s) (forget errored)",
                result.candidates - result.forgotten
            ));
        }
    }
    lines
}

/// The dry-run tail: the would-prune counts, then the reason for the heuristic.
fn dry_run_lines(result: &PruneResult) -> Vec<String> {
    let mut lines = vec![format!(
        "  Would prune:         {} record(s) (low + medium importance)",
        result.candidates
    )];
    if result.total > 0 {
        lines.push(format!(
            "  Would keep:          {} record(s) (critical + high importance)",
            result.total - result.candidates
        ));
    }
    if result.candidates == 0 {
        lines.push("  No prune candidates found.".to_owned());
        return lines;
    }
    let pct = result.candidates as f64 / result.total as f64 * 100.0;
    lines.push(format!("  ({pct:.1}% of records are prune candidates)"));
    lines.push(String::new());
    lines.extend(
        [
            "  This pass uses importance, not age: low and medium importance",
            "  memories are candidates; high and critical are always kept.",
        ]
        .map(str::to_owned),
    );
    lines
}

impl PruneResult {
    /// The records that prune kept because it cannot place them, as a sentence, or `None`.
    pub(crate) fn kept_unplaced(&self) -> Option<String> {
        (self.unknown > 0).then(|| {
            format!(
                "{} record(s) that prune cannot place, with an importance it does not know. Fix: \
                 check the icm version, or run `llmenv memory prune --dry-run` to see the counts",
                self.unknown
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_that_prune_cannot_place_are_named() {
        assert_eq!(PruneResult::default().kept_unplaced(), None);
        let result = PruneResult {
            unknown: 2,
            ..Default::default()
        };
        let text = result.kept_unplaced().unwrap_or_default();
        assert!(
            text.starts_with("2 record(s) that prune cannot place"),
            "{text}"
        );
    }

    fn joined(result: &PruneResult) -> String {
        summary_lines(result).join("\n")
    }

    #[test]
    fn a_dry_run_reports_candidates_and_the_importance_heuristic() {
        let result = PruneResult {
            dry_run: true,
            total: 10,
            critical: 2,
            high: 3,
            medium: 4,
            low: 1,
            candidates: 5,
            ..Default::default()
        };
        let text = joined(&result);
        assert!(
            text.starts_with("Prune dry-run — no changes made."),
            "{text}"
        );
        assert!(text.contains("medium (candidate):  4"), "{text}");
        assert!(text.contains("Would prune:         5 record(s)"), "{text}");
        assert!(text.contains("Would keep:          5 record(s)"), "{text}");
        assert!(
            text.contains("(50.0% of records are prune candidates)"),
            "{text}"
        );
        assert!(
            text.contains("This pass uses importance, not age"),
            "{text}"
        );
        assert!(!text.contains("Forgot:"), "{text}");
    }

    #[test]
    fn a_dry_run_with_no_candidates_says_so_and_skips_the_heuristic() {
        let result = PruneResult {
            dry_run: true,
            total: 2,
            critical: 2,
            ..Default::default()
        };
        let text = joined(&result);
        assert!(text.contains("No prune candidates found."), "{text}");
        assert!(!text.contains("ponytail"), "{text}");
    }

    #[test]
    fn a_real_run_reports_what_it_forgot_and_what_it_skipped() {
        let result = PruneResult {
            total: 6,
            medium: 3,
            low: 2,
            candidates: 5,
            forgotten: 4,
            ..Default::default()
        };
        let text = joined(&result);
        assert!(text.starts_with("Prune complete."), "{text}");
        assert!(text.contains("medium (pruned):     3"), "{text}");
        assert!(text.contains("Forgot:              4 record(s)"), "{text}");
        assert!(
            text.contains("Skipped:             1 record(s) (forget errored)"),
            "{text}"
        );
        assert!(!text.contains("Would prune"), "{text}");
    }

    #[test]
    fn unknown_records_get_their_own_line() {
        let result = PruneResult {
            unknown: 1,
            ..Default::default()
        };
        let text = joined(&result);
        assert!(text.contains("unknown (kept):      1"), "{text}");
    }
}
