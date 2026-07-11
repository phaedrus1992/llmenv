# Issue #231 — in-engine task tracker: CLI, injected context, ordering hooks

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/231
- **Milestone:** Large Projects
- **Type:** Feature (large; CLI + store + context injection + hooks)
- **Difficulty:** Hard. Four cooperating parts — ship in the phase order
  below; each phase is independently useful.

## Problem

Agents abandon in-progress work, start new efforts mid-stream, and lose
"what am I working on" across `/clear`//compact//new sessions. In-session
TODOs are ephemeral. llmenv already owns durable state (#175) and per-scope
context/hook injection, so it hosts a file-based task tracker the agent is
steered to use and mechanically reminded to finish.

Read the full issue — its Scope sections 1–4 are the spec. This doc adds
decisions for the issue's open questions and code anchors.

## Decisions (resolving the issue's open questions)

1. **Task identity: kebab-case slug** derived from the title (first ~6
   words), uniquified with `-2`, `-3` … on collision. One scheme only —
   no parallel numeric ids. Commands accept the slug or an unambiguous
   prefix of it (error listing candidates when ambiguous).
2. **No GitHub sync.** Local-first, per the issue's lean. File a follow-up
   if ever wanted.
3. **Skip detection lives on the `Stop` hook** (end-of-turn, once, cheap);
   **cross-session pickup lives on `SessionStart`** (inject "you have
   these `wip` tasks" context). No per-turn UserPromptSubmit scan — too
   hot. Both advisory-only: hooks steer via injected context, never block
   (issue's hot-path discipline).

## Storage

One JSON file per task: `<LLMENV_STATE_DIR>/tasks/<slug>.json`
(the durable state dir from #175 — find how other consumers resolve it via
`StateConfig` / `src/materialize/state.rs`).

```json
{
  "slug": "fix-login-timeout",
  "title": "Fix login timeout",
  "state": "open",            // open | wip | done
  "parent": null,              // slug of parent task (sub-tasks)
  "blocked_on": [],            // slugs; ordering edges
  "notes": [ {"at": "<rfc3339>", "text": "..."} ],
  "created_at": "<rfc3339>",
  "updated_at": "<rfc3339>"
}
```

Validation on every load: unknown state, dangling `parent`/`blocked_on`
slugs → warn (not fail) and treat edge as absent. Corrupt file → skip with
one-line stderr warning (never crash a hook or `ls` over one bad file).

## Phases

### Phase 1 — store + CLI (`src/cli/`, new `src/task/` module)

`llmenv task` subcommands per the issue: `add <title> [--parent <slug>]`,
`start`, `done`, `ls [--format json]`, `show`, `note <slug> <text>` (also
accept stdin like `yx field`), `block <slug> --on <other>`. Wire into clap
next to the existing `memory` subcommand family (`src/cli/mod.rs:319` area
shows the sub-subcommand pattern).

State-transition rules enforced in the store layer: `start` on `done` →
error; `done` on `open` → allowed (fast-path completion); `start` on a
task whose `blocked_on` contains a non-`done` slug → **warning printed,
transition still allowed** (agent may know better; the hook layer nags).

Unit tests: transitions, slug collision, prefix addressing, ambiguity
error, corrupt-file tolerance, JSON output shape.

### Phase 2 — injected context

A rules fragment steering the agent (add before starting, start to claim,
done to finish, sub-task-vs-finish-first) injected through the same
mechanism the token-efficiency bundle (#218) and ICM recall context use —
find that injection path (`src/icm.rs` context chunk + merge-contributed
rules via `src/merge`) and mirror it. Gate everything behind a feature/
bundle switch (default **off**): follow whichever of
`features.*`-vs-built-in-bundle mechanism #218 actually shipped with —
consistency with that sibling is the requirement.

### Phase 3 — hooks (`src/hook_run/`)

- **SessionStart:** if any `wip` tasks exist, inject "In-progress tasks:
  … Resume or `llmenv task done` them before new work."
- **Stop (skip detection):** if this session ran `task start`/`task add`
  (detectable from task file mtimes/state within session window — keep it
  a heuristic, note the ceiling) and a `wip` task remains, inject the
  redirect message from the issue ("You left task X in progress…").
- **New-project guard:** on `task add` of a *top-level* task (no
  `--parent`) while `wip` tasks exist, the CLI itself prints the
  decision-forcing message (sub-task it or finish first) — CLI-side beats
  transcript heuristics; the Stop hook repeats it if unresolved.
- All handlers: degrade to one-line stderr warning + exit 0 on any
  internal failure (extend `tests/hook_run_failsoft.rs` coverage).

### Phase 4 — docs + changelog

User-facing docs (feature reference + a short workflow example);
CHANGELOG `[Unreleased]` via keepachangelog skill.

## Acceptance criteria

The issue's checklist verbatim, plus:

- [ ] Feature off by default; enabling it is one config switch; disabled =
      byte-identical materialized output and zero hook cost.
- [ ] `task ls --format json` stable enough for hooks to consume (schema
      documented in the code).
- [ ] No new dependencies (serde + existing stack suffice).
- [ ] Clippy/fmt clean; full suite green per phase.

## Out of scope

- GitHub Issues sync; multi-agent locking (single-writer assumption —
  note as `ponytail:` ceiling in the store); task priorities/deadlines;
  TUI.
