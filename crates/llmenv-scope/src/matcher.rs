use llmenv_config::{ContentScope, HostScope, NetworkScope, UserScope};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Resolved project (discovered from `.llmenv.yaml` walking upward from cwd).
/// All fields default permissively; malformed YAML is logged as a warning
/// and yields a minimal project with defaults (cwd folder name for id/name).
#[derive(Debug, Clone)]
pub struct ResolvedProject {
    // `root`/`id` are read externally (`task::project` discovers the project
    // root via `discover_project`), so both are `pub`. Every other field is
    // only ever consumed inside this crate.
    pub root: std::path::PathBuf,
    pub id: String,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) enable_bundles: Vec<String>,
    /// Bundle names this scope removes from the firing set even if a lower-
    /// precedence scope's tag or `enable_bundles` turned them on (#194).
    /// Disable always wins, including within this same scope.
    pub(crate) disable_bundles: Vec<String>,
    /// Keys from the marker file not matching any declared field.
    pub(crate) unknown_fields: Vec<String>,
}

/// Schema for the body of `.llmenv.yaml` (project marker file).
/// All fields optional; an empty file is valid.
#[derive(Debug, Default, Deserialize)]
struct ProjectFile {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    enable_bundles: Vec<String>,
    #[serde(default)]
    disable_bundles: Vec<String>,
    /// Capture unknown fields for warning emission.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Env {
    pub hostname: String,
    pub user: String,
    pub cwd: String,
    pub gateway_mac: Option<String>,
    /// User's home directory. The `.llmenv.yaml` discovery walk stops at
    /// this boundary so a marker file dropped above $HOME (e.g. `/tmp` on a
    /// shared host) cannot be picked up.
    pub home: Option<std::path::PathBuf>,
    /// Target OS triple as reported by `std::env::consts::OS`. Used to
    /// auto-activate the OS as a tag (`linux`, `macos`, `windows`, etc.).
    /// Empty string when not set (tests, fallback).
    pub os: String,
    /// Tags from `$LLMENV_EXTRA_TAGS` (comma-separated), unioned into the
    /// active tag set regardless of whether a `.llmenv.yaml` is present —
    /// the escape hatch for activating tags without a committed project
    /// marker (#1020).
    pub extra_tags: Vec<String>,
}

/// 30-second TTL cache for [`Env::detect`]. Hostname, user, OS never change
/// mid-session. Gateway MAC only changes on network switch — ~30s staleness is
/// harmless.
struct CachedEnv {
    detected: Instant,
    env: Env,
}

static ENV_CACHE: Mutex<Option<CachedEnv>> = Mutex::new(None);

impl Env {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Detect environment, returning a cached result if fresher than 30 s.
    /// Detects the gateway MAC (route+arp subprocess forks); prefer
    /// [`Env::detect_for_config`] on the hook path, which skips those forks when
    /// no network scope can match.
    #[must_use]
    pub fn detect() -> Self {
        if let Ok(lock) = ENV_CACHE.lock()
            && let Some(cached) = lock.as_ref()
            && cached.detected.elapsed() < Duration::from_secs(30)
        {
            return cached.env.clone();
        }
        let env = Self::detect_fresh(true);
        if let Ok(mut lock) = ENV_CACHE.lock() {
            *lock = Some(CachedEnv {
                detected: Instant::now(),
                env: env.clone(),
            });
        }
        env
    }

    /// Detect environment for a specific config. Gateway-MAC detection shells out
    /// to `route`+`arp` (macOS) / `ip route`+`ip neigh` (Linux) — two subprocess
    /// forks on every call. Nothing can match on the gateway MAC unless a network
    /// scope is declared, so when there are none this skips those forks entirely.
    /// Each hook-run is a fresh process (the 30s cache never warms on that path),
    /// so on the common no-network-scope config this removes the dominant
    /// remaining hook-run subprocess cost. Not cached: without the forks the
    /// detection is cheap, and caching a MAC-less env could shadow a later
    /// [`detect`] that needs it within the same process.
    #[must_use]
    pub fn detect_for_config(config: &llmenv_config::Config) -> Self {
        if config.scope.network.is_empty() {
            Self::detect_fresh(false)
        } else {
            Self::detect()
        }
    }

    /// Fresh env detection. `need_gateway_mac` gates the route+arp forks; the
    /// hostname (uname syscall), user, and cwd probes always run.
    fn detect_fresh(need_gateway_mac: bool) -> Self {
        let hostname = detect_hostname().unwrap_or_else(|| {
            tracing::warn!("hostname detection failed; host-scope matching disabled");
            String::new()
        });
        let user = std::env::var("USER").unwrap_or_else(|_| {
            tracing::warn!("$USER unset; user-scope matching disabled");
            String::new()
        });
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| {
                tracing::warn!("current_dir() unavailable; project-scope matching disabled");
                String::new()
            });
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(std::path::PathBuf::from);
        Self {
            // Hostname comparison is case-insensitive — `hostname(1)` and
            // /etc/hostname may differ in case across hosts.
            hostname: hostname.to_ascii_lowercase(),
            user,
            cwd,
            gateway_mac: need_gateway_mac
                .then(super::network::detect_gateway_mac)
                .flatten(),
            home,
            os: std::env::consts::OS.to_string(),
            extra_tags: extra_tags_from_env(),
        }
    }
}

