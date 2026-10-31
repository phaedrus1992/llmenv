//! `llmenv setup --apply PLAN` (#2788, #2789): check an onboarding plan, ask for a yes for the
//! plan and for each gated entry, then create the absent files and add to `config.yaml`.
//! Design: docs/design/issue-2773-onboarding.md ("The plan file and `llmenv setup --apply`").

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, ensure};

use super::config_splice::splice;
use super::path_exists;
use super::plan::Plan;
use crate::config::Config;
use crate::merge::BundleRef;

/// The largest config.yaml that `--apply` reads, as for every other read of the config.
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// Asks the user a yes-or-no question. The terminal asks in production; a test answers.
pub(crate) trait Prompter {
    /// # Errors
    /// The answer cannot be read.
    fn confirm(&mut self, question: &str) -> anyhow::Result<bool>;
}

/// Check the plan at `plan_path` against the config in `config_dir`, ask for the yes answers,
/// and write. Returns true when it wrote files. A no to the plan or to the `config.yaml`
/// change writes nothing.
///
/// # Errors
/// The plan or the config cannot be read, the plan fails a check, a path to create exists, the
/// config changed during the run, or a write fails. A failed write removes what this run made.
pub(crate) fn apply(
    plan_path: &Path,
    config_dir: &Path,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
) -> anyhow::Result<bool> {
    let config_path = config_dir.join("config.yaml");
    let old = read_config(&config_path)?;
    let config = Config::load(&config_path)?;
    let mut plan = Plan::load(plan_path)?;
    let existing: BTreeSet<String> = config.bundle.iter().map(|b| b.name.clone()).collect();
    for name in plan.skip_existing(&existing, &config_dir.join("bundles"))? {
        writeln!(
            out,
            "Skipped bundle {name}: it exists, and llmenv never replaces a bundle."
        )?;
    }
    check_hosts(&plan, &config, config_dir)?;
    if plan.is_empty() {
        writeln!(out, "The plan adds nothing. Nothing was written.")?;
        return Ok(false);
    }
    check(&plan, &old, config_dir)?;
    print_plan(&plan, out)?;
    if !prompter.confirm("Apply this plan?")? {
        writeln!(out, "Nothing was written.")?;
        return Ok(false);
    }
    ask_items(&mut plan, prompter, out)?;
    if plan.is_empty() {
        writeln!(
            out,
            "Every entry got a no, so the plan adds nothing. Nothing was written."
        )?;
        return Ok(false);
    }
    check(&plan, &old, config_dir)?;
    let (new_text, diff) = splice(&old, &plan.additions())?;
    writeln!(out, "\nLines to add to {}:\n{diff}", config_path.display())?;
    if !prompter.confirm("Add these lines to config.yaml?")? {
        writeln!(
            out,
            "Nothing was written: a bundle that config.yaml does not list never fires."
        )?;
        return Ok(false);
    }
    write_all(&plan, config_dir, &old, &new_text, out)?;
    Ok(true)
}

fn read_config(path: &Path) -> anyhow::Result<String> {
    llmenv_paths::read_text_bounded(path, MAX_CONFIG_BYTES)
        .with_context(|| format!("reading {}", path.display()))?
        .with_context(|| {
            format!(
                "{} does not exist. Fix: run `llmenv setup` first, then apply the plan",
                path.display()
            )
        })
}

/// A plan host must be new: a host of the same name in config.yaml or in a bundle would point
/// an existing memory server somewhere else.
fn check_hosts(plan: &Plan, config: &Config, config_dir: &Path) -> anyhow::Result<()> {
    let mut known: BTreeSet<String> = config.host.keys().cloned().collect();
    for bundle in &config.bundle {
        let root = config_dir.join("bundles").join(&bundle.name);
        if let Some(caps) = crate::merge::read_bundle_yaml(&root, &bundle.name)? {
            known.extend(caps.host.into_keys());
        }
    }
    for name in plan.host_names() {
        ensure!(
            !known.contains(&name),
            "the plan adds host {name:?}, which config.yaml or a bundle already has. Fix: use the \
             existing host in the plan's features entry, and drop it from the plan's `host`"
        );
    }
    Ok(())
}

/// The changed config must load, and the merge of every bundle, old and new, must succeed.
/// Every bundle counts as firing, so a scalar collision between any two bundles shows now.
fn check(plan: &Plan, old: &str, config_dir: &Path) -> anyhow::Result<()> {
    let (new_text, _) = splice(old, &plan.additions())?;
    let tmp = tempfile::tempdir().context("making a folder to check the plan")?;
    std::fs::write(tmp.path().join("config.yaml"), &new_text).context("writing the check copy")?;
    for file in plan.files()? {
        let path = tmp.path().join(&file.rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("making the check copy")?;
        }
        std::fs::write(&path, file.text).context("writing the check copy")?;
    }
    let config = Config::load(&tmp.path().join("config.yaml"))
        .context("the config with the plan added does not load")?;
    let new: BTreeSet<String> = plan.bundle_names().into_iter().collect();
    let refs: Vec<BundleRef> = config
        .bundle
        .iter()
        .map(|b| {
            let root = if new.contains(&b.name) {
                tmp.path()
            } else {
                config_dir
            };
            BundleRef {
                name: b.name.clone(),
                path: root.join("bundles").join(&b.name),
                precedence: 1,
            }
        })
        .filter(|r| r.path.is_dir())
        .collect();
    crate::merge::merge(&config.capabilities, &config.native, &refs)
        .context("the bundles do not merge with the plan added")?;
    Ok(())
}

