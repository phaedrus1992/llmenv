# Launch API-Proxy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:executing-plans` to
> implement this plan task-by-task. **Do NOT use `superpowers:subagent-driven-development`**
> — repo policy (this user's global CLAUDE.md) overrides that skill's own
> recommendation; work each task inline in the current session instead. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `features.launch_proxy` to llmenv's config schema and wire a local
HTTP proxy into `llmenv launch claude_code` that rewrites outbound Anthropic API
requests (headers and JSON body) per declarative rules before forwarding them,
so a user can trim/override Claude Code's injected system prompt or set
conditionally-missing fields (e.g. `thinking`) without a TLS MITM.

**Architecture:** A new `src/launch/proxy.rs` module, structured like the
existing `src/launch/socket.rs` (same start-before-spawn / teardown-on-child-exit
lifecycle). A small JSON-path-lite parser/evaluator backs a rule engine that
gates each rule on zero or more AND-ed conditions (header/body presence,
absence, equality, or regex/substring match) before applying a `Set` (upsert),
`Remove`, or `Strip` op. The HTTP layer is `hyper` (server) + `reqwest`
(forwarding client, already a dependency) — `hyper`, `hyper-util`, and
`http-body-util` are already present in `Cargo.lock` transitively via
`reqwest`, so this promotes them to direct dependencies rather than adding new
crates to the dependency tree. `regex` (also already transitively present) is
promoted the same way for `Strip`/`Matches` pattern support.

**Tech Stack:** Rust 2024 edition, existing `llmenv` workspace (`crates/llmenv-config`
+ main `llmenv` binary crate), `hyper` 1.x server + `reqwest` 0.13 client,
`tokio`, `wiremock` (existing dev-dependency) for integration tests.

**Spec:** `docs/superpowers/specs/2026-08-28-launch-api-proxy-design.md`

## Global Constraints

- Workspace lints (`Cargo.toml:44-51`): `unsafe_code = "forbid"`,
  `clippy::unwrap_used = "deny"`, `clippy::expect_used = "deny"`. Every
  fallible call in new code uses `?`/`.context()` — never `.unwrap()`/`.expect()`
  outside test files (test files use
  `#[expect(clippy::unwrap_used, reason = "tests")]`, matching
  `crates/llmenv-config/src/lib.rs:91`).
- `cargo fmt` runs as a pre-commit hook — if a commit is rejected for
  formatting, run `cargo fmt` and re-commit.
- Every new `pub`/`pub(crate)` function with non-obvious behavior gets a doc
  comment, matching the existing style in `src/launch/socket.rs` and
  `src/launch/mod.rs`.
- Every user-facing change needs a `CHANGELOG-4.md` entry under
  `[Unreleased]` and matching `website/docs/` coverage, version-tagged
  `(added in v4.0.0)` per `AGENTS.md`'s hard rule — handled in Task 9.
- Rust edition 2024, `anyhow::Result` at the CLI-command level, `thiserror`
  for `llmenv-config`'s `ValidateError` enum (`crates/llmenv-config/src/validate.rs:15`).
- Pin new direct dependencies to the exact version already resolved in
  `Cargo.lock` (`=X.Y.Z`), matching the existing pinning convention
  (`Cargo.toml:14-42`).

---

## File Structure

- **Modify `Cargo.toml:14-43`** (`[workspace.dependencies]`) — add pinned
  `hyper`, `hyper-util`, `http-body-util`, `regex`.
- **Modify `Cargo.toml:73-115`** (`[dependencies]`) — reference the new
  workspace deps for the main crate (needed by `src/launch/proxy.rs`).
- **Modify `crates/llmenv-config/Cargo.toml`** — add `regex = { workspace = true }`
  (validation needs to compile-check regex patterns at config-load time).
- **Modify `crates/llmenv-config/src/schema.rs`** — add `LaunchProxy`,
  `ProxyRule`, `ProxyCondition`, `ProxyTarget`, `ProxyConditionTarget`,
  `ProxyOp`, `ProxyCheck` types; add `Features.launch_proxy: Option<LaunchProxy>`.
- **Modify `crates/llmenv-config/src/lib.rs:26-39`** — re-export the new
  schema types.
- **Modify `crates/llmenv-config/src/validate.rs`** — add
  `ValidateError::InvalidProxyPath`/`InvalidProxyRegex` variants,
  `Config::validate_launch_proxy`, called from `Config::validate` (line ~471).
- **Create `src/launch/proxy.rs`** — JSON-path-lite parser/evaluator, rule
  engine, and the `hyper` server that forwards via `reqwest`.
- **Modify `src/launch/mod.rs`** — add `mod proxy;` (line 13, alongside
  `credential_watch`/`drift`/`socket`), start the proxy inside `run`'s
  `rt.block_on` block (after the notice-socket bind, ~line 112) when
  `features.launch_proxy.enabled`, mutate `resolved.vars["ANTHROPIC_BASE_URL"]`.
- **Modify `tests/launch.rs`** — add proxy integration tests using `wiremock`.
- **Modify `website/docs/commands.md`** — new subsection under `## launch`
  documenting `features.launch_proxy`, tagged `(added in v4.0.0)`.
- **Modify `CHANGELOG-4.md`** — new entry under `[Unreleased]` → `### Added`.

---

### Task 1: Promote transitive HTTP/regex deps to direct, pinned dependencies

**Files:**
- Modify: `Cargo.toml:14-43` (`[workspace.dependencies]`)
- Modify: `Cargo.toml:73-115` (`[dependencies]`)
- Modify: `crates/llmenv-config/Cargo.toml`

**Interfaces:**
- Produces: `hyper::{server::conn::http1, service::service_fn, body::{Incoming, Frame}}`,
  `hyper_util::rt::TokioIo`, `http_body_util::{BodyExt, StreamBody, Full}`, and
  `regex::Regex` become available to `src/launch/proxy.rs` (Tasks 4-6) and
  `crates/llmenv-config/src/validate.rs` (Task 3).

- [ ] **Step 1: Check the exact versions already resolved**

Run: `grep -A1 '^name = "hyper"$\|^name = "hyper-util"$\|^name = "http-body-util"$\|^name = "regex"$' Cargo.lock`
Expected: four `version = "..."` lines (already present transitively via `reqwest`).
Use these exact versions for the `=X.Y.Z` pins below (do not guess).

- [ ] **Step 2: Add to `[workspace.dependencies]`, and enable reqwest's `stream` feature**

In `Cargo.toml`, after the `rustix` entry (line 42), add:

```toml
hyper = { version = "=<resolved-version>", features = ["server", "http1"] }
hyper-util = { version = "=<resolved-version>", features = ["tokio"] }
http-body-util = "=<resolved-version>"
regex = "=<resolved-version>"
```

Task 6's proxy forwarder streams the upstream response back via
`reqwest::Response::bytes_stream()`, which is gated behind reqwest's
`stream` Cargo feature — not currently enabled (`Cargo.toml:35`'s `reqwest`
entry only has `["rustls", "json", "blocking"]`). Change that line to:

```toml
reqwest = { version = "=0.13.4", default-features = false, features = ["rustls", "json", "blocking", "stream"] }
```

- [ ] **Step 3: Add to the main crate's `[dependencies]`**

In `Cargo.toml`'s `[dependencies]` section (after `reqwest = { workspace = true }`,
line 102), add:

