# Provider/Model Config Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a neutral, engine-agnostic schema for custom model provider
endpoints and per-scope default model selection, rendered by `CrushAdapter`
and no-op'd by `ClaudeCodeAdapter`.

**Architecture:** Four new schema types (`ModelProvider`, `ModelSource`,
`ModelCost`, `ModelRef`) plus two new `Capabilities` fields
(`model_providers: Vec<ModelProvider>`, `default_models:
BTreeMap<String, ModelRef>`), following the exact selection/merge/render
pipeline already used by `lsp`/`mcp` (tag-intersected list, concat+dedup at
merge time, map-insert-by-key at render time for override semantics) and by
`env` (per-key highest-precedence-wins map merge).

**Tech Stack:** Rust, `serde`/`serde_yaml`/`serde_json`, `proptest` (already
a dev-dependency).

## Global Constraints

- **Zero secrets-at-rest regression**: `api_key`/`headers` are pure
  passthrough strings — llmenv never executes commands or resolves `$VAR`
  syntax itself (spec Non-goals).
- **`ClaudeCodeAdapter` is a true no-op**: it must never read
  `capabilities.model_providers`/`.default_models` at all, matching how it
  already ignores `capabilities.lsp`.
- **No path/filesystem validation needed** for this schema — every field is
  a string rendered directly into JSON (spec Merge & Validation Rules).
