# Changelog

volamos has not had a tagged release yet — see
[the index page](index.md)'s note on project status. This page tracks
major milestones during development; once the first release ships, it
will follow the version scheme in `Cargo.toml`.

## Unreleased (0.1, in development)

- **Core runtime**: CPU + A-line trap dispatch plumbing over the
  [`m68k`](https://crates.io/crates/m68k) crate, real guest heap and
  stack regions (with overflow detection), a configurable total guest
  address space (`--ram`, default 16 MiB) with a clean upfront error
  if `--stack` doesn't leave it room, and a host-backed volume/assign
  filesystem (`-V`/`-a`/`--auto-assign`, multi-assign search order,
  real Amiga path semantics including `/`-as-parent-dir).
- **`dos.library`**: file I/O, locks and directory traversal, pattern
  matching (`ParsePattern`/`MatchFirst`/`MatchNext`), `ReadArgs`/
  `FreeArgs`, a real `ENV:` volume for environment variables, `LoadSeg`/
  `UnLoadSeg`/`RunCommand`, and `System()`/`Execute()` for nested guest
  programs.
- **`exec.library`**: memory allocation (flat `AllocMem`/`AllocVec`/
  memory pools, and a real coalescing `MemHeader`/`MemChunk` free list
  for `Allocate`/`Deallocate`), guest-visible lists/nodes/message
  ports, task/signal basics with host `SIGINT`/`SIGTERM` delivery, the
  full `SignalSemaphore` API, and CPU-detection plumbing (`AttnFlags`/
  `CacheControl`/`Supervisor`) — `--cpu`/`--fpu` select the emulated
  model.
- **`utility.library`/`locale.library`**: tag lists, case-insensitive
  compare/conversion (classic Amiga charset), Amiga date conversions.
- **`intuition.library`**: a thin headless stub (`DisplayAlert`/
  `AutoRequest`/`EasyRequestArgs`/`CurrentTime`), matching `vamos`'s own
  scope for this library.
- **Math libraries**: `mathffp`, `mathtrans`, `mathieeedoubbas`,
  `mathieeedoubtrans` — real arithmetic, including a faithfully
  reproduced historical `SPSub`/`SPDiv` argument-order quirk.
- **Empirical hardening**: extensive testing against a real Workbench
  3.1.4 `C:` command corpus and real third-party binaries (the PhxAss
  assembler, a real backup-tool project), plus a full audit against
  `vamos`'s own library/device coverage to close the gaps it flagged.

## What's not done yet

- A formal three-oracle parity pass (volamos vs. `vamos` vs. real
  Kickstart, on a shared corpus).
- A tagged release / packaged binaries for direct download.
- `exec.library`'s `MakeLibrary`/`SetFunction` (would need a real
  architectural extension — see
  [Supported Libraries](Supported-Libraries.md)).
