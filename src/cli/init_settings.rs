//! The settings import of `llmenv init`: pick keys from `~/.claude/settings.json` and seed them.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::config::Config;
use crate::paths;

use crate::onboarding::config_splice::{Addition, splice};

/// A settings key that the user may pick, with its value.
type Candidate = (String, serde_json::Value);

/// Interactive settings import for `llmenv init` (#172).
///
/// Reads `~/.claude/settings.json`, presents the non-owned keys as a checkbox
/// list, and stores selected keys in `config.yaml` under `init.seeded_settings`.
pub(super) fn run_init_settings_prompt(config_path: &Path) -> anyhow::Result<()> {
    let global_settings = PathBuf::from(paths::expand_tilde("~/.claude")?).join("settings.json");
    import_settings(config_path, &global_settings, prompt_for_keys)
}

/// The import with the choice injected. `choose` gets the labels and returns the picked indices,
/// or `None` when the user cancels. The prompt is the only interactive part, so a test passes its
/// own choice.
fn import_settings(
    config_path: &Path,
    settings: &Path,
    choose: impl FnOnce(&[String]) -> anyhow::Result<Option<Vec<usize>>>,
) -> anyhow::Result<()> {
    let Some(candidates) = read_candidates(settings) else {
        return Ok(());
    };
    let labels: Vec<String> = candidates
        .iter()
        .map(|(k, v)| format!("{k} = {v}"))
        .collect();
    let Some(indices) = choose(&labels)? else {
        return Ok(());
    };
    if indices.is_empty() {
        return Ok(());
    }
    write_picks(config_path, &candidates, &indices)
}

/// The non-owned keys of the settings file. `None` means there is nothing to offer, and a
/// warning names any file that cannot be used.
fn read_candidates(settings: &Path) -> Option<Vec<Candidate>> {
    use crate::adapter::claude_code::LLMENV_OWNED_SETTINGS_KEYS;
    // #918: NotFound → no settings to import (silent); a permission error
    // surfaces a warning instead of an exists() stat masking it as absent.
    let bytes = match std::fs::read(settings) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!(
                "warning: could not read {} — skipping settings import: {e}",
                settings.display()
            );
            return None;
        }
    };
    let doc: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "warning: {} is not valid JSON — skipping settings import: {e}",
                settings.display()
            );
            return None;
        }
    };
    let obj = doc.as_object()?;
    let candidates: Vec<Candidate> = obj
        .iter()
        .filter(|(k, _)| !LLMENV_OWNED_SETTINGS_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!candidates.is_empty()).then_some(candidates)
}

/// The checkbox prompt. `None` when the user cancels.
fn prompt_for_keys(labels: &[String]) -> anyhow::Result<Option<Vec<usize>>> {
    dialoguer::MultiSelect::new()
        .with_prompt(
            "Select settings from ~/.claude/settings.json to seed into new folders \
             (space to toggle, enter to confirm)",
        )
        .items(labels)
        .interact_opt()
        .map_err(|e| anyhow::anyhow!("settings prompt failed: {e}"))
}

/// Splice the picked keys into the config. The config must parse first, so a broken file is not
/// rewritten.
fn write_picks(
    config_path: &Path,
    candidates: &[Candidate],
    indices: &[usize],
) -> anyhow::Result<()> {
    // Check that the config parses. Then splice the keys in as text. Re-serializing the whole
    // document would drop the comments of the template (#2964).
    Config::load(config_path)?;
    let old = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    let picks: Vec<(&String, &serde_json::Value)> = indices
        .iter()
        .map(|&idx| (&candidates[idx].0, &candidates[idx].1))
        .collect();
    let new_text = with_seeded_settings(&old, &picks)?;
    paths::write_user_file_atomic(config_path, new_text.as_bytes())
        .with_context(|| format!("writing config {}", config_path.display()))?;
    eprintln!(
        "✓ {} setting(s) added to init.seeded_settings in config.yaml",
        indices.len()
    );
    Ok(())
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
    use super::{import_settings, with_seeded_settings};
    use std::path::Path;

    /// A chooser that must not run. Reaching it fails the test with its message.
    fn no_prompt(_: &[String]) -> anyhow::Result<Option<Vec<usize>>> {
        Err(anyhow::anyhow!("the prompt must not run here"))
    }

    fn settings_file(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("settings.json"), body).expect("write settings");
        dir
    }

    fn config_file() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("config.yaml"), "log_level: info\n").expect("write config");
        dir
    }

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
    fn a_missing_settings_file_offers_nothing() {
        let config = config_file();
        let missing = Path::new("absent-settings.json");
        import_settings(&config.path().join("config.yaml"), missing, no_prompt)
            .expect("no settings file is not an error");
    }

    #[test]
    fn invalid_json_offers_nothing() {
        let settings = settings_file("{ not json");
        let config = config_file();
        import_settings(
            &config.path().join("config.yaml"),
            &settings.path().join("settings.json"),
            no_prompt,
        )
        .expect("invalid JSON is a warning, not an error");
    }

    #[test]
    fn only_llmenv_owned_keys_offer_nothing() {
        let settings = settings_file(r#"{"hooks": {}}"#);
        let config = config_file();
        import_settings(
            &config.path().join("config.yaml"),
            &settings.path().join("settings.json"),
            no_prompt,
        )
        .expect("owned keys are never offered, so the prompt does not run");
    }

    #[test]
    fn a_cancelled_prompt_leaves_the_config_unchanged() {
        let settings = settings_file(r#"{"model": "on"}"#);
        let config = config_file();
        let config_path = config.path().join("config.yaml");
        import_settings(&config_path, &settings.path().join("settings.json"), |_| {
            Ok(None)
        })
        .expect("a cancelled prompt is not an error");
        let text = std::fs::read_to_string(&config_path).expect("read config");
        assert_eq!(text, "log_level: info\n");
    }

    #[test]
    fn a_picked_key_is_seeded_into_the_config() {
        let settings = settings_file(r#"{"model": "on", "theme": "dark"}"#);
        let config = config_file();
        let config_path = config.path().join("config.yaml");
        import_settings(
            &config_path,
            &settings.path().join("settings.json"),
            |labels| {
                let model = labels
                    .iter()
                    .position(|l| l.starts_with("model"))
                    .expect("model is offered");
                Ok(Some(vec![model]))
            },
        )
        .expect("a pick seeds the config");
        let text = std::fs::read_to_string(&config_path).expect("read config");
        assert!(text.contains("seeded_settings"), "{text}");
        assert!(
            text.contains("model: \"on\"") || text.contains("model: on"),
            "{text}"
        );
        assert!(
            !text.contains("theme"),
            "only the picked key is seeded: {text}"
        );
    }
}
