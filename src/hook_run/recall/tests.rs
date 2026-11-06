use std::cell::Cell;
use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::scope::{ActiveScope, ActiveScopes};

fn scope(kind: &'static str, tags: &[&str]) -> ActiveScope {
    ActiveScope {
        kind,
        tags: tags.iter().map(ToString::to_string).collect(),
        ..ActiveScope::default()
    }
}

fn active(scopes: Vec<ActiveScope>, extra: &[&str], loose: &[&str]) -> ActiveScopes {
    let mut tags: std::collections::BTreeSet<String> =
        scopes.iter().flat_map(|s| s.tags.iter().cloned()).collect();
    tags.extend(extra.iter().map(ToString::to_string));
    tags.extend(loose.iter().map(ToString::to_string));
    ActiveScopes {
        scopes,
        tags,
        extra_tags: extra.iter().map(ToString::to_string).collect(),
    }
}

#[test]
fn tag_specificity_ranks_by_scope_kind() {
    let scopes = vec![
        scope("project", &["proj", "shared"]),
        scope("content", &["cont"]),
        scope("network", &["net"]),
        scope("user", &["usr"]),
        scope("host", &["hst", "shared"]),
    ];
    let ranks = tag_specificity(&active(scopes, &["extra"], &["macos"]));
    let expected: BTreeMap<String, u8> = [
        ("proj", 1),
        ("shared", 1),
        ("cont", 2),
        ("net", 3),
        ("usr", 4),
        ("hst", 5),
        ("extra", 1),
        ("macos", 6),
    ]
    .into_iter()
    .map(|(tag, rank)| (tag.to_string(), rank))
    .collect();
    assert_eq!(ranks, expected);
}

fn records(count: usize, size: usize, tag: &str) -> String {
    (0..count)
        .map(|i| format!("[t] {tag}{i:04}{}", "x".repeat(size.saturating_sub(9))))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run_texts(texts: Vec<String>) -> (String, RecallBudget) {
    let actions = texts
        .iter()
        .map(|_| Action::Recall(String::new()))
        .collect();
    let index = Cell::new(0);
    run_with_budget(actions, |_| {
        let text = texts[index.get()].clone();
        index.set(index.get() + 1);
        async move { Ok(Reply::Records(test_records(&text))) }
    })
    .await
}

fn with_id(id: &str, text: &str) -> Recalled {
    Recalled {
        id: id.to_string(),
        ..Recalled::from_text(text)
    }
}

#[test]
fn two_records_with_the_same_text_and_different_ids_are_both_kept() {
    let mut budget = RecallBudget::default();
    budget.add_records(vec![with_id("a", "[t] same"), with_id("b", "[t] same")]);
    budget.add_records(vec![with_id("a", "[t] same")]);
    assert_eq!(budget.kept(), ["[t] same", "[t] same"]);
    assert_eq!(budget.duplicates, 1);
    assert_eq!(budget.kept_keys(), ["a", "b"]);
}

#[test]
fn a_ledger_of_text_hashes_dedups_once_then_moves_to_ids() {
    // A ledger from before #3369 holds the text hash of a record that it sent.
    let old = BTreeSet::from([record_hash("[t] sent before")]);
    let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES, old.clone());
    budget.add_records(vec![
        with_id("old-id", "[t] sent before"),
        with_id("new", "[t] new"),
    ]);
    assert_eq!(budget.kept(), ["[t] new"]);
    assert_eq!(budget.already_sent, 1);
    // The ledger learns the id of the old record beside the kept one.
    let keys = budget.kept_keys();
    assert_eq!(keys, ["new", "old-id"]);
    // Next turn the ledger holds ids, so the old record is skipped by its id.
    let mut sent = old;
    sent.extend(keys);
    let mut next = RecallBudget::new(RECALL_BUDGET_BYTES, sent);
    next.add_records(vec![with_id("old-id", "[t] sent before")]);
    assert!(next.kept().is_empty());
    assert!(
        next.kept_keys().is_empty(),
        "an id match is not migrated again"
    );
}

