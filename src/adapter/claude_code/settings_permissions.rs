//! The `permissions` object of the Claude Code `settings.json` (#34, #888, #946, #972, #1322).
//!
//! Neutral rules and native rules share the three arrays, and each array is rendered here. Deny
//! outranks ask, and ask outranks allow, so a native rule drops a neutral rule of a weaker action
//! only, and no source can weaken a deny.

use std::collections::BTreeSet;

use crate::adapter::claude_code::{
    CBM_DESTRUCTIVE, CBM_MCP_PREFIX, CBM_MUTATION, CBM_READ_ONLY, CTX_DESTRUCTIVE, CTX_MUTATION,
    CTX_READ_ONLY, ICM_DESTRUCTIVE, ICM_MCP_PREFIX, ICM_MUTATION, ICM_READ_ONLY, NativeCover,
    PermBuckets, apply_mcp_tier_permissions, normalize_deprecated_tool, permission_mode_str,
    render_permission_rule,
};
use crate::adapter::mcp_tiers::McpTier;
use crate::mcp::resolve::MEMORY_MCP_NAME;
use crate::merge::MergedManifest;
use crate::util::dedup;

/// The native rule strings of the three actions, with the deprecated `Write(...)` form rewritten
/// (#888). A bundle that follows an old example gets the same rewrite as a neutral rule, before
/// the suppression and before the rule reaches `settings.json`.
struct NativeRules {
    allow: Vec<String>,
    ask: Vec<String>,
    deny: Vec<String>,
}

impl NativeRules {
    fn of(manifest: &MergedManifest) -> Self {
        let native = manifest.capabilities.native_permissions.get("claude_code");
        let normalize = |select: fn(&crate::config::NativePermissionRules) -> &[String]| {
            native.map_or_else(Vec::new, |n| {
                select(n)
                    .iter()
                    .map(|s| normalize_deprecated_tool(s))
                    .collect()
            })
        };
        Self {
            allow: normalize(|n| &n.allow),
            ask: normalize(|n| &n.ask),
            deny: normalize(|n| &n.deny),
        }
    }
}

/// The `permissions` object, or `None` when no rule and no default mode is set.
pub(super) fn render_permissions(manifest: &MergedManifest) -> Option<serde_json::Value> {
    let perms = &manifest.capabilities.permissions;
    let native = NativeRules::of(manifest);
    // Only deny and ask can outrank a neutral rule (deny > ask > allow), so a native allow set is
    // never a suppressor.
    let native_ask: BTreeSet<&str> = native.ask.iter().map(String::as_str).collect();
    let native_deny: BTreeSet<&str> = native.deny.iter().map(String::as_str).collect();

    let mut allow = render_action(&perms.allow, &native.allow, &[&native_deny, &native_ask]);
    let mut ask = render_action(&perms.ask, &native.ask, &[&native_deny]);
    let mut deny = render_action(&perms.deny, &native.deny, &[]);

    let cover = NativeCover {
        deny: &native_deny,
        ask: &native_ask,
    };
    let mut buckets = PermBuckets {
        allow: &mut allow,
        ask: &mut ask,
        deny: &mut deny,
    };
    context_mode_tiers(manifest, &mut buckets, &cover);
    icm_tiers(manifest, &mut buckets, &cover);
    codebase_memory_tiers(manifest, &mut buckets, &cover);

    dedup(&mut allow);
    dedup(&mut ask);
    dedup(&mut deny);
    drop_overlaps(&mut allow, &mut ask, &deny);
    permissions_object(perms.default_mode, allow, ask, deny)
}

/// The rendered strings of the neutral rules of one action, then the native rules of that action.
/// A neutral string is dropped when a more authoritative native action holds it, unless this
/// action's own native list holds it too, so an agreeing pair still emits.
fn render_action(
    neutral: &[crate::config::PermissionRule],
    native_rules: &[String],
    outranking: &[&BTreeSet<&str>],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for rule in neutral {
        for s in render_permission_rule(rule) {
            let outranked = outranking.iter().any(|set| set.contains(s.as_str()));
            if outranked && !native_rules.contains(&s) {
                continue;
            }
            out.push(s);
        }
    }
    out.extend(native_rules.iter().cloned());
    dedup(&mut out);
    out
}

