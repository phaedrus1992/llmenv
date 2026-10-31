//! Settings that llmenv seeds into `settings.json` when they are absent: user-elected keys,
//! `installMethod`, and the default `statusLine`.

use std::path::Path;

use super::LLMENV_OWNED_SETTINGS_KEYS;
use crate::adapter::hook_program::hook_command;

/// Merge user-elected seeded keys into `out/settings.json` after the adapter
/// has already written the file (#172). Runs on every render, not just new
/// folders: `reconcile_settings` already preserves existing foreign keys, so
/// for re-renders this is nearly always a no-op (all seeded keys already
/// present). For a fresh folder, this adds user defaults that would otherwise
/// be absent from the first-rendered `settings.json`.
///
/// **Must be called after `adapter.materialize()`** so that if materialize
/// fails, settings.json is left either absent (new folder, no partial state)
/// or in its prior good state (re-render, reconcile is atomic). Calling before
/// materialize can leave a seeded-only settings.json (no llmenv-owned keys)
/// if materialize subsequently errors.
///
/// # Errors
/// Returns an error if serialization or the atomic write fails.
pub(crate) fn apply_seeded_settings(
    out: &Path,
    seeded: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    if seeded.is_empty() {
        return Ok(());
    }
    let path = out.join("settings.json");
    // Read whatever materialize wrote; no-op if file absent (materialize
    // failed or skipped — don't create a seeded-only file in that case).
    let existing: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            anyhow::anyhow!(
                "{} is not valid JSON ({e}), so the seeded settings were not applied",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading {} for seeding: {e}",
                path.display()
            ));
        }
    };
    let serde_json::Value::Object(mut obj) = existing else {
        anyhow::bail!(
            "{} is not a JSON object, so the seeded settings were not applied",
            path.display()
        );
    };
    let mut changed = false;
    for (k, v) in seeded {
        // Never add llmenv-owned keys — reconcile_settings owns those.
        if !LLMENV_OWNED_SETTINGS_KEYS.contains(&k.as_str()) && !obj.contains_key(k) {
            obj.insert(k.clone(), v.clone());
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let json = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    crate::paths::write_user_file_atomic(&path, json.as_bytes())
        .map_err(|e| anyhow::anyhow!("writing seeded settings {}: {e}", path.display()))
}

/// Classify a `claude` binary path as `"homebrew"`, `"npm"`, or `"native"`.
pub(super) fn classify_claude_path(path: &str) -> &'static str {
    let lc = path.to_ascii_lowercase();
    if lc.contains("/homebrew/") || lc.contains("/cellar/") || lc.contains("/linuxbrew/") {
        "homebrew"
    } else if lc.contains("node_modules")
        || lc.contains("/.npm")
        || lc.contains("/.nvm")
        || lc.contains("/npm/")
        || lc.contains("/.volta/")
        || lc.contains("/.fnm/")
        || lc.contains("/.local/share/pnpm/")
        || lc.contains("/library/pnpm/")
    {
        "npm"
    } else {
        "native"
    }
}

/// The path of the `claude` program on `PATH`, looked up without a process, so the lookup cannot
/// hang or fail to start (#2923).
fn find_claude_binary() -> Option<String> {
    find_claude_binary_in(std::env::var_os("PATH").as_deref())
}

fn find_claude_binary_in(path_var: Option<&std::ffi::OsStr>) -> Option<String> {
    crate::path_lookup::find_on_path("claude", path_var).map(|path| path.display().to_string())
}

#[cfg(test)]
#[test]
#[expect(clippy::expect_used, reason = "test code")]
fn the_claude_lookup_finds_the_program_in_an_absolute_folder_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("test");
    let claude = dir.path().join("claude");
    std::fs::write(&claude, "#!/bin/sh\n").expect("test");
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).expect("test");
    let path_var = std::env::join_paths([dir.path()]).expect("test");
    assert_eq!(
        find_claude_binary_in(Some(&path_var)),
        Some(claude.display().to_string())
    );
    assert_eq!(find_claude_binary_in(None), None);
    let empty = std::env::join_paths([dir.path().join("none")]).expect("test");
    assert_eq!(find_claude_binary_in(Some(&empty)), None);
}

/// Parse `settings.json` for seeding one key. Anything but a JSON object is an error that names
/// the file and the key, so a seed never acts on a file it could not read.
fn parse_settings_object(
    path: &std::path::Path,
    bytes: &[u8],
    key: &str,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(serde_json::Value::Object(obj)) => Ok(obj),
        Ok(_) => anyhow::bail!(
            "{} is not a JSON object, so {key} cannot be seeded. Fix: restore it to a JSON object.",
            path.display()
        ),
        Err(e) => anyhow::bail!(
            "parsing {} for seeding {key}: {e}. Fix: restore it to valid JSON.",
            path.display()
        ),
    }
}

