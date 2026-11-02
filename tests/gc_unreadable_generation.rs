#![expect(clippy::expect_used, reason = "test scaffolding")]
//! One generation folder that cannot be read must not stop the cache GC pass (#3195 review).

use std::time::Duration;

use llmenv::config::HashingMode;
use llmenv::materialize::cache::{self, AgeRule, AgeSource};
use tempfile::tempdir;

#[cfg(unix)]
#[test]
fn an_unreadable_generation_dir_does_not_stop_the_gc_pass() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempdir().expect("tempdir");
    let major = env!("CARGO_PKG_VERSION")
        .split('.')
        .next()
        .expect("major version");
    let current = tmp.path().join(major);
    std::fs::create_dir_all(&current).expect("mkdir current");
    std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let stale = tmp.path().join("0.0.0-stale");
    std::fs::create_dir_all(&stale).expect("mkdir stale");
    std::fs::write(stale.join("f"), b"x").expect("write stale");

    let rule = AgeRule {
        older_than: Duration::ZERO,
        source: AgeSource::MtimeOnly,
    };
    let result = cache::gc(tmp.path(), rule, HashingMode::Normal, false);
    std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o700)).expect("restore");

    result.expect("an unreadable generation must not fail the whole pass");
    assert!(
        !stale.exists(),
        "the sibling after the unreadable folder was kept"
    );
}
