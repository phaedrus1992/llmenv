//! The `gh` program and the deadlines of the PR lookup.

use std::time::Duration;

/// How long to wait for `gh pr view` when the render itself runs it, which is the case with no
/// state folder for a cache. `gh` hits the GitHub API over the network. A stalled connection must
/// not hang the statusline render, so the limit is short.
const DEFAULT_GH_PR_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the detached refresh waits for `gh pr view`. Nobody waits on the refresh, so the limit
/// can cover a slow shim and a loaded machine (#3889).
const GH_PR_REFRESH_TIMEOUT: Duration = Duration::from_secs(8);

/// Which `gh` binary [`gh_pr_view`] invokes, how long it may take, and where a cache miss runs it.
///
/// The timeout travels with the binary rather than being read from a constant
/// so tests can point at a fake `gh` *and* give it a budget that machine load
/// can't blow: a test asserting "this JSON parses into this `PrInfo`" must not
/// fail because a `/bin/sh` child missed a 3-second production deadline
/// (#1073). Bundling them also keeps [`derive_pr`] within the
/// positional-parameter limit.
#[derive(Clone, Copy, Debug)]
pub(super) struct GhPrCmd<'a> {
    pub(super) bin: &'a str,
    pub(super) timeout: Duration,
    /// Whether a cache miss starts a detached refresh and lets the render go on. When `false`,
    /// the render runs `gh` itself, and waits up to `timeout`.
    pub(super) background: bool,
}

impl<'a> GhPrCmd<'a> {
    /// `bin` with the production timeout ([`DEFAULT_GH_PR_TIMEOUT`]) and the detached refresh.
    pub(super) fn new(bin: &'a str) -> Self {
        Self {
            bin,
            timeout: DEFAULT_GH_PR_TIMEOUT,
            background: true,
        }
    }

    /// `bin` as the detached refresh runs it: the long limit, and no further detaching.
    pub(super) fn refresh(bin: &'a str) -> Self {
        Self {
            bin,
            timeout: GH_PR_REFRESH_TIMEOUT,
            background: false,
        }
    }
}
