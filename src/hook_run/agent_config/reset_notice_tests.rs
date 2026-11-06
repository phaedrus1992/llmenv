//! Agent config tests that read the notice text or the stored file.

#![expect(clippy::unwrap_used, reason = "test code")]

use super::{DIR, write_session_start};

#[test]
fn a_session_start_over_a_corrupt_config_tells_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join(DIR);
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("s1.json"), "not json").unwrap();
    write_session_start(dir.path(), "s1", super::tests::doc());
    let text = crate::agent_notice::current().unwrap_or_default();
    assert!(
        text.contains("agent config could not be read"),
        "got: {text}"
    );
}

/// Each strict prefix of a saved config, the empty file included, is the shape a crash leaves. It
/// reads as unreadable, `load` gives `None`, and the agent is told that the config was reset (#3130).
#[test]
fn every_cut_off_config_reads_as_a_reset() {
    let dir = tempfile::tempdir().unwrap();
    write_session_start(dir.path(), "s1", super::tests::doc());
    let file = dir.path().join(DIR).join("s1.json");
    let saved = std::fs::read(&file).unwrap();
    assert!(super::load(dir.path(), "s1").is_some());
    for len in 0..saved.len() {
        std::fs::write(&file, &saved[..len]).unwrap();
        assert!(
            super::load(dir.path(), "s1").is_none(),
            "prefix of {len} bytes"
        );
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(
            text.contains("could not read the agent config"),
            "{len}: {text}"
        );
    }
}
