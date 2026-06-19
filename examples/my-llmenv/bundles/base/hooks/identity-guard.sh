#!/usr/bin/env bash
# identity-guard.sh — enforce identity hygiene when operating under an alternate
# git identity (e.g. an open-source contributor account separate from your
# primary work account).
#
# Fires on: UserPromptSubmit, PreToolUse
# Exit 2: blocks the action, surfaces stderr message to Claude
# Exit 0: allows the action to proceed
#
# WHY THIS EXISTS:
#   Developers sometimes maintain separate identities for open-source
#   contributions (different git user, different GitHub account). Accidentally
#   cross-contaminating those identities — by referencing your real name in a
#   commit while pushing from the OSS account — is both embarrassing and hard
#   to undo once pushed. This hook catches the slip before it happens.
#
# WHAT IT CHECKS:
#   1. Is the current git identity the OSS (alternate) identity?
#      Gate: only enforces rules when git config user.email matches ANON_EMAIL.
#   2. Input must not reference the primary (personal) identity.
#      Catches name variants and misspellings, case-insensitive.
#   3. The active `gh auth status` account must match the repo owner:
#      - Repos owned by the OSS account → must use the OSS gh account.
#      - All other repos → must use the personal gh account.
#
# HOW IT INTEGRATES:
#   bundle.yaml registers this as both a UserPromptSubmit and PreToolUse hook.
#   UserPromptSubmit fires with the user's prompt text in the payload.
#   PreToolUse fires with the tool name and input in the payload.
#
# ADAPTATION GUIDE:
#   Replace the identity constants below with your own values.
#   PERSONAL_USER / PERSONAL_EMAIL: your real/primary identity.
#   ANON_USER / ANON_EMAIL: the alternate OSS identity.
#   ANON_ORG: the GitHub org or username for OSS repos.
#   Add more forbidden[] variants for common misspellings of your name.

# shellcheck disable=SC2016  # single quotes are deliberate: no shell expansion

python3 - "$@" <<'PYEOF'
import json
import re
import subprocess
import sys
import os

# --------------------------------------------------------------------------
# Identity fragments — assembled here to avoid literal self-matches in the
# source file itself (the guard would falsely trigger on its own source).
# Replace these with your real values.
# --------------------------------------------------------------------------
PERSONAL_FIRST = "Alice"
PERSONAL_LAST  = "Smith"
PERSONAL_USER  = PERSONAL_FIRST + PERSONAL_LAST   # "AliceSmith"
PERSONAL_EMAIL = "alice@example.com"

ANON_FIRST = "alice"
ANON_LAST  = "oss"
ANON_USER  = ANON_FIRST + "-" + ANON_LAST         # "alice-oss"
ANON_EMAIL = "alice-oss@example.com"
ANON_ORG   = "alice-oss"  # GitHub org/user that owns OSS repos


def fail(msg: str) -> None:
    print(msg, file=sys.stderr)
    sys.exit(2)


# --------------------------------------------------------------------------
# Gate: only enforce when the current git identity is the OSS (anon) account.
# If the user is committing as their primary identity, this hook is a no-op.
# --------------------------------------------------------------------------
try:
    result = subprocess.run(
        ["git", "config", "user.email"],
        capture_output=True, text=True, timeout=5
    )
    current_email = result.stdout.strip()
except Exception:
    sys.exit(0)  # can't read git config → not in a git repo, skip

if current_email != ANON_EMAIL:
    sys.exit(0)  # primary identity is active → no restrictions


# --------------------------------------------------------------------------
# Read the hook event payload from stdin.
# UserPromptSubmit carries `.prompt`; PreToolUse carries `.tool_input`.
# --------------------------------------------------------------------------
try:
    event = json.load(sys.stdin)
except (json.JSONDecodeError, EOFError):
    sys.exit(0)

hook_type = event.get("hook_event_name", "")
if hook_type == "UserPromptSubmit":
    haystack = event.get("prompt", "")
elif hook_type == "PreToolUse":
    # For tool calls, inspect the tool input as a JSON string so we catch
    # identity leaks in commit messages, branch names, file contents, etc.
    haystack = json.dumps(event.get("tool_input", {}))
