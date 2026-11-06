//! Freeing the link site: the transcript folder in the config dir becomes a link to the durable
//! store once its contents are folded in.

use std::path::Path;

use anyhow::Context as _;

use super::move_dir_newest_wins;

/// Free the link site, returning whether a link should now be created.
///
/// `Ok(false)` means "nothing to do" — either the link is already correct, or the
/// path is something we deliberately refuse to replace.
pub(super) fn clear_link_site(link: &Path, target: &Path) -> anyhow::Result<bool> {
    let file_type = match std::fs::symlink_metadata(link) {
        Ok(md) => md.file_type(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(anyhow::anyhow!("inspecting {}: {e}", link.display())),
    };
    if file_type.is_symlink() {
        if std::fs::read_link(link).is_ok_and(|dest| dest == target) {
            return Ok(false);
        }
        match std::fs::remove_file(link) {
            // A link that another process removed first is replaced, which is the goal.
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("replacing stale link {}", link.display()));
            }
            _ => return Ok(true),
        }
    }
    if file_type.is_dir() {
        let skipped = move_dir_newest_wins(link, target)?;
        // Deliberately not remove_dir_all: Claude Code writes into this directory
        // while the session runs, so anything it created between the fold above
        // and this call would be destroyed rather than inherited. Prune the empty
        // skeleton the fold left, then require the directory itself to be empty —
        // if it isn't, new transcripts landed mid-swap, so leave everything alone
        // and let the next export fold them in.
        crate::materialize::prune_empty_dirs(link)?;
        if !skipped.is_empty() {
            // These stay on every export, so the link is never made until they go.
            tracing::error!(
                "{} cannot become the link to the transcript store, because the fold leaves {} \
                 entries in it, such as {}: a link, another kind of entry, or a file as old as its \
                 copy. Fix: move these entries out of {}",
                link.display(),
                skipped.len(),
                skipped[0].display(),
                link.display()
            );
            return Ok(false);
        }
        if let Err(e) = std::fs::remove_dir(link) {
            tracing::warn!(
                "{} refilled during the swap ({e}); leaving it for the next export",
                link.display()
            );
            return Ok(false);
        }
        return Ok(true);
    }
    tracing::warn!(
        "{} is a regular file, not Claude Code's transcript dir — leaving it alone",
        link.display()
    );
    Ok(false)
}