/// Seed `installMethod` into `out/settings.json` if absent (#346).
///
/// Detects how `claude` was installed by inspecting its binary path, then
/// writes the result as a foreign key so it survives every re-render.
/// No-op if `settings.json` does not exist (materialize hasn't run yet), if
/// `installMethod` is already present, or if `claude` is not on `PATH`.
///
/// # Errors
/// Returns an error if the file exists but cannot be read or written.
pub(crate) fn seed_install_method(out: &std::path::Path) -> anyhow::Result<()> {
    let settings_path = out.join("settings.json");

    // Skip fork if installMethod already present.
    match std::fs::read(&settings_path) {
        Ok(bytes) => {
            let obj = parse_settings_object(&settings_path, &bytes, "installMethod")?;
            if obj.contains_key("installMethod") {
                return Ok(());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File doesn't exist yet, that's fine.
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading {} for seeding installMethod: {e}",
                settings_path.display()
            ));
        }
    }

    // Without the binary the method is unknown. The key is written only once, so a guess here
    // would stay after `claude` is on PATH; the next run seeds it instead (#2923).
    let Some(binary) = find_claude_binary() else {
        tracing::debug!("installMethod not seeded: claude is not on PATH");
        return Ok(());
    };
    let method = classify_claude_path(&binary);
    let mut seeded = serde_json::Map::new();
    seeded.insert("installMethod".to_string(), serde_json::Value::from(method));
    apply_seeded_settings(out, &seeded)
}

/// Seed the default `statusLine` hook into `out/settings.json` if absent
/// (#836), pointing Claude Code's statusline at `llmenv statusline`.
///
/// Deliberately a **seed**, not an owned/authoritative render key (contrast
/// `autoMemoryEnabled`): `statusLine` is not in [`LLMENV_OWNED_SETTINGS_KEYS`],
/// so if this were instead emitted unconditionally into `render_settings`'s
/// `fresh` map, `reconcile_settings`'s passthrough rule would write it
/// through on *every* re-render — permanently clobbering a user's own
/// `/statusline` customization (which writes straight to `settings.json`,
/// outside llmenv) on the very next shell-hook materialize. Seeding once,
/// only when absent, matches [`seed_install_method`]'s contract: once any
/// value exists — llmenv's default, a user customization, or a
/// `native.claude_code.statusLine` override already written by
/// `reconcile_settings` before this runs — it is left alone.
///
/// No-op if `settings.json` does not exist yet (materialize hasn't run) or if
/// `statusLine` is already present.
///
/// # Errors
/// Returns an error if the file exists but cannot be read or written.
pub(crate) fn seed_status_line(out: &std::path::Path) -> anyhow::Result<()> {
    let settings_path = out.join("settings.json");

    match std::fs::read(&settings_path) {
        Ok(bytes) => {
            let mut obj = parse_settings_object(&settings_path, &bytes, "statusLine")?;
            // A user's statusLine is kept as it is. Only the exact bare command that older llmenv
            // seeded is replaced, so an install from before #2956 gets the fixed command (#3051).
            match obj.get("statusLine") {
                None => {}
                // A null entry is a user's setting, so it is kept. The log says why it is kept.
                Some(serde_json::Value::Null) => {
                    // A kept user setting is not a degraded result, so this is debug only.
                    tracing::debug!(
                        "statusLine is null in {}, so llmenv does not seed the default. Fix: \
                         remove the statusLine key, and the next run seeds the default.",
                        settings_path.display()
                    );
                    return Ok(());
                }
                Some(existing) if !is_bare_seeded_status_line(existing) => return Ok(()),
                // `apply_seeded_settings` never overwrites a key, so the bare entry is removed
                // first and the seeded value is written in its place.
                Some(_) => {
                    obj.remove("statusLine");
                    let json = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
                    crate::paths::write_user_file_atomic(&settings_path, json.as_bytes())
                        .map_err(|e| anyhow::anyhow!("writing {}: {e}", settings_path.display()))?;
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File doesn't exist yet, that's fine.
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading {} for seeding statusLine: {e}",
                settings_path.display()
            ));
        }
    }

    let mut seeded = serde_json::Map::new();
    seeded.insert(
        "statusLine".to_string(),
        // `--color always`: Claude Code invokes this with stdout captured
        // (never a TTY), so the default `auto` color resolution would
        // silently disable every `style:` widget override — the seeded
        // command must opt in explicitly rather than rely on TTY detection.
        serde_json::json!({ "type": "command", "command": hook_command("statusline --color always") }),
    );
    apply_seeded_settings(out, &seeded)
}

/// The `statusLine` command that llmenv seeded before it named itself by absolute path (#2956).
/// Only this exact value is migrated (#3051).
const BARE_SEEDED_STATUS_COMMAND: &str = "llmenv statusline --color always";

