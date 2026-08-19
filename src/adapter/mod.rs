pub mod claude_code;
pub mod codex;
pub mod crush;
pub(crate) mod llmenv_skill;
pub(crate) mod native_keys;
pub mod opencode;
pub(crate) mod output_styles;
pub(crate) mod skills;
pub(crate) mod tools;

use std::path::{Path, PathBuf};

use crate::mcp::resolve::MEMORY_MCP_NAME;
use crate::merge::MergedManifest;

/// Which engine-neutral lifecycle events get a `hook-run` registration for this
/// manifest, and why.
///
/// Engine-neutral on purpose: it reads only the manifest, so every adapter that
/// wires lifecycle hooks gates them on the same answer, and `llmenv doctor`
/// reports what those adapters actually write (#741) rather than re-deriving it.
/// Codex wires the same set as Claude Code (#1435), so a gate that lived in one
/// adapter would have had to be duplicated into the other and kept in step by
/// hand.
///
/// `turn_start`'s gate is read straight from here by the generators.
/// `session_start`/`session_end` are registered unconditionally, and `stop` is
/// still derived independently in each adapter's session-log/task-tracker
/// branch — folding that one in would mean restructuring how the whole
/// session-log event set is emitted.
///
/// Each adapter's `lifecycle_registrations_match_*` test is what keeps those
/// independent gates honest: it renders config for each combination and asserts
/// this function agrees. Session logging is on in a default manifest, so
/// fixtures that leave it that way can't tell the two halves of `stop`'s
/// condition apart — the cases that disable it are the ones that make the
/// assertion capable of failing.
pub(crate) fn lifecycle_hook_registrations(
    manifest: &MergedManifest,
) -> Vec<(&'static str, bool, &'static str)> {
    let icm_active = manifest.mcps.iter().any(|m| m.name == MEMORY_MCP_NAME);
    let session_log = manifest.session_log.any_sink_enabled();
    let task_tracker = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref())
        .is_some_and(|t| t.enabled);
    // #317: the self-critique layer runs on Stop, so enabling it has to be a
    // reason to register the event. Without this the layer is a phantom —
    // config accepts it, `doctor` would report it, and nothing ever fires.
    let slippage_critique = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.slippage.as_ref())
        .is_some_and(|s| s.enabled && s.self_critique);
    vec![
        ("session_start", true, "always registered"),
        ("session_end", true, "always registered"),
        (
            "turn_start",
            icm_active,
            "needs a memory backend (features.memory)",
        ),
        (
            "stop",
            session_log || task_tracker || slippage_critique,
            "needs session logging, features.task_tracker, or slippage self_critique",
        ),
    ]
}

/// True when this manifest wants the `#317` rules digest reinjected on every
/// prompt. Separate from `lifecycle_hook_registrations` because it doesn't map
/// to one neutral event of its own — it rides the adapter's
/// `user_prompt_submit` registration, which session logging and per-turn memory
/// recall can already have claimed.
fn slippage_rule_reinjection_enabled(manifest: &MergedManifest) -> bool {
    manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.slippage.as_ref())
        .is_some_and(|s| s.enabled && s.rule_reinjection)
}

/// True when the session-log capture hooks (#382) will register an unmatched —
/// every-tool — `hook-run pre_tool_use` for this manifest.
///
/// Adapters must skip their matcher-scoped `pre_tool_use` registrations when
/// this is true. `hook-run pre_tool_use` dispatches on `tool_name` itself, so
/// the unmatched group already serves every matcher-scoped consumer: read-once
/// dedup (#318), the slippage guards (#317), the `index_repository` name guard
/// (#1331), the task-tool redirect (#985). Registering both makes a matching
/// tool call reach `hook-run pre_tool_use` twice, which trips `repeat_detect`'s
/// loop-breaker at half its configured threshold and makes `read_once` mistake
/// a file's own first read for a repeat (#1442).
///
/// Lives here rather than in either adapter because all three engines merge the
/// same two sources of `pre_tool_use` registrations, and a gate duplicated per
/// adapter is what let the bug reach a second one in the first place.
pub(crate) fn unmatched_pre_tool_use_registered(manifest: &MergedManifest) -> bool {
    manifest.session_log.any_sink_enabled()
}

/// True when `lifecycle_hook_registrations` says `event` gets registered.
pub(crate) fn lifecycle_event_registered(manifest: &MergedManifest, event: &str) -> bool {
    lifecycle_hook_registrations(manifest)
        .iter()
        .any(|(name, registered, _)| *name == event && *registered)
}

/// Convert a YAML native fragment to JSON and deep-merge it into `dst`.
///
/// Used by adapters to overlay engine-specific catch-all config keys
/// (e.g. `native.crush`, `native_mcp.opencode`) on top of the structured
/// rendering. `fragment` is `Option` so callers can pass a `.get()` result
/// directly without an extra guard.
///
/// # Errors
/// Returns an error if `fragment` is not a mapping, or if it cannot be
/// serialized to JSON (should not happen with a valid `serde_yaml::Value`).
pub(crate) fn overlay_native_json(
    dst: &mut serde_json::Value,
    fragment: Option<&serde_yaml::Value>,
    label: &str,
) -> anyhow::Result<()> {
    let Some(frag) = fragment else {
        return Ok(());
    };
    // A non-mapping fragment doesn't merge — `merge_json` replaces `dst` with it
    // wholesale, which silently discards the whole rendered block (and callers
    // then drop the key because it's no longer an object). Almost always a
    // YAML indentation slip, so fail fast instead.
    anyhow::ensure!(
        frag.is_mapping(),
        "`{label}` must be a mapping of keys to merge, got {}. \
         A non-mapping value would replace the rendered block instead of merging \
         into it — check the indentation of the entries under `{label}`.",
        yaml_value_kind_name(frag)
    );
    let as_json = serde_json::to_value(frag)
        .map_err(|e| anyhow::anyhow!("converting {label} fragment to JSON: {e}"))?;
    llmenv_util::merge_json(dst, as_json);
    Ok(())
}

/// Recursively remove null-valued keys from every JSON object in `value`.
///
/// #1264: a native `null` on a key the renderer already emitted lands as an
/// explicit JSON `null` rather than deleting the key — `merge_json`'s
/// shared-key overwrite arm deliberately does not null-strip (an explicit
/// null is intentional data, not an `Option::None` artifact), so the caller
/// must strip at the write boundary instead. Every adapter write path that
/// overlays a `native*` catch-all fragment onto already-rendered output calls
/// this as its last step, after the final overlay, so it catches every layer.
///
/// Also makes objects that differ only by null vs absent key compare equal
/// under [`PartialEq`], which `merge_json`'s array dedup relies on.
pub(crate) fn strip_json_nulls(value: &mut serde_json::Value) {
    strip_json_nulls_depth(value, 0);
}

