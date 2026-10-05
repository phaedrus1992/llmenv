//! The roots codebase-memory-mcp may index (#2406).
//!
//! codebase-memory-mcp 0.11.0 records allowed roots in `<cache dir>/allowed_roots`. It writes the
//! file through `codebase-memory-mcp allow-root <path>` and lists it with `allow-root --list`.
//! llmenv never edits the file. It runs those commands with the `CBM_CACHE_DIR` the server gets,
//! so the roots land where the server reads them. Design: docs/design/issue-2406-cbm-allowed-roots.md

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use llmenv_config::{CodebaseMemory, Config};

/// How long one `codebase-memory-mcp allow-root` call may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// The directories that the default roots come from.
#[derive(Debug, Clone)]
pub struct RootBases {
    project_root: PathBuf,
    config_dir: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
    pub home: Option<PathBuf>,
}

impl RootBases {
    /// The directories of this machine and `config`.
    ///
    /// # Errors
    /// The llmenv config or state directory cannot be resolved.
    pub fn from_config(config: &Config, project_root: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            project_root: project_root.to_path_buf(),
            config_dir: llmenv_paths::config_dir()?,
            cache_dir: PathBuf::from(llmenv_paths::expand_tilde(&config.cache.cache_dir)),
            state_dir: llmenv_paths::state_dir()?,
            home: std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from),
        })
    }
}

/// Replace `~` and `$VAR` or `${VAR}` in `entry`. `None` when a variable is unset or empty, or
/// when `~` needs a home directory that is not known.
fn expand_root(
    entry: &str,
    home: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    let entry = entry.trim();
    let mut out = String::new();
    let rest = if let Some(after) = entry.strip_prefix('~') {
        if !(after.is_empty() || after.starts_with('/')) {
            // `~user` is not supported.
            return None;
        }
        out.push_str(&home?.to_string_lossy());
        after
    } else {
        entry
    };
    out.push_str(&llmenv_util::expand_env_refs(rest, env)?);
    Some(PathBuf::from(out))
}

/// Whether `path` is too broad to allow as the project folder: the filesystem root, the home
/// folder, or a parent of it. The server keeps an allowed root for good, so a session started in
/// such a folder would leave every project below it open to `index_repository`.
fn is_broad_root(path: &Path, home: Option<&Path>) -> bool {
    path.parent().is_none() || home.is_some_and(|h| h.starts_with(path))
}

