//! Agent notices for hook output that the cache notice cannot join.

use super::*;

fn notices() -> String {
    crate::agent_notice::current().unwrap_or_default()
}

#[test]
fn a_non_object_hook_output_names_the_fix() {
    attach_system_message("[1]", "cache notice");
    assert!(notices().contains("is not a JSON object"), "{}", notices());
}

#[test]
fn a_non_json_hook_output_names_the_fix() {
    attach_system_message("not json", "cache notice");
    assert!(
        notices().contains("hook output is not JSON"),
        "{}",
        notices()
    );
}
