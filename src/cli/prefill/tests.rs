#![expect(
    clippy::unwrap_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]

use super::*;
use proptest::prelude::*;

fn shape(hash: &str, tools: &[(&str, &str)]) -> String {
    let tools: Vec<Value> = tools
        .iter()
        .map(|(name, desc)| serde_json::json!({"name": name, "description": desc, "input_schema": {}}))
        .collect();
    serde_json::json!({
        "type": "api-request-shape",
        "shapeHash": hash,
        "shape": {"system": [{"type": "text", "text": "sys"}], "tools": tools},
    })
    .to_string()
}

fn request(hash: &str) -> String {
    serde_json::json!({
        "type": "api-request",
        "shapeHash": hash,
        "cacheControl": [
            {"path": ["system", 2], "value": {"type": "ephemeral"}},
            {"path": ["messages", 3, "content", 0], "value": {"type": "ephemeral"}},
        ],
    })
    .to_string()
}

fn report_of(lines: &[String]) -> anyhow::Result<Report> {
    read_report(lines.join("\n").as_bytes())
}

fn error_of(lines: &[&str]) -> String {
    format!(
        "{:#}",
        read_report(lines.join("\n").as_bytes()).unwrap_err()
    )
}

#[test]
fn a_transcript_gives_sizes_top_tools_and_shape_changes() {
    let report = report_of(&[
        r#"{"type":"user","message":"hi"}"#.to_string(),
        shape(
            "a",
            &[("Read", "x"), ("icm_memory_recall", "a long description")],
        ),
        request("a"),
        request("a"),
        shape("b", &[("Read", "x")]),
        shape("b", &[("Read", "x")]),
        request("b"),
        "not json".to_string(),
    ])
    .unwrap();
    assert_eq!(report.requests, 3);
    assert_eq!(report.shape_changes, 1);
    assert_eq!(
        report.last_cache_control,
        vec!["system[2]", "messages[3].content[0]"]
    );
    let out = render(&report, Path::new("t.jsonl")).unwrap();
    assert!(
        out.contains("3 requests, 2 request shapes, 1 shape changes"),
        "{out}"
    );
    assert!(out.contains("in 1 schemas"), "{out}");
    assert!(
        out.contains("Cache breakpoints of the last request: system[2], messages[3].content[0]"),
        "{out}"
    );
}

