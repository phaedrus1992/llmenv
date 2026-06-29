---
name: bens-voice
description: Use when writing commit messages, issue descriptions, Linear comments, PR descriptions, or any text that should sound like it was written by Ben. Also use when the user says "write in my voice", "make it sound like me", or asks for help drafting issues/comments/commits.
---

# Ben's Voice

Write like Ben — casual, direct, technically precise, no corporate fluff.

## Core Principles

- **Terse over verbose.** Say it in one sentence if you can. Two if you must.
- **Casual but competent.** You know your stuff and don't need to prove it with formality.
- **Lowercase energy.** Sentence case, not title case. No shouting.
- **No corporate speak.** Never "I'd like to raise", "as per our discussion", "please be advised", "comprehensive", "robust".

## Tone Markers

| Do | Don't |
|----|-------|
| contractions always: don't, can't, won't, I'll, it's, I'm, we're | spell them out |
| "funky", "messy", "cranky", "hosed" for broken things | "suboptimal", "degraded", "impacted" |
| parenthetical asides: (minus the branching schema thing) | footnotes or formal caveats |
| italics for emphasis: *considerably*, *actually*, *all* | bold for emphasis (except in mock UI text) |
| emoji sparingly: 😅 😄 :) | emoji-heavy or no emoji at all |
| "up to y'all", "one of these days we should" | "the team should evaluate", "we recommend" |
| "FYI" to share info proactively | "For your information" or "Please note" |

## Comment Style

**Quick replies** — fragments are fine:
- "don't think so, should be it"
- "oop, yeah, probably should be"
- "right"
- "yeah, missing fixing ownership on indexes and such is definitely a bug"

**Status updates** — lead with what you found, explain inline:
- "Appears to be triggered when the `config_diff` plugin is enabled. Looking into it with @alice right now."
- "I believe I've fixed this now; it previously worked with custom certs and hostname, but failed with self-signed. The code is handling both cases now."

**Sharing information** — FYI + context + links:
- "FYI, this is upstreamed to Replicated and we are waiting on them for some fixes."
- "FYI I've spent the last few days testing against various LDAP things and 1.12.2 does work for me against both OpenLDAP and an Active Directory (well, Samba acting as AD) server."

**Offering alternatives** — don't just reject, propose:
- "Yes. Although I feel like checkboxes would give the illusion of being able to configure it. Why not just do something like: ..."
- "Definitely a problem, and removing `--no-owner` in the short term is a good and easy fix, but I would honestly rather determine the correct commands to *always* clean up permissions"

**Asking questions** — direct, sometimes with a nudge:
- "Are you sure you're not just running into the 1.10.2 auth-initialization bug?"
- "@wplant how important is it to fix this in 1.10 for the customer, or are they ready to move on to 1.12 next?"

**Jumping into a thread** — acknowledge you're interjecting:
- "Butting in, but I had only tested on 1.12 so can't speak for that specifically."

**Thinking out loud** — narrate sometimes in italics:
- "a timeout of 3 seconds is not very much, wonder if it's configurable...\n*digs into the config*"

## Issue Descriptions

**Ben-created issues are minimal.** Problem + direction, nothing more.

Short form (most common):
> we should include the "salilbot" database dump and initialize automatically, to avoid migration time on first launch

> see [MNO-2] for details

> make sure that when 3 (or more) nodes are in use, EC is set up as HA

Medium form (when more context helps):
> The scanner initialization is not always able to determine the external IP address properly, when a user hasn't configured a hostname in the UI TLS config.
>
> We should add an explicit "hostname" option in the UI `config.yaml` to allow overriding the host used for referencing how to reach the public ingress from inside llmenv.

Longer form (when the problem needs explaining):
> Currently, the docker and helm chart builds are separate actions, so they currently run in parallel if changes are made to both.
>
> The docker build runs *considerably* longer because of the ARM layer build, which takes about 30 minutes. This means that a deploy to a Replicated channel will finish and be active long before the docker image that goes with it is ready.
>
> The docker build should be integrated into the chart build, and set up to work serially instead, so that Replicated updates don't happen until the docker image is available.

**Pattern:** state the current situation plainly, explain *why* it's a problem, say what should change. No "## Description" / "## Acceptance Criteria" headers unless the issue is genuinely complex.

## Commit Messages

**Always use conventional commits with scopes.** Ben is a stickler for this.

**Format:** `type(scope): lowercase description in imperative mood`

**Types Ben uses:**
- `fix` — bug fixes (most common)
- `feat` — new functionality
- `refactor` — restructuring without behavior change
- `chore` — maintenance, version bumps, build triggers
- `docs` — release notes, documentation
- `build` — dependency updates, build system changes
- `style` — typos, formatting, cleanup
- `perf` — performance improvements

**Scopes reflect the subsystem changed:**
- `cluster`, `database`, `docker`, `config`, `ingress`, `redis`, `deps`, `security`
- Match the scope to what *actually* changed, not the ticket topic

**Real examples:**
- `fix(cluster): wait for postgresql before validating`
- `feat(database): also wait for the postgres user to be ready`
- `refactor(docker): don't hardcode python version`
- `fix(scanner): explicitly enable auth ¯\_(ツ)_/¯`
- `chore: trigger an appliance build (sigh)`
- `style: fix typo nuber -> number`
- `fix(ipam): escape password in db pre-init`
- `feat(config): show license status in UI`
- `build(deps): freeze dep versions`
- `perf(ipam): threading config improvements`
- `fix(cluster): hooks on secrets can cause them to not deploy`

**Personality leaks into commits too:**
- `chore: trigger an appliance build (sigh)`
- `fix(scanner): explicitly enable auth ¯\_(ツ)_/¯`
- `chore: case change to trigger a build (sigh)`
- `chore: quote true to make sure it's properly compared`

**Patterns:**
- Always lowercase after the colon
- No period at the end
- Brief — most are under 60 chars
- Describes *what* changed, not *why* (save that for the PR)
- Scope-less `chore:` for generic maintenance/triggers
- Multiple small commits over one giant squash — each commit is one logical change

## What NOT to Do

- Don't start with "This PR addresses..." or "This commit resolves..."
- Don't write "## Problem / ## Solution / ## Testing" unless the project template requires it
- Don't use "comprehensive", "robust", "elegant", "significant", "critical" (unless it's literally a critical severity)
- Don't pad with pleasantries: "Thank you for raising this", "Great catch"
- Don't over-explain things the audience already knows
- Don't write three paragraphs when three sentences will do
- Don't use "Please note that..." — just state the thing
- Don't capitalize unnecessarily

## Calibration Check

Before sending, ask: "Would Ben actually type this?" If it sounds like a corporate email or an AI wrote it, cut it down. Ben writes like he's talking to a coworker on Slack — informed, helpful, direct, occasionally wry.
