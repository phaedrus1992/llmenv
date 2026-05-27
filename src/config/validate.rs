use super::Config;
use thiserror::Error;

#[cfg(test)]
use super::{
    Bundle, HostMatch, HostScope, Icm, NetworkMatch, NetworkScope, ProjectMatch, ProjectScope,
    Scopes, Settings, UserMatch, UserScope,
};

#[derive(Debug, Error)]
pub enum ValidateError {
    #[error("duplicate scope id: {0}")]
    DuplicateScopeId(String),
    #[error("bundle {0} has no tags")]
    BundleNoTags(String),
    #[error("duplicate bundle name: {0}")]
    DuplicateBundleName(String),
    #[error("invalid CIDR notation: {0}")]
    InvalidCIDR(String),
    #[error("invalid MAC address: {0}")]
    InvalidMACAddress(String),
    #[error("invalid hostname: {0}")]
    InvalidHostname(String),
    #[error("invalid path prefix: {0}")]
    InvalidPathPrefix(String),
    #[error("bundle {0}: invalid variable name '{1}' (must match [A-Za-z_][A-Za-z0-9_]*)")]
    InvalidVarName(String, String),
    #[error("cache_dir contains path traversal components: {0}")]
    CacheDirTraversal(String),
    #[error("cache_retention_hours must be > 0")]
    CacheRetentionInvalid,
}

fn is_valid_cidr(cidr: &str) -> bool {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return false;
    }
    let octets: Vec<&str> = parts[0].split('.').collect();
    if octets.len() != 4 {
        return false;
    }
    for octet in octets {
        // Reject leading zeros ("01") which u8::parse would otherwise accept;
        // RFC 4632 dotted-decimal forbids them and they invite octal confusion.
        if (octet.len() > 1 && octet.starts_with('0')) || octet.parse::<u8>().is_err() {
            return false;
        }
    }
    matches!(parts[1].parse::<u8>(), Ok(n) if n <= 32)
}

fn is_valid_mac_address(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return false;
    }
    parts
        .iter()
        .all(|part| part.len() == 2 && u8::from_str_radix(part, 16).is_ok())
}

