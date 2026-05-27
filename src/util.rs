//! Small shared helpers with no better home.

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
///   `dst` on a scalar collision.
pub fn merge_yaml(dst: &mut serde_yaml::Value, src: serde_yaml::Value) {
    use serde_yaml::Value;
    match (dst, src) {
        (Value::Mapping(d), Value::Mapping(s)) => {
            for (k, v) in s {
                match d.get_mut(&k) {
                    Some(existing) => merge_yaml(existing, v),
                    None => {
                        d.insert(k, v);
                    }
                }
            }
        }
        (Value::Sequence(d), Value::Sequence(s)) => {
            d.extend(s);
            let mut deduped: Vec<Value> = Vec::new();
            for item in d.drain(..) {
                if !deduped.contains(&item) {
                    deduped.push(item);
                }
            }
            *d = deduped;
        }
        (dst, src) => *dst = src,
    }
}

/// Deep-merge `src` into `dst` for JSON-shaped engine-native config.
///
/// The JSON analogue of [`merge_yaml`]: adapters build engine config (e.g.
/// `settings.json`, `mcp.json`) as [`serde_json::Value`], then overlay a
/// per-engine `native_*` fragment converted from YAML. Same value-shape rule:
///
/// - **Objects** merge key-by-key — shared keys recurse, disjoint keys union.
/// - **Arrays** concatenate (`src` after `dst`), then dedup.
/// - **Scalars** and any shape mismatch are overwritten by `src` — the native
///   fragment is the higher-precedence overlay, so it wins on collision.
pub fn merge_json(dst: &mut serde_json::Value, src: serde_json::Value) {
    use serde_json::Value;
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                match d.get_mut(&k) {
                    Some(existing) => merge_json(existing, v),
                    None => {
                        d.insert(k, v);
                    }
                }
            }
        }
        (Value::Array(d), Value::Array(s)) => {
            d.extend(s);
            let mut deduped: Vec<Value> = Vec::new();
            for item in d.drain(..) {
                if !deduped.contains(&item) {
                    deduped.push(item);
                }
            }
            *d = deduped;
        }
        (dst, src) => *dst = src,
    }
}

#[cfg(test)]
mod tests {
    use super::{dedup, merge_json, merge_yaml};

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
        use proptest::prelude::*;
        use serde_json::Value;

        // A small recursive JSON generator: scalars, then arrays/objects of them.
        fn arb_json() -> impl Strategy<Value = Value> {
            let leaf = prop_oneof![
                Just(Value::Null),
                any::<bool>().prop_map(Value::Bool),
                any::<i32>().prop_map(Value::from),
                "[a-z]{0,4}".prop_map(Value::String),
            ];
            leaf.prop_recursive(3, 16, 4, |inner| {
                prop_oneof![
                    prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                    prop::collection::vec(("[a-z]{1,4}", inner), 0..4)
                        .prop_map(|kvs| { Value::Object(kvs.into_iter().collect()) }),
                ]
            })
        }

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
        }
    }
}
