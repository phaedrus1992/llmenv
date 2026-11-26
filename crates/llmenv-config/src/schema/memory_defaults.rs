//! The defaults of a `features.memory` entry, shared by serde and `Memory::default`.

use super::{Memory, default_listen_host};

/// The topics that hold facts for every project, as the ICM memory conventions name them.
pub(super) fn default_global_topics() -> Vec<String> {
    vec!["preferences".to_string(), "errors-resolved".to_string()]
}

pub(super) fn default_adaptive_recall() -> bool {
    true
}

pub(super) fn default_failure_recall() -> bool {
    true
}

/// Manual, not derived (#1044): `listen_host`'s real default is
/// `default_listen_host()` (`"127.0.0.1"`), not `String::default()`
/// (`""`) — a bare `#[derive(Default)]` would silently diverge from the
/// `#[serde(default = "default_listen_host")]` value every other
/// construction path (deserialization) actually uses.
impl Default for Memory {
    fn default() -> Self {
        Self {
            server_host: String::new(),
            port: 0,
            listen_host: default_listen_host(),
            when: Vec::new(),
            default_topics: Vec::new(),
            global_topics: default_global_topics(),
            default_type: None,
            default_importance: None,
            type_importance: std::collections::BTreeMap::new(),
            consolidation: None,
            retention: None,
            auto_prune: false,
            mcp_permissions: None,
            wakeup_max_tokens: None,
            adaptive_recall: default_adaptive_recall(),
            failure_recall: default_failure_recall(),
            always_load: None,
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    /// An entry that names no `global_topics` or `adaptive_recall` gets the same defaults as
    /// `Memory::default`, so a parsed and a built entry agree.
    #[test]
    fn a_parsed_entry_and_the_default_share_the_defaults() {
        let parsed: Memory = serde_yaml::from_str("server_host: h\nport: 9092\n").unwrap();
        assert_eq!(parsed.global_topics, ["preferences", "errors-resolved"]);
        assert!(parsed.adaptive_recall);
        assert_eq!(parsed.listen_host, "127.0.0.1");
        let built = Memory::default();
        assert_eq!(built.global_topics, parsed.global_topics);
        assert_eq!(built.adaptive_recall, parsed.adaptive_recall);
        assert!(parsed.failure_recall);
        assert_eq!(built.failure_recall, parsed.failure_recall);
        assert_eq!(built.listen_host, parsed.listen_host);
    }

    #[test]
    fn failure_recall_accepts_false() {
        let off: Memory =
            serde_yaml::from_str("server_host: h\nport: 9092\nfailure_recall: false\n").unwrap();
        assert!(!off.failure_recall);
        assert!(off.adaptive_recall);
    }
}
