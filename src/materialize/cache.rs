use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::config::{HashingMode, VersionFidelity};
use crate::merge::MergedManifest;

/// Stable SHA-256 of the merged manifest. Each field is length-prefixed
/// (little-endian u64) before its bytes so concatenation cannot ambiguate
/// boundaries — i.e. `{agents_md="ABC", files={"DE":"FG"}}` and
/// `{agents_md="ABCD", files={"E":"FG"}}` must hash differently.
/// Filesystem-safe version tag baked in by `build.rs`. Format:
/// `{pkg_version}-{git_short_hash}` (or bare `{pkg_version}` when built outside
/// a git checkout). No `-dirty` suffix — all dev builds at a given HEAD share
/// a bucket so iterating doesn't fragment the cache.
///
/// Used as the *prefix* of the materialized folder name (not mixed into the
/// content hash) so manual cleanup is obvious: `ls ~/.cache/llmenv/claude-code`
/// groups folders by binary version, and pruning means removing anything not
/// starting with the current tag.
pub const VERSION_TAG: &str = env!("LLMENV_VERSION_TAG");

/// Bare package version (`X.Y.Z`, or a semver prerelease like `1.2.3-rc.1`),
/// baked in by `build.rs`. Source for the `version`-mode folder name at every
/// fidelity below `commit`.
pub const PKG_VERSION: &str = env!("LLMENV_PKG_VERSION");

/// Short git commit hash, or empty when built outside a git checkout (crates.io
/// tarball). Appended at [`VersionFidelity::Commit`] fidelity.
pub const GIT_HASH: &str = env!("LLMENV_GIT_HASH");

/// Compose the on-disk folder name for the active [`HashingMode`].
///
/// - [`HashingMode::Strict`] → `{VERSION_TAG}-{content_hash}`. Splitting the
///   version off the content hash keeps the hash a function of inputs only, so
///   two folders that differ in version prefix but share the same content hash
///   are byte-identical — useful for diffing across upgrades.
/// - [`HashingMode::Version`] → the binary version at `fidelity` (the content
///   hash is *not* in the name; it lives in the manifest dotfile). Content
///   edits re-render into the same folder.
#[must_use]
pub fn folder_name(content_hash: &str, mode: HashingMode, fidelity: VersionFidelity) -> String {
    match mode {
        HashingMode::Strict => format!("{VERSION_TAG}-{content_hash}"),
        HashingMode::Version => version_folder_name(fidelity),
    }
}

/// The `version`-mode folder name at `fidelity`, composed from [`PKG_VERSION`]
/// and [`GIT_HASH`] (both baked in by `build.rs`). Filesystem-safe: package
/// versions and git short-hashes contain only `[0-9A-Za-z.+-]`.
///
/// A version of `1.2.3` yields `1` / `1.2` / `1.2.3` / `1.2.3-hhhhhhhh`. A
/// shorter-than-expected version (e.g. a bare `1`) degrades gracefully: each
/// fidelity takes as many leading `.`-separated components as exist.
#[must_use]
pub fn version_folder_name(fidelity: VersionFidelity) -> String {
    match fidelity {
        VersionFidelity::Major => PKG_VERSION
            .split('.')
            .next()
            .unwrap_or(PKG_VERSION)
            .to_string(),
        VersionFidelity::MajorMinor => {
            let mut parts = PKG_VERSION.split('.');
            let first = parts.next().unwrap_or("");
            if let Some(second) = parts.next() {
                format!("{first}.{second}")
            } else {
                first.to_string()
            }
        }
        VersionFidelity::Full => PKG_VERSION.to_string(),
        VersionFidelity::Commit => {
            if GIT_HASH.is_empty() {
                PKG_VERSION.to_string()
            } else {
                format!("{PKG_VERSION}-{GIT_HASH}")
            }
        }
    }
}

