use super::gh_tests::{fake_gh, write_fake_gh};

#[test]
fn engine_data_reads_the_session_id() {
    let with: EngineData = serde_json::from_str(r#"{"session_id":"abc"}"#).unwrap();
    assert_eq!(with.session_id.as_deref(), Some("abc"));
    let without: EngineData = serde_json::from_str("{}").unwrap();
    assert_eq!(without.session_id, None);
}

use super::usage::{UsageArgs, humanize_until, render_usage};
use super::*;
use proptest::prelude::*;

#[test]
fn usage_state_dir_with_env_unset_disables_tracking() {
    let get_env = |_: &str| None;
    assert_eq!(usage_state_dir_with_env(&get_env), None);
}

#[test]
fn usage_state_dir_with_env_empty_disables_tracking_like_unset() {
    let get_env = |name: &str| match name {
        "CLAUDE_CONFIG_DIR" => Some(String::new()),
        _ => None,
    };
    assert_eq!(usage_state_dir_with_env(&get_env), None);
}

#[test]
fn usage_state_dir_with_env_non_empty_used_verbatim() {
    let get_env = |name: &str| match name {
        "CLAUDE_CONFIG_DIR" => Some("/custom/config".to_string()),
        _ => None,
    };
    assert_eq!(
        usage_state_dir_with_env(&get_env),
        Some(PathBuf::from("/custom/config/statusline-state"))
    );
}

/// Test-only convenience wrapper: the `pr` widget's full render
/// (resolve + text + dynamic style), matching what `render_engine_widget`
/// dispatches to for `"pr"` since the resolve-once refactor folded the
/// old standalone `render_pr` into that dispatch.
fn render_pr(
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    render_engine_widget("pr", data, cfg, use_color).unwrap()
}

fn engine_data() -> EngineData {
    serde_json::from_value(serde_json::json!({
        "workspace": { "current_dir": "/home/user/llmenv" },
        "model": { "display_name": "Claude Opus 4.8" },
        "cost": { "total_duration_ms": 13_320_000 },
        "context_window": {
            "remaining_percentage": 65.0,
            "context_window_size": 200_000,
            "current_usage": {
                "input_tokens": 5000,
                "cache_creation_input_tokens": 1000,
                "cache_read_input_tokens": 4000
            }
        },
        "rate_limits": {
            "five_hour": { "used_percentage": 24.5, "resets_at": 1_713_264_000 },
            "seven_day": { "used_percentage": 41.0, "resets_at": 1_713_700_000 }
        },
        "branch": { "name": "main" },
        "pr": { "number": 42 }
    }))
    .unwrap()
}

#[test]
fn renders_model_default_format() {
    // engine_data()'s fixture display_name is "Claude Opus 4.8" with no
    // separate version field — short_name strips the "Claude" prefix and
    // the "4.8" token, and the version falls back to that same "4.8" token
    // parsed out of display_name, so the default format shows both.
    let out = render_engine_widget("model", &engine_data(), None, false).unwrap();
    assert_eq!(out, "Opus 4.8");
}

#[test]
fn renders_model_strips_trailing_parenthetical() {
    // Regression: a "(1M context)" suffix used to leak a "context)" token
    // into the short name ("Opus context)"). The parenthetical is stripped
    // and the version parsed from display_name (no separate version field).
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "model": { "display_name": "Opus 4.8 (1M context)" }
    }))
    .unwrap();
    let out = render_engine_widget("model", &data, None, false).unwrap();
    assert_eq!(out, "Opus 4.8");
}

#[test]
fn short_model_name_and_version_split_a_parenthetical_display_name() {
    assert_eq!(short_model_name("Opus 4.8 (1M context)"), "Opus");
    assert_eq!(
        version_from_display_name("Opus 4.8 (1M context)").as_deref(),
        Some("4.8")
    );
}

#[test]
fn renders_model_default_format_includes_version_field() {
    // Isolates {version} specifically: renders_model_default_format's
    // fixture has no separate `version` field, so a mutant swapping which
    // field feeds {short_name} vs {version} would go uncaught there.
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "model": { "display_name": "Claude Opus 4.8", "version": "4.8" }
    }))
    .unwrap();
    let out = render_engine_widget("model", &data, None, false).unwrap();
    assert_eq!(out, "Opus 4.8");
}

#[test]
fn short_model_name_strips_claude_prefix_and_version() {
    assert_eq!(short_model_name("Claude Opus 4.8"), "Opus");
    assert_eq!(short_model_name("Claude Sonnet 5"), "Sonnet");
    assert_eq!(short_model_name("GPT-Z"), "GPT-Z");
    assert_eq!(short_model_name(""), "");
}

#[test]
fn renders_folder_from_workspace_basename() {
    let out = render_engine_widget("folder", &engine_data(), None, false).unwrap();
    assert_eq!(out, "\u{1f4c1} llmenv");
}

#[test]
fn renders_duration_hms() {
    let out = render_engine_widget("duration", &engine_data(), None, false).unwrap();
    assert_eq!(out, "\u{23f1} 3h 42m"); // ⏱ 3h42m (13_320_000 ms)
}

#[test]
fn renders_duration_minutes_and_seconds_under_an_hour() {
    let data = EngineData {
        cost: Some(Cost {
            total_duration_ms: Some(150_000), // 2m30s
        }),
        ..Default::default()
    };
    assert_eq!(
        render_engine_widget("duration", &data, None, false).unwrap(),
        "\u{23f1} 2m 30s"
    );
}

#[test]
fn renders_duration_seconds_only_under_a_minute() {
    let data = EngineData {
        cost: Some(Cost {
            total_duration_ms: Some(45_000), // 45s
        }),
        ..Default::default()
    };
    assert_eq!(
        render_engine_widget("duration", &data, None, false).unwrap(),
        "\u{23f1} 45s"
    );
}

#[test]
fn model_display_mode_short_and_full() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "model": { "display_name": "Claude Opus 4.8 (1M context)" }
    }))
    .unwrap();
    let short = llmenv_config::WidgetConfig {
        display: Some("short".to_string()),
        ..Default::default()
    };
    let full = llmenv_config::WidgetConfig {
        display: Some("full".to_string()),
        ..Default::default()
    };
    assert_eq!(render_model(&data, Some(&short)), "Opus");
    assert_eq!(
        render_model(&data, Some(&full)),
        "Claude Opus 4.8 (1M context)"
    );
    // Unset display → default family + version.
    assert_eq!(render_model(&data, None), "Opus 4.8");
}

#[test]
fn renders_tokens_default_format() {
    let out = render_engine_widget("tokens", &engine_data(), None, false).unwrap();
    // total_tokens = input_tokens(5000) + cache_creation_input_tokens(1000)
    // + cache_read_input_tokens(4000) = 10000; format_token_count(10000):
    // 10000 / 1000 = 10.0, trailing ".0" dropped -> "10k".
    assert_eq!(out, "10k");
}

