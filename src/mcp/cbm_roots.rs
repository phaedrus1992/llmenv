//! The roots codebase-memory-mcp may index (#2406).
//!
//! codebase-memory-mcp 0.11.0 records allowed roots in `<cache dir>/allowed_roots`. Only the user
//! adds a root: with `codebase-memory-mcp allow-root <path>`, which the user types. llmenv never
//! runs that command and never edits the file (#2916). It reads the roots with `allow-root --list`,
//! reports what the config declares and the server lacks, and names the command for the user.
//! Design: docs/design/issue-2406-cbm-allowed-roots.md

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::CodebaseMemory;

mod declared;
mod guard;
pub(crate) use guard::guard_decision;
mod report_text;
pub(crate) use declared::{configured_roots, root_problems};
pub(crate) use report_text::describe;

/// How long one `codebase-memory-mcp allow-root --list` call may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// The inputs that the roots of a session need. The defaults are not among them: a folder that
/// llmenv creates or uses is an allowed root only when the config declares it (#2916).
#[derive(Debug, Clone)]
pub(crate) struct RootBases {
    pub project_root: PathBuf,
    pub home: Option<PathBuf>,
}

impl RootBases {
    /// The session folder and the home folder of this process.
    pub(crate) fn for_project(project_root: &Path) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            home: std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from),
        }
    }
}

/// The cache folder codebase-memory-mcp serves: `index_path`, or its own default.
///
/// # Errors
/// No `index_path` is set and `HOME` is unknown. A literal `~` would be
/// read as a directory name, so the caller must set `index_path` instead.
fn cache_dir(cm: &CodebaseMemory, home: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(index_path) = &cm.index_path {
        return Ok(PathBuf::from(crate::paths::expand_tilde(index_path)?));
    }
    let home = home.ok_or_else(|| {
        anyhow::anyhow!(
            "cannot locate the codebase-memory cache: HOME is not set. \
             Set features.codebase_memory index_path to a directory."
        )
    })?;
    Ok(home.join(".cache/codebase-memory-mcp"))
}

/// A `codebase-memory-mcp` command with the environment of the server llmenv launches.
///
/// # Errors
/// `codebase-memory-mcp` is not in an absolute PATH folder, or `index_path` cannot be expanded,
/// for example a leading `~` with `HOME` unset.
fn cbm_command(cm: &CodebaseMemory, args: &[&str]) -> anyhow::Result<Command> {
    let program = crate::path_lookup::resolve_program("codebase-memory-mcp")?;
    command_with(&program, cm, args)
}

/// [`cbm_command`] for the program given, so a test needs no installed server.
fn command_with(program: &Path, cm: &CodebaseMemory, args: &[&str]) -> anyhow::Result<Command> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(index_path) = &cm.index_path {
        cmd.env("CBM_CACHE_DIR", crate::paths::expand_tilde(index_path)?);
    }
    Ok(cmd)
}

/// The roots in the output of `allow-root --list`.
/// The roots in the output of `allow-root --list`. A line that is neither a root nor a known
/// header or empty notice means the format changed. The roots are then unknown, so the read
/// fails rather than reading as an empty, unconfined record (#2916).
///
/// # Errors
/// A line that llmenv does not recognize.
fn parse_roots(listing: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for line in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.starts_with('/') {
            roots.push(PathBuf::from(line));
        } else if !line.starts_with("allowed roots") && !line.starts_with("no allowed roots") {
            anyhow::bail!("unrecognized line in allow-root --list output: {line:?}");
        }
    }
    Ok(roots)
}

/// The roots that codebase-memory-mcp has recorded for the cache folder of `cm`.
///
/// # Errors
/// The command cannot run, times out, or exits non-zero.
fn list_roots(cm: &CodebaseMemory) -> anyhow::Result<Vec<PathBuf>> {
    let output = crate::process::run_traced(
        "cbm:allow-root",
        cbm_command(cm, &["allow-root", "--list"])?,
        COMMAND_TIMEOUT,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "codebase-memory-mcp allow-root --list: {e:#}. Is codebase-memory-mcp on PATH?"
        )
    })?;
    anyhow::ensure!(
        output.status.success(),
        "codebase-memory-mcp allow-root --list exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_roots(&String::from_utf8_lossy(&output.stdout))
}