#[test]
fn a_record_with_no_id_is_keyed_by_its_text_hash() {
    let mut budget = RecallBudget::new(
        RECALL_BUDGET_BYTES,
        BTreeSet::from([record_hash("[t] old")]),
    );
    budget.add_text("[t] old\n[t] new\n[t] new");
    assert_eq!(budget.kept(), ["[t] new"]);
    assert_eq!(budget.duplicates, 1);
    assert_eq!(budget.kept_keys(), [record_hash("[t] new")]);
}

#[test]
fn hook_text_keeps_the_omission_notice_out_of_memory() {
    // The notice is llmenv text, so it must not sit under the ICM memory header (#2764).
    let mut budget = RecallBudget::default();
    budget.add_strings(vec!["x".repeat(RECALL_BUDGET_BYTES + 1)]);
    let text = budget.hook_text(Vec::new());
    assert!(!text.memory.contains("omitted"), "{}", text.memory);
    assert!(
        text.notes.contains("1 lower-priority memories omitted"),
        "{}",
        text.notes
    );
}

#[tokio::test]
async fn budget_keeps_whole_records_and_reports_the_omitted_count() {
    let (text, budget) = run_texts(vec![records(30, 500, "a")]).await;
    let notice = budget.notice().expect("has a notice");
    assert!(
        text.len() <= RECALL_BUDGET_BYTES + notice.len() + SEPARATOR_BYTES,
        "{}",
        text.len()
    );
    assert_eq!(budget.kept().len() + budget.omitted, 30);
    assert!(budget.omitted > 0);
    assert_eq!(
        notice,
        format!(
            "[llmenv] {} lower-priority memories omitted to stay under the context limit.",
            budget.omitted
        )
    );
    assert!(
        budget.kept().iter().all(|r| r.len() == 500),
        "whole records only"
    );
}

#[tokio::test]
async fn a_duplicate_record_is_kept_once_at_its_first_position() {
    let (text, budget) = run_texts(vec![
        "[t] shared\n[t] one".to_string(),
        "[t] two\n[t] shared".to_string(),
    ])
    .await;
    assert_eq!(text, "[t] shared\n\n[t] one\n\n[t] two");
    assert_eq!(budget.duplicates, 1);
    assert_eq!(budget.omitted, 0);
}

#[tokio::test]
async fn a_small_record_still_fits_after_a_large_one_is_omitted() {
    let big = format!("[t] {}", "b".repeat(RECALL_BUDGET_BYTES));
    let (text, budget) = run_texts(vec![format!("[t] first\n{big}\n[t] small")]).await;
    assert!(text.starts_with("[t] first\n\n[t] small"), "{text}");
    assert_eq!(budget.omitted, 1);
}

#[tokio::test]
async fn later_recalls_are_skipped_once_the_budget_is_full() {
    let calls = Cell::new(0);
    let actions = vec![
        Action::Recall(String::new()),
        Action::Recall(String::new()),
        Action::Recall(String::new()),
    ];
    let (text, budget) = run_with_budget(actions, |_| {
        calls.set(calls.get() + 1);
        async {
            Ok(Reply::Records(test_records(&format!(
                "[t] {}\n[t] {}",
                "a".repeat(7_900),
                "b".repeat(500)
            ))))
        }
    })
    .await;
    assert_eq!(calls.get(), 1, "budget was full after the first recall");
    assert_eq!(budget.skipped_actions, 2);
    assert_eq!(budget.omitted, 1);
    assert!(
        budget.notice().unwrap_or_default().ends_with(&format!(
            "[llmenv] {} lower-priority memories omitted; 2 recall queries skipped to stay \
                 under the context limit.",
            budget.omitted
        )),
        "{text}"
    );
}

