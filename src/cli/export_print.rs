//! The `export` lines that `llmenv export` prints.

use anyhow::Context as _;
use std::collections::BTreeMap;

use super::{validate_var_name, validate_var_value};

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

#[cfg(test)]
mod tests;