pub fn hash_manifest(m: &MergedManifest) -> anyhow::Result<String> {
    let mut h = Sha256::new();
    update_len_prefixed(&mut h, m.agents_md.as_bytes());
    h.update((m.files.len() as u64).to_le_bytes());
    for (rel, abs) in &m.files {
        let rel_str = rel.to_string_lossy();
        update_len_prefixed(&mut h, rel_str.as_bytes());
        let bytes = std::fs::read(abs)?;
        update_len_prefixed(&mut h, &bytes);
    }
    // Note: the cache key is a function of (relative path, file contents) only.
    // The absolute `abs` source path is deliberately NOT hashed, so a bundle
    // reachable via a symlink or alias produces the same key as the canonical
    // path and reuses the cache (#66).
    // Mix in rules so adding/editing a `rules/*.md` invalidates the cache.
    // Hash the raw text — covers both frontmatter and body without needing
    // a second pass and matches what gets written to disk for Claude.
    h.update((m.rules.len() as u64).to_le_bytes());
    for r in &m.rules {
        update_len_prefixed(&mut h, r.bundle.as_bytes());
        let rel_str = r.rel.to_string_lossy();
        update_len_prefixed(&mut h, rel_str.as_bytes());
        update_len_prefixed(&mut h, r.raw.as_bytes());
    }
    // Mix in resolved MCP servers so any change in MCP wiring (selection,
    // role resolution, transport) invalidates the cache. Each entry is hashed
    // by its rendered shape, length-prefixed for unambiguous boundaries.
    h.update((m.mcps.len() as u64).to_le_bytes());
    for mcp in &m.mcps {
        update_len_prefixed(&mut h, mcp.name.as_bytes());
        match &mcp.kind {
            crate::mcp::resolve::ResolvedKind::Stdio { command, args, env } => {
                h.update([0u8]);
                update_len_prefixed(&mut h, command.as_bytes());
                h.update((args.len() as u64).to_le_bytes());
                for a in args {
                    update_len_prefixed(&mut h, a.as_bytes());
                }
                h.update((env.len() as u64).to_le_bytes());
                for (k, v) in env {
                    update_len_prefixed(&mut h, k.as_bytes());
                    update_len_prefixed(&mut h, v.as_bytes());
                }
            }
            crate::mcp::resolve::ResolvedKind::Remote { url, transport } => {
                h.update([1u8]);
                update_len_prefixed(&mut h, url.as_bytes());
                update_len_prefixed(&mut h, format!("{transport:?}").as_bytes());
            }
        }
    }
    // Mix in resolved plugins so changing the selected plugin set invalidates
    // the cache. Each entry is hashed by `marketplace:plugin` (provenance is not
    // hashed — it doesn't affect what gets rendered).
    h.update((m.plugins.len() as u64).to_le_bytes());
    for p in &m.plugins {
        update_len_prefixed(&mut h, p.marketplace.as_bytes());
        update_len_prefixed(&mut h, p.plugin.as_bytes());
    }
    // Mix in referenced marketplaces by name + source + content token (git HEAD,
    // or install location for path sources). A marketplace update (new HEAD)
    // therefore re-renders every scope that wires it.
    h.update((m.marketplaces.len() as u64).to_le_bytes());
    for mk in &m.marketplaces {
        update_len_prefixed(&mut h, mk.name.as_bytes());
        update_len_prefixed(&mut h, mk.source.as_bytes());
        update_len_prefixed(&mut h, mk.head.as_deref().unwrap_or("").as_bytes());
        update_len_prefixed(
            &mut h,
            mk.install_location.as_deref().unwrap_or("").as_bytes(),
        );
    }
    Ok(hex::encode(h.finalize()))
}

fn update_len_prefixed(h: &mut Sha256, data: &[u8]) {
    h.update((data.len() as u64).to_le_bytes());
    h.update(data);
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub removed: Vec<PathBuf>,
    pub kept: usize,
}

/// Selects which cache folders `prune` targets.
#[derive(Debug, Clone, Copy)]
pub enum PruneMode {
    /// Remove every cache folder unconditionally.
    All,
    /// Remove current-version folders whose newest mtime is older than this.
    OlderThan(Duration),
    /// Remove only stale (version-mismatched) folders. `*.tmp` always go.
    StaleOnly,
}

#[derive(Debug, Default)]
pub struct PruneReport {
    pub removed: Vec<PathBuf>,
    pub kept: usize,
}