```toml
hyper = { workspace = true }
hyper-util = { workspace = true }
http-body-util = { workspace = true }
regex = { workspace = true }
```

- [ ] **Step 4: Add `regex` to `llmenv-config`'s dependencies**

In `crates/llmenv-config/Cargo.toml`'s `[dependencies]` (after `serde_yaml`,
line 18), add:

```toml
regex = { workspace = true }
```

- [ ] **Step 5: Verify it builds with no new crates added**

Run: `cargo tree --duplicates` before and after — the dependency count should
be unchanged (all four packages were already in the graph). Then:
Run: `cargo check --workspace`
Expected: succeeds; `Cargo.lock` changes only which packages are "direct" vs
"transitive" (no new `[[package]]` blocks).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/llmenv-config/Cargo.toml
git commit -m "build: promote hyper/regex to direct deps for launch proxy"
```

---

### Task 2: Config schema types

**Files:**
- Modify: `crates/llmenv-config/src/schema.rs` (add near `CdGuard`, ~line 1110)
- Modify: `crates/llmenv-config/src/lib.rs:26-39` (re-export)

**Interfaces:**
- Produces: `pub struct LaunchProxy { pub enabled: bool, pub rules: Vec<ProxyRule> }`,
  `pub struct ProxyRule { pub when: Vec<ProxyCondition>, pub target: ProxyTarget, pub op: ProxyOp }`,
  `pub enum ProxyTarget { Header { name: String }, Body { path: String } }`,
  `pub struct ProxyCondition { pub target: ProxyConditionTarget, pub check: ProxyCheck }`,
  `pub enum ProxyConditionTarget { Header { name: String }, Body { path: Option<String> } }`,
  `pub enum ProxyOp { Set(serde_json::Value), Remove, Strip { pattern: String, regex: bool } }`,
  `pub enum ProxyCheck { Missing, Present, Equals(serde_json::Value), Matches { pattern: String, regex: bool } }`.
  Used by Task 3 (validation), Task 4/5 (evaluator), Task 7 (wiring).

- [ ] **Step 1: Write the failing serde round-trip test**

Add to the `#[cfg(test)]` module in `crates/llmenv-config/src/schema.rs` (near
the existing `CdGuard`/`RepeatDetect` tests):

```rust
#[test]
fn launch_proxy_round_trips_through_yaml() {
    let yaml = r#"
enabled: true
rules:
  - when:
      - target: header
        name: "x-billing-header"
        check: present
      - target: body
        path: "system[0].text"
        check:
          matches: "security monitor"
          regex: false
      - target: body
        path: "thinking"
        check: missing
    target: body
    path: "thinking"
    op:
      set:
        type: disabled
  - target: body
    path: "system[0].text"
    op:
      strip:
        pattern: "verbose boilerplate.*"
        regex: true
"#;
    let parsed: LaunchProxy = serde_yaml::from_str(yaml).unwrap();
    assert!(parsed.enabled);
    assert_eq!(parsed.rules.len(), 2);
    assert_eq!(parsed.rules[0].when.len(), 3);
    match &parsed.rules[0].target {
        ProxyTarget::Body { path } => assert_eq!(path, "thinking"),
        ProxyTarget::Header { .. } => panic!("expected Body target"),
    }
    match &parsed.rules[0].op {
        ProxyOp::Set(v) => assert_eq!(v["type"], "disabled"),
        _ => panic!("expected Set op"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p llmenv-config launch_proxy_round_trips_through_yaml`
Expected: FAIL — `LaunchProxy` is not defined.

- [ ] **Step 3: Add the types**

In `crates/llmenv-config/src/schema.rs`, add near `CdGuard` (~line 1110):

```rust
/// Local API-proxy mode for `llmenv launch claude_code` (#1289). Rewrites
/// outbound request headers/body per declarative rules before forwarding to
/// the upstream Anthropic API (or an existing `ANTHROPIC_BASE_URL`, chained
/// through rather than clobbered). Off by default — this touches live API
/// traffic and prompt content, unlike `repeat_detect`/`cd_guard`'s
/// lower-stakes on-by-default guardrails.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LaunchProxy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub rules: Vec<ProxyRule>,
}

/// One rewrite rule: fires when every condition in `when` matches (or always,
/// if `when` is empty), then applies `op` to the field named by `target`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProxyRule {
    #[serde(default)]
    pub when: Vec<ProxyCondition>,
    #[serde(flatten)]
    pub target: ProxyTarget,
    pub op: ProxyOp,
}

/// What a rule's `op` (or a condition's `check`) applies to: a request
/// header by name, or a JSON-path-lite location in the parsed request body.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum ProxyTarget {
    Header { name: String },
    Body { path: String },
}

/// One AND-ed condition gating a [`ProxyRule`]. `Body { path: None }` matches
/// against the whole serialized request body (only meaningful with
/// `check: Matches`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProxyCondition {
    #[serde(flatten)]
    pub target: ProxyConditionTarget,
    pub check: ProxyCheck,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum ProxyConditionTarget {
    Header {
        name: String,
    },
    Body {
        #[serde(default)]
        path: Option<String>,
    },
}

/// `Set` upserts (creates the path/header if missing) — required so a rule
/// can add a field Claude Code's own request omits entirely (e.g. `thinking`
/// on the auto-mode classifier request, which never sends one). `Remove` and
/// `Strip` are no-op-if-the-target-is-absent (see the launch-proxy design
/// spec's Error handling section).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProxyOp {
    Set(serde_json::Value),
    Remove,
    Strip { pattern: String, regex: bool },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProxyCheck {
    Missing,
    Present,
    Equals(serde_json::Value),
    Matches { pattern: String, regex: bool },
}
```

Then add `pub launch_proxy: Option<LaunchProxy>` to `Features` (`schema.rs:66`,
after `cd_guard`), with a doc comment:

```rust
    /// Local API-proxy mode for `llmenv launch claude_code` (#1289). Off by
    /// default.
    #[serde(default)]
    pub launch_proxy: Option<LaunchProxy>,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p llmenv-config launch_proxy_round_trips_through_yaml`
Expected: PASS

- [ ] **Step 5: Re-export from `lib.rs`**

In `crates/llmenv-config/src/lib.rs:26-39`'s `pub use schema::{...}` block, add
`LaunchProxy, ProxyCheck, ProxyCondition, ProxyConditionTarget, ProxyOp,
ProxyRule, ProxyTarget,` in alphabetical position (matching the existing
alphabetized list).

- [ ] **Step 6: Run the full config crate test suite**

