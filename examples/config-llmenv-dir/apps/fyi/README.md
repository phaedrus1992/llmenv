# fyi (living web todo)

A minimal localhost page that turns the one-shot `fyi` briefing into a
living document: check items off, and have it re-scan through the day and re-rank
while preserving what you've touched.

See `SPEC.md` for the design. No dependencies — stdlib Python + one HTML file.

## Run

```bash
cd ~/git/my-llmenv/apps/fyi
python3 server.py            # serves http://127.0.0.1:8787  (set MT_PORT to change)
```

Open <http://127.0.0.1:8787>. Hit **Refresh** to run the first scan (takes a
minute — it's a full Claude scan of GitHub/Linear/Slack/Pylon). Check boxes off
as you work; toggles persist immediately.

## How a scan works

`refresh.sh` runs `claude -p` with `refresh-prompt.md`, which reuses the
`fyi` skill to gather your open work and emits a raw JSON snapshot to
`data/scan.json`. `merge.py` then folds it into `data/data.json` by stable id:

- your check-offs persist across scans
- work that cleared (PR merged, issue Done) auto-completes + strikes through
- brand-new items get a `new` badge
- a new day starts fresh (done items pruned, checks reset)

A failed/empty scan leaves the existing list intact (merge.py refuses to write
an empty result).

## Schedule (every 2h, workday)

```bash
cp com.github.phaedrus1992.fyi.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.github.phaedrus1992.fyi.plist
# stop: launchctl unload ~/Library/LaunchAgents/com.github.phaedrus1992.fyi.plist
```

The page polls every 30s, so scheduled scans appear without a reload.

Cron alternative (if you don't want launchd):

```cron
0 8-18/2 * * 1-5  /bin/bash /Users/phaedrus/git/my-llmenv/apps/fyi/refresh.sh
```

## Files

| File | Role |
| ------ | ------ |
| `server.py` | localhost server: UI + JSON API + check persistence |
| `merge.py` | deterministic smart-merge (`python3 merge.py --selftest` to verify) |
| `refresh.sh` | headless scan -> `data/scan.json` -> merge |
| `refresh-prompt.md` | the headless scan prompt |
| `index.html` | the UI |
| `com.github.phaedrus1992.fyi.plist` | launchd schedule |
| `data/` | runtime state (gitignored) |

## Notes / limits (first release)

- localhost + single user, no auth.
- the scan runs `claude -p --dangerously-skip-permissions` — unattended, on your
  own machine, read-only company tools + writes only under `data/`. See the
  comment in `refresh.sh` to tighten.
- the scan owns the list; you can't hand-add items in the UI yet.