/// The default roots, then the roots of `cm`, without duplicates (#2406). An entry whose variable
/// is unset is dropped, and so is a project folder that is too broad ([`is_broad_root`]).
fn resolve_allowed_roots_in(
    cm: &CodebaseMemory,
    bases: &RootBases,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    let nbl_diag = env("NBL_DIAG_CACHE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| bases.home.as_ref().map(|h| h.join(".cache/nbl-diag")))
        .map(|dir| dir.join("repos"));
    let defaults = [
        (!is_broad_root(&bases.project_root, bases.home.as_deref()))
            .then(|| bases.project_root.clone()),
        Some(bases.config_dir.clone()),
        Some(bases.cache_dir.clone()),
        Some(bases.state_dir.clone()),
        nbl_diag,
    ];
    let configured = cm
        .allowed_roots
        .iter()
        .filter_map(|entry| expand_root(entry, bases.home.as_deref(), env));
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in defaults.into_iter().flatten().chain(configured) {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// [`resolve_allowed_roots_in`] with the environment of this process.
pub fn resolve_allowed_roots(cm: &CodebaseMemory, bases: &RootBases) -> Vec<PathBuf> {
    resolve_allowed_roots_in(cm, bases, &|name| std::env::var(name).ok())
}

/// The cache folder codebase-memory-mcp serves: `index_path`, or its own default.
fn cache_dir(cm: &CodebaseMemory, home: Option<&Path>) -> PathBuf {
    cm.index_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home.map_or_else(
                || PathBuf::from("~/.cache/codebase-memory-mcp"),
                |h| h.join(".cache/codebase-memory-mcp"),
            )
        })
}

/// A `codebase-memory-mcp` command with the environment of the server llmenv launches.
fn cbm_command(cm: &CodebaseMemory, args: &[&str]) -> Command {
    let mut cmd = Command::new("codebase-memory-mcp");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(index_path) = &cm.index_path {
        cmd.env("CBM_CACHE_DIR", index_path);
    }
    cmd
}

/// Run `cmd` and wait at most `timeout`. The child is killed on a timeout.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::io::Result<Output> {
    let mut child = cmd.spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no answer within {} ms", timeout.as_millis()),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The roots in the output of `allow-root --list`.
fn parse_roots(listing: &str) -> Vec<PathBuf> {
    listing
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

/// The roots that codebase-memory-mcp has recorded for the cache folder of `cm`.
///
/// # Errors
/// The command cannot run, times out, or exits non-zero.
fn list_roots(cm: &CodebaseMemory) -> anyhow::Result<Vec<PathBuf>> {
    let output = run_with_timeout(cbm_command(cm, &["allow-root", "--list"]), COMMAND_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("codebase-memory-mcp allow-root --list: {e}"))?;
    anyhow::ensure!(
        output.status.success(),
        "codebase-memory-mcp allow-root --list exited with {}",
        output.status
    );
    Ok(parse_roots(&String::from_utf8_lossy(&output.stdout)))
}

/// What the server allows, compared with what llmenv wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootsReport {
    /// The roots the server has recorded.
    listed: Vec<PathBuf>,
    /// The wanted roots that exist and are not recorded, with the reason.
    missing: Vec<(PathBuf, String)>,
    cache_dir: PathBuf,
}

/// The wanted roots that the server has not recorded. A root that does not exist cannot be
/// recorded, so it is left out: a default such as the code-explorer cache is often absent.
fn missing_roots(wanted: &[PathBuf], listed: &[PathBuf]) -> Vec<PathBuf> {
    wanted
        .iter()
        .filter(|root| root.is_dir() && !listed.iter().any(|l| same_path(l, root)))
        .cloned()
        .collect()
}

/// Whether two paths name one folder: equal, or equal after resolving links.
fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// Read-only: compare the wanted roots with what the server has recorded.
///
/// # Errors
/// The list cannot be read (see [`list_roots`]).
pub fn check_roots(
    cm: &CodebaseMemory,
    wanted: &[PathBuf],
    home: Option<&Path>,
) -> anyhow::Result<RootsReport> {
    let listed = list_roots(cm)?;
    let missing = missing_roots(wanted, &listed)
        .into_iter()
        .map(|root| (root, "not in the allowed roots".to_string()))
        .collect();
    Ok(RootsReport {
        listed,
        missing,
        cache_dir: cache_dir(cm, home),
    })
}

/// Record the wanted roots that the server lacks, through its own command, and report what is
/// still missing. A failed command is a missing root with the command's message.
///
/// # Errors
/// The list cannot be read (see [`list_roots`]).
fn apply_roots(
    cm: &CodebaseMemory,
    wanted: &[PathBuf],
    home: Option<&Path>,
) -> anyhow::Result<RootsReport> {
    let mut report = check_roots(cm, wanted, home)?;
    let mut still_missing = Vec::new();
    for (root, _) in std::mem::take(&mut report.missing) {
        let path = root.to_string_lossy().into_owned();
        let outcome = run_with_timeout(cbm_command(cm, &["allow-root", &path]), COMMAND_TIMEOUT);
        match outcome {
            Ok(out) if out.status.success() => report.listed.push(root),
            Ok(out) => {
                let text = String::from_utf8_lossy(if out.stderr.is_empty() {
                    &out.stdout
                } else {
                    &out.stderr
                })
                .into_owned();
                let reason = crate::stdio_rpc::tidy_reason(&text);
                tracing::warn!("codebase-memory-mcp allow-root {path} failed: {reason}");
                still_missing.push((root, reason));
            }
            Err(e) => {
                tracing::warn!("codebase-memory-mcp allow-root {path} failed: {e}");
                still_missing.push((root, e.to_string()));
            }
        }
    }
    report.missing = still_missing;
    Ok(report)
}

/// The text that tells the user what `report` found. `warn` is true when a root is missing.
pub fn describe(report: &RootsReport) -> (bool, String) {
    let list = |roots: &[PathBuf]| {
        roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if report.missing.is_empty() {
        return (
            false,
            format!(
                "codebase-memory: allowed roots {} (cache {})",
                list(&report.listed),
                report.cache_dir.display()
            ),
        );
    }
    let missing: Vec<String> = report
        .missing
        .iter()
        .map(|(root, why)| format!("{} ({why})", root.display()))
        .collect();
    (
        true,
        format!(
            "codebase-memory cannot index {}. Add the folder to \
             features.codebase_memory[].allowed_roots in the llmenv config and run llmenv \
             regenerate; do not run allow-root by hand. The server reads {} (index_path sets it).",
            missing.join(", "),
            report.cache_dir.display()
        ),
    )
}

/// The `repo_path` of an `index_repository` call that is outside the allowed roots, as the
/// reason to deny it. `None` when it is inside one. Both sides must be canonical, so a link into
/// a root passes and a `..` escape does not (#2406).
fn path_outside_roots(repo_path: &Path, roots: &[PathBuf]) -> Option<String> {
    if roots.iter().any(|root| repo_path.starts_with(root)) {
        return None;
    }
    let list = roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "{} is outside the codebase-memory allowed roots ({list}). Add it to \
         features.codebase_memory[].allowed_roots in llmenv config and run llmenv regenerate; \
         do not run allow-root by hand.",
        repo_path.display()
    ))
}