/// Prune cache folders under `cache_root` according to `mode`.
///
/// `version_folder` is the current [`HashingMode::Version`] folder name (e.g.
/// `1.2`) when version mode is active, or `None` in strict mode. It exists so a
/// version-mode folder — which has no `{VERSION_TAG}-` prefix to recognize it
/// by — is still counted as "current" and not swept by `StaleOnly`. In strict
/// mode pass `None`; the prefix test alone identifies current folders.
///
/// Behavior:
/// - `*.tmp` staging dirs are always removed (orphaned partial writes).
/// - `StaleOnly`: removes folders that are neither a `{VERSION_TAG}-…` folder
///   (strict, current binary) nor `version_folder` (version mode, current).
/// - `OlderThan(d)`: removes current folders older than `d`.
/// - `All`: removes every folder unconditionally.
///
/// Security invariants:
/// - Only direct children of `cache_root` are considered; the walk never
///   recurses across symlinks, so a symlinked entry is unlinked (link only,
///   never its target) rather than followed.
/// - When `dry_run` is true, zero filesystem mutations occur — the report
///   lists what *would* be removed.
///
/// # Errors
/// Returns an error if reading `cache_root` or removing an entry fails.
pub fn prune(
    cache_root: &Path,
    mode: PruneMode,
    version_folder: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<PruneReport> {
    let mut report = PruneReport::default();
    if !cache_root.exists() {
        return Ok(report);
    }
    let now = SystemTime::now();
    for entry in std::fs::read_dir(cache_root)? {
        let entry = entry?;
        let p = entry.path();
        // lstat-equivalent: a symlink at the top level is never followed. We
        // remove the link itself (never its target) so the cache root can't be
        // used to delete arbitrary files outside it.
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            remove_link(&p, dry_run, &mut report);
            continue;
        }
        if !ft.is_dir() {
            continue;
        }
        // Orphaned staging dirs are always removed regardless of mode.
        if p.extension().is_some_and(|e| e == "tmp") {
            remove_dir(&p, dry_run, &mut report)?;
            continue;
        }

        // The durable state dir (#175) is a stable sibling of the hashed config
        // folders that must outlive every materialization. It is never a prune
        // candidate under any mode — cache invalidation must not wipe tool state.
        if entry.file_name().to_str() == Some(crate::materialize::state::STATE_DIR_NAME) {
            report.kept += 1;
            continue;
        }

        let is_current = entry.file_name().to_str().is_some_and(|name| {
            name.starts_with(&format!("{VERSION_TAG}-")) || version_folder == Some(name)
        });

        let should_remove = match mode {
            PruneMode::All => true,
            PruneMode::StaleOnly => !is_current,
            PruneMode::OlderThan(older_than) => {
                // Only current-version folders are aged out; stale folders are
                // left to a StaleOnly/All pass so the two axes stay orthogonal.
                if is_current {
                    let m = newest_mtime(&p)?;
                    now.duration_since(m).unwrap_or_default() > older_than
                } else {
                    false
                }
            }
        };

        if should_remove {
            remove_dir(&p, dry_run, &mut report)?;
        } else {
            report.kept += 1;
        }
    }
    Ok(report)
}

/// Record a directory removal, performing the unlink unless `dry_run`.
fn remove_dir(p: &Path, dry_run: bool, report: &mut PruneReport) -> anyhow::Result<()> {
    if !dry_run {
        std::fs::remove_dir_all(p)?;
    }
    report.removed.push(p.to_path_buf());
    Ok(())
}

/// Record a symlink removal, performing the unlink unless `dry_run`.
fn remove_link(p: &Path, dry_run: bool, report: &mut PruneReport) {
    if !dry_run {
        // A failed unlink here is non-fatal: report what we attempted and
        // continue pruning the rest of the cache root.
        let _ = std::fs::remove_file(p);
    }
    report.removed.push(p.to_path_buf());
}