#[test]
fn renders_budget_default_format() {
    let out = render_engine_widget("budget", &engine_data(), None, false).unwrap();
    // used = total_tokens(same fixture) = 10000 -> "10k"; max =
    // context_window_size(200_000) -> format_token_count(200_000) = "200k"
    // (round values drop the trailing ".0"); default format is "{used}/{max}".
    assert_eq!(out, "10k/200k");
}

#[test]
fn renders_budget_uses_m_suffix_for_million_context_window() {
    // A 1M context window renders "1m", not "1000.0k".
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": {
            "context_window_size": 1_000_000,
            "current_usage": { "input_tokens": 9200, "cache_read_input_tokens": 100_000 }
        }
    }))
    .unwrap();
    let out = render_engine_widget("budget", &data, None, false).unwrap();
    assert_eq!(out, "109.2k/1m");
}

#[test]
fn renders_cache_usage_default_format() {
    let out = render_engine_widget("cache_usage", &engine_data(), None, false).unwrap();
    // cache = cache_read(4000) + cache_creation(1000) = 5000;
    // total = input(5000) + cache(5000) = 10000; pct = round(5000/10000*100) = 50.
    assert_eq!(out, "\u{21bb}50%");
}

#[test]
fn unknown_widget_name_renders_none() {
    assert!(render_engine_widget("not_a_widget", &engine_data(), None, false).is_none());
}

#[test]
fn missing_field_renders_empty_not_panic() {
    let empty = EngineData::default();
    let out = render_engine_widget("model", &empty, None, false).unwrap();
    assert_eq!(out, "");
}

#[test]
fn custom_format_overrides_default() {
    let cfg = llmenv_config::WidgetConfig {
        format: Some("{full_name}".to_string()),
        ..Default::default()
    };
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "model": { "display_name": "Claude Opus 4.8", "full_name": "claude-opus-4-8-20260101" }
    }))
    .unwrap();
    let out = render_engine_widget("model", &data, Some(&cfg), false).unwrap();
    assert_eq!(out, "claude-opus-4-8-20260101");
}

#[test]
fn context_clamps_absurdly_large_remaining_percentage() {
    // A corrupt/hostile engine sending remaining_percentage: 1e300 must
    // not produce a saturated i64-cast garbage string.
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": { "remaining_percentage": 1e300 }
    }))
    .unwrap();
    let out = render_engine_widget("context", &data, None, false).unwrap();
    assert_eq!(
        out,
        "0% \u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}"
    );
}

#[test]
fn context_clamps_absurdly_negative_remaining_percentage() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": { "remaining_percentage": -1e300 }
    }))
    .unwrap();
    let out = render_engine_widget("context", &data, None, false).unwrap();
    assert_eq!(
        out,
        "100% \u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}"
    );
}

#[test]
fn renders_branch_name() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "branch": { "name": "release/3.x" }
    }))
    .unwrap();
    assert_eq!(
        render_engine_widget("branch", &data, None, false).unwrap(),
        "\u{1f33f} release/3.x"
    );
}

#[test]
fn branch_falls_back_to_worktree_branch_when_no_branch_field() {
    // Claude Code `--worktree` sessions carry `worktree.branch` but no
    // top-level `branch`.
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "worktree": { "branch": "feat/thing" }
    }))
    .unwrap();
    assert_eq!(
        render_engine_widget("branch", &data, None, false).unwrap(),
        "\u{1f33f} feat/thing"
    );
}

#[test]
fn branch_derived_from_git_head_when_engine_sends_none() {
    // A regular Claude Code session sends no branch at all — only
    // workspace.current_dir. llmenv reads `.git/HEAD` to fill the widget.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(
        dir.path().join(".git").join("HEAD"),
        "ref: refs/heads/release/3.x\n",
    )
    .unwrap();
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "workspace": { "current_dir": dir.path().to_string_lossy() }
    }))
    .unwrap();
    assert_eq!(
        render_engine_widget("branch", &data, None, false).unwrap(),
        "\u{1f33f} release/3.x"
    );
}

#[test]
fn branch_empty_for_detached_head() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    // Detached HEAD: a raw sha, no `ref:` line.
    std::fs::write(
        dir.path().join(".git").join("HEAD"),
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
    )
    .unwrap();
    assert_eq!(git_branch(dir.path()), Ok(None));
}

#[test]
fn git_branch_reports_an_unreadable_head_instead_of_hiding_it() {
    // A directory where HEAD should be cannot be read as a file (#2710).
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git").join("HEAD")).unwrap();
    assert!(git_branch(dir.path()).is_err());
}

#[test]
fn find_git_dir_reports_a_gitfile_without_a_pointer() {
    // A `.git` file with no `gitdir:` line is a read error, not "no repository", so the
    // branch label can say it is unreadable (#2749).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".git"), "garbage\n").unwrap();
    assert!(find_git_dir(dir.path()).is_err());
}

#[test]
fn find_git_dir_follows_gitfile_pointer() {
    // A linked worktree has a `.git` *file* pointing at the real gitdir.
    let dir = tempfile::tempdir().unwrap();
    let real_gitdir = dir.path().join("real-gitdir");
    std::fs::create_dir(&real_gitdir).unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(
        work.join(".git"),
        format!("gitdir: {}\n", real_gitdir.display()),
    )
    .unwrap();
    assert_eq!(find_git_dir(&work), Ok(Some(real_gitdir)));
}

#[test]
fn find_git_dir_resolves_a_relative_pointer_against_the_git_file() {
    // A submodule's `.git` file holds a relative pointer such as `../.git/modules/x`.
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join(".git"), "gitdir: ../real-gitdir\n").unwrap();
    assert_eq!(find_git_dir(&work), Ok(Some(work.join("../real-gitdir"))));
}

#[test]
fn git_branch_reads_a_missing_head_as_no_branch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".git"), "gitdir: pruned-gitdir\n").unwrap();
    assert_eq!(git_branch(dir.path()), Ok(None));
}

/// A checkout whose `.git/HEAD` holds `content`.
fn checkout_with_head(content: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git").join("HEAD"), content).unwrap();
    dir
}

proptest::proptest! {
    #[test]
    fn git_branch_reads_any_branch_name_from_head(name in "[a-z0-9][a-z0-9/_.-]{0,30}") {
        let dir = checkout_with_head(&format!("ref: refs/heads/{name}\n"));
        proptest::prop_assert_eq!(git_branch(dir.path()), Ok(Some(name)));
    }

    #[test]
    fn git_branch_reads_a_detached_head_as_no_branch(sha in "[0-9a-f]{40}") {
        let dir = checkout_with_head(&format!("{sha}\n"));
        proptest::prop_assert_eq!(git_branch(dir.path()), Ok(None));
    }
}

#[test]
fn renders_pr_number() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "pr": { "number": 834 }
    }))
    .unwrap();
    assert_eq!(
        render_engine_widget("pr", &data, None, false).unwrap(),
        "#834"
    );
}