else:
    sys.exit(0)


# --------------------------------------------------------------------------
# Rule 1: input must not reference the personal (primary) identity.
#
# We build the forbidden list from fragments so the literal strings don't
# appear in source (otherwise this script would trigger on itself if someone
# asked Claude to show the hook code). Each entry is checked after
# normalizing runs of whitespace/separators to a single space so that
# "Alice_Smith", "Alice-Smith", "Alice.Smith" all match "Alice Smith".
# --------------------------------------------------------------------------
forbidden = []

# Primary username in various forms.
forbidden.append(PERSONAL_USER)                   # AliceSmith
forbidden.append(PERSONAL_FIRST + " " + PERSONAL_LAST)   # Alice Smith

# Common misspellings / separator variants (add your own).
forbidden.append(PERSONAL_FIRST + PERSONAL_LAST[:-1])    # AliceSmit (typo)

# Primary email.
forbidden.append(PERSONAL_EMAIL)


def normalize(s: str) -> str:
    """Collapse any separator run to a single space for fuzzy matching."""
    return re.sub(r"[\s_.\-]+", " ", s).lower()


normalized_haystack = normalize(haystack)
for needle in forbidden:
    if normalize(needle) in normalized_haystack:
        fail(
            f"identity-guard: input references the personal identity "
            f"(matched \"{needle}\") while the OSS git identity "
            f"({ANON_EMAIL}) is active. Remove the reference or switch "
            f"git identity (`git config user.email <your-email>`)."
        )


# --------------------------------------------------------------------------
# Rule 2: the active `gh auth status` account must match the repo owner.
#
# If we're inside a git repo, check that the authenticated GitHub account
# is the OSS account for OSS-owned repos, or the personal account otherwise.
# This prevents accidentally pushing to an OSS repo while authenticated as
# the personal account (or vice versa).
# --------------------------------------------------------------------------
try:
    # Get the remote URL to determine the repo owner.
    remote_result = subprocess.run(
        ["git", "remote", "get-url", "origin"],
        capture_output=True, text=True, timeout=5
    )
    remote_url = remote_result.stdout.strip()
    if not remote_url:
        sys.exit(0)  # no remote → nothing to check

    # Extract owner from HTTPS or SSH remote URLs.
    # HTTPS: https://github.com/<owner>/<repo>
    # SSH:   git@github.com:<owner>/<repo>
    match = re.search(r"[:/]([^/]+)/[^/]+(?:\.git)?$", remote_url)
    if not match:
        sys.exit(0)
    repo_owner = match.group(1)

    # Get the currently authenticated gh account.
    auth_result = subprocess.run(
        ["gh", "auth", "status", "--json", "username"],
        capture_output=True, text=True, timeout=10
    )
    if auth_result.returncode != 0:
        sys.exit(0)  # gh not configured → skip check

    auth_data = json.loads(auth_result.stdout)
    # gh may return a list (multiple hosts) or a single object.
    if isinstance(auth_data, list):
        gh_user = auth_data[0].get("username", "") if auth_data else ""
    else:
        gh_user = auth_data.get("username", "")

    # Enforce: OSS repos must use OSS account; everything else uses personal.
    if repo_owner == ANON_ORG and gh_user != ANON_USER:
        fail(
            f"identity-guard: repo is owned by OSS org ({ANON_ORG}) but "
            f"gh is authenticated as '{gh_user}'. Switch with "
            f"`gh auth switch --user {ANON_USER}`."
        )
    elif repo_owner != ANON_ORG and gh_user == ANON_USER:
        fail(
            f"identity-guard: repo '{repo_owner}' is not an OSS repo but "
            f"gh is authenticated as the OSS account '{ANON_USER}'. "
            f"Switch with `gh auth switch --user {PERSONAL_USER.lower()}`."
        )

except subprocess.TimeoutExpired:
    sys.exit(0)  # timeout → skip, don't block
except Exception:
    sys.exit(0)  # any other error → skip, don't block

sys.exit(0)
PYEOF