/// Remove cache subdirectories whose newest mtime is older than `older_than`.
/// `*.tmp` staging directories are removed regardless of age — they represent
/// orphaned partial writes from a previous crashed `materialize` call.
pub fn gc(cache_root: &Path, older_than: Duration) -> anyhow::Result<GcReport> {
    let mut report = GcReport::default();
    if !cache_root.exists() {
        return Ok(report);
    }
    let now = SystemTime::now();
    for entry in std::fs::read_dir(cache_root)? {
        let entry = entry?;
        let p = entry.path();
        // Use `file_type()` (lstat-equivalent) — a symlink at the top level
        // is never treated as a cache directory we own. Removing one removes
        // only the link, never the target.
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            std::fs::remove_file(&p)?;
            report.removed.push(p);
            continue;
        }
        if !ft.is_dir() {
            continue;
        }
        if p.extension().is_some_and(|e| e == "tmp") {
            std::fs::remove_dir_all(&p)?;
            report.removed.push(p);
            continue;
        }
        // The durable state dir (#175) is a stable sibling of the hashed config
        // folders and must outlive every materialization. It is never an age-based
        // collection candidate — cache invalidation must not wipe tool state.
        if entry.file_name().to_str() == Some(crate::materialize::state::STATE_DIR_NAME) {
            report.kept += 1;
            continue;
        }
        let m = newest_mtime(&p)?;
        if now.duration_since(m).unwrap_or_default() > older_than {
            std::fs::remove_dir_all(&p)?;
            report.removed.push(p);
        } else {
            report.kept += 1;
        }
    }
    Ok(report)
}

/// Newest mtime found anywhere under `dir` (including the dir itself).
fn newest_mtime(dir: &Path) -> anyhow::Result<SystemTime> {
    let mut newest = dir.metadata()?.modified()?;
    walk_mtime(dir, &mut newest)?;
    Ok(newest)
}

