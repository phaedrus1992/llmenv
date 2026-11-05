//! Version triples for the tools that llmenv checks, and the codebase-memory-mcp floor.

/// Oldest codebase-memory-mcp without the stability fixes that llmenv's SessionStart path depends
/// on: large indexes no longer corrupt the database, the startup settings race is gone, and Git
/// commands ignore hook-inherited `GIT_DIR` (upstream #2330, #2650, #2341).
const CBM_MIN_VERSION: (u64, u64, u64) = (0, 12, 0);

pub(super) use crate::util::semver::parse_semver_triple;

/// The warning text for a codebase-memory-mcp older than [`CBM_MIN_VERSION`],
/// or for a version that cannot be parsed. `None` when the version is at the floor
/// or above it.
///
/// An unparseable version warns too. Silence there would pass the floor check
/// for a binary that may be too old.
pub(super) fn cbm_floor_warning(version: &str) -> Option<String> {
    let Some(parsed) = parse_semver_triple(version) else {
        return Some(format!(
            "codebase-memory-mcp version {version} could not be checked against the 0.12.0 floor. \
             Run `codebase-memory-mcp --version` to confirm the version is 0.12.0 or newer."
        ));
    };
    (parsed < CBM_MIN_VERSION).then(|| {
        format!(
            "codebase-memory-mcp {version} is older than 0.12.0. Older releases can corrupt a \
             large index and fail at startup on a settings race. The first index after \
             upgrading rebuilds each project once."
        )
    })
}

/// The first opencode major that llmenv supports. llmenv writes only the 2.x plugin (#2840).
const OPENCODE_MIN_MAJOR: u64 = 2;

/// The warning text for an opencode older than 2.0, or for a version that cannot be parsed.
/// `None` for 2.0 or newer.
pub(super) fn opencode_floor_warning(version: &str) -> Option<String> {
    let Some((major, _, _)) = parse_semver_triple(version) else {
        return Some(format!(
            "opencode version {version} could not be checked. llmenv supports opencode 2.x only. \
             Run `opencode --version` to confirm the version is 2.0 or newer."
        ));
    };
    (major < OPENCODE_MIN_MAJOR).then(|| {
        format!(
            "opencode {version} is not supported: llmenv supports opencode 2.x only, and the \
             llmenv plugin does not load on 1.x. Fix: run `opencode upgrade`."
        )
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, clippy::panic, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn cbm_floor_warning_fires_only_below_the_floor() {
        let warning = cbm_floor_warning("0.10.8").expect("0.10.8 is below the floor");
        assert!(warning.contains("0.10.8 is older than 0.12.0"), "{warning}");
        assert!(
            cbm_floor_warning("0.11.0").is_some(),
            "0.11.0 is below the 0.12.0 floor"
        );
        for ok in ["0.12.0", "0.12.1", "1.0.0"] {
            assert_eq!(cbm_floor_warning(ok), None, "{ok:?}");
        }
    }

    #[test]
    fn opencode_below_2_is_reported_as_unsupported() {
        let warning = opencode_floor_warning("v1.14.2").expect("1.x is unsupported");
        assert!(warning.contains("not supported"), "{warning}");
        assert!(warning.contains("opencode upgrade"), "{warning}");
        for ok in ["v2.0.0", "v2.0.30", "3.1.0"] {
            assert_eq!(opencode_floor_warning(ok), None, "{ok:?}");
        }
        let unknown = opencode_floor_warning("version unknown").expect("an unknown version warns");
        assert!(unknown.contains("could not be checked"), "{unknown}");
    }

    #[test]
    fn cbm_floor_warning_fires_for_an_unparseable_version() {
        for unknown in ["version unknown", "garbage", ""] {
            let warning = cbm_floor_warning(unknown)
                .unwrap_or_else(|| panic!("{unknown:?} must warn, not pass silently"));
            assert!(warning.contains("could not be checked"), "{warning}");
            assert!(warning.contains(unknown), "{warning}");
        }
    }
}