#[test]
fn a_changed_record_format_names_the_line_and_never_reports_zero() {
    let err = error_of(&[r#"{"type":"api-request","cacheControl":[]}"#]);
    assert!(err.contains("line 1"), "{err}");
    assert!(err.contains("shapeHash"), "{err}");
    let err = error_of(&[r#"{"type":"api-request-shape","shapeHash":"a","shape":{"system":[]}}"#]);
    assert!(err.contains("shape.tools"), "{err}");
    let err = error_of(&[
        r#"{"type":"api-request-shape","shapeHash":"a","shape":{"system":null,"tools":[]}}"#,
    ]);
    assert!(err.contains("shape.system"), "{err}");
    let err = error_of(&[r#"{"type":"user"}"#]);
    assert!(err.contains("Claude Code 2.1.300"), "{err}");
}

#[test]
fn a_cut_request_record_is_an_error_but_other_bad_lines_are_skipped() {
    let full = request("a");
    let cut = r#"{"type":"api-request","shapeHash":"a","cacheCon"#;
    let err = error_of(&[&shape("a", &[]), cut]);
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("not complete JSON"), "{err}");
    assert!(report_of(&[shape("a", &[]), "{oops".to_string(), full]).is_ok());
}

#[test]
fn a_shape_hash_with_two_contents_is_an_error() {
    let err = error_of(&[&shape("a", &[]), &shape("a", &[("Read", "x")])]);
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("different content"), "{err}");
}

#[test]
fn a_request_whose_shape_is_missing_is_an_error_that_names_its_line() {
    let err = error_of(&[
        &shape("a", &[]),
        &request("a"),
        &request("zz"),
        &request("a"),
    ]);
    assert!(err.contains("line 3"), "{err}");
    assert!(
        err.contains("no api-request-shape record holds it"),
        "{err}"
    );
}

#[test]
fn transcript_text_is_cleaned_before_the_terminal() {
    let err = error_of(&[&request("\u{1b}[31mzz")]);
    assert!(!err.contains('\u{1b}'), "{err:?}");
    let report = report_of(&[shape("a", &[]), request("a")]).unwrap();
    let out = render(&report, Path::new("t\u{1b}[2J.jsonl")).unwrap();
    assert!(!out.contains('\u{1b}'), "{out:?}");
}

#[test]
fn a_breakpoint_path_is_a_list_of_keys_and_indexes() {
    let path = serde_json::json!(["messages", 1, "content", 0]);
    assert_eq!(breakpoint_path(&path).unwrap(), "messages[1].content[0]");
    for bad in [
        serde_json::json!("system[1]"),
        serde_json::json!([]),
        serde_json::json!(["system", -1]),
        serde_json::json!(["system", ""]),
        serde_json::json!(["system", null]),
        serde_json::json!(["system", 1.5]),
    ] {
        let err = breakpoint_path(&bad).unwrap_err().to_string();
        assert!(err.contains("cacheControl"), "{bad}: {err}");
    }
}

#[test]
fn the_project_folder_writes_each_non_alphanumeric_character_as_a_dash() {
    assert_eq!(
        project_folder_name("/Users/ranger/.cache/llmenv"),
        "-Users-ranger--cache-llmenv"
    );
    assert_eq!(project_folder_name("/a/my_dir-x/é"), "-a-my-dir-x--");
}

proptest! {
    /// The change count is the number of neighbouring requests with different shapes, and
    /// the tool sizes add up to the size of each schema's JSON text.
    #[test]
    fn shape_changes_count_neighbour_differences(hashes in proptest::collection::vec("[ab]", 1..12)) {
        let mut lines = vec![shape("a", &[("T", "d")]), shape("b", &[])];
        lines.extend(hashes.iter().map(|h| request(h)));
        let report = report_of(&lines).unwrap();
        let changes = hashes.windows(2).filter(|w| w[0] != w[1]).count();
        prop_assert_eq!(report.shape_changes, changes);
        prop_assert_eq!(report.requests, hashes.len());
        let tool = serde_json::json!({"name": "T", "description": "d", "input_schema": {}});
        prop_assert_eq!(report.shapes["a"].tools[0].1, tool.to_string().chars().count());
    }

    /// Any transcript gives a report or an error, never a panic, and a record error names a line.
    #[test]
    fn any_transcript_gives_a_report_or_a_line_error(
        lines in proptest::collection::vec(prop_oneof![
            ".{0,40}",
            Just(shape("a", &[("T", "d")])),
            Just(request("a")),
            "\\{\"type\":\"api-request(-shape)?\"(,\"shapeHash\":\"[a-z]{0,2}\")?\\}",
        ], 0..8)
    ) {
        if let Err(e) = report_of(&lines) {
            let err = format!("{e:#}");
            prop_assert!(err.contains("line ") || err.contains("no api-request record"), "{}", err);
        }
    }

    /// A path of non-empty keys and indexes renders each index as `[n]` and joins keys with `.`.
    #[test]
    fn a_breakpoint_path_renders_each_part_in_order(
        parts in proptest::collection::vec(prop_oneof![
            "[a-z]{1,6}".prop_map(Value::from),
            (0u64..1000).prop_map(Value::from),
        ], 1..6)
    ) {
        let mut expected = String::new();
        for part in &parts {
            match part {
                Value::Number(n) => expected.push_str(&format!("[{n}]")),
                Value::String(key) => {
                    if !expected.is_empty() {
                        expected.push('.');
                    }
                    expected.push_str(key);
                }
                other => return Err(TestCaseError::fail(format!("unexpected part {other}"))),
            }
        }
        prop_assert_eq!(breakpoint_path(&Value::Array(parts)).unwrap(), expected);
    }
}

#[test]
fn the_command_reads_a_transcript_and_fails_on_a_missing_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    std::fs::write(&path, [shape("a", &[]), request("a")].join("\n")).unwrap();
    assert!(
        run_prefill(&PrefillArgs {
            transcript: Some(path)
        })
        .is_ok()
    );
    let missing = PrefillArgs {
        transcript: Some(dir.path().join("absent.jsonl")),
    };
    let err = format!("{:#}", run_prefill(&missing).unwrap_err());
    assert!(err.contains("absent.jsonl"), "{err}");
}

#[test]
fn the_newest_jsonl_file_is_chosen_and_other_files_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
    let new = old + std::time::Duration::from_secs(60);
    let newest_other = new + std::time::Duration::from_secs(60);
    for (name, time) in [("a.jsonl", old), ("b.jsonl", new), ("c.txt", newest_other)] {
        let file = std::fs::File::create(dir.path().join(name)).unwrap();
        file.set_modified(time).unwrap();
    }
    let got = newest_jsonl_in(dir.path(), "Fix: x.").unwrap();
    assert_eq!(got, dir.path().join("b.jsonl"));
}

#[test]
fn a_folder_with_no_transcript_or_no_folder_is_an_error_with_the_fix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
    let err = newest_jsonl_in(dir.path(), "Fix: pass a path.")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("holds no transcript") && err.contains("Fix: pass a path."),
        "{err}"
    );
    let err = format!(
        "{:#}",
        newest_jsonl_in(&dir.path().join("absent"), "Fix: y.").unwrap_err()
    );
    assert!(
        err.contains("cannot list") && err.contains("Fix: y."),
        "{err}"
    );
}

#[test]
fn a_folder_named_like_a_transcript_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("newer.jsonl")).unwrap();
    let file = dir.path().join("real.jsonl");
    std::fs::File::create(&file)
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(newest_jsonl_in(dir.path(), "Fix: x.").unwrap(), file);
}

proptest! {
    /// For any set of transcripts with distinct change times, the newest one is chosen.
    #[test]
    fn the_newest_of_any_set_of_transcripts_is_chosen(
        secs in proptest::collection::btree_set(1u64..100_000, 1..6),
    ) {
        let dir = tempfile::tempdir().unwrap();
        for (i, s) in secs.iter().enumerate() {
            let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(*s);
            std::fs::File::create(dir.path().join(format!("t{i}.jsonl")))
                .unwrap()
                .set_modified(time)
                .unwrap();
        }
        let newest_index = secs.len() - 1;
        let want = dir.path().join(format!("t{newest_index}.jsonl"));
        prop_assert_eq!(newest_jsonl_in(dir.path(), "Fix: x.").unwrap(), want);
    }
}
