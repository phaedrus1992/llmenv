# Memory (ICM)

llmenv's memory backend. Use the `icm_*` MCP tools directly (never the `icm`
CLI — see this project's own instructions on why, if the host repo has any).
Typical flow:

- `icm_wake_up` at session start to recall relevant context automatically
  (llmenv already injects this via its `session_start` hook — you usually
  don't need to call it yourself).
- `icm_memory_recall` for a targeted query mid-session.
- `icm_memory_store` to persist something worth remembering across sessions —
  a decision, a gotcha, a solved problem.

Be aggressive about storing: any nontrivial code change, design decision, or
research finding is worth an `icm_memory_store` call, not just session-end
cleanup.
