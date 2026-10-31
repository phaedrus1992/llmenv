use super::network::NetworkNeeds;
use llmenv_config::{HostScope, UserScope};

pub(crate) use super::content_walk::matches_content_all;
pub use super::marker_file::discover_project;
// Kept public here: the root crate's `scope::matcher` shim and the hook code import it from this path.
pub use super::tags::is_valid_tag_charset;
use super::tags::sanitize_tags;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `$LLMENV_EXTRA_TAGS` is set but is not valid UTF-8.
#[derive(Debug, thiserror::Error)]
#[error(
    "$LLMENV_EXTRA_TAGS is not valid UTF-8, so its tags were not applied. Set it to plain \
     comma-separated text, or unset it."
)]
pub struct ExtraTagsNotUtf8;

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

#[derive(Debug, Clone, Default)]
pub struct Env {
    pub hostname: String,
    pub user: String,
    pub cwd: String,
    pub gateway_mac: Option<String>,
    /// Addresses of the local network interfaces (not loopback or link-local), for `match.cidr`
    /// (#1051).
    pub local_addrs: Vec<std::net::IpAddr>,
    /// SSID of the associated Wi-Fi network, for `match.ssid`. `None` when there is none or
    /// the platform cannot say (`llmenv doctor` reports which).
    pub ssid: Option<String>,
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
    needs: NetworkNeeds,
    env: Env,
}

static ENV_CACHE: Mutex<Option<CachedEnv>> = Mutex::new(None);

impl Env {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Detect environment, returning a cached result if fresher than 30 s.
    /// Detects every network fact (gateway MAC, interface addresses, SSID); prefer
    /// [`Env::detect_for_config`] on the hook path, which skips the probes that no
    /// network scope needs.
    ///
    /// # Errors
    /// Returns an error when `$LLMENV_EXTRA_TAGS` is not valid UTF-8.
    pub fn detect() -> Result<Self, ExtraTagsNotUtf8> {
        Self::detect_cached(NetworkNeeds::ALL)
    }

    fn detect_cached(needs: NetworkNeeds) -> Result<Self, ExtraTagsNotUtf8> {
        if let Ok(lock) = ENV_CACHE.lock()
            && let Some(cached) = lock.as_ref()
            && cached.detected.elapsed() < Duration::from_secs(30)
            && cached.needs.covers(needs)
        {
            return Ok(cached.env.clone());
        }
        let env = Self::detect_fresh(needs)?;
        if let Ok(mut lock) = ENV_CACHE.lock() {
            *lock = Some(CachedEnv {
                detected: Instant::now(),
                needs,
                env: env.clone(),
            });
        }
        Ok(env)
    }

    /// Detect environment for a specific config. The gateway MAC, interface address, and SSID
    /// probes each shell out or call the OS, and nothing can match on them unless a network
    /// scope uses that field. This runs only the probes that the declared network scopes need,
    /// and none on the common config with no network scope. Each hook-run is a fresh process
    /// (the 30s cache never warms on that path), so this removes the dominant remaining
    /// hook-run subprocess cost for such a config.
    ///
    /// # Errors
    /// Returns an error when `$LLMENV_EXTRA_TAGS` is not valid UTF-8.
    pub fn detect_for_config(config: &llmenv_config::Config) -> Result<Self, ExtraTagsNotUtf8> {
        let needs = NetworkNeeds::from_scopes(&config.scope.network);
        if needs == NetworkNeeds::default() {
            Self::detect_fresh(needs)
        } else {
            Self::detect_cached(needs)
        }
    }

