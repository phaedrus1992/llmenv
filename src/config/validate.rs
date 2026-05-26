use super::Config;
use thiserror::Error;

#[cfg(test)]
use super::{
    Bundle, HostMatch, HostScope, NetworkMatch, NetworkScope, ProjectMatch, ProjectScope, Scopes,
    Settings, UserMatch, UserScope,
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
        match octet.parse::<u16>() {
            Ok(n) if n <= 255 => {}
            _ => return false,
        }
    }
    matches!(parts[1].parse::<u32>(), Ok(n) if n <= 32)
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
    if hostname.is_empty() || hostname.len() > 253 {
        return false;
    }
    hostname
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !hostname.starts_with('-')
        && !hostname.ends_with('-')
        && !hostname.contains("..")
        && hostname
            .split('.')
            .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'))
}

fn is_valid_path_prefix(path: &str) -> bool {
    if path.is_empty() || path.len() > 4096 {
        return false;
    }
    !path.contains('\0') && !path.contains("../") && !path.contains("/..\\")
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
    !dir.contains('\0') && !dir.contains("../") && !dir.contains("/..")
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

    fn arb_config() -> impl Strategy<Value = Config> {
        (
            Just(Settings::default()),
            prop::collection::vec(arb_string(), 0..10).prop_map(|ids| {
                let network = ids
                    .iter()
                    .take(2)
                    .map(|id| NetworkScope {
                        id: id.clone(),
                        r#match: NetworkMatch {
                            gateway_mac: None,
                            ssid: None,
                            cidr: None,
                        },
                        tags: vec![],
                    })
                    .collect();
                let host = ids
                    .iter()
                    .skip(2)
                    .take(2)
                    .map(|id| HostScope {
                        id: id.clone(),
                        r#match: HostMatch { hostname: None },
                        tags: vec![],
                    })
                    .collect();
                let user = ids
                    .iter()
                    .skip(4)
                    .take(2)
                    .map(|id| UserScope {
                        id: id.clone(),
                        r#match: UserMatch { user: None },
                        tags: vec![],
                    })
                    .collect();
                let project = ids
                    .iter()
                    .skip(6)
                    .take(2)
                    .map(|id| ProjectScope {
                        id: id.clone(),
                        r#match: ProjectMatch {
                            path_prefix: None,
                            marker: None,
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
            Just(None),
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
}
