//! What memory-backend resolution found for the active scope, and the hook settings it carries.

use crate::mcp::resolve::MemoryHookSettings;

/// What memory-backend resolution found for the active scope.
///
/// Replaces the `Option<String>` that collapsed five distinguishable states
/// into `None` (#1131/#1132/#1140): a caller could not tell a project that
/// simply declares no memory from one whose only memory-carrying bundle is
/// suppressed by `disable_bundles`, or one whose declared entry is simply
/// gated on a tag that isn't active right now. The fifth state — a failed
/// bundle merge — is an `Err` from [`memory_url`] rather than a variant here,
/// because a backend may well be configured and merely unparseable: that is a
/// failure, not an absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryEndpoint {
    /// The memory backend resolved to this HTTP URL, carrying the active
    /// `features.memory` entry's hook settings (#1216, #2249).
    Active {
        url: String,
        settings: MemoryHookSettings,
    },
    /// No bundle fired for the active scopes and no top-level `features.memory`
    /// entry matched — nothing could have supplied a backend.
    NoBundlesFired,
    /// Bundles fired, but neither they nor the top-level config declare a
    /// `features.memory` entry active for these tags. `skipped_bundles` names
    /// firing bundles that `build_bundle_refs` dropped — for having no content
    /// directory, or for a rejected/unsafe name — so their `bundle.yaml` was
    /// never read (#1133/#1142).
    NotDeclared { skipped_bundles: Vec<String> },
    /// `features.memory` is supplied only by these bundles, which the active
    /// scopes suppress via `disable_bundles` (#194).
    SuppressedByDisableBundles(Vec<String>),
    /// A top-level or firing-bundle `features.memory` entry exists for these
    /// `server_host`s, but none of their `when` tags intersect the active
    /// scope — distinct from [`Self::NoBundlesFired`] (nothing declared at
    /// all) and [`Self::NotDeclared`] (declared by a bundle whose content
    /// never loaded). `resolve_mcps`'s `0 => {}` arm drops a tag-inactive
    /// entry silently; this variant is how `classify_missing_memory`
    /// recovers that information on the failure path (#1140).
    TagInactive { server_hosts: Vec<String> },
}

impl MemoryEndpoint {
    /// The bundles that fired without a content folder. Empty for every other endpoint (#3218).
    pub(super) fn skipped_bundles(&self) -> &[String] {
        match self {
            Self::NotDeclared { skipped_bundles } => skipped_bundles,
            _ => &[],
        }
    }

    /// The resolved URL, or an error naming why no backend is active.
    ///
    /// # Errors
    /// Every non-[`MemoryEndpoint::Active`] variant, rendered as the
    /// user-facing reason it is inactive.
    pub(crate) fn into_url(self) -> anyhow::Result<String> {
        const PREFIX: &str = "no memory backend active for this scope";
        match self {
            Self::Active { url, .. } => Ok(url),
            Self::NoBundlesFired => Err(anyhow::anyhow!(
                "{PREFIX}: no bundles fired and config.yaml declares no features.memory"
            )),
            Self::NotDeclared { skipped_bundles } if skipped_bundles.is_empty() => {
                Err(anyhow::anyhow!(
                    "{PREFIX}: no active bundle or config.yaml declares features.memory"
                ))
            }
            Self::NotDeclared { skipped_bundles } => Err(anyhow::anyhow!(
                "{PREFIX}: bundle(s) {} fired but were skipped while loading bundle \
                 content — either no content directory under the config dir's \
                 bundles/, or the bundle name was rejected (e.g. a traversal/absolute \
                 path) — so any features.memory they declare was never loaded",
                skipped_bundles.join(", ")
            )),
            Self::SuppressedByDisableBundles(names) => Err(anyhow::anyhow!(
                "{PREFIX}: features.memory is supplied only by bundle(s) {}, which \
                 this project turns off via disable_bundles",
                names.join(", ")
            )),
            Self::TagInactive { server_hosts } => Err(anyhow::anyhow!(
                "{PREFIX}: features.memory declares server_host(s) {}, but none of \
                 their `when` tags are in the active scope",
                server_hosts.join(", ")
            )),
        }
    }

    /// The active entry's hook settings (#1216, #2249). `None` for every
    /// non-[`MemoryEndpoint::Active`] variant.
    pub(super) fn settings(&self) -> Option<&MemoryHookSettings> {
        match self {
            Self::Active { settings, .. } => Some(settings),
            _ => None,
        }
    }

    /// Consume into `(url, settings)`, with the same errors as [`Self::into_url`].
    pub(super) fn into_url_and_settings(self) -> anyhow::Result<(String, MemoryHookSettings)> {
        let settings = self.settings().cloned().unwrap_or_else(default_memory_hook);
        Ok((self.into_url()?, settings))
    }
}

/// The settings of a `features.memory` entry that sets neither hook field.
pub(super) fn default_memory_hook() -> MemoryHookSettings {
    MemoryHookSettings {
        wakeup_max_tokens: None,
        adaptive_recall: true,
        default_type: None,
        default_importance: None,
        global_topics: crate::config::Memory::default().global_topics,
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::MemoryEndpoint;

    /// Without skipped bundles, the reason is that no declaration exists at all.
    #[test]
    fn not_declared_without_skipped_bundles_names_the_missing_declaration() {
        let err = MemoryEndpoint::NotDeclared {
            skipped_bundles: vec![],
        }
        .into_url()
        .expect_err("a non-active endpoint must error");
        let msg = err.to_string();
        assert!(
            msg.contains("no active bundle or config.yaml declares features.memory"),
            "{msg}"
        );
        assert!(!msg.contains("fired but were skipped"), "{msg}");
    }

    /// With skipped bundles, the reason names them, because their declaration was never read.
    #[test]
    fn not_declared_with_skipped_bundles_names_them() {
        let err = MemoryEndpoint::NotDeclared {
            skipped_bundles: vec!["alpha".into()],
        }
        .into_url()
        .expect_err("a non-active endpoint must error");
        let msg = err.to_string();
        assert!(msg.contains("alpha"), "{msg}");
        assert!(msg.contains("fired but were skipped"), "{msg}");
    }
}
