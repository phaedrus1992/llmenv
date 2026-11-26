//! The `cache` section of the config: the cache folder, the sync interval, and the GC ages.

use serde::{Deserialize, Serialize};

/// llmenv's own cache/sync behavior. Distinct from engine `capabilities` — this
/// governs the local materialization cache, not anything written into an agent's
/// config.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Cache {
    pub cache_dir: String,
    pub sync_interval_minutes: u64,
    pub cache_retention_hours: Option<u64>,
    /// How long a `state/tmp` entry is kept, in hours (#4046). `null` turns the `state/tmp` sweep
    /// off. An omitted key keeps entries for 168 hours (7 days).
    pub tmp_retention_hours: Option<u64>,
    /// Cache-folder strictness dial (#246). A single knob trading cache reuse
    /// against invalidation precision: `loose` ⊂ `normal` ⊂ `strict`. See
    /// [`HashingMode`] for the per-mode folder layout. Drift detection and
    /// reconciliation are identical across modes — the manifest dotfile carries
    /// the content hash and owned-file set regardless.
    pub hashing: HashingMode,
    /// Allow remote git operations (fetch, pull, push) on the config directory
    /// and plugin marketplaces. Set to `false` to prevent shell freezes when
    /// 1Password or other SSH credential helpers are locked. Default: `true`.
    #[serde(default = "default_remote_sync")]
    pub remote_sync: bool,
}

const fn default_remote_sync() -> bool {
    true
}

impl Cache {
    /// The age-based GC window, or `None` when `cache_retention_hours` is `null`. `null`
    /// turns age-based GC off, as the config docs state (#2829). An omitted key takes the
    /// default from [`Cache::default`], so only an explicit `null` reaches `None`.
    ///
    /// A multiplication that would wrap saturates, so it keeps every entry.
    #[must_use]
    pub fn gc_retention(&self) -> Option<std::time::Duration> {
        self.cache_retention_hours.map(hours_window)
    }

    /// The keep time of a `state/tmp` entry in hours, or `None` when `tmp_retention_hours` is
    /// `null`, or when `cache_retention_hours` is `null`. A `null` `cache_retention_hours` turns
    /// age-based GC off, and that includes `state/tmp` (#4046).
    #[must_use]
    pub fn tmp_gc_hours(&self) -> Option<u64> {
        self.cache_retention_hours.and(self.tmp_retention_hours)
    }

    /// The keep time of a `state/tmp` entry, or `None` when [`Cache::tmp_gc_hours`] is `None`
    /// (#4046). A multiplication that would wrap saturates, so it keeps every entry.
    #[must_use]
    pub fn tmp_retention(&self) -> Option<std::time::Duration> {
        self.tmp_gc_hours().map(hours_window)
    }
}

/// `hours` as a duration. A multiplication that would wrap saturates.
fn hours_window(hours: u64) -> std::time::Duration {
    std::time::Duration::from_secs(hours.saturating_mul(3600))
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            cache_dir: "~/.cache/llmenv".into(),
            sync_interval_minutes: 15,
            cache_retention_hours: Some(168), // 7 days
            tmp_retention_hours: Some(168),   // 7 days
            hashing: HashingMode::default(),
            remote_sync: default_remote_sync(),
        }
    }
}

