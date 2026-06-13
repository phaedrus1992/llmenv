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
                        d.insert(k, v);
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

/// Recursively dedup every sequence in a YAML value so it matches what
/// [`merge_yaml`] produces. Used on insert/overwrite paths to keep the merge
/// idempotent — including by callers (e.g. `merge::capabilities`) that insert a
/// fragment into a fresh map without routing it through [`merge_yaml`].
pub(crate) fn normalize_yaml(value: &mut serde_yaml::Value) {
    use serde_yaml::Value;
    match value {
        Value::Sequence(items) => {
            for item in items.iter_mut() {
                normalize_yaml(item);
            }
            dedup(items);
        }
        Value::Mapping(map) => {
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
                        d.insert(k, v);
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

/// Recursively dedup every array in a JSON value so it matches what
/// [`merge_json`] produces. Used on insert/overwrite paths to keep the merge
/// idempotent.
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
            for (_, v) in map.iter_mut() {
                normalize_json(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{dedup, merge_json, merge_yaml, normalize_yaml};

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
        use super::{dedup, merge_json, merge_yaml, normalize_yaml};
        use proptest::prelude::*;
        use serde_json::Value;
        use serde_yaml::Value as Y;

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

        // ===== dedup PBTs =====

        proptest! {
            #[test]
            fn dedup_output_has_no_duplicates(items in prop::collection::vec(0i32..10, 0..20)) {
                let mut v = items;
                dedup(&mut v);
                for i in 0..v.len() {
                    for j in (i + 1)..v.len() {
                        prop_assert_ne!(v[i], v[j], "duplicate at positions {},{}", i, j);
                    }
                }
            }

            #[test]
            fn dedup_preserves_first_occurrence_order(
                items in prop::collection::vec(0i32..5, 0..20)
            ) {
                let mut v = items.clone();
                dedup(&mut v);
                let mut expected: Vec<i32> = Vec::new();
                for x in &items {
                    if !expected.contains(x) {
                        expected.push(*x);
                    }
                }
                prop_assert_eq!(v, expected);
            }

            #[test]
            fn dedup_idempotent(items in prop::collection::vec(0i32..10, 0..20)) {
                let mut v = items;
                dedup(&mut v);
                let once = v.clone();
                dedup(&mut v);
                prop_assert_eq!(v, once);
            }

            #[test]
            fn dedup_output_len_leq_input_len(items in prop::collection::vec(0i32..10, 0..20)) {
                let original_len = items.len();
                let mut v = items;
                dedup(&mut v);
                prop_assert!(
                    v.len() <= original_len,
                    "dedup grew the vec: {} > {}",
                    v.len(),
                    original_len
                );
            }

            #[test]
            fn dedup_output_is_subset_of_input(items in prop::collection::vec(0i32..10, 0..20)) {
                let original = items.clone();
                let mut v = items;
                dedup(&mut v);
                for x in &v {
                    prop_assert!(original.contains(x), "dedup introduced {}", x);
                }
            }

            #[test]
            fn dedup_never_panics(items in prop::collection::vec(".*", 0..10)) {
                let mut v = items;
                dedup(&mut v);
            }
        }

        // ===== merge_yaml / normalize_yaml PBTs =====

        fn arb_yaml() -> impl Strategy<Value = serde_yaml::Value> {
            let leaf = prop_oneof![
                Just(Y::Null),
                any::<bool>().prop_map(Y::Bool),
                any::<i32>().prop_map(|n| Y::Number(n.into())),
                "[a-z]{0,4}".prop_map(Y::String),
            ];
            leaf.prop_recursive(3, 16, 4, |inner| {
                prop_oneof![
                    prop::collection::vec(inner.clone(), 0..4).prop_map(Y::Sequence),
                    prop::collection::vec(("[a-z]{1,4}", inner), 0..4).prop_map(|kvs| {
                        Y::Mapping(kvs.into_iter().map(|(k, v)| (Y::String(k), v)).collect())
                    }),
                ]
            })
        }

        proptest! {
            #[test]
            fn merge_yaml_total(mut dst in arb_yaml(), src in arb_yaml()) {
                merge_yaml(&mut dst, src);
            }

            #[test]
            fn merge_yaml_src_scalar_wins_on_shared_key(
                key in "[a-z]{1,4}",
                a in any::<i32>(),
                b in any::<i32>(),
            ) {
                let mut dst = serde_yaml::from_str::<Y>(&format!("{key}: {a}")).unwrap();
                merge_yaml(&mut dst, serde_yaml::from_str::<Y>(&format!("{key}: {b}")).unwrap());
                let got = dst.as_mapping().unwrap().get(Y::String(key)).unwrap();
                prop_assert_eq!(got, &Y::Number(b.into()));
            }

            // Self-merge idempotency: merge(merge(v,v), merge(v,v)) == merge(v,v).
            #[test]
            fn merge_yaml_idempotent(v in arb_yaml()) {
                let mut once = v.clone();
                merge_yaml(&mut once, v.clone());
                let mut twice = once.clone();
                merge_yaml(&mut twice, once.clone());
                prop_assert_eq!(once, twice);
            }

            // Convergence: re-applying src to an already-merged dst is a no-op.
            #[test]
            fn merge_yaml_convergent(
                dst in arb_yaml(),
                src in arb_yaml(),
            ) {
                let mut once = dst;
                merge_yaml(&mut once, src.clone());
                let mut twice = once.clone();
                merge_yaml(&mut twice, src);
                prop_assert_eq!(once, twice);
            }

            // Dst-key preservation: keys only in dst (not in src) survive merge.
            #[test]
            fn merge_yaml_dst_only_keys_preserved(
                dst_key in "[a-z]{1,4}",
                dst_val in any::<i32>(),
                src_key in "[a-z]{1,4}",
                src_val in any::<i32>(),
            ) {
                prop_assume!(dst_key != src_key);
                let mut dst = serde_yaml::from_str::<Y>(&format!("{dst_key}: {dst_val}")).unwrap();
                let src = serde_yaml::from_str::<Y>(&format!("{src_key}: {src_val}")).unwrap();
                merge_yaml(&mut dst, src);
                let map = dst.as_mapping().unwrap();
                let got = map.get(Y::String(dst_key)).unwrap();
                prop_assert_eq!(got, &Y::Number(dst_val.into()));
            }

            #[test]
            fn merge_yaml_sequences_no_duplicates(
                a in prop::collection::vec(0i32..5, 0..6),
                b in prop::collection::vec(0i32..5, 0..6),
            ) {
                let mut dst = Y::Sequence(a.iter().map(|n| Y::Number((*n).into())).collect());
                merge_yaml(&mut dst, Y::Sequence(b.iter().map(|n| Y::Number((*n).into())).collect()));
                let seq = dst.as_sequence().unwrap().clone();
                let mut deduped = seq.clone();
                dedup(&mut deduped);
                prop_assert_eq!(seq.len(), deduped.len(), "duplicates in merged sequence");
            }

            #[test]
            fn normalize_yaml_idempotent(v in arb_yaml()) {
                let mut once = v;
                normalize_yaml(&mut once);
                let mut twice = once.clone();
                normalize_yaml(&mut twice);
                prop_assert_eq!(once, twice);
            }

            #[test]
            fn normalize_yaml_sequences_have_no_duplicates(v in arb_yaml()) {
                let mut v = v;
                normalize_yaml(&mut v);
                prop_assert!(all_yaml_sequences_deduped(&v), "duplicate found after normalize");
            }

            #[test]
            fn normalize_yaml_never_panics(v in arb_yaml()) {
                let mut v = v;
                normalize_yaml(&mut v);
            }
        }

        fn all_yaml_sequences_deduped(v: &serde_yaml::Value) -> bool {
            match v {
                Y::Sequence(items) => {
                    let mut seen = items.clone();
                    dedup(&mut seen);
                    seen.len() == items.len() && items.iter().all(all_yaml_sequences_deduped)
                }
                Y::Mapping(map) => map.values().all(all_yaml_sequences_deduped),
                _ => true,
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
