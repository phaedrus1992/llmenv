//! Small shared helpers with no better home.

use anstyle::{AnsiColor, Color, Style};
use sha2::{Digest, Sha256};

/// Shared length-prefix hashing convention: length-prefix every field before
/// its bytes so concatenation can't ambiguate boundaries. Used by
/// `materialize::cache::hash_manifest` and `merge::merge_signature` (#920).
pub fn update_len_prefixed(h: &mut Sha256, data: &[u8]) {
    h.update((data.len() as u64).to_le_bytes());
    h.update(data);
}

/// Wrap text in an ANSI style when `use_color` is set, else return it plain.
/// `pub`, not `pub(crate)`: `cli::style`'s remaining doctor/marker functions
/// call this from a different crate after this move.
pub fn paint(text: &str, color: AnsiColor, use_color: bool) -> String {
    if use_color {
        let style = Style::new().fg_color(Some(Color::Ansi(color)));
        format!("{style}{text}{style:#}")
    } else {
        text.to_string()
    }
}

/// Format a doctor "warning" symbol (⚠) with optional yellow color.
pub fn doctor_warning(use_color: bool) -> String {
    paint("⚠", AnsiColor::Yellow, use_color)
}

/// Color mode: auto-detect, always on, or always off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Auto-detect based on stdout TTY and NO_COLOR env var
    Auto,
    /// Force colors on
    Always,
    /// Force colors off
    Never,
}

/// Determine whether to emit colors based on flags, env vars, and TTY state.
pub fn should_use_color(mode: Option<ColorMode>, is_tty: bool) -> bool {
    should_use_color_with_env(mode, is_tty, &|name| std::env::var(name).ok())
}

fn should_use_color_with_env<F>(mode: Option<ColorMode>, is_tty: bool, get_env: &F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    let effective_mode = mode.unwrap_or(ColorMode::Auto);
    match effective_mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            if get_env("NO_COLOR").is_some() {
                return false;
            }
            if get_env("CLICOLOR_FORCE")
                .filter(|v| !v.is_empty())
                .is_some()
            {
                return true;
            }
            is_tty
        }
    }
}

#[cfg(test)]
mod color_tests {
    use super::*;

    #[test]
    fn test_should_use_color_always_mode() {
        assert!(should_use_color(Some(ColorMode::Always), false));
        assert!(should_use_color(Some(ColorMode::Always), true));
    }

    #[test]
    fn test_should_use_color_never_mode() {
        assert!(!should_use_color(Some(ColorMode::Never), false));
        assert!(!should_use_color(Some(ColorMode::Never), true));
    }

    #[test]
    fn test_should_use_color_auto_respects_tty() {
        assert!(!should_use_color(Some(ColorMode::Auto), false));
    }

    #[test]
    fn test_should_use_color_auto_with_tty_isolated() {
        let no_env = |_name: &str| -> Option<String> { None };
        assert!(!should_use_color_with_env(
            Some(ColorMode::Auto),
            false,
            &no_env
        ));
        assert!(should_use_color_with_env(
            Some(ColorMode::Auto),
            true,
            &no_env
        ));
    }

    #[test]
    fn test_should_use_color_no_color_overrides() {
        let no_color_env = |name: &str| -> Option<String> {
            match name {
                "NO_COLOR" => Some("1".to_string()),
                _ => None,
            }
        };
        assert!(!should_use_color_with_env(
            Some(ColorMode::Auto),
            true,
            &no_color_env
        ));
    }

    #[test]
    fn test_should_use_color_no_color_empty_string() {
        let no_color_empty_env = |name: &str| -> Option<String> {
            match name {
                "NO_COLOR" => Some(String::new()),
                _ => None,
            }
        };
        assert!(!should_use_color_with_env(
            Some(ColorMode::Auto),
            true,
            &no_color_empty_env
        ));
    }

    #[test]
    fn test_should_use_color_clicolor_force_overrides() {
        let force_env = |name: &str| -> Option<String> {
            match name {
                "CLICOLOR_FORCE" => Some("1".to_string()),
                _ => None,
            }
        };
        assert!(should_use_color_with_env(
            Some(ColorMode::Auto),
            false,
            &force_env
        ));
    }

