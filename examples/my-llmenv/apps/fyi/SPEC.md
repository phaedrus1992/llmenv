# fyi — living web todo

Turn the `fyi` skill's one-shot briefing into a living, interactive
localhost page: check items off, and have it re-scan throughout the day and
re-rank, while preserving what you've touched.

## Why this shape

The scan needs Claude + the grid MCP tools (Linear/Slack/GitHub/Pylon) + `gh`.
A web server has none of that. So the work splits:

- **LLM part** (`refresh.sh` -> `claude -p`): produces a *raw snapshot* of current
  open work as JSON. No check state, no merge — just "what's open right now".
- **Deterministic part** (`merge.py`): merges that snapshot into the persisted
  living list by stable ID. The LLM never sees or clobbers your check-offs.
- **Web part** (`server.py` + `index.html`): renders the living list, writes check
  toggles back, triggers refreshes, polls for scheduled updates.

The `fyi` SKILL.md stays untouched. All JSON/merge/web logic lives here.

## Layout

```
apps/fyi/
  SPEC.md            this file
  README.md          run + schedule instructions
  server.py          stdlib http.server: UI + JSON API (~90 lines)
  merge.py           smart-merge scan.json + data.json -> data.json (~70 lines)
  refresh.sh         claude -p <prompt> -> data/scan.json ; then merge.py
  refresh-prompt.md  headless prompt: gather fyi sources, emit scan schema
  index.html         single-file vanilla JS/CSS UI
  com.github.phaedrus1992.fyi.plist  launchd agent template (every 2h, workday)
  data/              gitignored runtime state
    data.json        the living list
    scan.json        latest raw snapshot (transient)
    refresh.log      last headless run output
```

`data/` is gitignored (runtime state, not config).

## Data model — `data/data.json`

```json
{
  "date": "2026-06-22",
  "lastScan": "2026-06-22T13:40:00Z",
  "topFocus": { "text": "...", "id": "pr:llmenv#1423" },
  "items": [
    {
      "id": "linear:ABC-1473",
      "tier": "urgent",
      "title": "Change-management toggle for 2.2.0",
      "note": "current cycle; customer ask DEF-1392",
      "refs": [
        { "label": "ABC-1473", "url": "https://linear.app/phaedrus1992/issue/ABC-1473" }
      ],
      "checked": false,
      "status": "open",
      "manual": false,
      "firstSeen": "2026-06-22T13:40:00Z"
    }
  ]
}
```

- `id` — canonical stable ref. Rule, in priority order:
  `pr:<repo>#<n>` | `linear:<KEY>` | `slug:<kebab-title>` (ref-less items).
- `tier` — `urgent` | `in_progress` | `pending`.
- `status` — `open` | `new` (this scan, unseen before) | `done` (dropped out of scan;
  auto-completed).
- `checked` — render state of the checkbox. Persists across scans.
- `manual` — true if the user toggled it (vs auto-completed). Purely informational.

## Smart-merge rules — `merge.py`

Input: fresh `scan.json` (list of `{id,tier,title,note,refs}`), prior `data.json`.
Output: new `data.json`.

For each scan item, keyed by `id`:
- **not in prior data** -> `status:"new"`, `checked:false`, `firstSeen:now`.
- **in prior data** -> keep `checked`, `manual`, `firstSeen`; refresh `tier/title/note/refs`
  from scan; `status:"open"`.

For each prior item **absent from the scan** (work that cleared):
- it dropped out (PR merged, issue Done, etc.) -> `status:"done"`, `checked:true`.
  Kept visible for the rest of the day so you see what got cleared. Pruned on the
  first scan of a new date (date rollover wipes `done` items).

Ordering: tier (`urgent` > `in_progress` > `pending`), then scan order within tier;
`done` items sink to the bottom of their tier.

Determinism: merge is pure Python keyed on `id`. No LLM in the merge path, so a
re-scan can never silently uncheck your work.

## Web API — `server.py`

stdlib `http.server`, single process, binds `127.0.0.1:8787` (configurable via
`MT_PORT`).

- `GET  /`            -> `index.html`
- `GET  /api/data`    -> `data/data.json` (200; `{}` if no scan yet)
- `POST /api/toggle`  -> body `{id, checked}`; set `checked` + `manual:true`, write data.json
- `POST /api/refresh` -> spawn `refresh.sh` detached; return `{started:true}` (409 if already running)
- `GET  /api/status`  -> `{lastScan, refreshing, date}`

The UI polls `/api/data` + `/api/status` every ~30s, so launchd-driven scans show
up without a manual reload.

## UI — `index.html`

Single file, vanilla JS + CSS, no build, no deps. Renders top-focus banner, then the
three tiers as checkable lists. Each item: checkbox, title, inline ref links (open in
browser), `new`/`done` badges, struck-through when done. Header shows last-scan time,
a Refresh button (-> `/api/refresh`, shows spinner while `refreshing`), and date.
Checking a box POSTs immediately and optimistically updates.

## Headless scan — `refresh.sh` + `refresh-prompt.md`

`refresh.sh`:
```bash
set -euo pipefail
cd "$(dirname "$0")"
claude -p "$(cat refresh-prompt.md)" \
  --dangerously-skip-permissions \
  > data/scan.json 2> data/refresh.log
python3 merge.py data/scan.json data/data.json
```

`--dangerously-skip-permissions`: unattended job on the user's own machine; touches
only read-only company MCP tools (already allow-listed in the work bundle) and writes
under `data/`. Chosen for robustness — never prompts, never hangs. Tradeoff noted in
the script. # ponytail: skip-perms for the unattended run; tighten to acceptEdits + scoped allowedTools if it ever does more than read + write data/

`refresh-prompt.md`: instructs Claude to run the fyi gathering (reusing the
skill's sources + `gh-open-work.sh`), then emit **only** a JSON array of
`{id,tier,title,note,refs}` to stdout per the schema above — no markdown, no prose.
The id-derivation rule is spelled out so ids stay stable across runs.

## Scheduling — launchd

User agent `com.github.phaedrus1992.fyi.plist`, runs `refresh.sh` every 2h during the
workday (`StartCalendarInterval` for 08–18 on weekdays). Load:
`launchctl load ~/Library/LaunchAgents/com.github.phaedrus1992.fyi.plist`. README documents
load/unload and a cron one-liner alternative.

## Out of scope (for now)

- Auth / multi-user (localhost, single user).
- Editing/adding items by hand in the UI (the scan owns the list).
- History/trends across days (each day is its own `data.json`; date rollover resets).
```
