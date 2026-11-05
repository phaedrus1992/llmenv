//! `llmenv prefill`: the real prompt prefill of a Claude Code session, read from the request
//! records of its transcript (#3102).
//!
//! Claude Code 2.1.300 and later write two record kinds to the transcript. `api-request-shape`
//! holds the exact `system` blocks and `tools` array, keyed by `shapeHash`. `api-request` holds
//! one request: its `shapeHash` and its `cacheControl` breakpoints. The format is not documented,
//! so a record of either kind that lacks a field this report needs is an error that names the
//! line, never a zero.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde_json::Value;

/// How many tool schemas the report lists, largest first.
const TOP_TOOLS: usize = 10;

#[derive(Debug, clap::Args)]
pub(crate) struct PrefillArgs {
    /// The session transcript (`.jsonl`). Default: the newest transcript of the current folder
    /// under `$CLAUDE_CONFIG_DIR/projects`.
    transcript: Option<PathBuf>,
}

/// The size of one request shape: the `system` blocks and each tool schema, in characters of
/// their JSON text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shape {
    system_chars: usize,
    tools: Vec<(String, usize)>,
}

/// What the request records of one transcript hold.
#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    shapes: BTreeMap<String, Shape>,
    requests: usize,
    /// Requests whose shape differs from the request before. Each one is a full cache miss.
    shape_changes: usize,
    last_shape: Option<String>,
    last_cache_control: Vec<String>,
}

/// Run `llmenv prefill`.
///
/// # Errors
/// The transcript cannot be found or read, it holds no request record, or a request record lacks
/// a field.
pub(crate) fn run_prefill(args: &PrefillArgs) -> anyhow::Result<()> {
    let path = match &args.transcript {
        Some(path) => path.clone(),
        None => newest_transcript()?,
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read the transcript {}", path.display()))?;
    let report = read_report(&text).with_context(|| format!("in {}", path.display()))?;
    println!("{}", render(&report, &path)?);
    Ok(())
}

/// Read the request records of a transcript.
fn read_report(text: &str) -> anyhow::Result<Report> {
    let mut report = Report::default();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        // Other record kinds, and a line that is not JSON, are not request records.
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match record["type"].as_str() {
            Some("api-request-shape") => {
                let (hash, shape) = read_shape(&record).with_context(|| line_context(number))?;
                report.shapes.insert(hash, shape);
            }
            Some("api-request") => {
                read_request(&record, &mut report).with_context(|| line_context(number))?
            }
            _ => {}
        }
    }
    anyhow::ensure!(
        report.requests > 0,
        "the transcript holds no api-request record. Fix: use a transcript from Claude Code 2.1.300 \
         or later."
    );
    Ok(report)
}

fn line_context(number: usize) -> String {
    format!("line {number}: the request record format changed, so llmenv cannot read it")
}

