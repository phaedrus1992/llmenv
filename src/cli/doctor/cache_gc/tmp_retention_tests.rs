//! The age of `state/tmp` is its own setting, apart from `cache_retention_hours` (#4046).

use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use super::{GcOutput, Retention, sweep_state};
use crate::config::Config;

const DAY: Duration = Duration::from_secs(86_400);
const OUT: GcOutput<'static> = GcOutput {
    pass: "ok",
    warn: "warn",
    dry_run: false,
};

/// Two `state/tmp` entries, judged at a clock 8 days ahead. The sweep keeps an entry whose mtime
/// or change time is new, and a test cannot set a change time, so the later clock stands for it:
/// the change time of both entries is 8 days old, and the mtime decides.
struct Tmp {
    state: tempfile::TempDir,
    /// Not changed for 3 days.
    recent: PathBuf,
    /// Not changed for 9 days.
    old: PathBuf,
    later: SystemTime,
}

fn tmp_fixture() -> Tmp {
    let state = tempfile::tempdir().unwrap();
    let tmp = state.path().join("tmp");
    std::fs::create_dir(&tmp).unwrap();
    let later = SystemTime::now() + 8 * DAY;
    let make = |name: &str, idle: Duration| {
        let path = tmp.join(name);
        std::fs::write(&path, b"x").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later - idle)
            .unwrap();
        path
    };
    let recent = make("recent", 3 * DAY);
    let old = make("old", 9 * DAY);
    Tmp {
        state,
        recent,
        old,
        later,
    }
}

fn sweep(t: &Tmp, retention: Retention, out: GcOutput<'_>) {
    assert_eq!(
        sweep_state(t.state.path(), &Config::default(), retention, t.later, out),
        0
    );
}

#[test]
fn the_default_keeps_a_tmp_entry_for_seven_days() {
    let t = tmp_fixture();
    let retention = Retention::resolve(&Config::default(), None);
    assert_eq!(retention.tmp, Some(7 * DAY));

    sweep(&t, retention, OUT);

    assert!(t.recent.exists(), "a 3-day entry was removed");
    assert!(!t.old.exists(), "a 9-day entry was kept");
}

#[test]
fn the_config_setting_sets_the_age_of_state_tmp() {
    let t = tmp_fixture();
    let mut config = Config::default();
    config.cache.tmp_retention_hours = Some(48);

    sweep(&t, Retention::resolve(&config, None), OUT);

    assert!(!t.recent.exists(), "a 3-day entry survived a 48-hour age");
    assert!(!t.old.exists());
}

#[test]
fn the_flag_replaces_the_config_setting_for_state_tmp_only() {
    let mut config = Config::default();
    config.cache.tmp_retention_hours = Some(24 * 30);
    config.cache.cache_retention_hours = Some(24 * 14);

    let retention = Retention::resolve(&config, Some(2 * DAY));

    assert_eq!(retention.tmp, Some(2 * DAY));
    assert_eq!(retention.general, Some(14 * DAY));
    let t = tmp_fixture();
    sweep(&t, retention, OUT);
    assert!(!t.recent.exists(), "--tmp-older-than 2d kept a 3-day entry");
}

#[test]
fn the_cache_retention_does_not_change_how_long_state_tmp_is_kept() {
    let t = tmp_fixture();
    let mut config = Config::default();
    config.cache.cache_retention_hours = Some(1);

    sweep(&t, Retention::resolve(&config, None), OUT);

    assert!(t.recent.exists(), "cache_retention_hours reached state/tmp");
    assert!(!t.old.exists());
}

// A null `cache_retention_hours` is the opt-out of all age-based GC. It keeps state/tmp too.
#[test]
fn a_null_cache_retention_keeps_every_state_tmp_entry() {
    let t = tmp_fixture();
    let mut config = Config::default();
    config.cache.cache_retention_hours = None;

    let retention = Retention::resolve(&config, None);
    assert_eq!((retention.general, retention.tmp), (None, None));
    sweep(&t, retention, OUT);

    assert!(
        t.old.exists() && t.recent.exists(),
        "a null opt-out was ignored"
    );
}

#[test]
fn the_flag_sweeps_state_tmp_even_when_the_config_is_null() {
    let t = tmp_fixture();
    let mut config = Config::default();
    config.cache.cache_retention_hours = None;

    let retention = Retention::resolve(&config, Some(2 * DAY));
    assert_eq!((retention.general, retention.tmp), (None, Some(2 * DAY)));
    sweep(&t, retention, OUT);

    assert!(
        !t.old.exists() && !t.recent.exists(),
        "the flag was ignored"
    );
}

#[test]
fn a_null_tmp_retention_keeps_every_entry() {
    let t = tmp_fixture();
    let mut config = Config::default();
    config.cache.tmp_retention_hours = None;

    let retention = Retention::resolve(&config, None);
    assert_eq!(retention.tmp, None);
    sweep(&t, retention, OUT);

    assert!(t.old.exists() && t.recent.exists());
}

#[test]
fn a_dry_run_removes_nothing() {
    let t = tmp_fixture();
    let preview = GcOutput {
        dry_run: true,
        ..OUT
    };

    sweep(&t, Retention::resolve(&Config::default(), None), preview);

    assert!(
        t.old.exists() && t.recent.exists(),
        "a dry run removed an entry"
    );
}

#[test]
fn the_age_of_an_entry_names_why_it_cannot_be_given() {
    use crate::materialize::cache::AgeSource;
    use crate::materialize::tmp_sweep::entry_age;

    let t = tmp_fixture();
    let source = AgeSource::MtimeOnly;
    assert_eq!(entry_age(&t.recent, source, t.later), Ok(3 * DAY));

    // The entry changed after the clock of the sweep.
    let early = t.later - 30 * DAY;
    let future = entry_age(&t.recent, source, early).unwrap_err();
    assert!(future.contains("in the future"), "{future}");

    let missing = t.state.path().join("tmp").join("gone");
    let unreadable = entry_age(&missing, source, t.later).unwrap_err();
    assert!(unreadable.contains("cannot read it"), "{unreadable}");
}

mod properties {
    use std::time::Duration;

    use proptest::prelude::*;

    use super::{Config, Retention};

    const HOUR: u64 = 3600;

    proptest! {
        // The flag replaces the age of state/tmp only. The general age always comes from the
        // config, and a null cache_retention_hours turns off the config age of state/tmp.
        #[test]
        fn the_flag_sets_the_tmp_age_and_the_config_sets_the_rest(
            flag in prop::option::of(1u64..1_000_000),
            cache_hours in prop::option::of(1u64..100_000),
            tmp_hours in prop::option::of(1u64..100_000),
        ) {
            let mut config = Config::default();
            config.cache.cache_retention_hours = cache_hours;
            config.cache.tmp_retention_hours = tmp_hours;

            let got = Retention::resolve(&config, flag.map(Duration::from_secs));

            let hours = |h: u64| Duration::from_secs(h * HOUR);
            prop_assert_eq!(got.general, cache_hours.map(hours));
            let from_config = cache_hours.and(tmp_hours).map(hours);
            prop_assert_eq!(got.tmp, flag.map(Duration::from_secs).or(from_config));
        }
    }
}
