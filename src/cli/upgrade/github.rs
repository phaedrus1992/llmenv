//! GitHub release lookups for `llmenv upgrade`. Every request goes through `get_text`,
//! so a failure names the URL that failed.

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

use super::GhRelease;

/// The public GitHub API base. Any other base means `LLMENV_UPGRADE_GITHUB_API` is set.
const DEFAULT_API_BASE: &str = "https://api.github.com";

/// Characters of an unexpected response body to show. Enough to recognize a captive
/// portal or a proxy page, short enough to keep the error on one screen.
const BODY_EXCERPT_CHARS: usize = 200;

/// Fetch the latest non-prerelease GitHub release.
pub(super) fn fetch_latest(
    client: &reqwest::blocking::Client,
    base_url: &str,
) -> Result<GhRelease> {
    let url = format!("{base_url}/repos/phaedrus1992/llmenv/releases/latest");
    let text = get_text(client, base_url, &url)?;
    parse_json(&url, &text)
}

/// Fetch releases and return the first non-draft (beta track).
pub(super) fn fetch_beta(client: &reqwest::blocking::Client, base_url: &str) -> Result<GhRelease> {
    let url = format!("{base_url}/repos/phaedrus1992/llmenv/releases?per_page=10");
    let text = get_text(client, base_url, &url)?;
    let releases: Vec<GhRelease> = parse_json(&url, &text)?;
    releases
        .into_iter()
        .find(|r| !r.draft)
        .context("no published releases found")
}

/// GET a GitHub API URL and return its body. A non-success status names the URL. It also
/// names the override when `base_url` is not the public API, so a wrong base path, a proxy,
/// and a real missing release do not look the same.
fn get_text(client: &reqwest::blocking::Client, base_url: &str, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("failed to query GitHub releases API at {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let hint = if base_url == DEFAULT_API_BASE {
            ""
        } else {
            " (check LLMENV_UPGRADE_GITHUB_API, which is set)"
        };
        bail!("GitHub API returned {status} for {url}{hint}");
    }
    resp.text()
        .with_context(|| format!("failed to read the GitHub API response from {url}"))
}

/// Parse a GitHub response body. On failure the error shows the start of the body.
fn parse_json<T: DeserializeOwned>(url: &str, text: &str) -> Result<T> {
    serde_json::from_str(text).with_context(|| {
        let excerpt: String = text.chars().take(BODY_EXCERPT_CHARS).collect();
        format!("failed to parse the GitHub response from {url}; body starts with {excerpt:?}")
    })
}
