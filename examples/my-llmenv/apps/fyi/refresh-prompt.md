Run the `fyi` skill's gathering to survey my currently-open, in-flight,
and pending work across GitHub, Linear, Slack, and Pylon. Do the full scan and
ranking exactly as that skill specifies.

Then DO NOT write the markdown briefing or any note file. Instead emit ONLY a
JSON array to stdout — no prose, no markdown fences, nothing else — where each
element is one work item:

  {
    "id":    "<stable id, see below>",
    "tier":  "urgent" | "in_progress" | "pending",
    "title": "<short imperative description>",
    "note":  "<optional one-line context, e.g. 'current cycle', 'stale 6d', CI state>",
    "refs":  [ { "label": "ABC-1473", "url": "https://linear.app/phaedrus1992/issue/ABC-1473" } ]
  }

Tier mapping (from the skill's urgency tiers):
- urgent       -> customer-affecting, current-cycle, PR with red CI / changes-requested, review owed
- in_progress  -> your authored PRs in flight, active WIP, In Review
- pending      -> blocked / waiting-on-QA / waiting-on-review / triaged-not-today

`id` MUST be stable across runs so check-offs survive. Derive it, in this order:
  1. a PR        -> "pr:<repo>#<number>"        e.g. "pr:llmenv#1423"
  2. else a Linear key -> "linear:<KEY>"        e.g. "linear:ABC-1473"
  3. else        -> "slug:<kebab-of-title>"     e.g. "slug:review-queue"
Pick the SAME primary ref each run for a given piece of work. Put every
identifier you mention into `refs` as a label+url (Linear, GitHub PR, etc.) so
the UI can link them. Collapse duplicates that appear in multiple sources into
one item. Drop bot noise.

Output the JSON array and nothing else.