    #[test]
    fn test_should_use_color_clicolor_force_empty_string_does_not_force() {
        let empty_force_env = |name: &str| -> Option<String> {
            match name {
                "CLICOLOR_FORCE" => Some(String::new()),
                _ => None,
            }
        };
        assert!(!should_use_color_with_env(
            Some(ColorMode::Auto),
            false,
            &empty_force_env
        ));
    }

    #[test]
    fn test_should_use_color_no_color_takes_precedence_over_clicolor_force() {
        let both_env = |name: &str| -> Option<String> {
            match name {
                "NO_COLOR" => Some("1".to_string()),
                "CLICOLOR_FORCE" => Some("1".to_string()),
                _ => None,
            }
        };
        assert!(!should_use_color_with_env(
            Some(ColorMode::Auto),
            true,
            &both_env
        ));
    }

    #[test]
    fn doctor_warning_plain_when_no_color() {
        assert_eq!(doctor_warning(false), "⚠");
    }

    #[test]
    fn doctor_warning_colored_contains_escape_code() {
        assert!(doctor_warning(true).contains('\u{1b}'));
    }
}

/// Stable dedup preserving first-seen order. Lists here are small (permission
/// rules, hooks, plugin ids), so the quadratic scan is fine and avoids
/// requiring `Hash`/`Ord` on every element type.
pub fn dedup<T: PartialEq>(items: &mut Vec<T>) {
    let mut i = 0;
    while i < items.len() {
        if items[..i].contains(&items[i]) {
            items.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Deep-merge `src` into `dst` for opaque per-engine `native` fragments.
///
/// llmenv never interprets these values, so the merge is purely structural and
/// follows the same value-shape rule as the typed capabilities (see
/// `docs/design/engine-capabilities.md`, D2):
///
/// - **Mappings** merge key-by-key — shared keys recurse, disjoint keys union.
/// - **Sequences** concatenate (`src` appended after `dst`), then dedup.
/// - **Scalars** (and any shape mismatch, e.g. mapping vs. sequence) are
///   overwritten by `src` — the later, higher-precedence contributor wins.
///   Contributors are fed lowest-precedence first, so `src` always outranks
///   `dst` on a scalar collision.  Type conflicts (e.g. `dst` is a mapping and
///   `src` is a scalar, or vice versa) are treated as a complete replacement:
///   there is no safe structural merge across types, so the higher-precedence
///   value wins unconditionally.  Callers such as the `native:` merge pipeline
///   should be aware that a contributor changing a key's type will clobber the
///   lower-precedence value entirely.
pub fn merge_yaml(dst: &mut serde_yaml::Value, src: serde_yaml::Value) {
    use serde_yaml::Value;
    match (dst, src) {
        (Value::Mapping(d), Value::Mapping(s)) => {
            for (k, mut v) in s {
                match d.get_mut(&k) {
                    Some(existing) => merge_yaml(existing, v),
                    None => {
                        // Normalize the freshly-inserted subtree the same way the
                        // recursive-merge path would, so every sequence the merge
                        // produces is dedup-free regardless of which path created
                        // it. Without this, an inserted sequence keeps its own
                        // duplicates while a merged one drops them, making the
                        // overall merge non-idempotent.
                        normalize_yaml(&mut v);
                        // Skip null-valued keys — normalize_yaml strips them from
                        // mappings, so introducing one on the insert path would
                        // make merge_yaml non-idempotent (re-merging the same src
                        // would re-insert a null that was just stripped).
                        if !v.is_null() {
                            d.insert(k, v);
                        }
                    }
                }
            }
        }
        (Value::Sequence(d), Value::Sequence(s)) => {
            d.extend(s);
            for item in d.iter_mut() {
                normalize_yaml(item);
            }
            dedup(d);
        }
        (dst, src) => {
            *dst = src;
            normalize_yaml(dst);
        }
    }
}

/// Recursively normalize a YAML value for stable `PartialEq` comparison.
///
/// 1. **Dedup sequences** — removes duplicate entries.
/// 2. **Strip null-valued mapping keys** — removes `~` (null) entries, so
///    mappings differing only by null vs absent key collapse during
///    [`merge_yaml`]'s `PartialEq`-based dedup across merge generations.
///
/// Used on insert/overwrite paths to keep every [`merge_yaml`] caller
/// null-tolerant without per-caller post-processing.  Mirrors
/// [`normalize_json`] — the two must stay in sync so that YAML-shaped
/// `native` fragments behave the same as JSON-shaped ones.
pub fn normalize_yaml(value: &mut serde_yaml::Value) {
    use serde_yaml::Value;
    match value {
        Value::Sequence(items) => {
            for item in items.iter_mut() {
                normalize_yaml(item);
            }
            dedup(items);
        }
        Value::Mapping(map) => {
            // Strip null-valued entries so merge_yaml's PartialEq-based dedup
            // collapses mappings differing only by null vs absent across merge
            // generations (mirrors serialize_json's normalize_json).
            map.retain(|_, v| !v.is_null());
            for (_, v) in map.iter_mut() {
                normalize_yaml(v);
            }
        }
        _ => {}
    }
}

/// Deep-merge `src` into `dst` for JSON-shaped engine-native config.
///
/// The JSON analogue of [`merge_yaml`]: adapters build engine config (e.g.
/// `settings.json`, `mcp.json`) as [`serde_json::Value`], then overlay a
/// per-engine `native_*` fragment converted from YAML. Same value-shape rule:
///
/// - **Objects** merge key-by-key — shared keys recurse, disjoint keys union.
///   Disjoint keys from `src` skip null-valued entries (see insert path below).
///   Shared-key overwrites from `src` *do not* null-strip — a source that
///   explicitly sets a key to `null` is treated as intentional, not as an
///   `Option::None` serialization artifact.
/// - **Arrays** concatenate (`src` after `dst`), then dedup.
/// - **Scalars** and any shape mismatch are overwritten by `src` — the native
///   fragment is the higher-precedence overlay, so it wins on collision.
pub fn merge_json(dst: &mut serde_json::Value, src: serde_json::Value) {
    use serde_json::Value;
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, mut v) in s {
                match d.get_mut(&k) {
                    Some(existing) => merge_json(existing, v),
                    None => {
                        // Normalize the freshly-inserted subtree so every array
                        // the merge produces is dedup-free regardless of which
                        // path created it (see `merge_yaml` for the rationale).
                        normalize_json(&mut v);
                        // Skip null-valued keys — normalize_json strips them
                        // from objects, so introducing one on the insert path
                        // would make merge_json non-idempotent (re-merging
                        // the same src would re-insert a null that was just
                        // stripped).
                        if !v.is_null() {
                            d.insert(k, v);
                        }
                    }
                }
            }
        }
        (Value::Array(d), Value::Array(s)) => {
            d.extend(s);
            for item in d.iter_mut() {
                normalize_json(item);
            }
            dedup(d);
        }
        (dst, src) => {
            *dst = src;
            normalize_json(dst);
        }
    }
}

