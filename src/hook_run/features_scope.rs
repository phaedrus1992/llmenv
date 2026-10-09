//! Resolve the scalar `features` for the hook runtime the way the adapter does (#2460).
//!
//! The adapter reads each feature from the merged manifest: the root `features` block wins, then
//! the highest-precedence bundle that sets it. The hook runtime read the root block only, so a
//! feature that a tag-scoped bundle enabled registered its hooks and then ran them switched off.
//! The list features (`memory`, `throttle`, `codebase_memory`) and the `context_mode` and
//! `upgrade` settings are not read by hook handlers, so they are not resolved here.

use std::path::Path;

use crate::config::{Capabilities, Config, Features};
use crate::merge::{BundleRef, CapabilityContributor, merge_capabilities};
use crate::scope::ActiveScopes;

/// The root config outranks every bundle, as in `merge::merge`.
const TOP_LEVEL_PRECEDENCE: u8 = u8::MAX;

/// Return `config` with the hook-read features resolved from the active bundles.
///
/// A feature set in the root `features` block is final. With no bundle configured, this reads no
/// bundle file and runs no scope detection.
pub(crate) fn resolve(config: Config, config_path: &Path) -> Config {
    if config.bundle.is_empty() {
        return config;
    }
    let Some(config_dir) = config_path.parent() else {
        tracing::warn!(
            "features not resolved from bundles: config path {} has no parent directory",
            config_path.display()
        );
        return config;
    };
    let env = match crate::scope::matcher::Env::detect_for_config(&config) {
        Ok(env) => env,
        Err(e) => {
            tracing::error!(
                "features not resolved from bundles: {e:#}. Fix the environment, or unset the \
                 variable."
            );
            return config;
        }
    };
    let active = crate::scope::evaluate(&config, &env);
    resolve_with(config, config_dir, &active)
}

fn resolve_with(mut config: Config, config_dir: &Path, active: &ActiveScopes) -> Config {
    let firing = crate::cli::firing_bundles(&config.bundle, active, None);
    let refs = crate::cli::build_bundle_refs(config_dir, active, &firing);
    match bundle_features(&config.capabilities, &refs) {
        Ok(Some(from_bundles)) => overlay(config.features.get_or_insert_default(), from_bundles),
        Ok(None) => {}
        Err(e) => tracing::error!(
            "features not resolved from bundles in {}: {e:#}. Fix the bundle.yaml, or set the \
             feature in the root `features:` block of config.yaml",
            config_dir.display()
        ),
    }
    config
}

/// Fill each unset feature of `root` from `bundles`. The root value wins.
fn overlay(root: &mut Features, bundles: Features) {
    root.read_once = root.read_once.take().or(bundles.read_once);
    root.repeat_detect = root.repeat_detect.take().or(bundles.repeat_detect);
    root.slippage = root.slippage.take().or(bundles.slippage);
    root.task_tracker = root.task_tracker.take().or(bundles.task_tracker);
    root.cd_guard = root.cd_guard.take().or(bundles.cd_guard);
}

/// The hook-read features of one capability fragment, or `None` when it sets none of them.
fn hook_features(caps: &Capabilities) -> Option<Features> {
    let f = caps.features.as_ref()?;
    let cut = Features {
        read_once: f.read_once.clone(),
        repeat_detect: f.repeat_detect.clone(),
        slippage: f.slippage.clone(),
        task_tracker: f.task_tracker.clone(),
        cd_guard: f.cd_guard.clone(),
        ..Features::default()
    };
    (cut != Features::default()).then_some(cut)
}

