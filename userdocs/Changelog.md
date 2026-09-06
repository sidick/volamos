# Changelog

This page tracks major milestones during development, following the
version scheme in `Cargo.toml`.

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