fn read_shape(record: &Value) -> anyhow::Result<(String, Shape)> {
    let hash = text_field(record, "shapeHash")?;
    let shape = &record["shape"];
    let system = shape
        .get("system")
        .ok_or_else(|| anyhow::anyhow!("api-request-shape has no shape.system"))?;
    let tools = shape["tools"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("api-request-shape has no shape.tools list"))?;
    let tools = tools
        .iter()
        .map(|tool| {
            let name = text_field(tool, "name").context("a tool in shape.tools")?;
            Ok((name, tool.to_string().chars().count()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((
        hash,
        Shape {
            system_chars: system.to_string().chars().count(),
            tools,
        },
    ))
}

fn read_request(record: &Value, report: &mut Report) -> anyhow::Result<()> {
    let hash = text_field(record, "shapeHash")?;
    let breakpoints = record["cacheControl"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("api-request has no cacheControl list"))?;
    report.last_cache_control = breakpoints
        .iter()
        .map(|b| breakpoint_path(&b["path"]))
        .collect::<anyhow::Result<_>>()?;
    if report.last_shape.as_ref().is_some_and(|last| *last != hash) {
        report.shape_changes += 1;
    }
    report.requests += 1;
    report.last_shape = Some(hash);
    Ok(())
}

/// A breakpoint path is a list of keys and indexes into the request body, such as
/// `["messages", 1, "content", 0]`. The report shows it as `messages[1].content[0]`.
fn breakpoint_path(path: &Value) -> anyhow::Result<String> {
    let parts = path
        .as_array()
        .filter(|parts| !parts.is_empty())
        .ok_or_else(|| anyhow::anyhow!("a cacheControl entry has no path list"))?;
    let mut out = String::new();
    for part in parts {
        if let Some(index) = part.as_u64() {
            out.push_str(&format!("[{index}]"));
        } else if let Some(key) = part.as_str().filter(|k| !k.is_empty()) {
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(&crate::util::strip_unsafe_chars(key));
        } else {
            anyhow::bail!("a cacheControl path holds {part}, not a key or an index");
        }
    }
    Ok(out)
}

fn text_field(record: &Value, key: &str) -> anyhow::Result<String> {
    record[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("the record has no {key} text"))
}

/// The report text. The sizes are of the last request's shape, the one the next request reuses.
fn render(report: &Report, path: &Path) -> anyhow::Result<String> {
    let last = report.last_shape.as_deref().unwrap_or_default();
    let shape = report.shapes.get(last).ok_or_else(|| {
        anyhow::anyhow!(
            "the last api-request names shape {last}, but no api-request-shape record holds it"
        )
    })?;
    let tool_chars: usize = shape.tools.iter().map(|(_, chars)| chars).sum();
    let mut tools = shape.tools.clone();
    tools.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut out = format!(
        "{}\n{} requests, {} request shapes, {} shape changes (each one is a full cache miss)\n\
         Last shape: system {} chars, tools {} chars in {} schemas\n",
        path.display(),
        report.requests,
        report.shapes.len(),
        report.shape_changes,
        shape.system_chars,
        tool_chars,
        shape.tools.len()
    );
    out.push_str(&format!("Largest tool schemas (top {TOP_TOOLS}):\n"));
    for (name, chars) in tools.iter().take(TOP_TOOLS) {
        out.push_str(&format!(
            "  {chars:>8}  {}\n",
            crate::util::strip_unsafe_chars(name)
        ));
    }
    out.push_str(&format!(
        "Cache breakpoints of the last request: {}",
        if report.last_cache_control.is_empty() {
            "none".to_string()
        } else {
            report.last_cache_control.join(", ")
        }
    ));
    Ok(out)
}

/// The newest transcript of the current folder. Claude Code keeps it under
/// `$CLAUDE_CONFIG_DIR/projects/<folder>`, where `<folder>` is the working folder with each `/`
/// written as `-`.
fn newest_transcript() -> anyhow::Result<PathBuf> {
    let fix = "Fix: pass the transcript path, such as `llmenv prefill <file>.jsonl`.";
    let config_dir = std::env::var("CLAUDE_CONFIG_DIR")
        .map_err(|e| anyhow::anyhow!("cannot read CLAUDE_CONFIG_DIR ({e}). {fix}"))?;
    let cwd = std::env::current_dir().context("cannot read the current folder")?;
    let folder = cwd.to_string_lossy().replace('/', "-");
    let dir = Path::new(&config_dir).join("projects").join(&folder);
    let newest = std::fs::read_dir(&dir)
        .with_context(|| format!("cannot list {}. {fix}", dir.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(modified, _)| *modified);
    newest
        .map(|(_, path)| path)
        .ok_or_else(|| anyhow::anyhow!("{} holds no transcript. {fix}", dir.display()))
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
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

    #[test]
    fn a_transcript_gives_sizes_top_tools_and_shape_changes() {
        let text = [
            r#"{"type":"user","message":"hi"}"#.to_string(),
            shape(
                "a",
                &[("Read", "x"), ("icm_memory_recall", "a long description")],
            ),
            request("a"),
            request("a"),
            shape("b", &[("Read", "x")]),
            request("b"),
            "not json".to_string(),
        ]
        .join("\n");
        let report = read_report(&text).unwrap();
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
            out.contains(
                "Cache breakpoints of the last request: system[2], messages[3].content[0]"
            ),
            "{out}"
        );
    }

    #[test]
    fn a_changed_record_format_names_the_line_and_never_reports_zero() {
        let missing_hash = r#"{"type":"api-request","cacheControl":[]}"#;
        let err = format!("{:#}", read_report(missing_hash).unwrap_err());
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("shapeHash"), "{err}");
        let no_tools = r#"{"type":"api-request-shape","shapeHash":"a","shape":{"system":[]}}"#;
        let err = format!("{:#}", read_report(no_tools).unwrap_err());
        assert!(err.contains("shape.tools"), "{err}");
        let err = format!("{:#}", read_report(r#"{"type":"user"}"#).unwrap_err());
        assert!(err.contains("Claude Code 2.1.300"), "{err}");
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
        ] {
            let err = breakpoint_path(&bad).unwrap_err().to_string();
            assert!(err.contains("cacheControl"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_request_whose_shape_is_missing_is_an_error() {
        let report = read_report(&request("zz")).unwrap();
        let err = render(&report, Path::new("t")).unwrap_err().to_string();
        assert!(
            err.contains("no api-request-shape record holds it"),
            "{err}"
        );
    }

    proptest! {
        /// The change count is the number of neighbouring requests with different shapes, and
        /// the tool sizes add up to the size of each schema's JSON text.
        #[test]
        fn shape_changes_count_neighbour_differences(hashes in proptest::collection::vec("[ab]", 1..12)) {
            let mut lines = vec![shape("a", &[("T", "d")]), shape("b", &[])];
            lines.extend(hashes.iter().map(|h| request(h)));
            let report = read_report(&lines.join("\n")).unwrap();
            let changes = hashes.windows(2).filter(|w| w[0] != w[1]).count();
            prop_assert_eq!(report.shape_changes, changes);
            prop_assert_eq!(report.requests, hashes.len());
            let tool = serde_json::json!({"name": "T", "description": "d", "input_schema": {}});
            prop_assert_eq!(report.shapes["a"].tools[0].1, tool.to_string().chars().count());
        }
    }
}