/// Depth-limited implementation of [`strip_json_nulls`].
///
/// The depth guard prevents stack overflow on pathological JSON nesting
/// (config depth is normally <10 levels). The serde_json parser has its own
/// recursion limit, but that guards _parsing_ — the value tree can be
/// arbitrarily nested after deserialization.
///
/// # Fail-open, deliberately (#1274)
///
/// Bailing past depth 64 leaves the remaining subtree's nulls unstripped,
/// which can now reach persistent files this function's callers write to
/// directly (`.claude.json`, `settings.json`, `crush.json`, `opencode.json`,
/// `config.toml`) — not just rebuildable cache output. Fail-closed (propagating an error
/// instead of bailing) was considered and rejected — not because every
/// caller would need reworking (four of this function's six call sites
/// already return `anyhow::Result` and use `?`: `merge_mcp_into_claude_json`,
/// `generate_settings_json`, opencode's and crush's `materialize`), but
/// because the other two (`dedup_hooks_doc`'s call inside a `.map()`/
/// `.or_else()` merge chain, and `normalized_hook` called from a
/// `Vec::retain` predicate, both in `claude_code.rs`) can't return early with
/// `?` without restructuring those combinators. Making four call sites
/// fail-closed while the other two stay fail-open would be an inconsistent
/// half-fix for an input that must already survive a full `serde_yaml`/
/// `serde_json` parse to reach 64 levels of nesting, and the null surviving
/// past the guard is at worst equivalent to the key being absent — the
/// deny-never-weakened invariant is enforced by
/// `reject_modeled_keys_in_catch_all`, not by null-stripping. Realistic worst
/// case stays what #1274 found it to be: a cosmetically wrong config, never
/// a panic or crash.
///
/// One caller differs (#233): the Codex adapter serializes to TOML, which has
/// no null representation at all — `toml` returns `UnsupportedType` at every
/// depth tested rather than omitting the value. So a null surviving past the
/// guard makes that one render *fail* instead of being cosmetically wrong. Still
/// not a panic, and still unreachable without 64 levels of nesting, but it is a
/// louder failure than the JSON callers'.
fn strip_json_nulls_depth(value: &mut serde_json::Value, depth: usize) {
    if depth > 64 {
        tracing::error!(
            "strip_json_nulls: exceeded max depth (64) — bailing on the remaining subtree, \
             which may leave stray null values in a persistent config file"
        );
        return;
    }
    match value {
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                strip_json_nulls_depth(item, depth + 1);
            }
        }
        serde_json::Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for v in map.values_mut() {
                strip_json_nulls_depth(v, depth + 1);
            }
        }
        _ => {}
    }
}

/// Human-readable YAML value kind, for error messages when a config value has
/// the wrong shape (e.g. a native fragment that isn't a mapping).
pub(crate) fn yaml_value_kind_name(value: &serde_yaml::Value) -> &'static str {
    match value {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "a bool",
        serde_yaml::Value::Number(_) => "a number",
        serde_yaml::Value::String(_) => "a string",
        serde_yaml::Value::Sequence(_) => "a sequence",
        serde_yaml::Value::Mapping(_) => "a mapping",
        serde_yaml::Value::Tagged(_) => "a tagged value",
    }
}

/// Reject a native catch-all fragment that carries keys already fully modeled
/// by the adapter's structured rendering paths.
///
/// Each adapter defines its own `MODELED_KEYS` constant. Overlaying these keys
/// last would silently clobber the security-rendered output (permissions, hooks)
/// or the structured rendering (mcp, lsp).
///
/// # Errors
/// Returns an error if `fragment` contains any key in `modeled_keys`, with a
/// message naming the key and where to put it instead.
fn reject_modeled_native_keys(
    fragment: &serde_yaml::Value,
    modeled_keys: &[&str],
    engine: &str,
) -> anyhow::Result<()> {
    let Some(map) = fragment.as_mapping() else {
        return Ok(());
    };
    for key in modeled_keys {
        if map.contains_key(serde_yaml::Value::String((*key).into())) {
            anyhow::bail!(
                "top-level `native.{engine}` carries the modeled-feature key `{key}`, \
                 which would silently clobber the rendered `{key}`. {}",
                modeled_key_redirect(key, engine)
            );
        }
    }
    Ok(())
}

/// Where a rejected modeled key actually belongs. Not every modeled key has a
/// `native_*` escape hatch, so this names the real destination per key rather
/// than pointing at a `native_{key}.{engine}` field that may not exist (#1008).
fn modeled_key_redirect(key: &str, engine: &str) -> String {
    let hatch = match key {
        "permission" | "permissions" => "native_permissions",
        "hooks" => "native_hooks",
        "mcp" => "native_mcp",
        "provider" | "providers" => "native_model_providers",
        "model" | "models" | "small_model" => return no_hatch(key, engine, "default_models"),
        // Codex spells the same concept `model_instructions_file`; both point
        // at the merged rules pipeline, which has no per-engine escape hatch.
        "instructions" | "model_instructions_file" => return no_hatch(key, engine, "rules"),
        _ => return no_hatch(key, engine, key),
    };
    format!("Use `{hatch}.{engine}` instead, which merges in the safe direction.")
}

fn no_hatch(key: &str, engine: &str, neutral: &str) -> String {
    format!(
        "There is no `native_*.{engine}` escape hatch for `{key}` — declare it through \
         `capabilities.{neutral}` instead."
    )
}

/// Per-agent rules for translating a [`MergedManifest`] into an on-disk layout
/// and a set of environment variables that point the agent at it.
///
/// Adapters are stateless value types; instantiate with `default()` or a unit
/// constructor at the call site.
pub trait AgentAdapter {
    /// Stable identifier used as the cache subdirectory and in diagnostics.
    fn name(&self) -> &'static str;

    /// Whether this adapter is the one running in the current process, judged
    /// by its own environment signal (an env var the engine sets, or a binary
    /// on `PATH`). Used by [`active_adapter`] to pick which adapter answers
    /// for a subprocess (hook-run, throttle) that isn't told its engine
    /// identity directly.
    ///
    /// Each adapter owns its own signal here instead of `active_adapter`
    /// matching on `name()` — a registry-derived dispatch means a newly
    /// registered adapter is detected automatically instead of silently
    /// falling through a `_ => false` arm nobody remembered to extend (#1115).
    fn is_active(&self) -> bool;

    /// Binary name that must be present on `PATH` for this adapter to be
    /// active. Used by [`crate::paths::binary_on_path`] to PATH-gate the adapter during
    /// export orchestration — if the binary is absent, the adapter is skipped
    /// entirely so a machine without the tool installed sees zero change.
    fn binary_name(&self) -> &'static str;

    /// Whether this adapter supports Claude Code–style plugins (skills,
    /// marketplaces, `installed_plugins.json`). Callers that write plugin
    /// artefacts consult this before invoking plugin rendering paths.
    fn supports_plugins(&self) -> bool;

    /// Whether this adapter supports LSP integration. Reserved for adapters
    /// that wire in language-server configuration natively; Claude Code does
    /// not (it has its own built-in language tooling).
    fn supports_lsp(&self) -> bool;

    /// Whether this adapter supports multiple model providers and
    /// default-model selection. Claude Code does not (Anthropic-only, no
    /// provider switching).
    fn supports_model_providers(&self) -> bool;

    /// Whether this adapter has a native output-style concept — a file
    /// appended to the system prompt to change tone/role/format, selected
    /// via a single settings key (#1130). Only Claude Code does. Adapters
    /// that don't render the same declared content as a generated skill
    /// instead (`adapter::output_styles::write_output_style_as_skill`).
    fn supports_output_styles(&self) -> bool;

