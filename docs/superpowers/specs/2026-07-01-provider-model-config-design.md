# Provider/model config as a first-class, engine-agnostic concept

Tracked by: #508

## Background

`CrushAdapter` (#501/#506) left provider/model selection (Crush's `providers`/
`models` blocks) entirely inside the `native.crush` escape hatch. That was the
right call for shipping the Crush adapter — provider/model config wasn't
needed to reach parity, and bolting a neutral schema onto Crush-specific work
risked getting the abstraction wrong. This spec designs that neutral schema,
informed by the real config shapes of three engines: Crush, Pi
(`earendil-works/pi`), and OpenCode.

### Real-world schema survey

**Crush** (`internal/config/config.go`, Go):

- `ProviderConfig{ID, Name, BaseURL, Type, APIKey, OAuthToken, Disable,
  SystemPromptPrefix, ExtraHeaders, ExtraBody, ProviderOptions, ExtraParams,
  FlatRate, AutoDiscoverModels, Models []catwalk.Model}`
- `SelectedModel{Model, Provider, ReasoningEffort, Think, MaxTokens,
  Temperature, TopP, TopK, FrequencyPenalty, PresencePenalty,
  ProviderOptions}`
- Top-level: `Config.Models map[SelectedModelType]SelectedModel` — comment:
  *"We currently only support large/small as values here."*
- `ExtraHeaders` values "run through shell expansion at config-load time."
  Crush has its own `internal/config/resolve.go` (`NewShellVariableResolver`,
  `ResolvedEnv`, `ResolvedArgs`) that resolves `$VAR`-style references at
  Crush's own runtime — confirmed present, not assumed.

**Pi** (`packages/coding-agent/docs/{providers,models}.md`, TS):

- Provider (`models.json`): `{baseUrl, api, apiKey, headers, authHeader,
  models: Model[], modelOverrides}`. `api` is one of `openai-completions`,
  `openai-responses`, `anthropic-messages`, `google-generative-ai`.
- Model: `{id, name, api, reasoning, thinkingLevelMap, input: string[],
  contextWindow, maxTokens, cost: {input,output,cacheRead,cacheWrite},
  compat: {...}}`. Only `id` is required for local models — everything else
  defaults.
- Credential (`apiKey`/`headers`) value resolution: `"!command"` (shell,
  cached per-process), `"$VAR"`/`"${VAR}"` (env interpolation), `"$$"`/`"$!"`
  escapes, else literal. Resolved at **Pi's own runtime**, per-request — never
  written to disk as a separate materialized artifact.

**OpenCode** (`packages/core/src/v1/config/provider.ts`, TS/Effect schema):

- Provider (`Info`): `{api, name, env: string[], id, npm, whitelist, blacklist,
  options: {apiKey, baseURL, enterpriseUrl, setCacheKey, timeout,
  headerTimeout, chunkTimeout, ...arbitrary}, models: Record<id, Model>}`.
  `env` is a list of env var *names* to check in order, not a value.
- Model: `{id, name, family, release_date, attachment, reasoning, temperature,
  tool_call, interleaved, cost: {input,output,cache_read,cache_write,
  context_over_200k:{...}}, limit: {context,input,output},
  modalities: {input:[],output:[]}, experimental, status, provider:{npm,api},
  options, headers, variants}`.
- Also has a separate, explicitly-decoupled `experimental.policies` concept
  (`provider.use` action, allow/deny by provider ID) — provider *configuration*
  and provider *policy* are deliberately separate concerns in OpenCode's own
  design. Out of scope here (see Non-goals).

