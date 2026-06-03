use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::config::HashingMode;
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
/// baked in by `build.rs`. Source for the `normal`-mode `version_mm` folder
/// segment.
pub const PKG_VERSION: &str = env!("LLMENV_PKG_VERSION");

/// Short git commit hash, or empty when built outside a git checkout (crates.io
/// tarball). Carried in [`VERSION_TAG`] for `strict`-mode folder names.
pub const GIT_HASH: &str = env!("LLMENV_GIT_HASH");

/// Compose the on-disk folder name (relative to the adapter root) for the
/// active [`HashingMode`] (#246).
///
/// - [`HashingMode::Loose`] → `<shape>`. Selection-addressed only; a binary
///   upgrade reuses the same folder, so this returns just the shape digest.
/// - [`HashingMode::Normal`] → `<version_mm>/<shape>`. Nests the shape under the
///   `major.minor` version so a minor bump or selection change mints a new
///   folder while content edits re-render in place.
/// - [`HashingMode::Strict`] → `{VERSION_TAG}-{content_hash}`. Splitting the
///   version off the content hash keeps the hash a function of inputs only, so
///   two folders that differ in version prefix but share the same content hash
///   are byte-identical — useful for diffing across upgrades.
#[must_use]
pub fn folder_name(mode: HashingMode, shape: &str, content_hash: &str) -> String {
    match mode {
        HashingMode::Loose => shape.to_string(),
        HashingMode::Normal => format!("{}/{}", version_mm(), shape),
        HashingMode::Strict => format!("{VERSION_TAG}-{content_hash}"),
    }
}

/// The `major.minor` version segment used to nest `normal`-mode folders,
/// composed from [`PKG_VERSION`] (baked in by `build.rs`). Filesystem-safe:
/// package versions contain only `[0-9A-Za-z.+-]`.
///
/// A version of `1.2.3` yields `1.2`. A shorter-than-expected version (e.g. a
/// bare `1`) degrades gracefully to whatever leading components exist.
#[must_use]
pub fn version_mm() -> String {
    let mut parts = PKG_VERSION.split('.');
    let first = parts.next().unwrap_or_default();
    if let Some(second) = parts.next() {
        format!("{first}.{second}")
    } else {
        first.to_string()
    }
}

/// 12-hex-char digest of the active *selection shape* (#246): the set of active
/// tags and the set of directly-enabled bundles.
///
/// The two sets are encoded in separate, length-prefixed groups (tags group
/// then bundles group, each prefixed by its element count) so a tag named `foo`
/// and a bundle named `foo` can never alias into the same shape. Each element
/// is itself length-prefixed before its bytes so element boundaries are
/// unambiguous. Both inputs are [`BTreeSet`]s, so iteration is already sorted —
/// the shape is independent of insertion order.
#[must_use]
pub fn shape(tags: &BTreeSet<String>, bundles: &BTreeSet<String>) -> String {
    let mut h = Sha256::new();
    h.update((tags.len() as u64).to_le_bytes());
    for tag in tags {
        update_len_prefixed(&mut h, tag.as_bytes());
    }
    h.update((bundles.len() as u64).to_le_bytes());
    for bundle in bundles {
        update_len_prefixed(&mut h, bundle.as_bytes());
    }
    let digest = hex::encode(h.finalize());
    digest[..12].to_string()
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
    // Mix in native: passthrough values so adding/editing a native key (top-level
    // or bundle-contributed) invalidates the cache and forces re-materialization (#292).
    h.update((m.native.len() as u64).to_le_bytes());
    for (key, value) in &m.native {
        update_len_prefixed(&mut h, key.as_bytes());
        let serialized = serde_yaml::to_string(value)
            .map_err(|e| anyhow::anyhow!("serializing native key '{key}': {e}"))?;
        update_len_prefixed(&mut h, serialized.as_bytes());
    }
    // Mix in capability-native fragments rendered by adapters but previously
    // unhashed: native_hooks, native_plugins, native_mcp. Same encoding as
    // m.native above — engine key + serialized YAML fragment.
    hash_native_capability_map(&mut h, &m.capabilities.native_hooks)?;
    hash_native_capability_map(&mut h, &m.capabilities.native_plugins)?;
    hash_native_capability_map(&mut h, &m.capabilities.native_mcp)?;
    Ok(hex::encode(h.finalize()))
}