/// Read and validate `$LLMENV_EXTRA_TAGS` from the process environment.
/// Pulled out of [`Env::detect_fresh`] so callers that need only this one
/// env-derived tag source — not a full [`Env::detect`] — can read it without
/// paying for hostname/cwd/gateway-MAC detection (#1538: the statusline
/// re-reads this live on every render, and a full `Env::detect` would also
/// fork `route`/`arp` whenever a network scope is configured).
#[must_use]
pub fn extra_tags_from_env() -> Vec<String> {
    match std::env::var("LLMENV_EXTRA_TAGS") {
        Ok(raw) => parse_extra_tags(&raw),
        Err(std::env::VarError::NotPresent) => Vec::new(),
        Err(std::env::VarError::NotUnicode(_)) => {
            tracing::warn!("$LLMENV_EXTRA_TAGS is not valid UTF-8; extra tags disabled");
            Vec::new()
        }
    }
}

/// Parse `$LLMENV_EXTRA_TAGS`'s comma-separated format (matching
/// `LLMENV_ACTIVE_TAGS`'s own output format). Empty segments (from a blank
/// value, leading/trailing commas, or repeated commas) are dropped; each
/// remaining tag is trimmed of surrounding whitespace, then run through
/// [`sanitize_tags`] (#1035).
fn parse_extra_tags(raw: &str) -> Vec<String> {
    let tags = raw
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect();
    sanitize_tags(tags, "$LLMENV_EXTRA_TAGS")
}

/// Charset every tag must satisfy, regardless of source: non-empty,
/// alphanumeric plus `-`/`_`. Matches `hook_run::validate_tag`'s recall-query
/// charset — a tag outside it trips that validation at ICM-recall time, and
/// the resulting error silently disables memory recall/store *and*
/// session-log for the whole session (#1035), so untrusted sources are
/// filtered here, at creation, rather than left to fail downstream.
pub fn is_valid_tag_charset(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Max bytes for a single tag from an untrusted source. Charset alone
/// doesn't bound length, and an oversized tag still bloats
/// `LLMENV_ACTIVE_TAGS` and every ICM recall keyword built from it (#1035).
const MAX_TAG_LEN: usize = 64;

/// Max tags accepted from a single untrusted source in one ingest pass. A
/// hand-written `.llmenv.yaml` never approaches this; a generated/scripted
/// `$LLMENV_EXTRA_TAGS` trivially could, and every active tag becomes one
/// `Action::RecallTag` per turn (#1035).
const MAX_TAGS_PER_SOURCE: usize = 64;

/// Drop tags that fail [`is_valid_tag_charset`] or exceed [`MAX_TAG_LEN`],
/// then cap the remainder to [`MAX_TAGS_PER_SOURCE`] — logging a warning
/// naming each rejected tag and any overflow. `source` labels the origin in
/// the warning (e.g. `.llmenv.yaml` or `$LLMENV_EXTRA_TAGS`). Also used for
/// bundle names (`enable_bundles`/`disable_bundles`), which share the same
/// charset rule and the same downstream `hook_run::validate_bundle` failure
/// mode as tags.
pub(crate) fn sanitize_tags(raw: Vec<String>, source: &str) -> Vec<String> {
    // #1345: these report a tag the *user* wrote never taking effect, so they
    // have to be `eprintln!`. The default `EnvFilter` is `ERROR`, so a
    // `tracing::warn!` here reached neither stderr nor the log file — the tag
    // silently did nothing and no bundle gated on it ever fired.
    let mut valid = Vec::with_capacity(raw.len());
    for tag in raw {
        if !is_valid_tag_charset(&tag) {
            eprintln!("warning: {source}: dropping tag {tag:?} (only alphanumeric, -, _ allowed)");
        } else if tag.len() > MAX_TAG_LEN {
            eprintln!("warning: {source}: dropping tag {tag:?} (exceeds {MAX_TAG_LEN}-byte limit)");
        } else {
            valid.push(tag);
        }
    }
    if valid.len() > MAX_TAGS_PER_SOURCE {
        eprintln!(
            "warning: {source}: {} tags exceeds cap of {MAX_TAGS_PER_SOURCE}; keeping the first {MAX_TAGS_PER_SOURCE}",
            valid.len()
        );
        valid.truncate(MAX_TAGS_PER_SOURCE);
    }
    valid
}

/// Cap the *union* of tags across every source (`config.yaml`'s network/
/// host/user/content scopes, `.llmenv.yaml`, `$LLMENV_EXTRA_TAGS`, `env.os`)
/// to [`MAX_TAGS_PER_SOURCE`] — [`sanitize_tags`] already bounds each source
/// individually, but several active scopes plus a large `.llmenv.yaml` plus
/// `$LLMENV_EXTRA_TAGS` can still combine into several hundred tags, each
/// becoming one `Action::RecallTag` per turn (#1041). Reuses
/// `MAX_TAGS_PER_SOURCE` as the aggregate bound too, rather than a second
/// magic number: the same "one source's worth" ceiling that's already
/// accepted as tolerable for a single source is exactly the ceiling worth
/// enforcing on the combination.
///
/// Keeps the alphabetically-first entries (`BTreeSet`'s natural iteration
/// order) — the same "keep the first N, warn about the rest" policy
/// `sanitize_tags` uses per-source, just applied to the union so no one
/// source is treated as more important than another when something has to
/// be dropped.
pub(crate) fn cap_aggregate_tags(tags: BTreeSet<String>) -> BTreeSet<String> {
    let total = tags.len();
    if total <= MAX_TAGS_PER_SOURCE {
        return tags;
    }
    tracing::warn!(
        "aggregate tag count {total} exceeds cap of {MAX_TAGS_PER_SOURCE} across all sources \
         combined; keeping the alphabetically-first {MAX_TAGS_PER_SOURCE}"
    );
    tags.into_iter().take(MAX_TAGS_PER_SOURCE).collect()
}

fn detect_hostname() -> Option<String> {
    // uname(2) syscall rather than spawning the `hostname` binary: each hook-run
    // is a fresh process, so the process-static Env cache never helps the hook
    // path — the subprocess fork/exec ran on every hook and dominated scope
    // evaluation (~15ms/event). rustix reads the kernel nodename with one
    // syscall, no subprocess, no unsafe.
    let nodename = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    let trimmed = nodename.trim();
    if trimmed.is_empty() {
        tracing::warn!("uname nodename empty; host-scope matching disabled");
        return None;
    }
    Some(trimmed.to_string())
}

#[must_use]
pub(crate) fn matches_network(s: &NetworkScope, env: &Env) -> bool {
    let Some(want) = s.r#match.gateway_mac.as_deref() else {
        // ssid/cidr are not yet supported for matching; without gateway_mac we cannot match.
        return false;
    };
    env.gateway_mac
        .as_deref()
        .is_some_and(|got| got.eq_ignore_ascii_case(want))
}

