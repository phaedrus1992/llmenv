use super::*;

fn fetch() -> serde_json::Value {
    json!({"tool_name": "WebFetch", "tool_input": {"url": "https://a"}, "tool_response": "x"})
}

#[test]
fn a_page_is_stored_under_the_topic_of_its_project() {
    let args = web_fetch_store_args(&fetch(), Some("proj-a")).unwrap();
    assert_eq!(args["topic"], "web-fetch-proj-a");
}

#[test]
fn a_page_with_no_project_keeps_the_plain_topic() {
    for project in [None, Some("")] {
        let args = web_fetch_store_args(&fetch(), project).unwrap();
        assert_eq!(args["topic"], "web-fetch", "{project:?}");
    }
}

#[test]
fn only_the_web_fetch_topics_count_as_fetched_pages() {
    for topic in ["web-fetch", "web-fetch-llmenv", "web-fetch-a-b"] {
        assert!(is_web_fetch_topic(topic), "{topic}");
    }
    for topic in [
        "web-fetcher",
        "context-web-fetch",
        "web",
        "",
        "decisions-llmenv",
    ] {
        assert!(!is_web_fetch_topic(topic), "{topic}");
    }
}

#[test]
fn a_page_line_that_looks_like_a_record_stays_inside_the_page_record() {
    let mut payload = fetch();
    payload["tool_response"] = json!("intro\n[1] Smith J., 2020.\n[web-fetch] more");
    let args = web_fetch_store_args(&payload, Some("proj-a")).unwrap();
    let recalled = format!("[web-fetch-proj-a] {}", args["content"].as_str().unwrap());
    let records = crate::hook_run::action::split_recall_records(&recalled);
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(records[0].contains("Smith"), "{records:?}");
}

#[test]
fn a_call_that_is_not_a_web_fetch_looks_up_no_project() {
    let payload = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}, "cwd": "/"});
    assert!(handle_web_fetch_in(None, &payload).is_none());
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(!notices.contains("fetched page"), "{notices}");
}

#[test]
fn an_unknown_project_is_an_agent_notice() {
    assert_eq!(session_project(&json!({"cwd": "/"})), None);
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(notices.contains("stored with no project"), "{notices}");
}

proptest::proptest! {
    #[test]
    fn only_the_web_fetch_topics_match(topic in "(web-fetch|web|fetch|x)?[-a-z]{0,8}") {
        let expected = topic == "web-fetch" || topic.starts_with("web-fetch-");
        proptest::prop_assert_eq!(is_web_fetch_topic(&topic), expected);
    }

    #[test]
    fn a_stored_page_topic_is_a_web_fetch_topic(project in proptest::option::of("[-a-z0-9]{0,8}")) {
        proptest::prop_assert!(is_web_fetch_topic(&web_fetch_topic(project.as_deref())));
    }
}

#[test]
fn the_project_comes_from_the_payload_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("named-folder");
    std::fs::create_dir(&cwd).unwrap();
    let payload = json!({"cwd": cwd.display().to_string()});
    assert_eq!(session_project(&payload).as_deref(), Some("named-folder"));
}

#[test]
fn web_fetch_store_args_extracts_url_and_summary() {
    let payload = json!({
        "tool_name": "WebFetch",
        "tool_input": {"url": "https://example.com"},
        "tool_response": "# Hello\n\nThis is fetched content",
    });
    let args = web_fetch_store_args(&payload, None).expect("should detect WebFetch");
    assert_eq!(args["topic"], "web-fetch");
    assert_eq!(args["importance"], "low");
    let content = args["content"].as_str().unwrap();
    assert!(content.contains("https://example.com"), "url in content");
    assert!(content.contains("WebFetch"), "tool name in content");
    assert!(
        content.contains("Fetched at (epoch)"),
        "timestamp in content"
    );
    assert!(content.contains("Hello"), "content preview in content");
}

#[test]
fn web_fetch_request_id_is_stable_per_tool_call_and_keyed_by_session() {
    let payload = |tool_use_id: &str, url: &str| {
        json!({
            "session_id": "sess-1", "tool_use_id": tool_use_id, "tool_name": "WebFetch",
            "tool_input": {"url": url}, "tool_response": "body",
        })
    };
    let a = web_fetch_store_args(&payload("tu1", "https://a"), None).expect("args");
    let again = web_fetch_store_args(&payload("tu1", "https://a"), None).expect("args");
    assert_eq!(
        a[detached_store::REQUEST_ID_FIELD],
        again[detached_store::REQUEST_ID_FIELD]
    );
    assert_eq!(a[detached_store::REQUEST_KEY_FIELD], "sess-1");
    let other_call = web_fetch_store_args(&payload("tu2", "https://a"), None).expect("args");
    assert_ne!(
        a[detached_store::REQUEST_ID_FIELD],
        other_call[detached_store::REQUEST_ID_FIELD]
    );
    let other_url = web_fetch_store_args(&payload("tu1", "https://b"), None).expect("args");
    assert_ne!(
        a[detached_store::REQUEST_ID_FIELD],
        other_url[detached_store::REQUEST_ID_FIELD]
    );
}