#[test]
fn pr_url_display_mode_and_number_fallback() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "pr": { "number": 834, "url": "https://github.com/o/r/pull/834" }
    }))
    .unwrap();
    let url_mode = llmenv_config::WidgetConfig {
        display: Some("url".to_string()),
        ..Default::default()
    };
    assert_eq!(
        render_pr(&data, Some(&url_mode), false),
        "https://github.com/o/r/pull/834"
    );
    // url mode but no url → falls back to #number.
    let no_url: EngineData = serde_json::from_value(serde_json::json!({
        "pr": { "number": 834 }
    }))
    .unwrap();
    assert_eq!(render_pr(&no_url, Some(&url_mode), false), "#834");
}

#[test]
fn pr_review_state_colors_the_widget() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "pr": { "number": 1, "review_state": "approved" }
    }))
    .unwrap();
    let out = render_engine_widget("pr", &data, None, true).unwrap();
    assert!(
        out.starts_with("\x1b[32m"),
        "expected green (approved): {out:?}"
    );
}

#[test]
fn pr_url_becomes_osc8_hyperlink_under_color() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "pr": { "number": 5, "url": "https://github.com/o/r/pull/5" }
    }))
    .unwrap();
    let out = render_pr(&data, None, true);
    assert!(
        out.contains("\x1b]8;;https://github.com/o/r/pull/5\x1b\\"),
        "expected OSC 8 link: {out:?}"
    );
    assert!(out.contains("#5"));
    // A non-http(s) URL must NOT be linked (injection guard).
    let bad: EngineData = serde_json::from_value(serde_json::json!({
        "pr": { "number": 5, "url": "javascript:alert(1)" }
    }))
    .unwrap();
    assert!(!render_pr(&bad, None, true).contains("\x1b]8"));
    // No hyperlink when color is off.
    assert!(!render_pr(&data, None, false).contains("\x1b]8"));
}

#[test]
fn branch_links_to_pr_url_under_color() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "branch": { "name": "feat/x" },
        "pr": { "number": 5, "url": "https://github.com/o/r/pull/5" }
    }))
    .unwrap();
    let out = render_branch(&data, None, true);
    assert!(
        out.contains("\x1b]8;;https://github.com/o/r/pull/5"),
        "expected branch→PR link: {out:?}"
    );
}

#[cfg(unix)]
#[test]
fn branch_hyperlink_derives_pr_when_engine_sends_none() {
    // #973: engine sends no `pr` (Claude Code), branch widget must still
    // link via the derived PR — the same self-resolve #950 gave `{pr}`.
    let bin_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo_dir.path().join(".git")).unwrap();
    std::fs::write(
        repo_dir.path().join(".git").join("HEAD"),
        "ref: refs/heads/feat/x\n",
    )
    .unwrap();
    let gh = write_fake_gh(
        bin_dir.path(),
        "#!/bin/sh\necho '{\"number\":973,\"url\":\"https://github.com/o/r/pull/973\",\"reviewDecision\":\"APPROVED\"}'\n",
    );
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "workspace": { "current_dir": repo_dir.path().to_string_lossy() }
    }))
    .unwrap();
    let out = render_branch_with(&data, None, true, fake_gh(&gh.to_string_lossy()));
    assert!(
        out.contains("\x1b]8;;https://github.com/o/r/pull/973"),
        "expected branch→derived-PR link: {out:?}"
    );
}

#[test]
fn resolve_pr_none_when_no_workspace_dir() {
    assert_eq!(resolve_pr(&EngineData::default()), Ok(None));
}

#[cfg(unix)]
#[test]
fn resolve_pr_with_derives_pr_when_engine_sends_none() {
    // Full chain: no engine-supplied `pr`, but a resolvable branch and a
    // (faked) gh that has an open PR for it.
    let bin_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo_dir.path().join(".git")).unwrap();
    std::fs::write(
        repo_dir.path().join(".git").join("HEAD"),
        "ref: refs/heads/feat/x\n",
    )
    .unwrap();
    let gh = write_fake_gh(
        bin_dir.path(),
        "#!/bin/sh\necho '{\"number\":834,\"url\":\"https://github.com/o/r/pull/834\",\"reviewDecision\":\"APPROVED\"}'\n",
    );
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "workspace": { "current_dir": repo_dir.path().to_string_lossy() }
    }))
    .unwrap();
    assert_eq!(
        resolve_pr_with(&data, fake_gh(&gh.to_string_lossy())),
        Ok(Some(PrInfo {
            number: Some(834),
            url: Some("https://github.com/o/r/pull/834".to_string()),
            review_state: Some("approved".to_string()),
        }))
    );
}

#[cfg(unix)]
#[test]
fn resolve_pr_prefers_engine_supplied_over_derivation() {
    // gh would return a *different* PR (#999) if invoked — proves the
    // engine-supplied value wins without ever consulting derivation.
    let bin_dir = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(
        dir.path().join(".git").join("HEAD"),
        "ref: refs/heads/feat/x\n",
    )
    .unwrap();
    let gh = write_fake_gh(bin_dir.path(), "#!/bin/sh\necho '{\"number\":999}'\n");
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "workspace": { "current_dir": dir.path().to_string_lossy() },
        "pr": { "number": 5, "url": "https://github.com/o/r/pull/5" }
    }))
    .unwrap();
    assert_eq!(
        resolve_pr_with(&data, fake_gh(&gh.to_string_lossy())),
        Ok(Some(PrInfo {
            number: Some(5),
            url: Some("https://github.com/o/r/pull/5".to_string()),
            review_state: None,
        }))
    );
}

#[cfg(unix)]
#[test]
fn resolve_pr_none_for_detached_head_never_calls_gh() {
    // A gh that would succeed if invoked — proves detached HEAD stops
    // derivation before ever reaching gh, not that gh happened to fail.
    let bin_dir = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(
        dir.path().join(".git").join("HEAD"),
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
    )
    .unwrap();
    let gh = write_fake_gh(bin_dir.path(), "#!/bin/sh\necho '{\"number\":1}'\n");
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "workspace": { "current_dir": dir.path().to_string_lossy() }
    }))
    .unwrap();
    assert_eq!(
        resolve_pr_with(&data, fake_gh(&gh.to_string_lossy())),
        Ok(None)
    );
}

#[test]
fn pr_widget_shows_unreadable_when_branch_cannot_be_read() {
    // A directory where HEAD should be cannot be read as a file (#2710).
    // The PR widget says so without the branch widget, so it does not hide (#2749).
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git").join("HEAD")).unwrap();
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "workspace": { "current_dir": dir.path().to_string_lossy() }
    }))
    .unwrap();
    assert_eq!(render_pr(&data, None, false), "pr: unreadable");
}

#[test]
fn map_review_decision_matches_gh_values() {
    assert_eq!(map_review_decision("APPROVED"), Some("approved"));
    assert_eq!(
        map_review_decision("CHANGES_REQUESTED"),
        Some("changes_requested")
    );
    assert_eq!(
        map_review_decision("REVIEW_REQUIRED"),
        Some("review_required")
    );
    assert_eq!(map_review_decision(""), None);
    assert_eq!(map_review_decision("SOMETHING_NEW"), None);
}

#[test]
fn parse_gh_pr_view_maps_fields_and_review_decision() {
    let json =
        br#"{"number":834,"url":"https://github.com/o/r/pull/834","reviewDecision":"APPROVED"}"#;
    assert_eq!(
        parse_gh_pr_view(json),
        Some(PrInfo {
            number: Some(834),
            url: Some("https://github.com/o/r/pull/834".to_string()),
            review_state: Some("approved".to_string()),
        })
    );
}