- **`f64` cost fields break `Eq`**: `ModelCost.input`/`.output` are `f64`
  (floats aren't `Eq` — NaN violates reflexivity). This means `ModelCost`,
  `ModelSource`, `ModelProvider`, and therefore `Capabilities` itself can no
  longer derive `Eq` (only `PartialEq`). Confirmed nothing in the codebase
  currently requires `Capabilities: Eq` as a trait bound (checked via
  `rg "Capabilities.*Hash|HashSet<Capabilities>"`, zero hits) — this is a
  visible but currently-inert signature change. Task 1 makes this change
  explicitly and documents why.

---

### Task 1: Schema types + `Capabilities` fields

**Files:**
- Modify: `crates/llmenv-config/src/schema.rs` (add types after `LspServer`,
  which ends around line 705; add `Capabilities` fields after `host` at
  line ~319; update `Capabilities::is_empty()` around line ~334)
- Test: `crates/llmenv-config/src/schema.rs` (inline `#[cfg(test)]` module,
  or wherever existing `LspServer`/`McpServer` round-trip tests live —
  search `mod tests` in the same file)

**Interfaces:**
- Produces: `ModelProvider{id: String, name: Option<String>, when: Vec<String>,
  base_url: Option<String>, api_type: Option<String>, api_key: Option<String>,
  headers: BTreeMap<String,String>, disabled: bool, models: Vec<ModelSource>}`
- Produces: `ModelSource{id: String, name: Option<String>, reasoning: bool,
  context_window: Option<u32>, max_tokens: Option<u32>, cost: Option<ModelCost>,
  modalities: Vec<String>}`
- Produces: `ModelCost{input: f64, output: f64, cache_read: Option<f64>,
  cache_write: Option<f64>}`
- Produces: `ModelRef{provider: String, model: String}`
- Produces: `Capabilities.model_providers: Vec<ModelProvider>`
- Produces: `Capabilities.default_models: BTreeMap<String, ModelRef>`

- [ ] **Step 1: Write the failing round-trip test**

Find the existing YAML round-trip test pattern for `LspServer` in
`crates/llmenv-config/src/schema.rs` (search `mod tests` in that file — there
is an existing test module with `#[test]` functions covering `LspServer`
serde round-trips) and add a sibling test:

```rust
#[test]
fn model_provider_yaml_roundtrip() {
    let yaml = r#"
id: ollama
name: Local Ollama
base_url: http://localhost:11434/v1
api_type: openai
api_key: "$OLLAMA_KEY"
headers:
  x-custom: value
models:
  - id: llama3.1:8b
    name: Llama 3.1 8B
    reasoning: false
    context_window: 128000
    max_tokens: 32000
    cost:
      input: 0.0
      output: 0.0
    modalities:
      - text
"#;
    let provider: ModelProvider = serde_yaml::from_str(yaml).expect("parse");
    assert_eq!(provider.id, "ollama");
    assert_eq!(provider.base_url, Some("http://localhost:11434/v1".to_string()));
    assert_eq!(provider.models.len(), 1);
    assert_eq!(provider.models[0].id, "llama3.1:8b");
    assert_eq!(
        provider.models[0].cost,
        Some(ModelCost { input: 0.0, output: 0.0, cache_read: None, cache_write: None })
    );
}

#[test]
fn model_provider_only_id_required() {
    // Mirrors Pi's "only id is required for local models" convention.
    let yaml = "id: bare-provider\n";
    let provider: ModelProvider = serde_yaml::from_str(yaml).expect("parse");
    assert_eq!(provider.id, "bare-provider");
    assert_eq!(provider.name, None);
    assert!(provider.models.is_empty());
}

#[test]
fn default_models_map_yaml_roundtrip() {
    let yaml = r#"
large:
  provider: anthropic
  model: claude-opus-4-7
small:
  provider: ollama
  model: llama3.1:8b
"#;
    let map: std::collections::BTreeMap<String, ModelRef> =
        serde_yaml::from_str(yaml).expect("parse");
    assert_eq!(map["large"].provider, "anthropic");
    assert_eq!(map["small"].model, "llama3.1:8b");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib -p llmenv-config model_provider_yaml_roundtrip model_provider_only_id_required default_models_map_yaml_roundtrip`
Expected: FAIL — `ModelProvider`/`ModelSource`/`ModelCost`/`ModelRef` are
undefined (`cannot find type` compile error).

- [ ] **Step 3: Add the schema types**

Insert after the `LspServer` struct (ends at line ~705 — find the exact spot
by searching for `pub struct LspServer` and inserting after its closing
`}`), in `crates/llmenv-config/src/schema.rs`:

```rust
/// A custom/self-hosted model provider endpoint (Ollama, vLLM, LM Studio, a
/// proxy, or an override of a built-in provider). Selected by tag
/// intersection like `mcp`/`lsp`/`skills`. Engines without a multi-provider
/// concept (`supports_model_providers() == false`) silently skip these —
/// declaring one in a shared bundle is legitimate; it is simply a no-op for
/// such adapters.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct ModelProvider {
    /// Stable identifier, used as the map key when rendered (e.g. "ollama",
    /// "my-proxy") and as the `provider` field target of `ModelRef`.
    pub id: String,
    /// Display name.
    pub name: Option<String>,
    /// Tags that activate this provider, intersected with active scope tags.
    #[serde(default)]
    pub when: Vec<String>,
    /// API endpoint URL.
    pub base_url: Option<String>,
    /// Wire format, e.g. "openai", "anthropic", "google". Open string, not
    /// an enum — new wire formats appear faster than llmenv releases.
    pub api_type: Option<String>,
    /// Passthrough credential string — may be a literal, or a $VAR/!command
    /// reference the *target engine* resolves at its own runtime. llmenv
    /// never interprets this value (resolving it here would write a
    /// plaintext secret into the materialized cache directory).
    pub api_key: Option<String>,
    /// Extra HTTP headers, passthrough (same rationale as `api_key`).
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// When `true` the provider is excluded from the resolved set for all engines.
    #[serde(default)]
    pub disabled: bool,
    /// Models exposed by this provider.
    #[serde(default)]
    pub models: Vec<ModelSource>,
}

/// One model exposed by a `ModelProvider`. All fields but `id` are optional
/// — mirrors Pi's "only `id` is required for local models" convention.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct ModelSource {
    pub id: String,
    pub name: Option<String>,
    /// Supports extended thinking/reasoning.
    #[serde(default)]
    pub reasoning: bool,
    /// Context window size in tokens.
    pub context_window: Option<u32>,
    /// Maximum output tokens.
    pub max_tokens: Option<u32>,
    /// Cost per million tokens.
    pub cost: Option<ModelCost>,
    /// Input modalities, e.g. `["text"]` or `["text", "image"]`.
    #[serde(default)]
    pub modalities: Vec<String>,
}

/// Cost per million tokens, matching the near-identical shape used by Crush,
/// Pi, and OpenCode's own model schemas.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Default)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

/// A pointer to a model+provider pair, used for default-model selection.
/// `provider` may be a `ModelProvider.id` declared alongside it, or an
/// engine builtin id (e.g. Crush's built-in `"anthropic"`) that llmenv has
/// no knowledge of and does not validate against.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}
```

Note `ModelRef` keeps `Eq` (its fields are both `String`) while
`ModelProvider`/`ModelSource`/`ModelCost` drop it (transitively, because of
`ModelCost`'s `f64` fields).

- [ ] **Step 4: Add the `Capabilities` fields**

In `crates/llmenv-config/src/schema.rs`, add after the `host` field (ends at
line ~319, just before the closing `}` of `Capabilities`):

```rust
    /// Custom/self-hosted model provider endpoints declared inside a bundle.
    /// A list — concatenates across contributors, same model as `mcp`/`lsp`.
    /// Selected by tag intersection. Engines with
    /// `supports_model_providers() == false` silently skip these entries.
    #[serde(default)]
    pub model_providers: Vec<ModelProvider>,
    /// Default model selection, keyed by an open-string role ("large",
    /// "small", etc — matches Crush's real `SelectedModelType` without
    /// hardcoding to it). Merged per-key: higher-precedence contributor
    /// wins on collision (same scalar rule as `env`/`host`), not
    /// tag-intersected like a list — there is only one default per role.
    #[serde(default)]
    pub default_models: std::collections::BTreeMap<String, ModelRef>,
```

Because `ModelProvider` no longer implements `Eq`, remove `Eq` from
`Capabilities`'s own derive list (line ~240):

```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct Capabilities {
```

Update `Capabilities::is_empty()` (search for `pub fn is_empty` in the same
`impl Capabilities` block) to add:

```rust
            && self.model_providers.is_empty()
            && self.default_models.is_empty()
```

to the existing `&&`-chain (insert alongside the `self.host.is_empty()` line
or wherever the chain currently ends).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib -p llmenv-config model_provider_yaml_roundtrip model_provider_only_id_required default_models_map_yaml_roundtrip`
Expected: PASS (3 tests).

Then run the full config crate suite to confirm the `Eq` removal doesn't
break anything: `cargo test --lib -p llmenv-config`
Expected: PASS, same count as before this task (no new failures — if
anything fails here, find the call site requiring `Capabilities: Eq` and
report before proceeding; do not silently add a `PartialEq`-only workaround
without understanding the failure).

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/llmenv-config/src/schema.rs
git commit -m "feat(config): add ModelProvider/ModelSource schema for #508

Adds the neutral schema for custom model provider endpoints and
role-keyed default model selection, per the design in
docs/superpowers/specs/2026-07-01-provider-model-config-design.md.

Capabilities drops its Eq derive (PartialEq only) because ModelCost's
f64 fields can't implement Eq — confirmed nothing in the codebase
requires Capabilities: Eq as a trait bound."
```

---

### Task 2: Validation rules

**Files:**
- Modify: `crates/llmenv-config/src/validate.rs` (add error variants to
  `ValidateError` enum at line ~154, add `validate_model_providers`/
  `validate_default_models` methods near `validate_lsp` at line ~518, wire
  both into `validate()` near line ~333)
- Test: same file, `#[cfg(test)]` module (search `mod tests` — there's an
  existing test module with helpers like `config_with_skills`; look for a
  `config_with_lsp`-style helper or build one following that pattern)

**Interfaces:**
- Consumes: `ModelProvider`, `ModelSource`, `ModelRef` from Task 1.
- Produces: `ValidateError::ModelProviderEmptyId`,
  `ValidateError::ModelProviderDuplicateId(String)`,
  `ValidateError::ModelSourceEmptyId(String)`,
  `ValidateError::ModelSourceDuplicateId(String, String)`,
  `ValidateError::DefaultModelEmptyRole`,
  `ValidateError::DefaultModelEmptyRef(String)`.

- [ ] **Step 1: Write the failing tests**

Add near the existing `skill_valid_entry_is_accepted`-style tests in
`crates/llmenv-config/src/validate.rs`'s `#[cfg(test)] mod tests`:

```rust
#[test]
fn model_provider_duplicate_id_rejected() {
    let mut cfg = Config::default();
    cfg.capabilities.model_providers = vec![
        crate::ModelProvider { id: "ollama".into(), ..Default::default() },
        crate::ModelProvider { id: "ollama".into(), ..Default::default() },
    ];
    let err = cfg.validate().unwrap_err();
    assert!(matches!(err, ValidateError::ModelProviderDuplicateId(ref id) if id == "ollama"));
}

#[test]
fn model_provider_empty_id_rejected() {
    let mut cfg = Config::default();
    cfg.capabilities.model_providers =
        vec![crate::ModelProvider { id: String::new(), ..Default::default() }];
    let err = cfg.validate().unwrap_err();
    assert!(matches!(err, ValidateError::ModelProviderEmptyId));
}

#[test]
fn model_source_duplicate_id_within_provider_rejected() {
    let mut cfg = Config::default();
    cfg.capabilities.model_providers = vec![crate::ModelProvider {
        id: "ollama".into(),
        models: vec![
            crate::ModelSource { id: "llama3.1:8b".into(), ..Default::default() },
            crate::ModelSource { id: "llama3.1:8b".into(), ..Default::default() },
        ],
        ..Default::default()
    }];
    let err = cfg.validate().unwrap_err();
    assert!(matches!(
        err,
        ValidateError::ModelSourceDuplicateId(ref p, ref m)
            if p == "ollama" && m == "llama3.1:8b"
    ));
}

#[test]
fn model_provider_valid_entry_is_accepted() {
    let mut cfg = Config::default();
    cfg.capabilities.model_providers = vec![crate::ModelProvider {
        id: "ollama".into(),
        base_url: Some("http://localhost:11434/v1".into()),
        models: vec![crate::ModelSource { id: "llama3.1:8b".into(), ..Default::default() }],
        ..Default::default()
    }];
    assert!(cfg.validate().is_ok());
}

#[test]
fn default_model_empty_provider_rejected() {
    let mut cfg = Config::default();
    cfg.capabilities.default_models.insert(
        "large".into(),
        crate::ModelRef { provider: String::new(), model: "gpt-4o".into() },
    );
    let err = cfg.validate().unwrap_err();
    assert!(matches!(err, ValidateError::DefaultModelEmptyRef(ref role) if role == "large"));
}

#[test]
fn default_model_valid_entry_is_accepted() {
    let mut cfg = Config::default();
    cfg.capabilities.default_models.insert(
        "large".into(),
        crate::ModelRef { provider: "anthropic".into(), model: "claude-opus-4-7".into() },
    );
    assert!(cfg.validate().is_ok());
}
```

Check the exact `Config::default()` + `cfg.capabilities.model_providers = ...`
construction style against however `skill_valid_entry_is_accepted` builds its
`Config` in this same file (it likely goes through a `config_with_skills`
helper, not raw field assignment) — match that existing helper style rather
than the sketch above if one exists; add a `config_with_model_providers`
helper alongside it if not.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib -p llmenv-config model_provider_duplicate_id_rejected model_provider_empty_id_rejected model_source_duplicate_id_within_provider_rejected model_provider_valid_entry_is_accepted default_model_empty_provider_rejected default_model_valid_entry_is_accepted`
Expected: FAIL — `ValidateError` variants don't exist (compile error).

- [ ] **Step 3: Add the error variants**

In `crates/llmenv-config/src/validate.rs`, add to the `ValidateError` enum
(before its closing `}` at line ~154, alongside the existing
`SkillPathTraversal` variant):

```rust
    #[error("model provider has an empty id")]
    ModelProviderEmptyId,
    #[error("duplicate model provider id: {0}")]
    ModelProviderDuplicateId(String),
    #[error("model provider '{0}' has a model with an empty id")]
    ModelSourceEmptyId(String),
    #[error("model provider '{0}': duplicate model id '{1}'")]
    ModelSourceDuplicateId(String, String),
    #[error("default_models has an entry with an empty role key")]
    DefaultModelEmptyRole,
    #[error("default_models role '{0}' has an empty provider or model")]
    DefaultModelEmptyRef(String),
```

- [ ] **Step 4: Add the validation methods**

In the same `impl Config` block as `validate_lsp` (near line ~518), add:

```rust
    fn validate_model_providers(&self) -> Result<(), ValidateError> {
        let mut seen_ids = std::collections::HashSet::new();
        for p in &self.capabilities.model_providers {
            if p.id.is_empty() {
                return Err(ValidateError::ModelProviderEmptyId);
            }
            if !seen_ids.insert(&p.id) {
                return Err(ValidateError::ModelProviderDuplicateId(p.id.clone()));
            }
            let mut seen_model_ids = std::collections::HashSet::new();
            for m in &p.models {
                if m.id.is_empty() {
                    return Err(ValidateError::ModelSourceEmptyId(p.id.clone()));
                }
                if !seen_model_ids.insert(&m.id) {
                    return Err(ValidateError::ModelSourceDuplicateId(
                        p.id.clone(),
                        m.id.clone(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_default_models(&self) -> Result<(), ValidateError> {
        for (role, r#ref) in &self.capabilities.default_models {
            if role.is_empty() {
                return Err(ValidateError::DefaultModelEmptyRole);
            }
            if r#ref.provider.is_empty() || r#ref.model.is_empty() {
                return Err(ValidateError::DefaultModelEmptyRef(role.clone()));
            }
        }
        Ok(())
    }
```

Wire both into `validate()` (near line ~333, alongside
`self.validate_skills()?;`):

```rust
        self.validate_model_providers()?;
        self.validate_default_models()?;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib -p llmenv-config model_provider_duplicate_id_rejected model_provider_empty_id_rejected model_source_duplicate_id_within_provider_rejected model_provider_valid_entry_is_accepted default_model_empty_provider_rejected default_model_valid_entry_is_accepted`
Expected: PASS (6 tests).

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/llmenv-config/src/validate.rs
git commit -m "feat(config): validate model_providers/default_models for #508

Duplicate/empty id checks mirror the existing validate_mcps/validate_skills
pattern. No path-traversal checks needed — this schema is pure passthrough
strings, never joined to a filesystem path."
```

---

### Task 3: Merge rules

**Files:**
- Modify: `src/merge/capabilities.rs` (extend `merge_capabilities` around
  line 43-157; add a `resolve_default_models` function alongside
  `resolve_env` at line ~243)
- Test: same file, `#[cfg(test)] mod tests` block (search `fn contributor`
  helper and the existing `lsp`/`mcp` merge tests near line 632+)

**Interfaces:**
- Consumes: `ModelProvider`, `ModelRef` from Task 1; `dedup` from
  `crate::util` (already imported at line 23).
- Produces: `merge_capabilities` now populates
  `Capabilities.model_providers`/`.default_models` on its `Ok(Capabilities {
  ... })` return value.

- [ ] **Step 1: Write the failing tests**

Add near the existing `lsp`/`mcp` merge tests (search for `lsp: vec![server(...)]`
around line 632 in `src/merge/capabilities.rs` — there's a `server(name:
&str) -> LspServer`-style helper; add a `model_provider(id: &str) ->
ModelProvider` helper alongside it):

```rust
fn model_provider(id: &str) -> crate::config::ModelProvider {
    crate::config::ModelProvider {
        id: id.to_string(),
        ..Default::default()
    }
}

#[test]
fn model_providers_concat_across_contributors() {
    let a = contributor(
        "a",
        0,
        Capabilities { model_providers: vec![model_provider("ollama")], ..Default::default() },
    );
    let b = contributor(
        "b",
        1,
        Capabilities { model_providers: vec![model_provider("vllm")], ..Default::default() },
    );
    let out = merge_capabilities(&[a, b]).unwrap();
    assert_eq!(out.model_providers.len(), 2);
}

#[test]
fn model_providers_dedup_exact_duplicates() {
    let a = contributor(
        "a",
        0,
        Capabilities {
            model_providers: vec![model_provider("ollama"), model_provider("ollama")],
            ..Default::default()
        },
    );
    let out = merge_capabilities(&[a]).unwrap();
    assert_eq!(out.model_providers.len(), 1);
}

#[test]
fn default_models_higher_precedence_wins_per_role() {
    let a = contributor(
        "a",
        0,
        Capabilities {
            default_models: std::collections::BTreeMap::from([(
                "large".to_string(),
                crate::config::ModelRef { provider: "anthropic".into(), model: "claude-opus-4-7".into() },
            )]),
            ..Default::default()
        },
    );
    let b = contributor(
        "b",
        1,
        Capabilities {
            default_models: std::collections::BTreeMap::from([(
                "large".to_string(),
                crate::config::ModelRef { provider: "ollama".into(), model: "llama3.1:8b".into() },
            )]),
            ..Default::default()
        },
    );
    let out = merge_capabilities(&[a, b]).unwrap();
    assert_eq!(out.default_models["large"].provider, "ollama");
}

#[test]
fn default_models_independent_roles_both_survive() {
    let a = contributor(
        "a",
        0,
        Capabilities {
            default_models: std::collections::BTreeMap::from([(
                "large".to_string(),
                crate::config::ModelRef { provider: "anthropic".into(), model: "claude-opus-4-7".into() },
            )]),
            ..Default::default()
        },
    );
    let b = contributor(
        "b",
        1,
        Capabilities {
            default_models: std::collections::BTreeMap::from([(
                "small".to_string(),
                crate::config::ModelRef { provider: "ollama".into(), model: "llama3.1:8b".into() },
            )]),
            ..Default::default()
        },
    );
    let out = merge_capabilities(&[a, b]).unwrap();
    assert_eq!(out.default_models["large"].provider, "anthropic");
    assert_eq!(out.default_models["small"].provider, "ollama");
}

#[test]
fn default_models_same_precedence_conflict_errors() {
    let a = contributor(
        "a",
        0,
        Capabilities {
            default_models: std::collections::BTreeMap::from([(
                "large".to_string(),
                crate::config::ModelRef { provider: "anthropic".into(), model: "claude-opus-4-7".into() },
            )]),
            ..Default::default()
        },
    );
    let b = contributor(
        "b",
        0,
        Capabilities {
            default_models: std::collections::BTreeMap::from([(
                "large".to_string(),
                crate::config::ModelRef { provider: "ollama".into(), model: "llama3.1:8b".into() },
            )]),
            ..Default::default()
        },
    );
    let err = merge_capabilities(&[a, b]).unwrap_err();
    assert!(err.to_string().contains("conflicting default_models"));
}
```

Confirm the exact `contributor(name, precedence, capabilities)` helper
signature matches what's already in this file's test module before using it
verbatim — it's used throughout the existing `lsp`/`mcp` tests, so copy its
real signature rather than guessing.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib -p llmenv model_providers_concat_across_contributors model_providers_dedup_exact_duplicates default_models_higher_precedence_wins_per_role default_models_independent_roles_both_survive default_models_same_precedence_conflict_errors`
Expected: FAIL — `out.model_providers`/`out.default_models` don't exist yet
on the returned `Capabilities` (compile error, since `merge_capabilities`
doesn't populate them).

- [ ] **Step 3: Extend `merge_capabilities` for `model_providers`**

In `src/merge/capabilities.rs`, add a `model_providers` accumulator next to
the existing `lsp`/`skills` ones (near line 47-48):

```rust
    let mut model_providers = Vec::new();
```

Extend it in the same contributor loop as `lsp`/`skills` (near line 63-64):

```rust
        model_providers.extend(caps.model_providers.iter().cloned());
```

Dedup it alongside the others (near line 81-82):

```rust
    dedup(&mut model_providers);
```

- [ ] **Step 4: Add `resolve_default_models`**

Add a new function alongside `resolve_env` (after its closing `}` at line
280), adapting its exact precedence-comparison pattern for `ModelRef`
values instead of `&str`:

```rust
/// Resolve `default_models` across contributors: highest precedence wins
/// per role key; same-precedence disagreement on a role is a hard error.
/// Matches `resolve_env`'s per-key scalar policy.
fn resolve_default_models(
    contributors: &[CapabilityContributor],
) -> anyhow::Result<BTreeMap<String, crate::config::ModelRef>> {
    let mut roles: BTreeMap<String, (&CapabilityContributor, &crate::config::ModelRef)> =
        BTreeMap::new();

    for c in contributors {
        for (role, r#ref) in &c.capabilities.default_models {
            match roles.get(role) {
                None => {
                    roles.insert(role.clone(), (c, r#ref));
                }
                Some((prev_c, prev_ref)) => {
                    if c.precedence > prev_c.precedence {
                        roles.insert(role.clone(), (c, r#ref));
                    } else if c.precedence == prev_c.precedence && r#ref != *prev_ref {
                        anyhow::bail!(
                            "conflicting default_models role '{role}' at the same precedence: \
                             '{}' sets {:?} but '{}' sets {:?} — no scope can break \
                             the tie; resolve by giving one a higher-precedence scope",
                            prev_c.name,
                            prev_ref,
                            c.name,
                            r#ref,
                        );
                    }
                }
            }
        }
    }

    Ok(roles
        .into_iter()
        .map(|(k, (_, v))| (k, v.clone()))
        .collect())
}
```

- [ ] **Step 5: Wire both into `merge_capabilities`'s return value**

Call `resolve_default_models` near where `resolve_env`/`resolve_default_mode`
are called (near line 76/92):

```rust
    let default_models = resolve_default_models(contributors)?;
```

Add both new fields to the `Ok(Capabilities { ... })` struct literal (near
line 133-156, alongside `host`):

```rust
        model_providers,
        default_models,
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib -p llmenv model_providers_concat_across_contributors model_providers_dedup_exact_duplicates default_models_higher_precedence_wins_per_role default_models_independent_roles_both_survive default_models_same_precedence_conflict_errors`
Expected: PASS (5 tests).

Then run the full merge module suite: `cargo test --lib -p llmenv merge::`
Expected: PASS, no regressions in existing `lsp`/`mcp`/`env` tests.

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/merge/capabilities.rs
git commit -m "feat(merge): merge model_providers/default_models for #508

model_providers: concat + dedup, identical to lsp/mcp/skills — override-by-id
happens naturally at CrushAdapter render time (map-insert-by-id), not here.

default_models: per-role highest-precedence-wins, adapted directly from
resolve_env's exact pattern. Independent roles (large vs small) don't
clobber each other; same-precedence disagreement on one role is a hard
error, matching the env/default_mode scalar policy."
```

---

### Task 4: `AgentAdapter` capability probe

**Files:**
- Modify: `src/adapter/mod.rs` (add trait method after `supports_lsp` at
  line 32; extend the `registry_adapter_trait_probes` test at the bottom of
  the file)
- Modify: `src/adapter/claude_code.rs` (add impl after `supports_lsp` at
  line 106)
- Modify: `src/adapter/crush.rs` (add impl after `supports_lsp` at line 35)

**Interfaces:**
- Produces: `AgentAdapter::supports_model_providers(&self) -> bool`

- [ ] **Step 1: Write the failing test**

In `src/adapter/mod.rs`'s `mod tests` block, extend
`registry_adapter_trait_probes` (the existing test asserting
`supports_lsp()` for both adapters) with two new assertions:

```rust
        assert!(
            !a.supports_model_providers(),
            "ClaudeCodeAdapter does not support model providers"
        );
```

right after the existing `assert!(!a.supports_lsp(), ...)` line for
`ClaudeCodeAdapter`, and:

```rust
        assert!(
            c.supports_model_providers(),
            "CrushAdapter supports model providers"
        );
```

right after the existing `assert!(c.supports_lsp(), ...)` line for
`CrushAdapter`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib registry_adapter_trait_probes`
Expected: FAIL — `supports_model_providers` method doesn't exist (compile
error).

- [ ] **Step 3: Add the trait method**

In `src/adapter/mod.rs`, add after `fn supports_lsp(&self) -> bool;` (line
32):

```rust
    /// Whether this adapter supports multiple model providers and
    /// default-model selection. Claude Code does not (Anthropic-only, no
    /// provider switching).
    fn supports_model_providers(&self) -> bool;
```

In `src/adapter/claude_code.rs`, add after its `supports_lsp` impl (line
106-108):

```rust
    fn supports_model_providers(&self) -> bool {
        false
    }
```

In `src/adapter/crush.rs`, add after its `supports_lsp` impl (line 35-37):

```rust
    fn supports_model_providers(&self) -> bool {
        true
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib registry_adapter_trait_probes`
Expected: PASS.

Then run the full adapter module suite to confirm nothing else implements
`AgentAdapter` and is now missing the method: `cargo build --lib`
Expected: clean build (if any other type implements `AgentAdapter`, this
step fails with a compile error naming it — there should be none besides
the two adapters).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/adapter/mod.rs src/adapter/claude_code.rs src/adapter/crush.rs
git commit -m "feat(adapter): add supports_model_providers() capability probe

Mirrors supports_lsp() exactly. ClaudeCodeAdapter false (Anthropic-only,
no provider switching), CrushAdapter true."
```

---

### Task 5: `CrushAdapter` rendering — confirm `catwalk.Model` field names

**Files:**
- None modified — this is a research-only task that produces the exact
  field mapping Task 6 needs. Its output is a comment block, not code.

**Interfaces:**
- Produces: a confirmed mapping from `ModelSource` fields to
  `catwalk.Model`'s literal JSON tags, to paste into Task 6's render
  function as a doc comment.

- [ ] **Step 1: Locate the `catwalk.Model` type**

`catwalk` is an external Go module (`github.com/charmbracelet/catwalk`), not
vendored into the Crush repo. Locate it via the Go module cache:

```bash
find "$(go env GOMODCACHE 2>/dev/null || echo ~/go/pkg/mod)" -maxdepth 1 -iname "catwalk*" 2>/dev/null
```

If not present locally, fetch the module source directly:

```bash
go doc github.com/charmbracelet/catwalk Model 2>&1 || \
  gh repo clone charmbracelet/catwalk ~/git/reference/catwalk -- --depth 1
```

If cloned, find the type: `grep -rn "type Model struct" ~/git/reference/catwalk/`

- [ ] **Step 2: Record the exact JSON field names**

Read the struct definition and write down every JSON tag (not just the Go
field name) for: model id, display name, cost fields (input/output/cache
read/cache write), context window, max output tokens, reasoning-capability
flag. Cross-check each against the corresponding field already confirmed in
Pi's `models.md` (`id, name, cost.{input,output,cacheRead,cacheWrite},
contextWindow, maxTokens, reasoning`) and OpenCode's `provider.ts` Model
schema (`id, name, cost.{input,output,cache_read,cache_write},
limit.{context,output}, reasoning`) — the three should agree closely on
*shape*; this step confirms Crush's literal *JSON key spelling*, which
determines the exact strings Task 6's `json!({...})` macro calls must use.

- [ ] **Step 3: Hand off to Task 6**

Paste the confirmed field-name table as a comment at the top of the
`render_model_providers` function written in Task 6, Step 3, before writing
any `json!()` calls that depend on it.

---

### Task 6: `CrushAdapter` rendering

**Files:**
- Modify: `src/adapter/crush.rs` (add `render_model_providers` and
  `render_default_models` functions alongside `render_lsp` at line 369; call
  both from `materialize()` where `render_lsp`'s result is inserted into
  `doc` — search for where the `render_lsp(...)` call's output gets
  `doc.insert("lsp".into(), ...)` in `materialize()`, and mirror that
  insertion pattern for `"providers"` and `"models"` keys)
- Test: same file, `#[cfg(test)] mod tests` block (alongside
  `materialize_lsp_server_written`/`materialize_lsp_empty_omitted`)

**Interfaces:**
- Consumes: `ModelProvider`, `ModelSource`, `ModelCost`, `ModelRef` from
  Task 1; the confirmed `catwalk.Model` field-name mapping from Task 5.
- Produces: `render_model_providers(providers: &[ModelProvider]) ->
  anyhow::Result<serde_json::Value>`, `render_default_models(models:
  &BTreeMap<String, ModelRef>) -> serde_json::Value`.

- [ ] **Step 1: Write the failing tests**

Add alongside `materialize_lsp_server_written`/`materialize_lsp_empty_omitted`
in `src/adapter/crush.rs`'s test module:

```rust
#[test]
fn materialize_model_provider_written() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = Capabilities::default();
    caps.model_providers.push(crate::config::ModelProvider {
        id: "ollama".into(),
        base_url: Some("http://localhost:11434/v1".into()),
        api_type: Some("openai".into()),
        models: vec![crate::config::ModelSource {
            id: "llama3.1:8b".into(),
            context_window: Some(128000),
            ..Default::default()
        }],
        ..Default::default()
    });
    CrushAdapter
        .materialize(&manifest_with_caps(caps), tmp.path())
        .unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        doc["providers"]["ollama"]["base_url"],
        serde_json::json!("http://localhost:11434/v1"),
        "provider base_url must be written"
    );
}

#[test]
fn materialize_model_providers_empty_omitted() {
    let tmp = tempfile::tempdir().unwrap();
    CrushAdapter
        .materialize(&empty_manifest(), tmp.path())
        .unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        doc.get("providers").is_none(),
        "\"providers\" key must be absent when no model providers configured"
    );
}

#[test]
fn materialize_default_model_written() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = Capabilities::default();
    caps.default_models.insert(
        "large".into(),
        crate::config::ModelRef { provider: "anthropic".into(), model: "claude-opus-4-7".into() },
    );
    CrushAdapter
        .materialize(&manifest_with_caps(caps), tmp.path())
        .unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(CRUSH_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(doc["models"]["large"]["provider"], serde_json::json!("anthropic"));
    assert_eq!(doc["models"]["large"]["model"], serde_json::json!("claude-opus-4-7"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib materialize_model_provider_written materialize_model_providers_empty_omitted materialize_default_model_written`
Expected: FAIL — `doc["providers"]`/`doc["models"]` are absent (the
manifest's `model_providers`/`default_models` aren't rendered yet).

- [ ] **Step 3: Write the render functions**

Add alongside `render_lsp` in `src/adapter/crush.rs` (after its closing
`}` near line 400ish — check the exact end of `render_lsp` before
inserting):

```rust
// catwalk.Model field-name mapping confirmed in Task 5:
// [paste the table from Task 5 Step 2 here before implementing]
fn render_model_providers(providers: &[llmenv_config::ModelProvider]) -> anyhow::Result<serde_json::Value> {
    let mut obj = serde_json::Map::new();
    for p in providers {
        if p.disabled {
            continue;
        }
        let mut entry = serde_json::Map::new();
        entry.insert("id".into(), json!(p.id));
        if let Some(name) = &p.name {
            entry.insert("name".into(), json!(name));
        }
        if let Some(base_url) = &p.base_url {
            entry.insert("base_url".into(), json!(base_url));
        }
        if let Some(api_type) = &p.api_type {
            entry.insert("type".into(), json!(api_type));
        }
        if let Some(api_key) = &p.api_key {
            entry.insert("api_key".into(), json!(api_key));
        }
        if !p.headers.is_empty() {
            entry.insert("extra_headers".into(), json!(p.headers));
        }
        if !p.models.is_empty() {
            let models: Vec<serde_json::Value> = p.models.iter().map(render_model_source).collect();
            entry.insert("models".into(), json!(models));
        }
        obj.insert(p.id.clone(), serde_json::Value::Object(entry));
    }
    Ok(serde_json::Value::Object(obj))
}

fn render_model_source(m: &llmenv_config::ModelSource) -> serde_json::Value {
    let mut entry = serde_json::Map::new();
    entry.insert("id".into(), json!(m.id));
    if let Some(name) = &m.name {
        entry.insert("name".into(), json!(name));
    }
    if m.reasoning {
        entry.insert("reasoning".into(), json!(true));
    }
    if let Some(ctx) = m.context_window {
        entry.insert("context_window".into(), json!(ctx));
    }
    if let Some(max) = m.max_tokens {
        entry.insert("max_tokens".into(), json!(max));
    }
    if let Some(cost) = &m.cost {
        entry.insert(
            "cost".into(),
            json!({
                "input": cost.input,
                "output": cost.output,
                "cache_read": cost.cache_read,
                "cache_write": cost.cache_write,
            }),
        );
    }
    serde_json::Value::Object(entry)
}

fn render_default_models(models: &std::collections::BTreeMap<String, llmenv_config::ModelRef>) -> serde_json::Value {
    let obj: serde_json::Map<String, serde_json::Value> = models
        .iter()
        .map(|(role, r#ref)| {
            (
                role.clone(),
                json!({ "provider": r#ref.provider, "model": r#ref.model }),
            )
        })
        .collect();
    serde_json::Value::Object(obj)
}
```

Wire both into `materialize()`. Find where `render_lsp(...)`'s result gets
inserted into `doc` (search `render_lsp(` inside the `materialize` method
body) and add the analogous insertion immediately after, guarding on
emptiness the same way the existing LSP insertion does:

```rust
        let providers_value = render_model_providers(&manifest.capabilities.model_providers)?;
        if !providers_value
            .as_object()
            .is_none_or(serde_json::Map::is_empty)
        {
            doc.insert("providers".into(), providers_value);
        }
        let default_models_value = render_default_models(&manifest.capabilities.default_models);
        if !default_models_value
            .as_object()
            .is_none_or(serde_json::Map::is_empty)
        {
            doc.insert("models".into(), default_models_value);
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib materialize_model_provider_written materialize_model_providers_empty_omitted materialize_default_model_written`
Expected: PASS (3 tests).

Then run the full crush adapter suite: `cargo test --lib crush::`
Expected: PASS, no regressions.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/adapter/crush.rs
git commit -m "feat(crush): render model_providers/default_models into crush.json

model_providers -> providers map, keyed by id. Because the merged Vec is
precedence-ordered (Task 3) and this insertion is by-key, a higher-precedence
same-id provider naturally overwrites a lower one here at render time —
achieves the spec's override-by-id semantics without new merge-time logic,
identical to how render_lsp's map-insert-by-name already works.

default_models -> top-level models map, role key passed through as-is.
Unknown roles surface as Crush's own config error, not llmenv's to
pre-validate."
```

---

### Task 7: Property tests

**Files:**
- Modify: `src/adapter/crush.rs` (add proptest cases in the existing
  `proptest! { ... }` block alongside `prop_render_lsp_*`)

**Interfaces:**
- Consumes: `render_model_providers`, `render_model_source`,
  `render_default_models` from Task 6.

- [ ] **Step 1: Write the property tests**

Add inside the existing `proptest! { ... }` block in `src/adapter/crush.rs`
(alongside `prop_render_lsp_keys_match_non_disabled_servers`):

```rust
        #[test]
        fn prop_render_model_providers_keys_match_non_disabled(
            ids in prop::collection::vec("[a-z][a-z0-9-]{0,15}", 0..6),
            disabled_flags in prop::collection::vec(proptest::bool::ANY, 0..6),
        ) {
            let providers: Vec<llmenv_config::ModelProvider> = ids
                .iter()
                .zip(disabled_flags.iter())
                .map(|(id, &d)| llmenv_config::ModelProvider {
                    id: id.clone(),
                    disabled: d,
                    ..Default::default()
                })
                .collect();
            let expected: std::collections::BTreeSet<String> = providers
                .iter()
                .filter(|p| !p.disabled)
                .map(|p| p.id.clone())
                .collect();
            let result = super::render_model_providers(&providers).unwrap();
            let got: std::collections::BTreeSet<String> = result
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            prop_assert_eq!(got, expected);
        }

        #[test]
        fn prop_render_model_providers_no_panic(
            id in ".*",
            base_url in prop::option::of(".*"),
            api_key in prop::option::of(".*"),
        ) {
            let provider = llmenv_config::ModelProvider {
                id,
                base_url,
                api_key,
                ..Default::default()
            };
            let _ = super::render_model_providers(std::slice::from_ref(&provider));
        }

        #[test]
        fn prop_render_default_models_no_panic(
            role in ".*",
            provider in ".*",
            model in ".*",
        ) {
            let mut map = std::collections::BTreeMap::new();
            map.insert(role, llmenv_config::ModelRef { provider, model });
            let _ = super::render_default_models(&map);
        }
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test --lib prop_render_model_providers_keys_match_non_disabled prop_render_model_providers_no_panic prop_render_default_models_no_panic`
Expected: PASS (3 tests, each running proptest's default case count).

- [ ] **Step 3: Commit**

```bash
cargo fmt
git add src/adapter/crush.rs
git commit -m "test(crush): property tests for model_providers/default_models render

Mirrors the existing prop_render_lsp_* pattern: key-set invariant +
no-panic on arbitrary input."
```

---

### Task 8: Docs + changelog

**Files:**
- Modify: `CHANGELOG.md` (add an `[Unreleased]` entry)
- Modify: any user-facing config reference docs that enumerate
  `Capabilities` fields (check `docs/` for an existing `mcp`/`lsp`
  reference table and add `model_providers`/`default_models` rows alongside
  them — search `docs/` for where `lsp:` is documented as a config key)

**Interfaces:**
- None — documentation only.

- [ ] **Step 1: Find the existing LSP documentation**

```bash
grep -rln "lsp:" docs/ README.md 2>/dev/null | grep -v superpowers
```

- [ ] **Step 2: Add matching sections for `model_providers`/`default_models`**

Following whatever format the found file(s) use for `lsp:`/`mcp:` (field
table, YAML example, or prose — match the existing style exactly rather than
introducing a new format), document:
- `model_providers`: full field list from Task 1, with a minimal example
  (the Ollama example from Task 1's round-trip test works as-is).
- `default_models`: the role-map shape, with an example showing both
  `large` and `small`.

- [ ] **Step 3: Invoke the `keepachangelog` skill**

Run the `keepachangelog` skill to add an `[Unreleased]` entry for this
feature (new `model_providers`/`default_models` capability, `CrushAdapter`
rendering support). Follow the skill's own format rules rather than
hand-writing the entry here.

- [ ] **Step 4: Commit**

```bash
git add CHANGELOG.md docs/
git commit -m "docs: document model_providers/default_models config for #508"
```

## Self-Review

**Spec coverage:**
- Schema (ModelProvider/ModelSource/ModelCost/ModelRef, Capabilities fields) → Task 1. ✓
- Merge & validation rules → Tasks 2, 3. ✓
- Adapter rendering (capability probe, CrushAdapter render, ClaudeCodeAdapter no-op) → Tasks 4, 5, 6. ✓
- Testing strategy (validation, merge, render, property, no-op) → Tasks 2, 3, 6, 7. The spec's "ClaudeCodeAdapter no-op test: assert materialize output is byte-identical" is not yet its own task — **gap found, added below.**
- Docs → Task 8 (not in spec explicitly, but standard project practice per AGENTS.md).

**Gap fix:** Task 4 Step 4 already runs `cargo build --lib` to confirm no
other `AgentAdapter` impl is missing the method, but no test asserts
`ClaudeCodeAdapter::materialize()` output is unaffected by
`model_providers`/`default_models` presence. Adding as Task 6, Step 1
(folded into the existing test-writing step rather than a new task, since
it shares the same file and test-running cycle):

```rust
#[test]
fn claude_code_materialize_ignores_model_providers() {
    // ClaudeCodeAdapter must never read capabilities.model_providers or
    // .default_models — verifies the true no-op by comparing output with
    // and without them present.
    let tmp_without = tempfile::tempdir().unwrap();
    crate::adapter::claude_code::ClaudeCodeAdapter
        .materialize(&crate::merge::MergedManifest::default(), tmp_without.path())
        .unwrap();

    let mut caps = crate::config::Capabilities::default();
    caps.model_providers.push(crate::config::ModelProvider {
        id: "ollama".into(),
        ..Default::default()
    });
    caps.default_models.insert(
        "large".into(),
        crate::config::ModelRef { provider: "anthropic".into(), model: "claude-opus-4-7".into() },
    );
    let manifest_with = crate::merge::MergedManifest { capabilities: caps, ..Default::default() };
    let tmp_with = tempfile::tempdir().unwrap();
    crate::adapter::claude_code::ClaudeCodeAdapter
        .materialize(&manifest_with, tmp_with.path())
        .unwrap();

    // Compare the rendered settings.json (or whichever file ClaudeCodeAdapter
    // writes) byte-for-byte between the two runs.
    let settings_path = "settings.json"; // confirm exact filename against
                                          // CLAUDE_JSON_FILE / settings file
                                          // constants in claude_code.rs
    let without = std::fs::read_to_string(tmp_without.path().join(settings_path))
        .unwrap_or_default();
    let with = std::fs::read_to_string(tmp_with.path().join(settings_path))
        .unwrap_or_default();
    assert_eq!(without, with, "ClaudeCodeAdapter output must be unaffected by model_providers/default_models");
}
```

This test belongs in `src/adapter/claude_code.rs`'s test module (not
`crush.rs`), added in **Task 4, Step 1** (write it alongside the trait-probe
test, since both establish the ClaudeCodeAdapter no-op contract) rather than
as a separate task — confirm the exact settings filename constant
(`CLAUDE_JSON_FILE`-equivalent for settings.json, distinct from
`.claude.json`) against `src/adapter/claude_code.rs`'s existing constants
before running.

**Placeholder scan:** No "TBD"/"TODO" strings. Task 5 is intentionally a
research task whose deliverable is a confirmed fact (field-name table), not
code — this is not a placeholder, it's a real dependency Task 6 needs before
it can write correct `json!()` calls, called out explicitly rather than
guessed.

**Type consistency:** `ModelProvider`/`ModelSource`/`ModelCost`/`ModelRef`
field names and types are identical across Tasks 1, 2, 3, 6, 7 (spot-checked
`context_window: Option<u32>`, `cost: Option<ModelCost>`, `default_models:
BTreeMap<String, ModelRef>` — all consistent with Task 1's definitions).