Run: `cargo test -p llmenv-config`
Expected: PASS (no regressions from the new `Features` field — it's
`#[serde(default)]` so every existing fixture config still parses).

- [ ] **Step 7: Commit**

```bash
git add crates/llmenv-config/src/schema.rs crates/llmenv-config/src/lib.rs
git commit -m "feat(config): add features.launch_proxy schema (#1289)"
```

---

### Task 3: Config validation

**Files:**
- Modify: `crates/llmenv-config/src/validate.rs` (add `ValidateError` variants
  near line 25; add `validate_launch_proxy` near `validate_permissions`, ~line 475)

**Interfaces:**
- Consumes: `LaunchProxy`, `ProxyRule`, `ProxyTarget`, `ProxyOp`, `ProxyCondition`,
  `ProxyConditionTarget`, `ProxyCheck` (Task 2). `crate::proxy_path::parse_path`
  (Task 4 — validation only needs the parser, not the get/set/remove evaluator,
  so this task is written to compile against a `parse_path` stub added in Step 1
  and Task 4 fills in the real implementation; see note in Step 1).
- Produces: `Config::validate_launch_proxy(&self) -> Result<(), ValidateError>`,
  called from `Config::validate` (~line 471).

> **Ordering note:** this task is written assuming Task 4's `proxy_path::parse_path`
> already exists. If executing tasks strictly in order, do Task 4 before Task 3,
> or add a minimal `parse_path` stub now and let Task 4 replace it — either
> order works since the two tasks don't share a test file. This plan lists
> validation first because it's the smaller, more self-contained change; an
> executor following it strictly should do **Task 4 before Task 3** to avoid
> a stub.

- [ ] **Step 1: Write the failing tests**

Add to `crates/llmenv-config/src/validate.rs`'s `#[cfg(test)]` module:

```rust
#[test]
fn validate_rejects_bad_json_path() {
    let mut cfg = Config::default();
    cfg.features = Some(Features {
        launch_proxy: Some(LaunchProxy {
            enabled: true,
            rules: vec![ProxyRule {
                when: vec![],
                target: ProxyTarget::Body {
                    path: "system[[.text".into(), // malformed: unmatched bracket
                },
                op: ProxyOp::Remove,
            }],
        }),
        ..Default::default()
    });
    assert!(matches!(
        cfg.validate(),
        Err(ValidateError::InvalidProxyPath(_))
    ));
}

#[test]
fn validate_rejects_bad_regex() {
    let mut cfg = Config::default();
    cfg.features = Some(Features {
        launch_proxy: Some(LaunchProxy {
            enabled: true,
            rules: vec![ProxyRule {
                when: vec![],
                target: ProxyTarget::Body {
                    path: "system[0].text".into(),
                },
                op: ProxyOp::Strip {
                    pattern: "(unclosed".into(),
                    regex: true,
                },
            }],
        }),
        ..Default::default()
    });
    assert!(matches!(
        cfg.validate(),
        Err(ValidateError::InvalidProxyRegex(_))
    ));
}

#[test]
fn validate_accepts_well_formed_launch_proxy() {
    let mut cfg = Config::default();
    cfg.features = Some(Features {
        launch_proxy: Some(LaunchProxy {
            enabled: true,
            rules: vec![ProxyRule {
                when: vec![ProxyCondition {
                    target: ProxyConditionTarget::Body { path: None },
                    check: ProxyCheck::Matches {
                        pattern: "security monitor".into(),
                        regex: false,
                    },
                }],
                target: ProxyTarget::Body {
                    path: "thinking".into(),
                },
                op: ProxyOp::Set(serde_json::json!({"type": "disabled"})),
            }],
        }),
        ..Default::default()
    });
    assert!(cfg.validate().is_ok());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p llmenv-config validate_rejects_bad_json_path validate_rejects_bad_regex validate_accepts_well_formed_launch_proxy`
Expected: FAIL — `ValidateError::InvalidProxyPath`/`InvalidProxyRegex` don't exist yet.

- [ ] **Step 3: Add the `ValidateError` variants**

In `crates/llmenv-config/src/validate.rs`'s `ValidateError` enum (near line
25, alongside `InvalidCIDR`):

```rust
    #[error("launch_proxy rule has an invalid JSON path: {0}")]
    InvalidProxyPath(String),
    #[error("launch_proxy rule has an invalid regex pattern: {0}")]
    InvalidProxyRegex(String),
```

- [ ] **Step 4: Implement `validate_launch_proxy`**

Add near `validate_permissions` (~line 475):

```rust
    fn validate_launch_proxy(&self) -> Result<(), ValidateError> {
        let Some(proxy) = self.features.as_ref().and_then(|f| f.launch_proxy.as_ref()) else {
            return Ok(());
        };
        for rule in &proxy.rules {
            let path = match &rule.target {
                ProxyTarget::Header { .. } => None,
                ProxyTarget::Body { path } => Some(path.as_str()),
            };
            if let Some(path) = path {
                crate::proxy_path::parse_path(path)
                    .map_err(|e| ValidateError::InvalidProxyPath(format!("{path}: {e}")))?;
            }
            if let ProxyOp::Strip { pattern, regex: true } = &rule.op {
                regex::Regex::new(pattern)
                    .map_err(|e| ValidateError::InvalidProxyRegex(format!("{pattern}: {e}")))?;
            }
            for cond in &rule.when {
                if let ProxyConditionTarget::Body { path: Some(path) } = &cond.target {
                    crate::proxy_path::parse_path(path)
                        .map_err(|e| ValidateError::InvalidProxyPath(format!("{path}: {e}")))?;
                }
                if let ProxyCheck::Matches { pattern, regex: true } = &cond.check {
                    regex::Regex::new(pattern)
                        .map_err(|e| ValidateError::InvalidProxyRegex(format!("{pattern}: {e}")))?;
                }
            }
        }
        Ok(())
    }
```

Add `self.validate_launch_proxy()?;` to `Config::validate` (~line 471, after
`self.validate_permissions()?;`).