#[test]
fn a_cached_failure_reads_back_as_a_failure_not_as_no_pr() {
    let cache_dir = tempfile::tempdir().unwrap();
    let path = cache_dir.path().join("pr-cache-x");
    write_pr_cache(&path, &Err(GhFailure::Unauthenticated), 1_000);
    assert_eq!(
        read_pr_cache(&path, 1_010),
        Some(Err(GhFailure::Unauthenticated))
    );
    assert_eq!(read_pr_cache(&path, 1_000 + PR_CACHE_TTL_SECS), None);
    // An entry from an older version has no failure field.
    std::fs::write(&path, r#"{"ts":1000,"pr":null}"#).unwrap();
    assert_eq!(read_pr_cache(&path, 1_010), Some(Ok(None)));
    assert_eq!(GhFailure::Unauthenticated.label(), "pr: gh not logged in");
}

#[test]
fn an_unreadable_cache_is_logged_and_read_as_a_miss() {
    // A directory in place of the file fails the read with an error other than NotFound.
    let cache_dir = tempfile::tempdir().unwrap();
    let path = cache_dir.path().join("pr-cache-x");
    std::fs::create_dir(&path).unwrap();
    let logs = crate::test_log_capture::capture_logs(|| {
        assert_eq!(read_pr_cache(&path, 1_000), None);
    });
    assert!(logs.contains("cannot be read"), "{logs}");
}

#[cfg(unix)]
#[test]
fn derive_pr_cache_hit_skips_gh() {
    let bin_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let gh = write_fake_gh(
        bin_dir.path(),
        "#!/bin/sh\necho called >> gh-calls.log\necho '{\"number\":1,\"url\":null,\"reviewDecision\":null}'\n",
    );
    let cache_path = pr_cache_path(cache_dir.path(), repo_dir.path(), "feat/x");
    write_pr_cache(
        &cache_path,
        &Ok(Some(PrInfo {
            number: Some(999),
            url: None,
            review_state: None,
        })),
        1_000,
    );
    // now=1_030 is within the 60s TTL of the ts=1_000 cache entry.
    let pr = derive_pr(
        fake_gh(&gh.to_string_lossy()),
        repo_dir.path(),
        "feat/x",
        Some(cache_dir.path()),
        1_030,
    );
    assert_eq!(
        pr.clone().ok().flatten().and_then(|p| p.number),
        Some(999),
        "must return the cached value, not gh's"
    );
    assert!(
        !repo_dir.path().join("gh-calls.log").exists(),
        "a cache hit must not shell out to gh"
    );
}

#[cfg(unix)]
#[test]
fn derive_pr_cache_expiry_calls_gh_again() {
    let bin_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let gh = write_fake_gh(
        bin_dir.path(),
        "#!/bin/sh\necho called >> gh-calls.log\necho '{\"number\":2,\"url\":null,\"reviewDecision\":null}'\n",
    );
    let cache_path = pr_cache_path(cache_dir.path(), repo_dir.path(), "feat/x");
    write_pr_cache(
        &cache_path,
        &Ok(Some(PrInfo {
            number: Some(999),
            url: None,
            review_state: None,
        })),
        1_000,
    );
    // now=1_000+PR_CACHE_TTL_SECS+1 is past the cache entry's TTL.
    let now = 1_000 + PR_CACHE_TTL_SECS + 1;
    let pr = derive_pr(
        fake_gh(&gh.to_string_lossy()),
        repo_dir.path(),
        "feat/x",
        Some(cache_dir.path()),
        now,
    );
    assert_eq!(
        pr.clone().ok().flatten().and_then(|p| p.number),
        Some(2),
        "expiry must re-query gh, not reuse the stale cache"
    );
    let calls = std::fs::read_to_string(repo_dir.path().join("gh-calls.log")).unwrap();
    assert_eq!(calls.lines().count(), 1, "gh should be called after expiry");
}

// #1198: distinct from the throttle usage-cache directory #1186 already
// hardened — this is the statusline widget's own PR-lookup cache.
#[cfg(unix)]
#[test]
fn write_pr_cache_creates_dir_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let cache_dir = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_path = pr_cache_path(cache_dir.path(), repo_dir.path(), "feat/x");
    write_pr_cache(&cache_path, &Ok(None), 1_000);

    let mode = std::fs::metadata(cache_path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "pr cache dir must be owner-only, got {mode:o}");
}

// #1198: distinct from the throttle usage-cache directory #1186 already
// hardened — this is the statusline widget's own usage-delta cache.
#[cfg(unix)]
#[test]
fn write_usage_state_creates_dir_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let state_dir = tempfile::tempdir().unwrap();
    let path = state_dir.path().join("nested").join("usage-delta");
    write_usage_state(&path, 42.0, 1_000);

    let mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o700,
        "usage delta state dir must be owner-only, got {mode:o}"
    );
}

#[test]
fn untrusted_fields_are_sanitized_end_to_end() {
    // A directory name carrying an escape sequence must not leak into the
    // terminal now that finish() no longer strips.
    let folder: EngineData = serde_json::from_value(serde_json::json!({
        "workspace": { "current_dir": "/tmp/ev\u{1b}[31mil" }
    }))
    .unwrap();
    let out = render_engine_widget("folder", &folder, None, false).unwrap();
    assert!(
        !out.contains('\u{1b}'),
        "escape leaked from folder: {out:?}"
    );

    let model: EngineData = serde_json::from_value(serde_json::json!({
        "model": { "display_name": "Op\u{1b}[31mus 4.8" }
    }))
    .unwrap();
    let out = render_engine_widget("model", &model, None, false).unwrap();
    assert!(!out.contains('\u{1b}'), "escape leaked from model: {out:?}");
}

#[test]
fn renders_context_percent_and_bar_by_default() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": { "remaining_percentage": 65.0 }
    }))
    .unwrap();
    let out = render_engine_widget("context", &data, None, false).unwrap();
    assert_eq!(out, "35% ▓▓▓░░░░░░░");
}

#[test]
fn context_threshold_colors_by_usage() {
    assert_eq!(threshold_style(30.0, [50, 80]), "green");
    assert_eq!(threshold_style(60.0, [50, 80]), "yellow");
    assert_eq!(threshold_style(90.0, [50, 80]), "red");
    // End-to-end: 90% used (remaining 10) → red SGR wrap under color.
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": { "remaining_percentage": 10.0 }
    }))
    .unwrap();
    let out = render_engine_widget("context", &data, None, true).unwrap();
    assert!(out.starts_with("\x1b[31m"), "expected red wrap: {out:?}");
}

#[test]
fn context_dims_empty_cells_per_cell_under_color() {
    // 35% used → 3 filled ▓ + 7 dim ░, each cell independently colored.
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": { "remaining_percentage": 65.0 }
    }))
    .unwrap();
    let out = render_engine_widget("context", &data, None, true).unwrap();
    assert!(
        out.contains("\x1b[2m░\x1b[0m"),
        "empty cells should be dim: {out:?}"
    );
    assert!(out.contains('▓'), "filled cells present: {out:?}");
}

