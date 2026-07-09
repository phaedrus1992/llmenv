#!/usr/bin/env bash
# Run a headless fyi scan -> data/scan.json, then merge into data.json.
# Invoked by the Refresh button (server.py) and by the launchd schedule.
set -euo pipefail
cd "$(dirname "$0")"

# Cross-process lock: concurrent runs (launchd + Refresh button) clobber
# scan.json. mkdir is atomic on macOS; flock doesn't exist here.
LOCKDIR="data/.refresh-lock"
if ! mkdir "$LOCKDIR" 2>/dev/null; then
  echo "refresh already running; exiting" >&2
  exit 0
fi
# Clean up the lock on exit (success or failure). If the script crashes the
# stale lockdir blocks the next run — delete it before starting a new one.
trap 'rmdir "$LOCKDIR" 2>/dev/null || true' EXIT
# In case a previous run died and left the lockdir, remove it if it's stale
# (over 10 min old). find -newer can't test absence-of-change, so use mtime as
# a proxy: if the lockdir is older than the script's own start time minus a
# generous timeout, it's stale.
if [[ -d "$LOCKDIR" ]]; then
  if [[ "$(uname)" == "Darwin" ]]; then
    lock_age=$(($(date +%s) - $(stat -f %m "$LOCKDIR" 2>/dev/null || echo 0)))
  else
    lock_age=$(($(date +%s) - $(stat -c %Y "$LOCKDIR" 2>/dev/null || echo 0)))
  fi
  if [[ ${lock_age:-0} -gt 600 ]]; then
    rmdir "$LOCKDIR" 2>/dev/null || true
    mkdir "$LOCKDIR" 2>/dev/null || { echo "refresh already running; exiting" >&2; exit 0; }
  fi
fi

# launchd hands us a bare PATH; make sure claude + python3 are findable.
export PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:/usr/bin:/bin"
mkdir -p data

llmenv sync
llmenv plugin-sync
llmenv regenerate
eval "$(llmenv export)"

echo "calling refresh prompt:"
# ponytail: --dangerously-skip-permissions because this is an unattended job on
# my own machine that only reads via the (already allow-listed) grid MCP tools
# and writes under ./data. Tighten to --permission-mode acceptEdits + scoped
# --allowedTools if it ever does more than read + write data/.
claude -p "$(cat refresh-prompt.md)" \
  --model sonnet \
  --effort high \
  --dangerously-skip-permissions \
  >data/scan.json 2>data/refresh.log
echo "refresh prompt complete"

# merge.py refuses to overwrite data.json if the scan came back empty, so a
# failed scan leaves yesterday's living list intact rather than blanking it.
# Pipe through refresh.log so a merge failure (or the regex-fallback warning)
# is captured next to the scan output instead of vanishing; pipefail propagates
# a non-zero merge exit so the job is marked failed.
python3 merge.py data/scan.json data/data.json 2>&1 | tee -a data/refresh.log