    /// The `native_*` config maps this adapter actually reads, named by their
    /// config field (see the `NATIVE_*` constants in [`native_keys`]).
    ///
    /// A per-engine key naming this adapter in a map that is *absent* from this
    /// list is dead config: it deserializes, merges, and hashes, then no code
    /// ever looks it up. [`native_keys::dead_native_engine_keys`] reports those.
    ///
    /// This is deliberately a declaration of *consumption*, not of capability.
    /// The two diverge — opencode reports `supports_plugins() == true` and a
    /// non-empty [`AgentAdapter::supported_hook_events`], yet reads neither
    /// `native_plugins` nor `native_hooks` (it renders hooks from the neutral
    /// `capabilities.hooks` through its JS shim instead). Gating on the
    /// capability predicates therefore blesses keys nothing reads (#1032).
    ///
    /// # Extending
    /// Adding a `manifest.capabilities.native_x.get("<id>")` call to an adapter
    /// means adding `NATIVE_X` here in the same edit.
    fn native_maps(&self) -> &'static [&'static str];

    /// The set of native hook-event names this adapter emits. Callers use this
    /// to guard event registration so events an adapter never fires are not
    /// written into its settings file.
    fn supported_hook_events(&self) -> &'static [&'static str];

    /// Environment variables the shell hook should `export` so the agent
    /// discovers `cache_dir` as its config root and `state_dir` for durable state.
    ///
    /// Implementations may create adapter-specific subdirectories under
    /// `state_dir` as a side effect (e.g. so a directory referenced by an emitted
    /// env var exists on disk before the agent launches) — this is the only place
    /// that knows both the exact path and that it must exist.
    ///
    /// # Arguments
    /// * `cache_dir` — hashed config directory (garbage-collected on content change)
    /// * `state_dir` — stable state directory (persists across config changes)
    ///
    /// # Errors
    /// Returns an error if either path is not valid UTF-8 — env vars cannot
    /// carry arbitrary bytes on all platforms, so callers that surface a
    /// non-UTF-8 path should fail loudly rather than emit a lossy path the agent
    /// will silently mis-parse. Also returns an error if creating a required
    /// subdirectory fails.
    fn env_vars(&self, cache_dir: &Path, state_dir: &Path)
    -> anyhow::Result<Vec<(String, String)>>;

    /// Write the manifest into `out` in the agent-native layout, returning the
    /// set of paths the adapter wrote, each relative to `out`. The returned set
    /// is llmenv's *owned* set for `out`: callers union it with the generic
    /// copied files to build the [`crate::materialize::manifest::CacheManifest`]
    /// and to reconcile ghost files on a version-mode re-render (#196).
    ///
    /// Implementations must be idempotent — callers re-run after cache GC and
    /// re-render in place in version mode. Files an implementation merges over
    /// (rather than overwrites) to preserve foreign in-session state — e.g.
    /// `settings.json`, which a plugin may self-register hooks into (#175) — are
    /// still reported as owned, because llmenv authored their llmenv-controlled
    /// keys.
    ///
    /// # Errors
    /// Returns any I/O error encountered while creating directories or
    /// copying files.
    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<Vec<PathBuf>>;

    /// Format injected hook context in the engine's native hook-output shape so
    /// the agent runtime adds it to the model's context. Empty input returns an
    /// empty string, which suppresses any output.
    ///
    /// # Arguments
    /// * `hook_event_name` — the event name from the hook payload (e.g.
    ///   `"SessionStart"`), echoed back as `hookEventName` inside
    ///   `hookSpecificOutput` for runtimes that validate it.
    /// * `text` — the injected memory context, placed as `additionalContext`
    ///   inside `hookSpecificOutput`.
    fn emit_hook_context(&self, hook_event_name: &str, text: &str) -> String;

    /// JSON Schema describing this adapter's materialized output, derived
    /// from the same typed structs that build it. `None` (the default)
    /// means the adapter has no typed output structs yet and emits no
    /// schema sidecar.
    fn config_schema(&self) -> Option<serde_json::Value> {
        None
    }
}

/// Detect which adapter is running in the current process by checking each
/// registered adapter's environment signal. Falls back to Claude Code when
/// no signal is found (backward-compatible default).
///
/// Used by hook-run and throttle subcommands that are invoked as subprocesses
/// by the LLM CLI and don't receive the adapter identity through stdin.
#[must_use]
pub(crate) fn active_adapter() -> Box<dyn AgentAdapter> {
    active_adapter_from(registered_adapters())
}

/// Pick the first adapter whose [`AgentAdapter::is_active`] signal fires,
/// falling back to Claude Code when none do.
///
/// Split out from [`active_adapter`] so the dispatch order and fallback
/// behavior are unit-testable with fake adapters instead of exercising real
/// `is_active()` implementations, which read process-global env vars
/// (`CLAUDE_CONFIG_DIR`, `CRUSH_GLOBAL_CONFIG`, `OPENCODE_CONFIG_DIR`) shared
/// with other tests in the same binary — mutating them via `set_var`/
/// `remove_var` is both `unsafe` under Rust 2024 (denied workspace-wide) and
/// a cross-test race under `cargo test`'s parallel threads (#1305). Testing
/// this selection algorithm against fakes sidesteps both problems; the real
/// per-adapter env-var checks are exercised end-to-end by every hook-run
/// invocation from an actual engine.
fn active_adapter_from(adapters: Vec<Box<dyn AgentAdapter>>) -> Box<dyn AgentAdapter> {
    adapters
        .into_iter()
        .find(|a| a.is_active())
        .unwrap_or_else(|| Box::new(claude_code::ClaudeCodeAdapter))
}

/// Returns every adapter llmenv ships with, in preference order.
///
/// Callers PATH-gate each entry via [`crate::paths::binary_on_path`] before activating it,
/// so adapters for tools the user has not installed are silently skipped.
///
/// # Extending the registry
/// Add new adapters here once their crate is wired in:
pub(crate) fn registered_adapters() -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(claude_code::ClaudeCodeAdapter),
        Box::new(codex::CodexAdapter),
        Box::new(crush::CrushAdapter),
        Box::new(opencode::OpencodeAdapter),
    ]
}

/// Resolve an adapter by its engine ID (the underscore form from `--engine`
/// flags, e.g. `"claude_code"` or `"crush"`), or `None` when no registered
/// adapter answers to it.
///
/// Used by hook-run to honour the caller's `--engine` flag instead of
/// re-sniffing environment variables for adapter detection.
///
/// This used to fall back to env-sniffing [`active_adapter`] on no match, which
/// meant `hook-run --engine typo` ran the hook against whatever the environment
/// looked like, announced only by a `warn!` that llmenv's ERROR-only default
/// `EnvFilter` swallowed (#1386). Callers reject the value instead — see
/// [`require_known_engine`].
#[must_use]
pub(crate) fn adapter_for_engine(engine: &str) -> Option<Box<dyn AgentAdapter>> {
    registered_adapters()
        .into_iter()
        .find(|a| engine_id(a.as_ref()) == engine)
}

/// Reject an `--engine` value no registered adapter answers to, naming the ones
/// that exist (#1386).
///
/// Defined in terms of [`adapter_for_engine`] rather than its own comparison so
/// validation and resolution cannot drift: a value this accepts always resolves,
/// and one it rejects never silently resolves to something else.
///
/// # Errors
/// Returns an error listing every registered engine id when `engine` matches
/// none of them.
pub(crate) fn require_known_engine(engine: &str) -> anyhow::Result<()> {
    if adapter_for_engine(engine).is_some() {
        return Ok(());
    }
    Err(unknown_engine_error(engine))
}

/// The error [`require_known_engine`] and hook-run's adapter resolution both
/// report, so the two never describe the same rejection differently.
#[must_use]
pub(crate) fn unknown_engine_error(engine: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "unrecognized engine '{engine}' — expected one of: {}",
        known_engine_ids().join(", ")
    )
}

/// Resolve an adapter for `llmenv launch <target>`, matching `target` against
/// either the adapter's binary name (what a user types on the command line,
/// e.g. `claude`) or its engine id (the underscore form, e.g. `claude_code`).
///
/// Unlike [`adapter_for_engine`], this also accepts the binary name: `launch`'s
/// argument is what the user types to run the engine, not the underscore-form id
/// baked into hook commands.
#[must_use]
pub(crate) fn adapter_for_launch_target(target: &str) -> Option<Box<dyn AgentAdapter>> {
    registered_adapters()
        .into_iter()
        .find(|a| a.binary_name() == target || engine_id(a.as_ref()) == target)
}

