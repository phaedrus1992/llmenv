#!/usr/bin/env bash
# session-log.sh — audit trail for Claude Code sessions
#
# Fires on: PostToolUse (after every tool call)
# Output:   ${CLAUDE_CONFIG_DIR}/session-logs/YYYY-MM-DD.jsonl
#
# Each line in the log is a JSON object with:
#   timestamp   ISO-8601 timestamp
#   session_id  Claude Code session identifier (from CLAUDE_SESSION_ID env var)
#   tool        tool name (Bash, Read, Edit, Write, …)
#   detail      most useful parameter for that tool type (command, path, etc.)
#   status      exit_code or error message from the tool response
#
# WHY: Long agentic runs can make many tool calls over hours. This log lets you
# review exactly what happened, in order, without relying on Claude's summary.
#
# HOW IT INTEGRATES:
#   bundle.yaml registers this as a PostToolUse hook. llmenv injects the
#   handler into Claude Code's settings.json hooks array at materialize time.
#   Claude Code executes it after every tool call, passing the event payload
#   as JSON on stdin.
#
# DESIGN NOTES:
#   - All processing is done in Python (launched inline via heredoc) to avoid
#     shell quoting issues with arbitrary tool input/output strings.
#   - The script always exits 0 — logging must never block Claude.
#   - The log directory is created if it doesn't exist.
#   - One file per day keeps individual files manageable.

# shellcheck disable=SC2016  # heredoc body is Python, not shell

python3 - "$@" <<'PYEOF'
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

# --------------------------------------------------------------------------
# Read the event payload from stdin.
# Claude Code sends the full PostToolUse event as a JSON object.
# --------------------------------------------------------------------------
try:
    event = json.load(sys.stdin)
except (json.JSONDecodeError, EOFError):
    sys.exit(0)  # malformed input — skip silently, never block

tool_name = event.get("tool_name", "unknown")
tool_input = event.get("tool_input", {})
tool_response = event.get("tool_response", {})

# --------------------------------------------------------------------------
# Extract the most useful detail per tool type.
# We log only the key parameter — full inputs can be huge and redundant.
# --------------------------------------------------------------------------
if tool_name == "Bash":
    detail = tool_input.get("command", "")[:200]  # truncate long commands
elif tool_name in ("Read", "Write", "Edit"):
    detail = tool_input.get("file_path", tool_input.get("path", ""))
elif tool_name == "WebFetch":
    detail = tool_input.get("url", "")
elif tool_name == "WebSearch":
    detail = tool_input.get("query", "")
elif tool_name == "Agent":
    detail = tool_input.get("description", "")[:100]
else:
    # For any other tool, log the first value from the input dict.
    detail = str(next(iter(tool_input.values()), "")) [:100]

# --------------------------------------------------------------------------
# Extract tool response status (exit codes for Bash, errors for others).
# --------------------------------------------------------------------------
if isinstance(tool_response, dict):
    status = tool_response.get("exit_code", tool_response.get("error", "ok"))
elif isinstance(tool_response, str):
    # Some tools return a plain string on success.
    status = "ok"
else:
    status = "ok"

# --------------------------------------------------------------------------
# Build and append the log entry.
# --------------------------------------------------------------------------
entry = {
    "timestamp": datetime.now(timezone.utc).isoformat(),
    "session_id": os.environ.get("CLAUDE_SESSION_ID", "unknown"),
    "tool": tool_name,
    "detail": detail,
    "status": str(status),
}

config_dir = os.environ.get("CLAUDE_CONFIG_DIR", os.path.expanduser("~/.claude"))
log_dir = Path(config_dir) / "session-logs"
log_dir.mkdir(parents=True, exist_ok=True)

date_str = datetime.now().strftime("%Y-%m-%d")
log_file = log_dir / f"{date_str}.jsonl"

with open(log_file, "a", encoding="utf-8") as f:
    f.write(json.dumps(entry) + "\n")

sys.exit(0)  # always 0 — logging must never block Claude
PYEOF
