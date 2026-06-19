# General coding rules

<!--
  This file is a `rules/` entry in the `base` bundle. All files under
  rules/ are injected into the agent's system context alongside AGENTS.md.

  Use rules/ files to split topic-specific guidance out of the main AGENTS.md
  so each file stays focused and scannable. llmenv concatenates them all.
-->

- Explicit over implicit
- No premature abstraction
- Test every edit
- Code involving RFC specs (hostnames, IP addresses, URIs, etc.): validate
  against the spec, not general knowledge. If a third-party library already
  implements the type or validator, use it — don't roll your own.