fn print_plan(plan: &Plan, out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(out, "The plan adds:")?;
    for line in plan.bundle_lines().into_iter().chain(plan.scope_lines()) {
        writeln!(out, "  {line}")?;
    }
    for file in plan.files()? {
        writeln!(out, "  file {}", file.rel.display())?;
    }
    let items = plan.gated_items()?.len();
    writeln!(
        out,
        "{items} {} a separate yes.",
        if items == 1 {
            "entry needs"
        } else {
            "entries need"
        }
    )?;
    Ok(())
}

/// Ask for each gated entry, and remove each one that gets a no.
fn ask_items(
    plan: &mut Plan,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let mut declined = BTreeSet::new();
    for (pos, (label, text)) in plan.gated_items()?.into_iter().enumerate() {
        writeln!(out, "\n{label}:")?;
        for line in text.lines() {
            writeln!(out, "    {line}")?;
        }
        if !prompter.confirm(&format!("Allow this {label}?"))? {
            declined.insert(pos);
        }
    }
    if !declined.is_empty() {
        let n = declined.len();
        writeln!(
            out,
            "\n{n} entr{} left out.",
            if n == 1 { "y is" } else { "ies are" }
        )?;
    }
    plan.remove_items(&declined)
}

/// Create the bundle folders and files, then write `config.yaml` last. The config is checked
/// first, so a change during the run stops before anything is made. A failure removes what this
/// run made, so a re-run of `--apply` starts from the same state.
fn write_all(
    plan: &Plan,
    config_dir: &Path,
    old: &str,
    new_text: &str,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let config_path = config_dir.join("config.yaml");
    ensure!(
        read_config(&config_path)? == old,
        "{} changed while the plan ran. Nothing was written. Fix: run `llmenv setup --apply` \
         again",
        config_path.display()
    );
    let mut made = Vec::new();
    let result = make_files(plan, config_dir, &mut made).and_then(|()| {
        llmenv_paths::write_user_file_atomic(&config_path, new_text.as_bytes())
            .with_context(|| format!("writing {}", config_path.display()))
    });
    if let Err(e) = result {
        return Err(e.context(undo(&made)));
    }
    for path in &made {
        writeln!(out, "Created {}", path.display())?;
    }
    writeln!(out, "Updated {}", config_path.display())?;
    Ok(())
}

/// Make each bundle folder and each folder in it with `create_dir`, which fails on anything
/// already at the path, and each file with `create_new`. Each path made is pushed to `made`.
fn make_files(plan: &Plan, config_dir: &Path, made: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let bundles_dir = config_dir.join("bundles");
    if !path_exists(&bundles_dir)? {
        make_dir(&bundles_dir, made)?;
    }
    for name in plan.bundle_names() {
        make_dir(&config_dir.join("bundles").join(name), made)?;
    }
    for file in plan.files()? {
        let path = config_dir.join(&file.rel);
        for folder in new_folders(&file.rel) {
            let dir = config_dir.join(folder);
            if !made.contains(&dir) {
                make_dir(&dir, made)?;
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        if file.executable {
            set_executable(&mut options);
        }
        let mut handle = options
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        made.push(path.clone());
        handle
            .write_all(file.text.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// The folders between the bundle folder and the file at `rel`, outermost first. `rel` starts
/// with `bundles/<name>`, and that folder is made before any file.
fn new_folders(rel: &Path) -> Vec<&Path> {
    let bundle_dir: PathBuf = rel.iter().take(2).collect();
    let mut folders: Vec<&Path> = rel
        .ancestors()
        .skip(1)
        .take_while(|a| *a != bundle_dir)
        .collect();
    folders.reverse();
    folders
}

#[cfg(unix)]
fn set_executable(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.mode(0o755);
}

/// A file off Unix has no executable bit, so there is nothing to set.
#[cfg(not(unix))]
fn set_executable(_options: &mut std::fs::OpenOptions) {}

fn make_dir(dir: &Path, made: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    std::fs::create_dir(dir).with_context(|| {
        format!(
            "making {}; llmenv never writes into a folder it did not make",
            dir.display()
        )
    })?;
    made.push(dir.to_path_buf());
    Ok(())
}

/// Remove what this run made, newest first, and say what happened.
fn undo(made: &[PathBuf]) -> String {
    let mut left = Vec::new();
    for path in made.iter().rev() {
        // Judge the entry itself. `is_dir()` follows a link, and `remove_dir` refuses a link, so a
        // link to a folder would stay in place (#3116).
        let removed = match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir(path),
            _ => std::fs::remove_file(path),
        };
        if let Err(e) = removed
            && path_exists(path).unwrap_or(true)
        {
            left.push(format!("{} ({e})", path.display()));
        }
    }
    if left.is_empty() {
        "the apply failed, and the files it made are removed; config.yaml is unchanged".to_string()
    } else {
        format!(
            "the apply failed, and these paths it made could not be removed: {}. Fix: remove \
             them, then run `llmenv setup --apply` again",
            left.join(", ")
        )
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests;
