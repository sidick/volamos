# Contributing

## Pull requests

All changes land through pull requests against `main`; nothing is
committed to `main` directly. PRs are **squash-merged**, with the PR
title becoming the commit subject on `main`.

## User-facing docs

A `fix:`/`feat:` PR that changes `crates/**` has to settle one
question: does this change something a user reads about? The `User
docs decision` check (`.github/workflows/user-docs-gate.yml`) passes
when the PR either

* updates a page under `userdocs/` — see the table in `CLAUDE.md` for
  which page covers what, and run `mkdocs build --strict` afterwards,
  as CI does; or
* records that none applies, as a `Docs:` line in the PR body (the PR
  template has a slot for it) or the `no-user-docs` label.

"No user-visible surface" is a perfectly good answer — an internal
refactor of a trap-dispatch detail has nothing to document. The check
exists so that is a stated decision rather than an omission. One
exception worth naming: `userdocs/Changelog.md` never counts, because
release-please writes it (see [Releases](#releases)) and it is never
hand-edited.

Claude Code sessions in this repo get the same reminder before they
finish, via the `Stop` hook in `.claude/settings.json`
(`.claude/hooks/check-user-docs.sh`).

## PR titles (Conventional Commits)

Because [release-please](https://github.com/googleapis/release-please)
derives version bumps and the changelog from the squashed commit
subjects, every PR title must follow
[Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>
```

The scope is optional. The `PR title` check enforces this on every PR.

### Types

| Type | Release effect | Use for |
|------|----------------|---------|
| `feat` | **minor** version bump | new functionality: a new library call, flag, device, or behavior |
| `fix` | **patch** version bump | bug fixes, including divergences from real AmigaOS behavior |
| `docs` | none | documentation only |
| `test` | none | adding or fixing tests/fixtures only |
| `chore` | none | maintenance: dependency bumps, tooling, release plumbing |
| `refactor` | none | code change that neither fixes a bug nor adds a feature |
| `perf` | none | performance improvement with no behavior change |
| `build` | none | build system or packaging |
| `ci` | none | CI workflows |
| `style` | none | formatting only |
| `revert` | none | reverting a previous change |

A **breaking change** appends `!` after the type/scope
(`feat(cli)!: ...`) or carries a `BREAKING CHANGE:` footer in the PR
body, and produces a major version bump.

Only `feat`, `fix`, and breaking changes trigger a release; the other
types are still required to parse so the history stays machine-readable.

### Scopes

Use the subsystem the change lives in, matching the historical commit
prefixes: `exec`, `dos`, `cli`, `backend`, `loader`, `sanitize`,
`mathlibs`, `bsdsocket`, `docs`, `release`. Examples:

```
feat(dos): add PIPE: channels for libnix pipe()
fix(exec): queue ObtainSemaphore waiters on ss_WaitQueue
chore(deps): update rust crate m68k to v0.16.0
```

## Releases

Releases are fully automated: release-please keeps a release PR open
on `main` that accumulates `feat:`/`fix:` changes; merging that PR
bumps the crate versions, updates `userdocs/Changelog.md`, tags the
release, and publishes the binaries, Docker image, and docs.
