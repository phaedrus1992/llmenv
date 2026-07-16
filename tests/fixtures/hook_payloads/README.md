# Hook payload fixtures

Real (snake_case) Claude Code `PreToolUse`/`PostToolUse` hook wire-format
payloads, one per tool: `read.json`, `read_partial.json`, `write.json`,
`edit.json`, `multi_edit.json`, `bash.json`, `web_search.json`.

**These fixtures must be sourced from real Claude Code hook invocations, not
hand-typed guesses.** That is the whole point of #839: a prior bug (#724)
shipped because unit tests used a hand-typed camelCase payload
(`oldString`/`newString`) instead of Claude Code's real snake_case wire shape
(`old_string`/`new_string`), so the tests passed against fake data while the
real integration silently no-op'd. The wire schema is documented in
`docs/reference/claude-code/hooks.md` (common fields: `session_id`,
`transcript_path`, `cwd`, `hook_event_name`, `tool_name`, `tool_input`); the
`tool_input` shapes here are cross-checked against the already-correct
snake_case usage in `src/hook_run/read_once.rs`.

**Caveat:** the fixtures currently committed here were hand-constructed by
cross-referencing `src/hook_run/read_once.rs` and
`docs/reference/claude-code/hooks.md`, not captured from live Claude Code
hook traffic — they do not yet meet the "real invocation" bar stated above.
Replace them with genuine captured payloads as they become available.

Load fixtures via `crate::test_fixtures::load_hook_payload("edit.json")`
(parsed `serde_json::Value`) or `load_hook_payload_raw("edit.json")` (raw
string, e.g. for feeding a stdin-shaped parser under test).