/// True when `existing` is the bare command that an older llmenv seeded.
fn is_bare_seeded_status_line(existing: &serde_json::Value) -> bool {
    existing.get("command").and_then(serde_json::Value::as_str) == Some(BARE_SEEDED_STATUS_COMMAND)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod status_line_tests {
    use super::*;

    proptest::proptest! {
        #[test]
        fn a_settings_file_parses_exactly_when_it_is_a_json_object(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
        ) {
            let parsed = parse_settings_object(std::path::Path::new("s.json"), &bytes, "k");
            let is_object = matches!(
                serde_json::from_slice::<serde_json::Value>(&bytes),
                Ok(serde_json::Value::Object(_))
            );
            proptest::prop_assert_eq!(parsed.is_ok(), is_object);
        }

        #[test]
        fn a_json_object_keeps_every_key_and_value(
            keys in proptest::collection::btree_map("[a-z]{1,6}", "[a-z0-9 ]{0,8}", 0..6),
        ) {
            let text = serde_json::to_vec(&keys).expect("test setup");
            let parsed = parse_settings_object(std::path::Path::new("s.json"), &text, "k").expect("test setup");
            proptest::prop_assert_eq!(parsed.len(), keys.len());
            for (key, value) in &keys {
                proptest::prop_assert_eq!(parsed[key].as_str(), Some(value.as_str()));
            }
        }

        #[test]
        fn a_user_status_line_and_every_other_key_survive_seeding_byte_for_byte(
            other in proptest::collection::btree_map("[a-z]{1,6}", "[a-z]{0,6}", 0..4),
            command in "[a-z]{1,10}",
        ) {
            let mut obj = serde_json::Map::new();
            for (key, value) in &other {
                obj.insert(key.clone(), serde_json::Value::from(value.clone()));
            }
            let user = serde_json::json!({ "type": "command", "command": format!("my-{command}") });
            obj.insert("statusLine".to_string(), user);
            let before = serde_json::to_vec(&obj).expect("test setup");
            let dir = tempfile::tempdir().expect("test setup");
            let settings = dir.path().join("settings.json");
            std::fs::write(&settings, &before).expect("test setup");

            seed_status_line(dir.path()).expect("test setup");

            proptest::prop_assert_eq!(std::fs::read(&settings).expect("test setup"), before);
        }
    }

    #[test]
    fn a_corrupt_settings_file_is_an_error_that_names_the_file_and_the_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, b"{not json").expect("write fixture");

        let err = seed_status_line(dir.path())
            .expect_err("corrupt settings must fail")
            .to_string();

        assert!(err.contains("seeding statusLine"), "{err}");
        assert!(err.contains(&settings.display().to_string()), "{err}");
    }

    #[test]
    fn a_null_status_line_is_kept_and_the_log_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, br#"{"statusLine": null}"#).expect("write fixture");
        let mut result = None;

        let logs = crate::test_log_capture::capture_logs(|| {
            result = Some(seed_status_line(dir.path()));
        });

        result
            .expect("seed ran")
            .expect("a null statusLine is not an error");
        assert!(logs.contains("statusLine is null"), "{logs}");
        let kept: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings).expect("read")).expect("parse");
        assert!(kept["statusLine"].is_null(), "{kept}");
    }

    fn seeded_command(out: &std::path::Path) -> String {
        let bytes = std::fs::read(out.join("settings.json")).expect("read settings");
        let settings: serde_json::Value = serde_json::from_slice(&bytes).expect("parse settings");
        settings["statusLine"]["command"]
            .as_str()
            .expect("statusLine command")
            .to_owned()
    }

    #[test]
    fn seed_status_line_migrates_the_bare_llmenv_command() {
        let out = tempfile::tempdir().expect("tempdir");
        // The marker key is not part of a seeded entry, so it is gone only if the entry was
        // rewritten. The seeded command depends on PATH, so the test does not compare it.
        let before = format!(
            r#"{{"statusLine":{{"type":"command","command":"{BARE_SEEDED_STATUS_COMMAND}","marker":1}}}}"#
        );
        std::fs::write(out.path().join("settings.json"), before).expect("write settings");

        seed_status_line(out.path()).expect("seed");

        let bytes = std::fs::read(out.path().join("settings.json")).expect("read settings");
        let settings: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert!(
            settings["statusLine"].get("marker").is_none(),
            "the bare entry was kept: {settings}"
        );
        let command = seeded_command(out.path());
        assert!(command.ends_with("statusline --color always"), "{command}");
    }

    #[test]
    fn seed_status_line_keeps_a_user_command() {
        let out = tempfile::tempdir().expect("tempdir");
        let before = r#"{"statusLine":{"type":"command","command":"my-bar --wide"}}"#;
        std::fs::write(out.path().join("settings.json"), before).expect("write settings");

        seed_status_line(out.path()).expect("seed");

        assert_eq!(seeded_command(out.path()), "my-bar --wide");
    }
}
