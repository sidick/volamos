# Changelog

This page tracks major milestones during development, following the
version scheme in `Cargo.toml`.

## Unreleased

- **Built-in standard-volume defaults** (issue #43): `SYS:`, `RAM:`,
  and the standard `C:`/`S:`/`LIBS:`/`DEVS:`/`ENVARC:`/`T:`/`ENV:`
  assigns onto them now resolve out of the box, with zero `-V`/`-a`
  configuration needed — backed by empty host directories created only
  on first actual use (`SYS:` persists across runs under
  `--volumes-dir`/`VOLUMES_DIR`, default `~/.volamos.d/volumes`;
  `RAM:`/`T:`/`ENV:` are a fresh, unique-per-process temp directory,
  removed automatically once the run ends — never shared between, or
  surviving past, a single `volamos` invocation). An explicit `-V`/`-a`
  for the same name always overrides the matching default, so
  `-V SYS:~/amiga/wb31` brings that volume's own real `C:`/`Libs:`/etc.
  along with it rather than the synthetic skeleton reappearing
  underneath it. `--no-defaults`/`DEFAULTS=false` restores the
  original "nothing configured means no filesystem at all" behavior.
  Only these specific real-AmigaOS names are covered — unlike `vamos`'s
  broader auto-assign machinery, a genuinely unknown/typo'd volume name
  still fails loudly with an `IoErr()`. See
  [Volumes and Assigns](Volumes-and-Assigns.md#standard-defaults).
- **Program-directory config file** (issue #16): a `.volamos` next to
  the launched binary (in `<program>`'s own containing directory) is
  now consulted between `./.volamos` and `~/.volamos`, so a toolchain
  installation can carry its own volume/CPU settings and be invoked
  from anywhere. **Behavior change**: relative `VOLUME`/`AUTO_ASSIGN`
  paths in *any* config file now resolve against that file's own
  directory instead of volamos's process working directory — an
  existing `~/.volamos` or `./.volamos` using relative paths resolves
  differently if its directory isn't where you invoke volamos from
  (CLI-supplied relative paths are unchanged). See
  [Configuration](Configuration.md).
- **Parent-step (`a//b`) fidelity**: a parent step that climbs above
  the volume root now fails like a missing object instead of being
  clamped at the root — the root has no parent, matching real FFS
  behavior as verified in
  [amitools PR #7](https://github.com/AmigaPorts/amitools/pull/7)'s
  writeup (vamos made the same change there). Lock names reported back
  to the guest (`NameFromLock()`) now also carry the volume/assign
  name in its *configured* spelling (`Lock("sys:foo")` names itself
  `SYS:foo`), completing the canonical-path treatment their components
  already got (parent steps collapsed, on-disk case).
- **`ExAll`/`ExAllEnd`**: the batched directory scanner libnix's
  `readdir()` (and so most gcc-built programs that scan a directory)
  is built on, including continuation across calls via `eac_LastKey`,
  `eac_MatchString` pattern filtering, all `ED_NAME`..`ED_OWNER`
  entry levels, and `AllocDosObject`/`FreeDosObject` support for
  `DOS_EXALLCONTROL`. Before this, `ExAll` was an unhandled call, so
  every directory looked empty to `readdir()`-based programs. Mirrors
  [amitools PR #8](https://github.com/AmigaPorts/amitools/pull/8),
  which added the same to vamos.

## 0.3

- **Interpreter and release-build performance**: `FlatMemory`'s
  multi-byte reads/writes now do a single bounds check plus a native
  big-endian load/store instead of decomposing into repeated
  byte-level calls, and the non-`--jit` execution path now runs
  through the `m68k` crate's `run_batch` (batch size 1) instead of
  looping its plain `step`, picking up `run_batch`'s non-cycle-accurate
  bus mode and raw-pointer fast-memory access — same per-instruction
  granularity, no observable behavior change. Release builds also gain
  `lto = true`/`codegen-units = 1`. On a real CoreMark 1.0 run, this
  took the interpreter from 105.9 to 198.2 iterations/sec (~1.9x) and
  `--jit` from 445.1 to 537.1 (~1.2x) — see the [CLI Reference](CLI-Reference.md#-jit-no-jit)'s
  `--jit` note for the full comparison table, including `vamos`'s
  270.6 on the same binary.

## Unreleased (0.1, in development)

- **Core runtime**: CPU + A-line trap dispatch plumbing over the
  [`m68k`](https://crates.io/crates/m68k) crate, real guest heap and
  stack regions (with overflow detection), a configurable total guest
  address space (`--ram`, default 16 MiB) with a clean upfront error
  if `--stack` doesn't leave it room, a host-backed volume/assign
  filesystem (`-V`/`-a`/`--auto-assign`, multi-assign search order,
  real Amiga path semantics including `/`-as-parent-dir), and
  [config files](Configuration.md) (`~/.volamos`/`.volamos`) supplying
  default flag values for repeated-use projects.
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