fn is_valid_hostname(hostname: &str) -> bool {
    // RFC 1123 §2.1 / RFC 952: total length <= 253 octets, each label
    // 1..=63 octets, labels are alphanumeric plus interior hyphens.
    if hostname.is_empty() || hostname.len() > 253 {
        return false;
    }
    hostname.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

fn is_valid_path_prefix(path: &str) -> bool {
    if path.is_empty() || path.len() > 4096 {
        return false;
    }
    // Parse components rather than substring-match so `foo/..` (no trailing
    // slash) and host-OS separators are caught, matching is_safe_cache_dir.
    !path.contains('\0') && !crate::paths::has_parent_component(path)
}

fn is_valid_var_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let first = name.chars().next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    name.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_safe_cache_dir(dir: &str) -> bool {
    if dir.is_empty() || dir.len() > 4096 {
        return false;
    }
    // Parse components rather than substring-match so traversal can't slip
    // through as `foo/..` (no trailing slash) or via host-OS separators.
    !dir.contains('\0') && !crate::paths::has_parent_component(dir)
}

impl Config {
    pub fn validate(&self) -> Result<(), ValidateError> {
        if !is_safe_cache_dir(&self.settings.cache_dir) {
            return Err(ValidateError::CacheDirTraversal(
                self.settings.cache_dir.clone(),
            ));
        }
        if let Some(hours) = self.settings.cache_retention_hours
            && hours == 0
        {
            return Err(ValidateError::CacheRetentionInvalid);
        }
        let mut seen_scope_ids = std::collections::HashSet::new();
        let ids = self
            .scope
            .network
            .iter()
            .map(|s| &s.id)
            .chain(self.scope.host.iter().map(|s| &s.id))
            .chain(self.scope.user.iter().map(|s| &s.id))
            .chain(self.scope.project.iter().map(|s| &s.id));
        for id in ids {
            if !seen_scope_ids.insert(id) {
                return Err(ValidateError::DuplicateScopeId(id.clone()));
            }
        }
        for scope in &self.scope.network {
            if let Some(cidr) = &scope.r#match.cidr
                && !is_valid_cidr(cidr)
            {
                return Err(ValidateError::InvalidCIDR(cidr.clone()));
            }
            if let Some(mac) = &scope.r#match.gateway_mac
                && !is_valid_mac_address(mac)
            {
                return Err(ValidateError::InvalidMACAddress(mac.clone()));
            }
        }
        for scope in &self.scope.host {
            if let Some(hostname) = &scope.r#match.hostname
                && !is_valid_hostname(hostname)
            {
                return Err(ValidateError::InvalidHostname(hostname.clone()));
            }
        }
        for scope in &self.scope.project {
            if let Some(path) = &scope.r#match.path_prefix
                && !is_valid_path_prefix(path)
            {
                return Err(ValidateError::InvalidPathPrefix(path.clone()));
            }
        }
        let mut seen_bundle_names = std::collections::HashSet::new();
        for b in &self.bundle {
            if b.tags.is_empty() {
                return Err(ValidateError::BundleNoTags(b.name.clone()));
            }
            if !seen_bundle_names.insert(&b.name) {
                return Err(ValidateError::DuplicateBundleName(b.name.clone()));
            }
            for var_name in b.vars.keys() {
                if !is_valid_var_name(var_name) {
                    return Err(ValidateError::InvalidVarName(
                        b.name.clone(),
                        var_name.clone(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_string() -> impl Strategy<Value = String> {
        r"[a-zA-Z0-9_-]{1,20}"
    }

    // Some(arb)/None so the round-trip exercises both branches of every
    // Option<String> match field rather than only the None default.
    fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
        prop::option::of(arb_string())
    }

    fn arb_settings() -> impl Strategy<Value = Settings> {
        (arb_string(), 0u64..120, prop::option::of(0u64..10_000)).prop_map(
            |(cache_dir, sync_interval_minutes, cache_retention_hours)| Settings {
                cache_dir,
                sync_interval_minutes,
                cache_retention_hours,
            },
        )
    }

    fn arb_icm() -> impl Strategy<Value = Option<Icm>> {
        prop::option::of(
            (
                arb_string(),
                arb_string(),
                arb_string(),
                prop::collection::vec(arb_string(), 0..3),
            )
                .prop_map(|(server_tag, server_bind, client_url, default_topics)| Icm {
                    server_tag,
                    server_bind,
                    client_url,
                    default_topics,
                }),
        )
    }

    fn arb_config() -> impl Strategy<Value = Config> {
        (
            arb_settings(),
            prop::collection::vec((arb_string(), arb_opt_string(), arb_opt_string(), arb_opt_string()), 0..10).prop_map(|ids| {
                let network = ids
                    .iter()
                    .take(2)
                    .map(|(id, gateway_mac, ssid, cidr)| NetworkScope {
                        id: id.clone(),
                        r#match: NetworkMatch {
                            gateway_mac: gateway_mac.clone(),
                            ssid: ssid.clone(),
                            cidr: cidr.clone(),
                        },
                        tags: vec![],
                    })
                    .collect();
                let host = ids
                    .iter()
                    .skip(2)
                    .take(2)
                    .map(|(id, hostname, _, _)| HostScope {
                        id: id.clone(),
                        r#match: HostMatch {
                            hostname: hostname.clone(),
                        },
                        tags: vec![],
                    })
                    .collect();
                let user = ids
                    .iter()
                    .skip(4)
                    .take(2)
                    .map(|(id, user, _, _)| UserScope {
                        id: id.clone(),
                        r#match: UserMatch { user: user.clone() },
                        tags: vec![],
                    })
                    .collect();
                let project = ids
                    .iter()
                    .skip(6)
                    .take(2)
                    .map(|(id, path_prefix, marker, _)| ProjectScope {
                        id: id.clone(),
                        r#match: ProjectMatch {
                            path_prefix: path_prefix.clone(),
                            marker: marker.clone(),
                        },
                        tags: vec![],
                    })
                    .collect();
                (network, host, user, project)
            }),
            prop::collection::vec(
                (arb_string(), prop::collection::vec(arb_string(), 1..3)),
                0..3,
            )
            .prop_map(|bundles| {
                bundles
                    .into_iter()
                    .enumerate()
                    .map(|(i, (name, tags))| Bundle {
                        name: format!("bundle-{}-{}", i, name),
                        tags,
                        vars: Default::default(),
                    })
                    .collect()
            }),
            arb_icm(),
        )
            .prop_map(
                |(settings, (network, host, user, project), bundle, icm)| Config {
                    settings,
                    scope: Scopes {
                        network,
                        host,
                        user,
                        project,
                    },
                    bundle,
                    icm,
                },
            )
    }

    proptest! {
        #[test]
        fn prop_config_yaml_roundtrip(config in arb_config()) {
            let yaml_str = serde_yaml::to_string(&config).expect("serialize failed");
            let deserialized: Config = serde_yaml::from_str(&yaml_str).expect("deserialize failed");
            prop_assert_eq!(config, deserialized, "roundtrip should preserve config");
        }

        #[test]
        fn prop_config_validate_enforces_unique_scope_ids(
            id in arb_string(),
        ) {
            let network = vec![
                NetworkScope {
                    id: id.clone(),
                    r#match: NetworkMatch { gateway_mac: None, ssid: None, cidr: None },
                    tags: vec![],
                },
                NetworkScope {
                    id, // Duplicate ID
                    r#match: NetworkMatch { gateway_mac: None, ssid: None, cidr: None },
                    tags: vec![],
                },
            ];

            let config = Config {
                settings: Settings::default(),
                scope: Scopes { network, host: vec![], user: vec![], project: vec![] },
                bundle: vec![],
                icm: None,
            };
            prop_assert!(
                config.validate().is_err(),
                "config with duplicate scope IDs should fail validation"
            );
        }

        #[test]
        fn prop_config_validate_enforces_bundle_tags(
            names in prop::collection::vec(arb_string(), 1..3)
        ) {
            let mut bundles = names.iter()
                .map(|name| Bundle { name: name.clone(), tags: vec!["tag1".to_string()], vars: Default::default() })
                .collect::<Vec<_>>();
            if !bundles.is_empty() {
                bundles[0].tags.clear();
            }
            let config = Config {
                settings: Settings::default(),
                scope: Scopes::default(),
                bundle: bundles,
                icm: None,
            };
            prop_assert!(
                config.validate().is_err(),
                "config with empty bundle tags should fail validation"
            );
        }

        #[test]
        fn prop_config_validate_enforces_unique_bundle_names(
            name in arb_string(),
        ) {
            let config = Config {
                settings: Settings::default(),
                scope: Scopes::default(),
                bundle: vec![
                    Bundle { name: name.clone(), tags: vec!["tag1".to_string()], vars: Default::default() },
                    Bundle { name, tags: vec!["tag2".to_string()], vars: Default::default() },
                ],
                icm: None,
            };
            prop_assert!(
                config.validate().is_err(),
                "config with duplicate bundle names should fail validation"
            );
        }
    }

    #[test]
    fn test_valid_config_passes_validation() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: Some("aa:bb:cc:dd:ee:ff".to_string()),
                        ssid: None,
                        cidr: None,
                    },
                    tags: vec![],
                }],
                host: vec![],
                user: vec![],
                project: vec![],
            },
            bundle: vec![Bundle {
                name: "test-bundle".to_string(),
                tags: vec!["prod".to_string()],
                vars: Default::default(),
            }],
            icm: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_invalid_cidr_prefix_too_large() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: None,
                        ssid: None,
                        cidr: Some("192.168.1.0/33".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                }],
                host: vec![],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_cidr_malformed() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: None,
                        ssid: None,
                        cidr: Some("256.256.256.256/24".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                }],
                host: vec![],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_mac_incomplete() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: Some("aa:bb:cc:dd:ee".to_string()),
                        ssid: None,
                        cidr: None,
                    },
                    tags: vec!["tag1".to_string()],
                }],
                host: vec![],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_mac_invalid_hex() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: Some("zz:bb:cc:dd:ee:ff".to_string()),
                        ssid: None,
                        cidr: None,
                    },
                    tags: vec!["tag1".to_string()],
                }],
                host: vec![],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_starts_with_hyphen() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some("-invalid.local".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                }],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_ends_with_hyphen() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some("invalid-".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                }],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_double_dot() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some("invalid..local".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                }],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_label_too_long() {
        // RFC 1123: a single label may not exceed 63 octets.
        let long_label = "a".repeat(64);
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some(format!("{long_label}.example.com")),
                    },
                    tags: vec!["tag1".to_string()],
                }],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_cidr_leading_zero_octet() {
        // Dotted-decimal forbids leading zeros ("01") even though they parse.
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: None,
                        ssid: None,
                        cidr: Some("01.168.1.0/24".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                }],
                host: vec![],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_label_ends_with_hyphen() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some("foo-.example.com".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                }],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_label_starts_with_hyphen() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some("foo.-example.com".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                }],
                user: vec![],
                project: vec![],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_path_with_traversal() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![],
                host: vec![],
                user: vec![],
                project: vec![ProjectScope {
                    id: "proj1".to_string(),
                    r#match: ProjectMatch {
                        path_prefix: Some("/foo/../bar".to_string()),
                        marker: None,
                    },
                    tags: vec!["tag1".to_string()],
                }],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_path_prefix_trailing_parent_no_slash() {
        // `foo/..` has no "../" substring but is a real traversal — semantic
        // parsing must reject it (variant of #65, found in pre-pr-review).
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![],
                host: vec![],
                user: vec![],
                project: vec![ProjectScope {
                    id: "proj1".to_string(),
                    r#match: ProjectMatch {
                        path_prefix: Some("/foo/bar/..".to_string()),
                        marker: None,
                    },
                    tags: vec!["tag1".to_string()],
                }],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_path_with_null_byte() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes {
                network: vec![],
                host: vec![],
                user: vec![],
                project: vec![ProjectScope {
                    id: "proj1".to_string(),
                    r#match: ProjectMatch {
                        path_prefix: Some("/foo\0bar".to_string()),
                        marker: None,
                    },
                    tags: vec!["tag1".to_string()],
                }],
            },
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_var_name_starts_with_digit() {
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("123var".to_string(), "value".to_string());
        let config = Config {
            settings: Settings::default(),
            scope: Scopes::default(),
            bundle: vec![Bundle {
                name: "test".to_string(),
                tags: vec!["tag1".to_string()],
                vars,
            }],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_var_name_contains_hyphen() {
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("my-var".to_string(), "value".to_string());
        let config = Config {
            settings: Settings::default(),
            scope: Scopes::default(),
            bundle: vec![Bundle {
                name: "test".to_string(),
                tags: vec!["tag1".to_string()],
                vars,
            }],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_valid_var_names() {
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("MY_VAR".to_string(), "value1".to_string());
        vars.insert("_private".to_string(), "value2".to_string());
        vars.insert("var123".to_string(), "value3".to_string());
        let config = Config {
            settings: Settings::default(),
            scope: Scopes::default(),
            bundle: vec![Bundle {
                name: "test".to_string(),
                tags: vec!["tag1".to_string()],
                vars,
            }],
            icm: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cache_dir_with_traversal() {
        let config = Config {
            settings: Settings {
                cache_dir: "~/.cache/../../../etc/passwd".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: Some(168),
            },
            scope: Scopes::default(),
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_dir_trailing_parent_no_slash() {
        // `foo/..` has no "../" or "/.." substring on the right side but is a
        // real traversal — semantic parsing (#65) must reject it.
        let config = Config {
            settings: Settings {
                cache_dir: "~/.cache/llmenv/..".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: Some(168),
            },
            scope: Scopes::default(),
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_dir_with_null_byte() {
        let config = Config {
            settings: Settings {
                cache_dir: "~/.cache/llm\0env".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: Some(168),
            },
            scope: Scopes::default(),
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_dir_valid() {
        let config = Config {
            settings: Settings::default(),
            scope: Scopes::default(),
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cache_retention_zero() {
        let config = Config {
            settings: Settings {
                cache_dir: "~/.cache/llmenv".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: Some(0),
            },
            scope: Scopes::default(),
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_retention_valid() {
        let config = Config {
            settings: Settings {
                cache_dir: "~/.cache/llmenv".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: Some(168),
            },
            scope: Scopes::default(),
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cache_retention_none() {
        let config = Config {
            settings: Settings {
                cache_dir: "~/.cache/llmenv".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: None,
            },
            scope: Scopes::default(),
            bundle: vec![],
            icm: None,
        };
        assert!(config.validate().is_ok());
    }

    // ===== Property tests against the real validators =====
    //
    // These call the private is_valid_* functions directly (rather than
    // re-implementing the rules in an external integration test) so a change to
    // the validators is caught here instead of silently diverging from a copy.

    // RFC 1123 hostname: 1..=63-octet labels, alnum + interior hyphens, total <= 253.
    fn rfc1123_label() -> impl Strategy<Value = String> {
        prop::string::string_regex("[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?")
            .expect("valid label regex")
    }

    fn valid_hostname() -> impl Strategy<Value = String> {
        prop::collection::vec(rfc1123_label(), 1..4)
            .prop_map(|labels| labels.join("."))
            .prop_filter("total length <= 253", |h| h.len() <= 253)
    }

    fn valid_cidr() -> impl Strategy<Value = String> {
        (0u8..=255, 0u8..=255, 0u8..=255, 0u8..=255, 0u8..=32)
            .prop_map(|(a, b, c, d, m)| format!("{a}.{b}.{c}.{d}/{m}"))
    }

    fn valid_var_name() -> impl Strategy<Value = String> {
        prop::string::string_regex("[A-Za-z_][A-Za-z0-9_]*").expect("valid var name regex")
    }

    proptest! {
        #[test]
        fn prop_valid_hostnames_accepted(h in valid_hostname()) {
            prop_assert!(is_valid_hostname(&h), "RFC 1123 hostname rejected: {h:?}");
        }

        #[test]
        fn prop_label_over_63_octets_rejected(
            prefix in rfc1123_label(),
            extra in 0usize..40,
        ) {
            // Build a single label of 64..=63+40 octets; must be rejected even
            // though it is otherwise alphanumeric.
            let label = "a".repeat(64 + extra);
            prop_assert!(!is_valid_hostname(&label), "64+ octet label accepted");
            // The valid prefix alone must still pass, proving it's the length
            // that's rejected, not the characters.
            prop_assert!(is_valid_hostname(&prefix));
        }

        #[test]
        fn prop_hostname_with_underscore_rejected(
            a in rfc1123_label(),
            b in rfc1123_label(),
        ) {
            let h = format!("{a}_{b}");
            prop_assert!(!is_valid_hostname(&h), "underscore accepted in hostname: {h:?}");
        }

        #[test]
        fn prop_valid_cidrs_accepted(c in valid_cidr()) {
            prop_assert!(is_valid_cidr(&c), "valid CIDR rejected: {c}");
        }

        #[test]
        fn prop_cidr_prefix_over_32_rejected(
            a in 0u8..=255, b in 0u8..=255, c in 0u8..=255, d in 0u8..=255,
            m in 33u16..=255,
        ) {
            let cidr = format!("{a}.{b}.{c}.{d}/{m}");
            prop_assert!(!is_valid_cidr(&cidr), "prefix >32 accepted: {cidr}");
        }

        #[test]
        fn prop_cidr_leading_zero_octet_rejected(
            b in 0u8..=255, c in 0u8..=255, d in 0u8..=255, m in 0u8..=32,
        ) {
            // "01" is forbidden dotted-decimal even though u8 parse accepts it.
            let cidr = format!("01.{b}.{c}.{d}/{m}");
            prop_assert!(!is_valid_cidr(&cidr), "leading-zero octet accepted: {cidr}");
        }

        #[test]
        fn prop_valid_var_names_accepted(name in valid_var_name()) {
            prop_assert!(is_valid_var_name(&name), "valid var name rejected: {name}");
        }

        #[test]
        fn prop_var_name_leading_digit_rejected(
            d in 0u8..=9,
            rest in "[A-Za-z0-9_]{0,10}",
        ) {
            let name = format!("{d}{rest}");
            prop_assert!(!is_valid_var_name(&name), "leading-digit var name accepted: {name}");
        }

        #[test]
        fn prop_path_prefix_with_null_byte_rejected(
            before in "[a-z0-9/_-]{0,20}",
            after in "[a-z0-9/_-]{0,20}",
        ) {
            let path = format!("{before}\0{after}");
            prop_assert!(!is_valid_path_prefix(&path), "null byte accepted in path");
        }

        #[test]
        fn prop_path_prefix_with_parent_component_rejected(
            before in "[a-z0-9_-]{1,10}",
            after in "[a-z0-9_-]{1,10}",
        ) {
            let path = format!("{before}/../{after}");
            prop_assert!(!is_valid_path_prefix(&path), "parent component accepted: {path}");
        }

        #[test]
        fn prop_valid_mac_addresses_accepted(octets in prop::array::uniform6(0u8..=255)) {
            let mac = octets
                .iter()
                .map(|o| format!("{o:02x}"))
                .collect::<Vec<_>>()
                .join(":");
            prop_assert!(is_valid_mac_address(&mac), "valid MAC rejected: {mac}");
        }

        #[test]
        fn prop_mac_wrong_group_count_rejected(count in prop_oneof![0usize..6, 7usize..12]) {
            let mac = vec!["aa"; count].join(":");
            prop_assert!(!is_valid_mac_address(&mac), "MAC with {count} groups accepted");
        }

        #[test]
        fn prop_mac_non_hex_rejected(
            pos in 0usize..6,
            bad in "[g-zG-Z]{2}",
        ) {
            let mut octets = vec!["aa".to_string(); 6];
            octets[pos] = bad;
            let mac = octets.join(":");
            prop_assert!(!is_valid_mac_address(&mac), "non-hex MAC accepted: {mac}");
        }

        #[test]
        fn prop_cache_dir_with_parent_component_rejected(
            before in "[a-z0-9_-]{1,10}",
            after in "[a-z0-9_-]{1,10}",
        ) {
            let dir = format!("{before}/../{after}");
            prop_assert!(!is_safe_cache_dir(&dir), "parent component accepted: {dir}");
        }

        #[test]
        fn prop_cache_dir_with_null_byte_rejected(
            before in "[a-z0-9/_-]{0,20}",
            after in "[a-z0-9/_-]{0,20}",
        ) {
            let dir = format!("{before}\0{after}");
            prop_assert!(!is_safe_cache_dir(&dir), "null byte accepted in cache dir");
        }

        #[test]
        fn prop_cache_dir_over_max_length_rejected(len in 4097usize..5000) {
            let dir = "a".repeat(len);
            prop_assert!(!is_safe_cache_dir(&dir), "over-length cache dir accepted");
        }
    }
}
