//! The settings import of `llmenv init`: pick keys from `~/.claude/settings.json` and seed them.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::config::Config;
use crate::paths;

use crate::onboarding::config_splice::{Addition, splice};

/// Interactive settings import for `llmenv init` (#172).
///
/// Reads `~/.claude/settings.json`, presents the non-owned keys as a checkbox
/// list, and stores selected keys in `config.yaml` under `init.seeded_settings`.
pub(super) fn run_init_settings_prompt(config_path: &Path) -> anyhow::Result<()> {
    use dialoguer::MultiSelect;

    let global_settings = PathBuf::from(paths::expand_tilde("~/.claude")?).join("settings.json");
    let Some(obj) = read_settings_object(&global_settings) else {
        return Ok(());
    };
    let candidates = import_candidates(&obj);
    if candidates.is_empty() {
        return Ok(());
    }

    let labels: Vec<String> = candidates
        .iter()
        .map(|(k, v)| format!("{k} = {v}"))
        .collect();
    let chosen = MultiSelect::new()
        .with_prompt(
            "Select settings from ~/.claude/settings.json to seed into new folders \
             (space to toggle, enter to confirm)",
        )
        .items(&labels)
        .interact_opt()
        .map_err(|e| anyhow::anyhow!("settings prompt failed: {e}"))?;

    let Some(indices) = chosen else {
        return Ok(());
    };
    let picks: Vec<(&String, &serde_json::Value)> = indices
        .into_iter()
        .filter_map(|idx| candidates.get(idx).copied())
        .collect();
    if picks.is_empty() {
        return Ok(());
    }
    seed_settings(config_path, &picks)?;
    eprintln!(
        "✓ {} setting(s) added to init.seeded_settings in config.yaml",
        picks.len()
    );
    Ok(())
}

/// The top-level object of the settings file at `path`, or `None` when there is nothing to import.
///
/// A missing file is silent. An unreadable file or a file that is not JSON gives a warning, so
/// a permission error does not look like an absent file (#918).
fn read_settings_object(path: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!(
                "warning: could not read {} — skipping settings import: {e}. \
                 Fix: make the file readable, or remove it to skip the import.",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(serde_json::Value::Object(obj)) => Some(obj),
        Ok(_) => {
            eprintln!(
                "warning: {} is not a JSON object — skipping settings import. \
                 Fix: keep the settings as one JSON object.",
                path.display()
            );
            None
        }
        Err(e) => {
            eprintln!(
                "warning: {} is not valid JSON — skipping settings import: {e}. \
                 Fix: correct the JSON, or remove the file to skip the import.",
                path.display()
            );
            None
        }
    }
}