/// The tiers of the context-mode MCP, when its plugin is enabled (#946).
fn context_mode_tiers(
    manifest: &MergedManifest,
    buckets: &mut PermBuckets<'_>,
    cover: &NativeCover<'_>,
) {
    let enabled = manifest.plugins.iter().any(|p| {
        p.marketplace == crate::config::CONTEXT_MODE_MARKETPLACE
            && p.plugin == crate::config::CONTEXT_MODE_PLUGIN
    });
    if !enabled {
        return;
    }
    let overrides = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.context_mode.as_ref())
        .and_then(|c| c.mcp_permissions.as_ref());
    apply_mcp_tier_permissions(
        buckets,
        crate::config::CONTEXT_MODE_MCP_PREFIX,
        [
            (CTX_READ_ONLY, McpTier::ReadOnly),
            (CTX_MUTATION, McpTier::Mutation),
            (CTX_DESTRUCTIVE, McpTier::Destructive),
        ],
        overrides,
        cover,
    );
}

/// The tiers of the memory MCP, when it is wired.
fn icm_tiers(manifest: &MergedManifest, buckets: &mut PermBuckets<'_>, cover: &NativeCover<'_>) {
    let Some(icm) = manifest.mcps.iter().find(|m| m.name == MEMORY_MCP_NAME) else {
        return;
    };
    apply_mcp_tier_permissions(
        buckets,
        ICM_MCP_PREFIX,
        [
            (ICM_READ_ONLY, McpTier::ReadOnly),
            (ICM_MUTATION, McpTier::Mutation),
            (ICM_DESTRUCTIVE, McpTier::Destructive),
        ],
        icm.mcp_permissions.as_ref(),
        cover,
    );
}

/// The tiers of the codebase-memory MCP, when it is wired.
fn codebase_memory_tiers(
    manifest: &MergedManifest,
    buckets: &mut PermBuckets<'_>,
    cover: &NativeCover<'_>,
) {
    let name = crate::mcp::resolve::CODEBASE_MEMORY_MCP_NAME;
    let Some(cbm) = manifest.mcps.iter().find(|m| m.name == name) else {
        return;
    };
    apply_mcp_tier_permissions(
        buckets,
        CBM_MCP_PREFIX,
        [
            (CBM_READ_ONLY, McpTier::ReadOnly),
            (CBM_MUTATION, McpTier::Mutation),
            (CBM_DESTRUCTIVE, McpTier::Destructive),
        ],
        cbm.mcp_permissions.as_ref(),
        cover,
    );
}

/// Remove from a weaker array each rule that a stronger array holds (#1322). The suppression
/// above resolves neutral against native rules only, so a rule authored in both `allow` and
/// `deny` would land in both. This pass covers every source of the three arrays.
fn drop_overlaps(allow: &mut Vec<String>, ask: &mut Vec<String>, deny: &[String]) {
    let deny_set: BTreeSet<&str> = deny.iter().map(String::as_str).collect();
    ask.retain(|s| !deny_set.contains(s.as_str()));
    allow.retain(|s| !deny_set.contains(s.as_str()));
    let ask_set: BTreeSet<&str> = ask.iter().map(String::as_str).collect();
    allow.retain(|s| !ask_set.contains(s.as_str()));
}

/// The object for `settings.json`. All three arrays are emitted when any permission config
/// exists, so the shape matches the object schema of Claude Code even when one action is empty.
fn permissions_object(
    default_mode: Option<crate::config::PermissionMode>,
    allow: Vec<String>,
    ask: Vec<String>,
    deny: Vec<String>,
) -> Option<serde_json::Value> {
    let empty = allow.is_empty() && ask.is_empty() && deny.is_empty();
    if empty && default_mode.is_none() {
        return None;
    }
    let mut object = serde_json::Map::new();
    if let Some(mode) = default_mode {
        object.insert(
            "defaultMode".into(),
            serde_json::json!(permission_mode_str(mode)),
        );
    }
    object.insert("allow".into(), serde_json::json!(allow));
    object.insert("ask".into(), serde_json::json!(ask));
    object.insert("deny".into(), serde_json::json!(deny));
    Some(serde_json::Value::Object(object))
}
