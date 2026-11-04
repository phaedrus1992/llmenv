//! Doctor check for the program name in llmenv's hook commands (#3049, #3081, #3119). The hook
//! commands name the running llmenv binary. The bare name is used only when that binary's path is
//! not UTF-8 or the OS cannot report it. A path in a build or temp folder, or a removed binary,
//! is also reported, because a hook then runs a file that anyone who can write there controls.

use std::path::{Path, PathBuf};

use crate::adapter::hook_program::{fallback_message, program_name};

/// The suffix Linux appends to the path of a binary that was removed or replaced.
const DELETED_SUFFIX: &str = " (deleted)";

/// The fixed temp folders. Their canonical forms, such as `/private/tmp` on macOS, come from
/// `with_canonical`.
const FIXED_TEMP_ROOTS: [&str; 2] = ["/tmp", "/var/tmp"];

/// A reason the binary that hook commands name is not safe to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookPathRisk {
    /// The path has a `target` folder in it, which is where cargo builds.
    BuildFolder,
    /// The path is under a temp folder.
    TempFolder,
    /// The path ends in ` (deleted)`: Linux reports this for a binary that was removed or replaced.
    Deleted,
}

/// Print the hook-command check for the running binary.
pub(super) fn run_doctor_hook_command(use_color: bool) {
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let exe = std::env::current_exe();
    let for_check = match &exe {
        Ok(path) => Ok(path.clone()),
        Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
    };
    match (program_name(for_check), exe) {
        (Ok(_), Ok(path)) => report_hook_path(&path, &pass, &warn),
        (Ok(_), Err(e)) => eprintln!(
            "{warn} llmenv could not read its own path, so the hook commands are not checked: {e}. \
             Fix: run llmenv from an installed binary."
        ),
        (Err(reason), _) => eprintln!("{warn} {}", fallback_message(&reason)),
    }
}

/// Print one warning per risk of `exe`, or one pass line when there is none.
fn report_hook_path(exe: &Path, pass: &str, warn: &str) {
    let risks = hook_path_risks(exe, &temp_roots());
    if risks.is_empty() {
        eprintln!("{pass} Hook commands name llmenv by the path of the running binary");
        return;
    }
    for risk in risks {
        eprintln!("{warn} {}", risk_message(risk, exe));
    }
}

/// The risks of `exe`, the path that hook commands name. `temp_roots` are the temp folders to
/// test against. The result is in the order Deleted, BuildFolder, TempFolder, and has no repeats.
fn hook_path_risks(exe: &Path, temp_roots: &[PathBuf]) -> Vec<HookPathRisk> {
    let mut risks = Vec::new();
    let text = exe.to_string_lossy();
    let path = match text.strip_suffix(DELETED_SUFFIX) {
        Some(stripped) => {
            risks.push(HookPathRisk::Deleted);
            PathBuf::from(stripped)
        }
        None => exe.to_path_buf(),
    };
    // A symlink can point into a build folder, so the resolved path is checked as well.
    let candidates = with_canonical(std::iter::once(path));
    if candidates.iter().any(|c| is_build_folder(c)) {
        risks.push(HookPathRisk::BuildFolder);
    }
    if is_under_temp(&candidates, temp_roots) {
        risks.push(HookPathRisk::TempFolder);
    }
    risks
}

/// Whether a parent folder of `path` is a whole component named `target`. A folder such as
/// `targets` or `my-target` does not match.
fn is_build_folder(path: &Path) -> bool {
    path.parent()
        .is_some_and(|dir| dir.components().any(|c| c.as_os_str() == "target"))
}

/// Whether any candidate path is under a temp root. The roots are compared in canonical form too,
/// because on macOS `/tmp` is a link to `/private/tmp`.
fn is_under_temp(candidates: &[PathBuf], temp_roots: &[PathBuf]) -> bool {
    let roots = with_canonical(temp_roots.iter().cloned());
    candidates
        .iter()
        .any(|c| roots.iter().any(|r| c.starts_with(r)))
}

/// Each path, then its canonical form when the OS can resolve it. A path that does not exist has
/// no canonical form, so only its raw form is kept.
fn with_canonical(paths: impl Iterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in paths {
        if let Ok(canonical) = std::fs::canonicalize(&path) {
            out.push(canonical);
        }
        out.push(path);
    }
    out
}

/// The temp folders that a hook binary must not live under, without repeats.
fn temp_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut add = |path: PathBuf| {
        if !roots.contains(&path) {
            roots.push(path);
        }
    };
    add(std::env::temp_dir());
    for fixed in FIXED_TEMP_ROOTS {
        add(PathBuf::from(fixed));
    }
    // A session sets TMPDIR to this folder, so the system roots alone can miss it.
    match crate::paths::state_dir() {
        Ok(state) => add(state.join("tmp")),
        Err(e) => tracing::warn!(
            "doctor: the state folder cannot be found, so its temp folder is not checked: {e}. \
             Fix: set HOME."
        ),
    }
    roots
}

