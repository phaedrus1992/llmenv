---
scope: general
priority: critical
---

# Workflow Continuity

When a skill's instructions explicitly hand off to another skill (e.g. dev-sprint → ship-issue,
ship-issue → pre-pr-review), invoke that skill immediately. Do NOT:
- Stop to summarize "Phase N complete"
- Ask if you should continue
- Treat the handoff as a natural stopping point

The only valid stops mid-workflow:
(a) Genuinely blocked on missing info
(b) About to take a destructive/irreversible action not yet authorized
(c) Scope materially changed

"Phase X done, next is Y" is not a stop — it is a handoff. Execute Y.