/// The problems in the roots that the user wrote or started a session in, as lines to show:
/// an entry that cannot expand (an unset variable or `~user`), an entry that is not a folder, and
/// a project folder that is too broad to allow. A default root may be absent, but a root the
/// user wrote is a typo or a missing mount.
pub fn root_problems(cm: &CodebaseMemory, bases: &RootBases) -> Vec<String> {
    root_problems_in(cm, bases, &|name| std::env::var(name).ok())
}

fn root_problems_in(
    cm: &CodebaseMemory,
    bases: &RootBases,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut unexpandable = Vec::new();
    let mut absent = Vec::new();
    for entry in &cm.allowed_roots {
        match expand_root(entry, bases.home.as_deref(), env) {
            None => unexpandable.push(entry.as_str()),
            Some(root) if root.is_absolute() && !root.is_dir() => {
                absent.push(root.display().to_string());
            }
            Some(_) => {}
        }
    }
    if !unexpandable.is_empty() {
        problems.push(format!(
            "codebase-memory: allowed_roots entries that cannot expand (an unset or empty \
             variable, or ~user): {}. Fix the entry in the llmenv config.",
            unexpandable.join(", ")
        ));
    }
    if !absent.is_empty() {
        problems.push(format!(
            "codebase-memory: allowed_roots entries that are not folders: {}. Fix the entry in \
             the llmenv config.",
            absent.join(", ")
        ));
    }
    if is_broad_root(&bases.project_root, bases.home.as_deref()) {
        problems.push(format!(
            "codebase-memory: the session folder {} is too broad to allow, so it is not an \
             allowed root. Start the session in a project folder, or add the folders to index \
             to allowed_roots.",
            bases.project_root.display()
        ));
    }
    problems
}

/// Apply the roots at `SessionStart`. Returns the text to show the user when a root is missing
/// or the server cannot be asked, and `None` when all is well (#2406).
pub fn session_start_notice(
    config: &Config,
    cm: &CodebaseMemory,
    project_root: &Path,
) -> Option<String> {
    let bases = match RootBases::from_config(config, project_root) {
        Ok(bases) => bases,
        Err(e) => return Some(format!("codebase-memory: cannot resolve the roots: {e}\n")),
    };
    let wanted = resolve_allowed_roots(cm, &bases);
    let problems = root_problems(cm, &bases);
    match apply_roots(cm, &wanted, bases.home.as_deref()) {
        Ok(report) => {
            let (warn, text) = describe(&report);
            let text = [warn.then_some(text)]
                .into_iter()
                .flatten()
                .chain(problems)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then(|| format!("{text}\n"))
        }
        Err(e) => {
            tracing::warn!(error = %e, "codebase-memory roots could not be applied");
            Some(format!(
                "codebase-memory: cannot apply the allowed roots: {e}. Run `llmenv doctor`.\n"
            ))
        }
    }
}

