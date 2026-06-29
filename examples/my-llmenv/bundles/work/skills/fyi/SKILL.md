---
name: fyi
description: Use when the user asks for an "fyi", "morning todo", "morning briefing", "what's on my plate", "what do I have open", "what's in flight", "where did I leave off", "what needs my attention today", or "my open work across projects". Surveys the user's currently-open, in-flight, and pending work across GitHub, Linear, Slack, Pylon, and Notion, then produces a ranked, GitHub-markdown briefing for the day. Forward-looking sibling of daily-update (which reports what already happened).
resources:
  - scripts/gh-open-work.sh
---

# FYI

Produce a ranked briefing of what the user **currently owns and needs to act on** —
open, in-flight, or pending work — to start the day from. This is the
forward-looking sibling of `daily-update`: that skill reports what *already
happened* on a day; this one surveys what is *still open* and ranks it by urgency.

This is a reporting task — **read-only everywhere**. Never post, comment, edit,
or close anything. The only side effects are a local note file and memory.

Not the same as `dev-sprint`. That skill *selects new work* from a milestone to
pick up. This skill *surveys existing open work* the user already owns. If the
user wants to start something new, defer to `dev-sprint`.

## What counts as "what I own"

- **DEF** (customer help-desk) tickets the user is assigned to **or subscribed
  to** (commenting auto-subscribes) **or discussed in Slack**.
- **GHI / ABC** Linear issues assigned to the user, state not Done/Cancelled.
- **GitHub** open PRs the user authored, plus PRs awaiting the user's review.
- **llmenv-chart maintainer duty** — the user is a core maintainer of
  `llmenv-community/llmenv-chart`. Treat **every** open issue and PR in that repo
  as something the user needs to keep on top of / review, even when they are not
  the author, assignee, or a requested reviewer. This is a whole-repo watch, not a
  mention filter.

The projects of note (priority repos — not an exhaustive filter):
`llmenv`, `dochub`, `llmenv-plugin`, `llmenv-cli`, `dev-commons`.

## Sources (gather all, in parallel)

Run the scripts and MCP queries concurrently. Process large script output with
`ctx_execute`/`ctx_execute_file` rather than reading raw logs into context.

1. **GitHub — open PRs.** The in-flight spine.
   ```bash
   "${CLAUDE_CONFIG_DIR}/skills/fyi/scripts/gh-open-work.sh" phaedrus1992
   ```
   Emits two lists: PRs the user authored (with draft/ready + last-updated) and
   PRs awaiting the user's review (with author + last-updated). The query is
   org-scoped, so it catches repos beyond the priority seven — rank those lower.

2. **Linear — open issues.** List the user's open issues and keep state ∉
   {Done, Cancelled, Backlog-without-cycle}:
   `mcp__grid__linear_list_issues(assignee="<user work email>")`. To catch
   *subscribed-but-not-assigned* DEF (the "commented on" case), also
   `linear_search_issues(query="DEF")` and keep ones where the user appears as
   subscriber or commenter (`linear_get_issue_comments` to confirm authorship when
   ambiguous). Read the current **cycle** off each ABC issue — current-cycle
   assignment is a strong urgency signal (see below).

3. **Slack — pending threads + context.** Use the **Slack MCP plugin**
   (`mcp__plugin_slack_slack__*`), not the grid `slack_*` tools — the plugin
   searches private channels and DMs the grid index doesn't cover, which is where
   most handoffs and customer escalations actually live. Resolve the user's Slack id
   first (`mcp__plugin_slack_slack__slack_search_users` on the work email). Then find
   live threads that imply work owed:
   - DEF keys the user discussed:
     `mcp__plugin_slack_slack__slack_search_public_and_private(query="from:<@USER_ID> DEF", sort="timestamp")`
     and threads mentioning a key the user is participating in.
   - Recent handoffs/asks directed at the user (`to:<@USER_ID>` or mentions) still
     awaiting a reply.
   Capture a **permalink** for any thread worth surfacing — include it inline in
   the briefing when it adds context the ticket alone doesn't carry.

4. **Pylon — customer impact** (if the tools are available; the bundle permits the
   read-only `pylon_*` tools). Pylon is the customer help-desk behind DEF. Use
   `pylon_search_issues` / `pylon_list_issues` to confirm whether an DEF ticket
   maps to a live customer issue — the strongest "affects a customer" signal. If
   Pylon tools are not connected, say so once and fall back to Linear labels/links.

5. **llmenv-chart — maintainer watch.** The script above is `phaedrus1992`-scoped and
   only catches authored/review-requested PRs, so it misses this repo entirely. List
   *all* open issues and PRs the user should keep an eye on:
   ```bash
   gh issue list --repo llmenv-community/llmenv-chart --state open \
     --json number,title,url,updatedAt,labels -L 50
   gh pr list --repo llmenv-community/llmenv-chart --state open \
     --json number,title,url,updatedAt,isDraft,author -L 50
   ```
   Surface these regardless of author/assignee. Rank a fresh issue/PR (no maintainer
   reply yet) higher than a stale one already under discussion. Still drop bot noise
   (dependabot, etc.) per the rules below.