pub(crate) fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern_lower = pattern.to_ascii_lowercase();
    let text_lower = text.to_ascii_lowercase();

    // ponytail: simple `*` glob, no `?` or `[..]`. Upgrade if needed for complex patterns.
    if !pattern_lower.contains('*') {
        return pattern_lower == text_lower;
    }

    let parts: Vec<&str> = pattern_lower.split('*').collect();

    // First part must match at the start (unless empty, which means pattern started with *)
    if !parts[0].is_empty() && !text_lower.starts_with(parts[0]) {
        return false;
    }

    // Last part must match at the end (unless empty, which means pattern ended with *)
    let last_part = parts[parts.len() - 1];
    if !last_part.is_empty() && !text_lower.ends_with(last_part) {
        return false;
    }

    // Prefix and suffix must not overlap: text must be long enough for both
    if text_lower.len() < parts[0].len() + last_part.len() {
        return false;
    }

    // Middle parts must appear in order between prefix and suffix
    let mut pos = parts[0].len();
    for &part in &parts[1..parts.len() - 1] {
        if let Some(idx) = text_lower[pos..].find(part) {
            pos += idx + part.len();
        } else {
            return false;
        }
    }

    true
}

#[must_use]
pub(crate) fn matches_host(s: &HostScope, env: &Env) -> bool {
    s.r#match
        .hostname
        .as_deref()
        .is_some_and(|h| glob_matches(h, &env.hostname))
}

#[must_use]
pub(crate) fn matches_user(s: &UserScope, env: &Env) -> bool {
    s.r#match.user.as_deref().is_some_and(|u| u == env.user)
}