/// What the server allows, compared with what the config declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootsReport {
    /// The roots the server has recorded.
    pub listed: Vec<PathBuf>,
    /// The declared roots that exist and that no recorded root contains, with the reason.
    pub missing: Vec<(PathBuf, String)>,
    /// The recorded roots that no declared root contains. Earlier llmenv versions recorded them.
    pub undeclared: Vec<PathBuf>,
    pub cache_dir: PathBuf,
}

/// The canonical form of `path`, or `None` when it does not resolve. A cause other than a
/// missing path is a notice, because the root check then leaves the root out (#3462).
fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path)
        .inspect_err(|e| {
            if e.kind() != std::io::ErrorKind::NotFound {
                crate::agent_notice::record_logged(format!(
                    "llmenv: the code index root {} cannot be resolved, so the root check leaves \
                     it out: {e}. Fix: check its permissions.",
                    path.display()
                ));
            }
        })
        .ok()
}

/// Whether `path` is one of `roots` or sits under one. Both sides resolve links first, so a
/// link into a recorded folder counts (#2406).
fn is_within_any(path: &Path, roots: &[PathBuf]) -> bool {
    let Some(path) = canonical(path) else {
        return false;
    };
    roots
        .iter()
        .filter_map(|root| canonical(root))
        .any(|root| path.starts_with(root))
}

/// The declared roots that the server does not cover. An empty record means the server is
/// unconfined, so nothing is missing (#2916). A root that does not exist cannot be recorded, so
/// it is left out.
fn missing_roots(wanted: &[PathBuf], listed: &[PathBuf]) -> Vec<PathBuf> {
    if listed.is_empty() {
        return Vec::new();
    }
    wanted
        .iter()
        .filter(|root| is_folder(root) && !is_within_any(root, listed))
        .cloned()
        .collect()
}

/// Whether `path` is a folder. A stat error other than "not found" is a notice, because it
/// would otherwise drop a declared root from the report without a word (#2916, #3462).
fn is_folder(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_dir(),
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                crate::agent_notice::record_logged(format!(
                    "llmenv: the declared code index root {} cannot be read, so the root \
                     check leaves it out: {e}. Fix: check its permissions.",
                    path.display()
                ));
            }
            false
        }
    }
}

/// The recorded roots that no declared root covers. A recorded root that sits under a declared
/// one is covered, so it is not reported.
fn undeclared_roots(listed: &[PathBuf], wanted: &[PathBuf]) -> Vec<PathBuf> {
    listed
        .iter()
        .filter(|root| {
            if canonical(root).is_none() {
                crate::agent_notice::record_logged(format!(
                    "llmenv: the recorded code index root {} does not resolve, so the root \
                     check leaves it out. Fix: run `llmenv doctor`.",
                    root.display()
                ));
                return false;
            }
            !is_within_any(root, wanted)
        })
        .cloned()
        .collect()
}

/// Read-only: compare the declared roots with what the server has recorded. llmenv runs no
/// `allow-root` here (#2916).
///
/// # Errors
/// The list cannot be read (see [`list_roots`]).
pub(crate) fn check_roots(
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
        undeclared: undeclared_roots(&listed, wanted),
        listed,
        missing,
        cache_dir: cache_dir(cm, home)?,
    })
}

/// Read the roots at `SessionStart`. Returns the text to show the user when a declared root is
/// missing or the server cannot be asked, and `None` when all is well (#2406). It only reads: a
/// session never records a root (#2916).
pub(crate) fn session_start_notice(cm: &CodebaseMemory, project_root: &Path) -> Option<String> {
    let bases = RootBases::for_project(project_root);
    let wanted = configured_roots(cm, &bases);
    let problems = root_problems(cm, &bases);
    match check_roots(cm, &wanted, bases.home.as_deref()) {
        Ok(report) => {
            let (warn, text) = describe(&report);
            let undeclared = !report.undeclared.is_empty();
            let text = [(warn || undeclared).then_some(text)]
                .into_iter()
                .flatten()
                .chain(problems)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then(|| format!("{text}\n"))
        }
        Err(e) => {
            tracing::error!("codebase-memory allowed roots could not be read: {e:#}");
            Some(format!(
                "codebase-memory: cannot read the allowed roots: {e:#}. Run `llmenv doctor`.\n"
            ))
        }
    }
}

#[cfg(test)]
mod set_props;

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests;