/// Normalise an adapter's identity to the underscore form used by `--engine`
/// flags, `native.<engine>` config keys, and `disabled_engines` entries.
/// [`AgentAdapter::name`] is the hyphenated cache-dir form (`claude-code`);
/// this converts it to `claude_code` for comparison against those
/// user-facing engine-id strings.
#[must_use]
pub(crate) fn engine_id(adapter: &dyn AgentAdapter) -> String {
    adapter.name().replace('-', "_")
}

/// Every registered adapter's [`engine_id`], for validating user-facing
/// engine-id strings (`--engine`, `disabled_engines`) against what's actually
/// registered.
#[must_use]
pub(crate) fn known_engine_ids() -> Vec<String> {
    registered_adapters()
        .iter()
        .map(|a| engine_id(a.as_ref()))
        .collect()
}

/// Resolve bundle-relative paths in a hook command string.
/// Scans whitespace-separated tokens and resolves those containing '/' (but not
/// starting with '/', '~', '$', or '-') to absolute paths relative to `bundle_dir`.
///
/// Shared across adapters: any engine that renders a hook `command` string must
/// resolve bundle-relative script paths the same way, since a bundle is authored
/// once and materialized for every engine.
pub(crate) fn resolve_bundle_relative_paths(command: &str, bundle_dir: &Path) -> Option<String> {
    let mut resolved = false;
    let mut result = String::new();
    for (i, token) in command.split_whitespace().enumerate() {
        if i > 0 {
            result.push(' ');
        }
        if token.contains('/')
            && !token.starts_with('/')
            && !token.starts_with('~')
            && !token.starts_with('$')
            && !token.starts_with('-')
            && !crate::paths::is_unsafe_join_target(token)
        {
            let abs_path = bundle_dir.join(token);
            result.push_str(&abs_path.to_string_lossy());
            resolved = true;
        } else {
            result.push_str(token);
        }
    }
    if resolved { Some(result) } else { None }
}

/// Rewrite bundle-authored hook commands that reference files copied into the
/// cache directory, even when the command uses shell variables or absolute
/// paths that `resolve_bundle_relative_paths` cannot match.
///
/// For each whitespace-delimited token that contains `/`, checks whether the
/// token **ends with** any relative path in `known_files` at a path-component
/// boundary. When it does, the matched suffix is replaced with
/// `cache_dir.join(rel)`, re-anchoring the reference to the materialized copy.
/// When multiple known files match the same token, the **longest** suffix wins.
/// Tokens that don't match any known file are left untouched.
///
/// This handles cases like:
/// ```text
/// bash ${HOME}/git/my-llmenv/bundles/base/hooks/guard.sh
/// ```
/// where the token `${HOME}/git/my-llmenv/bundles/base/hooks/guard.sh` ends
/// with `hooks/guard.sh` — a file that was copied into the cache.
pub(crate) fn resolve_command_paths_against_files(
    command: &str,
    cache_dir: &Path,
    known_files: &std::collections::BTreeMap<PathBuf, PathBuf>,
) -> Option<String> {
    // Pre-compute string representations once so the inner loop stays O(1)
    // per candidate rather than O(files) allocations.
    // Sort by key length descending so the first filter+max_by_key pass
    // naturally prefers the longest (most specific) suffix.
    let mut candidates: Vec<(&Path, String)> = known_files
        .keys()
        .map(|k| {
            let s = k.to_string_lossy().into_owned();
            (k.as_path(), s)
        })
        .collect();
    candidates.sort_by_key(|(_, b)| std::cmp::Reverse(b.len()));

    let mut resolved = false;
    let mut result = String::new();
    for (i, token) in command.split_whitespace().enumerate() {
        if i > 0 {
            result.push(' ');
        }
        // Unlike resolve_bundle_relative_paths, we never join the token
        // itself — the join operand is `rel`, a trusted key from known_files.
        // So is_unsafe_join_target on the token is not needed here; absolute
        // paths and even `../`-prefixed paths can be safely suffix-matched.
        if token.contains('/')
            && let Some((rel, _suffix)) = candidates.iter().find(|(_, s)| {
                // Require a path-component boundary before the suffix:
                // the suffix starts at position 0 in the token, or the
                // character immediately before it is '/'.
                let prefix_len = token.len().saturating_sub(s.len());
                token.ends_with(s.as_str())
                    && (prefix_len == 0 || token.as_bytes().get(prefix_len - 1) == Some(&b'/'))
            })
        {
            // Defense in depth: rel is trusted (it came from a filesystem
            // walk + strip_prefix), but guard against future changes that add
            // user-supplied paths to known_files.
            debug_assert!(
                !crate::paths::is_unsafe_join_target(rel.to_string_lossy().as_ref()),
                "known_files key contains traversal: {}",
                rel.display()
            );
            let abs_path = cache_dir.join(rel);
            result.push_str(&abs_path.to_string_lossy());
            resolved = true;
            continue;
        }
        result.push_str(token);
    }
    if resolved { Some(result) } else { None }
}

/// Format injected hook context in the adapter-native hook-output shape.
///
/// Empty input always returns an empty string. Store-only events
/// (SessionStart, SessionEnd) also return empty — they have no model turn
/// to inject context into, and all known adapter schemas reject
/// `additionalContext` in their `hookSpecificOutput` for these events.
///
/// This is the shared implementation behind every adapter's
/// [`AgentAdapter::emit_hook_context`], replacing the three copies that
/// previously existed in claude_code.rs, crush.rs, and opencode.rs.
///
/// # Arguments
/// * `hook_event_name` — the event name (e.g. `"SessionStart"`), echoed
///   back as `hookEventName` inside `hookSpecificOutput`.
/// * `text` — the injected context, placed as `additionalContext`.
#[must_use]
pub(crate) fn emit_hook_context(hook_event_name: &str, text: &str) -> String {
    // Whitespace-only counts as empty: an all-advisory recall (stripped by
    // strip_advisory) can leave blank lines behind, and wrapping those would
    // inject an empty "[ICM MEMORY CONTEXT]" block on every turn (#978).
    if text.trim().is_empty() {
        return String::new();
    }
    // Store-only events (SessionStart, SessionEnd) have no model turn to inject
    // context into, and most adapters' hook schemas reject additionalContext in
    // hookSpecificOutput. Return empty so these events emit no output. (#558)
    if matches!(hook_event_name, "SessionStart" | "SessionEnd") {
        return String::new();
    }
    let wrapped = format!("[ICM MEMORY CONTEXT (auto-injected)]\n{text}");
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": hook_event_name,
            "additionalContext": wrapped
        }
    })
    .to_string()
}

/// Resolve the on-disk payload directory for a plugin.
///
/// External plugins (`install_path = Some`) use that path directly.
/// First-party plugins look up their marketplace `install_location`.
///
/// Shared across adapters: previously lived in `crush.rs` and was
/// cross-imported by opencode via `super::crush::resolve_plugin_payload`.
fn resolve_plugin_payload(
    plugin: &crate::plugins::resolve::ResolvedPlugin,
    marketplaces: &[crate::plugins::resolve::ResolvedMarketplace],
) -> anyhow::Result<PathBuf> {
    // P2-5/#534: guard before any path join, regardless of which path is taken.
    if !crate::paths::is_valid_short_name(&plugin.plugin) {
        anyhow::bail!("plugin name '{}' is not a valid name", plugin.plugin);
    }
    if let Some(p) = &plugin.install_path {
        return Ok(PathBuf::from(p));
    }
    let mkt = marketplaces
        .iter()
        .find(|m| m.name == plugin.marketplace)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "plugin '{}': marketplace '{}' not found in resolved marketplaces",
                plugin.plugin,
                plugin.marketplace
            )
        })?;
    let install_location = mkt.install_location.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "plugin '{}': marketplace '{}' has no install_location (not yet synced?)",
            plugin.plugin,
            plugin.marketplace
        )
    })?;
    Ok(PathBuf::from(install_location).join(&plugin.plugin))
}

