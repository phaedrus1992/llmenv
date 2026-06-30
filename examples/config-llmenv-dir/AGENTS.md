# my-llmenv

Personal llmenv configuration repo for `phaedrus@personal-laptop.local`.

## Workflow

**This repo does NOT use branching for changes.** It is a living configuration
repo, not a software project — there is no release process, no CI gating merges,
and a single user. Commit changes directly to `main` and push.

This overrides the global "never commit directly to main / feature branch + PR"
rule, which exists for shared software projects and does not apply here.

`llmenv sync` adds, commits, and pushes the current config directly to `main`.
