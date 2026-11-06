//! The `export` lines that `llmenv export` prints.

use anyhow::Context as _;
use std::collections::{BTreeMap, BTreeSet};

use super::env_vars::{validate_var_name, validate_var_value};
use super::{firing_bundles, marker_disabled_bundle_names};
use crate::config::{Bundle, Config};
use crate::scope::ActiveScopes;
use crate::scope::matcher::Env;

/// The `export` lines for `vars`. Every variable is validated before any line is returned, so one
/// bad value fails the whole export and a shell never sources part of it (#543). With
/// `bundle_list`, each line first names its source, so an explained export shows which bundles
/// fired.
pub(super) fn export_lines(
    vars: &BTreeMap<String, String>,
    bundle_list: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut lines = Vec::with_capacity(vars.len() * 2);
    for (key, value) in vars {
        validate_var_name(key).with_context(|| format!("variable '{key}'"))?;
        validate_var_value(value).with_context(|| format!("variable '{key}': invalid value"))?;
        if let Some(bundle_list) = bundle_list {
            if key.starts_with("LLMENV_") {
                lines.push("# source: llmenv introspection".to_string());
            } else {
                lines.push(format!("# source: adapter (bundles: {bundle_list})"));
            }
        }
        lines.push(format!(
            "export {}={}",
            key,
            llmenv_util::shell::single_quote(value)
        ));
    }
    Ok(lines)
}

/// Print the `export` lines for `vars`. Nothing prints unless every variable is valid.
pub(super) fn print_exports(
    vars: &BTreeMap<String, String>,
    bundle_list: Option<&str>,
) -> anyhow::Result<()> {
    for line in export_lines(vars, bundle_list)? {
        println!("{line}");
    }
    Ok(())
}

/// The bundles that fire, and the scopes that the materialize step uses. With `--scope`, only that
/// scope's tags and `enable_bundles` count, because `--scope` narrows the export. A bundle that any
/// active scope disables stays off, because narrowing never turns a bundle on (#3269). An unknown
/// scope id fires nothing, and a warning says so (#1399): `launch` injects the result into an
/// agent, so a typo must not hand it every active scope.
pub(super) fn select_firing<'a>(
    config: &'a Config,
    active: &ActiveScopes,
    env: &Env,
    scope: Option<&str>,
    tag: Option<&str>,
) -> (Vec<&'a Bundle>, ActiveScopes) {
    let Some(scope_id) = scope else {
        let firing = firing_bundles(&config.bundle, active, tag);
        return (firing, active.clone());
    };
    // The narrowed scopes carry every active disable, so the firing set and the cache shape,
    // which both read `disable_bundles`, agree.
    let mut all_disabled: Vec<String> = marker_disabled_bundle_names(active).into_iter().collect();
    all_disabled.sort();
    let filtered_scopes = active
        .scopes
        .iter()
        .filter(|s| s.id == scope_id)
        .map(|s| {
            let mut s = s.clone();
            s.disable_bundles = all_disabled.clone();
            s
        })
        .collect::<Vec<_>>();
    if filtered_scopes.is_empty() {
        eprintln!(
            "warning: scope '{scope_id}' not active in current environment \
             — no bundles will fire for this scope"
        );
        // Fully empty, including `extra_tags`, so "nothing fired" is the one shape of a miss.
        let empty = ActiveScopes::default();
        let firing = firing_bundles(&config.bundle, &empty, tag);
        return (firing, empty);
    }
    let mut filtered_tags: BTreeSet<String> = filtered_scopes
        .iter()
        .flat_map(|s| s.tags.iter().cloned())
        .collect();
    if !env.os.is_empty() {
        filtered_tags.insert(env.os.clone());
    }
    filtered_tags.extend(active.extra_tags.iter().cloned());
    let filtered = ActiveScopes {
        scopes: filtered_scopes,
        tags: filtered_tags,
        extra_tags: active.extra_tags.clone(),
    };
    let firing = firing_bundles(&config.bundle, &filtered, tag);
    (firing, filtered)
}

#[cfg(test)]
mod tests;
