---
name: daily-update
description: Use when the user asks for a "standup", "scrum update", "daily update", "status update", "what did I work on", or "what did I do yesterday". Scans llmenv session logs, Linear, Slack, and GitHub for the user's own activity on a given day and produces a short scrum-style update, copied to the clipboard as Slack markdown.
resources:
  - scripts/scan-sessions.mjs
  - scripts/gh-activity.sh
---

# Daily Update

Produce a short, paste-ready scrum/standup update from the user's real activity
across four sources. Default day is **yesterday** (local time); honor any date or
range the user gives. End by copying the update to the clipboard.

This is a reporting task — **read-only everywhere**. Never post to Slack, edit
Linear, or touch GitHub. The only side effect is the clipboard.

## Required tools — no silent fallback

Every source and tool below is **required**, not best-effort. If any tool, MCP, or
script the skill depends on is unavailable or unauthenticated — the slack plugin not
logged in, the grid MCP disconnected, a script missing, Linear unreachable — **STOP
and tell the user to fix or authenticate it.** Do NOT quietly fall back to a degraded
subset and produce a partial report: a standup that silently drops a whole source
reads as "that's all that happened" when it isn't, and Slack in particular carries
work that never shows up in GitHub or Linear. The slack plugin needs an interactive
OAuth login; if its search tools aren't registered, trigger its `authenticate` tool
and wait for the user to complete the flow before continuing.

## Sources (gather all four, in parallel)

1. **llmenv session logs** — what the user actually did at the keyboard.

   ```bash
   "${CLAUDE_CONFIG_DIR}/skills/daily-update/scripts/scan-sessions.mjs" --date <YYYY-MM-DD>
   ```

   Emits a per-project digest: human prompts (noise-filtered) + the intent of any
   compacted sessions. It scans **all** llmenv profile hashes plus user global claude
   -- a single day routinely spans several projects and profiles, so trust the script
   over any single transcript.

2. **GitHub** — what shipped. Scope to the work org to cut noise.

   ```bash
   "${CLAUDE_CONFIG_DIR}/skills/daily-update/scripts/gh-activity.sh" <YYYY-MM-DD> <owner>
   ```

   Returns PRs updated/merged, commits authored, and issues touched. Merged PRs and
   commit subjects are the strongest "done" signal — prefer them over log chatter.

