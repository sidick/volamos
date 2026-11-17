#!/usr/bin/env bash
# PreToolUse(Bash) hook: refuse `gh pr create` until /code-review has
# been run, and its valid findings addressed, for the current HEAD.
#
# Proof is a marker holding the reviewed commit, kept in the git dir so
# it is never tracked and never leaks into a PR. Honour-system by
# design: a hook cannot run a skill, only refuse until the marker
# matches HEAD. Any commit after the review makes the marker stale, so
# a fix made after review means re-reviewing (or re-recording).
#
# Anything unexpected (no jq, not a git repo) exits 0 silently: a
# broken hook must not stop work.

set -uo pipefail
command -v jq >/dev/null 2>&1 || exit 0

cmd=$(jq -r '.tool_input.command // empty' 2>/dev/null)
# `gh` must be in command position (start, or after ; & | ( ), optionally
# behind VAR=value assignments, so a command that merely mentions the
# phrase (echo, grep, a commit message) does not trip it.
printf '%s' "$cmd" | grep -Eq '(^|[;&|(])[[:space:]]*([A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*[[:space:]]+)*gh[[:space:]]+pr[[:space:]]+create' || exit 0

# Escape hatch for a PR that genuinely needs no review (typo, dep bump).
printf '%s' "$cmd" | grep -q 'CLAUDE_SKIP_CODE_REVIEW=1' && exit 0

git rev-parse --show-toplevel >/dev/null 2>&1 || exit 0
head=$(git rev-parse HEAD 2>/dev/null) || exit 0
marker="$(git rev-parse --git-dir)/claude-code-review-head"
[ "$(cat "$marker" 2>/dev/null)" = "$head" ] && exit 0

cat >&2 <<EOF
Blocked: /code-review has not been recorded for HEAD ($head).
Before creating the PR: run /code-review, fix or answer each finding,
commit any fixes, then record the commit you are opening the PR from:
  git rev-parse HEAD > "$marker"
and re-run this command. (For a PR that genuinely needs no review,
prefix the command with CLAUDE_SKIP_CODE_REVIEW=1.)
EOF
exit 2