/// Map a resolved remote transport onto the `type` discriminator string shared
/// by every engine's remote-MCP config shape (`"http"` / `"sse"`).
///
/// `ResolvedKind::Remote` never actually carries `McpTransport::Stdio` (stdio
/// servers always resolve to `ResolvedKind::Stdio` instead — see
/// `crate::mcp::resolve`), so that arm is unreachable in practice; it is
/// folded to `"http"` defensively rather than panicking.
pub(crate) fn remote_transport_type_str(transport: crate::config::McpTransport) -> &'static str {
    use crate::config::McpTransport;
    match transport {
        McpTransport::Sse => "sse",
        McpTransport::Http | McpTransport::Stdio => "http",
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        AgentAdapter, MEMORY_MCP_NAME, active_adapter_from, adapter_for_launch_target,
        emit_hook_context, engine_id, known_engine_ids, lifecycle_event_registered,
        lifecycle_hook_registrations, modeled_key_redirect, overlay_native_json,
        registered_adapters, remote_transport_type_str, resolve_bundle_relative_paths,
        resolve_command_paths_against_files, strip_json_nulls,
    };
    use crate::merge::MergedManifest;

    /// Minimal `AgentAdapter` stand-in for testing dispatch logic
    /// (`active_adapter_from`) without depending on any real adapter's
    /// `is_active()`, which reads process-global env vars.
    struct FakeAdapter {
        name: &'static str,
        active: bool,
    }

    impl AgentAdapter for FakeAdapter {
        fn name(&self) -> &'static str {
            self.name
        }

        fn is_active(&self) -> bool {
            self.active
        }

        fn binary_name(&self) -> &'static str {
            "fake"
        }

        fn supports_plugins(&self) -> bool {
            false
        }

        fn supports_lsp(&self) -> bool {
            false
        }

        fn supports_model_providers(&self) -> bool {
            false
        }

        fn supports_output_styles(&self) -> bool {
            false
        }

        fn native_maps(&self) -> &'static [&'static str] {
            &[]
        }

        fn supported_hook_events(&self) -> &'static [&'static str] {
            &[]
        }

        fn env_vars(
            &self,
            _cache_dir: &Path,
            _state_dir: &Path,
        ) -> anyhow::Result<Vec<(String, String)>> {
            Ok(Vec::new())
        }

        fn materialize(
            &self,
            _manifest: &MergedManifest,
            _out: &Path,
        ) -> anyhow::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }

        fn emit_hook_context(&self, _hook_event_name: &str, _text: &str) -> String {
            String::new()
        }
    }

    /// #1008: the rejection message must never invent a `native_*` field. Keys
    /// with a real hatch name it; keys without one are sent to the neutral field.
    #[test]
    fn modeled_key_redirect_only_names_hatches_that_exist() {
        for (key, expected) in [
            ("permission", "native_permissions.opencode"),
            ("permissions", "native_permissions.crush"),
            ("hooks", "native_hooks.crush"),
            ("mcp", "native_mcp.opencode"),
            ("provider", "native_model_providers.opencode"),
            ("providers", "native_model_providers.crush"),
        ] {
            let engine = expected.rsplit('.').next().unwrap();
            assert!(
                modeled_key_redirect(key, engine).contains(expected),
                "`{key}` must be redirected to `{expected}`"
            );
        }
        for key in ["model", "models", "small_model", "lsp", "instructions"] {
            let msg = modeled_key_redirect(key, "opencode");
            assert!(
                msg.contains("no `native_*.opencode` escape hatch"),
                "`{key}` has no hatch — the message must say so, got: {msg}"
            );
        }
    }

    /// A non-mapping fragment replaces rather than merges, so it must be
    /// rejected at the shared choke point for every `native_*` channel.
    #[test]
    fn overlay_native_json_rejects_non_mapping_fragment() {
        for (yaml, kind) in [("oops", "a string"), ("~", "null"), ("[1]", "a sequence")] {
            let mut dst = serde_json::json!({"kept": 1});
            let frag: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
            let err = overlay_native_json(&mut dst, Some(&frag), "native_mcp.crush").unwrap_err();
            assert!(err.to_string().contains(kind), "got: {err}");
            assert_eq!(dst["kept"], 1, "dst must be left untouched on error");
        }
    }

    // #978: a recall stripped down to only blank lines must inject nothing, not
    // an empty "[ICM MEMORY CONTEXT]" block.
    #[test]
    fn config_schema_defaults_to_none_for_adapters_without_a_schema() {
        assert!(
            crate::adapter::claude_code::ClaudeCodeAdapter
                .config_schema()
                .is_none()
        );
        assert!(
            crate::adapter::crush::CrushAdapter
                .config_schema()
                .is_none()
        );
    }

    #[test]
    fn emit_hook_context_treats_whitespace_only_as_empty() {
        assert!(emit_hook_context("UserPromptSubmit", "\n\n").is_empty());
        assert!(emit_hook_context("UserPromptSubmit", "   ").is_empty());
    }

    #[test]
    fn registered_adapters_are_expected() {
        let adapters = registered_adapters();
        assert_eq!(
            adapters.len(),
            4,
            "registry should have exactly four adapters"
        );
        assert_eq!(adapters[0].name(), "claude-code");
        assert_eq!(adapters[1].name(), "codex");
        assert_eq!(adapters[2].name(), "crush");
        assert_eq!(adapters[3].name(), "opencode");
    }

    #[test]
    fn adapter_for_launch_target_matches_binary_name() {
        let a = adapter_for_launch_target("claude");
        assert!(a.is_some(), "binary name 'claude' should resolve");
        assert_eq!(a.unwrap().binary_name(), "claude");
    }

    #[test]
    fn adapter_for_launch_target_matches_engine_id() {
        let a = adapter_for_launch_target("claude_code");
        assert!(a.is_some(), "engine id 'claude_code' should resolve");
        assert_eq!(a.unwrap().binary_name(), "claude");
    }

    #[test]
    fn adapter_for_launch_target_matches_other_adapters() {
        assert_eq!(
            adapter_for_launch_target("crush").unwrap().binary_name(),
            "crush"
        );
        assert_eq!(
            adapter_for_launch_target("opencode").unwrap().binary_name(),
            "opencode"
        );
    }

    proptest::proptest! {
        /// The three enablers of `stop` are independent: any one of them on is
        /// enough, and all three off is the only way it stays unregistered.
        /// Hand-picked fixtures cover a handful of the eight combinations;
        /// this covers all of them, so a gate that silently starts requiring
        /// two of the three can't slip through.
        #[test]
        fn stop_is_registered_for_any_single_enabler(
            session_log in proptest::bool::ANY,
            task_tracker in proptest::bool::ANY,
            self_critique in proptest::bool::ANY,
        ) {
            let mut manifest = MergedManifest::default();
            if !session_log {
                manifest.session_log.file = None;
                manifest.session_log.transcript = None;
            }
            manifest.capabilities.features = Some(llmenv_config::Features {
                task_tracker: Some(llmenv_config::TaskTracker {
                    enabled: task_tracker,
                    ..Default::default()
                }),
                slippage: Some(llmenv_config::SlippageControl {
                    enabled: true,
                    self_critique,
                    ..Default::default()
                }),
                ..Default::default()
            });
            proptest::prop_assert_eq!(
                lifecycle_event_registered(&manifest, "stop"),
                manifest.session_log.any_sink_enabled() || task_tracker || self_critique
            );
        }

        /// `turn_start` tracks the memory MCP and nothing else — no other
        /// feature may switch a per-prompt network hook on behind the user.
        #[test]
        fn turn_start_tracks_only_the_memory_mcp(
            task_tracker in proptest::bool::ANY,
            self_critique in proptest::bool::ANY,
            memory in proptest::bool::ANY,
        ) {
            let mut manifest = MergedManifest::default();
            manifest.capabilities.features = Some(llmenv_config::Features {
                task_tracker: Some(llmenv_config::TaskTracker {
                    enabled: task_tracker,
                    ..Default::default()
                }),
                slippage: Some(llmenv_config::SlippageControl {
                    enabled: true,
                    self_critique,
                    ..Default::default()
                }),
                ..Default::default()
            });
            if memory {
                manifest.mcps.push(crate::mcp::resolve::ResolvedMcp {
                    name: MEMORY_MCP_NAME.to_string(),
                    kind: crate::mcp::resolve::ResolvedKind::Remote {
                        url: "http://localhost:9999".into(),
                        transport: crate::config::McpTransport::Http,
                    },
                    headers: Default::default(),
                    timeout: None,
                    disabled_tools: vec![],
                    mcp_permissions: None,
                    wakeup_max_tokens: None,
                });
            }
            proptest::prop_assert_eq!(
                lifecycle_event_registered(&manifest, "turn_start"),
                memory
            );
        }

        /// `lifecycle_event_registered` is only a lookup — it must never
        /// disagree with the table it reads, and an event that isn't in the
        /// table is not registered.
        #[test]
        fn lifecycle_event_registered_agrees_with_the_table(event in "[a-z_]{0,20}") {
            let manifest = MergedManifest::default();
            let expected = lifecycle_hook_registrations(&manifest)
                .into_iter()
                .any(|(name, registered, _)| name == event && registered);
            proptest::prop_assert_eq!(lifecycle_event_registered(&manifest, &event), expected);
        }

        /// The safety contract in `adapter_for_launch_target`'s doc comment —
        /// never silently resolve an unrecognized engine, because that risks
        /// launching the wrong binary. Example tests only cover the four names
        /// someone thought to write down; this covers arbitrary input.
        #[test]
        fn adapter_for_launch_target_only_resolves_known_names(target in r"\PC*") {
            let known: Vec<String> = registered_adapters()
                .iter()
                .flat_map(|a| [a.binary_name().to_string(), engine_id(a.as_ref())])
                .collect();
            match adapter_for_launch_target(&target) {
                None => proptest::prop_assert!(
                    !known.contains(&target),
                    "a known engine name '{}' failed to resolve",
                    target
                ),
                Some(a) => proptest::prop_assert!(
                    a.binary_name() == target || engine_id(a.as_ref()) == target,
                    "'{}' resolved to unrelated adapter '{}'",
                    target,
                    a.binary_name()
                ),
            }
        }
    }

    #[test]
    fn adapter_for_launch_target_rejects_unknown_target() {
        assert!(
            adapter_for_launch_target("__llmenv_no_such_engine_xyzzy__").is_none(),
            "an unrecognized engine must not silently resolve to any adapter"
        );
    }

    #[test]
    fn registry_adapter_trait_probes() {
        let adapters = registered_adapters();

        // ClaudeCodeAdapter
        let a = &*adapters[0];
        assert_eq!(a.binary_name(), "claude");
        assert!(a.supports_plugins(), "ClaudeCodeAdapter supports plugins");
        assert!(a.supports_lsp(), "ClaudeCodeAdapter supports LSP (#556)");
        assert!(
            !a.supports_model_providers(),
            "ClaudeCodeAdapter does not support model providers"
        );
        let events = a.supported_hook_events();
        for expected in [
            "SessionStart",
            "SessionEnd",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Notification",
            "Stop",
            "SubagentStop",
            "PreCompact",
        ] {
            assert!(
                events.contains(&expected),
                "supported_hook_events missing {expected}"
            );
        }

        // CodexAdapter (#233 slice 1: MCP + AGENTS.md only)
        let x = &*adapters[1];
        assert_eq!(x.binary_name(), "codex");
        assert!(!x.supports_plugins(), "Codex plugin parity is #1106");
        assert!(!x.supports_lsp(), "Codex has no LSP config block");
        assert!(
            x.supported_hook_events().contains(&"PreToolUse"),
            "Codex renders hooks now (#233)"
        );
        assert!(
            !x.supported_hook_events().contains(&"Notification"),
            "Codex has no Notification event — claiming it would silently drop the hook"
        );

        // CrushAdapter
        let c = &*adapters[2];
        assert_eq!(c.binary_name(), "crush");
        assert!(
            !c.supports_plugins(),
            "CrushAdapter does not support plugins"
        );
        assert!(c.supports_lsp(), "CrushAdapter supports LSP");
        assert!(
            c.supports_model_providers(),
            "CrushAdapter supports model providers"
        );
        assert!(
            c.supported_hook_events().contains(&"PreToolUse"),
            "CrushAdapter must support PreToolUse"
        );
    }

    /// Every adapter's `materialize` must report its owned paths **relative to
    /// `out`** — the trait says so, and `CacheManifest::new` enforces it by
    /// filtering absolute paths through `is_unsafe_join_target` (#196). An
    /// adapter that returns absolute paths doesn't fail: its owned set silently
    /// becomes empty, so files it later stops writing are never ghost-reconciled
    /// and stale config lingers in the engine's config dir.
    ///
    /// Nothing enforced this until the Codex adapter got it wrong (#233), so the
    /// check lives here, over the whole registry, rather than in one adapter's
    /// tests.
    #[test]
    fn every_adapter_reports_owned_paths_relative_to_out() {
        let manifest = MergedManifest {
            agents_md: "# Rules\n".into(),
            ..MergedManifest::default()
        };
        for adapter in registered_adapters() {
            let dir = tempfile::tempdir().unwrap();
            let Ok(owned) = adapter.materialize(&manifest, dir.path()) else {
                // An adapter that declines to render this minimal manifest has
                // nothing to say about path shape.
                continue;
            };
            for path in &owned {
                assert!(
                    path.is_relative(),
                    "{} returned the absolute owned path {} — CacheManifest would \
                     discard it, leaving the file unowned and never cleaned up",
                    adapter.name(),
                    path.display()
                );
            }
        }
    }

    #[test]
    fn engine_id_normalises_hyphen_to_underscore() {
        let adapters = registered_adapters();
        assert_eq!(engine_id(adapters[0].as_ref()), "claude_code");
        assert_eq!(engine_id(adapters[1].as_ref()), "codex");
        assert_eq!(engine_id(adapters[2].as_ref()), "crush");
        assert_eq!(engine_id(adapters[3].as_ref()), "opencode");
    }

    #[test]
    fn known_engine_ids_matches_registered_adapters() {
        assert_eq!(
            known_engine_ids(),
            vec!["claude_code", "codex", "crush", "opencode"]
        );
    }

    /// Locks the `native_*` consumption matrix (#1032). This is the source of
    /// truth `native_keys` validates against, and it is hand-maintained — grep
    /// for `native_<map>.get(` in each adapter to re-derive it. Adding a `.get()`
    /// without adding the map here means the key is still silently dropped;
    /// removing a `.get()` without removing it here means dead config is blessed.
    #[test]
    fn native_maps_match_actual_consumers() {
        use crate::adapter::native_keys as nk;
        let declared: Vec<(String, Vec<&str>)> = registered_adapters()
            .iter()
            .map(|a| (engine_id(a.as_ref()), a.native_maps().to_vec()))
            .collect();
        assert_eq!(
            declared,
            vec![
                (
                    "claude_code".to_string(),
                    vec![
                        nk::NATIVE_PERMISSIONS,
                        nk::NATIVE_HOOKS,
                        nk::NATIVE_PLUGINS,
                        nk::NATIVE_MCP,
                        nk::NATIVE,
                    ]
                ),
                // Codex renders MCP and hooks (#233). `native_permissions` stays
                // absent until #1102 lands, so `doctor` keeps reporting that key
                // as dead for Codex rather than blessing config that does
                // nothing.
                (
                    "codex".to_string(),
                    vec![nk::NATIVE_MCP, nk::NATIVE_HOOKS, nk::NATIVE]
                ),
                (
                    "crush".to_string(),
                    vec![
                        nk::NATIVE_PERMISSIONS,
                        nk::NATIVE_HOOKS,
                        nk::NATIVE_MCP,
                        nk::NATIVE_MODEL_PROVIDERS,
                        nk::NATIVE_DEFAULT_MODELS,
                        nk::NATIVE,
                    ]
                ),
                (
                    "opencode".to_string(),
                    vec![
                        nk::NATIVE_PERMISSIONS,
                        nk::NATIVE_MCP,
                        nk::NATIVE_MODEL_PROVIDERS,
                        nk::NATIVE,
                    ]
                ),
            ]
        );
    }

    /// The colon-prefix permission lint (#838) resolves opencode by this exact
    /// engine id, so a rename must fail here rather than silently disable it.
    #[test]
    fn opencode_engine_id_is_stable() {
        assert!(
            known_engine_ids().contains(&"opencode".to_string()),
            "the #838 permission lint looks opencode up by this id"
        );
    }

    #[test]
    fn engine_id_matches_baked_engine_flag_default() {
        // The `--engine` flag default baked into hook commands is the underscore
        // form of an adapter's name (`claude_code`), while name() is hyphenated
        // (`claude-code`). Guard that at least one registered adapter's normalised
        // identity equals the baked default, so warn_if_unknown_engine (which
        // normalises the same way) never spuriously warns on the default path.
        let adapters = registered_adapters();
        assert!(
            adapters
                .iter()
                .any(|a| engine_id(a.as_ref()) == "claude_code"),
            "no registered adapter's engine id matches the baked --engine default 'claude_code'"
        );
    }

    #[test]
    fn resolve_bundle_relative_paths_rewrites_relative_token() {
        let dir = std::path::Path::new("/bundles/foo");
        let resolved = resolve_bundle_relative_paths("bash hooks/guard.sh", dir);
        assert_eq!(
            resolved,
            Some("bash /bundles/foo/hooks/guard.sh".to_string())
        );
    }

    #[test]
    fn resolve_bundle_relative_paths_leaves_absolute_and_shell_tokens_alone() {
        let dir = std::path::Path::new("/bundles/foo");
        assert!(resolve_bundle_relative_paths("bash /abs/path.sh", dir).is_none());
        assert!(resolve_bundle_relative_paths("bash ${HOME}/x.sh", dir).is_none());
        assert!(resolve_bundle_relative_paths("bash ~/x.sh", dir).is_none());
        assert!(resolve_bundle_relative_paths("echo hello", dir).is_none());
    }

    #[test]
    fn remote_transport_type_str_maps_http_and_sse() {
        use crate::config::McpTransport;
        assert_eq!(remote_transport_type_str(McpTransport::Http), "http");
        assert_eq!(remote_transport_type_str(McpTransport::Sse), "sse");
        assert_eq!(
            remote_transport_type_str(McpTransport::Stdio),
            "http",
            "unreachable in practice, but must not panic"
        );
    }

    // ---- resolve_command_paths_against_files ----

    fn known_files_from_paths(paths: &[&str]) -> std::collections::BTreeMap<PathBuf, PathBuf> {
        paths
            .iter()
            .map(|p| (PathBuf::from(p), PathBuf::from(format!("/source/{p}"))))
            .collect()
    }

    #[test]
    fn suffix_matches_shell_var_prefixed_token() {
        let files = known_files_from_paths(&["hooks/guard.sh"]);
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files(
            "bash ${HOME}/bundles/base/hooks/guard.sh",
            cache,
            &files,
        );
        assert_eq!(resolved, Some("bash /cache/hooks/guard.sh".to_string()));
    }

    #[test]
    fn picks_longest_suffix_when_multiple_match() {
        let files = known_files_from_paths(&["guard.sh", "hooks/guard.sh"]);
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files(
            "bash ${HOME}/bundles/base/hooks/guard.sh",
            cache,
            &files,
        );
        assert_eq!(
            resolved,
            Some("bash /cache/hooks/guard.sh".to_string()),
            "must pick hooks/guard.sh (longer), not guard.sh"
        );
    }

    #[test]
    fn requires_path_component_boundary_before_suffix() {
        // "my-hooks/guard.sh" ends with "hooks/guard.sh" but the substring
        // crosses a component boundary — it should not match.
        let files = known_files_from_paths(&["hooks/guard.sh"]);
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files("bash my-hooks/guard.sh", cache, &files);
        assert_eq!(
            resolved, None,
            "must not match suffix that crosses a path-component boundary"
        );
    }

    #[test]
    fn matches_absolute_path_token() {
        // Absolute-path tokens are not blocked — the join operand is the
        // trusted `rel` key, not the untrusted token.
        let files = known_files_from_paths(&["hooks/guard.sh"]);
        let cache = Path::new("/cache");
        let resolved =
            resolve_command_paths_against_files("bash /abs/path/hooks/guard.sh", cache, &files);
        assert_eq!(resolved, Some("bash /cache/hooks/guard.sh".to_string()));
    }

    #[test]
    fn empty_known_files_never_matches() {
        let files: std::collections::BTreeMap<PathBuf, PathBuf> = std::collections::BTreeMap::new();
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files("bash hooks/guard.sh", cache, &files);
        assert_eq!(resolved, None);
    }

    #[test]
    fn token_without_slash_never_matches() {
        let files = known_files_from_paths(&["guard.sh"]);
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files("bash guard.sh", cache, &files);
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolves_multiple_tokens_in_command() {
        let files = known_files_from_paths(&["hooks/pre.sh", "hooks/post.sh"]);
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files(
            "bash /some/where/hooks/pre.sh /other/where/hooks/post.sh",
            cache,
            &files,
        );
        assert_eq!(
            resolved,
            Some("bash /cache/hooks/pre.sh /cache/hooks/post.sh".to_string())
        );
    }

    // #1305: active_adapter_from is the dispatch algorithm behind
    // active_adapter(), split out so it's testable with fakes instead of
    // mutating real adapters' env-var-backed is_active() signals.
    #[test]
    fn active_adapter_from_picks_first_active_in_order() {
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![
            Box::new(FakeAdapter {
                name: "first",
                active: false,
            }),
            Box::new(FakeAdapter {
                name: "second",
                active: true,
            }),
            Box::new(FakeAdapter {
                name: "third",
                active: true,
            }),
        ];
        assert_eq!(
            active_adapter_from(adapters).name(),
            "second",
            "must return the first active adapter, not just any active one"
        );
    }

    #[test]
    fn active_adapter_from_falls_back_to_claude_code_when_none_active() {
        let adapters: Vec<Box<dyn AgentAdapter>> = vec![
            Box::new(FakeAdapter {
                name: "first",
                active: false,
            }),
            Box::new(FakeAdapter {
                name: "second",
                active: false,
            }),
        ];
        assert_eq!(active_adapter_from(adapters).name(), "claude-code");
    }

    #[test]
    fn active_adapter_from_handles_empty_registry() {
        let adapters: Vec<Box<dyn AgentAdapter>> = Vec::new();
        assert_eq!(active_adapter_from(adapters).name(), "claude-code");
    }

    // #793: resolve_bundle_relative_paths rewrites bundle-relative tokens in a
    // hook command against an absolute bundle dir. It runs on untrusted,
    // bundle-authored command strings, so the invariants that matter are
    // no-panic on any input, idempotence (a resolved command has no more
    // relative tokens to rewrite), and that every rewritten token is absolute
    // under the bundle dir.
    #[allow(clippy::expect_used)]
    mod resolve_bundle_relative_paths_proptests {
        use super::resolve_bundle_relative_paths;
        use proptest::prelude::*;
        use std::path::Path;

        // A single command token biased across every branch of the resolver:
        // bundle-relative (rewritten), plus the four skip cases (absolute,
        // tilde, shell-var, flag), plain words, and traversal attempts.
        fn arb_token() -> impl Strategy<Value = String> {
            prop_oneof![
                "[a-z]{1,5}/[a-z]{1,5}",    // bundle-relative → rewritten
                "/[a-z]{1,8}",              // absolute → skipped
                "~/[a-z]{1,8}",             // tilde → skipped
                "[$][A-Z]{1,5}/[a-z]{1,5}", // shell var → skipped
                "-[a-z]{1,5}",              // flag → skipped
                "[a-z]{1,8}",               // plain word (no slash) → skipped
                "[.][.]/[a-z]{1,5}",        // traversal → skipped (unsafe join)
            ]
        }

        fn arb_command() -> impl Strategy<Value = String> {
            proptest::collection::vec(arb_token(), 0..6).prop_map(|toks| toks.join(" "))
        }

        proptest! {
            // Arbitrary command strings never panic the resolver.
            #[test]
            fn never_panics(command in ".{0,60}") {
                let _ = resolve_bundle_relative_paths(&command, Path::new("/bundle/dir"));
            }

            // Idempotence: once resolved, every eligible token is now absolute,
            // so a second pass finds nothing to rewrite and returns None.
            #[test]
            fn is_idempotent(command in arb_command()) {
                let dir = Path::new("/bundle/dir");
                if let Some(resolved) = resolve_bundle_relative_paths(&command, dir) {
                    prop_assert_eq!(
                        resolve_bundle_relative_paths(&resolved, dir),
                        None,
                        "re-resolving an already-resolved command must be a no-op"
                    );
                }
            }

            // A command built only from bundle-relative tokens resolves, and
            // every resulting token is absolute under the bundle dir.
            #[test]
            fn resolved_relative_tokens_are_absolute_under_bundle(
                rels in proptest::collection::vec("[a-z]{1,5}/[a-z]{1,5}", 1..5),
            ) {
                let dir = Path::new("/bundle/dir");
                let command = rels.join(" ");
                let resolved = resolve_bundle_relative_paths(&command, dir)
                    .expect("all-relative command must resolve");
                for token in resolved.split_whitespace() {
                    prop_assert!(
                        token.starts_with("/bundle/dir/"),
                        "resolved token {token:?} not absolute under bundle dir"
                    );
                }
            }
        }
    }

    #[test]
    fn strip_json_nulls_removes_null_vals() {
        let mut v = serde_json::json!({
            "a": null,
            "b": 1,
            "c": { "d": null, "e": [{"f": null, "g": 2}] }
        });
        strip_json_nulls(&mut v);
        assert_eq!(
            v,
            serde_json::json!({
                "b": 1,
                "c": { "e": [{ "g": 2 }] }
            })
        );
    }

    // #1274: pins the depth-guard's documented fail-open boundary so a future
    // change to the bound (or an accidental fail-closed rewrite) shows up as
    // a failing test rather than a silent behavior drift. `arb_json` above
    // only nests ~3-4 levels deep, so this boundary is otherwise untested.
    #[test]
    fn strip_json_nulls_bails_open_past_depth_64() {
        let mut v = serde_json::json!({"null_here": null, "depth": 65});
        for depth in (0..65).rev() {
            v = serde_json::json!({"null_here": null, "depth": depth, "next": v});
        }
        strip_json_nulls(&mut v);

        let mut current = &v;
        for depth in 0..=65 {
            assert_eq!(current["depth"], depth, "walked to the wrong nesting level");
            if depth <= 64 {
                assert!(
                    current.get("null_here").is_none(),
                    "depth {depth} is within the guard's bound and must be stripped"
                );
            } else {
                assert_eq!(
                    current.get("null_here"),
                    Some(&serde_json::Value::Null),
                    "depth {depth} is past the guard's bound and must be left untouched"
                );
            }
            if depth < 65 {
                current = &current["next"];
            }
        }
    }

    #[allow(clippy::expect_used)]
    mod strip_json_nulls_proptests {
        use super::strip_json_nulls;
        use llmenv_util::testkit::arb_json;
        use proptest::prelude::*;

        fn contains_no_nulls(v: &serde_json::Value) -> bool {
            match v {
                // Only check for null-valued *keys in objects* — that's what
                // strip_json_nulls removes. Bare null or null array elements
                // are not touched, so don't flag them.
                serde_json::Value::Array(items) => items.iter().all(contains_no_nulls),
                serde_json::Value::Object(map) => {
                    !map.values().any(|v| v.is_null()) && map.values().all(contains_no_nulls)
                }
                _ => true,
            }
        }

        fn count_non_null_leaves(v: &serde_json::Value) -> usize {
            match v {
                serde_json::Value::Null => 0,
                serde_json::Value::Array(items) => items.iter().map(count_non_null_leaves).sum(),
                serde_json::Value::Object(map) => map.values().map(count_non_null_leaves).sum(),
                _ => 1,
            }
        }

        proptest! {
            // strip_json_nulls never panics on arbitrary JSON input.
            #[test]
            fn strip_json_nulls_total(mut v in arb_json()) {
                strip_json_nulls(&mut v);
            }

            // Idempotency: applying strip_json_nulls twice equals applying it once.
            #[test]
            fn strip_json_nulls_idempotent(v in arb_json()) {
                let mut once = v.clone();
                strip_json_nulls(&mut once);
                let mut twice = once.clone();
                strip_json_nulls(&mut twice);
                prop_assert_eq!(once, twice);
            }

            // Completeness: after strip_json_nulls, no Value::Null exists at any depth.
            #[test]
            fn strip_json_nulls_no_nulls_remain(mut v in arb_json()) {
                strip_json_nulls(&mut v);
                prop_assert!(contains_no_nulls(&v), "null values remain after strip_json_nulls");
            }

            // Non-null preservation: non-null leaf values are structurally preserved.
            #[test]
            fn strip_json_nulls_preserves_non_null(mut v in arb_json()) {
                let expected = count_non_null_leaves(&v);
                strip_json_nulls(&mut v);
                let actual = count_non_null_leaves(&v);
                prop_assert_eq!(expected, actual,
                    "strip_json_nulls should not remove non-null values");
            }
        }
    }
}