#[tokio::test]
async fn non_recall_actions_pass_through_outside_the_budget() {
    let big = "w".repeat(RECALL_BUDGET_BYTES + 1000);
    let (text, budget) = run_with_budget(
        vec![Action::WakeUp(
            crate::hook_run::action::WakeUpArgs::default(),
        )],
        |_| {
            let big = big.clone();
            async move { Ok(Reply::Text(big)) }
        },
    )
    .await;
    assert_eq!(text, big);
    assert_eq!(budget, RecallBudget::default());
}

fn record_of(len: usize) -> String {
    format!("[t] {}", "x".repeat(len - 4))
}

#[test]
fn the_budget_is_used_exactly_and_never_exceeded() {
    let mut budget = RecallBudget::default();
    // Two records use 3,002 bytes each with the separator, so 1,994 bytes of room remain.
    budget.add_strings(vec![record_of(3_000), format!("{}y", record_of(2_999))]);
    budget.add_strings(vec![record_of(1_995), record_of(1_994)]);
    let lengths: Vec<usize> = budget.kept().iter().map(|r| r.len()).collect();
    assert_eq!(lengths, [3_000, 3_000, 1_994]);
    assert_eq!(budget.omitted, 1);
}

#[test]
fn a_full_budget_starts_at_exactly_the_minimum_free_bytes() {
    let mut budget = RecallBudget::default();
    // 7,798 + 2 separator bytes leaves exactly MIN_FREE_BYTES free: not full yet.
    budget.add_strings(vec![record_of(
        RECALL_BUDGET_BYTES - MIN_FREE_BYTES - SEPARATOR_BYTES,
    )]);
    assert!(!budget.is_full());
    budget.add_strings(vec![record_of(6)]);
    assert!(budget.is_full());
}

#[test]
fn sent_records_are_skipped_and_counted() {
    let sent: BTreeSet<String> = [record_hash("[t] old")].into();
    let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES, sent);
    budget.add_text("[t] old\n[t] new");
    assert_eq!(budget.kept(), ["[t] new"]);
    assert_eq!(budget.already_sent, 1);
    assert_eq!(budget.kept_keys(), [record_hash("[t] new")]);
}

#[test]
fn a_smaller_limit_is_respected() {
    let mut budget = RecallBudget::new(2_000, BTreeSet::new());
    budget.add_strings(vec![record_of(1_500), format!("{}y", record_of(1_499))]);
    assert_eq!(budget.kept().len(), 1);
    assert_eq!(budget.omitted, 1);
}

#[test]
fn trace_line_is_absent_until_a_recall_ran() {
    assert_eq!(RecallBudget::default().trace_line(true), None);
    let skipped_only = RecallBudget {
        skipped_actions: 2,
        ..RecallBudget::default()
    };
    assert!(skipped_only.trace_line(true).is_some());
    assert_eq!(skipped_only.trace_line(false), None);
}

#[test]
fn trace_line_reports_every_counter() {
    let mut budget = RecallBudget::default();
    budget.add_strings(vec![
        "[t] a".to_string(),
        "[t] a".to_string(),
        "[t] bb".to_string(),
    ]);
    assert_eq!(
        budget.trace_line(true).as_deref(),
        Some(
            "[LLMENV_CONTEXT] recall_entries=3 recall_bytes=16 injected_entries=2 \
                 injected_bytes=11 duplicate_records=1 already_sent=0 omitted=0 \
                 skipped_actions=0"
        )
    );
}

#[tokio::test]
async fn passthrough_text_skips_empty_results_and_exact_duplicates() {
    let texts = ["", "pack", "pack", "other"];
    let index = Cell::new(0);
    let (text, _) = run_with_budget(
        vec![Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()); 4],
        |_| {
            let text = texts[index.get()].to_string();
            index.set(index.get() + 1);
            async move { Ok(Reply::Text(text)) }
        },
    )
    .await;
    assert_eq!(text, "pack\n\nother");
}

