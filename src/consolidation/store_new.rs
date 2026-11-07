//! Storing the new rules of one consolidation run, and skipping those already stored.

use super::*;

/// Store each rule that no stored rule or earlier rule in `rules` matches.
/// Returns the number stored and the number skipped as duplicates.
pub(super) async fn store_new_rules(
    client: &McpHttpClient,
    state_dir: Option<&std::path::Path>,
    run: &RunScope<'_>,
    rules: &[&str],
) -> (usize, usize) {
    let project = run.project;
    let mut stored = 0usize;
    let mut duplicates = 0usize;
    let mut batch: Vec<&str> = Vec::new();
    for rule in rules.iter().copied() {
        match dedup::is_stored(client, project, rule).await {
            Ok(false) if !dedup::matches_any(rule, batch.iter().copied()) => {}
            Ok(_) => {
                duplicates += 1;
                continue;
            }
            Err(e) => {
                // The agent is told here, because the summary names the notices (#3507).
                crate::agent_notice::record_fixed_with_cause(
                    "llmenv: consolidation could not check a rule for duplicates, so the rule was \
                     skipped. Fix: run `llmenv doctor` to check the memory server. The cause is in \
                     the llmenv log.",
                    &e,
                );
                continue;
            }
        }
        batch.push(rule);
        match store_rule(client, state_dir, run, rule).await {
            Ok(()) => stored += 1,
            Err(e) => {
                crate::agent_notice::record_fixed_with_cause(
                    "llmenv: consolidation could not store a rule, and it is tried again on the \
                     next run. Fix: run `llmenv doctor` to check the memory server. The cause is in \
                     the llmenv log.",
                    &e,
                );
            }
        }
    }
    (stored, duplicates)
}