/// The reason to deny an `index_repository` call, or `None` to let it pass (#2406). The allowed
/// set is the configured roots plus what the server lists, so a root that an earlier run
/// recorded still counts.
pub fn guard_decision(repo_path: &str, config: &Config, project_root: &Path) -> Option<String> {
    let entries = config
        .features
        .as_ref()
        .map(|f| f.codebase_memory.as_slice())?;
    if entries.is_empty() {
        return None;
    }
    // This guard denies an `index_repository` call, so a failure to read its inputs denies too.
    let bases = match RootBases::from_config(config, project_root) {
        Ok(bases) => bases,
        Err(e) => {
            return Some(format!(
                "cannot check {repo_path} against the codebase-memory allowed roots: {e:#}. Run \
                 `llmenv doctor`."
            ));
        }
    };
    let mut listed = Vec::new();
    let mut unreadable = Vec::new();
    for cm in entries {
        match list_roots(cm) {
            Ok(roots) => listed.extend(roots),
            Err(e) => unreadable.push(format!("{e:#}")),
        }
    }
    decide(repo_path, entries, &bases, listed)
        .map(|reason| with_unreadable_note(reason, &unreadable))
}

/// `reason` with the failures to read the server's recorded roots, so a deny that came from a
/// failed read does not look like a root the user forgot to add.
fn with_unreadable_note(reason: String, unreadable: &[String]) -> String {
    if unreadable.is_empty() {
        return reason;
    }
    format!(
        "{reason} The roots that the server recorded could not be read: {}.",
        unreadable.join("; ")
    )
}