#[test]
fn context_width_configurable() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": { "remaining_percentage": 50.0 }
    }))
    .unwrap();
    let cfg = llmenv_config::WidgetConfig {
        width: Some(4),
        ..Default::default()
    };
    // 50% of 4 cells = 2 filled.
    assert_eq!(render_context(&data, Some(&cfg), false), "50% ▓▓░░");
}

#[test]
fn missing_branch_and_pr_render_empty() {
    let empty = EngineData::default();
    assert_eq!(
        render_engine_widget("branch", &empty, None, false).unwrap(),
        ""
    );
    assert_eq!(render_engine_widget("pr", &empty, None, false).unwrap(), "");
    assert_eq!(
        render_engine_widget("context", &empty, None, false).unwrap(),
        ""
    );
}

fn usage_args(window: &RateLimitWindow, now: i64, window_secs: i64) -> UsageArgs<'_> {
    UsageArgs {
        window: Some(window),
        now,
        window_secs,
        thresholds: [70, 90],
        state_dir: None, // delta disabled in tests
        state_key: "t",
        use_color: false,
    }
}

#[test]
fn renders_usage_5h_minutes_reset() {
    // now=1000, resets_at=now+1380 (23m), used 8%. Format has no {bar}, so
    // the pace marker doesn't show.
    let window = RateLimitWindow {
        used_percentage: Some(8.0),
        resets_at: Some(1000 + 1380),
    };
    assert_eq!(
        render_usage(
            &usage_args(&window, 1000, usage::FIVE_HOUR_SECS),
            None,
            "5h {pct}% ➡{reset}"
        ),
        "5h 8% ➡23m"
    );
}

#[test]
fn renders_usage_bar_with_pace_marker() {
    // now=0, resets_at=10980 (3h03m), window 5h → 39% elapsed, so the
    // pace-target marker │ lands at bar column 4; used 100% fills the rest.
    let window = RateLimitWindow {
        used_percentage: Some(100.0),
        resets_at: Some(10_980),
    };
    assert_eq!(
        render_usage(
            &usage_args(&window, 0, usage::FIVE_HOUR_SECS),
            None,
            "{bar} {pct}% ➡{reset}"
        ),
        "▓▓▓▓│▓▓▓▓▓ 100% ➡3h03m"
    );
}

#[test]
fn usage_bar_width_configurable() {
    // 50% of a 4-cell bar = 2 filled, matching `context`/`cache_usage`'s
    // `width` override (#905 unified percent/bar rendering).
    let window = RateLimitWindow {
        used_percentage: Some(50.0),
        resets_at: None,
    };
    let cfg = llmenv_config::WidgetConfig {
        width: Some(4),
        ..Default::default()
    };
    assert_eq!(
        render_usage(
            &usage_args(&window, 0, usage::FIVE_HOUR_SECS),
            Some(&cfg),
            "{pct}% {bar}"
        ),
        "50% ▓▓░░"
    );
}

#[test]
fn usage_widget_via_dispatcher_reads_five_hour_window() {
    // engine_data()'s fixture rate_limits.five_hour.used_percentage = 24.5,
    // which rounds to 25. resets_at is in the past → "➡0m".
    let out = render_engine_widget("usage_5h", &engine_data(), None, false).unwrap();
    assert!(out.starts_with("5h 25%"), "usage_5h: {out}");
}

#[test]
fn usage_widgets_empty_when_windows_absent() {
    let empty = EngineData::default();
    assert_eq!(
        render_engine_widget("usage_5h", &empty, None, false).unwrap(),
        ""
    );
    assert_eq!(
        render_engine_widget("usage_7d", &empty, None, false).unwrap(),
        ""
    );
}

#[test]
fn usage_pace_indicator_over_and_under() {
    // now=1000, window=18000s. resets_at = now + (18000 - elapsed).
    // Under pace: only 10% used but 50% of the window elapsed → ⇣.
    let under = RateLimitWindow {
        used_percentage: Some(10.0),
        resets_at: Some(1000 + 9000), // 9000s remaining = 50% elapsed
    };
    assert_eq!(
        render_usage(&usage_args(&under, 1000, 18_000), None, "{pace}"),
        " \u{21e3}40%" // ⇣ 50 - 10 = 40 under
    );
    // Over pace: 80% used but only 25% elapsed → ⇡.
    let over = RateLimitWindow {
        used_percentage: Some(80.0),
        resets_at: Some(1000 + 13_500), // 25% elapsed
    };
    assert_eq!(
        render_usage(&usage_args(&over, 1000, 18_000), None, "{pace}"),
        " \u{21e1}55%" // ⇡ 80 - 25 = 55 over
    );
}

#[test]
fn usage_delta_tracks_change_across_renders() {
    let dir = tempfile::tempdir().unwrap();
    let window1 = RateLimitWindow {
        used_percentage: Some(20.0),
        resets_at: None,
    };
    let args1 = UsageArgs {
        window: Some(&window1),
        now: 1_000_000,
        window_secs: usage::FIVE_HOUR_SECS,
        thresholds: [70, 90],
        state_dir: Some(dir.path()),
        state_key: "usage_5h",
        use_color: false,
    };
    // First render: no prior sample → no delta, but state is written.
    assert_eq!(render_usage(&args1, None, "{delta}"), "");
    // Second render 120s later at 24.5% → +4.5.
    let window2 = RateLimitWindow {
        used_percentage: Some(24.5),
        resets_at: None,
    };
    let args2 = UsageArgs {
        window: Some(&window2),
        now: 1_000_120,
        ..args1
    };
    assert_eq!(render_usage(&args2, None, "{delta}"), " (+4.5)");
}

#[test]
fn usage_threshold_color_applied_end_to_end() {
    // 95% of the 5h window (threshold crit 90) → the pct is red-wrapped
    // (self-colored, so it's inside the "5h " prefix rather than leading).
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "rate_limits": { "five_hour": { "used_percentage": 95.0 } }
    }))
    .unwrap();
    let out = render_engine_widget("usage_5h", &data, None, true).unwrap();
    assert!(
        out.contains("\x1b[31m95\x1b[0m"),
        "expected red pct: {out:?}"
    );
}

#[test]
fn peak_widget_renders_symbol_and_label() {
    let out = render_engine_widget("peak", &EngineData::default(), None, false).unwrap();
    // Always one of the two forms, with a label.
    assert!(
        out.contains("peak") || out.contains("off-peak"),
        "peak widget: {out}"
    );
    assert!(out.starts_with('\u{25b3}') || out.starts_with('\u{25bd}'));
}

#[test]
fn humanize_until_formats_and_clamps() {
    assert_eq!(humanize_until(500, 1000), "0m"); // past → clamp
    assert_eq!(humanize_until(1000 + 1380, 1000), "23m");
    assert_eq!(humanize_until(10_980, 0), "3h03m");
}

