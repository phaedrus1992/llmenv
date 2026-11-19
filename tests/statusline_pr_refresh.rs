#![expect(clippy::unwrap_used, reason = "test scaffolding")]
//! The detached PR refresh of `llmenv statusline` (#3889). A fake `gh` stands in for GitHub, so
//! these tests start the real binary and the real detached child.

use assert_cmd::Command;
use predicates::prelude::*;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

mod support;

const CONFIG: &str = r#"
scope:
  network: []
  host: []
  user: []

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code

statusline:
  rows:
    - "{pr}"
"#;

/// A fake `gh` that logs each run, waits `sleep` seconds, and then fails when a `fail` file is in
/// the folder it runs in, or else prints PR number 42.
fn fake_gh(bin_dir: &Path, sleep: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        "#!/bin/sh\necho called >> gh-calls.log\nsleep {sleep}\n\
         if [ -f fail ]; then echo 'error connecting to api.github.com' >&2; exit 1; fi\n\
         echo '{{\"number\":42,\"url\":null,\"reviewDecision\":null}}'\n"
    );
    let path = bin_dir.join("gh");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Env {
    config: TempDir,
    data: TempDir,
    bin: TempDir,
    repo: TempDir,
}

impl Env {
    fn new(gh_sleep: u32) -> Self {
        let env = Self {
            config: TempDir::new().unwrap(),
            data: TempDir::new().unwrap(),
            bin: TempDir::new().unwrap(),
            repo: TempDir::new().unwrap(),
        };
        std::fs::write(env.config.path().join("config.yaml"), CONFIG).unwrap();
        fake_gh(env.bin.path(), gh_sleep);
        env
    }

    fn statusline(&self) -> Command {
        let path = format!(
            "{}:{}",
            self.bin.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = support::isolated_llmenv_cmd(self.config.path());
        cmd.env("LLMENV_CONFIG", self.config.path().join("config.yaml"))
            .env("CLAUDE_CONFIG_DIR", self.data.path())
            .env("PATH", path)
            .arg("statusline")
            .write_stdin(
                serde_json::json!({
                    "workspace": { "current_dir": self.repo.path() },
                    "branch": { "name": "feat/x" },
                })
                .to_string(),
            );
        cmd
    }

    fn cache_dir(&self) -> PathBuf {
        self.data.path().join("statusline-state")
    }

    fn cache_file(&self) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(self.repo.path().to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(b"feat/x");
        self.cache_dir()
            .join(format!("pr-cache-{}", hex::encode(hasher.finalize())))
    }

    /// Wait until a refresh has written the cache entry and released its lock.
    fn wait_for_refresh(&self) {
        let lock = PathBuf::from(format!("{}.lock", self.cache_file().display()));
        let deadline = Instant::now() + Duration::from_secs(30);
        while !(self.cache_file().exists() && lock_is_free(&lock)) {
            assert!(Instant::now() < deadline, "the refresh did not finish");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn gh_calls(&self) -> usize {
        std::fs::read_to_string(self.repo.path().join("gh-calls.log"))
            .map_or(0, |text| text.lines().count())
    }
}

/// Whether no refresh holds the lock. The lock file stays, and the system lock on it is the signal.
fn lock_is_free(lock: &Path) -> bool {
    match std::fs::OpenOptions::new().write(true).open(lock) {
        Ok(file) => file.try_lock().is_ok(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn a_cold_render_does_not_wait_for_gh_and_the_next_render_shows_the_pr() {
    let env = Env::new(4);
    let started = Instant::now();
    env.statusline()
        .timeout(Duration::from_secs(60))
        .assert()
        .success()
        .stdout(predicate::str::contains("#").not());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the render waited for gh: {:?}",
        started.elapsed()
    );
    env.wait_for_refresh();
    env.statusline()
        .timeout(Duration::from_secs(30))
        .assert()
        .success()
        .stdout(predicate::str::contains("#42"));
}

#[test]
fn two_renders_start_one_refresh() {
    let env = Env::new(1);
    env.statusline().assert().success();
    env.statusline().assert().success();
    env.wait_for_refresh();
    assert_eq!(env.gh_calls(), 1, "one refresh ran, not two");
}

#[test]
fn a_failed_refresh_keeps_showing_the_last_pr() {
    let env = Env::new(0);
    std::fs::create_dir_all(env.cache_dir()).unwrap();
    let pr = serde_json::json!({ "number": 42, "url": null, "review_state": null });
    // An expired answer that holds the last good PR.
    std::fs::write(
        env.cache_file(),
        serde_json::json!({ "ts": 0, "pr": pr, "good": [unix_now() - 100, pr] }).to_string(),
    )
    .unwrap();
    std::fs::write(env.repo.path().join("fail"), "").unwrap();

    env.statusline()
        .assert()
        .success()
        .stdout(predicate::str::contains("#42"));
    // Wait for the refresh to fail and write its entry.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !std::fs::read_to_string(env.cache_file())
        .unwrap()
        .contains("failure")
    {
        assert!(Instant::now() < deadline, "the refresh did not finish");
        std::thread::sleep(Duration::from_millis(100));
    }
    env.wait_for_refresh();
    env.statusline()
        .assert()
        .success()
        .stdout(predicate::str::contains("#42"))
        .stdout(predicate::str::contains("#?").not());
}