Add `mod proxy_path;` to `crates/llmenv-config/src/lib.rs:1-3` (alongside
`mod schema; mod template; mod validate;`) and `pub use proxy_path::{parse_path, PathParseError};`
to the `pub use` block — this crate now owns the path parser since both
validation (here) and, transitively, the main crate's runtime evaluator
(Task 4 lives in `crates/llmenv-config/src/proxy_path.rs`, not
`src/launch/proxy.rs` — see Task 4's note) depend on the exact same grammar.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p llmenv-config validate_rejects_bad_json_path validate_rejects_bad_regex validate_accepts_well_formed_launch_proxy`
Expected: PASS (once Task 4's `parse_path` exists — see the ordering note
above).

- [ ] **Step 6: Commit**

```bash
git add crates/llmenv-config/src/validate.rs crates/llmenv-config/src/lib.rs
git commit -m "feat(config): validate launch_proxy JSON paths and regexes"
```

---

### Task 4: JSON-path-lite parser and get/set/remove evaluator

**Files:**
- Create: `crates/llmenv-config/src/proxy_path.rs`

> Lives in `llmenv-config`, not `src/launch/proxy.rs`, because Task 3's
> config-load-time validation needs the exact same parser the runtime
> evaluator uses — putting it in the main binary crate would leave
> `llmenv-config` unable to validate paths at all (dependency direction only
> goes `llmenv` → `llmenv-config`, never the reverse).

**Interfaces:**
- Produces: `pub enum PathSegment { Key(String), Index(usize) }`,
  `pub struct PathParseError(String)` (implements `std::error::Error` via
  `thiserror`), `pub fn parse_path(path: &str) -> Result<Vec<PathSegment>, PathParseError>`,
  `pub fn get_path<'a>(value: &'a serde_json::Value, segments: &[PathSegment]) -> Option<&'a serde_json::Value>`,
  `pub fn set_path(value: &mut serde_json::Value, segments: &[PathSegment], new_value: serde_json::Value)`
  (upsert — creates missing intermediate objects/array slots),
  `pub fn remove_path(value: &mut serde_json::Value, segments: &[PathSegment]) -> bool`
  (returns whether the path existed). Used by Task 3 (validation, parse only)
  and Task 5 (rule engine, all four functions).

- [ ] **Step 1: Write the failing tests**

Create `crates/llmenv-config/src/proxy_path.rs` starting with just the test
module (so Step 2 fails on missing types, not missing tests):

```rust
#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
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
        assert_eq!(parse_path("thinking").unwrap(), vec![PathSegment::Key("thinking".into())]);
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
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p llmenv-config proxy_path`
Expected: FAIL to compile — `PathSegment`, `parse_path`, etc. don't exist.

- [ ] **Step 3: Implement the parser and evaluator**

Add above the test module in `crates/llmenv-config/src/proxy_path.rs`:

```rust
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
                let Some(close) = rest.find(']') else {
                    return Err(PathParseError(format!("unmatched '[' in path: {path}")));
                };
                if !rest.starts_with('[') {
                    return Err(PathParseError(format!("expected '[' in path: {path}")));
                }
                let idx_str = &rest[1..close];
                let idx: usize = idx_str
                    .parse()
                    .map_err(|_| PathParseError(format!("invalid index '{idx_str}' in path: {path}")))?;
                segments.push(PathSegment::Index(idx));
                rest = &rest[close + 1..];
            }
        } else {
            segments.push(PathSegment::Key(rest.to_string()));
        }
    }
    if segments.is_empty() {
        return Err(PathParseError(format!("no segments parsed from path: {path}")));
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
pub fn set_path(value: &mut serde_json::Value, segments: &[PathSegment], new_value: serde_json::Value) {
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
                map.entry(k.clone())
                    .or_insert(serde_json::Value::Null)
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
        (PathSegment::Index(i), serde_json::Value::Array(arr)) => {
            if *i < arr.len() {
                arr.remove(*i);
                true
            } else {
                false
            }
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p llmenv-config proxy_path`
Expected: PASS (all 11 tests).

- [ ] **Step 5: Add a property test for get/set round-trip**

Add to the same test module (uses `proptest`, already a dev-dependency of
`llmenv-config`):

```rust
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
```

- [ ] **Step 6: Run the property test**

Run: `cargo test -p llmenv-config set_then_get_round_trips`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/llmenv-config/src/proxy_path.rs
git commit -m "feat(config): add JSON-path-lite parser for launch proxy rules"
```

---

### Task 5: Rule engine

**Files:**
- Create: `src/launch/proxy.rs` (this task adds only the rule-application
  logic; Task 6 adds the HTTP server to the same file)

**Interfaces:**
- Consumes: `llmenv_config::{ProxyRule, ProxyCondition, ProxyTarget, ProxyConditionTarget, ProxyOp, ProxyCheck}`
  (Task 2), `llmenv_config::{parse_path, get_path, set_path, remove_path, PathSegment}`
  (Task 4).
- Produces: `pub(crate) fn apply_rules(rules: &[ProxyRule], headers: &mut http::HeaderMap, body: &mut serde_json::Value)`.
  Used by Task 6's request handler.

- [ ] **Step 1: Write the failing tests**

Create `src/launch/proxy.rs` with:

```rust
//! Local API-proxy mode for `llmenv launch claude_code` (#1289). See
//! `docs/superpowers/specs/2026-08-28-launch-api-proxy-design.md`.

use llmenv_config::{ProxyCheck, ProxyCondition, ProxyConditionTarget, ProxyOp, ProxyRule, ProxyTarget};

/// Apply every rule in `rules`, in order, to `headers`/`body`. A rule with a
/// `when` clause whose conditions don't all match is skipped. Application
/// failures (missing `Remove`/`Strip` target) are logged and skipped, never
/// fatal — see the design spec's Error handling section.
pub(crate) fn apply_rules(rules: &[ProxyRule], headers: &mut http::HeaderMap, body: &mut serde_json::Value) {
    for rule in rules {
        if !rule.when.iter().all(|c| condition_matches(c, headers, body)) {
            continue;
        }
        apply_op(rule, headers, body);
    }
}

fn condition_matches(cond: &ProxyCondition, headers: &http::HeaderMap, body: &serde_json::Value) -> bool {
    match &cond.target {
        ProxyConditionTarget::Header { name } => {
            let value = headers.get(name).and_then(|v| v.to_str().ok());
            check_matches_str(&cond.check, value)
        }
        ProxyConditionTarget::Body { path: None } => {
            let Ok(serialized) = serde_json::to_string(body) else {
                return false;
            };
            check_matches_str(&cond.check, Some(&serialized))
        }
        ProxyConditionTarget::Body { path: Some(path) } => {
            let Ok(segments) = llmenv_config::parse_path(path) else {
                tracing::warn!("launch proxy: unparseable path '{path}' at request time, skipping condition");
                return false;
            };
            let found = llmenv_config::get_path(body, &segments);
            match &cond.check {
                ProxyCheck::Missing => found.is_none(),
                ProxyCheck::Present => found.is_some(),
                ProxyCheck::Equals(expected) => found == Some(expected),
                ProxyCheck::Matches { pattern, regex } => {
                    let Some(found) = found else { return false };
                    let text = found.as_str().map(str::to_string).unwrap_or_else(|| found.to_string());
                    matches_pattern(pattern, *regex, &text)
                }
            }
        }
    }
}

fn check_matches_str(check: &ProxyCheck, value: Option<&str>) -> bool {
    match check {
        ProxyCheck::Missing => value.is_none(),
        ProxyCheck::Present => value.is_some(),
        ProxyCheck::Equals(expected) => value.and_then(|v| expected.as_str().map(|e| e == v)).unwrap_or(false),
        ProxyCheck::Matches { pattern, regex } => {
            let Some(value) = value else { return false };
            matches_pattern(pattern, *regex, value)
        }
    }
}

fn matches_pattern(pattern: &str, is_regex: bool, text: &str) -> bool {
    if is_regex {
        regex::Regex::new(pattern).is_ok_and(|re| re.is_match(text))
    } else {
        text.contains(pattern)
    }
}

fn apply_op(rule: &ProxyRule, headers: &mut http::HeaderMap, body: &mut serde_json::Value) {
    match (&rule.target, &rule.op) {
        (ProxyTarget::Header { name }, ProxyOp::Set(value)) => {
            let Some(s) = value.as_str() else {
                tracing::warn!("launch proxy: header rule for '{name}' has a non-string value, skipping");
                return;
            };
            let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(name.as_str()),
                http::HeaderValue::try_from(s),
            ) else {
                tracing::warn!("launch proxy: invalid header name/value for '{name}', skipping");
                return;
            };
            headers.insert(name, value);
        }
        (ProxyTarget::Header { name }, ProxyOp::Remove) => {
            headers.remove(name);
        }
        (ProxyTarget::Header { name }, ProxyOp::Strip { pattern, regex }) => {
            let Some(current) = headers.get(name).and_then(|v| v.to_str().ok()) else {
                tracing::warn!("launch proxy: strip rule target header '{name}' is absent, skipping");
                return;
            };
            let stripped = strip_pattern(pattern, *regex, current);
            if let Ok(value) = http::HeaderValue::try_from(stripped) {
                headers.insert(name.parse::<http::HeaderName>().unwrap_or(http::header::WARNING), value);
            }
        }
        (ProxyTarget::Body { path }, op) => apply_body_op(path, op, body),
    }
}