fn update_len_prefixed(h: &mut Sha256, data: &[u8]) {
    h.update((data.len() as u64).to_le_bytes());
    h.update(data);
}

/// Hash a per-engine YAML map (e.g. `capabilities.native_hooks`) into `h`.
/// Uses the same length-prefix encoding as `m.native` so the two are domain-
/// separated by the call order rather than a prefix byte.
fn hash_native_capability_map(
    h: &mut Sha256,
    map: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> anyhow::Result<()> {
    h.update((map.len() as u64).to_le_bytes());
    for (key, value) in map {
        update_len_prefixed(h, key.as_bytes());
        let serialized = serde_yaml::to_string(value)
            .map_err(|e| anyhow::anyhow!("serializing native capability key '{key}': {e}"))?;
        update_len_prefixed(h, serialized.as_bytes());
    }
    Ok(())
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
    /// Entries prune attempted to unlink but could not (a non-fatal symlink
    /// removal failure — see [`remove_link`]). Kept separate from `removed` so
    /// the report never claims work it didn't do (#255).
    pub failed: Vec<PathBuf>,
}

/// Prune cache folders under `cache_root` according to `mode` (#246).
///
/// `hashing` selects how a folder is recognized as belonging to the *current*
/// generation — the test that `StaleOnly` and `OlderThan` key off:
/// - [`HashingMode::Loose`] — every shape folder is current (no version axis);
///   `current_version` is ignored. `StaleOnly` therefore removes nothing but
///   `*.tmp`, and only `All`/`OlderThan` trim shapes.
/// - [`HashingMode::Normal`] — `current_version` is the live `version_mm`
///   segment (e.g. `1.2`); the direct child of that name is current.
/// - [`HashingMode::Strict`] — a `{VERSION_TAG}-…` folder is current;
///   `current_version` is ignored.
///
/// Behavior:
/// - `*.tmp` staging dirs are always removed (orphaned partial writes).
/// - `StaleOnly`: removes folders that aren't current for `hashing`.
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
    hashing: HashingMode,
    current_version: Option<&str>,
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

        let is_current = entry
            .file_name()
            .to_str()
            .is_some_and(|name| match hashing {
                HashingMode::Loose => true,
                HashingMode::Normal => current_version == Some(name),
                HashingMode::Strict => name.starts_with(&format!("{VERSION_TAG}-")),
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
///
/// A failed unlink is non-fatal: pruning continues over the rest of the cache
/// root. But the failure is recorded in `report.failed` and logged, never
/// reported as `removed` — claiming a removal that didn't happen misleads the
/// caller (#255). Under `dry_run` no unlink is attempted, so the entry is
/// always reported as an intended removal.
fn remove_link(p: &Path, dry_run: bool, report: &mut PruneReport) {
    if dry_run {
        report.removed.push(p.to_path_buf());
        return;
    }
    match std::fs::remove_file(p) {
        Ok(()) => report.removed.push(p.to_path_buf()),
        Err(e) => {
            tracing::warn!(path = %p.display(), error = %e, "failed to unlink cache symlink; skipping");
            report.failed.push(p.to_path_buf());
        }
    }
}

/// Remove cache subdirectories whose newest mtime is older than `older_than`.
/// `*.tmp` staging directories are removed regardless of age — they represent
/// orphaned partial writes from a previous crashed `materialize` call.
///
/// gc operates on the *direct children* of `cache_root`. In
/// [`HashingMode::Normal`] those children are `<version_mm>` generation dirs, so
/// collection happens at generation granularity: a `<version_mm>` dir is reaped
/// only once *every* shape under it ages out. Per-shape collection within a live
/// generation is a follow-up (see #246 deferred work).
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

    fn empty_shape() -> String {
        shape(&BTreeSet::new(), &BTreeSet::new())
    }

    #[test]
    fn strict_folder_name_is_version_tag_plus_hash() {
        let name = folder_name(HashingMode::Strict, &empty_shape(), "deadbeef");
        assert_eq!(name, format!("{VERSION_TAG}-deadbeef"));
    }

    #[test]
    fn loose_folder_name_is_bare_shape() {
        let s = empty_shape();
        // Loose mode is selection-addressed only: the folder is exactly the
        // shape, with no version segment and no content-hash suffix.
        let name = folder_name(HashingMode::Loose, &s, "ignored-hash");
        assert_eq!(name, s);
    }

    #[test]
    fn normal_folder_name_nests_shape_under_version_mm() {
        let s = empty_shape();
        // Normal mode nests the shape under the major.minor version; the content
        // hash is not part of the name (it lives in the manifest dotfile).
        let name = folder_name(HashingMode::Normal, &s, "ignored-hash");
        assert_eq!(name, format!("{}/{}", version_mm(), s));
    }

    #[test]
    fn version_mm_is_two_leading_components() {
        // version_mm takes the leading `major.minor` of PKG_VERSION (baked at
        // build time), or degrades to whatever leading components exist.
        let mm = version_mm();
        assert!(
            PKG_VERSION.starts_with(&mm),
            "version_mm ({mm}) must be a prefix of PKG_VERSION ({PKG_VERSION})"
        );
        let dots = PKG_VERSION.split('.').count();
        if dots >= 2 {
            let expected = {
                let mut parts = PKG_VERSION.split('.');
                let major = parts.next().unwrap();
                let minor = parts.next().unwrap();
                format!("{major}.{minor}")
            };
            assert_eq!(mm, expected, "version_mm is exactly major.minor");
        }
    }

    #[test]
    fn shape_is_deterministic_and_12_hex() {
        let mut tags = BTreeSet::new();
        tags.insert("rust".to_string());
        tags.insert("backend".to_string());
        let bundles = BTreeSet::from(["core".to_string()]);
        let a = shape(&tags, &bundles);
        let b = shape(&tags, &bundles);
        assert_eq!(a, b, "shape must be deterministic");
        assert_eq!(a.len(), 12, "shape is 12 hex chars");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "shape is lowercase hex"
        );
    }

    #[test]
    fn shape_is_order_independent() {
        // BTreeSet iterates sorted, so insertion order cannot perturb the digest.
        let mut a = BTreeSet::new();
        a.insert("z".to_string());
        a.insert("a".to_string());
        a.insert("m".to_string());
        let mut b = BTreeSet::new();
        b.insert("a".to_string());
        b.insert("m".to_string());
        b.insert("z".to_string());
        assert_eq!(shape(&a, &BTreeSet::new()), shape(&b, &BTreeSet::new()));
    }

    #[test]
    fn shape_does_not_alias_tag_and_bundle_namespaces() {
        // A tag named "foo" and a bundle named "foo" must not collide: the same
        // string in the tag group vs the bundle group yields distinct shapes.
        let foo = || BTreeSet::from(["foo".to_string()]);
        let tag_foo = shape(&foo(), &BTreeSet::new());
        let bundle_foo = shape(&BTreeSet::new(), &foo());
        assert_ne!(
            tag_foo, bundle_foo,
            "tag/bundle namespaces must not alias into one shape"
        );
    }

    #[test]
    fn shape_distinguishes_grouping_boundary() {
        // {tags: [a, b], bundles: []} vs {tags: [a], bundles: [b]} must differ —
        // the length-prefixed grouping prevents the boundary from sliding.
        let split_left = shape(
            &BTreeSet::from(["a".to_string(), "b".to_string()]),
            &BTreeSet::new(),
        );
        let split_across = shape(
            &BTreeSet::from(["a".to_string()]),
            &BTreeSet::from(["b".to_string()]),
        );
        assert_ne!(split_left, split_across);
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
        let report = prune(&missing, PruneMode::All, HashingMode::Strict, None, false).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.kept, 0);
    }

    #[test]
    fn prune_all_removes_every_folder() {
        let tmp = tempfile::tempdir().unwrap();
        touch_dir(tmp.path(), &format!("{VERSION_TAG}-aaaa"));
        touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(tmp.path(), PruneMode::All, HashingMode::Strict, None, false).unwrap();
        assert_eq!(report.removed.len(), 2);
        assert_eq!(report.kept, 0);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn prune_stale_only_keeps_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let current = touch_dir(tmp.path(), &format!("{VERSION_TAG}-aaaa"));
        let stale = touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(
            tmp.path(),
            PruneMode::StaleOnly,
            HashingMode::Strict,
            None,
            false,
        )
        .unwrap();
        assert_eq!(report.kept, 1);
        assert!(report.removed.contains(&stale));
        assert!(!report.removed.contains(&current));
        assert!(current.exists());
        assert!(!stale.exists());
    }

    #[test]
    fn prune_stale_only_keeps_normal_version_folder() {
        // #246: a normal-mode generation dir (e.g. "1.2") has no `{VERSION_TAG}-`
        // prefix, so `StaleOnly` must be told the current `version_mm` segment
        // explicitly or it would wrongly sweep the live config dir.
        let tmp = tempfile::tempdir().unwrap();
        let current = touch_dir(tmp.path(), "1.2");
        let stale = touch_dir(tmp.path(), "1.1");
        let report = prune(
            tmp.path(),
            PruneMode::StaleOnly,
            HashingMode::Normal,
            Some("1.2"),
            false,
        )
        .unwrap();
        assert_eq!(report.kept, 1);
        assert!(current.exists(), "current version folder must survive");
        assert!(!stale.exists(), "older version folder is swept");
        assert!(report.removed.contains(&stale));
    }

    #[test]
    fn prune_stale_only_in_loose_mode_keeps_all_shapes() {
        // #246: loose mode has no version axis — every shape folder is current,
        // so `StaleOnly` sweeps nothing (only `All`/`OlderThan` trim shapes).
        let tmp = tempfile::tempdir().unwrap();
        let a = touch_dir(tmp.path(), "aaaaaaaaaaaa");
        let b = touch_dir(tmp.path(), "bbbbbbbbbbbb");
        let report = prune(
            tmp.path(),
            PruneMode::StaleOnly,
            HashingMode::Loose,
            None,
            false,
        )
        .unwrap();
        assert_eq!(report.kept, 2, "loose mode treats every shape as current");
        assert!(report.removed.is_empty());
        assert!(a.exists());
        assert!(b.exists());
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
            let report = prune(tmp.path(), mode, HashingMode::Strict, None, false).unwrap();
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
        let report = prune(
            tmp.path(),
            PruneMode::StaleOnly,
            HashingMode::Strict,
            None,
            false,
        )
        .unwrap();
        assert!(report.removed.contains(&staging));
        assert!(!staging.exists());
    }

    #[test]
    fn prune_dry_run_mutates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let a = touch_dir(tmp.path(), &format!("{VERSION_TAG}-aaaa"));
        let b = touch_dir(tmp.path(), "0.0.1-old-bbbb");
        let report = prune(tmp.path(), PruneMode::All, HashingMode::Strict, None, true).unwrap();
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
            HashingMode::Strict,
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

        let report = prune(
            &cache_root,
            PruneMode::All,
            HashingMode::Strict,
            None,
            false,
        )
        .unwrap();
        assert!(report.removed.contains(&link));
        // The link is gone; the target and its contents survive.
        assert!(!link.exists());
        assert!(outside.join("keep.txt").exists());
    }

    #[test]
    fn prune_failed_unlink_recorded_as_failed_not_removed() {
        // #255: a symlink whose unlink fails must NOT be reported as removed —
        // that claimed work the prune never did. remove_link stays non-fatal
        // (pruning continues) but surfaces the failure in `failed`, not `removed`.
        let mut report = PruneReport::default();
        // A path under a non-existent parent: the real unlink fails (NotFound),
        // exercising the failure branch deterministically without perms hacks.
        let missing = Path::new("/nonexistent-llmenv-255-dir/dangling-link");
        remove_link(missing, false, &mut report);
        assert!(
            report.removed.is_empty(),
            "a failed unlink must never be reported as removed"
        );
        assert_eq!(
            report.failed,
            vec![missing.to_path_buf()],
            "the failed unlink is surfaced in `failed`"
        );
    }

    #[test]
    fn prune_dry_run_symlink_reports_intended_removal() {
        // Dry-run attempts no unlink, so nothing can fail: the link is still
        // reported as a would-remove entry and never lands in `failed` (#255
        // must not regress the dry-run contract).
        let mut report = PruneReport::default();
        let missing = Path::new("/nonexistent-llmenv-255-dir/dangling-link");
        remove_link(missing, true, &mut report);
        assert_eq!(report.removed, vec![missing.to_path_buf()]);
        assert!(report.failed.is_empty());
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

        // A shape from an arbitrary tag set, used to exercise folder_name layout.
        fn arb_shape() -> impl Strategy<Value = String> {
            proptest::collection::btree_set("[a-z]{1,8}", 0..4)
                .prop_map(|tags| shape(&tags, &BTreeSet::new()))
        }

        proptest! {
            #[test]
            fn folder_name_strict_always_has_version_tag(
                hash in "[a-f0-9]{64}", s in arb_shape()
            ) {
                let name = folder_name(HashingMode::Strict, &s, &hash);
                prop_assert!(name.starts_with(&format!("{VERSION_TAG}-")),
                    "strict mode must always prefix with VERSION_TAG");
                prop_assert!(name.ends_with(&hash), "strict mode must suffix with the hash");
            }

            #[test]
            fn folder_name_loose_is_exactly_shape(
                hash in "[a-f0-9]{64}", s in arb_shape()
            ) {
                // Loose mode ignores both the version axis and the content hash.
                prop_assert_eq!(folder_name(HashingMode::Loose, &s, &hash), s);
            }

            #[test]
            fn folder_name_normal_nests_shape_and_ignores_hash(
                hash1 in "[a-f0-9]{64}", hash2 in "[a-f0-9]{64}", s in arb_shape()
            ) {
                prop_assume!(hash1 != hash2);
                let name1 = folder_name(HashingMode::Normal, &s, &hash1);
                let name2 = folder_name(HashingMode::Normal, &s, &hash2);
                // Normal mode ignores the content hash: same shape → same folder.
                prop_assert_eq!(&name1, &name2, "normal mode must ignore the hash argument");
                prop_assert_eq!(name1, format!("{}/{}", version_mm(), s),
                    "normal mode nests <shape> under <version_mm>");
            }

            #[test]
            fn shape_is_always_12_hex(
                tags in proptest::collection::btree_set("[a-z]{1,8}", 0..5),
                bundles in proptest::collection::btree_set("[a-z]{1,8}", 0..5),
            ) {
                let s = shape(&tags, &bundles);
                prop_assert_eq!(s.len(), 12, "shape is always 12 chars");
                prop_assert!(s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                    "shape is lowercase hex");
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

    // #292: editing a native: key must invalidate the hash.
    #[test]
    fn hash_manifest_changes_when_native_changes() {
        use std::collections::BTreeMap;

        let mut native_a: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        native_a.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("statusLine: hello").unwrap(),
        );
        let mut native_b: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        native_b.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("statusLine: world").unwrap(),
        );

        let manifest_a = MergedManifest {
            native: native_a,
            ..MergedManifest::default()
        };
        let manifest_b = MergedManifest {
            native: native_b,
            ..MergedManifest::default()
        };

        assert_ne!(
            hash_manifest(&manifest_a).unwrap(),
            hash_manifest(&manifest_b).unwrap(),
            "changing a native: value must produce a different hash"
        );
    }

    // Editing a capabilities.native_hooks key must invalidate the hash.
    #[test]
    fn hash_manifest_changes_when_native_hooks_changes() {
        use crate::config::Capabilities;
        use std::collections::BTreeMap;

        let mut hooks_a: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        hooks_a.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("key: value-a").unwrap(),
        );
        let mut hooks_b: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        hooks_b.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("key: value-b").unwrap(),
        );

        let manifest_a = MergedManifest {
            capabilities: Capabilities {
                native_hooks: hooks_a,
                ..Capabilities::default()
            },
            ..MergedManifest::default()
        };
        let manifest_b = MergedManifest {
            capabilities: Capabilities {
                native_hooks: hooks_b,
                ..Capabilities::default()
            },
            ..MergedManifest::default()
        };

        assert_ne!(
            hash_manifest(&manifest_a).unwrap(),
            hash_manifest(&manifest_b).unwrap(),
            "changing a capabilities.native_hooks value must produce a different hash"
        );
    }

    // Editing a capabilities.native_plugins key must invalidate the hash.
    #[test]
    fn hash_manifest_changes_when_native_plugins_changes() {
        use crate::config::Capabilities;
        use std::collections::BTreeMap;

        let mut plugins_a: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        plugins_a.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("key: value-a").unwrap(),
        );
        let mut plugins_b: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        plugins_b.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("key: value-b").unwrap(),
        );

        let manifest_a = MergedManifest {
            capabilities: Capabilities {
                native_plugins: plugins_a,
                ..Capabilities::default()
            },
            ..MergedManifest::default()
        };
        let manifest_b = MergedManifest {
            capabilities: Capabilities {
                native_plugins: plugins_b,
                ..Capabilities::default()
            },
            ..MergedManifest::default()
        };

        assert_ne!(
            hash_manifest(&manifest_a).unwrap(),
            hash_manifest(&manifest_b).unwrap(),
            "changing a capabilities.native_plugins value must produce a different hash"
        );
    }

    // Editing a capabilities.native_mcp key must invalidate the hash.
    #[test]
    fn hash_manifest_changes_when_native_mcp_changes() {
        use crate::config::Capabilities;
        use std::collections::BTreeMap;

        let mut mcp_a: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        mcp_a.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("key: value-a").unwrap(),
        );
        let mut mcp_b: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        mcp_b.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("key: value-b").unwrap(),
        );

        let manifest_a = MergedManifest {
            capabilities: Capabilities {
                native_mcp: mcp_a,
                ..Capabilities::default()
            },
            ..MergedManifest::default()
        };
        let manifest_b = MergedManifest {
            capabilities: Capabilities {
                native_mcp: mcp_b,
                ..Capabilities::default()
            },
            ..MergedManifest::default()
        };

        assert_ne!(
            hash_manifest(&manifest_a).unwrap(),
            hash_manifest(&manifest_b).unwrap(),
            "changing a capabilities.native_mcp value must produce a different hash"
        );
    }

    // Hashing is stable: the same manifest always produces the same hash.
    #[test]
    fn hash_manifest_native_is_stable() {
        use std::collections::BTreeMap;

        let mut native: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        native.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("statusLine: stable").unwrap(),
        );
        let manifest = MergedManifest {
            native,
            ..MergedManifest::default()
        };

        assert_eq!(
            hash_manifest(&manifest).unwrap(),
            hash_manifest(&manifest).unwrap(),
            "hash_manifest must be deterministic"
        );
    }
}
