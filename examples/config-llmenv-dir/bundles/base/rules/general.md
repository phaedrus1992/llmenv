<!-- markdownlint-disable MD003 MD013 MD022 MD041 -->
---

scope: general
priority: high
---

# General coding rules

- Explicit over implicit
- No premature abstraction
- Test your edits — verify they work before shipping
- Code with RFC spec (hostnames, IP addresses, etc): validate against spec, not general knowledge. If 3rd-party module already implements type/validator, use it — don't roll your own.