fn walk_mtime(dir: &Path, newest: &mut SystemTime) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let m = entry.metadata()?.modified()?;
        if m > *newest {
            *newest = m;
        }
        if file_type.is_dir() {
            walk_mtime(&entry.path(), newest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    fn touch_dir(root: &Path, name: &str) -> PathBuf {
        let p = root.join(name);
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("file.txt"), b"x").unwrap();
        p
    }

    #[test]
    fn strict_folder_name_is_version_tag_plus_hash() {
        let name = folder_name("deadbeef", HashingMode::Strict, VersionFidelity::default());
        assert_eq!(name, format!("{VERSION_TAG}-deadbeef"));
    }

    #[test]
    fn version_folder_name_nests_by_fidelity() {
        // PKG_VERSION is baked at build time, so assert structural relationships
        // rather than literal strings: each coarser fidelity is a prefix of the
        // finer one, and commit is full (+ optional `-hash`).
        let major = version_folder_name(VersionFidelity::Major);
        let major_minor = version_folder_name(VersionFidelity::MajorMinor);
        let full = version_folder_name(VersionFidelity::Full);
        let commit = version_folder_name(VersionFidelity::Commit);

        assert_eq!(
            full, PKG_VERSION,
            "full fidelity is the bare package version"
        );
        assert!(
            major_minor.starts_with(&major),
            "major ({major}) is a prefix of major_minor ({major_minor})"
        );
        assert!(
            full.starts_with(&major_minor),
            "major_minor ({major_minor}) is a prefix of full ({full})"
        );
        if GIT_HASH.is_empty() {
            assert_eq!(commit, full, "no git hash → commit degrades to full");
        } else {
            assert_eq!(commit, format!("{full}-{GIT_HASH}"));
        }
        // Folder mode ignores the content hash argument entirely.
        assert_eq!(
            folder_name("ignored", HashingMode::Version, VersionFidelity::Full),
            full
        );
    }

    #[test]
    fn version_folder_name_takes_leading_components() {
        // The major/major_minor split counts `.`-separated leading components.
        let dots = PKG_VERSION.split('.').count();
        if dots >= 2 {
            assert_ne!(
                version_folder_name(VersionFidelity::Major),
                version_folder_name(VersionFidelity::MajorMinor),
                "a >=2-component version distinguishes major from major_minor"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn hash_manifest_is_stable_across_symlinked_source_paths() {
        use std::collections::BTreeMap;
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(&real).unwrap();
        let file = real.join("AGENTS.md");
        fs::write(&file, b"content").unwrap();

        // Same file reached via a symlinked directory — a different absolute
        // path that resolves to the same bytes at the same relative key.
        let link = tmp.path().join("link");
        symlink(&real, &link).unwrap();
        let aliased = link.join("AGENTS.md");

        let manifest_real = MergedManifest {
            files: BTreeMap::from([(PathBuf::from("AGENTS.md"), file)]),
            ..MergedManifest::default()
        };
        let manifest_aliased = MergedManifest {
            files: BTreeMap::from([(PathBuf::from("AGENTS.md"), aliased)]),
            ..MergedManifest::default()
        };

        // #66: cache key is (relative path, contents) only — the absolute
        // source path must not perturb it, so the two hash identically.
        assert_eq!(
            hash_manifest(&manifest_real).unwrap(),
            hash_manifest(&manifest_aliased).unwrap()
        );
    }

    #[test]
    fn prune_missing_root_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let report = prune(&missing, PruneMode::All, None, false).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.kept, 0);
    }

    #[test]
    fn prune_all_removes_every_folder() {
        let tmp = tempfile::tempdir().unwrap();
        touch_dir(tmp.path(), &format!("{VERSION_TAG}-aaaa"));
        touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(tmp.path(), PruneMode::All, None, false).unwrap();
        assert_eq!(report.removed.len(), 2);
        assert_eq!(report.kept, 0);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn prune_stale_only_keeps_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let current = touch_dir(tmp.path(), &format!("{VERSION_TAG}-aaaa"));
        let stale = touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(tmp.path(), PruneMode::StaleOnly, None, false).unwrap();
        assert_eq!(report.kept, 1);
        assert!(report.removed.contains(&stale));
        assert!(!report.removed.contains(&current));
        assert!(current.exists());
        assert!(!stale.exists());
    }

    #[test]
    fn prune_stale_only_keeps_version_mode_folder() {
        // #196: a version-mode folder (e.g. "1.2") has no `{VERSION_TAG}-`
        // prefix, so `StaleOnly` must be told the current version folder name
        // explicitly or it would wrongly sweep the live config dir.
        let tmp = tempfile::tempdir().unwrap();
        let current = touch_dir(tmp.path(), "1.2");
        let stale = touch_dir(tmp.path(), "1.1");
        let report = prune(tmp.path(), PruneMode::StaleOnly, Some("1.2"), false).unwrap();
        assert_eq!(report.kept, 1);
        assert!(current.exists(), "current version folder must survive");
        assert!(!stale.exists(), "older version folder is swept");
        assert!(report.removed.contains(&stale));
    }

    #[test]
    fn prune_never_removes_durable_state_dir() {
        // The durable state dir (#175) is a stable sibling of the hashed config
        // folders; it holds tool state that must outlive every config folder, so
        // no prune mode may delete it — not even `All` (cache invalidation must
        // not wipe user/tool state).
        use crate::materialize::state::STATE_DIR_NAME;
        for mode in [PruneMode::StaleOnly, PruneMode::All] {
            let tmp = tempfile::tempdir().unwrap();
            let state = touch_dir(tmp.path(), STATE_DIR_NAME);
            // A genuinely stale folder: a different version tag, so it lacks the
            // current `{VERSION_TAG}-` prefix and is a real prune candidate.
            let stale = touch_dir(tmp.path(), "0.0.1-old-deadbeef");
            let report = prune(tmp.path(), mode, None, false).unwrap();
            assert!(state.exists(), "state dir must survive {mode:?}");
            assert!(
                !report.removed.contains(&state),
                "state dir never reported removed under {mode:?}"
            );
            // Sanity: a genuine stale folder is still swept under both modes.
            assert!(!stale.exists(), "stale folder still swept under {mode:?}");
        }
    }

    #[test]
    fn gc_never_removes_durable_state_dir() {
        // The age-based gc() path must honor the same durability guarantee as
        // prune() (#175): the durable state dir is a stable sibling of the hashed
        // config folders and must never be removed by cache invalidation, even
        // once it ages past the retention window. `Duration::ZERO` makes every
        // entry "older than", so a normal folder is swept and the state dir must
        // be the only survivor.
        use crate::materialize::state::STATE_DIR_NAME;
        let tmp = tempfile::tempdir().unwrap();
        let state = touch_dir(tmp.path(), STATE_DIR_NAME);
        let stale = touch_dir(tmp.path(), &format!("{VERSION_TAG}-deadbeef"));
        let report = gc(tmp.path(), Duration::ZERO).unwrap();
        assert!(state.exists(), "state dir must survive gc");
        assert!(
            !report.removed.contains(&state),
            "state dir never reported removed by gc"
        );
        // Sanity: an aged non-state folder is still collected.
        assert!(!stale.exists(), "aged folder still collected by gc");
        assert!(report.removed.contains(&stale));
    }

    #[test]
    fn prune_always_removes_tmp_staging_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = touch_dir(tmp.path(), &format!("{VERSION_TAG}-cccc.tmp"));
        let report = prune(tmp.path(), PruneMode::StaleOnly, None, false).unwrap();
        assert!(report.removed.contains(&staging));
        assert!(!staging.exists());
    }

    #[test]
    fn prune_dry_run_mutates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let a = touch_dir(tmp.path(), &format!("{VERSION_TAG}-aaaa"));
        let b = touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(tmp.path(), PruneMode::All, None, true).unwrap();
        // Reports what *would* be removed, but leaves the filesystem intact.
        assert_eq!(report.removed.len(), 2);
        assert!(a.exists());
        assert!(b.exists());
    }

    #[test]
    fn prune_older_than_skips_stale_folders() {
        let tmp = tempfile::tempdir().unwrap();
        // A stale folder is NOT aged out by OlderThan — only current-version
        // folders are subject to the age check, keeping the two axes orthogonal.
        let stale = touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(
            tmp.path(),
            PruneMode::OlderThan(Duration::ZERO),
            None,
            false,
        )
        .unwrap();
        // Current-version dir absent; stale kept because OlderThan ignores it.
        assert!(stale.exists());
        assert_eq!(report.kept, 1);
        assert!(report.removed.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn prune_unlinks_symlink_without_following() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        // Target lives OUTSIDE the cache root; pruning must not touch it.
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"important").unwrap();

        let cache_root = tmp.path().join("cache");
        fs::create_dir_all(&cache_root).unwrap();
        let link = cache_root.join("link");
        symlink(&outside, &link).unwrap();

        let report = prune(&cache_root, PruneMode::All, None, false).unwrap();
        assert!(report.removed.contains(&link));
        // The link is gone; the target and its contents survive.
        assert!(!link.exists());
        assert!(outside.join("keep.txt").exists());
    }

    /// Check if a string is exactly a 64-char lowercase hex SHA-256 hash.
    #[must_use]
    fn is_content_hash(s: &str) -> bool {
        s.len() == 64
            && s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    }

    mod hash_properties {
        use super::*;
        use crate::merge::MergedManifest;
        use crate::merge::rules::RuleFile;
        use crate::plugins::resolve::{ResolvedMarketplace, ResolvedPlugin};
        use proptest::prelude::*;

        fn rule(bundle: &str, rel: &str, raw: &str) -> RuleFile {
            RuleFile {
                bundle: bundle.into(),
                rel: PathBuf::from(rel),
                frontmatter: None,
                body: raw.into(),
                raw: raw.into(),
            }
        }

        // Build a manifest from in-memory fields only (no `files`, which would
        // require on-disk sources). These fields alone exercise the hash's
        // determinism and per-field sensitivity.
        fn manifest(
            agents_md: &str,
            rules: Vec<RuleFile>,
            plugins: Vec<ResolvedPlugin>,
            marketplaces: Vec<ResolvedMarketplace>,
        ) -> MergedManifest {
            MergedManifest {
                agents_md: agents_md.into(),
                rules,
                plugins,
                marketplaces,
                ..Default::default()
            }
        }

        proptest! {
            #[test]
            fn hash_is_deterministic(s in ".{0,64}", body in ".{0,64}") {
                let m = manifest(&s, vec![rule("b", "rules/a.md", &body)], vec![], vec![]);
                // Hashing the same manifest twice must yield the same digest:
                // the cache key would otherwise be unstable across runs.
                prop_assert_eq!(hash_manifest(&m).unwrap(), hash_manifest(&m).unwrap());
            }

            #[test]
            fn agents_md_edit_changes_hash(a in ".{0,48}", b in ".{0,48}") {
                prop_assume!(a != b);
                let ha = hash_manifest(&manifest(&a, vec![], vec![], vec![])).unwrap();
                let hb = hash_manifest(&manifest(&b, vec![], vec![], vec![])).unwrap();
                prop_assert_ne!(ha, hb, "an AGENTS.md edit must invalidate the cache");
            }

            #[test]
            fn rule_edit_changes_hash(a in ".{0,48}", b in ".{0,48}") {
                prop_assume!(a != b);
                let ha = hash_manifest(&manifest("x", vec![rule("b", "r.md", &a)], vec![], vec![]))
                    .unwrap();
                let hb = hash_manifest(&manifest("x", vec![rule("b", "r.md", &b)], vec![], vec![]))
                    .unwrap();
                prop_assert_ne!(ha, hb, "a rules/*.md edit must invalidate the cache");
            }

            #[test]
            fn plugin_set_change_changes_hash(name in "[a-z]{1,12}") {
                let base = manifest("x", vec![], vec![], vec![]);
                let with_plugin = manifest(
                    "x",
                    vec![],
                    vec![ResolvedPlugin {
                        marketplace: "mk".into(),
                        plugin: name,
                        collection: "c".into(),
                    }],
                    vec![],
                );
                prop_assert_ne!(
                    hash_manifest(&base).unwrap(),
                    hash_manifest(&with_plugin).unwrap(),
                    "adding a plugin must invalidate the cache"
                );
            }

            #[test]
            fn marketplace_head_change_changes_hash(h1 in "[a-f0-9]{1,12}", h2 in "[a-f0-9]{1,12}") {
                prop_assume!(h1 != h2);
                let mk = |head: &str| ResolvedMarketplace {
                    name: "mk".into(),
                    source: "https://example.com/x.git".into(),
                    install_location: None,
                    head: Some(head.into()),
                };
                let ha = hash_manifest(&manifest("x", vec![], vec![], vec![mk(&h1)])).unwrap();
                let hb = hash_manifest(&manifest("x", vec![], vec![], vec![mk(&h2)])).unwrap();
                prop_assert_ne!(ha, hb, "a marketplace HEAD bump must invalidate the cache");
            }
        }
    }

    mod folder_naming {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn folder_name_strict_always_has_version_tag(hash in "[a-f0-9]{64}") {
                let name = folder_name(&hash, HashingMode::Strict, VersionFidelity::default());
                prop_assert!(name.starts_with(&format!("{VERSION_TAG}-")),
                    "strict mode must always prefix with VERSION_TAG");
                prop_assert!(name.ends_with(&hash), "strict mode must suffix with the hash");
            }

            #[test]
            fn folder_name_version_ignores_hash(hash1 in "[a-f0-9]{64}", hash2 in "[a-f0-9]{64}") {
                prop_assume!(hash1 != hash2);
                let name1 = folder_name(&hash1, HashingMode::Version, VersionFidelity::Major);
                let name2 = folder_name(&hash2, HashingMode::Version, VersionFidelity::Major);
                // Version mode ignores the hash argument entirely.
                prop_assert_eq!(name1, name2, "version mode must ignore the hash argument");
            }

            #[test]
            fn version_folder_name_nesting_is_monotonic(f1 in 0usize..4, f2 in 0usize..4) {
                let fidelities = [
                    VersionFidelity::Major,
                    VersionFidelity::MajorMinor,
                    VersionFidelity::Full,
                    VersionFidelity::Commit,
                ];
                let name1 = version_folder_name(fidelities[f1]);
                let name2 = version_folder_name(fidelities[f2]);
                // Each fidelity is a prefix or equal to higher fidelities.
                if f1 < f2 {
                    prop_assert!(name2.starts_with(&name1) || name1 == name2,
                        "fidelity {f1} should be a prefix of fidelity {f2}");
                }
            }

            #[test]
            fn is_content_hash_accepts_valid_sha256(hash in "[a-f0-9]{64}") {
                prop_assert!(is_content_hash(&hash), "64 lowercase hex chars should be valid");
            }

            #[test]
            fn is_content_hash_rejects_wrong_length(len in 0usize..=256usize) {
                prop_assume!(len != 64);
                let s = "a".repeat(len);
                prop_assert!(!is_content_hash(&s),
                    "length {len} should not be valid (only 64 is)");
            }

            #[test]
            fn is_content_hash_rejects_uppercase(s in "[A-F0-9]{64}") {
                prop_assume!(s.chars().any(|c| c.is_ascii_uppercase()));
                prop_assert!(!is_content_hash(&s),
                    "uppercase hex should not be valid");
            }

            #[test]
            fn is_content_hash_rejects_non_hex(s in "[g-z]{64}") {
                prop_assert!(!is_content_hash(&s),
                    "non-hex characters should not be valid");
            }
        }
    }
}
