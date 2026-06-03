#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use llmenv::config::{Config, Scopes};
use llmenv::scope::Env;
use proptest::prelude::*;

// ===== Scope Evaluation Determinism =====

#[test]
fn prop_scope_evaluation_is_deterministic() {
    proptest!(|(hostname in "[a-z0-9]{1,20}")| {
            let cfg = Config {
                scope: Scopes {
                    host: vec![
                        llmenv::config::HostScope {
                            id: "h1".into(),
                            r#match: llmenv::config::HostMatch {
                                hostname: Some(hostname.clone()),
                            },
                            tags: vec!["tag1".into()],
                        env: Default::default(),
    },
                        llmenv::config::HostScope {
                            id: "h2".into(),
                            r#match: llmenv::config::HostMatch {
                                hostname: None,
                            },
                            tags: vec!["tag2".into()],
                        env: Default::default(),
    },
                    ],
                    ..Default::default()
                },
                ..Default::default()
            };

            let env = Env {
                hostname: hostname.clone(),
                user: "testuser".into(),
                cwd: "/tmp".into(),
                ..Env::empty()
            };

            let result1 = llmenv::scope::evaluate(&cfg, &env);
            let result2 = llmenv::scope::evaluate(&cfg, &env);

            assert_eq!(result1.scopes.len(), result2.scopes.len());
            assert_eq!(result1.tags.len(), result2.tags.len());

            for (s1, s2) in result1.scopes.iter().zip(result2.scopes.iter()) {
                assert_eq!(s1.id, s2.id);
                assert_eq!(s1.tags, s2.tags);
            }
        });
}

// ===== Multiple Scope Matches Accumulate Tags =====

#[test]
fn prop_multiple_scopes_accumulate_tags() {
    proptest!(|(
            hostname in "[a-z0-9]{1,15}",
            user in "[a-z]{1,10}"
        )| {
            let cfg = Config {
                scope: Scopes {
                    host: vec![llmenv::config::HostScope {
                        id: "h".into(),
                        r#match: llmenv::config::HostMatch {
                            hostname: Some(hostname.clone()),
                        },
                        tags: vec!["host_tag".into()],
                    env: Default::default(),
    }],
                    user: vec![llmenv::config::UserScope {
                        id: "u".into(),
                        r#match: llmenv::config::UserMatch {
                            user: Some(user.clone()),
                        },
                        tags: vec!["user_tag".into()],
                    env: Default::default(),
    }],
                    ..Default::default()
                },
                ..Default::default()
            };

            let env = Env {
                hostname: hostname.clone(),
                user: user.clone(),
                cwd: "/tmp".into(),
                ..Env::empty()
            };

            let active = llmenv::scope::evaluate(&cfg, &env);

            assert_eq!(active.scopes.len(), 2);
            assert!(active.tags.contains("host_tag"));
            assert!(active.tags.contains("user_tag"));
        });
}

// ===== Scope Order Independence =====

#[test]
fn prop_scope_matching_order_independent() {
    proptest!(|(hostname in "[a-z0-9]{1,15}")| {
            let scope1 = llmenv::config::HostScope {
                id: "first".into(),
                r#match: llmenv::config::HostMatch {
                    hostname: Some(hostname.clone()),
                },
                tags: vec!["tag1".into()],
            env: Default::default(),
    };

            let scope2 = llmenv::config::HostScope {
                id: "second".into(),
                r#match: llmenv::config::HostMatch {
                    hostname: None,
                },
                tags: vec!["tag2".into()],
            env: Default::default(),
    };

            let cfg1 = Config {
                scope: Scopes {
                    host: vec![scope1.clone(), scope2.clone()],
                    ..Default::default()
                },
                ..Default::default()
            };

            let cfg2 = Config {
                scope: Scopes {
                    host: vec![scope2.clone(), scope1.clone()],
                    ..Default::default()
                },
                ..Default::default()
            };

            let env = Env {
                hostname: hostname.clone(),
                user: "testuser".into(),
                cwd: "/tmp".into(),
                ..Env::empty()
            };

            let result1 = llmenv::scope::evaluate(&cfg1, &env);
            let result2 = llmenv::scope::evaluate(&cfg2, &env);

            let ids1: std::collections::HashSet<_> =
                result1.scopes.iter().map(|s| s.id.as_str()).collect();
            let ids2: std::collections::HashSet<_> =
                result2.scopes.iter().map(|s| s.id.as_str()).collect();

            assert_eq!(ids1, ids2);
        });
}