    /// Fresh env detection. `needs` gates the network probes; the hostname (uname syscall),
    /// user, and cwd probes always run.
    fn detect_fresh(needs: NetworkNeeds) -> Result<Self, ExtraTagsNotUtf8> {
        // Error level: a scope that is silently disabled changes which bundles load (#2666).
        let hostname = detect_hostname().unwrap_or_else(|| {
            tracing::error!("hostname detection failed; host-scope matching disabled");
            String::new()
        });
        let user = match std::env::var("USER") {
            Ok(user) => user,
            Err(std::env::VarError::NotPresent) => {
                tracing::error!("$USER unset; user-scope matching disabled");
                String::new()
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                tracing::error!("$USER is not valid UTF-8; user-scope matching disabled");
                String::new()
            }
        };
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| {
                tracing::error!("current_dir() unavailable; project-scope matching disabled");
                String::new()
            });
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(std::path::PathBuf::from);
        Ok(Self {
            // Hostname comparison is case-insensitive — `hostname(1)` and
            // /etc/hostname may differ in case across hosts.
            hostname: hostname.to_ascii_lowercase(),
            user,
            cwd,
            gateway_mac: needs
                .gateway_mac
                .then(super::network::detect_gateway_mac)
                .flatten(),
            local_addrs: if needs.local_addrs {
                super::network::detect_local_addrs()
            } else {
                Vec::new()
            },
            ssid: needs.ssid.then(super::ssid::detect_ssid_name).flatten(),
            home,
            os: std::env::consts::OS.to_string(),
            extra_tags: extra_tags_from_var(std::env::var("LLMENV_EXTRA_TAGS"))?,
        })
    }
}

/// Read `$LLMENV_EXTRA_TAGS` for a caller that cannot fail. A value that is not UTF-8 gives no
/// tags and a warning. The statusline re-reads this live on every render and cannot stop there,
/// and a full `Env::detect` would also fork `route`/`arp` whenever a network scope is
/// configured (#1538).
#[must_use]
pub fn extra_tags_from_env() -> Vec<String> {
    extra_tags_from_var(std::env::var("LLMENV_EXTRA_TAGS")).unwrap_or_else(|e| {
        tracing::warn!("{e}");
        Vec::new()
    })
}

/// Map the read of `$LLMENV_EXTRA_TAGS` to its tags. A value that is not UTF-8 is an error.
/// An empty list would make the user's tags vanish with no visible sign.
fn extra_tags_from_var(
    value: Result<String, std::env::VarError>,
) -> Result<Vec<String>, ExtraTagsNotUtf8> {
    match value {
        Ok(raw) => Ok(parse_extra_tags(&raw)),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(std::env::VarError::NotUnicode(_)) => Err(ExtraTagsNotUtf8),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{Env, discover_project, glob_matches, matches_content_all, parse_extra_tags};
    use crate::tags::{
        MAX_TAG_LEN, MAX_TAGS_PER_SOURCE, cap_aggregate_tags, is_valid_tag_charset, sanitize_tags,
    };
    use llmenv_config::ContentScope;
    use proptest::prelude::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    #[test]
    fn extra_tags_from_env_not_set_is_no_tags() {
        assert!(
            super::extra_tags_from_var(Err(std::env::VarError::NotPresent))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn extra_tags_from_env_reads_utf8_value() {
        assert_eq!(
            super::extra_tags_from_var(Ok("dev, work".to_string())).unwrap(),
            vec!["dev", "work"]
        );
    }

    #[test]
    fn extra_tags_from_env_non_utf8_is_an_error_naming_the_variable() {
        let value = std::env::VarError::NotUnicode(std::ffi::OsString::from("x"));
        let err = super::extra_tags_from_var(Err(value)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("LLMENV_EXTRA_TAGS"), "{msg}");
        assert!(msg.contains("unset it"), "{msg}");
    }

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
        let at_limit = "a".repeat(MAX_TAG_LEN);
        let over_limit = "a".repeat(MAX_TAG_LEN + 1);
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
        let huge = "a".repeat(super::super::marker_file::MAX_DESCRIPTION_BYTES + 500);
        write_project_file(temp_dir.path(), &format!("description: \"{huge}\"\n"));

        let env = env_in(temp_dir.path(), temp_dir.path());
        let project = discover_project(&env).expect("discover");
        let desc = project.description.expect("description");
        assert!(
            desc.len() <= super::super::marker_file::MAX_DESCRIPTION_BYTES,
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
        // Every tag parse_extra_tags returns obeys the tag rules the rest of scope matching relies on.
        #[test]
        fn parse_extra_tags_output_obeys_tag_rules(raw in r"\PC*") {
            let tags = parse_extra_tags(&raw);
            prop_assert!(tags.len() <= MAX_TAGS_PER_SOURCE);
            for tag in &tags {
                prop_assert!(!tag.is_empty());
                prop_assert!(tag.len() <= MAX_TAG_LEN);
                prop_assert!(is_valid_tag_charset(tag));
            }
        }

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