#[test]
fn missing_workspace_and_context_window_render_empty() {
    // Covers the "no data" guard for the 5 engine widgets not exercised
    // by missing_field_renders_empty_not_panic / missing_branch_and_pr_render_empty.
    let empty = EngineData::default();
    for name in ["folder", "context", "duration", "tokens", "budget"] {
        assert_eq!(
            render_engine_widget(name, &empty, None, false).unwrap(),
            "",
            "widget {name} should render empty on missing data"
        );
    }
}

#[test]
fn render_budget_empty_when_context_window_size_absent() {
    // render_budget has two guards: no context_window at all (covered
    // above), and context_window present but context_window_size unset —
    // this test isolates the second guard specifically.
    let data = EngineData {
        context_window: Some(ContextWindow {
            remaining_percentage: Some(50.0),
            context_window_size: None,
            current_usage: None,
        }),
        ..Default::default()
    };
    let out = render_engine_widget("budget", &data, None, false).unwrap();
    assert_eq!(out, "");
}

#[test]
fn context_renders_empty_for_nan_and_infinite() {
    // Same untrusted-input hazard as elsewhere: NaN survives f64::clamp
    // unchanged (NaN comparisons are always false), so this must be
    // checked explicitly rather than relying on clamp alone.
    let nan_data = EngineData {
        context_window: Some(ContextWindow {
            remaining_percentage: Some(f64::NAN),
            context_window_size: None,
            current_usage: None,
        }),
        ..Default::default()
    };
    let out = render_engine_widget("context", &nan_data, None, false).unwrap();
    assert_eq!(out, "");

    let inf_data = EngineData {
        context_window: Some(ContextWindow {
            remaining_percentage: Some(f64::INFINITY),
            context_window_size: None,
            current_usage: None,
        }),
        ..Default::default()
    };
    let out = render_engine_widget("context", &inf_data, None, false).unwrap();
    assert_eq!(out, "");
}

#[test]
fn context_full_at_zero_remaining() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": { "remaining_percentage": 0.0 }
    }))
    .unwrap();
    let out = render_engine_widget("context", &data, None, false).unwrap();
    assert_eq!(out, "100% ▓▓▓▓▓▓▓▓▓▓");
}

#[test]
fn context_empty_at_full_remaining() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": { "remaining_percentage": 100.0 }
    }))
    .unwrap();
    let out = render_engine_widget("context", &data, None, false).unwrap();
    assert_eq!(out, "0% ░░░░░░░░░░");
}

#[test]
fn render_context_honors_custom_format() {
    let mut data = engine_data();
    data.context_window = Some(ContextWindow {
        remaining_percentage: Some(65.0),
        ..data.context_window.unwrap()
    });
    let cfg = llmenv_config::WidgetConfig {
        format: Some("used {pct} percent".to_string()),
        ..Default::default()
    };
    assert_eq!(render_context(&data, Some(&cfg), false), "used 35 percent");
}

#[test]
fn render_folder_honors_custom_format() {
    let data = engine_data(); // workspace.current_dir = "/home/user/llmenv"
    let cfg = llmenv_config::WidgetConfig {
        format: Some("{path}/{basename}".to_string()),
        ..Default::default()
    };
    assert_eq!(render_folder(&data, Some(&cfg)), "/home/user/llmenv/llmenv");
}

#[test]
fn render_duration_honors_custom_format() {
    let data = engine_data(); // cost.total_duration_ms = 13_320_000
    let cfg = llmenv_config::WidgetConfig {
        format: Some("{s}s total, {total_ms}ms".to_string()),
        ..Default::default()
    };
    assert_eq!(
        render_duration(&data, Some(&cfg)),
        "13320s total, 13320000ms"
    );
}

#[test]
fn render_tokens_honors_custom_format() {
    let data = engine_data(); // input 5000, cache_create 1000, cache_read 4000
    let cfg = llmenv_config::WidgetConfig {
        format: Some("in={input} cr={cache_read} cc={cache_create} tot={total}".to_string()),
        ..Default::default()
    };
    assert_eq!(
        render_tokens(&data, Some(&cfg)),
        "in=5k cr=4k cc=1k tot=10k"
    );
}

#[test]
fn render_budget_honors_custom_format() {
    let data = engine_data(); // context_window_size 200_000, used 10_000
    let cfg = llmenv_config::WidgetConfig {
        format: Some("{max} total, {used} used".to_string()),
        ..Default::default()
    };
    assert_eq!(render_budget(&data, Some(&cfg)), "200k total, 10k used");
}

#[test]
fn render_cache_usage_honors_custom_format() {
    let data = engine_data(); // cache 5000 / total 10000 = 50%
    let cfg = llmenv_config::WidgetConfig {
        format: Some("cache={pct}%".to_string()),
        ..Default::default()
    };
    assert_eq!(render_cache_usage(&data, Some(&cfg), false), "cache=50%");
}

#[test]
fn render_branch_honors_custom_format() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "branch": { "name": "release/3.x" }
    }))
    .unwrap();
    let cfg = llmenv_config::WidgetConfig {
        format: Some("on {name}".to_string()),
        ..Default::default()
    };
    assert_eq!(render_branch(&data, Some(&cfg), false), "on release/3.x");
}

#[test]
fn render_pr_honors_custom_format() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "pr": { "number": 834 }
    }))
    .unwrap();
    let cfg = llmenv_config::WidgetConfig {
        format: Some("PR#{number}".to_string()),
        ..Default::default()
    };
    assert_eq!(render_pr(&data, Some(&cfg), false), "PR#834");
}

#[test]
fn render_context_honors_custom_format_with_bar() {
    let data: EngineData = serde_json::from_value(serde_json::json!({
        "context_window": { "remaining_percentage": 65.0 }
    }))
    .unwrap();
    let cfg = llmenv_config::WidgetConfig {
        format: Some("{pct}|{bar}".to_string()),
        ..Default::default()
    };
    assert_eq!(render_context(&data, Some(&cfg), false), "35|▓▓▓░░░░░░░");
}

#[test]
fn format_token_count_thresholds_and_trimmed_decimals() {
    assert_eq!(format_token_count(42), "42");
    assert_eq!(format_token_count(999), "999");
    assert_eq!(format_token_count(1000), "1k"); // trailing ".0" dropped
    assert_eq!(format_token_count(1500), "1.5k");
    assert_eq!(format_token_count(109_200), "109.2k");
    assert_eq!(format_token_count(200_000), "200k");
    assert_eq!(format_token_count(1_000_000), "1m");
    assert_eq!(format_token_count(1_500_000), "1.5m");
}

#[test]
fn render_context_rounds_fractional_remaining_pct_but_truncates_bar_fill() {
    let data = EngineData {
        context_window: Some(ContextWindow {
            remaining_percentage: Some(64.5),
            context_window_size: None,
            current_usage: None,
        }),
        ..Default::default()
    };
    // used = 35.5: displayed pct rounds to 36, but fill truncates to 3
    // cells (35.5 / 10.0 = 3.55 -> 3), matching the doc comment's stated
    // "truncate, not round" bar-fill behavior.
    let out = render_engine_widget("context", &data, None, false).unwrap();
    assert_eq!(out, "36% ▓▓▓░░░░░░░");
}