/// Cache-folder strictness dial (#246). One knob, three positions, ordered by
/// how aggressively a folder is reused: `loose` ⊂ `normal` ⊂ `strict`. Default
/// is [`Self::Normal`] — the common path, isolating by selection *shape* and
/// binary major version while still reusing a folder across content edits and
/// minor/patch upgrades, so a running agent's in-session state survives a
/// re-render (#1263).
///
/// The *shape* is a 12-hex digest over the active selection (`active_tags ∪
/// directly_enabled_bundles`), so two different tag/bundle combinations never
/// collide in one folder (the version-mode overwrite bug that motivated #246).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HashingMode {
    /// Folder = `<adapter>/<shape>`. No version axis: a binary upgrade reuses
    /// the same per-shape folder. Fewest folders; relies on age-based gc to
    /// trim shapes that fall out of use.
    Loose,
    /// Folder = `<adapter>/<version_major>/<shape>`. The default. Content
    /// edits and minor/patch version bumps re-render into the same folder;
    /// only a major version bump or a selection change mints a new one
    /// (#1263).
    #[default]
    Normal,
    /// Folder = `<adapter>/<VERSION_TAG>-<content_hash>`. Any input change mints
    /// a fresh folder — strongest isolation, most cache churn.
    Strict,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::Cache;

    const HOUR: u64 = 3600;

    proptest::proptest! {
        #[test]
        fn gc_retention_is_hours_in_seconds_and_monotone(h1 in proptest::num::u64::ANY, h2 in proptest::num::u64::ANY) {
            let at = |hours| Cache { cache_retention_hours: Some(hours), ..Cache::default() }.gc_retention();
            proptest::prop_assert_eq!(at(h1).map(|d| d.as_secs()), Some(h1.saturating_mul(3600)));
            if h1 <= h2 {
                proptest::prop_assert!(at(h1) <= at(h2));
            }
        }

        #[test]
        fn tmp_retention_is_hours_in_seconds_and_monotone(h1 in proptest::num::u64::ANY, h2 in proptest::num::u64::ANY) {
            let at = |hours| Cache {
                cache_retention_hours: Some(168),
                tmp_retention_hours: Some(hours),
                ..Cache::default()
            }
            .tmp_retention();
            proptest::prop_assert_eq!(at(h1).map(|d| d.as_secs()), Some(h1.saturating_mul(3600)));
            if h1 <= h2 {
                proptest::prop_assert!(at(h1) <= at(h2));
            }
        }

        #[test]
        fn a_null_cache_retention_turns_the_tmp_window_off(hours in proptest::num::u64::ANY) {
            let cache = Cache {
                cache_retention_hours: None,
                tmp_retention_hours: Some(hours),
                ..Cache::default()
            };
            proptest::prop_assert_eq!(cache.tmp_retention(), None);
        }
    }

    #[test]
    fn remote_sync_is_on_unless_the_config_turns_it_off() {
        assert!(Cache::default().remote_sync);
        let omitted: Cache = serde_yaml::from_str("sync_interval_minutes: 5").unwrap();
        assert!(omitted.remote_sync);
        let off: Cache = serde_yaml::from_str("remote_sync: false").unwrap();
        assert!(!off.remote_sync);
    }

    #[test]
    fn gc_retention_is_off_only_for_an_explicit_null() {
        let explicit_null: Cache = serde_yaml::from_str("cache_retention_hours: null").unwrap();
        assert_eq!(explicit_null.gc_retention(), None);
        let omitted: Cache = serde_yaml::from_str("sync_interval_minutes: 5").unwrap();
        assert_eq!(
            omitted.gc_retention(),
            Some(std::time::Duration::from_secs(168 * 3600))
        );
        let huge = Cache {
            cache_retention_hours: Some(u64::MAX),
            ..Cache::default()
        };
        assert_eq!(
            huge.gc_retention(),
            Some(std::time::Duration::from_secs(u64::MAX))
        );
    }

    #[test]
    fn tmp_retention_defaults_to_a_week_and_is_off_only_for_an_explicit_null() {
        let omitted: Cache = serde_yaml::from_str("sync_interval_minutes: 5").unwrap();
        assert_eq!(
            omitted.tmp_retention(),
            Some(std::time::Duration::from_secs(168 * HOUR))
        );
        let set: Cache = serde_yaml::from_str("tmp_retention_hours: 24").unwrap();
        assert_eq!(
            set.tmp_retention(),
            Some(std::time::Duration::from_secs(24 * HOUR))
        );
        let null: Cache = serde_yaml::from_str("tmp_retention_hours: null").unwrap();
        assert_eq!(null.tmp_retention(), None);
    }

    #[test]
    fn the_cache_retention_sets_no_age_for_state_tmp() {
        let cache: Cache = serde_yaml::from_str("cache_retention_hours: 1").unwrap();
        assert_eq!(
            cache.tmp_retention(),
            Some(std::time::Duration::from_secs(168 * HOUR))
        );
    }

    #[test]
    fn a_null_cache_retention_turns_state_tmp_gc_off_too() {
        let cache: Cache = serde_yaml::from_str("cache_retention_hours: null").unwrap();
        assert_eq!(cache.tmp_gc_hours(), None);
        assert_eq!(cache.tmp_retention(), None);
        let with_tmp: Cache =
            serde_yaml::from_str("cache_retention_hours: null\ntmp_retention_hours: 24").unwrap();
        assert_eq!(with_tmp.tmp_retention(), None);
    }
}
