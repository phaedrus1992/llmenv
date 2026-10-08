//! Per-model effort entries in Claude Code's `modelSettings` (#2144).
//!
//! `modelSettings` is shared with Claude Code's own `/effort`, which writes into
//! the same `settings.json`. llmenv records the entries it wrote in a companion
//! file, so the next render can remove its own stale fields and keep the user's.
//! Design: docs/design/issue-2144-per-model-effort.md

use std::path::Path;

use serde_json::{Map, Value};

use crate::config::Capabilities;
use crate::util::merge_json;

/// Models that ignore a top-level `effortLevel` in user settings (Claude Code
/// docs, `effortLevel`). Add each new Claude model here when it ships.
const PER_MODEL_EFFORT_MODELS: &[&str] = &["claude-opus-5-5", "claude-haiku-5-5"];

/// Companion file next to `settings.json`: the `modelSettings` entries llmenv
/// wrote on the previous render, as one JSON object.
pub(crate) const OWNED_MODEL_SETTINGS_FILE: &str = "settings.json.llmenv-owned-model-settings";

const MODEL_SETTINGS_KEY: &str = "modelSettings";

/// One render's `modelSettings` work, split around `reconcile_settings`.
#[derive(Debug)]
pub(crate) struct ModelSettingsMerge {
    managed: Map<String, Value>,
    prev: Map<String, Value>,
    native: Option<Value>,
}

impl ModelSettingsMerge {
    /// Take `modelSettings` out of the fresh `settings` doc and read the
    /// companion file. The companion file is read only when `settings.json`
    /// exists, because a first render has no earlier llmenv entries.
    ///
    /// # Errors
    /// Returns an error when the companion file exists but cannot be read.
    pub(crate) fn prepare(
        out: &Path,
        settings: &mut Value,
        caps: &Capabilities,
    ) -> anyhow::Result<Self> {
        let native = settings
            .as_object_mut()
            .and_then(|o| o.remove(MODEL_SETTINGS_KEY));
        let prev = if out.join("settings.json").exists() {
            read_owned(out)?
        } else {
            Map::new()
        };
        Ok(Self {
            managed: managed_entries(caps),
            prev,
            native,
        })
    }

    /// Merge the managed entries into the reconciled doc. A native
    /// `modelSettings` goes on top, because native is the highest-precedence layer.
    ///
    /// # Errors
    /// Returns an error when `reconciled` is not a JSON object.
    pub(crate) fn apply(&self, reconciled: &mut Value) -> anyhow::Result<()> {
        let Some(obj) = reconciled.as_object_mut() else {
            anyhow::bail!(
                "rendered settings.json is not a JSON object; cannot merge modelSettings"
            );
        };
        merge_model_settings(obj, &self.managed, &self.prev);
        if let Some(native) = &self.native {
            let target = obj
                .entry(MODEL_SETTINGS_KEY)
                .or_insert_with(|| Value::Object(Map::new()));
            merge_json(target, native.clone());
        }
        Ok(())
    }

    /// Record the managed entries for the next render.
    ///
    /// # Errors
    /// Returns an error when the companion file write or remove fails.
    pub(crate) fn persist(&self, out: &Path) -> anyhow::Result<()> {
        write_owned(out, &self.managed).map_err(|e| {
            anyhow::anyhow!(
                "{e:#}. settings.json is updated, but llmenv has no record of its \
                 modelSettings entries; check the permissions of {} and run \
                 `llmenv regenerate` again",
                out.display()
            )
        })
    }
}

/// Build the `modelSettings` entries llmenv manages for `caps`.
///
/// `effort_level` goes to every model in [`PER_MODEL_EFFORT_MODELS`]. A
/// `model_effort` entry replaces those fields for its model.
fn managed_entries(caps: &Capabilities) -> Map<String, Value> {
    let mut managed = Map::new();
    if let Some(level) = &caps.effort_level {
        for id in PER_MODEL_EFFORT_MODELS {
            let mut entry = Map::new();
            entry.insert("effortLevel".into(), Value::String(level.clone()));
            managed.insert((*id).to_string(), Value::Object(entry));
        }
    }
    for (id, effort) in &caps.model_effort {
        let mut entry = Map::new();
        if let Some(level) = &effort.effort_level {
            entry.insert("effortLevel".into(), Value::String(level.clone()));
        }
        if let Some(level) = &effort.max_effort_level {
            entry.insert("maxEffortLevel".into(), Value::String(level.clone()));
        }
        managed.insert(id.clone(), Value::Object(entry));
    }
    managed
}