/// Recursively normalize a JSON value for stable `PartialEq` comparison.
///
/// 1. **Dedup arrays** — removes duplicate entries.
/// 2. **Strip null-valued object keys** — removes entries where the value is
///    `null`, so objects differing only by null vs absent key collapse during
///    [`merge_json`]'s `PartialEq`-based dedup across render generations.
///
/// Used on insert/overwrite paths to keep every `merge_json` caller
/// null-tolerant without per-caller post-processing.
fn normalize_json(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Array(items) => {
            for item in items.iter_mut() {
                normalize_json(item);
            }
            dedup(items);
        }
        Value::Object(map) => {
            // Strip null-valued keys so merge_json's PartialEq-based dedup
            // collapses objects differing only by null vs absent across
            // render generations (e.g. `"tool": null` vs absent key).
            map.retain(|_, v| !v.is_null());
            for (_, v) in map.iter_mut() {
                normalize_json(v);
            }
        }
        _ => {}
    }
}

/// Shared `proptest` generators for llmenv's own tests and, via the
/// `test-util` feature, for other workspace crates' dev-dependencies
/// (`llmenv-config`, the main `llmenv` crate). Exists so generators that
/// were drifting apart as separate copies in each crate (#1281) — same
/// shape, no way to import one from the other across the crate boundary —
/// have one canonical home instead.
///
/// `cfg(any(test, feature = "test-util"))` rather than plain `cfg(test)`:
/// `cfg(test)` is only active while compiling *this* crate's own test
/// target, so a downstream crate's tests never see it. `test-util` is the
/// escape hatch — a consumer enables it only in its own `[dev-dependencies]`
/// entry, keeping `proptest` out of the default (non-test) dependency graph.
#[cfg(any(test, feature = "test-util"))]
pub mod testkit {
    use proptest::prelude::*;

