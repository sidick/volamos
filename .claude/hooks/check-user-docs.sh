#!/usr/bin/env bash
# Stop hook: before Claude Code hands work back, check whether this
# branch changed runtime code without touching the user-facing docs
# site, and if so block once with a reminder naming the pages that
# plausibly cover the change.
#
# This is the in-session half of the docs rule CLAUDE.md states and
# .github/workflows/user-docs-gate.yml enforces on the PR. The CI job
# is the real guarantee; this hook just moves the reminder earlier,
# before a PR exists, where acting on it costs nothing.
#
# Deliberately a *reminder*, not a wall:
#
#   - It blocks at most once per (session, tree state). Plenty of real
#     fixes have no user-visible surface (see the CI job's `Docs:`
#     escape hatch), so a hook that re-blocked until userdocs/ changed
#     would force busywork -- or, worse, loop. Once reminded, Claude
#     decides; the PR gate is where the decision gets recorded.
#   - userdocs/Changelog.md does NOT count as a docs change: it is
#     release-please's file and is never hand-edited (see CLAUDE.md).
#   - Anything unexpected (no git, no jq, detached HEAD, no merge base)
#     exits 0 silently. A broken hook must not stop work.

set -uo pipefail

# jq is how the session id is read; without it there's no way to key
# the once-per-session sentinel, so skip the check entirely.
command -v jq >/dev/null 2>&1 || exit 0

payload=$(cat)
session_id=$(printf '%s' "$payload" | jq -r '.session_id // empty' 2>/dev/null)
[ -n "$session_id" ] || exit 0

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0
cd "$repo_root" || exit 0

# Every path this branch touches: committed since the merge base with
# the default branch, plus whatever is still uncommitted or untracked.
default_branch=main
changed=$(
  {
    if base=$(git merge-base "$default_branch" HEAD 2>/dev/null); then
      git diff --name-only "$base" HEAD 2>/dev/null
    fi
    git diff --name-only HEAD 2>/dev/null
    git ls-files --others --exclude-standard 2>/dev/null
  } | sort -u
)
[ -n "$changed" ] || exit 0

touches_code=$(printf '%s\n' "$changed" | grep -c '^crates/')
[ "$touches_code" -gt 0 ] || exit 0

# userdocs/Changelog.md is release-please's; it never satisfies the rule.
touches_docs=$(printf '%s\n' "$changed" | grep '^userdocs/' | grep -cv '^userdocs/Changelog\.md$')
[ "$touches_docs" -eq 0 ] || exit 0

# Remind once per session per tree state: a later stop with further
# changes gets a fresh reminder, the same stop twice does not.
state_hash=$(printf '%s' "$changed" | git hash-object --stdin 2>/dev/null) || exit 0
sentinel_dir="${TMPDIR:-/tmp}/volamos-docs-hook"
sentinel="$sentinel_dir/${session_id}-${state_hash}"
mkdir -p "$sentinel_dir" 2>/dev/null || exit 0
[ -e "$sentinel" ] && exit 0
: >"$sentinel"

jq -n '{
  decision: "block",
  reason: (
    "This branch changes crates/** but nothing under userdocs/ (Changelog.md excluded -- it is release-please'"'"'s file and is never hand-edited).\n\n" +
    "Decide now, before handing the work back, whether this change has a user-visible surface, and say which in your summary:\n" +
    "  - CLI flags or arguments      -> userdocs/CLI-Reference.md\n" +
    "  - library/handler coverage    -> userdocs/Supported-Libraries.md\n" +
    "  - volume, assign or path rules-> userdocs/Volumes-and-Assigns.md\n" +
    "  - .volamos config keys        -> userdocs/Configuration.md\n" +
    "  - a hardware-settled vamos disagreement -> userdocs/Differences-from-vamos.md\n" +
    "  - a worked example worth showing        -> userdocs/Examples.md\n\n" +
    "If one applies, update that page now (and run `mkdocs build --strict`). If none does -- an internal fix with no surface a user reads about -- that is a fine answer: put the reason in the PR as a `Docs:` line, which is what the user-docs-gate CI check looks for.\n\n" +
    "This reminder fires once; proceed either way after deciding."
  )
}'
exit 0
