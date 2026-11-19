//! Small shared helpers with no better home.

pub mod agent_notice;
pub mod mcp_trace;
pub mod proc_start;
pub mod process;
pub mod shell;
pub mod time;
pub mod trace;
pub mod ttl;

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
mod color_tests;

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

/// Replace each `$VAR` and `${VAR}` in `text` with the value that `env` returns. A variable name
/// is ASCII letters, digits, and `_`. `None` when a `$` is bare, a `${` does not close after a
/// plain name, or `env` has no non-empty value for a name.
#[must_use]
pub fn expand_env_refs(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&n) = chars.peek() {
            if !(n.is_ascii_alphanumeric() || n == '_') {
                break;
            }
            name.push(n);
            chars.next();
        }
        if name.is_empty() || (braced && chars.next() != Some('}')) {
            return None;
        }
        out.push_str(&env(&name).filter(|v| !v.is_empty())?);
    }
    Some(out)
}

/// Replace every control character and every bidirectional or zero-width format character in
/// `s` with its Rust-literal escape (a newline becomes the two characters `\n`) and leave every
/// other character, including non-ASCII text, as it is.
/// Use it on text from files or the network before it reaches a terminal or a line-based log:
/// a raw newline can forge an extra line, a raw ESC byte reaches the terminal as a control
/// sequence, and a bidirectional override can reorder the text that the reader sees.
#[must_use]
pub fn escape_control(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() || is_unsafe_format_char(c) {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
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
pub mod testkit;

/// The format characters that `strip_unsafe_chars` removes besides control characters: zero-width
/// and joiner marks (U+200B to U+200F), bidirectional overrides and isolates (U+202A to U+202E,
/// U+2066 to U+2069), word joiner and invisible operators (U+2060 to U+2064), the Arabic letter
/// mark (U+061C), and the byte order mark (U+FEFF). `char::is_control` misses all of them.
fn is_unsafe_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// Remove every character that can spoof or hide text on a terminal or in a prompt: control
/// characters, including newline, and the invisible and bidirectional format characters.
/// Use it on text that an agent or a user wrote and that llmenv prints back. Split the text into
/// lines first when line breaks must survive.
#[must_use]
pub fn strip_unsafe_chars(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() && !is_unsafe_format_char(*c))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
