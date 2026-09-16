# Changelog

This page tracks major milestones during development, following the
version scheme in `Cargo.toml`.

## 0.6

- **Added `--sanitize`, a valgrind/ASan-style memory sanitizer**
  (issue #65). `m68k-amigaos-gcc` has no `-fsanitize=address`, and
  MMU-based tools like MuForce are page-granular, so single-byte heap
  overruns and use-after-free went undetected. volamos can do better
  because it *is* the allocator and every guest access already funnels
  through one trait: a shadow byte per guest byte now backs poisoned
  redzones either side of every `AllocMem`/`AllocVec`/`AllocPooled`
  block (including the alignment padding), plus a free quarantine that
  keeps a freed address out of circulation so use-after-free can't hide
  behind an address nothing happened to reuse. Host-side library
  handlers write guest memory through the same checked path, so bad
  buffers passed to `dos.library` calls are caught with no
  per-function instrumentation. Violations are reported to stderr,
  deduplicated with hit counts, and never abort the guest -- a detector,
  not an enforcer. Two things worth knowing: `--sanitize` forces the
  JIT off, because its raw-pointer fast path bypasses the checks
  entirely (a sanitized JIT run would look clean no matter what the
  program did); and overflows *within* a single stack frame remain
  invisible, exactly as they are to valgrind, since catching those needs
  compiler instrumentation. New `fixtures/memtest` exercises all of it,
  and is the first fixture whose `.s` is assembled by the real PhxAss
  running under volamos itself.

- **Extended `--sanitize` to stack bugs** (issue #65, increment 2):
  accesses below the stack pointer, and return-address corruption via a
  shadow call stack that records what each `JSR`/`BSR` pushed and
  verifies it at the matching return — stack-smash detection valgrind
  itself doesn't offer. Getting this to zero false positives on real
  software was most of the work: a push writes below the stack pointer
  by definition, so a 64-byte grace band (sized to `MOVEM`'s worst case,
  the same window valgrind forgives) is needed or every subroutine call
  reports; volamos performs library-call returns itself rather than
  executing an `RTS`, so `dispatch` has to retire shadow frames
  explicitly or a stale frame sits exactly where the next push lands;
  and `StackSwap` abandons a whole stack, so both the stale poison and
  the pending call frames have to be cleared or the replacement stack's
  reused addresses produce bogus reports. Real PhxAss, real pLhA and the
  real SAS/C 6.58 compiler now all run clean, with `sc`'s object file
  byte-identical to an unsanitized run's. New `fixtures/stacktest`
  covers both detectors plus three false-positive guards.

## 0.5

- **Fixed the guest command-line buffer's missing trailing space**
  (issue #63): whenever a launched program has at least one argument,
  real AmigaOS's own command-line buffer carries a trailing space
  before the final `'\n'` (`"foo bar baz \n"`, not `"foo bar
  baz\n"`) -- confirmed directly against real Kickstart 2.0/3.0/3.1
  hardware via a new local, real-Kickstart comparison harness
  (`tools/compare_kickstart_versions.py`, using Copperline's
  `copperhf.device`). Kickstart 3.2 alone doesn't add it -- a real,
  intentionally-untouched AmigaOS version difference, not a bug (this
  project targets 3.1 first). Also fixed a real bug found along the
  way in `fixtures/echoargs.s`/`libcall.s`: `A0` is a scratch register
  across any library call (real Kickstart's `OpenLibrary` clobbers it;
  volamos's own doesn't), so reading the command-line pointer back from
  `A0` *after* an `OpenLibrary` call is real-hardware-unsafe even
  though it happened to work under volamos.
- **Implemented `mathieeesingbas.library`/`mathieeesingtrans.library`**:
  the single-precision IEEE math libraries, previously only a fake
  `OpenLibrary` stand-in with no real function support. Mirrors
  `mathieeedoubbas.library`/`mathieeedoubtrans.library` function-for-
  function on plain `f32` instead of `f64`. Fixed a real bug found via
  amitools' own `math_single_trans` ground truth along the way:
  `IEEESPPow`'s result is `y` raised to the `x` power (`IEEESPPow(3.0,
  4.0)` -> `64.0` = `4**3`), not `x` raised to `y` as a naive reading
  of its `.conf`-derived argument names would suggest -- tracing this
  down also revealed `mathieeedoubtrans.library`'s existing
  `IEEEDPPow` already had the equivalent (correct) behavior, just a
  misleading doc comment, now corrected.

## 0.4

- **Fixed `mathffp.library`/`mathtrans.library`'s FFP encoding**
  (issue #53): the sign bit and exponent field were in swapped bit
  positions (an old bug hidden by a unit test that re-derived the same
  wrong layout from this module's own -- also wrong -- doc comment
  instead of checking against real hardware), and overflow/underflow/
  domain-error (`NaN`) results weren't saturating correctly. Found via
  a new local comparison harness against amitools' own test corpus
  (`tools/compare_amitools_suite.py`); took the affected tests'
  divergence from `vamos` from the large majority of their output
  lines down to a handful of residual, believed-benign ones (a single
  overflow-boundary edge case and ordinary cross-implementation
  transcendental-function rounding variance). Both since confirmed
  against real Kickstart 3.1 hardware: the overflow-boundary case
  (`SPMul` saturating exactly at FFP's maximum exponent field) was
  correct as fixed, and two more real bugs turned up in the same pass
  -- `RawDoFmt`/`VPrintf`'s `%x`/`%lx` printed lowercase hex where real
  hardware prints uppercase (issue #48), and `IEEEDPCeil()` returned
  `-0.0` for a ceil-to-zero result where real hardware (matching
  `vamos`) returns `+0.0` (issue #51, originally misdiagnosed as
  correct IEEE-754 behavior on volamos's side and `vamos`'s divergence
  -- backwards). Also confirmed that `mathieeedoubbas`/
  `mathieeedoubtrans`'s positive-signed `NaN` convention for
  domain-error results (`0/0`, out-of-domain `acos`/`asin`/`log`/etc.)
  matches real hardware and `vamos`'s negative-signed convention is
  `vamos`'s own divergence (issue #52), and canonicalized an internal
  inconsistency where Rust's own `f64::asin`/`acos` didn't agree with
  themselves on `NaN` sign for symmetric out-of-domain inputs.
- **Fixed `RawDoFmt`'s `%b` (BSTR) format** (issue #45): the data-list
  entry for `%b` is a `BPTR`, not a raw byte address, so it needs the
  same `<< 2` conversion `dos.library`'s own `BPTR`-taking calls
  already apply -- confirmed against real Kickstart 3.1 hardware and
  amitools' own `exec_rawdofmt` test (`BStr: 'Hoi!'`, previously
  garbled).
- **Fixed `Seek()` not rejecting an out-of-range target position**
  (issue #47): a host file's own `seek()` happily allows seeking
  arbitrarily far past end-of-file (standard POSIX behavior), but real
  `Seek()`'s own NDK autodoc says "you cannot Seek() beyond the end of
  a file." The target position is now computed and validated against
  the file's actual length (and against a negative result) before
  touching the host file at all, matching `-1`/`ERROR_SEEK_ERROR`, the
  modern (post-V39) contract -- not the old, documented-as-fixed
  pre-V39 behavior of returning the current position instead, which
  amitools' own `dos_seek` test still (incorrectly, for a V40 target)
  expects since it's a literal `vamos`-captured assertion, not real
  hardware.
- **Implemented `dos.library/FindArg`** (issue #55): finds the
  zero-based slot index a keyword names in a `ReadArgs`-style template
  (or `-1`), reusing the same template parser and `NAME=ABBREV` alias
  handling `ReadArgs` itself already had. Previously an unhandled
  library call.
- **Fixed `AnchorPath`'s `ap_Buf` qualification** (issue #46): confirmed
  via real Kickstart 3.1 hardware (through a genuine in-memory FFS/OFS
  volume synthesized from a host directory, not just a convenience
  boot mount) that `ap_Buf` tracks whatever device/path qualification
  the caller's own `MatchFirst`/`MatchNext` pattern text had -- a
  device-qualified pattern (`"sys:"`) reports device-qualified entries
  (`"sys:c"`); a bare, current-directory-relative pattern with no
  prefix at all reports entries with no qualification either, matching
  `fib_FileName` exactly. volamos previously always stripped `ap_Buf`
  down to a bare relative name regardless (issue #14's own fix, which
  turned out to be treating a symptom rather than the actual
  mechanism -- see #46's closing comment for the full story).
- **Fixed `AnchorPath`'s volume-root `fib_FileName`** (issue #58): a
  `MatchFirst`/`MatchNext` report for a bare volume root now has a
  blank `fib_FileName`, matching real Kickstart 3.1 hardware -- a
  distinct convention from plain `Lock()`/`Examine()` on the same
  volume root, which correctly keeps reporting the volume name.
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