#[test]
fn render_cache_usage_empty_when_total_tokens_zero_but_context_window_present() {
    let data = EngineData {
        context_window: Some(ContextWindow {
            remaining_percentage: None,
            context_window_size: Some(200_000),
            current_usage: Some(TokenUsage {
                input_tokens: Some(0),
                cache_creation_input_tokens: Some(0),
                cache_read_input_tokens: Some(0),
            }),
        }),
        ..Default::default()
    };
    let out = render_engine_widget("cache_usage", &data, None, false).unwrap();
    assert_eq!(out, "");
}

#[test]
fn tokens_and_cache_usage_saturate_instead_of_overflowing() {
    let data = EngineData {
        context_window: Some(ContextWindow {
            remaining_percentage: None,
            context_window_size: None,
            current_usage: Some(TokenUsage {
                input_tokens: Some(u64::MAX),
                cache_creation_input_tokens: Some(u64::MAX),
                cache_read_input_tokens: Some(u64::MAX),
            }),
        }),
        ..Default::default()
    };
    // Must not panic (debug overflow) and must not wrap into a bogus
    // small number (release overflow).
    let tokens = render_engine_widget("tokens", &data, None, false).unwrap();
    assert!(!tokens.is_empty());
    let cache_usage = render_engine_widget("cache_usage", &data, None, false).unwrap();
    assert_eq!(cache_usage, "\u{21bb}100%");
}

fn data_with_remaining(remaining: f64) -> EngineData {
    EngineData {
        context_window: Some(ContextWindow {
            remaining_percentage: Some(remaining),
            context_window_size: None,
            current_usage: None,
        }),
        ..Default::default()
    }
}

/// (name, declared placeholders) for every engine widget that builds its
/// output via a chained `format.replace()` call.
const ENGINE_WIDGET_PLACEHOLDERS: &[(&str, &[&str])] = &[
    ("model", &["short_name", "version", "full_name"]),
    ("folder", &["basename", "path"]),
    ("duration", &["h", "m", "s", "total_ms"]),
    ("tokens", &["total", "input", "cache_read", "cache_create"]),
    ("budget", &["used", "max"]),
    ("cache_usage", &["pct", "bar"]),
    ("branch", &["name"]),
    ("pr", &["number", "url", "review_state"]),
    ("context", &["pct", "bar"]),
    ("usage_5h", &["pct", "bar", "reset", "pace", "delta"]),
    ("usage_7d", &["pct", "bar", "reset", "pace", "delta"]),
    ("peak", &["symbol", "label", "countdown"]),
];

/// Arbitrary [`PrInfo`], for the JSON-roundtrip and cache-entry proptests.
fn arb_pr_info() -> impl Strategy<Value = PrInfo> {
    (
        prop::option::of(any::<u64>()),
        prop::option::of(".{0,80}"),
        prop::option::of(".{0,40}"),
    )
        .prop_map(|(number, url, review_state)| PrInfo {
            number,
            url,
            review_state,
        })
}

