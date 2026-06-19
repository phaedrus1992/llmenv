# Workflow Continuity

<!--
  This rule is separate from AGENTS.md because it governs the behavior of
  multi-phase skills (dev-sprint, ship-issue, pre-pr-review) and needs to
  be highly visible. Keeping it in its own file makes it easy to find,
  update, and reference in skill SKILL.md files.

  It interacts with skills by overriding the default "pause and summarize"
  behavior that LLMs tend toward at phase boundaries. The rule tells the
  agent to execute skill handoffs immediately rather than stopping to ask.
-->

When a skill's instructions explicitly hand off to another skill (e.g.
dev-sprint → ship-issue, ship-issue → pre-pr-review), invoke that skill
immediately. Do NOT:
- Stop to summarize "Phase N complete"
- Ask if you should continue
- Treat the handoff as a natural stopping point

The only valid stops mid-workflow:
(a) Genuinely blocked on missing info
(b) About to take a destructive/irreversible action not yet authorized
(c) Scope has materially changed

"Phase X done, next is Y" is not a stop — it is a handoff. Execute Y.