/// The warning text for `risk`, naming `exe` and the fix.
fn risk_message(risk: HookPathRisk, exe: &Path) -> String {
    let path = exe.display();
    match risk {
        HookPathRisk::BuildFolder => format!(
            "Hook commands name {path}, which is in a cargo target folder. A later build replaces \
             it, and anyone who can write that folder controls every hook. Fix: install llmenv, \
             then run llmenv regenerate with the installed binary."
        ),
        HookPathRisk::TempFolder => format!(
            "Hook commands name {path}, which is in a temp folder. A temp sweep can remove it, and \
             anyone who can write that folder controls every hook. Fix: install llmenv, then run \
             llmenv regenerate with the installed binary."
        ),
        HookPathRisk::Deleted => format!(
            "Hook commands name {path}, but that binary was removed or replaced after it started. \
             Fix: run llmenv regenerate with the installed binary."
        ),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/tmp")]
    }

    #[test]
    fn an_unknown_binary_path_is_reported_with_the_fix() {
        let reason = program_name(Err(std::io::Error::other("no path")))
            .expect_err("an unknown path is reported");
        let message = fallback_message(&reason);
        assert!(message.contains("Fix: move the llmenv binary"), "{message}");
    }

    #[test]
    fn a_known_binary_path_is_not_reported() {
        let exe = std::path::PathBuf::from("/opt/llmenv/bin/llmenv");
        assert!(program_name(Ok(exe)).is_ok());
    }

    #[test]
    fn a_binary_in_a_cargo_target_folder_is_a_build_folder_risk() {
        let exe = PathBuf::from("/home/u/llmenv/target/debug/llmenv");
        assert_eq!(
            hook_path_risks(&exe, &roots()),
            vec![HookPathRisk::BuildFolder]
        );
    }

    #[test]
    fn a_binary_in_a_temp_folder_is_a_temp_folder_risk() {
        let exe = PathBuf::from("/tmp/build-1234/llmenv");
        assert_eq!(
            hook_path_risks(&exe, &roots()),
            vec![HookPathRisk::TempFolder]
        );
    }

    #[test]
    fn a_deleted_binary_is_a_deleted_risk() {
        let exe = PathBuf::from("/usr/local/bin/llmenv (deleted)");
        assert_eq!(hook_path_risks(&exe, &roots()), vec![HookPathRisk::Deleted]);
    }

    #[test]
    fn an_installed_binary_has_no_risk() {
        let exe = PathBuf::from("/home/u/.cargo/bin/llmenv");
        assert!(hook_path_risks(&exe, &roots()).is_empty());
    }

    #[test]
    fn a_folder_named_like_target_is_not_a_build_folder() {
        let exe = PathBuf::from("/home/u/targets/llmenv");
        assert!(hook_path_risks(&exe, &roots()).is_empty());
    }

    #[test]
    fn a_deleted_build_binary_has_both_risks() {
        let exe = PathBuf::from("/home/u/p/target/release/llmenv (deleted)");
        assert_eq!(
            hook_path_risks(&exe, &roots()),
            vec![HookPathRisk::Deleted, HookPathRisk::BuildFolder]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_into_a_target_folder_is_a_build_folder_risk() {
        let dir = tempfile::tempdir().expect("test");
        let built = dir.path().join("target").join("debug");
        std::fs::create_dir_all(&built).expect("test");
        let real = built.join("llmenv");
        std::fs::write(&real, "").expect("test");
        let link = dir.path().join("llmenv");
        std::os::unix::fs::symlink(&real, &link).expect("test");
        assert_eq!(hook_path_risks(&link, &[]), vec![HookPathRisk::BuildFolder]);
    }

    #[test]
    fn each_risk_message_names_the_path_and_the_fix() {
        for risk in [
            HookPathRisk::BuildFolder,
            HookPathRisk::TempFolder,
            HookPathRisk::Deleted,
        ] {
            let message = risk_message(risk, Path::new("/x/llmenv"));
            assert!(message.contains("/x/llmenv"), "{message}");
            assert!(message.contains("Fix:"), "{message}");
        }
    }

    proptest! {
        /// Any path under a temp root is a temp-folder risk, whatever its child names are.
        #[test]
        fn a_child_of_a_temp_root_is_a_temp_folder_risk(
            children in proptest::collection::vec("[a-z][a-z0-9]{0,7}", 1..4),
        ) {
            let root = PathBuf::from("/opt/llmenv-temp-root");
            let mut exe = root.clone();
            for child in &children {
                exe.push(child);
            }
            let risks = hook_path_risks(&exe, std::slice::from_ref(&root));
            prop_assert!(risks.contains(&HookPathRisk::TempFolder), "{risks:?}");
        }
    }
}