/// [`guard_decision`] with the bases and the listed roots given. A `repo_path` that does not
/// exist is denied: the server cannot index it, and no canonical path exists to compare.
fn decide(
    repo_path: &str,
    entries: &[CodebaseMemory],
    bases: &RootBases,
    listed: Vec<PathBuf>,
) -> Option<String> {
    let mut roots = listed;
    for cm in entries {
        roots.extend(resolve_allowed_roots(cm, bases));
    }
    let roots: Vec<PathBuf> = roots
        .into_iter()
        .filter_map(|root| std::fs::canonicalize(&root).ok())
        .collect();
    match std::fs::canonicalize(repo_path) {
        Ok(canonical) => path_outside_roots(&canonical, &roots),
        Err(e) => Some(format!(
            "{repo_path} cannot be indexed: it does not resolve to a folder ({e}). Pass the \
             path of an existing repository."
        )),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn bases() -> RootBases {
        RootBases {
            project_root: "/work/proj".into(),
            config_dir: "/home/u/.config/llmenv".into(),
            cache_dir: "/home/u/.cache/llmenv".into(),
            state_dir: "/home/u/.cache/llmenv/state".into(),
            home: Some("/home/u".into()),
        }
    }

    fn cm(roots: &[&str]) -> CodebaseMemory {
        CodebaseMemory {
            when: vec!["p".into()],
            allowed_roots: roots.iter().map(|r| (*r).to_string()).collect(),
            ..Default::default()
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn strings(roots: Vec<PathBuf>) -> Vec<String> {
        roots.into_iter().map(|r| r.display().to_string()).collect()
    }

    #[test]
    fn the_defaults_come_first_in_order() {
        let roots = strings(resolve_allowed_roots_in(&cm(&[]), &bases(), &no_env));
        assert_eq!(
            roots,
            [
                "/work/proj",
                "/home/u/.config/llmenv",
                "/home/u/.cache/llmenv",
                "/home/u/.cache/llmenv/state",
                "/home/u/.cache/nbl-diag/repos",
            ]
        );
    }

    #[test]
    fn nbl_diag_cache_overrides_the_default_and_user_roots_are_appended() {
        let env = |name: &str| (name == "NBL_DIAG_CACHE").then(|| "/data/diag".to_string());
        let roots = strings(resolve_allowed_roots_in(
            &cm(&["/srv/extra", "~/notes"]),
            &bases(),
            &env,
        ));
        assert_eq!(roots[4], "/data/diag/repos");
        assert_eq!(&roots[5..], ["/srv/extra", "/home/u/notes"]);
    }

    #[test]
    fn variables_expand_and_an_unset_one_drops_the_entry() {
        let env = |name: &str| (name == "WORK").then(|| "/mnt/work".to_string());
        let roots = strings(resolve_allowed_roots_in(
            &cm(&[
                "$WORK/a",
                "${WORK}/b",
                "$MISSING/c",
                "${MISSING}",
                "~other/x",
            ]),
            &bases(),
            &env,
        ));
        assert_eq!(&roots[5..], ["/mnt/work/a", "/mnt/work/b"]);
    }

    #[test]
    fn duplicates_are_removed_and_an_empty_variable_counts_as_unset() {
        let env = |name: &str| (name == "EMPTY").then(String::new);
        let roots = strings(resolve_allowed_roots_in(
            &cm(&["/work/proj", "/srv/x", "/srv/x", "$EMPTY/y"]),
            &bases(),
            &env,
        ));
        assert_eq!(roots.iter().filter(|r| *r == "/work/proj").count(), 1);
        assert_eq!(roots.iter().filter(|r| *r == "/srv/x").count(), 1);
        assert_eq!(roots.len(), 6);
    }

    #[test]
    fn without_a_home_a_tilde_entry_and_the_nbl_diag_default_are_dropped() {
        let mut b = bases();
        b.home = None;
        let roots = strings(resolve_allowed_roots_in(&cm(&["~/x"]), &b, &no_env));
        assert_eq!(roots.len(), 4);
    }

    #[test]
    fn a_broad_project_folder_is_not_an_allowed_root() {
        for project in ["/", "/home", "/home/u"] {
            let mut b = bases();
            b.project_root = project.into();
            let roots = strings(resolve_allowed_roots_in(&cm(&[]), &b, &no_env));
            assert!(!roots.iter().any(|r| r == project), "{project}: {roots:?}");
            let problems = root_problems_in(&cm(&[]), &b, &no_env);
            assert!(
                problems.iter().any(|p| p.contains("too broad")),
                "{project}: {problems:?}"
            );
        }
        let roots = strings(resolve_allowed_roots_in(&cm(&[]), &bases(), &no_env));
        assert_eq!(roots[0], "/work/proj");
        assert!(root_problems_in(&cm(&[]), &bases(), &no_env).is_empty());
    }

    #[test]
    fn an_entry_that_cannot_expand_is_reported_not_dropped_silently() {
        let problems =
            root_problems_in(&cm(&["$MISSING/c", "~other/x", "/srv"]), &bases(), &no_env);
        let text = problems.join("\n");
        assert!(
            text.contains("$MISSING/c") && text.contains("~other/x"),
            "{text}"
        );
        assert!(text.contains("cannot expand"), "{text}");
    }

    #[test]
    fn an_entry_that_is_not_a_folder_is_reported() {
        let problems = root_problems_in(&cm(&["/no/such/folder/x"]), &bases(), &no_env);
        assert!(
            problems.iter().any(|p| p.contains("/no/such/folder/x")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_deny_names_a_failed_read_of_the_recorded_roots() {
        assert_eq!(with_unreadable_note("denied.".into(), &[]), "denied.");
        let text =
            with_unreadable_note("denied.".into(), &["timed out".into(), "no binary".into()]);
        assert!(text.starts_with("denied. "), "{text}");
        assert!(text.contains("timed out; no binary"), "{text}");
    }

    #[test]
    fn the_listing_is_read_after_its_header() {
        let listing = "allowed roots:\n/Users/u/git\n/Users/u/.cache/nbl-diag/repos\n";
        assert_eq!(
            strings(parse_roots(listing)),
            ["/Users/u/git", "/Users/u/.cache/nbl-diag/repos"]
        );
        assert!(parse_roots("no allowed roots recorded \n").is_empty());
        assert!(parse_roots("").is_empty());
    }

    #[test]
    fn only_existing_unrecorded_roots_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (have, lack, gone) = (
            dir.path().join("have"),
            dir.path().join("lack"),
            dir.path().join("gone"),
        );
        std::fs::create_dir(&have).unwrap();
        std::fs::create_dir(&lack).unwrap();
        let wanted = vec![have.clone(), lack.clone(), gone];
        assert_eq!(missing_roots(&wanted, std::slice::from_ref(&have)), [lack]);
    }

    #[test]
    fn a_recorded_root_counts_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(same_path(&real, &link));
        assert!(!same_path(&real, &dir.path().join("other")));
        assert!(missing_roots(&[link], &[real]).is_empty());
    }

    #[test]
    fn the_description_lists_the_roots_or_the_missing_ones_with_the_fix() {
        let ok = RootsReport {
            listed: vec!["/a".into(), "/b".into()],
            missing: vec![],
            cache_dir: "/cache".into(),
        };
        assert_eq!(
            describe(&ok),
            (
                false,
                "codebase-memory: allowed roots /a, /b (cache /cache)".to_string()
            )
        );
        let bad = RootsReport {
            missing: vec![("/c".into(), "refused".to_string())],
            ..ok
        };
        let (warn, text) = describe(&bad);
        assert!(warn);
        assert!(
            text.contains("/c (refused)") && text.contains("allowed_roots"),
            "{text}"
        );
        assert!(
            text.contains("do not run allow-root by hand") && text.contains("/cache"),
            "{text}"
        );
    }

    #[test]
    fn the_cache_dir_is_the_index_path_or_the_default() {
        let mut c = cm(&[]);
        assert_eq!(
            cache_dir(&c, Some(Path::new("/home/u"))),
            Path::new("/home/u/.cache/codebase-memory-mcp")
        );
        c.index_path = Some("/idx".into());
        assert_eq!(cache_dir(&c, None), Path::new("/idx"));
    }

    #[test]
    fn the_command_gets_the_cache_dir_of_the_server() {
        let env_of = |c: &CodebaseMemory| {
            cbm_command(c, &["allow-root", "--list"])
                .get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new("CBM_CACHE_DIR"))
                .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        let mut c = cm(&[]);
        assert_eq!(env_of(&c), None);
        c.index_path = Some("/idx".into());
        assert_eq!(env_of(&c).as_deref(), Some("/idx"));
    }

    #[test]
    fn a_command_that_runs_too_long_is_killed() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdout(Stdio::piped()).stderr(Stdio::piped());
        let err = run_with_timeout(cmd, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        let mut quick = Command::new("echo");
        quick
            .arg("hi")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = run_with_timeout(quick, Duration::from_secs(5)).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[test]
    fn a_path_is_inside_a_root_or_the_reason_names_the_fix() {
        let roots = vec![PathBuf::from("/work/proj"), PathBuf::from("/srv/x")];
        assert_eq!(path_outside_roots(Path::new("/work/proj"), &roots), None);
        assert_eq!(
            path_outside_roots(Path::new("/work/proj/sub/dir"), &roots),
            None
        );
        let reason = path_outside_roots(Path::new("/work/projx"), &roots).unwrap();
        assert!(
            reason.contains("/work/projx") && reason.contains("/work/proj, /srv/x"),
            "{reason}"
        );
        assert!(
            reason.contains("allowed_roots") && reason.contains("llmenv regenerate"),
            "{reason}"
        );
        assert!(path_outside_roots(Path::new("/tmp/elsewhere"), &roots).is_some());
        assert!(path_outside_roots(Path::new("/tmp"), &[]).is_some());
    }

    fn temp_bases(base: &Path) -> RootBases {
        RootBases {
            project_root: base.join("project"),
            config_dir: base.join("config"),
            cache_dir: base.join("cache"),
            state_dir: base.join("state"),
            home: Some(base.join("home")),
        }
    }

    #[test]
    fn the_guard_passes_a_repo_in_a_root_and_denies_others() {
        let base = tempfile::tempdir().unwrap();
        let inside = base.path().join("inside");
        let repo = inside.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let entries = [cm(&[inside.to_str().unwrap()])];
        let bases = temp_bases(base.path());
        assert_eq!(
            decide(repo.to_str().unwrap(), &entries, &bases, vec![]),
            None
        );
        let reason = decide(outside.to_str().unwrap(), &entries, &bases, vec![]).unwrap();
        assert!(reason.contains("allowed_roots"), "{reason}");
    }

    #[test]
    fn a_root_that_the_server_lists_counts_for_the_guard() {
        let base = tempfile::tempdir().unwrap();
        let listed = base.path().join("listed");
        std::fs::create_dir(&listed).unwrap();
        let bases = temp_bases(base.path());
        let path = listed.to_str().unwrap();
        assert!(decide(path, &[cm(&[])], &bases, vec![]).is_some());
        assert_eq!(decide(path, &[cm(&[])], &bases, vec![listed.clone()]), None);
    }

    #[test]
    fn the_guard_denies_a_path_that_does_not_exist_and_a_dot_dot_escape() {
        let base = tempfile::tempdir().unwrap();
        let inside = base.path().join("inside");
        std::fs::create_dir(&inside).unwrap();
        let entries = [cm(&[inside.to_str().unwrap()])];
        let bases = temp_bases(base.path());
        let missing = inside.join("gone");
        let reason = decide(missing.to_str().unwrap(), &entries, &bases, vec![]).unwrap();
        assert!(reason.contains("does not resolve"), "{reason}");
        let escape = inside.join("..");
        assert!(decide(escape.to_str().unwrap(), &entries, &bases, vec![]).is_some());
    }

    #[test]
    fn a_link_out_of_a_root_is_denied_and_a_link_into_one_passes() {
        let base = tempfile::tempdir().unwrap();
        let inside = base.path().join("inside");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let out_link = inside.join("out");
        std::os::unix::fs::symlink(&outside, &out_link).unwrap();
        let in_link = outside.join("in");
        std::os::unix::fs::symlink(&inside, &in_link).unwrap();
        let entries = [cm(&[inside.to_str().unwrap()])];
        let bases = temp_bases(base.path());
        assert!(decide(out_link.to_str().unwrap(), &entries, &bases, vec![]).is_some());
        assert_eq!(
            decide(in_link.to_str().unwrap(), &entries, &bases, vec![]),
            None
        );
    }

    #[test]
    fn the_guard_does_not_apply_without_a_codebase_memory_entry() {
        let project = tempfile::tempdir().unwrap();
        assert_eq!(
            guard_decision("/anywhere", &Config::default(), project.path()),
            None
        );
    }

    proptest! {
        #[test]
        fn expanding_never_panics_and_leaves_no_dollar(
            entry in "\\PC{0,40}",
        ) {
            let env = |name: &str| Some(format!("/v/{name}"));
            if let Some(path) = expand_root(&entry, Some(Path::new("/home/u")), &env) {
                let text = path.to_string_lossy().into_owned();
                prop_assert!(!text.contains('$'), "{text}");
            }
        }

        #[test]
        fn every_parsed_root_is_an_absolute_line_of_the_input(listing in "(\\PC{0,30}\n){0,6}") {
            for root in parse_roots(&listing) {
                prop_assert!(root.is_absolute());
                prop_assert!(listing.lines().any(|l| l.trim() == root.to_string_lossy()));
            }
        }

        #[test]
        fn a_path_under_any_root_is_inside_and_a_path_under_none_is_outside(
            roots in prop::collection::vec("/[a-z]{1,6}(/[a-z]{1,6}){0,2}", 1..4),
            pick in 0usize..4,
            tail in "(/[a-z]{1,6}){0,3}",
            other in "/zz[0-9]{1,4}(/[a-z]{1,6}){0,2}",
        ) {
            let roots: Vec<PathBuf> = roots.into_iter().map(PathBuf::from).collect();
            let inside = PathBuf::from(format!("{}{tail}", roots[pick % roots.len()].display()));
            prop_assert_eq!(path_outside_roots(&inside, &roots), None);
            prop_assert!(path_outside_roots(Path::new(&other), &roots).is_some());
        }

        #[test]
        fn resolving_never_repeats_a_root_and_keeps_the_defaults_first(
            extra in prop::collection::vec("/[a-z]{1,5}(/[a-z]{1,5}){0,2}", 0..5),
        ) {
            let c = cm(&extra.iter().map(String::as_str).collect::<Vec<_>>());
            let roots = resolve_allowed_roots_in(&c, &bases(), &no_env);
            let unique: std::collections::BTreeSet<_> = roots.iter().collect();
            prop_assert_eq!(unique.len(), roots.len());
            prop_assert_eq!(roots[0].as_path(), Path::new("/work/proj"));
        }
    }
}
