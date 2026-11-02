//! Drops the credential entries of the cache folders that GC removed (#1057).

use std::path::PathBuf;

use super::GcOutput;

/// How the credential drops of one sweep went. A failed drop is in neither `removed` nor a
/// silent gap, so the summary can name it (#3073).
#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::cli) struct CredentialDrops {
    /// Entries whose credential was deleted.
    pub(in crate::cli) removed: usize,
    /// Entries whose credential could not be deleted. Each one is logged where it fails.
    pub(in crate::cli) failed: usize,
}

/// Drop the platform credential entry belonging to each cache folder GC just
/// deleted, and report how many went and how many failed (#1057, #3073).
///
/// Keyed by folder path, so this only ever touches entries llmenv's own folders
/// owned — never the user's default `~/.claude` login. Deliberately confined to
/// the explicit `llmenv clean` path: removing credential entries as a side effect
/// of `export` would be the wrong default.
pub(in crate::cli) fn forget_credentials_for(removed: &[PathBuf]) -> CredentialDrops {
    tally(removed.iter().map(|path| {
        crate::auth::credentials::forget(path).inspect_err(|e| {
            tracing::error!(
                "could not drop credential entry for {}: {e}",
                path.display()
            );
        })
    }))
}

/// Count the results of a drop run. `Ok(true)` is a removed credential, `Ok(false)` is an entry
/// that had none, and `Err` is a drop that failed.
fn tally(results: impl IntoIterator<Item = anyhow::Result<bool>>) -> CredentialDrops {
    results
        .into_iter()
        .fold(CredentialDrops::default(), |mut drops, result| {
            match result {
                Ok(true) => drops.removed += 1,
                Ok(false) => {}
                Err(_) => drops.failed += 1,
            }
            drops
        })
}

/// Drop the credentials of the removed folders and print what happened. Returns the count of
/// failed drops, so the sweep counts them as failures (#3073).
pub(super) fn report_credential_drops(removed: &[PathBuf], out: GcOutput<'_>) -> usize {
    let drops = forget_credentials_for(removed);
    if drops.removed > 0 {
        eprintln!(
            "{} GC: dropped {} orphaned OAuth credential entries",
            out.pass, drops.removed
        );
    }
    if drops.failed > 0 {
        eprintln!(
            "{} GC: could not drop {} OAuth credential entries; see the error above",
            out.warn, drops.failed
        );
    }
    drops.failed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tally_counts_removed_and_failed_drops_apart() {
        let results = vec![
            Ok(true),
            Ok(false),
            Err(anyhow::anyhow!("keychain locked")),
            Ok(true),
        ];
        assert_eq!(
            tally(results),
            CredentialDrops {
                removed: 2,
                failed: 1
            }
        );
    }

    #[test]
    fn tally_of_no_entries_is_all_zero() {
        assert_eq!(tally(Vec::new()), CredentialDrops::default());
    }

    proptest::proptest! {
        /// Any run of drop results counts each result once, as removed or as failed.
        #[test]
        fn tally_counts_every_result(
            outcomes in proptest::collection::vec(proptest::prelude::any::<Option<bool>>(), 0..40),
        ) {
            let results: Vec<anyhow::Result<bool>> = outcomes
                .iter()
                .map(|o| o.ok_or_else(|| anyhow::anyhow!("keychain locked")))
                .collect();
            let removed = outcomes.iter().filter(|o| **o == Some(true)).count();
            let failed = outcomes.iter().filter(|o| o.is_none()).count();
            proptest::prop_assert_eq!(
                tally(results),
                CredentialDrops { removed, failed }
            );
        }
    }
}