/// Merge `managed` into the `modelSettings` of `settings`.
///
/// `prev` is what llmenv wrote on the previous render. A field that llmenv
/// wrote before and does not write now is removed only when its value is
/// still the one llmenv wrote; a different value came from `/effort` and stays.
/// A model entry that llmenv touched and that is now empty is removed.
/// Entries for other models are not touched.
fn merge_model_settings(
    settings: &mut Map<String, Value>,
    managed: &Map<String, Value>,
    prev: &Map<String, Value>,
) {
    // With nothing to write or clean up, the key belongs to Claude Code alone.
    if managed.is_empty() && prev.is_empty() {
        return;
    }
    let mut current = match settings.remove(MODEL_SETTINGS_KEY) {
        Some(Value::Object(map)) => map,
        Some(other) => {
            // The default log filter drops warn!, so print the loss on stderr.
            eprintln!(
                "llmenv: settings.json modelSettings is not a JSON object ({other}); \
                 replacing it with the entries from llmenv config"
            );
            Map::new()
        }
        None => Map::new(),
    };
    for (id, prev_entry) in prev {
        let Some(prev_fields) = prev_entry.as_object() else {
            continue;
        };
        let now = managed.get(id).and_then(Value::as_object);
        if let Some(Value::Object(entry)) = current.get_mut(id) {
            for (field, prev_value) in prev_fields {
                let still_managed = now.is_some_and(|n| n.contains_key(field));
                if !still_managed && entry.get(field) == Some(prev_value) {
                    entry.remove(field);
                }
            }
        }
    }
    for (id, fields) in managed {
        let Some(fields) = fields.as_object() else {
            continue;
        };
        let entry = current
            .entry(id.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        if let Value::Object(entry) = entry {
            for (field, value) in fields {
                entry.insert(field.clone(), value.clone());
            }
        }
    }
    let touched = prev.keys().chain(managed.keys());
    let empty: Vec<String> = touched
        .filter(|id| {
            current
                .get(*id)
                .and_then(Value::as_object)
                .is_some_and(Map::is_empty)
        })
        .cloned()
        .collect();
    for id in empty {
        current.remove(&id);
    }
    if !current.is_empty() {
        settings.insert(MODEL_SETTINGS_KEY.into(), Value::Object(current));
    }
}

/// Read the companion file in `out`. An absent file gives an empty map.
///
/// A file that does not parse also gives an empty map, with a message on
/// stderr: the next write replaces it, so the render goes on.
///
/// # Errors
/// Returns an error when the file exists but cannot be read.
fn read_owned(out: &Path) -> anyhow::Result<Map<String, Value>> {
    let path = out.join(OWNED_MODEL_SETTINGS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => anyhow::bail!(
            "cannot read {}: {e}; check its permissions, or remove it and check \
             modelSettings in settings.json by hand",
            path.display()
        ),
    };
    match serde_json::from_str::<Map<String, Value>>(&text) {
        Ok(map) => Ok(map),
        Err(e) => {
            // The default log filter drops warn!, so print this on stderr.
            eprintln!(
                "llmenv: cannot parse {} ({e}); llmenv cannot tell its own modelSettings \
                 entries from /effort entries this time. Check modelSettings in \
                 settings.json by hand",
                path.display()
            );
            Ok(Map::new())
        }
    }
}

/// Write `managed` to the companion file in `out`, or remove the file when
/// `managed` is empty.
///
/// # Errors
/// Returns an error when the atomic write or the remove fails. A stale file
/// would claim fields that a later `/effort` writes, and delete them.
fn write_owned(out: &Path, managed: &Map<String, Value>) -> anyhow::Result<()> {
    let path = out.join(OWNED_MODEL_SETTINGS_FILE);
    if managed.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => anyhow::bail!("cannot remove {}: {e}", path.display()),
        }
        return Ok(());
    }
    let json = serde_json::to_string_pretty(managed)?;
    crate::paths::write_owner_only_atomic(&path, json.as_bytes())
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test scaffolding"
)]
mod tests {
    use super::*;
    use crate::config::ModelEffort;
    use proptest::prelude::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            other => panic!("not an object: {other}"),
        }
    }

    fn effort(effort: Option<&str>, max: Option<&str>) -> ModelEffort {
        ModelEffort {
            effort_level: effort.map(str::to_string),
            max_effort_level: max.map(str::to_string),
        }
    }

    #[test]
    fn effort_level_reaches_the_per_model_list() {
        let caps = Capabilities {
            effort_level: Some("high".into()),
            ..Capabilities::default()
        };
        assert_eq!(
            Value::Object(managed_entries(&caps)),
            json!({
                "claude-opus-5-5": {"effortLevel": "high"},
                "claude-haiku-5-5": {"effortLevel": "high"},
            })
        );
    }

    #[test]
    fn model_effort_replaces_the_default_for_its_model() {
        let caps = Capabilities {
            effort_level: Some("high".into()),
            model_effort: [
                ("claude-opus-5-5".to_string(), effort(None, Some("xhigh"))),
                ("claude-fable-5-1".to_string(), effort(Some("low"), None)),
            ]
            .into_iter()
            .collect(),
            ..Capabilities::default()
        };
        assert_eq!(
            Value::Object(managed_entries(&caps)),
            json!({
                "claude-opus-5-5": {"maxEffortLevel": "xhigh"},
                "claude-fable-5-1": {"effortLevel": "low"},
                "claude-haiku-5-5": {"effortLevel": "high"},
            })
        );
    }

    #[test]
    fn no_effort_config_manages_nothing() {
        assert!(managed_entries(&Capabilities::default()).is_empty());
    }

    #[test]
    fn fresh_file_gets_the_managed_entries() {
        let mut settings = Map::new();
        let managed = obj(json!({"claude-opus-5-5": {"effortLevel": "high"}}));
        merge_model_settings(&mut settings, &managed, &Map::new());
        assert_eq!(
            settings["modelSettings"],
            json!({"claude-opus-5-5": {"effortLevel": "high"}})
        );
    }

    #[test]
    fn foreign_entry_from_effort_command_survives() {
        let mut settings =
            obj(json!({"modelSettings": {"claude-sonnet-5": {"effortLevel": "low"}}}));
        let managed = obj(json!({"claude-opus-5-5": {"effortLevel": "high"}}));
        merge_model_settings(&mut settings, &managed, &Map::new());
        assert_eq!(
            settings["modelSettings"],
            json!({
                "claude-sonnet-5": {"effortLevel": "low"},
                "claude-opus-5-5": {"effortLevel": "high"},
            })
        );
    }

    #[test]
    fn stale_entry_goes_when_unchanged() {
        let prev = obj(json!({"claude-fable-5-1": {"maxEffortLevel": "high"}}));
        let mut settings = obj(json!({"modelSettings": prev.clone()}));
        merge_model_settings(&mut settings, &Map::new(), &prev);
        assert!(!settings.contains_key("modelSettings"), "{settings:?}");
    }

    #[test]
    fn stale_entry_stays_when_the_user_changed_it() {
        let prev = obj(json!({"claude-fable-5-1": {"maxEffortLevel": "high"}}));
        let mut settings =
            obj(json!({"modelSettings": {"claude-fable-5-1": {"maxEffortLevel": "low"}}}));
        merge_model_settings(&mut settings, &Map::new(), &prev);
        assert_eq!(
            settings["modelSettings"],
            json!({"claude-fable-5-1": {"maxEffortLevel": "low"}})
        );
    }

    #[test]
    fn user_field_in_a_managed_entry_survives() {
        let prev = obj(json!({"claude-opus-5-5": {"maxEffortLevel": "xhigh"}}));
        let mut settings = obj(json!({"modelSettings": {
            "claude-opus-5-5": {"maxEffortLevel": "xhigh", "effortLevel": "low"}
        }}));
        merge_model_settings(&mut settings, &prev, &prev);
        assert_eq!(
            settings["modelSettings"],
            json!({"claude-opus-5-5": {"maxEffortLevel": "xhigh", "effortLevel": "low"}})
        );
    }

    #[test]
    fn dropped_field_goes_and_user_field_stays() {
        let prev = obj(json!({"claude-opus-5-5": {"maxEffortLevel": "xhigh"}}));
        let mut settings = obj(json!({"modelSettings": {
            "claude-opus-5-5": {"maxEffortLevel": "xhigh", "effortLevel": "low"}
        }}));
        merge_model_settings(&mut settings, &Map::new(), &prev);
        assert_eq!(
            settings["modelSettings"],
            json!({"claude-opus-5-5": {"effortLevel": "low"}})
        );
    }

    #[test]
    fn config_value_wins_over_an_earlier_effort_save() {
        let mut settings =
            obj(json!({"modelSettings": {"claude-opus-5-5": {"effortLevel": "low"}}}));
        let managed = obj(json!({"claude-opus-5-5": {"effortLevel": "high"}}));
        merge_model_settings(&mut settings, &managed, &managed);
        assert_eq!(
            settings["modelSettings"],
            json!({"claude-opus-5-5": {"effortLevel": "high"}})
        );
    }

    #[test]
    fn malformed_model_settings_is_replaced() {
        let mut settings = obj(json!({"modelSettings": "bad"}));
        let managed = obj(json!({"claude-opus-5-5": {"effortLevel": "high"}}));
        merge_model_settings(&mut settings, &managed, &Map::new());
        assert_eq!(
            settings["modelSettings"],
            json!({"claude-opus-5-5": {"effortLevel": "high"}})
        );
    }

    #[test]
    fn foreign_model_settings_is_untouched_when_nothing_is_managed() {
        let mut settings = obj(json!({"modelSettings": "future-shape"}));
        merge_model_settings(&mut settings, &Map::new(), &Map::new());
        assert_eq!(settings["modelSettings"], json!("future-shape"));
    }

    // A companion path that cannot be read must stop the render, not reset the record.
    #[test]
    fn unreadable_owned_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(OWNED_MODEL_SETTINGS_FILE)).expect("mkdir");
        let err = read_owned(dir.path()).unwrap_err();
        assert!(err.to_string().contains("cannot read"), "{err}");
    }

    // A stale companion that cannot be removed would later delete an /effort save.
    #[test]
    fn owned_file_remove_failure_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(OWNED_MODEL_SETTINGS_FILE);
        std::fs::create_dir(&path).expect("mkdir");
        std::fs::write(path.join("x"), "").expect("write");
        let err = write_owned(dir.path(), &Map::new()).unwrap_err();
        assert!(err.to_string().contains("cannot remove"), "{err}");
    }

    #[test]
    fn apply_rejects_a_non_object_doc() {
        let merge = ModelSettingsMerge {
            managed: Map::new(),
            prev: Map::new(),
            native: None,
        };
        assert!(merge.apply(&mut json!([])).is_err());
    }

    #[test]
    fn owned_file_round_trips_and_clears() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_owned(dir.path()).expect("read").is_empty());
        let managed = obj(json!({"claude-opus-5-5": {"effortLevel": "high"}}));
        write_owned(dir.path(), &managed).expect("write");
        assert_eq!(read_owned(dir.path()).expect("read"), managed);
        write_owned(dir.path(), &Map::new()).expect("clear");
        assert!(!dir.path().join(OWNED_MODEL_SETTINGS_FILE).exists());
    }

    #[test]
    fn corrupt_owned_file_reads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(OWNED_MODEL_SETTINGS_FILE), "[1,").expect("write");
        assert!(read_owned(dir.path()).expect("read").is_empty());
    }

    fn level() -> impl Strategy<Value = Value> {
        prop::sample::select(vec!["low", "medium", "high", "xhigh"]).prop_map(Value::from)
    }

    fn entries() -> impl Strategy<Value = Map<String, Value>> {
        let id = prop::sample::select(vec!["claude-opus-5-5", "claude-fable-5-1", "claude-x"]);
        let field = prop::sample::select(vec!["effortLevel", "maxEffortLevel"]);
        prop::collection::btree_map(id, prop::collection::btree_map(field, level(), 0..3), 0..4)
            .prop_map(|m| {
                m.into_iter()
                    .map(|(id, fields)| {
                        let fields = fields
                            .into_iter()
                            .map(|(k, v)| (k.to_string(), v))
                            .collect();
                        (id.to_string(), Value::Object(fields))
                    })
                    .collect()
            })
    }

    proptest! {
        // A second render with the same config must not change the file.
        #[test]
        fn merge_is_idempotent(disk in entries(), prev in entries(), managed in entries()) {
            let mut once = Map::new();
            once.insert("modelSettings".into(), Value::Object(disk));
            merge_model_settings(&mut once, &managed, &prev);
            let mut twice = once.clone();
            merge_model_settings(&mut twice, &managed, &managed);
            prop_assert_eq!(once, twice);
        }

        // Every field llmenv manages ends up on disk with llmenv's value.
        #[test]
        fn managed_fields_always_land(disk in entries(), prev in entries(), managed in entries()) {
            let mut settings = Map::new();
            settings.insert("modelSettings".into(), Value::Object(disk));
            merge_model_settings(&mut settings, &managed, &prev);
            for (id, fields) in &managed {
                for (field, value) in fields.as_object().into_iter().flatten() {
                    prop_assert_eq!(&settings["modelSettings"][id][field], value);
                }
            }
        }
    }
}