6. **Sessions + Notion — WIP and cycle context.** Reuse the daily-update scanner to
   catch work that exists *only* in yesterday's session logs (uncommitted, no PR or
   issue yet):
   ```bash
   "${CLAUDE_CONFIG_DIR}/skills/daily-update/scripts/scan-sessions.mjs" --date <yesterday>
   ```
   This script is owned by the `daily-update` skill, which ships in the same `work`
   bundle — it is a cross-skill reuse, not a resource of this skill. If daily-update
   is absent, skip this source rather than failing the briefing.
   Query Notion only for specific urgency context (ABC cycle plan, a referenced doc)
   — `notion_search` / `notion_get_recent_changes`, not a broad sweep.

## Urgency heuristics (drive the ranking)

Rank by importance, highest first. Signals that raise urgency:

- **Customer-affecting DEF** → top tier. Confirm via Pylon mapping, a customer
  account link, or a customer label on the Linear issue.
- **ABC current-cycle assignment** → high. An issue committed to the active cycle
  outranks one with no cycle.
- **PR awaiting the user's review** → high — it blocks a teammate. A user-authored
  PR with failing CI or changes-requested is also high — it blocks the merge.
  (Drill CI with `gh pr checks <n>`: for review requests, run it *before* ranking
  to apply the failing-tests skip below; for user-authored PRs, only for those
  that reach the briefing.)
  **Exception: a review request on a still-draft PR** is not reviewable yet —
  drop it to the Pending/blocked tier marked `_(draft — not ready for review)_`,
  not Urgent/High.
  **Exception: a review request on a PR with failing tests** is not reviewable
  yet either — the author has to get CI green first. **Skip it entirely** (don't
  surface it in any tier). A check counts as failing only when it has concluded
  red; still-running or skipped checks do not trigger the skip.
- **Staleness** → flag, don't bury: an in-progress item with no movement for several
  days needs a nudge ("stale N days").
- **Waiting-on-someone** (blocked on review, customer reply, another team) → drops to
  the Pending/blocked tier — real, but not today's action.

Cross-source corrections, same as daily-update:

- **Collapse duplicates** — the same task appears in PR + Linear + Slack; report once,
  linking all its identifiers.
- **Drop noise** — bot-authored review requests (`dependabot`, `github-actions[bot]`,
  Copilot) and abandoned/very-stale PRs are not "what needs my attention" unless the
  user clearly owns them. Mention a bot cluster as one line, don't enumerate.
- **Don't drop a single-source item** — a Linear issue with no PR, or a Slack ask with
  no ticket, is still real work.
- **Surface DEF linkage** — when a PR or ABC/GHI issue is tied to a customer
  **DEF** ticket (a `Fixes`/`relates to` in the PR or issue body, a Linear relation,
  a shared branch name, or the same Slack thread), attach the `[DEF-xxxx](url)` ref
  to that item and mark it `_customer_`. Resolve the link via `linear_read_issue`
  relations or the PR body (`github_read_pr`). A customer-support item should never
  read as plain internal work — the visible DEF ref is the tell.

## Output format

GitHub-flavored markdown (the user reads this themselves; it is **not** pasted into
Slack). One top-focus line, then tiers. Omit an empty tier.

```markdown
# FYI — YYYY-MM-DD

**Top focus:** <the single most important thing, one line, with its ref>

## Urgent
- [ ] <item> — [DEF-1234](url) · [#456](pr-url) · _customer: <name>_
- [ ] <item> — [ABC-1234](url) _(current cycle)_

## In progress
- [ ] <item> — [#1361](pr-url) _(ready, updated 2026-06-16)_
- [ ] <item> — [ABC-1234](url) _(stale 6 days)_

## Pending / blocked
- [ ] <item> — [#1393](pr-url) _(awaiting review from @x)_ · [slack](thread-url)
- [ ] <item> — [#1402](pr-url) _(draft — not ready for review)_
```

- **Link every identifier — always a markdown hyperlink, never a bare ref.** Every
  issue key, PR number, and Slack thread must be a `[text](url)` link so the terminal
  renders it clickable (OSC 8). Do **not** emit an identifier as bold (`**ABC-1234**`)
  or a code span (`` `ABC-1234` ``) — those are not clickable. Map:
  - Linear keys → `https://linear.app/phaedrus1992/issue/<KEY>` (any prefix: DEF, ABC,
    GHI, JKL).
  - GitHub PRs → `https://github.com/phaedrus1992/<repo>/pull/<n>`.
  - Slack threads → the permalink captured during gathering.
- ASCII only — no Unicode arrows/symbols carried from source titles
  (`vcluster↔EC` → `vcluster/EC`, `->` not `→`).
- Plain and factual. A bug fix is a bug fix. **Do not** invoke `bens-voice` — this is
  a private briefing, not outward communication.
- Short. A handful of bullets per tier; the user can ask any item to be expanded.

## Deliver

1. **Print** the briefing in the reply.
2. **Write** it to a dated note: `~/notes/todo-YYYY-MM-DD.md` (create `~/notes/` if
   missing; overwrite if re-run the same day).
3. **Persist** the salient open items and their urgency to memory so the next session
   recalls what was on the user's plate.

## Date handling

- Default is **today** (the briefing is "what's open right now"). The session-scan
  source uses **yesterday** to catch fresh WIP regardless.
- Open-state queries (GitHub `--state=open`, Linear non-terminal states) are not
  date-scoped — they reflect the live state at run time.
