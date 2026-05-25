use super::Config;
use thiserror::Error;

#[cfg(test)]
use super::{Settings, Scopes, Bundle, NetworkScope, NetworkMatch, HostScope, HostMatch, UserScope, UserMatch, ProjectScope, ProjectMatch};

#[derive(Debug, Error)]
pub enum ValidateError {
    #[error("duplicate scope id: {0}")]
    DuplicateScopeId(String),
    #[error("bundle {0} has no tags")]
    BundleNoTags(String),
    #[error("duplicate bundle name: {0}")]
    DuplicateBundleName(String),
}

impl Config {
    pub fn validate(&self) -> Result<(), ValidateError> {
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
        let mut seen_bundle_names = std::collections::HashSet::new();
        for b in &self.bundle {
            if b.tags.is_empty() {
                return Err(ValidateError::BundleNoTags(b.name.clone()));
            }
            if !seen_bundle_names.insert(&b.name) {
                return Err(ValidateError::DuplicateBundleName(b.name.clone()));
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

    fn arb_network_scope() -> impl Strategy<Value = NetworkScope> {
        (arb_string(), prop::option::of(arb_string()), prop::option::of(arb_string()), prop::option::of(arb_string()))
            .prop_map(|(id, gateway_mac, ssid, cidr)| NetworkScope {
                id,
                r#match: NetworkMatch { gateway_mac, ssid, cidr },
                tags: vec![],
            })
    }

    fn arb_host_scope() -> impl Strategy<Value = HostScope> {
        (arb_string(), prop::option::of(arb_string()))
            .prop_map(|(id, hostname)| HostScope {
                id,
                r#match: HostMatch { hostname },
                tags: vec![],
            })
    }

    fn arb_user_scope() -> impl Strategy<Value = UserScope> {
        (arb_string(), prop::option::of(arb_string()))
            .prop_map(|(id, user)| UserScope {
                id,
                r#match: UserMatch { user },
                tags: vec![],
            })
    }

    fn arb_project_scope() -> impl Strategy<Value = ProjectScope> {
        (arb_string(), prop::option::of(arb_string()), prop::option::of(arb_string()))
            .prop_map(|(id, path_prefix, marker_file)| ProjectScope {
                id,
                r#match: ProjectMatch { path_prefix, marker_file },
                tags: vec![],
            })
    }

    fn arb_bundle() -> impl Strategy<Value = Bundle> {
        (arb_string(), prop::collection::vec(arb_string(), 1..5))
            .prop_map(|(name, tags)| Bundle { name, tags })
    }

    fn arb_config() -> impl Strategy<Value = Config> {
        (
            Just(Settings::default()),
            (
                prop::collection::vec(arb_network_scope(), 0..3),
                prop::collection::vec(arb_host_scope(), 0..3),
                prop::collection::vec(arb_user_scope(), 0..3),
                prop::collection::vec(arb_project_scope(), 0..3),
            ),
            prop::collection::vec(arb_bundle(), 0..3),
            Just(None),
        )
            .prop_map(|(settings, (network, host, user, project), bundle, icm)| Config {
                settings,
                scope: Scopes { network, host, user, project },
                bundle,
                icm,
            })
    }

    proptest! {
        #[test]
        fn prop_config_toml_roundtrip(config in arb_config()) {
            prop_assume!(config.validate().is_ok(), "generated config must be valid");
            let toml_str = toml::to_string(&config).expect("serialize to TOML");
            let deserialized: Config = toml::from_str(&toml_str).expect("deserialize from TOML");
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
                .map(|name| Bundle { name: name.clone(), tags: vec!["tag1".to_string()] })
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
                    Bundle { name: name.clone(), tags: vec!["tag1".to_string()] },
                    Bundle { name, tags: vec!["tag2".to_string()] },
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
            }],
            icm: None,
        };
        assert!(config.validate().is_ok());
    }
}
