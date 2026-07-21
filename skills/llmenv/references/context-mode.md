# Context Mode

Token-efficiency tooling for large tool outputs — runs analysis in a sandbox
and returns only the derived answer, keeping raw bytes out of your context.

- `ctx_batch_execute` — run several shell commands in parallel, each
  auto-indexed; pass `queries` to get matching sections back in the same round
  trip.
- `ctx_search` — follow-up questions against anything already indexed
  (including auto-captured session memory) — batch multiple questions in one
  call.
- `ctx_execute` / `ctx_execute_file` — derive an answer from data you've
  already gathered (filter, count, aggregate) without pulling the raw data into
  your conversation.

Reach for these before reading a large file or command output directly
whenever you only need a derived answer, not the raw content.