All three converge on: a **provider** (id, name, base_url, credential,
wire-format, headers, model list) and a **model** (id, name, cost broken into
input/output/cache-read/cache-write, context window, max output tokens,
reasoning capability, input modalities). All three also have a large
per-engine "quirks" escape hatch (Pi's `compat`, OpenCode's
`options`/`variants`, Crush's `provider_options`/`extra_body`/`extra_params`)
— this maps directly onto llmenv's existing `native_*` pattern, so quirks stay
there rather than being neutrally modeled.

## Goals

- Neutral schema for custom/self-hosted model provider endpoints (Ollama,
  vLLM, LM Studio, proxies) that renders correctly into any adapter that
  supports the concept.
- Neutral schema for per-scope default model selection, role-aware
  (`large`/`small`/etc, matching Crush's real `SelectedModelType`), selected
  via the existing scope/tag-precedence system.
- Zero secrets-at-rest regression: credential values pass through
  unresolved. Each target engine's own runtime resolves `$VAR`/`!command`
  syntax — confirmed both Crush (`resolve.go`) and Pi already do this
  natively.
- Adapters without this concept (Claude Code — Anthropic-only, no
  multi-provider selection) silently no-op, consistent with the LSP
  precedent (`ClaudeCodeAdapter::supports_lsp() == false`).

## Non-goals

- Provider allow/deny **policy** (OpenCode's `experimental.policies`
  concept). OpenCode itself treats this as a separate concern from provider
  *configuration* — a future issue if ever needed, not bundled here.
- A model metadata *catalog* or auto-discovery. llmenv only renders what a
  bundle/config declares; it never fetches or caches upstream provider/model
  lists.
- llmenv-side credential resolution (`!command` execution, `$VAR`
  expansion). Rejected: resolving at llmenv's materialize time would write
  the **resolved plaintext secret** into `~/.cache/llmenv/<hash>/<engine>/`,
  a secrets-at-rest exposure that doesn't exist today. Passthrough relies on
  the target engine's own resolver, which both surveyed real-world targets
  (Crush, Pi) already have.
- Retrofitting existing `native.crush` provider/model configs. This is
  additive — existing native-passthrough configs keep working untouched.

## Schema

Added to `crates/llmenv-config/src/schema.rs`:

```rust
/// A custom/self-hosted model provider endpoint (Ollama, vLLM, LM Studio, a
/// proxy, or an override of a built-in provider). Selected by tag
/// intersection like `mcp`/`lsp`/`skills`. Engines without a multi-provider
/// concept (Claude Code) silently skip these — declaring one in a shared
/// bundle is legitimate; it is simply a no-op for such adapters.
pub struct ModelProvider {
    pub id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub when: Vec<String>,
    pub base_url: Option<String>,
    /// Wire format, e.g. "openai", "anthropic", "google". Open string, not
    /// an enum — new wire formats appear faster than llmenv releases.
    pub api_type: Option<String>,
    /// Passthrough credential string — may be a literal, or a $VAR/!command
    /// reference the *target engine* resolves at its own runtime. llmenv
    /// never interprets this value (see Non-goals).
    pub api_key: Option<String>,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub models: Vec<ModelSource>,
}

/// One model exposed by a `ModelProvider`. All fields but `id` are optional
/// — mirrors Pi's "only `id` is required for local models" convention.
pub struct ModelSource {
    pub id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub reasoning: bool,
    pub context_window: Option<u32>,
    pub max_tokens: Option<u32>,
    pub cost: Option<ModelCost>,
    #[serde(default)]
    pub modalities: Vec<String>,
}

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
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}
```

`Capabilities` gains:

```rust
/// Custom/self-hosted model provider endpoints, tag-intersected like
/// `mcp`/`lsp`.
pub model_providers: Vec<ModelProvider>,
/// Default model selection, keyed by an open-string role ("large", "small",
/// etc — matches Crush's `SelectedModelType` without hardcoding to it).
/// Merged by override like `Permissions::default_mode`, not tag-intersected
/// like a list — there is only one default per role.
pub default_models: std::collections::BTreeMap<String, ModelRef>,
```

## Merge & Validation Rules

- `model_providers` is tag-intersected: only providers whose `when:` tags
  intersect the active scope tag set are selected (identical selection model
  to `McpServer`/`LspServer`).
- Duplicate `ModelProvider.id` **within one bundle** is a hard validation
  error (`ModelProviderDuplicateId`, mirrors `ValidateError::DuplicateMcpName`).
- **Across** scope precedence, a higher-precedence scope's `ModelProvider`
  fully **replaces** a lower-precedence one with the same `id` — not merged
  field-by-field. Simplest, most predictable rule; consistent with how
  `native.*` overrides already behave.
- Duplicate `ModelSource.id` within the same provider's `models` list is a
  validation error. Empty `id` on either `ModelProvider` or `ModelSource` is
  a validation error (mirrors `SkillEmptyName`/`SkillEmptyPath`).
- `api_key`/`headers`: no format validation — pure passthrough strings, same
  as `McpServer.headers` today.
- `default_models` merges by override: a higher-precedence scope's entry for
  a given role key replaces the lower-precedence entry for *that role only*
  — setting `"large"` in one scope does not clobber `"small"` set in
  another. `ModelRef.provider`/`.model` must be non-empty strings; llmenv
  does not check that `provider` resolves to a configured `ModelProvider.id`
  (see Schema doc comment).
- No path-traversal/filesystem concerns apply to this schema — every field is
  a passthrough string rendered directly into JSON, never joined to a
  filesystem path (unlike the skills/plugins work).

## Adapter Rendering

New capability probe on `AgentAdapter` (`src/adapter/mod.rs`), mirroring
`supports_lsp()`:

```rust
/// Whether this adapter supports multiple model providers and default-model
/// selection. Claude Code does not (Anthropic-only, no provider switching).
fn supports_model_providers(&self) -> bool;
```

- `ClaudeCodeAdapter::supports_model_providers()` → `false`.
  `ClaudeCodeAdapter::materialize()` never reads
  `capabilities.model_providers`/`.default_models` — a true no-op, matching
  how it already ignores `capabilities.lsp` today.
- `CrushAdapter::supports_model_providers()` → `true`.
  `CrushAdapter::materialize()` renders:
  - `model_providers` → `crush.json`'s `providers` map, keyed by
    `ModelProvider.id`: `{name, base_url, type: api_type, api_key,
    extra_headers: headers, models: [...]}`. Each `ModelSource` renders into
    Crush's `catwalk.Model` shape (`id, name, cost{...}, context_window,
    max_tokens, reasoning`).
  - `default_models` → `crush.json`'s top-level `models` map, role key
    passed through as-is (`{"large": {...}, "small": {...}}`). llmenv does
    not validate the role is `large`/`small` — an unknown role surfaces as
    Crush's own config error, not something llmenv pre-checks.

**Implementation note:** `catwalk.Model`'s exact JSON field names live in an
external Go module (`github.com/charmbracelet/catwalk`), not vendored into
Crush's own repo, so they weren't directly confirmed during this design pass
(the *shape* was cross-checked against Pi's and OpenCode's near-identical
model schemas instead). The first implementation task for the `CrushAdapter`
render path is `go doc github.com/charmbracelet/catwalk Model` (or equivalent
source inspection) to confirm literal JSON tags before wiring the render
function.

## Testing Strategy

- **Validation tests**: duplicate `ModelProvider.id` within a bundle
  rejected; duplicate `ModelSource.id` within a provider rejected; empty
  `id` fields rejected on both types.
- **Merge tests**: higher-precedence scope's `ModelProvider` fully replaces
  a same-`id` lower-precedence one; `default_models` role entries override
  independently per role key.
- **CrushAdapter render tests**: `model_providers` → `providers` map shape,
  `default_models` → `models` map shape, round-tripped through
  `serde_json` and asserted against expected keys/values (same style as
  existing `materialize_lsp_server_written`/`materialize_lsp_empty_omitted`).
- **Property tests**: arbitrary `ModelProvider`/`ModelSource` fuzzing for
  no-panic on render, following the `prop_render_lsp_*` pattern already in
  `crush.rs`.
- **ClaudeCodeAdapter no-op test**: assert materialize output is
  byte-identical whether or not `model_providers`/`default_models` are
  present in the manifest.
