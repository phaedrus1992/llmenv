//! `llmenv prefill`: the real prompt prefill of a Claude Code session, read from the request
//! records of its transcript (#3102).
//!
//! Claude Code 2.1.300 and later write two record kinds to the transcript. `api-request-shape`
//! holds the exact `system` blocks and `tools` array, keyed by `shapeHash`. `api-request` holds
//! one request: its `shapeHash` and its `cacheControl` breakpoints. The format is not documented,
//! so a record of either kind that lacks a field this report needs is an error that names the
//! line, never a zero.

use std::collections::BTreeMap;
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde_json::Value;

use crate::util::strip_unsafe_chars;

#[cfg(test)]
mod tests;

/// How many tool schemas the report lists, largest first.
const TOP_TOOLS: usize = 10;

/// The text that starts the `type` of both request record kinds. A line that holds it but is not
/// JSON is a damaged request record, not a record of another kind.
const REQUEST_TYPE_MARK: &str = "\"api-request";

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
    /// A hash of the whole shape text, so a second record with the same `shapeHash` but other
    /// content is found.
    content: u64,
}

/// What the request records of one transcript hold.
#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    shapes: BTreeMap<String, Shape>,
    /// Each shape that a request names, with the line of the first such request.
    used_shapes: BTreeMap<String, usize>,
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
    let file = std::fs::File::open(&path)
        .with_context(|| format!("cannot open the transcript {}", path.display()))?;
    let report = read_report(std::io::BufReader::new(file))
        .with_context(|| format!("in {}", path.display()))?;
    println!("{}", render(&report, &path)?);
    Ok(())
}

/// Read the request records of a transcript, one line at a time, because a transcript can be
/// hundreds of megabytes.
fn read_report(reader: impl std::io::BufRead) -> anyhow::Result<Report> {
    let mut report = Report::default();
    for (index, line) in reader.lines().enumerate() {
        let number = index.saturating_add(1);
        let line = line.with_context(|| format!("line {number}: cannot read the line"))?;
        read_line(&line, number, &mut report)?;
    }
    anyhow::ensure!(
        report.requests > 0,
        "the transcript holds no api-request record. Fix: use a transcript from Claude Code 2.1.300 \
         or later."
    );
    if let Some((hash, line)) = report
        .used_shapes
        .iter()
        .find(|(hash, _)| !report.shapes.contains_key(*hash))
    {
        anyhow::bail!(
            "line {line}: the api-request names shape {}, but no api-request-shape record holds it",
            strip_unsafe_chars(hash)
        );
    }
    Ok(report)
}

fn read_line(line: &str, number: usize, report: &mut Report) -> anyhow::Result<()> {
    let record = match serde_json::from_str::<Value>(line) {
        Ok(record) => record,
        Err(e) if line.contains(REQUEST_TYPE_MARK) => anyhow::bail!(
            "line {number}: a request record is not complete JSON ({e}). Fix: check that the \
             transcript is not cut short."
        ),
        // Other record kinds, and a line that is not JSON, are not request records.
        Err(_) => return Ok(()),
    };
    match record["type"].as_str() {
        Some(kind @ "api-request-shape") => {
            read_shape(&record, report).with_context(|| line_context(number, kind))
        }
        Some(kind @ "api-request") => {
            read_request(&record, number, report).with_context(|| line_context(number, kind))
        }
        _ => Ok(()),
    }
}

fn line_context(number: usize, kind: &str) -> String {
    format!(
        "line {number}: cannot read the {kind} record. Fix: check that the line is complete. A \
         newer Claude Code can also write a format that llmenv does not know"
    )
}

fn read_shape(record: &Value, report: &mut Report) -> anyhow::Result<()> {
    let hash = text_field(record, "shapeHash")?;
    let shape = &record["shape"];
    let system = shape["system"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("api-request-shape has no shape.system list"))?;
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
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    shape.to_string().hash(&mut hasher);
    let new = Shape {
        system_chars: Value::Array(system.clone()).to_string().chars().count(),
        tools,
        content: hasher.finish(),
    };
    if let Some(old) = report.shapes.get(&hash) {
        anyhow::ensure!(
            *old == new,
            "shape {} has two records with different content",
            strip_unsafe_chars(&hash)
        );
    }
    report.shapes.insert(hash, new);
    Ok(())
}

fn read_request(record: &Value, number: usize, report: &mut Report) -> anyhow::Result<()> {
    let hash = text_field(record, "shapeHash")?;
    let breakpoints = record["cacheControl"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("api-request has no cacheControl list"))?;
    report.last_cache_control = breakpoints
        .iter()
        .map(|b| breakpoint_path(&b["path"]))
        .collect::<anyhow::Result<_>>()?;
    if report.last_shape.as_ref().is_some_and(|last| *last != hash) {
        report.shape_changes = report.shape_changes.saturating_add(1);
    }
    report.requests = report.requests.saturating_add(1);
    report.used_shapes.entry(hash.clone()).or_insert(number);
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
            out.push_str(&strip_unsafe_chars(key));
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
            "the last api-request names shape {}, but no api-request-shape record holds it",
            strip_unsafe_chars(last)
        )
    })?;
    let tool_chars: usize = shape.tools.iter().map(|(_, chars)| chars).sum();
    let mut tools = shape.tools.clone();
    tools.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut out = format!(
        "{}\n{} requests, {} request shapes, {} shape changes (each one is a full cache miss)\n\
         Last shape: system {} chars, tools {} chars in {} schemas\n",
        strip_unsafe_chars(&path.display().to_string()),
        report.requests,
        report.used_shapes.len(),
        report.shape_changes,
        shape.system_chars,
        tool_chars,
        shape.tools.len()
    );
    out.push_str(&format!("Largest tool schemas (top {TOP_TOOLS}):\n"));
    for (name, chars) in tools.iter().take(TOP_TOOLS) {
        out.push_str(&format!("  {chars:>8}  {}\n", strip_unsafe_chars(name)));
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

/// The folder name under which Claude Code keeps the transcripts of `cwd`: each character that is
/// not an ASCII letter or digit is written as `-`.
fn project_folder_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The newest transcript of the current folder, under `$CLAUDE_CONFIG_DIR/projects/<folder>`.
fn newest_transcript() -> anyhow::Result<PathBuf> {
    let fix = "Fix: pass the transcript path, such as `llmenv prefill <file>.jsonl`.";
    let config_dir = crate::env_var::utf8_var("CLAUDE_CONFIG_DIR")?
        .ok_or_else(|| anyhow::anyhow!("CLAUDE_CONFIG_DIR is not set. {fix}"))?;
    let cwd = std::env::current_dir().context("cannot read the current folder")?;
    let folder = project_folder_name(&cwd.to_string_lossy());
    let dir = Path::new(&config_dir)
        .join(crate::materialize::inherit::PROJECTS_DIR)
        .join(&folder);
    newest_jsonl_in(&dir, fix)
}

/// The `.jsonl` file in `dir` with the newest change time. `fix` ends each error.
fn newest_jsonl_in(dir: &Path, fix: &str) -> anyhow::Result<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("cannot list {}. {fix}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("cannot list {}. {fix}", dir.display()))?;
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .with_context(|| format!("cannot read the change time of {}. {fix}", path.display()))?;
        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, path));
        }
    }
    newest
        .map(|(_, path)| path)
        .ok_or_else(|| anyhow::anyhow!("{} holds no transcript. {fix}", dir.display()))
}
