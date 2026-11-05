//! The summary that `llmenv memory prune` prints. Only the command prints it: the `auto_prune`
//! pass of `llmenv export` must keep stdout for the shell (#2947).

use super::PruneResult;

/// Print the summary of one prune pass to stdout.
#[expect(
    clippy::print_stdout,
    reason = "the summary is the output of the prune command"
)]
pub(super) fn print(result: &PruneResult) {
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
    println!("{title}");
    println!("  Total records:       {}", result.total);
    println!("  critical (kept):     {}", result.critical);
    println!("  high (kept):         {}", result.high);
    println!("  {medium}{}", result.medium);
    println!("  {low}{}", result.low);
    if result.unknown > 0 {
        println!(
            "  unknown (kept):      {} (an importance that prune does not know)",
            result.unknown
        );
    }
    if result.dry_run {
        print_dry_run(result);
    } else {
        println!("  Forgot:              {} record(s)", result.forgotten);
        if result.forgotten != result.candidates {
            println!(
                "  Skipped:             {} record(s) (forget errored)",
                result.candidates - result.forgotten
            );
        }
    }
}

#[expect(
    clippy::print_stdout,
    reason = "the summary is the output of the prune command"
)]
fn print_dry_run(result: &PruneResult) {
    println!(
        "  Would prune:         {} record(s) (low + medium importance)",
        result.candidates
    );
    if result.total > 0 {
        println!(
            "  Would keep:          {} record(s) (critical + high importance)",
            result.total - result.candidates
        );
    }
    if result.candidates == 0 {
        println!("  No prune candidates found.");
        return;
    }
    let pct = result.candidates as f64 / result.total as f64 * 100.0;
    println!("  ({pct:.1}% of records are prune candidates)");
    println!();
    println!("  This pass uses importance, not age: low and medium importance");
    println!("  memories are candidates; high and critical are always kept.");
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
}
