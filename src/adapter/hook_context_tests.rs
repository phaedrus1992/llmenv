//! Tests of the hook-output envelope that `emit_hook_context` builds.

use super::*;

#[test]
fn emit_hook_context_treats_whitespace_only_as_empty() {
    for ss in [SessionStartContext::Accepted, SessionStartContext::Rejected] {
        assert!(emit_hook_context("UserPromptSubmit", "\n\n", "", ss).is_empty());
        assert!(emit_hook_context("SessionStart", "   ", "", ss).is_empty());
        assert!(emit_hook_context("PreToolUse", "", " \n", ss).is_empty());
    }
}

use proptest::prelude::*;

proptest! {
    // The header labels the memory exactly when memory has text, and the memory is the last
    // text in the envelope, so no notice can follow it unlabeled (#2754).
    #[test]
    fn memory_is_labelled_once_and_ends_the_context(
        memory in "[ -~\n]{0,80}",
        notes in "[ -~\n]{0,80}",
    ) {
        let out = emit_hook_context("UserPromptSubmit", &memory, &notes, SessionStartContext::Accepted);
        let ctx = additional_context(&out);
        let labels = ctx.matches("[ICM MEMORY CONTEXT (auto-injected)]").count();
        prop_assert_eq!(labels, usize::from(!memory.trim().is_empty()));
        if memory.trim().is_empty() && notes.trim().is_empty() {
            prop_assert!(out.is_empty());
        }
        if !memory.trim().is_empty() {
            prop_assert!(ctx.ends_with(&memory));
        }
    }
}

fn additional_context(out: &str) -> String {
    // Invalid JSON yields Null here, so the assertion that follows fails with the output.
    let value: serde_json::Value = serde_json::from_str(out).unwrap_or_default();
    value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// #2754: only ICM memory carries the ICM header. llmenv notes go out unlabeled.
#[test]
fn notes_go_out_without_the_icm_header() {
    let out = emit_hook_context(
        "PreToolUse",
        "",
        "note: this command changes the working directory",
        SessionStartContext::Accepted,
    );
    assert_eq!(
        additional_context(&out),
        "note: this command changes the working directory"
    );
}

#[test]
fn memory_is_labelled_and_the_notes_follow_it() {
    let out = emit_hook_context(
        "UserPromptSubmit",
        "recalled fact",
        "note: budget",
        SessionStartContext::Accepted,
    );
    assert_eq!(
        additional_context(&out),
        "note: budget\n\n[ICM MEMORY CONTEXT (auto-injected)]\nrecalled fact"
    );
}

// #3331: hook_run reads the notice list before it attaches a notice, so an output that is not
// a JSON object would drop the notice. Every registered adapter must keep this contract.
#[test]
fn every_adapter_returns_a_json_object_or_an_empty_string() {
    use proptest::prelude::*;
    let events = [
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
    ];
    // Control characters, quotes, backslashes, and non-ASCII must reach the JSON encoder.
    proptest!(|(memory in "(\\PC|\n|\t|\r){0,60}", notes in "(\\PC|\n|\t|\r){0,60}")| {
        for adapter in registered_adapters() {
            for event in events {
                let out = adapter.emit_hook_context(event, &memory, &notes);
                if out.is_empty() {
                    continue;
                }
                let is_object = serde_json::from_str::<serde_json::Value>(&out)
                    .is_ok_and(|value| value.is_object());
                prop_assert!(is_object, "{event} output is not a JSON object: {out}");
            }
        }
    });
}

// #2142: per event and per engine capability, the header that wraps the
// text, or `None` for no output.
#[test]
fn emit_hook_context_event_table() {
    use SessionStartContext::{Accepted, Rejected};
    let cases = [
        (
            "SessionStart",
            Accepted,
            Some("[ICM MEMORY CONTEXT (session start)]"),
        ),
        ("SessionStart", Rejected, None),
        ("SessionEnd", Accepted, None),
        ("SessionEnd", Rejected, None),
        (
            "UserPromptSubmit",
            Accepted,
            Some("[ICM MEMORY CONTEXT (auto-injected)]"),
        ),
        (
            "UserPromptSubmit",
            Rejected,
            Some("[ICM MEMORY CONTEXT (auto-injected)]"),
        ),
    ];
    for (event, ss, header) in cases {
        let out = emit_hook_context(event, "body", "", ss);
        match header {
            None => assert_eq!(out, "", "{event} {ss:?}"),
            Some(header) => {
                let v: serde_json::Value = serde_json::from_str(&out).unwrap();
                assert_eq!(v["hookSpecificOutput"]["hookEventName"], event);
                assert_eq!(
                    v["hookSpecificOutput"]["additionalContext"],
                    format!("{header}\nbody"),
                    "{event} {ss:?}"
                );
            }
        }
    }
}
