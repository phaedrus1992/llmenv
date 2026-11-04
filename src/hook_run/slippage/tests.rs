use super::*;

mod access;
mod reads_and_metrics;
mod transcript;

fn cfg(enabled: bool, self_critique: bool) -> SlippageControl {
    SlippageControl {
        enabled,
        self_critique,
        ..Default::default()
    }
}

fn on() -> SlippageControl {
    SlippageControl {
        enabled: true,
        read_before_edit: true,
        ..Default::default()
    }
}

fn payload(tool: &str, path: &std::path::Path) -> serde_json::Value {
    serde_json::json!({
        "tool_name": tool,
        "tool_input": { "file_path": path.to_str().unwrap() },
    })
}

// #3354: a Monitor-started `cat` is a read, the same as a Bash `cat`.
fn scan_cfg(answer: bool, explain: bool) -> SlippageControl {
    SlippageControl {
        enabled: true,
        answer_before_act: answer,
        explain_before_act: explain,
        ..Default::default()
    }
}

fn transcript_with(lines: &[serde_json::Value]) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(file.path(), body).unwrap();
    file
}

fn user_line(text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
    })
}

fn assistant_line(text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant",
        "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] },
    })
}

fn bash_payload(file: &tempfile::NamedTempFile, command: &str) -> serde_json::Value {
    serde_json::json!({
        "tool_name": "Bash",
        "tool_input": { "command": command },
        "transcript_path": file.path().to_str().unwrap(),
    })
}