/// Evaluate every content scope against `cwd` in a single directory walk.
///
/// Each content scope previously triggered its own `walkdir` traversal
/// (#703) — N active content scopes meant N full tree walks on every hook
/// fire and export. Here all globs are compiled up front and evaluated
/// per entry against a single walk; a scope drops out of the pending set as
/// soon as it matches, and the walk ends early once none remain pending.
///
/// Returns the `id`s of scopes whose glob matched.
#[must_use]
pub(crate) fn matches_content_all<'a>(
    scopes: &'a [ContentScope],
    cwd: &std::path::Path,
) -> std::collections::BTreeSet<&'a str> {
    let mut pending: Vec<(&str, globset::GlobMatcher, Option<usize>)> = scopes
        .iter()
        .filter_map(|s| match globset::Glob::new(&s.r#match.glob) {
            Ok(glob) => Some((s.id.as_str(), glob.compile_matcher(), s.r#match.depth)),
            Err(_) => {
                tracing::debug!("content scope {}: invalid glob pattern", s.id);
                None
            }
        })
        .collect();

    let mut matched = std::collections::BTreeSet::new();
    if pending.is_empty() {
        return matched;
    }

    // Cap the walk at the loosest per-scope depth limit; any scope with no
    // limit forces an unbounded walk (short-circuits to `None` below).
    let max_depth = pending
        .iter()
        .map(|(_, _, d)| *d)
        .try_fold(0usize, |acc, d| d.map(|d| acc.max(d)));

    let mut walker = walkdir::WalkDir::new(cwd).follow_links(false);
    if let Some(depth) = max_depth {
        walker = walker.max_depth(depth);
    }

    for entry in walker {
        if pending.is_empty() {
            break;
        }
        let Ok(entry) =
            entry.inspect_err(|e| tracing::warn!(error = %e, "walkdir entry error; skipping"))
        else {
            continue;
        };
        if entry.file_type().is_dir() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(cwd) else {
            // walkdir only yields paths under root, so this is a walkdir bug
            debug_assert!(
                false,
                "walkdir path {:?} not under root {:?}",
                entry.path(),
                cwd,
            );
            tracing::warn!(
                path = ?entry.path(),
                cwd = ?cwd,
                "walkdir yielded path outside root; skipping",
            );
            continue;
        };
        let entry_depth = entry.depth();
        pending.retain(|(id, matcher, depth)| {
            if depth.is_some_and(|d| entry_depth > d) {
                return true;
            }
            if matcher.is_match(relative) {
                matched.insert(*id);
                false
            } else {
                true
            }
        });
    }
    matched
}

/// Discover project by walking cwd upward looking for `.llmenv.yaml`.
/// When found, parse and return a `ResolvedProject` with all fields resolved
/// (defaults applied, unknown fields collected). If YAML is malformed, log a
/// warning and return a minimal `ResolvedProject` with id/name from the
/// folder basename.
///
/// The walk is bounded at `$HOME`: a marker at `~/.llmenv.yaml` activates,
/// but the walk does not ascend above home. This prevents a hostile marker
/// dropped in e.g. `/tmp` (on a shared host) or `/Volumes/...` from being
/// picked up. When `$HOME` is unknown, only the cwd itself is checked.
#[must_use]
pub fn discover_project(env: &Env) -> Option<ResolvedProject> {
    let mut cur = std::path::PathBuf::from(&env.cwd);
    loop {
        let marker_path = cur.join(".llmenv.yaml");
        if marker_path.exists() {
            let pf = read_project_file(&marker_path);
            let basename = cur
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("llmenv")
                .to_string();
            let id = pf.id.unwrap_or_else(|| basename.clone());
            let name = pf.name.unwrap_or_else(|| basename.clone());
            let unknown_fields: Vec<String> = pf
                .extra
                .keys()
                .filter(|k| {
                    !matches!(
                        k.as_str(),
                        "id" | "name"
                            | "description"
                            | "tags"
                            | "enable_bundles"
                            | "disable_bundles"
                    )
                })
                .cloned()
                .collect();
            return Some(ResolvedProject {
                root: cur,
                id,
                name,
                description: pf.description,
                tags: sanitize_tags(pf.tags, ".llmenv.yaml"),
                enable_bundles: sanitize_tags(pf.enable_bundles, ".llmenv.yaml enable_bundles"),
                disable_bundles: sanitize_tags(pf.disable_bundles, ".llmenv.yaml disable_bundles"),
                unknown_fields,
            });
        }
        // Stop the walk once we've checked $HOME (or if home is unknown,
        // after checking only cwd). This blocks markers above home from
        // activating.
        match &env.home {
            Some(h) if cur == *h => break,
            None => break,
            _ => {}
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Maximum length (in bytes) for the project description. Anything longer
/// is truncated and a warning is logged. The description is surfaced into
/// LLM context chunks; a hard cap prevents a malformed or hostile marker
/// from bloating every prompt.
const MAX_DESCRIPTION_BYTES: usize = 1024;

/// Parse `.llmenv.yaml` file into a `ProjectFile`. Empty file → all defaults.
/// Malformed YAML → log warning and return defaults. The `description`
/// field is truncated to `MAX_DESCRIPTION_BYTES` if oversized.
fn read_project_file(path: &std::path::Path) -> ProjectFile {
    let Ok(body) = std::fs::read_to_string(path).inspect_err(|e| {
        tracing::warn!(path = %path.display(), error = %e, "failed to read project marker file; using defaults")
    }) else {
        return ProjectFile::default();
    };
    if body.trim().is_empty() {
        return ProjectFile::default();
    }
    match serde_yaml::from_str::<ProjectFile>(&body) {
        Ok(mut pf) => {
            if let Some(desc) = pf.description.as_mut()
                && desc.len() > MAX_DESCRIPTION_BYTES
            {
                tracing::warn!(
                    "project marker file {} has description >{} bytes; truncating",
                    path.display(),
                    MAX_DESCRIPTION_BYTES
                );
                // Truncate at a char boundary so the result remains valid UTF-8.
                let mut cut = MAX_DESCRIPTION_BYTES;
                while cut > 0 && !desc.is_char_boundary(cut) {
                    cut -= 1;
                }
                desc.truncate(cut);
            }
            pf
        }
        Err(e) => {
            tracing::warn!(
                "project marker file {} is not valid YAML: {e}; using defaults",
                path.display()
            );
            ProjectFile::default()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{
        ContentScope, Env, MAX_TAGS_PER_SOURCE, cap_aggregate_tags, discover_project, glob_matches,
        is_valid_tag_charset, matches_content_all, parse_extra_tags, sanitize_tags,
    };
    use proptest::prelude::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    #[test]
    fn parse_extra_tags_empty_string_yields_no_tags() {
        assert!(parse_extra_tags("").is_empty());
    }

    #[test]
    fn parse_extra_tags_single_tag() {
        assert_eq!(parse_extra_tags("rust"), vec!["rust"]);
    }

    #[test]
    fn parse_extra_tags_multiple_tags() {
        assert_eq!(parse_extra_tags("rust,office"), vec!["rust", "office"]);
    }

    #[test]
    fn parse_extra_tags_trims_whitespace() {
        assert_eq!(parse_extra_tags(" rust , office "), vec!["rust", "office"]);
    }

    #[test]
    fn parse_extra_tags_drops_empty_segments() {
        // Blank value, leading/trailing/repeated commas must not yield empty tags.
        assert_eq!(parse_extra_tags(",rust,,office,"), vec!["rust", "office"]);
        assert!(parse_extra_tags(",,").is_empty());
    }

    #[test]
    fn parse_extra_tags_drops_invalid_charset_segments() {
        // #1035: charset must be enforced at creation, not left to fail at
        // ICM-recall query time (where the failure silently disables memory
        // for the whole session).
        assert_eq!(
            parse_extra_tags("rust,my project,lang:rust,office"),
            vec!["rust", "office"]
        );
    }

    #[test]
    fn parse_extra_tags_caps_tag_count() {
        // #1035: a scripted/generated env var can trivially produce far more
        // tags than a hand-written .llmenv.yaml ever would; each becomes one
        // ICM recall query per turn, so the source must be capped.
        let raw = (0..(MAX_TAGS_PER_SOURCE + 10))
            .map(|i| format!("tag{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(parse_extra_tags(&raw).len(), MAX_TAGS_PER_SOURCE);
    }

    #[test]
    fn cap_aggregate_tags_below_cap_is_unchanged() {
        let tags: BTreeSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(cap_aggregate_tags(tags.clone()), tags);
    }

    #[test]
    fn cap_aggregate_tags_at_exactly_the_cap_is_unchanged() {
        let tags: BTreeSet<String> = (0..MAX_TAGS_PER_SOURCE)
            .map(|i| format!("t{i:03}"))
            .collect();
        assert_eq!(cap_aggregate_tags(tags.clone()), tags);
    }

    #[test]
    fn cap_aggregate_tags_over_the_cap_keeps_alphabetically_first_n() {
        // #1041: several sources, each within their own MAX_TAGS_PER_SOURCE
        // limit, can still union into more than MAX_TAGS_PER_SOURCE tags —
        // the aggregate cap keeps the alphabetically-first N.
        let tags: BTreeSet<String> = (0..(MAX_TAGS_PER_SOURCE + 10))
            .map(|i| format!("t{i:03}"))
            .collect();
        let capped = cap_aggregate_tags(tags);
        assert_eq!(capped.len(), MAX_TAGS_PER_SOURCE);
        assert!(capped.contains("t000"));
        assert!(!capped.contains(&format!("t{:03}", MAX_TAGS_PER_SOURCE + 9)));
    }

    #[test]
    fn is_valid_tag_charset_accepts_alphanumeric_hyphen_underscore() {
        assert!(is_valid_tag_charset("rust-lang_123"));
        assert!(!is_valid_tag_charset(""));
        assert!(!is_valid_tag_charset("has space"));
        assert!(!is_valid_tag_charset("lang:rust"));
        assert!(!is_valid_tag_charset("tag.dot"));
    }

    #[test]
    fn sanitize_tags_drops_tag_over_max_len() {
        // #1035: charset alone doesn't bound length.
        let at_limit = "a".repeat(super::MAX_TAG_LEN);
        let over_limit = "a".repeat(super::MAX_TAG_LEN + 1);
        assert_eq!(
            sanitize_tags(vec![at_limit.clone(), over_limit], "test"),
            vec![at_limit]
        );
    }

    fn content_scope(id: &str, glob: &str, depth: Option<usize>) -> ContentScope {
        ContentScope {
            id: id.to_string(),
            r#match: llmenv_config::ContentMatch {
                glob: glob.to_string(),
                depth,
            },
            tags: Vec::new(),
        }
    }

    fn write_project_file(temp_dir: &Path, body: &str) {
        let path = temp_dir.join(".llmenv.yaml");
        std::fs::write(&path, body).expect("write .llmenv.yaml");
    }

    /// Build an `Env` with cwd inside `temp_dir`, treating `temp_dir`'s
    /// parent as $HOME so the walk reaches markers at `temp_dir` (and
    /// upward as long as we're under the boundary).
    fn env_in(cwd: &Path, home: &Path) -> Env {
        Env {
            cwd: cwd.to_string_lossy().to_string(),
            home: Some(home.to_path_buf()),
            ..Env::empty()
        }
    }

    #[test]
    fn discovers_project_with_all_fields() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let yaml =
            "id: myapp\nname: MyApp\ndescription: Test app\ntags: [a, b]\nenable_bundles: [base]\n";
        write_project_file(temp_dir.path(), yaml);

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.id, "myapp");
        assert_eq!(project.name, "MyApp");
        assert_eq!(project.description, Some("Test app".to_string()));
        assert_eq!(project.tags, vec!["a", "b"]);
        assert_eq!(project.enable_bundles, vec!["base"]);
        assert!(project.unknown_fields.is_empty());
    }

    #[test]
    fn discovers_project_with_disable_bundles() {
        // #194
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let yaml = "id: myapp\nenable_bundles: [github-issues]\ndisable_bundles: [yaks]\n";
        write_project_file(temp_dir.path(), yaml);

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.enable_bundles, vec!["github-issues"]);
        assert_eq!(project.disable_bundles, vec!["yaks"]);
        assert!(project.unknown_fields.is_empty());
    }

    #[test]
    fn discover_project_drops_invalid_charset_tags() {
        // #1035: a hand-edited .llmenv.yaml is just as untrusted an ingest
        // point as $LLMENV_EXTRA_TAGS — an invalid tag here trips the same
        // ICM-recall failure and must be filtered at creation.
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let yaml = "id: myapp\ntags: [good, \"bad tag\", \"lang:rust\", also-good_1]\n";
        write_project_file(temp_dir.path(), yaml);

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.tags, vec!["good", "also-good_1"]);
    }

    #[test]
    fn discover_project_drops_invalid_charset_bundle_names() {
        // #1035: enable_bundles/disable_bundles hit the exact same
        // hook_run::validate_bundle failure mode as tags — same ingest
        // boundary, same sanitizer.
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let yaml = "id: myapp\nenable_bundles: [good, \"bad bundle\"]\ndisable_bundles: [\"lang:rust\", ok]\n";
        write_project_file(temp_dir.path(), yaml);

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.enable_bundles, vec!["good"]);
        assert_eq!(project.disable_bundles, vec!["ok"]);
    }

    #[test]
    fn empty_file_uses_defaults() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        write_project_file(temp_dir.path(), "");

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        let basename = temp_dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(project.id, basename.as_ref());
        assert_eq!(project.name, basename.as_ref());
        assert_eq!(project.description, None);
        assert!(project.tags.is_empty());
        assert!(project.enable_bundles.is_empty());
        assert!(project.disable_bundles.is_empty());
    }

    #[test]
    fn walks_upward_to_find_marker() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let root = temp_dir.path();
        let subdir = root.join("a").join("b");
        std::fs::create_dir_all(&subdir).expect("mkdir");
        write_project_file(root, "id: found\n");

        let env = env_in(&subdir, root);

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.id, "found");
        assert_eq!(project.root, root);
    }

    #[test]
    fn walk_stops_at_home_boundary() {
        // Marker is above $HOME (in an ancestor of home) — must not be
        // picked up even when cwd is below home.
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let above_home = temp_dir.path();
        let home = above_home.join("home");
        let workdir = home.join("project");
        std::fs::create_dir_all(&workdir).expect("mkdir");
        // Hostile marker above home.
        write_project_file(above_home, "id: hostile\n");

        let env = env_in(&workdir, &home);
        assert!(
            discover_project(&env).is_none(),
            "marker above $HOME must not activate"
        );
    }

    #[test]
    fn walk_finds_marker_at_home() {
        // Marker exactly at $HOME — must activate (boundary is inclusive).
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let home = temp_dir.path();
        let workdir = home.join("project");
        std::fs::create_dir_all(&workdir).expect("mkdir");
        write_project_file(home, "id: home-project\n");

        let env = env_in(&workdir, home);
        let project = discover_project(&env).expect("discover");
        assert_eq!(project.id, "home-project");
        assert_eq!(project.root, home);
    }

    #[test]
    fn no_walk_above_cwd_when_home_unknown() {
        // With no HOME, only cwd itself is checked — no upward walk.
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let root = temp_dir.path();
        let subdir = root.join("sub");
        std::fs::create_dir_all(&subdir).expect("mkdir");
        write_project_file(root, "id: parent\n");

        let env = Env {
            cwd: subdir.to_string_lossy().to_string(),
            ..Env::empty()
        };
        assert!(
            discover_project(&env).is_none(),
            "without HOME, walk must not ascend"
        );
    }

    #[test]
    fn returns_none_when_no_marker_found() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env);
        assert!(project.is_none());
    }

    #[test]
    fn malformed_yaml_uses_defaults() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        write_project_file(temp_dir.path(), "not: [valid: yaml");

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        let basename = temp_dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(project.id, basename.as_ref());
        assert_eq!(project.name, basename.as_ref());
    }

    #[test]
    fn long_description_is_truncated() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let huge = "a".repeat(super::MAX_DESCRIPTION_BYTES + 500);
        write_project_file(temp_dir.path(), &format!("description: \"{huge}\"\n"));

        let env = env_in(temp_dir.path(), temp_dir.path());
        let project = discover_project(&env).expect("discover");
        let desc = project.description.expect("description");
        assert!(
            desc.len() <= super::MAX_DESCRIPTION_BYTES,
            "description must be capped"
        );
    }

    #[test]
    fn captures_unknown_fields() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        write_project_file(
            temp_dir.path(),
            "id: test\nunknown_field: value\nanother: 42\n",
        );

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.unknown_fields.len(), 2);
        assert!(
            project
                .unknown_fields
                .contains(&"unknown_field".to_string())
        );
        assert!(project.unknown_fields.contains(&"another".to_string()));
    }

    #[test]
    fn glob_matches_exact() {
        assert!(glob_matches("localhost", "localhost"));
        assert!(glob_matches("example.com", "example.com"));
        assert!(!glob_matches("example.com", "other.com"));
    }

    #[test]
    fn glob_matches_case_insensitive() {
        assert!(glob_matches("LOCALHOST", "localhost"));
        assert!(glob_matches("Example.COM", "example.com"));
        assert!(glob_matches("localhost", "LOCALHOST"));
    }

    #[test]
    fn glob_matches_leading_wildcard() {
        assert!(glob_matches("*.example.com", "dev.example.com"));
        assert!(glob_matches("*.example.com", "prod.example.com"));
        assert!(glob_matches("*.example.com", "api.staging.example.com"));
        assert!(!glob_matches("*.example.com", "example.com"));
        assert!(!glob_matches("*.example.com", "example.org"));
    }

    #[test]
    fn glob_matches_trailing_wildcard() {
        assert!(glob_matches("host-*", "host-001"));
        assert!(glob_matches("host-*", "host-prod"));
        assert!(glob_matches("host-*", "host-"));
        assert!(!glob_matches("host-*", "other-001"));
    }

    #[test]
    fn glob_matches_multiple_wildcards() {
        assert!(glob_matches("*-prod-*", "web-prod-01"));
        assert!(glob_matches("*-prod-*", "api-prod-staging"));
        assert!(glob_matches("*-prod-*", "-prod-"));
        assert!(!glob_matches("*-prod-*", "web-dev-01"));
    }

    #[test]
    fn glob_matches_only_wildcard() {
        assert!(glob_matches("*", "localhost"));
        assert!(glob_matches("*", "any.host.example.com"));
        assert!(glob_matches("*", ""));
    }

    #[test]
    fn glob_matches_preserves_ordering() {
        assert!(glob_matches("*-prod-*-01", "web-prod-east-01"));
        assert!(!glob_matches("*-prod-*-01", "web-01-prod-east"));
    }

    #[test]
    fn glob_matches_overlapping_prefix_suffix() {
        // Critical: prefix and suffix must not overlap
        assert!(!glob_matches("abc*abc", "abc"));
        assert!(!glob_matches("abc*cd", "abcd"));
        assert!(!glob_matches("abcde*cde", "abcde"));
        assert!(!glob_matches("host*host", "host"));
        // Valid matches where prefix+suffix fits
        assert!(glob_matches("abc*abc", "abcXabc"));
        assert!(glob_matches("abc*cd", "abcXcd"));
    }

    #[test]
    fn glob_matches_exact_length_match() {
        // Pattern prefix+suffix exactly matches text length (no middle content)
        assert!(glob_matches("a*b", "ab"));
        assert!(glob_matches("host*prod", "hostprod"));
        assert!(!glob_matches("host*prod", "host"));
        assert!(glob_matches("abc*def", "abcdef")); // prefix+suffix fit exactly
        // Pattern with middle parts matching exactly
        assert!(glob_matches("a*b*c", "abc")); // a + nothing + b + nothing + c
        assert!(!glob_matches("a*x*c", "abc")); // a + nothing + x (missing) + nothing + c
    }

    #[test]
    fn matches_content_all_evaluates_every_scope_in_one_walk() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let root = temp_dir.path();
        std::fs::write(root.join("main.rs"), "").expect("write");
        std::fs::write(root.join("readme.md"), "").expect("write");
        std::fs::create_dir(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("sub").join("nested.py"), "").expect("write");

        let scopes = vec![
            content_scope("rust", "*.rs", None),
            content_scope("markdown", "*.md", None),
            content_scope("no-match", "*.go", None),
        ];

        let matched = matches_content_all(&scopes, root);
        assert!(matched.contains("rust"));
        assert!(matched.contains("markdown"));
        assert!(!matched.contains("no-match"));
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn matches_content_all_respects_per_scope_depth() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let root = temp_dir.path();
        std::fs::create_dir(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("sub").join("nested.py"), "").expect("write");

        // depth 0 = root only, so the nested file (depth 2) must not match.
        let shallow = content_scope("shallow", "*.py", Some(0));
        // No depth limit, so the same nested file must match.
        let deep = content_scope("deep", "*.py", None);

        let scopes = [shallow, deep];
        let matched = matches_content_all(&scopes, root);
        assert!(!matched.contains("shallow"));
        assert!(matched.contains("deep"));
    }

    #[test]
    fn matches_content_all_skips_invalid_glob_but_evaluates_rest() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let root = temp_dir.path();
        std::fs::write(root.join("main.rs"), "").expect("write");

        let scopes = vec![
            content_scope("bad", "[", None),
            content_scope("good", "*.rs", None),
        ];

        let matched = matches_content_all(&scopes, root);
        assert!(!matched.contains("bad"));
        assert!(matched.contains("good"));
    }

    #[test]
    fn matches_content_all_empty_scopes_returns_empty() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let matched = matches_content_all(&[], temp_dir.path());
        assert!(matched.is_empty());
    }

    proptest! {
        // parse_extra_tags never panics on arbitrary input.
        #[test]
        fn parse_extra_tags_never_panics(raw in r"\PC*") {
            let _ = parse_extra_tags(&raw);
        }

        // Every non-empty, whitespace-trimmed segment from a comma-joined
        // input round-trips through parse_extra_tags.
        #[test]
        fn parse_extra_tags_roundtrips_nonempty_segments(
            tags in prop::collection::vec("[a-z][a-z0-9-]{0,10}", 1..5)
        ) {
            let raw = tags.join(",");
            prop_assert_eq!(parse_extra_tags(&raw), tags);
        }

        // Any string built entirely from the accepted charset (alphanumeric,
        // `-`, `_`) is accepted, regardless of length or which characters
        // from that set it uses (property-test-gap-finder, #1465).
        #[test]
        fn is_valid_tag_charset_accepts_any_string_from_the_allowed_charset(
            tag in "[a-zA-Z0-9_-]{1,64}"
        ) {
            prop_assert!(is_valid_tag_charset(&tag));
        }

        // A string containing at least one character outside the accepted
        // charset is always rejected, wherever that character falls.
        #[test]
        fn is_valid_tag_charset_rejects_any_string_containing_a_disallowed_char(
            prefix in "[a-zA-Z0-9_-]{0,10}",
            bad in prop::char::any().prop_filter(
                "must be outside the accepted charset",
                |c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'),
            ),
            suffix in "[a-zA-Z0-9_-]{0,10}",
        ) {
            let tag = format!("{prefix}{bad}{suffix}");
            prop_assert!(!is_valid_tag_charset(&tag));
        }

        // discover_project never panics on arbitrary cwd paths.
        #[test]
        fn discover_arbitrary_path_never_panics(cwd in r"/[a-z/]*") {
            let env = Env {
                cwd,
                ..Env::empty()
            };
            let _ = discover_project(&env);
        }

        // #1041: for any input, cap_aggregate_tags never exceeds the cap,
        // never introduces a tag that wasn't in the input, and leaves an
        // already-within-cap input untouched.
        #[test]
        fn cap_aggregate_tags_never_exceeds_cap_and_is_a_subset(
            tags in prop::collection::btree_set("[a-z][a-z0-9-]{0,10}", 0..(MAX_TAGS_PER_SOURCE * 2))
        ) {
            let original = tags.clone();
            let capped = cap_aggregate_tags(tags);
            prop_assert!(capped.len() <= MAX_TAGS_PER_SOURCE);
            prop_assert!(capped.is_subset(&original));
            if original.len() <= MAX_TAGS_PER_SOURCE {
                prop_assert_eq!(capped, original);
            }
        }

        // Malformed YAML never panics; always degrades to defaults.
        #[test]
        fn malformed_yaml_never_panics(body in r"\PC*") {
            let temp_dir = tempfile::TempDir::new().expect("tempdir");
            write_project_file(temp_dir.path(), &body);
            let env = env_in(temp_dir.path(), temp_dir.path());
            let _ = discover_project(&env);
        }

        // Property test #165: Unicode-safe basename derivation.
        // Derived project id/name must be valid UTF-8 and handle special chars.
        #[test]
        fn unicode_safe_basename_derivation(
            name_part in r"[^\x00/\.]|[^\x00/][^\x00/]*[^\x00/.]"
        ) {
            let temp_dir = tempfile::TempDir::new().expect("tempdir");
            let root = temp_dir.path();
            let sub = root.join(&name_part);
            // Reject test cases where directory creation fails.
            prop_assume!(std::fs::create_dir_all(&sub).is_ok());

            write_project_file(&sub, "");
            let env = env_in(&sub, root);
            let project = discover_project(&env).expect("discover");

            // id and name must be valid UTF-8 (already guaranteed by String).
            // Both must be non-empty (basename fallback is "llmenv").
            prop_assert!(!project.id.is_empty());
            prop_assert!(!project.name.is_empty());
            // name_part is guaranteed non-empty, no leading/trailing dots
            prop_assert_eq!(project.id, name_part.clone());
            prop_assert_eq!(project.name, name_part);
        }

        // Property test #166: discover_project walk termination with deep nesting.
        // Walk must not descend infinitely; should terminate at home boundary or root.
        #[test]
        fn walk_terminates_at_home_boundary(
            depth in 1..32usize,
        ) {
            let temp_dir = tempfile::TempDir::new().expect("tempdir");
            let root = temp_dir.path();
            let mut deep_path = root.to_path_buf();
            for i in 0..depth {
                deep_path.push(format!("d{i}"));
            }
            prop_assume!(std::fs::create_dir_all(&deep_path).is_ok());

            // Place marker at root; walk from deep_path should find it.
            write_project_file(root, "id: root-marker\n");

            let env = env_in(&deep_path, root);
            let project = discover_project(&env).expect("discover at depth");
            prop_assert_eq!(project.id, "root-marker");
            prop_assert_eq!(project.root, root);

            // Now test walk stops at home: place hostile marker above home.
            let temp_dir2 = tempfile::TempDir::new().expect("tempdir2");
            let above_home = temp_dir2.path();
            let home = above_home.join("home");
            let mut deep_work = home.to_path_buf();
            for i in 0..depth {
                deep_work.push(format!("w{i}"));
            }
            prop_assume!(std::fs::create_dir_all(&deep_work).is_ok());
            write_project_file(above_home, "id: hostile\n");

            let env2 = env_in(&deep_work, &home);
            let result = discover_project(&env2);
            // Hostile marker above home must not be found, even at depth.
            prop_assert!(result.is_none(), "hostile marker above home must not activate");
        }

        // Property test #167: ProjectFile unknown-fields filtering correctness.
        // Unknown fields must be captured; known fields must not appear in unknown_fields.
        #[test]
        fn project_file_unknown_fields_filtering(
            unknown_count in 0..10usize,
            known_id in "[a-z0-9]+",
        ) {
            let temp_dir = tempfile::TempDir::new().expect("tempdir");

            // Build YAML with known fields + unknown fields.
            let mut yaml = format!("id: {}\n", known_id);
            yaml.push_str("name: TestName\n");
            yaml.push_str("tags: [a, b, c]\n");

            // Append arbitrary unknown fields.
            for i in 0..unknown_count {
                yaml.push_str(&format!("field_{}: value_{}\n", i, i));
            }

            write_project_file(temp_dir.path(), &yaml);
            let env = env_in(temp_dir.path(), temp_dir.path());
            let project = discover_project(&env).expect("discover");

            // Verify known fields were parsed.
            prop_assert_eq!(project.id, known_id);
            prop_assert_eq!(project.name, "TestName");
            prop_assert_eq!(project.tags, vec!["a", "b", "c"]);

            // Verify unknown fields were captured.
            prop_assert_eq!(
                project.unknown_fields.len(),
                unknown_count,
                "all unknown fields must be captured"
            );

            // Verify no known field names appear in unknown_fields.
            for uf in &project.unknown_fields {
                prop_assert!(!matches!(
                    uf.as_str(),
                    "id" | "name" | "description" | "tags" | "enable_bundles" | "disable_bundles"
                ));
            }
        }
    }

    #[test]
    fn read_project_file_io_error_returns_defaults() {
        // When the marker file exists but can't be read (e.g. it's a directory),
        // read_project_file must return defaults — not panic, not hang.
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let marker = temp_dir.path().join(".llmenv.yaml");
        std::fs::create_dir(&marker).expect("create .llmenv.yaml as directory");

        let env = env_in(temp_dir.path(), temp_dir.path());
        let project = discover_project(&env).expect("discover must return Some even on I/O error");
        // Should get a basename-derived id instead of crashing or None.
        assert!(!project.id.is_empty());
        assert!(!project.name.is_empty());
    }
}
