# Task Tracker

Durable, cross-session task state — use it instead of relying on in-session
TODOs.

## Sessions are mandatory

Every task belongs to a session. Before your first `task add`:

```
llmenv task session start "<name>" [--description "<text>"]
```

Pass `--description` whenever you have enough context to make one useful —
a dev-sprint issue number, a brainstorming topic, whatever helps a human
skimming `task session ls` tell your session apart from another one in the
same project. `--description` is separate from `<name>`; keep the name short.

**If one or more sessions are already open for this project**, `session
start` errors and lists them (id, name, description, idle time). Pick one:

- `--resume <id>` — this is your session from before (e.g. after a context
  compaction wiped your memory of it). Adopts it, no new id.
- `--replace` — the listed session(s) are stale/abandoned. Untags their
  incomplete tasks (noting what happened) and starts fresh.
- `--new` — you are deliberately running alongside another active session in
  this same project (rare — two windows genuinely working in parallel).

## Adding and working tasks

```
llmenv task add "<title>"                # auto-tags to your one open session
llmenv task add "<title>" --session <id> # explicit, if you have 2+ open
llmenv task start <slug>                 # claim it
llmenv task done <slug>                  # finish it
llmenv task note <slug> "<text>"         # record progress before compaction
llmenv task wait <slug> "<reason>"       # blocked on external/human input
```

`task add` errors if zero or 2+ sessions are open for this project and you
didn't pass `--session` — it will not silently create one for you.

## Surviving a context compaction

If you no longer remember your session id, run `llmenv task session ls`. In
the common case (one agent, one project) there's exactly one match — use it.
If there are two or more matches for this project, that means real concurrency
is in play and you need to have durably noted your specific session id
somewhere in your own context before the compaction — there's no engine-level
mechanism that preserves it for you across a compaction.

## Sub-tasks and dependencies

```
llmenv task add "<title>" --parent <slug>   # sub-task
llmenv task block <slug> --on <other-slug>  # ordering dependency
```

## Closing out

```
llmenv task session finish [<id>]   # auto-resolves if exactly one is open
llmenv task session show [<id>]
```
