//! JSON-path-lite: `key`, `key.key`, `key[N]`, and combinations
//! (`system[0].text`), used to target a location inside a JSON request body
//! for `features.launch_proxy` (#1289). Deliberately not a full JSONPath
//! implementation — only what the launch-proxy rule engine needs.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct PathParseError(String);

/// Parse a JSON-path-lite string into segments.
///
/// # Errors
/// Returns an error when the path is empty, has an unmatched `[`/`]`, or a
/// bracket doesn't contain a valid non-negative integer index.
pub fn parse_path(path: &str) -> Result<Vec<PathSegment>, PathParseError> {
    if path.is_empty() {
        return Err(PathParseError("path must not be empty".into()));
    }
    let mut segments = Vec::new();
    for dotted in path.split('.') {
        if dotted.is_empty() {
            return Err(PathParseError(format!("empty segment in path: {path}")));
        }
        let mut rest = dotted;
        // A segment may be `key` or `key[N][M]...` — split the key off the
        // front, then consume zero or more bracketed indices.
        if let Some(bracket_start) = rest.find('[') {
            let key = &rest[..bracket_start];
            if !key.is_empty() {
                segments.push(PathSegment::Key(key.to_string()));
            }
            rest = &rest[bracket_start..];
            while !rest.is_empty() {
                if !rest.starts_with('[') {
                    return Err(PathParseError(format!("expected '[' in path: {path}")));
                }
                let Some(close) = rest.find(']') else {
                    return Err(PathParseError(format!("unmatched '[' in path: {path}")));
                };
                let idx_str = &rest[1..close];
                let idx: usize = idx_str.parse().map_err(|_| {
                    PathParseError(format!("invalid index '{idx_str}' in path: {path}"))
                })?;
                segments.push(PathSegment::Index(idx));
                rest = &rest[close + 1..];
            }
        } else {
            segments.push(PathSegment::Key(rest.to_string()));
        }
    }
    if segments.is_empty() {
        return Err(PathParseError(format!(
            "no segments parsed from path: {path}"
        )));
    }
    Ok(segments)
}

/// Navigate `value` by `segments`, returning `None` if any segment along the
/// way is missing or type-mismatched (object segment on a non-object, etc.).
#[must_use]
pub fn get_path<'a>(
    value: &'a serde_json::Value,
    segments: &[PathSegment],
) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for seg in segments {
        current = match (seg, current) {
            (PathSegment::Key(k), serde_json::Value::Object(map)) => map.get(k)?,
            (PathSegment::Index(i), serde_json::Value::Array(arr)) => arr.get(*i)?,
            _ => return None,
        };
    }
    Some(current)
}

/// Set `value` at `segments`, creating missing intermediate objects (for
/// `Key` segments) along the way. An `Index` segment into a too-short array
/// extends the array with `Value::Null` up to that index. Overwrites a
/// type-mismatched intermediate node (e.g. a string where an object was
/// expected) rather than failing — the launch-proxy design spec calls `Set`
/// an unconditional upsert.
pub fn set_path(
    value: &mut serde_json::Value,
    segments: &[PathSegment],
    new_value: serde_json::Value,
) {
    let Some((last, rest)) = segments.split_last() else {
        *value = new_value;
        return;
    };
    let mut current = value;
    for seg in rest {
        current = match seg {
            PathSegment::Key(k) => {
                if !matches!(current, serde_json::Value::Object(_)) {
                    *current = serde_json::Value::Object(serde_json::Map::new());
                }
                let serde_json::Value::Object(map) = current else {
                    unreachable!("just normalized to Object above");
                };
                map.entry(k.clone()).or_insert(serde_json::Value::Null)
            }
            PathSegment::Index(i) => {
                if !matches!(current, serde_json::Value::Array(_)) {
                    *current = serde_json::Value::Array(Vec::new());
                }
                let serde_json::Value::Array(arr) = current else {
                    unreachable!("just normalized to Array above");
                };
                if arr.len() <= *i {
                    arr.resize(*i + 1, serde_json::Value::Null);
                }
                &mut arr[*i]
            }
        };
    }
    match last {
        PathSegment::Key(k) => {
            if !matches!(current, serde_json::Value::Object(_)) {
                *current = serde_json::Value::Object(serde_json::Map::new());
            }
            if let serde_json::Value::Object(map) = current {
                map.insert(k.clone(), new_value);
            }
        }
        PathSegment::Index(i) => {
            if !matches!(current, serde_json::Value::Array(_)) {
                *current = serde_json::Value::Array(Vec::new());
            }
            if let serde_json::Value::Array(arr) = current {
                if arr.len() <= *i {
                    arr.resize(*i + 1, serde_json::Value::Null);
                }
                arr[*i] = new_value;
            }
        }
    }
}