/// Merge the hook-read features of the top-level config and the bundles by precedence. Each
/// contributor is cut down to those fields, so a conflict in an unrelated capability cannot
/// switch a feature off. A bundle whose `bundle.yaml` cannot be read is skipped and named in the
/// log: it cannot be shown to set a feature.
fn bundle_features(
    top_level: &Capabilities,
    bundles: &[BundleRef],
) -> anyhow::Result<Option<Features>> {
    let mut contributors = Vec::new();
    let mut push = |name: String, precedence: u8, caps: &Capabilities| {
        if let Some(features) = hook_features(caps) {
            contributors.push(CapabilityContributor {
                name,
                precedence,
                capabilities: Capabilities {
                    features: Some(features),
                    ..Capabilities::default()
                },
            });
        }
    };
    push("config.yaml".to_string(), TOP_LEVEL_PRECEDENCE, top_level);
    for b in bundles {
        match crate::merge::read_bundle_yaml(&b.path, &b.name) {
            Ok(Some(caps)) => push(format!("bundle '{}'", b.name), b.precedence, &caps),
            Ok(None) => {}
            Err(e) => tracing::warn!("bundle '{}' skipped for features: {e:#}", b.name),
        }
    }
    Ok(merge_capabilities(&contributors)?.features)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use crate::config::Bundle;
    use crate::scope::ActiveScope;

    fn write_bundle(root: &Path, name: &str, features_yaml: &str) {
        let dir = root.join("bundles").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("bundle.yaml"),
            format!("features:\n{features_yaml}"),
        )
        .unwrap();
    }

    fn tracker(enabled: bool) -> String {
        format!("  task_tracker:\n    enabled: {enabled}\n")
    }

    fn active(kind: &'static str, tag: &str) -> ActiveScopes {
        ActiveScopes {
            scopes: vec![ActiveScope {
                id: kind.to_string(),
                kind,
                tags: vec![tag.to_string()],
                ..ActiveScope::default()
            }],
            tags: [tag.to_string()].into(),
            ..ActiveScopes::default()
        }
    }

    fn config_with_bundles(names: &[&str], tag: &str) -> Config {
        Config {
            bundle: names
                .iter()
                .map(|n| Bundle {
                    name: (*n).to_string(),
                    when: vec![tag.to_string()],
                })
                .collect(),
            ..Config::default()
        }
    }

    fn bundle_ref(root: &Path, name: &str, precedence: u8) -> BundleRef {
        BundleRef {
            name: name.into(),
            path: root.join("bundles").join(name),
            precedence,
        }
    }

    fn tracker_enabled(config: &Config) -> Option<bool> {
        config
            .features
            .as_ref()
            .and_then(|f| f.task_tracker.as_ref())
            .map(|t| t.enabled)
    }

    #[test]
    fn tracker_enabled_only_by_a_firing_bundle_is_resolved() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(dir.path(), "tracked", &tracker(true));
        let out = resolve_with(
            config_with_bundles(&["tracked"], "rust"),
            dir.path(),
            &active("user", "rust"),
        );
        assert_eq!(tracker_enabled(&out), Some(true));
    }

    #[test]
    fn every_hook_read_feature_resolves_from_a_bundle() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(
            dir.path(),
            "all",
            "  read_once:\n    enabled: true\n  repeat_detect:\n    enabled: true\n  \
             slippage:\n    enabled: true\n  cd_guard:\n    enabled: true\n",
        );
        let out = resolve_with(
            config_with_bundles(&["all"], "rust"),
            dir.path(),
            &active("user", "rust"),
        );
        let f = out.features.unwrap();
        assert!(f.read_once.is_some() && f.repeat_detect.is_some());
        assert!(f.slippage.is_some() && f.cd_guard.is_some());
    }

    #[test]
    fn root_feature_wins_and_the_bundle_fills_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(
            dir.path(),
            "b",
            &format!("{}  slippage:\n    enabled: true\n", tracker(true)),
        );
        let mut config = config_with_bundles(&["b"], "rust");
        config.features = Some(Features {
            task_tracker: Some(serde_yaml::from_str("enabled: false").unwrap()),
            ..Features::default()
        });
        let out = resolve_with(config, dir.path(), &active("user", "rust"));
        assert_eq!(tracker_enabled(&out), Some(false));
        assert!(out.features.unwrap().slippage.is_some());
    }

    #[test]
    fn bundle_that_does_not_fire_leaves_features_unset() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(dir.path(), "tracked", &tracker(true));
        let out = resolve_with(
            config_with_bundles(&["tracked"], "rust"),
            dir.path(),
            &active("user", "python"),
        );
        assert!(out.features.is_none());
    }

    #[test]
    fn higher_precedence_bundle_wins_over_a_lower_one() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(dir.path(), "low", &tracker(true));
        write_bundle(dir.path(), "high", &tracker(false));
        let refs = [
            bundle_ref(dir.path(), "low", 1),
            bundle_ref(dir.path(), "high", 4),
        ];
        let got = bundle_features(&Capabilities::default(), &refs)
            .unwrap()
            .unwrap();
        assert!(!got.task_tracker.unwrap().enabled);
    }

    #[test]
    fn bundle_without_features_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bundles/plain")).unwrap();
        let refs = [bundle_ref(dir.path(), "plain", 1)];
        assert!(
            bundle_features(&Capabilities::default(), &refs)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn config_with_no_bundles_is_returned_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            features: Some(Features {
                task_tracker: Some(serde_yaml::from_str("enabled: false").unwrap()),
                ..Features::default()
            }),
            ..Config::default()
        };
        let out = resolve(config.clone(), &dir.path().join("config.yaml"));
        assert_eq!(out.features, config.features);
    }

    #[test]
    fn a_broken_sibling_bundle_does_not_switch_the_tracker_off() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(dir.path(), "good", &tracker(true));
        let broken = dir.path().join("bundles/broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("bundle.yaml"), "features: [not a map").unwrap();
        let out = resolve_with(
            config_with_bundles(&["broken", "good"], "rust"),
            dir.path(),
            &active("user", "rust"),
        );
        assert_eq!(tracker_enabled(&out), Some(true));
    }

    #[test]
    fn unreadable_only_bundle_leaves_features_unset() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("bundles/broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("bundle.yaml"), "features: [not a map").unwrap();
        let out = resolve_with(
            config_with_bundles(&["broken"], "rust"),
            dir.path(),
            &active("user", "rust"),
        );
        assert_eq!(tracker_enabled(&out), None);
    }

    proptest::proptest! {
        // Each case writes bundle files, so the case count stays low.
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

        // The highest precedence wins whatever order the bundles are listed in.
        #[test]
        fn highest_precedence_wins_in_any_order(
            bundles in proptest::collection::btree_map(0u8..250, proptest::prelude::any::<bool>(), 1..6),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let mut refs = Vec::new();
            for (precedence, enabled) in &bundles {
                let name = format!("b{precedence}");
                write_bundle(dir.path(), &name, &tracker(*enabled));
                refs.push(bundle_ref(dir.path(), &name, *precedence));
            }
            let expected = *bundles.values().next_back().unwrap();
            for order in [false, true] {
                if order {
                    refs.reverse();
                }
                let got = bundle_features(&Capabilities::default(), &refs).unwrap().unwrap();
                proptest::prop_assert_eq!(got.task_tracker.unwrap().enabled, expected);
            }
        }
    }
}