    /// A small recursive JSON generator: scalars, then arrays/objects of
    /// them. Was duplicated byte-for-byte in three places (`llmenv-util`,
    /// `src/adapter/mod.rs`, `src/adapter/claude_code.rs`) before #1281.
    pub fn arb_json() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i32>().prop_map(serde_json::Value::from),
            "[a-z]{0,4}".prop_map(serde_json::Value::String),
        ];
        leaf.prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
                prop::collection::vec(("[a-z]{1,4}", inner), 0..4)
                    .prop_map(|kvs| serde_json::Value::Object(kvs.into_iter().collect())),
            ]
        })
    }

    // -- Hook-field string generators (#1281) --
    //
    // `llmenv`'s `crate::config::Hook` and `llmenv-config`'s `schema::Hook`
    // are two distinct, non-interchangeable types (one per crate) — this
    // module can't generate either one directly, and forcing them into a
    // single shared type isn't worth it for a test-only generator. What
    // *was* a genuine, safe-to-share duplicate is the escaping-relevant
    // character classes each field's strategy uses below: added to
    // `llmenv`'s `src/adapter/skills.rs::arb_hook_handler` during #1265's
    // pre-pr-review (a plain-alnum-only charset couldn't stress the JSON
    // escaping the `opencode`/`claude_code` renderers rely on when splicing
    // hook strings into their output), but never carried over to
    // `llmenv-config`'s `validate.rs::arb_hook`, which kept a plain alnum
    // charset for the same three fields. Sharing the charset here — instead
    // of each crate's `arb_hook`-equivalent inlining its own regex literal —
    // is what actually closes the "more permissive character set" gap #1281
    // found, without needing the two `Hook` types to be the same type.
    //
    // `arb_yaml_value` was considered for the same treatment and rejected:
    // `llmenv-config::validate::arb_yaml_value` and `llmenv::skills::
    // arb_yaml_value` generate meaningfully different shapes for different
    // purposes (the former covers YAML-ambiguous scalars, tagged values, and
    // non-string mapping keys for round-trip fidelity; the latter is a
    // simpler generator for native-fragment merge fuzzing) — unifying them
    // would either weaken the round-trip coverage or bloat the merge-fuzz
    // generator with cases irrelevant to what it tests.

    /// A hook `matcher` string — an event glob/regex fragment. Plain
    /// `String`, not `Option<String>` (a hook commonly has no matcher) —
    /// callers wrap in `proptest::option::of` themselves, matching
    /// [`arb_hook_command_str`]/[`arb_hook_tool_str`]'s interface.
    pub fn arb_hook_matcher() -> impl Strategy<Value = String> {
        r#"[a-zA-Z0-9*][a-zA-Z0-9*\\"'$`{}\n]{0,7}"#
    }

    /// A hook's `command` string (the `Command`-kind handler's shell
    /// command). Charset covers path-like shapes (`.`, `/`, `-`) and digits
    /// alongside the escaping-relevant characters — digits so this doesn't
    /// regress `llmenv-config`'s prior plain-alnum coverage (#1281
    /// pre-pr-review finding: dropping digits loses the YAML
    /// scalar-ambiguity class of round-trip bug, e.g. an unquoted `123`).
    pub fn arb_hook_command_str() -> impl Strategy<Value = String> {
        r#"[a-z0-9][a-z0-9 ./\\"'$`{}\n-]{0,20}"#
    }

    /// A hook's `tool` string (the `McpTool`-kind handler's MCP tool name).
    /// Charset covers underscore-separated identifier shapes and digits
    /// alongside the escaping-relevant characters (see
    /// [`arb_hook_command_str`] for why digits matter here).
    pub fn arb_hook_tool_str() -> impl Strategy<Value = String> {
        r#"[a-z0-9_][a-z0-9_\\"'$`{}\n]{0,15}"#
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{dedup, merge_json, merge_yaml, normalize_json};

    fn yaml(s: &str) -> serde_yaml::Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn merge_yaml_unions_disjoint_mapping_keys() {
        let mut dst = yaml("a: 1");
        merge_yaml(&mut dst, yaml("b: 2"));
        assert_eq!(dst, yaml("a: 1\nb: 2"));
    }

    #[test]
    fn merge_yaml_concatenates_and_dedups_sequences() {
        let mut dst = yaml("- one\n- two");
        merge_yaml(&mut dst, yaml("- two\n- three"));
        assert_eq!(dst, yaml("- one\n- two\n- three"));
    }

    #[test]
    fn merge_yaml_recurses_into_shared_mapping_keys() {
        let mut dst = yaml("outer:\n  a: 1\n  list: [x]");
        merge_yaml(&mut dst, yaml("outer:\n  b: 2\n  list: [y]"));
        assert_eq!(dst, yaml("outer:\n  a: 1\n  b: 2\n  list: [x, y]"));
    }

    #[test]
    fn merge_yaml_src_scalar_overwrites_dst() {
        let mut dst = yaml("k: old");
        merge_yaml(&mut dst, yaml("k: new"));
        assert_eq!(dst, yaml("k: new"));
    }

    #[test]
    fn merge_yaml_shape_mismatch_src_wins() {
        let mut dst = yaml("k: [a, b]");
        merge_yaml(&mut dst, yaml("k: scalar"));
        assert_eq!(dst, yaml("k: scalar"));
    }

    fn jsn(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn merge_json_unions_disjoint_object_keys() {
        let mut dst = jsn(r#"{"a": 1}"#);
        merge_json(&mut dst, jsn(r#"{"b": 2}"#));
        assert_eq!(dst, jsn(r#"{"a": 1, "b": 2}"#));
    }

    #[test]
    fn merge_json_concatenates_and_dedups_arrays() {
        let mut dst = jsn(r#"["one", "two"]"#);
        merge_json(&mut dst, jsn(r#"["two", "three"]"#));
        assert_eq!(dst, jsn(r#"["one", "two", "three"]"#));
    }

    #[test]
    fn merge_json_recurses_into_shared_object_keys() {
        let mut dst = jsn(r#"{"outer": {"a": 1, "list": ["x"]}}"#);
        merge_json(&mut dst, jsn(r#"{"outer": {"b": 2, "list": ["y"]}}"#));
        assert_eq!(
            dst,
            jsn(r#"{"outer": {"a": 1, "b": 2, "list": ["x", "y"]}}"#)
        );
    }

    #[test]
    fn merge_json_src_scalar_overwrites_dst() {
        let mut dst = jsn(r#"{"k": "old"}"#);
        merge_json(&mut dst, jsn(r#"{"k": "new"}"#));
        assert_eq!(dst, jsn(r#"{"k": "new"}"#));
    }

    #[test]
    fn merge_json_shape_mismatch_src_wins() {
        let mut dst = jsn(r#"{"k": ["a", "b"]}"#);
        merge_json(&mut dst, jsn(r#"{"k": "scalar"}"#));
        assert_eq!(dst, jsn(r#"{"k": "scalar"}"#));
    }

    #[test]
    fn normalize_json_strips_null_keys() {
        let mut v = jsn(r#"{"a": null, "b": 1, "c": {"d": null, "e": [{"f": null, "g": 2}]}}"#);
        normalize_json(&mut v);
        assert_eq!(v, jsn(r#"{"b": 1, "c": {"e": [{"g": 2}]}}"#));
    }

    #[test]
    fn merge_json_dedups_when_null_vs_absent_differs() {
        // Objects differing only by null vs absent key (e.g. "tool": null
        // from an older render vs absent key from the current) must collapse
        // during merge_json's PartialEq-based dedup. This is the #699/#718
        // fix: normalize_json strips nulls first so the compare succeeds.
        let mut dst = jsn(r#"[{"command": "test", "tool": null}]"#);
        let src = jsn(r#"[{"command": "test"}]"#);
        merge_json(&mut dst, src);
        assert_eq!(dst, jsn(r#"[{"command": "test"}]"#));
    }

    #[test]
    fn removes_later_duplicates_preserving_order() {
        let mut v = vec!["a", "b", "a", "c", "b"];
        dedup(&mut v);
        assert_eq!(v, vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_and_singleton_are_noops() {
        let mut empty: Vec<i32> = Vec::new();
        dedup(&mut empty);
        assert!(empty.is_empty());
        let mut one = vec![1];
        dedup(&mut one);
        assert_eq!(one, vec![1]);
    }

    #[test]
    fn idempotent() {
        let mut v = vec![1, 1, 2, 3, 3, 3];
        dedup(&mut v);
        let once = v.clone();
        dedup(&mut v);
        assert_eq!(v, once);
    }

    mod props {
        use super::{dedup, merge_json};
        use crate::testkit::arb_json;
        use proptest::prelude::*;
        use serde_json::Value;

        proptest! {
            // merge_json never panics on arbitrary input pairs.
            #[test]
            fn merge_json_total(mut dst in arb_json(), src in arb_json()) {
                merge_json(&mut dst, src);
            }

            // Disjoint object keys survive the merge; shared keys take src's value
            // when both are scalars (src wins on scalar collision).
            #[test]
            fn merge_json_src_scalar_wins_on_shared_key(
                key in "[a-z]{1,4}",
                a in any::<i32>(),
                b in any::<i32>(),
            ) {
                let mut dst = serde_json::json!({ &key: a });
                merge_json(&mut dst, serde_json::json!({ &key: b }));
                prop_assert_eq!(&dst[&key], &Value::from(b));
            }

            // Merging an object into itself is idempotent once arrays are
            // dedup-stable: re-merging the result changes nothing.
            #[test]
            fn merge_json_idempotent(v in arb_json()) {
                let mut once = v.clone();
                merge_json(&mut once, v.clone());
                let mut twice = once.clone();
                merge_json(&mut twice, once.clone());
                prop_assert_eq!(once, twice);
            }

            // Array merge output carries no duplicates (concat + dedup).
            #[test]
            fn merge_json_arrays_dedup(
                a in prop::collection::vec(0i32..5, 0..6),
                b in prop::collection::vec(0i32..5, 0..6),
            ) {
                let mut dst = Value::Array(a.iter().map(|n| Value::from(*n)).collect());
                merge_json(&mut dst, Value::Array(b.iter().map(|n| Value::from(*n)).collect()));
                let arr = dst.as_array().unwrap();
                let mut seen = arr.clone();
                dedup(&mut seen);
                prop_assert_eq!(arr.len(), seen.len(), "no duplicates in merged array");
            }

            // Stronger idempotence: merging ANY src into ANY dst is idempotent —
            // re-applying the same src to the merged result is a no-op. This holds
            // even when src's own arrays carry duplicates, because the merge
            // normalizes every array on insert as well as on recursive merge.
            #[test]
            fn merge_json_idempotent_for_arbitrary_pairs(
                dst in arb_json(),
                src in arb_json(),
            ) {
                let mut once = dst;
                merge_json(&mut once, src.clone());
                let mut twice = once.clone();
                merge_json(&mut twice, src);
                prop_assert_eq!(once, twice);
            }

            // Normalization is preserved: if `dst` is already dedup-free (the
            // real-world invariant — every `dst` is itself a prior merge_json
            // output), then merging arbitrary `src` keeps the output dedup-free at
            // every depth. The insert path normalizes src subtrees just like the
            // recursive-merge path, so output shape is independent of which path
            // produced a value.
            #[test]
            fn merge_json_preserves_normalization(
                dst in arb_json(),
                src in arb_json(),
            ) {
                // Establish the precondition by normalizing dst via a self-merge
                // into an empty object's key (merge_json is the normalizer).
                let mut normalized = Value::Null;
                merge_json(&mut normalized, dst);
                prop_assume!(all_arrays_deduped(&normalized));

                merge_json(&mut normalized, src);
                prop_assert!(
                    all_arrays_deduped(&normalized),
                    "merge introduced a non-deduped array: {normalized}"
                );
            }
        }

        // True iff every array nested anywhere in `v` contains no duplicates.
        fn all_arrays_deduped(v: &Value) -> bool {
            match v {
                Value::Array(items) => {
                    let mut seen = items.clone();
                    dedup(&mut seen);
                    seen.len() == items.len() && items.iter().all(all_arrays_deduped)
                }
                Value::Object(map) => map.values().all(all_arrays_deduped),
                _ => true,
            }
        }
    }
}
