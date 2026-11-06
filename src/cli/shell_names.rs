//! The shell that `llmenv completions --install` targets, and the file name of its script.

use std::path::Path;

/// Map `$SHELL`'s basename to a [`clap_complete::Shell`]. `shell_env` is the
/// raw `$SHELL` value (injected so this is unit-testable without mutating
/// process env) — typically a path like `/bin/zsh`.
pub(super) fn detect_shell(shell_env: Option<&str>) -> anyhow::Result<clap_complete::Shell> {
    let shell_path = shell_env.ok_or_else(|| {
        anyhow::anyhow!("--shell not given and $SHELL is not set; pass --shell explicitly")
    })?;
    let name = Path::new(shell_path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!("could not determine shell name from $SHELL={shell_path}")
        })?;
    match name {
        "bash" => Ok(clap_complete::Shell::Bash),
        "zsh" => Ok(clap_complete::Shell::Zsh),
        "fish" => Ok(clap_complete::Shell::Fish),
        other => anyhow::bail!(
            "unrecognized shell '{other}' from $SHELL={shell_path}; pass --shell explicitly \
             (bash, zsh, or fish)"
        ),
    }
}

/// The file name a completion script is conventionally installed under.
/// zsh's autoload-based completion system requires the leading underscore.
pub(super) fn completion_filename(shell: clap_complete::Shell) -> &'static str {
    match shell {
        clap_complete::Shell::Zsh => "_llmenv",
        clap_complete::Shell::Fish => "llmenv.fish",
        _ => "llmenv",
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn detect_shell_maps_shell_env_basename() {
        assert_eq!(
            detect_shell(Some("/bin/bash")).expect("test"),
            clap_complete::Shell::Bash
        );
        assert_eq!(
            detect_shell(Some("/usr/bin/zsh")).expect("test"),
            clap_complete::Shell::Zsh
        );
        assert_eq!(
            detect_shell(Some("/usr/local/bin/fish")).expect("test"),
            clap_complete::Shell::Fish
        );
    }

    #[test]
    fn detect_shell_errors_when_shell_env_unset() {
        let err = detect_shell(None).expect_err("test");
        assert!(err.to_string().contains("SHELL"));
    }

    #[test]
    fn detect_shell_errors_on_unrecognized_shell() {
        let err = detect_shell(Some("/bin/csh")).expect_err("test");
        assert!(err.to_string().contains("csh"));
    }

    #[test]
    fn completion_filename_prefixes_zsh_with_underscore() {
        assert_eq!(completion_filename(clap_complete::Shell::Zsh), "_llmenv");
        assert_eq!(completion_filename(clap_complete::Shell::Bash), "llmenv");
        assert_eq!(
            completion_filename(clap_complete::Shell::Fish),
            "llmenv.fish"
        );
    }
}