#[tokio::test]
async fn a_failing_action_keeps_the_text_of_the_others() {
    let actions = vec![Action::Recall(String::new()), Action::Recall(String::new())];
    let mut calls = 0;
    let (text, _) = run_with_budget(actions, |_| {
        calls += 1;
        let first = calls == 1;
        async move {
            if first {
                Err(anyhow::anyhow!("boom"))
            } else {
                Ok(Reply::Records(test_records("[t] kept record")))
            }
        }
    })
    .await;
    assert_eq!(text, "[t] kept record");
}

/// Run one passthrough action per outcome. `None` is a failed action.
fn run_outcomes(outcomes: Vec<Option<String>>) -> String {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    let actions =
        vec![Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()); outcomes.len()];
    let mut outcomes = outcomes.into_iter();
    let (text, _) = runtime.block_on(run_with_budget(actions, |_| {
        let outcome = outcomes.next().expect("one outcome per action");
        async move {
            outcome
                .map(Reply::Text)
                .ok_or_else(|| anyhow::anyhow!("boom"))
        }
    }));
    text
}

proptest! {
    /// A failed action changes nothing else: the text equals the run without that action.
    #[test]
    fn failing_actions_do_not_change_the_text_of_the_others(
        outcomes in prop::collection::vec(prop::option::of("[a-z]{1,8}"), 1..10),
    ) {
        let kept: Vec<Option<String>> = outcomes.iter().filter(|o| o.is_some()).cloned().collect();
        prop_assert_eq!(run_outcomes(outcomes), run_outcomes(kept));
    }
}

fn arb_record() -> impl Strategy<Value = Recalled> {
    ("[a-d]", "[a-z]{1,40}").prop_map(|(id, body)| Recalled {
        id,
        ..Recalled::from_text(&format!("[t] {body}"))
    })
}

proptest! {
    /// Every record is kept, a duplicate, already sent, or omitted; no kept id was sent; and
    /// once the kept keys go back to the ledger, the same records keep nothing.
    #[test]
    fn the_budget_accounts_for_every_record_and_learns_its_keys(
        records in prop::collection::vec(arb_record(), 0..12),
        sent_ids in prop::collection::btree_set("[a-d]", 0..3),
        limit in 50usize..400,
    ) {
        let mut budget = RecallBudget::new(limit, sent_ids.clone());
        let count = records.len();
        budget.add_records(records);
        prop_assert_eq!(
            budget.kept().len() + budget.duplicates + budget.already_sent + budget.omitted,
            count
        );
        prop_assert!(budget.kept.iter().all(|r| !sent_ids.contains(&r.id)));
        let mut sent = sent_ids;
        sent.extend(budget.kept_keys());
        let mut next = RecallBudget::new(limit, sent);
        next.add_records(budget.kept.to_vec());
        prop_assert!(next.kept().is_empty());
    }
}

proptest! {
    /// Whatever the records, the kept text stays within budget and every kept record is
    /// byte-identical to an input record.
    #[test]
    fn kept_records_fit_and_come_from_the_input(
        input in prop::collection::vec("[a-z ]{1,900}", 0..40)
    ) {
        let input: Vec<String> = input
            .into_iter()
            .map(|body| format!("[t] {}", body.trim_end()))
            .collect();
        let mut budget = RecallBudget::default();
        budget.add_strings(input.clone());
        let kept_bytes: usize = budget.kept().iter().map(|r| r.len() + SEPARATOR_BYTES).sum();
        prop_assert!(kept_bytes <= RECALL_BUDGET_BYTES);
        for record in budget.kept() {
            prop_assert!(input.iter().any(|i| i == record));
        }
        prop_assert_eq!(
            budget.kept().len() + budget.omitted + budget.duplicates,
            input.len()
        );
    }
}

// The line must keep the shape that website/docs/trace.md shows (#3018).
#[test]
fn trace_line_matches_the_docs_page() {
    let mut budget = RecallBudget::default();
    budget.add_strings(vec!["[t] a".to_string()]);
    let line = budget.trace_line(true).unwrap_or_default();
    assert!(
        crate::trace::docs_sync::docs_show(&line),
        "website/docs/trace.md shows no template for `{line}`"
    );
}