/// Remove the value at `segments` if present. Returns `true` if something was
/// removed, `false` if any segment along the way was already absent
/// (no-op-if-absent, per the launch-proxy design spec's error handling).
pub fn remove_path(value: &mut serde_json::Value, segments: &[PathSegment]) -> bool {
    let Some((last, rest)) = segments.split_last() else {
        return false;
    };
    let Some(parent) = get_path_mut(value, rest) else {
        return false;
    };
    match (last, parent) {
        (PathSegment::Key(k), serde_json::Value::Object(map)) => map.remove(k).is_some(),
        (PathSegment::Index(i), serde_json::Value::Array(arr)) if *i < arr.len() => {
            arr.remove(*i);
            true
        }
        _ => false,
    }
}

fn get_path_mut<'a>(
    value: &'a mut serde_json::Value,
    segments: &[PathSegment],
) -> Option<&'a mut serde_json::Value> {
    let mut current = value;
    for seg in segments {
        current = match (seg, current) {
            (PathSegment::Key(k), serde_json::Value::Object(map)) => map.get_mut(k)?,
            (PathSegment::Index(i), serde_json::Value::Array(arr)) => arr.get_mut(*i)?,
            _ => return None,
        };
    }
    Some(current)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn parses_dotted_and_indexed_segments() {
        let segs = parse_path("system[0].text").unwrap();
        assert_eq!(
            segs,
            vec![
                PathSegment::Key("system".into()),
                PathSegment::Index(0),
                PathSegment::Key("text".into()),
            ]
        );
    }

    #[test]
    fn parses_bare_key() {
        assert_eq!(
            parse_path("thinking").unwrap(),
            vec![PathSegment::Key("thinking".into())]
        );
    }

    #[test]
    fn rejects_unmatched_bracket() {
        assert!(parse_path("system[0.text").is_err());
    }

    #[test]
    fn rejects_empty_path() {
        assert!(parse_path("").is_err());
    }

    #[test]
    fn get_path_navigates_object_and_array() {
        let v = json!({"system": [{"text": "hello"}]});
        let segs = parse_path("system[0].text").unwrap();
        assert_eq!(get_path(&v, &segs), Some(&json!("hello")));
    }

    #[test]
    fn get_path_returns_none_when_absent() {
        let v = json!({"system": []});
        let segs = parse_path("thinking").unwrap();
        assert_eq!(get_path(&v, &segs), None);
    }

    #[test]
    fn set_path_upserts_missing_intermediate_object() {
        let mut v = json!({});
        let segs = parse_path("thinking").unwrap();
        set_path(&mut v, &segs, json!({"type": "disabled"}));
        assert_eq!(v, json!({"thinking": {"type": "disabled"}}));
    }

    #[test]
    fn set_path_overwrites_existing_value() {
        let mut v = json!({"thinking": {"type": "adaptive"}});
        let segs = parse_path("thinking").unwrap();
        set_path(&mut v, &segs, json!({"type": "disabled"}));
        assert_eq!(v, json!({"thinking": {"type": "disabled"}}));
    }

    #[test]
    fn set_path_writes_through_existing_array_index() {
        let mut v = json!({"system": [{"text": "old"}]});
        let segs = parse_path("system[0].text").unwrap();
        set_path(&mut v, &segs, json!("new"));
        assert_eq!(v, json!({"system": [{"text": "new"}]}));
    }

    #[test]
    fn remove_path_deletes_existing_key_and_reports_true() {
        let mut v = json!({"thinking": {"type": "adaptive"}});
        let segs = parse_path("thinking").unwrap();
        assert!(remove_path(&mut v, &segs));
        assert_eq!(v, json!({}));
    }

    #[test]
    fn remove_path_is_noop_on_missing_key_and_reports_false() {
        let mut v = json!({});
        let segs = parse_path("thinking").unwrap();
        assert!(!remove_path(&mut v, &segs));
        assert_eq!(v, json!({}));
    }

    proptest::proptest! {
        #[test]
        fn set_then_get_round_trips_for_any_key_path(
            key in "[a-z]{1,8}",
            n in 1i64..1000,
        ) {
            let mut v = serde_json::json!({});
            let segs = parse_path(&key).unwrap();
            set_path(&mut v, &segs, serde_json::json!(n));
            prop_assert_eq!(get_path(&v, &segs), Some(&serde_json::json!(n)));
        }
    }
}