/// The settings keys that a user can import. llmenv writes its own keys, so those are left out.
fn import_candidates(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Vec<(&String, &serde_json::Value)> {
    use crate::adapter::claude_code::LLMENV_OWNED_SETTINGS_KEYS;
    obj.iter()
        .filter(|(k, _)| !LLMENV_OWNED_SETTINGS_KEYS.contains(&k.as_str()))
        .collect()
}

/// Add `picks` under `init.seeded_settings` in the config at `config_path`.
///
/// # Errors
/// The config does not parse, or it cannot be read or written.
fn seed_settings(
    config_path: &Path,
    picks: &[(&String, &serde_json::Value)],
) -> anyhow::Result<()> {
    // Check that the config parses. Then splice the keys in as text. Re-serializing the whole
    // document would drop the comments of the template (#2964).
    Config::load(config_path)?;
    let old = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    let new_text = with_seeded_settings(&old, picks)?;
    paths::write_user_file_atomic(config_path, new_text.as_bytes())
        .with_context(|| format!("writing config {}", config_path.display()))
}

/// The config text with the picked keys added under `init.seeded_settings`. The keys are
/// spliced in as text, so the comments of `old` stay (#2964).
fn with_seeded_settings(
    old: &str,
    picks: &[(&String, &serde_json::Value)],
) -> anyhow::Result<String> {
    let mut keys = serde_yaml::Mapping::new();
    for (key, val) in picks {
        let yaml_val = serde_yaml::to_value(val).context("serializing a seeded setting")?;
        keys.insert(serde_yaml::Value::from(key.as_str()), yaml_val);
    }
    let addition = Addition::Keys {
        key: "init",
        sub: Some("seeded_settings"),
        keys,
    };
    let (new_text, _diff) = splice(old, &[addition])?;
    Ok(new_text)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::{import_candidates, read_settings_object, seed_settings, with_seeded_settings};

    #[test]
    fn seeded_settings_keep_the_template_comments() {
        let old = "# guidance for the user\nlog_level: info\n";
        let key = "model".to_string();
        let value = serde_json::json!("on");

        let text = with_seeded_settings(old, &[(&key, &value)]).expect("splice");

        assert!(text.contains("# guidance for the user"), "{text}");
        assert!(text.contains("model: on"), "{text}");
    }

    #[test]
    fn a_missing_non_json_or_non_object_settings_file_gives_nothing_to_import() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        assert!(read_settings_object(&path).is_none());
        std::fs::write(&path, "{ not json").expect("write");
        assert!(read_settings_object(&path).is_none());
        std::fs::write(&path, "[1, 2]").expect("write");
        assert!(read_settings_object(&path).is_none());
        std::fs::write(&path, r#"{"model": "opus"}"#).expect("write");
        assert_eq!(
            read_settings_object(&path).expect("an object").get("model"),
            Some(&serde_json::json!("opus"))
        );
    }

    #[test]
    fn an_unreadable_settings_path_gives_nothing_to_import() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_settings_object(dir.path()).is_none());
    }

    #[test]
    fn import_candidates_leave_out_the_keys_llmenv_writes() {
        let obj = serde_json::json!({"permissions": {}, "hooks": {}, "model": "opus"});
        let obj = obj.as_object().expect("an object");
        let keys: Vec<&str> = import_candidates(obj)
            .into_iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, vec!["model"]);
    }

    #[test]
    fn seed_settings_writes_the_picks_and_keeps_the_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, "# keep me\n").expect("write");
        let key = "model".to_string();
        let value = serde_json::json!("opus");

        seed_settings(&config, &[(&key, &value)]).expect("seed");

        let text = std::fs::read_to_string(&config).expect("read");
        assert!(text.contains("# keep me"), "{text}");
        assert!(text.contains("model: opus"), "{text}");
    }

    #[test]
    fn seed_settings_refuses_a_config_that_does_not_parse() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, "log_level: [unclosed\n").expect("write");
        let key = "model".to_string();
        let value = serde_json::json!("opus");

        assert!(seed_settings(&config, &[(&key, &value)]).is_err());
        let text = std::fs::read_to_string(&config).expect("read");
        assert_eq!(text, "log_level: [unclosed\n");
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod property_tests {
    use super::{import_candidates, with_seeded_settings};
    use crate::adapter::claude_code::LLMENV_OWNED_SETTINGS_KEYS;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    /// A JSON value of up to two levels, with awkward strings: colons, quotes, hashes, newlines.
    fn json_value() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i32>().prop_map(|n| serde_json::json!(n)),
            "[ -~\n]{0,24}".prop_map(serde_json::Value::String),
        ];
        leaf.prop_recursive(2, 16, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
                proptest::collection::btree_map("[a-z]{1,6}", inner, 0..4)
                    .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
            ]
        })
    }

    proptest! {
        /// The offered keys are exactly the keys that llmenv does not own.
        #[test]
        fn offered_keys_are_exactly_the_non_owned_keys(
            extra in proptest::collection::btree_set("[a-z]{1,8}", 0..6),
            owned_mask in proptest::collection::vec(any::<bool>(), LLMENV_OWNED_SETTINGS_KEYS.len()),
        ) {
            let mut obj = serde_json::Map::new();
            for (key, take) in LLMENV_OWNED_SETTINGS_KEYS.iter().zip(owned_mask) {
                if take {
                    obj.insert((*key).to_string(), serde_json::json!(1));
                }
            }
            for key in &extra {
                obj.insert(key.clone(), serde_json::json!(1));
            }
            let offered: BTreeSet<String> =
                import_candidates(&obj).into_iter().map(|(k, _)| k.clone()).collect();
            let expected: BTreeSet<String> = obj
                .keys()
                .filter(|k| !LLMENV_OWNED_SETTINGS_KEYS.contains(&k.as_str()))
                .cloned()
                .collect();
            prop_assert_eq!(offered, expected);
        }

        /// A seeded value reads back equal from the spliced config, whatever its shape.
        #[test]
        fn a_seeded_value_reads_back_equal_after_the_splice(value in json_value()) {
            let key = "model".to_string();
            let text = with_seeded_settings("log_level: info\n", &[(&key, &value)])
                .expect("the splice accepts any JSON value");
            let parsed: serde_yaml::Value =
                serde_yaml::from_str(&text).expect("the spliced config parses as YAML");
            let read_back = serde_json::to_value(&parsed["init"]["seeded_settings"]["model"])
                .expect("a YAML value converts to JSON");
            prop_assert_eq!(read_back, value);
        }
    }
}