#[test]
fn web_fetch_store_args_supports_web_search() {
    let payload = json!({
        "tool_name": "WebSearch",
        "tool_input": {"query": "rust programming"},
        "tool_response": "Search results here",
    });
    let args = web_fetch_store_args(&payload, None).expect("should detect WebSearch");
    assert_eq!(args["topic"], "web-fetch");
    let content = args["content"].as_str().unwrap();
    assert!(content.contains("WebSearch"));
    assert!(content.contains("Search results"));
    assert!(content.contains("Query: rust programming"));
    assert!(content.contains("Tool: WebSearch"));
}

#[test]
fn web_fetch_store_args_ignores_non_web_fetch_tools() {
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
        "tool_response": "result",
    });
    assert!(
        web_fetch_store_args(&payload, None).is_none(),
        "non-WebFetch tool should return None"
    );
}

#[test]
fn web_fetch_store_args_handles_missing_url_or_tool_input() {
    // Missing url key within tool_input.
    let payload = json!({
        "tool_name": "WebFetch",
        "tool_input": {},
        "tool_response": "content",
    });
    let args = web_fetch_store_args(&payload, None).expect("should handle missing url");
    let content = args["content"].as_str().unwrap();
    assert!(content.contains("unknown"), "should fall back to 'unknown'");

    // Missing entire tool_input key — same serde_json Null path.
    let payload2 = json!({
        "tool_name": "WebFetch",
        "tool_response": "content",
    });
    let args2 = web_fetch_store_args(&payload2, None).expect("should handle missing tool_input");
    let content2 = args2["content"].as_str().unwrap();
    assert!(
        content2.contains("unknown"),
        "should fall back to 'unknown'"
    );
}

#[test]
fn web_fetch_store_args_handles_empty_response() {
    let payload = json!({
        "tool_name": "WebFetch",
        "tool_input": {"url": "https://example.com"},
        "tool_response": "",
    });
    let args = web_fetch_store_args(&payload, None).expect("should handle empty response");
    let content = args["content"].as_str().unwrap();
    assert!(
        content.contains("Content preview:\n"),
        "empty content after preview header"
    );
}

#[test]
fn a_preview_of_exactly_the_limit_has_no_truncation_mark() {
    let preview = |len: usize| {
        let payload = json!({
            "tool_name": "WebFetch",
            "tool_input": {"url": "https://example.com"},
            "tool_response": "x".repeat(len),
        });
        let args = web_fetch_store_args(&payload, None).expect("a web fetch");
        args["content"].as_str().unwrap().to_string()
    };
    assert!(!preview(1000).ends_with("(truncated)"));
    assert!(preview(1001).ends_with("... (truncated)"));
}

#[test]
fn web_fetch_store_args_truncates_long_content() {
    let long = "x".repeat(2000);
    let payload = json!({
        "tool_name": "WebFetch",
        "tool_input": {"url": "https://example.com"},
        "tool_response": long,
    });
    let args = web_fetch_store_args(&payload, None).expect("should handle long content");
    let content = args["content"].as_str().unwrap();
    let preview = content.split("Content preview:\n").nth(1).unwrap_or("");
    assert!(
        preview.ends_with("... (truncated)"),
        "truncation indicator should be present, got: {preview:?}"
    );
    let truncated = preview.strip_suffix("... (truncated)").unwrap_or(preview);
    assert!(
        truncated.len() <= 1000,
        "truncated content should be at most 1000 chars, got {}",
        truncated.len()
    );
}

#[test]
fn web_fetch_store_args_returns_none_for_null_payload() {
    assert!(web_fetch_store_args(&serde_json::Value::Null, None).is_none());
}

#[test]
fn web_fetch_store_args_returns_none_for_missing_tool_name() {
    let payload = json!({
        "tool_input": {"url": "https://example.com"},
        "tool_response": "content",
    });
    assert!(web_fetch_store_args(&payload, None).is_none());
}

#[test]
fn web_fetch_store_args_handles_object_tool_response() {
    let payload = json!({
        "tool_name": "WebFetch",
        "tool_input": {"url": "https://example.com"},
        "tool_response": {"content": [{"type": "text", "text": "hello world"}]},
    });
    let args = web_fetch_store_args(&payload, None).expect("should handle object response");
    let content = args["content"].as_str().unwrap();
    assert!(
        content.contains("hello world"),
        "extracted text from object response"
    );
}