3. **Linear** — issue status movement. List the user's issues and keep the ones
   whose `updated_at` falls on the target day:
   `mcp__grid__linear_list_issues(assignee="<user work email>")`. Use
   `linear_read_issue` / `linear_search_issues` only to clarify a specific ticket.
   A status move is itself the work signal — an issue that went to **In Review** or
   **In Progress** that day belongs in the update even when there's no merged PR,
   commit, session log, or Slack mention for it. Don't require corroboration to
   include a Linear item; missing these is the most common gap. Map state to section:
   `Done`/`Merged` → *Yesterday*, `In Review`/`In Progress` → *Today* (or wherever
   the day's framing puts it).

4. **Slack** — discussions, decisions, handoffs. Two different tools, by intent —
   use both:
   - **The user's OWN messages** → the authenticated **slack plugin** MCP
     (`mcp__plugin_slack_slack__*`), which searches *as the user*. Resolve their
     Slack id first (`slack_search_users` on their work email, or read it straight
     from the `slack_search_public_and_private` tool description, which states the
     logged-in user's id), then:
     `slack_search_public_and_private(query="from:<@USER_ID> on:<YYYY-MM-DD>", sort="timestamp")`.
     **Page through all results** — a busy day exceeds the 20-result cap, so follow
     the cursor until exhausted. This is the *only* tool that finds what the user
     said; its `from:`/`on:` operators work because it runs under the user's identity.
   - **General team chatter about the user's issues** → the **grid** Slack tools
     (`mcp__grid__slack_search_messages`). This is a bot-authenticated, full-text
     content index: it **ignores `from:`/`on:` operators** (so `from:@me` returns
     nothing) and can't be scoped to the user — but it's useful for what *others*
     said about a ticket or PR. Never use it as a substitute for the user's own
     messages; it cannot surface them.
   Long handoff/summary messages the user posted are high-signal; one-word replies
   and DM banter are not.

Run the two scripts and the MCP queries concurrently. Process large script output
with `ctx_execute`/`ctx_execute_file` rather than reading raw logs into context.

## Synthesis

Cross-reference before writing — the sources correct each other:

- **GitHub is ground truth for code.** If a log session guessed a PR's purpose, the
  PR title/number wins. (E.g. a PR titled "leader election" is leader election,
  even if a chat called it something else.)
- **Map work to identifiers, and link them.** Tie each line to its PR (`#1234`),
  Linear key (`ABC-1234` / `DEF-1234`), or Slack thread where one exists. Every
  identifier you mention must be a link, never bare text:
  - Linear keys → `[ABC-1234](https://linear.app/phaedrus1992/issue/ABC-1234)` (any
    team prefix: ABC, DEF, JKL, etc. — same URL shape).
  - GitHub PRs → `[#1234](https://github.com/phaedrus1992/<repo>/pull/1234)`.
  This applies to *every* mention, not just the headline item on a line.
- **Linked follow-ups.** If the day's work created issues (you'll see "create an ABC
  issue…" in the session logs), find each one in Linear and link it by key — don't
  write "filed follow-ups" without saying which. Resolve the actual key via
  `linear_search_issues`; the same prompt often spawns a mistaken duplicate on another
  team that gets cancelled, so prefer the live ABC issue.
- **Mind the timezone and the day boundary.** GitHub/`gh` timestamps are UTC; the
  user's day — and Slack timestamps — are local. A PR merged near midnight local time
  can land on the adjacent UTC day in the `gh` results, so cross-check against the
  Slack local-time activity before attributing it to the target day. And a standup the
  user posted on the *morning* of the target day reports the *previous* day's work, not
  the target day's — diff against it, but don't re-report its contents as the target
  day's. Lean on the user's own Slack messages (local time) to anchor what actually
  happened on the day in question.
- **Collapse duplicates.** The same task usually appears in all four sources; report
  it once.
- **Don't drop a source that only one source knows about.** GitHub PRs are the spine,
  but a Linear issue that moved with no PR, or a decision that lives only in Slack, is
  still real work. Each source can contribute items the others miss — sweep all four,
  don't let the PR list become the whole report.
- **Drop non-work.** Banter, reactions, and off-topic DMs do not belong in a standup.

Before finalizing, sanity-check the draft against each source one more time: every
Linear issue that moved on the day, every merged PR, and every substantive Slack
handoff should map to a bullet or be a deliberate drop — not an oversight. If an
async-standup bot or teammate posted a draft for the same day (often a DM), diff
against it; anything it caught that you didn't is a gap to fold in.

## Output format

Keep it to a basic standup. Default shape:

```text
*Yesterday:*
• <thing done> (<ref>)
• ...

*Today:*
• <next step>

*Blockers:* <blocker, or "none">
```

- Slack-flavored: `*bold*` (single asterisks), `•` bullets. No `#` headings, no tables.
- Links use **markdown** form `[label](url)`, not Slack's `<url|label>` — the user's
  Slack does not render the angle-bracket form.
- No emoji. ASCII only — don't carry over Unicode arrows or symbols from source titles
  (e.g. a PR titled "vcluster↔EC" becomes "vcluster/EC", `->` instead of `→`).
- Short. A handful of bullets per section — the user can ask for the long version.
- **Write it in the user's voice: invoke the `bens-voice` skill** before drafting.
  Plain and factual — a bug fix is a bug fix, not a "critical stability improvement."

## Deliver

Copy the final update to the clipboard (macOS), then show it back in the reply:

```bash
pbcopy <<'EOF'
<the update>
EOF
```

Confirm it's on the clipboard and ready to paste into Slack.

## Date handling

- No date given → yesterday (the script default).
- "today", "this week", a specific date, or a range → pass the matching `--date`
  to the script and the same `on:`/`after:`/`before:` filters to Slack/GitHub. For a
  range, run the scan once per day or widen the GitHub/Slack date filters and group
  the result by day.
