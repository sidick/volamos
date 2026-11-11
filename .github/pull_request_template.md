<!--
PR title format (enforced by the "PR title" check): Conventional Commits.
This repo squash-merges with the PR title as the commit subject, and
release-please builds releases and the changelog from those subjects.

  <type>(<scope>): <description>        e.g.  fix(exec): wake Delay() waiters once per deadline

Types that trigger a release: feat (minor bump), fix (patch bump).
Also valid (no release): docs, test, chore, refactor, perf, build, ci, style, revert.
Scope is optional -- use the subsystem (exec, dos, cli, backend, sanitize, ...).
Breaking change: add ! after the type/scope, e.g. feat(cli)!: ...

Full details: CONTRIBUTING.md
-->

## Summary


## Docs

<!--
Required for fix:/feat: PRs that touch crates/** (the "User docs decision"
check). Either list the userdocs/ page(s) you updated, or say why none
applies, keeping the `Docs:` prefix -- that line is what the check looks
for. userdocs/Changelog.md doesn't count: release-please owns it.
-->

Docs:

## Validation