fn apply_body_op(path: &str, op: &ProxyOp, body: &mut serde_json::Value) {
    let Ok(segments) = llmenv_config::parse_path(path) else {
        tracing::warn!("launch proxy: unparseable path '{path}' at request time, skipping");
        return;
    };
    match op {
        ProxyOp::Set(value) => llmenv_config::set_path(body, &segments, value.clone()),
        ProxyOp::Remove => {
            if !llmenv_config::remove_path(body, &segments) {
                tracing::warn!("launch proxy: remove rule target '{path}' is absent, skipping");
            }
        }
        ProxyOp::Strip { pattern, regex } => {
            let Some(current) = llmenv_config::get_path(body, &segments).and_then(|v| v.as_str()) else {
                tracing::warn!("launch proxy: strip rule target '{path}' is absent or not a string, skipping");
                return;
            };
            let stripped = strip_pattern(pattern, *regex, current);
            llmenv_config::set_path(body, &segments, serde_json::Value::String(stripped));
        }
    }
}

fn strip_pattern(pattern: &str, is_regex: bool, text: &str) -> String {
    if is_regex {
        match regex::Regex::new(pattern) {
            Ok(re) => re.replace_all(text, "").into_owned(),
            Err(e) => {
                tracing::warn!("launch proxy: invalid regex '{pattern}': {e}, leaving text unchanged");
                text.to_string()
            }
        }
    } else {
        text.replace(pattern, "")
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(when: Vec<ProxyCondition>, target: ProxyTarget, op: ProxyOp) -> ProxyRule {
        ProxyRule { when, target, op }
    }

    #[test]
    fn set_upserts_missing_body_field() {
        let rules = vec![rule(
            vec![],
            ProxyTarget::Body { path: "thinking".into() },
            ProxyOp::Set(json!({"type": "disabled"})),
        )];
        let mut headers = http::HeaderMap::new();
        let mut body = json!({});
        apply_rules(&rules, &mut headers, &mut body);
        assert_eq!(body, json!({"thinking": {"type": "disabled"}}));
    }

    #[test]
    fn rule_skipped_when_any_when_condition_fails() {
        let rules = vec![rule(
            vec![ProxyCondition {
                target: ProxyConditionTarget::Body { path: None },
                check: ProxyCheck::Matches { pattern: "nope".into(), regex: false },
            }],
            ProxyTarget::Body { path: "thinking".into() },
            ProxyOp::Set(json!({"type": "disabled"})),
        )];
        let mut headers = http::HeaderMap::new();
        let mut body = json!({});
        apply_rules(&rules, &mut headers, &mut body);
        assert_eq!(body, json!({}));
    }

    #[test]
    fn rule_fires_when_all_and_conditions_match() {
        let rules = vec![rule(
            vec![
                ProxyCondition {
                    target: ProxyConditionTarget::Header { name: "x-billing-header".into() },
                    check: ProxyCheck::Present,
                },
                ProxyCondition {
                    target: ProxyConditionTarget::Body { path: Some("system[0].text".into()) },
                    check: ProxyCheck::Matches { pattern: "security monitor".into(), regex: false },
                },
                ProxyCondition {
                    target: ProxyConditionTarget::Body { path: Some("thinking".into()) },
                    check: ProxyCheck::Missing,
                },
            ],
            ProxyTarget::Body { path: "thinking".into() },
            ProxyOp::Set(json!({"type": "disabled"})),
        )];
        let mut headers = http::HeaderMap::new();
        headers.insert("x-billing-header", "1".parse().unwrap());
        let mut body = json!({"system": [{"text": "You are a security monitor for autonomous AI coding agents."}]});
        apply_rules(&rules, &mut headers, &mut body);
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn remove_is_noop_when_path_absent() {
        let rules = vec![rule(vec![], ProxyTarget::Body { path: "nope".into() }, ProxyOp::Remove)];
        let mut headers = http::HeaderMap::new();
        let mut body = json!({"a": 1});
        apply_rules(&rules, &mut headers, &mut body);
        assert_eq!(body, json!({"a": 1}));
    }

    #[test]
    fn strip_removes_regex_match_from_body_text() {
        let rules = vec![rule(
            vec![],
            ProxyTarget::Body { path: "system[0].text".into() },
            ProxyOp::Strip { pattern: "boilerplate.*".into(), regex: true },
        )];
        let mut headers = http::HeaderMap::new();
        let mut body = json!({"system": [{"text": "keep this. boilerplate stuff to cut"}]});
        apply_rules(&rules, &mut headers, &mut body);
        assert_eq!(body["system"][0]["text"], "keep this. ");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test proxy::tests`
Expected: FAIL to compile — `src/launch/proxy.rs` isn't wired into the module
tree yet.

- [ ] **Step 3: Wire the module in**

In `src/launch/mod.rs:11-13`, add `pub(crate) mod proxy;` alongside the
existing `mod credential_watch; mod drift; pub(crate) mod socket;` (module
needs to be `pub(crate)`, not private, since Task 7 in `mod.rs` itself calls
into it, and it's already same-crate so this matches `socket`'s visibility).

Add `http = "=<resolved-version>"` to `Cargo.toml`'s `[workspace.dependencies]`
and reference it as `{ workspace = true }` in the main crate's
`[dependencies]` — `http::HeaderMap`/`HeaderName`/`HeaderValue` are already
transitively present (pulled in by `reqwest`/`hyper`), same promotion pattern
as Task 1. Check the resolved version first:
Run: `grep -A1 '^name = "http"$' Cargo.lock`

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test proxy::tests`
Expected: PASS (all 5 tests).

- [ ] **Step 5: Run clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings. (The `unwrap_or(http::header::WARNING)` fallback in
`apply_op`'s header-strip branch exists only to satisfy the type checker when
re-parsing an already-valid header name back into a `HeaderName` — if clippy
flags it, replace with a direct `HeaderName::try_from(name.as_str())` reuse
instead of re-parsing a `&str`.)

- [ ] **Step 6: Commit**

```bash
git add src/launch/proxy.rs src/launch/mod.rs Cargo.toml Cargo.lock
git commit -m "feat(launch): add proxy rule engine (#1289)"
```

---

### Task 6: HTTP proxy server

**Files:**
- Modify: `src/launch/proxy.rs` (add to the same file as Task 5)

**Interfaces:**
- Consumes: `apply_rules` (Task 5), `reqwest::Client` (existing dependency).
- Produces: `pub(crate) async fn bind() -> anyhow::Result<(tokio::net::TcpListener, std::net::SocketAddr)>`,
  `pub(crate) async fn serve(listener: tokio::net::TcpListener, upstream: url::Url, rules: std::sync::Arc<Vec<llmenv_config::ProxyRule>>, mut shutdown: tokio::sync::watch::Receiver<bool>)`.
  Used by Task 7 (`src/launch/mod.rs`'s `run`).

- [ ] **Step 1: Write the failing integration test**

Add to `src/launch/proxy.rs`'s test module (this needs `tokio::test` and
`wiremock`, both already dev-dependencies):

```rust
    #[tokio::test]
    async fn proxy_forwards_rewritten_request_and_streams_response() {
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .and(wiremock::matchers::body_partial_json(
                json!({"thinking": {"type": "disabled"}}),
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&upstream)
            .await;

        let rules = std::sync::Arc::new(vec![rule(
            vec![],
            ProxyTarget::Body { path: "thinking".into() },
            ProxyOp::Set(json!({"type": "disabled"})),
        )]);
        let (listener, addr) = bind().await.unwrap();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let upstream_url: url::Url = upstream.uri().parse().unwrap();
        tokio::spawn(serve(listener, upstream_url, rules, rx));

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/v1/messages"))
            .json(&json!({"model": "claude-x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test proxy_forwards_rewritten_request_and_streams_response`
Expected: FAIL to compile — `bind`/`serve` don't exist.

- [ ] **Step 3: Implement `bind` and `serve`**

Add to `src/launch/proxy.rs` (above the test module):

```rust
use http_body_util::BodyExt;

/// Bind the local proxy listener on an OS-assigned ephemeral port, loopback
/// only.
///
/// # Errors
/// Returns an error when the bind fails.
pub(crate) async fn bind() -> anyhow::Result<(tokio::net::TcpListener, std::net::SocketAddr)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding launch proxy listener")?;
    let addr = listener.local_addr().context("reading launch proxy listener address")?;
    Ok((listener, addr))
}

/// Accept connections until `shutdown` reports `true` (set when the
/// supervised engine exits — see `src/launch/mod.rs`). Each request is
/// rewritten per `rules`, forwarded to `upstream`, and the response streamed
/// back unmodified.
pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    upstream: url::Url,
    rules: std::sync::Arc<Vec<llmenv_config::ProxyRule>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let client = reqwest::Client::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let io = hyper_util::rt::TokioIo::new(stream);
                let client = client.clone();
                let upstream = upstream.clone();
                let rules = std::sync::Arc::clone(&rules);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| {
                        handle(req, client.clone(), upstream.clone(), std::sync::Arc::clone(&rules))
                    });
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        tracing::debug!("launch proxy: connection error: {e}");
                    }
                });
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

type ProxyResponse = hyper::Response<http_body_util::combinators::BoxBody<hyper::body::Bytes, std::convert::Infallible>>;

async fn handle(
    req: hyper::Request<hyper::body::Incoming>,
    client: reqwest::Client,
    upstream: url::Url,
    rules: std::sync::Arc<Vec<llmenv_config::ProxyRule>>,
) -> Result<ProxyResponse, std::convert::Infallible> {
    let (parts, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return Ok(error_response(&format!("reading request body: {e}"))),
    };
    let mut json_body: serde_json::Value = if body_bytes.is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => return Ok(error_response(&format!("request body was not valid JSON: {e}"))),
        }
    };
    let mut headers = parts.headers.clone();
    apply_rules(&rules, &mut headers, &mut json_body);

    let Some(target_url) = upstream.join(parts.uri.path()) else {
        return Ok(error_response("could not build upstream URL"));
    };
    let mut builder = client.request(
        reqwest_method(&parts.method),
        target_url,
    );
    for (name, value) in &headers {
        if name == http::header::HOST {
            continue;
        }
        if let Ok(v) = value.to_str() {
            builder = builder.header(name.as_str(), v);
        }
    }
    let outgoing = match serde_json::to_vec(&json_body) {
        Ok(bytes) => bytes,
        Err(e) => return Ok(error_response(&format!("re-serializing rewritten body: {e}"))),
    };
    builder = builder.body(outgoing);

    let upstream_resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => return Ok(bad_gateway(&format!("upstream request failed: {e}"))),
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let stream = upstream_resp.bytes_stream().map(|chunk| {
        chunk
            .map(hyper::body::Frame::data)
            .map_err(|e| tracing::debug!("launch proxy: upstream stream error: {e}"))
    });
    // `map_err` above turns the error into `()` (logged, not propagated) —
    // the response body's error type here must be `Infallible` to match
    // `ProxyResponse`, and a stream read failure mid-response has nothing
    // better to do than end the stream; the client sees a truncated body,
    // which is the honest outcome.
    let stream = stream.filter_map(|r| std::future::ready(r.ok()));
    let body = http_body_util::StreamBody::new(stream.map(Ok::<_, std::convert::Infallible>))
        .boxed();

    let mut response = hyper::Response::new(body);
    *response.status_mut() = hyper::StatusCode::from_u16(status.as_u16()).unwrap_or(hyper::StatusCode::BAD_GATEWAY);
    for (name, value) in &resp_headers {
        if let (Ok(name), Ok(value)) = (
            hyper::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            hyper::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    Ok(response)
}

fn reqwest_method(method: &hyper::Method) -> reqwest::Method {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST)
}

fn error_response(msg: &str) -> ProxyResponse {
    tracing::warn!("launch proxy: {msg}");
    let body = http_body_util::Full::new(hyper::body::Bytes::from(msg.to_string()))
        .map_err(|never| match never {})
        .boxed();
    let mut resp = hyper::Response::new(body);
    *resp.status_mut() = hyper::StatusCode::BAD_REQUEST;
    resp
}

fn bad_gateway(msg: &str) -> ProxyResponse {
    tracing::warn!("launch proxy: {msg}");
    let body = http_body_util::Full::new(hyper::body::Bytes::from(msg.to_string()))
        .map_err(|never| match never {})
        .boxed();
    let mut resp = hyper::Response::new(body);
    *resp.status_mut() = hyper::StatusCode::BAD_GATEWAY;
    resp
}
```

Add `use anyhow::Context;` and `use futures_util::StreamExt;` to the top of
`src/launch/proxy.rs` (`futures_util` is already transitively present via
`reqwest`/`hyper`; promote it the same way as Task 1 —
Run: `grep -A1 '^name = "futures-util"$' Cargo.lock` for the exact version,
add to `[workspace.dependencies]` and the main crate's `[dependencies]`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test proxy_forwards_rewritten_request_and_streams_response`
Expected: PASS.

- [ ] **Step 5: Run clippy and fix any warnings**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings. (`ProxyResponse`'s `Infallible` error type is a real
compile-time constraint from `hyper::body::Body` — if clippy or `cargo check`
disagrees with the exact bound shown here, that's expected drift risk in this
task; fix by matching whatever `http_body_util::combinators::BoxBody`'s actual
signature requires in the resolved `http-body-util` version, not by
loosening error handling.)

- [ ] **Step 6: Commit**

```bash
git add src/launch/proxy.rs Cargo.toml Cargo.lock
git commit -m "feat(launch): add hyper-based proxy server forwarding via reqwest (#1289)"
```

---

### Task 7: Wire the proxy into `launch`

**Files:**
- Modify: `src/launch/mod.rs`

**Interfaces:**
- Consumes: `proxy::bind`, `proxy::serve` (Task 6), `crate::config::Config::load`
  (existing, used the same way `credential_watch`'s wiring already does at
  `src/launch/mod.rs:130`).
- Produces: nothing new exported — this task only changes `run`'s body.

- [ ] **Step 1: Write the failing integration test**

Add to `tests/launch.rs` (see that file's existing `isolated_llmenv_cmd`
pattern for full context — this step assumes a config fixture helper already
exists there; if not, follow the pattern of the nearest existing test that
writes a `config.yaml` fixture before invoking `launch`):

```rust
#[tokio::test]
async fn launch_proxy_rewrites_thinking_field_end_to_end() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&upstream)
        .await;

    let (temp, mut cmd) = support::isolated_llmenv_cmd();
    support::write_config(
        &temp,
        &format!(
            r#"
features:
  launch_proxy:
    enabled: true
    rules:
      - target: body
        path: "thinking"
        op:
          set:
            type: disabled
"#
        ),
    );
    let env_dump = temp.path().join("env.txt");
    cmd.env("ANTHROPIC_BASE_URL", upstream.uri())
        .env("FAKE_ENGINE_ENV_DUMP", &env_dump)
        .arg("launch")
        .arg("claude_code");
    cmd.assert().success();

    let dumped = std::fs::read_to_string(&env_dump).unwrap();
    let base_url_line = dumped
        .lines()
        .find(|l| l.starts_with("ANTHROPIC_BASE_URL="))
        .expect("ANTHROPIC_BASE_URL should be set in the child's env");
    assert!(
        base_url_line != format!("ANTHROPIC_BASE_URL={}", upstream.uri()),
        "ANTHROPIC_BASE_URL should be rewritten to the local proxy, not left as the original upstream"
    );
}
```

(This test asserts the env-wiring half — that `ANTHROPIC_BASE_URL` really
does change. The rewrite-rule behavior itself is already covered end-to-end
by Task 6's `proxy_forwards_rewritten_request_and_streams_response`, so this
test doesn't need to also make an HTTP call through the fake engine.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test launch launch_proxy_rewrites_thinking_field_end_to_end`
Expected: FAIL — `ANTHROPIC_BASE_URL` is unchanged (proxy not started yet).

- [ ] **Step 3: Wire the proxy into `run`**

In `src/launch/mod.rs`, change `let resolved = crate::cli::resolve_env(...)?;`
(line 80) to `let mut resolved = crate::cli::resolve_env(...)?;` (needs `mut`
now).

Inside `rt.block_on`'s `async` block, after the notice-socket bind block
(after line 112's closing `};`) and before the drift-watch block (line 114),
add:

```rust
        let proxy_shutdown_tx: Option<tokio::sync::watch::Sender<bool>> =
            match crate::config::Config::load(&config_path) {
                Ok(config) => {
                    // Claude Code only for now (#1289's approved design scope
                    // — `ANTHROPIC_BASE_URL` is Claude Code/Anthropic-SDK
                    // specific); same gating pattern as the credential-watch
                    // wiring above (line 129: `adapter.name() == "claude-code"`).
                    let launch_proxy = config
                        .features
                        .as_ref()
                        .and_then(|f| f.launch_proxy.as_ref())
                        .filter(|p| p.enabled && adapter.name() == "claude-code");
                    match launch_proxy {
                        Some(launch_proxy) => match proxy::bind().await {
                            Ok((listener, addr)) => {
                                let upstream_str = resolved
                                    .vars
                                    .get("ANTHROPIC_BASE_URL")
                                    .cloned()
                                    .unwrap_or_else(|| "https://api.anthropic.com".to_string());
                                match upstream_str.parse::<url::Url>() {
                                    Ok(upstream) => {
                                        let rules = Arc::new(launch_proxy.rules.clone());
                                        let (tx, rx) = tokio::sync::watch::channel(false);
                                        tokio::spawn(proxy::serve(listener, upstream, rules, rx));
                                        resolved.vars.insert(
                                            "ANTHROPIC_BASE_URL".to_string(),
                                            format!("http://{addr}"),
                                        );
                                        Some(tx)
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "llmenv: could not parse existing ANTHROPIC_BASE_URL \
                                             '{upstream_str}', launch proxy disabled for this \
                                             session: {e}"
                                        );
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "llmenv: could not start launch proxy, continuing without \
                                     request rewriting: {e:#}"
                                );
                                None
                            }
                        },
                        None => None,
                    }
                }
                Err(e) => {
                    tracing::debug!("launch: could not load config, launch proxy disabled: {e:#}");
                    None
                }
            };
```

Then change the block's tail expression (currently, ~line 155-165, just
`supervision_loop(...).await`) so the proxy's shutdown signal fires once the
engine session ends, before the block returns:

```rust
        let result = supervision_loop(
            EngineTarget {
                adapter: adapter.as_ref(),
                bin_path: &bin_path,
                args: &args,
            },
            &resolved,
            notice_socket,
            narrow.auto_restart,
        )
        .await;
        if let Some(tx) = proxy_shutdown_tx {
            let _ = tx.send(true);
        }
        result
```

This keeps the async block's overall type unchanged
(`anyhow::Result<std::process::ExitStatus>`, matching `supervision_loop`'s own
return type) — `result` is exactly that value, returned as the bare tail
expression, so `let status = rt.block_on(async { ... })?;` (line 93/166)
needs no change.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test launch launch_proxy_rewrites_thinking_field_end_to_end`
Expected: PASS.

- [ ] **Step 5: Run the full launch test suite for regressions**

Run: `cargo test --test launch`
Expected: PASS — no regressions in existing `launch` behavior (proxy is
opt-in via `features.launch_proxy.enabled`, so every test without that config
takes the untouched path).

- [ ] **Step 6: Run clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add src/launch/mod.rs tests/launch.rs
git commit -m "feat(launch): start/stop the API proxy around the supervised session (#1289)"
```

---

### Task 8: Documentation and changelog

**Files:**
- Modify: `website/docs/commands.md` (new subsection under `## launch`)
- Modify: `CHANGELOG-4.md` (new entry under `[Unreleased]` → `### Added`)
- Regenerate: `website/docs/changelog.md` via `scripts/sync-changelog-doc.sh`
  (generated file, never hand-edited — see its own header comment)

- [ ] **Step 1: Check the current `## launch` section for placement**

Run: `grep -n "^## launch" website/docs/commands.md`

- [ ] **Step 2: Add the docs subsection**

In `website/docs/commands.md`, under the existing `## launch` section, add:

```markdown
### API proxy (`features.launch_proxy`)

(added in v4.0.0)

`llmenv launch claude_code` can start a local HTTP proxy for the session that
rewrites outbound Anthropic API requests before they leave the machine —
useful for trimming Claude Code's injected system prompt, or conditionally
setting fields the request would otherwise omit.

Enable it in `config.yaml`:

```yaml
features:
  launch_proxy:
    enabled: true
    rules:
      - target: body
        path: "system[0].text"
        op:
          strip:
            pattern: "verbose boilerplate.*"
            regex: true
```

Each rule has an optional `when` list (AND-combined conditions gating whether
the rule fires: `missing`/`present`/`equals`/`matches` on a header or a
JSON-path-lite location in the request body) and an `op`: `set` (creates the
target if missing), `remove`, or `strip` (regex or substring removal from a
string value). `ANTHROPIC_BASE_URL` is chained through if already set (e.g. a
corporate gateway) rather than clobbered. Off by default.
```

- [ ] **Step 3: Add the changelog entry**

In `CHANGELOG-4.md`, under `## [Unreleased]` → `### Added` (create the
`### Added` subsection if the file's `[Unreleased]` section doesn't already
have one — check first):

```markdown
- `features.launch_proxy`: `llmenv launch claude_code` can start a local
  HTTP proxy that rewrites outbound Anthropic API requests per declarative
  rules before forwarding them — e.g. trimming the injected system prompt.
  See [Commands: API proxy](https://phaedrus1992.github.io/llmenv/docs/commands#api-proxy-featureslaunch_proxy).
  (#1289)
```

- [ ] **Step 4: Regenerate the docs-site changelog**

Run: `scripts/sync-changelog-doc.sh`
Expected: `website/docs/changelog.md` updates to include the new entry.

- [ ] **Step 5: Run markdownlint**

Run: `prek run markdownlint --files website/docs/commands.md CHANGELOG-4.md website/docs/changelog.md`
Expected: PASS (or run whatever markdown lint hook `prek run --all-files`
surfaces for these files — matches the `markdownlint` hook already observed
running on this repo's commits, per Task 1's spec-doc commit output).

- [ ] **Step 6: Commit**

```bash
git add website/docs/commands.md CHANGELOG-4.md website/docs/changelog.md
git commit -m "docs: document features.launch_proxy (#1289)"
```

---

## Execution Correction (discovered during Task 2)

`ProxyOp::Set(serde_json::Value)` and `ProxyCheck::Equals(serde_json::Value)`
as written in this plan are **tuple variants** on an **externally-tagged**
enum. `serde_yaml_ng` (this repo's YAML deserializer) cannot deserialize a
data-carrying externally-tagged enum variant from a plain YAML map — it
requires a YAML `!tag`, which `config.yaml` never uses. Verified with a
minimal repro against the real dependency (not guessed): externally-tagged
`Matches { pattern, regex }` failed with `"invalid type: map, expected a YAML
tag starting with '!'"`; switching both enums to internal tagging
(`#[serde(tag = "kind", rename_all = "snake_case")]`) and wrapping the tuple
payload in a named field fixed it.

**The actual, corrected shapes** (already implemented in Task 2, committed):

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProxyOp {
    Set { value: serde_json::Value },
    Remove,
    Strip { pattern: String, regex: bool },
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProxyCheck {
    Missing,
    Present,
    Equals { value: serde_json::Value },
    Matches { pattern: String, regex: bool },
}
```

Every later task in this plan (3, 5, 6, 8) that constructs, matches, or
writes YAML for `ProxyOp::Set(...)`/`ProxyCheck::Equals(...)` as a tuple
variant, or writes `check: present`/`check: missing` as a bare scalar, or
`op: { set: ... }`/`check: { matches: ... }` without a `kind:` discriminator,
is stale — apply the corrected shape above instead:

- Rust construction: `ProxyOp::Set(x)` → `ProxyOp::Set { value: x }`;
  `ProxyCheck::Equals(x)` → `ProxyCheck::Equals { value: x }`. Match arms:
  `ProxyOp::Set(v) => ...` → `ProxyOp::Set { value: v } => ...` (same for
  `ProxyCheck::Equals`).
- YAML: `check: present` → `check: { kind: present }`; `check: missing` →
  `check: { kind: missing }`; `check: { matches: { pattern: P, regex: R } }`
  → `check: { kind: matches, pattern: P, regex: R }`; `op: { set: V } }` →
  `op: { kind: set, value: V }`; `op: { strip: { pattern: P, regex: R } }` →
  `op: { kind: strip, pattern: P, regex: R }`; `op: remove` stays a bare
  string (`Remove` is still a unit variant, and internally-tagged unit
  variants under `serde_yaml_ng` still need the map form: `op: { kind:
  remove }` — not a bare string; verify this the same way if it matters to a
  later task's test).

## Self-Review Notes (for the executor)

- **Task ordering:** Task 4 must land before Task 3's tests can pass (see the
  explicit note in Task 3). Do them in file order (3 then 4) only if you add
  the stub Task 3 mentions; otherwise do 4 then 3.
- **`ProxyResponse`'s exact type** (Task 6) depends on the precise API shape
  of the resolved `http-body-util` version. If `cargo check` reports a type
  mismatch on `BoxBody`/`StreamBody`, that's expected drift risk flagged
  in-task — resolve by matching the installed version's actual signatures,
  not by weakening error handling or unwrapping.
- **Every new dependency in this plan (`hyper`, `hyper-util`, `http-body-util`,
  `regex`, `http`, `futures-util`) is already present in `Cargo.lock`
  transitively** — confirm this at each promotion step (Tasks 1, 5, 6) with
  `cargo tree --duplicates` before/after, per the Global Constraints
  dependency-pinning rule.
