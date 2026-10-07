//! Resolve `features.task_tracker` for the hook runtime the way the adapter does (#2460).
//!
//! The adapter reads the tracker from the merged manifest: the root `features` block wins, then
//! the highest-precedence bundle that sets it. The hook runtime read the root block only, so a
//! tracker that a tag-scoped bundle enabled registered the hooks and then ran them switched off.

use std::path::Path;

use crate::config::{Capabilities, Config, Features, TaskTracker};
use crate::merge::{BundleRef, CapabilityContributor, merge_capabilities};
use crate::scope::ActiveScopes;

/// The root config outranks every bundle, as in `merge::merge`.
const TOP_LEVEL_PRECEDENCE: u8 = u8::MAX;

/// Return `config` with `features.task_tracker` resolved from the active bundles.
///
/// A tracker set in the root `features` block is final, so that case reads no bundle file and
/// runs no scope detection.
pub(crate) fn resolve(config: Config, config_dir: &Path) -> Config {
    if config
        .features
        .as_ref()
        .is_some_and(|f| f.task_tracker.is_some())
        || config.bundle.is_empty()
    {
        return config;
    }
    let env = crate::scope::matcher::Env::detect_for_config(&config);
    let active = crate::scope::evaluate(&config, &env);
    resolve_with(config, config_dir, &active)
}

fn resolve_with(mut config: Config, config_dir: &Path, active: &ActiveScopes) -> Config {
    let firing = crate::cli::firing_bundles(&config.bundle, active, None);
    let refs = crate::cli::build_bundle_refs(config_dir, active, &firing);
    match bundle_tracker(&config.capabilities, &refs) {
        Ok(Some(tracker)) => {
            config.features.get_or_insert_default().task_tracker = Some(tracker);
        }
        Ok(None) => {}
        Err(e) => tracing::error!("task tracker left off, cannot resolve it from bundles: {e}"),
    }
    config
}

/// Pick the `task_tracker` of the highest-precedence contributor. Each contributor is cut down
/// to that one field, so a conflict in an unrelated capability cannot switch the tracker off.
fn bundle_tracker(
    top_level: &Capabilities,
    bundles: &[BundleRef],
) -> anyhow::Result<Option<TaskTracker>> {
    let mut contributors = Vec::new();
    let mut push = |name: String, precedence: u8, caps: &Capabilities| {
        let tracker = caps.features.as_ref().and_then(|f| f.task_tracker.clone());
        if tracker.is_some() {
            contributors.push(CapabilityContributor {
                name,
                precedence,
                capabilities: Capabilities {
                    features: Some(Features {
                        task_tracker: tracker,
                        ..Features::default()
                    }),
                    ..Capabilities::default()
                },
            });
        }
    };
    push("config.yaml".to_string(), TOP_LEVEL_PRECEDENCE, top_level);
    for b in bundles {
        if let Some(caps) = crate::merge::read_bundle_yaml(&b.path, &b.name)? {
            push(format!("bundle '{}'", b.name), b.precedence, &caps);
        }
    }
    Ok(merge_capabilities(&contributors)?
        .features
        .and_then(|f| f.task_tracker))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use crate::config::Bundle;
    use crate::scope::ActiveScope;

    fn write_bundle(root: &Path, name: &str, tracker_yaml: &str) {
        let dir = root.join("bundles").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("bundle.yaml"),
            format!("features:\n  task_tracker:\n{tracker_yaml}"),
        )
        .unwrap();
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

    fn config_with_bundle(name: &str, tag: &str) -> Config {
        Config {
            bundle: vec![Bundle {
                name: name.to_string(),
                when: vec![tag.to_string()],
            }],
            ..Config::default()
        }
    }

    fn enabled(config: &Config) -> bool {
        config
            .features
            .as_ref()
            .and_then(|f| f.task_tracker.as_ref())
            .is_some_and(|t| t.enabled)
    }

    #[test]
    fn tracker_enabled_only_by_a_firing_bundle_is_resolved() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(dir.path(), "tracked", "    enabled: true\n");
        let out = resolve_with(
            config_with_bundle("tracked", "rust"),
            dir.path(),
            &active("user", "rust"),
        );
        assert!(enabled(&out));
    }

    #[test]
    fn bundle_that_does_not_fire_leaves_the_tracker_off() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(dir.path(), "tracked", "    enabled: true\n");
        let out = resolve_with(
            config_with_bundle("tracked", "rust"),
            dir.path(),
            &active("user", "python"),
        );
        assert!(!enabled(&out));
        assert!(out.features.is_none());
    }

    #[test]
    fn higher_precedence_bundle_wins_over_a_lower_one() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(dir.path(), "low", "    enabled: true\n");
        write_bundle(dir.path(), "high", "    enabled: false\n");
        let refs = [
            BundleRef {
                name: "low".into(),
                path: dir.path().join("bundles/low"),
                precedence: 1,
            },
            BundleRef {
                name: "high".into(),
                path: dir.path().join("bundles/high"),
                precedence: 4,
            },
        ];
        let tracker = bundle_tracker(&Capabilities::default(), &refs).unwrap();
        assert!(!tracker.unwrap().enabled);
    }

    #[test]
    fn bundle_without_a_tracker_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundles/plain");
        std::fs::create_dir_all(&path).unwrap();
        let refs = [BundleRef {
            name: "plain".into(),
            path,
            precedence: 1,
        }];
        assert!(
            bundle_tracker(&Capabilities::default(), &refs)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn root_tracker_is_final_and_reads_no_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with_bundle("missing", "rust");
        config.features = Some(Features {
            task_tracker: Some(serde_yaml::from_str("enabled: false").unwrap()),
            ..Features::default()
        });
        let out = resolve(config.clone(), dir.path());
        assert_eq!(out.features, config.features);
    }

    #[test]
    fn unreadable_bundle_yaml_keeps_the_tracker_off() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundles/broken");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("bundle.yaml"), "features: [not a map").unwrap();
        let out = resolve_with(
            config_with_bundle("broken", "rust"),
            dir.path(),
            &active("user", "rust"),
        );
        assert!(!enabled(&out));
    }
}