proptest! {
    /// The format string comes from user config — untrusted-ish. No
    /// arbitrary text should ever make a `.replace()` chain panic.
    #[test]
    fn engine_widget_never_panics_on_arbitrary_format_string(
        idx in 0..ENGINE_WIDGET_PLACEHOLDERS.len(),
        format in ".{0,200}",
    ) {
        let (name, _) = ENGINE_WIDGET_PLACEHOLDERS[idx];
        let cfg = llmenv_config::WidgetConfig {
            format: Some(format),
            ..Default::default()
        };
        let _ = render_engine_widget(name, &engine_data(), Some(&cfg), false);
    }

    /// Every placeholder a widget declares (present in its default format
    /// string) must be fully consumed by the `.replace()` chain — none
    /// should survive into the rendered output.
    #[test]
    fn engine_widget_consumes_all_declared_placeholders(junk in "[^{}]{0,10}") {
        let data = engine_data();
        for (name, placeholders) in ENGINE_WIDGET_PLACEHOLDERS {
            let mut format = junk.clone();
            for p in *placeholders {
                format.push('{');
                format.push_str(p);
                format.push('}');
                format.push_str(&junk);
            }
            let cfg = llmenv_config::WidgetConfig {
                format: Some(format),
                ..Default::default()
            };
            let out = render_engine_widget(name, &data, Some(&cfg), false).unwrap();
            for p in *placeholders {
                let token = format!("{{{p}}}");
                prop_assert!(
                    !out.contains(&token),
                    "widget {name} left placeholder {token} unconsumed in {out:?}"
                );
            }
        }
    }

    /// `remaining_percentage` is untrusted external input — must never
    /// panic across the full f64 space (NaN, +/-inf, denormals, extremes)
    /// and must always stay within the documented output contract.
    #[test]
    fn render_context_never_panics_and_stays_in_contract(remaining in any::<f64>()) {
        let data = data_with_remaining(remaining);
        let out = render_engine_widget("context", &data, None, false).unwrap();
        if remaining.is_finite() {
            let (pct_str, bar) = out.split_once(' ').unwrap();
            let pct: i64 = pct_str.trim_end_matches('%').parse().unwrap();
            prop_assert!((0..=100).contains(&pct));
            prop_assert_eq!(bar.chars().count(), 10);
            prop_assert!(bar.chars().all(|c| c == '▓' || c == '░'));
        } else {
            prop_assert_eq!(out, "");
        }
    }

    /// Numeric formatting across the full `u64` space: no panics on the
    /// division/rounding, the m/k/bare thresholds hold, and a trailing
    /// ".0" is always dropped (never surfaces in output).
    #[test]
    fn format_token_count_respects_threshold(n in any::<u64>()) {
        let out = format_token_count(n);
        if n < 1000 {
            prop_assert_eq!(out, n.to_string());
        } else if n < 1_000_000 {
            prop_assert!(out.ends_with('k'), "expected k suffix: {out}");
            prop_assert!(!out.contains(".0"), "trailing .0 not trimmed: {out}");
        } else {
            prop_assert!(out.ends_with('m'), "expected m suffix: {out}");
            prop_assert!(!out.contains(".0"), "trailing .0 not trimmed: {out}");
        }
    }

    /// `ms -> h/m` conversion across the full `u64` space: must never
    /// panic (division/modulo only, no overflow-prone arithmetic) and
    /// must reproduce the same breakdown via independent u128 math.
    #[test]
    fn render_duration_never_panics_and_matches_independent_calc(ms in any::<u64>()) {
        let data = EngineData {
            cost: Some(Cost {
                total_duration_ms: Some(ms),
            }),
            ..Default::default()
        };
        let out = render_engine_widget("duration", &data, None, false).unwrap();
        let total_secs = ms / 1000;
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        let body = if h > 0 {
            format!("{h}h {m}m")
        } else if m > 0 {
            format!("{m}m {s}s")
        } else {
            format!("{s}s")
        };
        prop_assert_eq!(out, format!("\u{23f1} {body}"));
    }

    /// Token counts are untrusted `u64`s from the engine's stdin JSON —
    /// must never panic (saturating arithmetic) and, when a total exists,
    /// the cache percentage must stay within the documented `0..=100`
    /// contract, mirroring `render_context`'s guarantee.
    #[test]
    fn render_cache_usage_never_panics_and_stays_in_contract(
        input in any::<u64>(),
        cache_creation in any::<u64>(),
        cache_read in any::<u64>(),
    ) {
        let data = EngineData {
            context_window: Some(ContextWindow {
                remaining_percentage: None,
                context_window_size: None,
                current_usage: Some(TokenUsage {
                    input_tokens: Some(input),
                    cache_creation_input_tokens: Some(cache_creation),
                    cache_read_input_tokens: Some(cache_read),
                }),
            }),
            ..Default::default()
        };
        let out = render_engine_widget("cache_usage", &data, None, false).unwrap();
        let cache = cache_read.saturating_add(cache_creation);
        let total = input.saturating_add(cache);
        if total == 0 {
            prop_assert_eq!(out, "");
        } else {
            let pct: i64 = out
                .trim_start_matches('↻')
                .trim_end_matches('%')
                .parse()
                .unwrap();
            prop_assert!((0..=100).contains(&pct));
        }
    }

    /// `short_model_name` only ever removes tokens (the "claude" literal
    /// and any version-shaped token) — every surviving token must be one
    /// of the original whitespace-split tokens, in original order.
    #[test]
    fn short_model_name_only_removes_claude_and_version_tokens(
        tokens in prop::collection::vec("[a-zA-Z0-9]{1,8}", 0..6),
    ) {
        let display_name = tokens.join(" ");
        let out = short_model_name(&display_name);
        let kept: Vec<&str> = out.split_whitespace().collect();
        let mut remaining = tokens.iter().map(String::as_str);
        for k in &kept {
            prop_assert!(
                !k.eq_ignore_ascii_case("claude") && !k.chars().any(|c| c.is_ascii_digit()),
                "kept token {k:?} should have been filtered"
            );
            prop_assert!(
                remaining.by_ref().any(|t| t == *k),
                "kept token {k:?} not found in original order in {tokens:?}"
            );
        }
    }

    /// A bar's filled-cell count never exceeds its width, and its total
    /// glyph count always equals `width` exactly — shared by every
    /// percentage-based widget via `pct_and_bar`, so a single mutation
    /// in this math would otherwise silently propagate to all of them.
    #[test]
    fn block_bar_filled_never_exceeds_width_and_total_len_matches(
        pct in 0.0f64..=100.0,
        width in 0usize..=50,
    ) {
        let bar = block_bar(pct, width);
        let filled = bar.chars().filter(|&c| c == '\u{2593}').count();
        prop_assert!(filled <= width);
        prop_assert_eq!(bar.chars().count(), width);
    }

    /// Monotonicity: a higher percentage never yields fewer filled cells
    /// at the same width.
    #[test]
    fn block_bar_is_monotonic_in_pct(
        lo in 0.0f64..=100.0,
        hi in 0.0f64..=100.0,
        width in 1usize..=50,
    ) {
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let filled_lo = block_bar(lo, width).chars().filter(|&c| c == '\u{2593}').count();
        let filled_hi = block_bar(hi, width).chars().filter(|&c| c == '\u{2593}').count();
        prop_assert!(filled_lo <= filled_hi);
    }

    /// Boundaries: 0% is always empty, 100% is always fully filled, at
    /// any width.
    #[test]
    fn block_bar_boundaries(width in 0usize..=50) {
        let empty = block_bar(0.0, width);
        prop_assert_eq!(empty.chars().filter(|&c| c == '\u{2593}').count(), 0);
        let full = block_bar(100.0, width);
        prop_assert_eq!(full.chars().filter(|&c| c == '\u{2593}').count(), width);
    }

    /// `colored_bar` with `use_color: false` and no marker must degrade
    /// to exactly `block_bar`'s output — it's documented as doing so.
    #[test]
    fn colored_bar_matches_block_bar_when_uncolored_and_unmarked(
        pct in 0.0f64..=100.0,
        width in 0usize..=50,
    ) {
        prop_assert_eq!(colored_bar(pct, width, "yellow", None, false), block_bar(pct, width));
    }

    /// `clean_display_name` is idempotent — applying it a second time
    /// never changes the result further.
    #[test]
    fn clean_display_name_is_idempotent(display_name in ".{0,60}") {
        let once = clean_display_name(&display_name);
        let twice = clean_display_name(once);
        prop_assert_eq!(once, twice);
    }

    /// `PrInfo` round-trips through JSON exactly — both the on-disk cache
    /// entry and any future engine-supplied `pr` field depend on this.
    #[test]
    fn pr_info_json_roundtrips(pr in arb_pr_info()) {
        let json = serde_json::to_string(&pr).unwrap();
        let parsed: PrInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(pr, parsed);
    }

    /// `PrCacheEntry` (the on-disk cache record) round-trips through JSON
    /// exactly, matching `read_pr_cache`/`write_pr_cache`'s expectations.
    #[test]
    fn pr_cache_entry_json_roundtrips(
        ts in any::<i64>(),
        pr in prop::option::of(arb_pr_info()),
        failure in prop::option::of(prop_oneof![
            Just(GhFailure::Missing),
            Just(GhFailure::Unauthenticated),
            Just(GhFailure::Failed),
        ]),
    ) {
        let entry = PrCacheEntry { ts, pr, failure };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: PrCacheEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(entry, parsed);
    }

    /// `pr_cache_path` is deterministic: the same (cache_dir, repo_dir,
    /// branch) key always yields the same path.
    #[test]
    fn pr_cache_path_is_deterministic(
        repo in "[a-zA-Z0-9/_.-]{1,40}",
        branch in "[a-zA-Z0-9/_.-]{1,40}",
    ) {
        let cache_dir = Path::new("/tmp/llmenv-pr-cache");
        let repo_dir = PathBuf::from(format!("/{repo}"));
        let a = pr_cache_path(cache_dir, &repo_dir, &branch);
        let b = pr_cache_path(cache_dir, &repo_dir, &branch);
        prop_assert_eq!(a, b);
    }

    /// Distinct `(repo_dir, branch)` pairs must produce distinct cache
    /// filenames — otherwise two unrelated repos/branches could collide
    /// on (and clobber, or read) each other's cached PR.
    #[test]
    fn pr_cache_path_distinct_for_distinct_keys(
        repo1 in "[a-zA-Z0-9/_.-]{1,40}",
        branch1 in "[a-zA-Z0-9/_.-]{1,40}",
        repo2 in "[a-zA-Z0-9/_.-]{1,40}",
        branch2 in "[a-zA-Z0-9/_.-]{1,40}",
    ) {
        prop_assume!(repo1 != repo2 || branch1 != branch2);
        let cache_dir = Path::new("/tmp/llmenv-pr-cache");
        let a = pr_cache_path(cache_dir, &PathBuf::from(format!("/{repo1}")), &branch1);
        let b = pr_cache_path(cache_dir, &PathBuf::from(format!("/{repo2}")), &branch2);
        prop_assert_ne!(a, b);
    }
}
