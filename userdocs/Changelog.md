# Changelog

This page tracks major milestones during development, following the
version scheme in `Cargo.toml`.

## 0.9

- **`exec.library`'s `SetFunction`** — patches a single library
  jump-table entry with a real `JMP` to guest code and returns a
  function pointer the caller can chain through to the original
  behavior, matching real `SetFunction` semantics. Previously
  deliberately out of scope (see `docs/plan.md`'s 2026-08-19 gap audit)
  on the assumption it would need a "call back into guest code"
  primitive this runtime didn't have; it turns out not to need one —
  the entry just needs to stop being a host trap and become a real
  `JMP`, which the `m68k` backend already executes as ordinary guest
  code, the same trick `crate::execlib::make_library`'s `MakeLibrary`
  already uses for a genuinely `LoadSeg`ed library's own vectors. The
  "old function" returned for a first-ever patch is a synthesized stub
  (a verbatim copy of the entry's previous bytes) rather than a bare
  sentinel, so a guest hook that calls through it to get default
  behavior — the common real-world `SetFunction` idiom — works, not
  just a bare patch/restore pair. Doesn't yet support patching an
  auto-created fake (vamos-escape-hatch) library's jump table, which
  lacks the real 6-byte-per-vector spacing a `JMP abs.l` needs. A patch
  only affects the single guest run that installed it — there's no
  persistence across separate `volamos` invocations or shared
  system-wide library state, so this is for testing a program's own
  hooking logic against itself, not emulating a real system-wide patch
  tool.

- **`--sanitize` exits `99` on a violation, and no longer misreports a
  `longjmp` return as a stack-smash** (issue #118). A `longjmp` returns
  through its `setjmp` slot, which a later call's frame may still
  occupy if nothing unwound it first — the check now recognizes a
  return landing on a reused slot instead of reporting it. Separately,
  a run that logged any violation now exits with status `99` instead
  of the guest's own exit code, so a harness that only checks the exit
  status doesn't pass a run that corrupted memory; a program started
  with `System()`/`Execute()` from inside the run still returns its
  own code to its caller.

- **The current task is now a real CLI process, and a bare `System()`
  command is looked up in `C:`** (issue #117). The task previously had
  `pr_CIS`/`pr_COS`/`pr_ConsoleTask` all zero, so startup code reading
  them directly instead of calling `Input()`/`Output()` — DICE's does —
  ran with no stdin, stdout or stderr; `SelectInput`/`SelectOutput` now
  keep those fields in step. Separately, a `System()`/`Execute()`
  command with no volume was resolved only against the current
  directory, but the real shell also searches `C:` — SAS/C's `sc`
  invokes its linker as `slink WITH ...` and failed on exactly this.

- **Dispatch now recognizes task-struct corruption instead of
  misreporting it as stack bounds.** `check_stack_bounds` reads
  `tc_SPLower`/`tc_SPUpper` fresh from guest memory on every dispatched
  trap, but a guest that overflows its stack without making a single
  library call in between can push `A7` all the way down through the
  task struct before the next trap gets a chance to fire — by then the
  "bounds" it reads are stack debris, not real bounds, and the old
  message presented that debris as the task's stack and suggested a
  larger `--stack`, a knob that can't help. Inverted bounds
  (`lower > upper`) are unambiguous: no runtime code path ever produces
  them. The diagnostic now says what actually happened — the task
  struct was overwritten and the overflow predates this call — instead
  of printing garbage numbers as if they meant something. Found
  debugging a Rust/LLVM-m68k guest whose panic handler re-panicked
  forever (an LLVM M68k backend miscompile), descending ~16 MB of stack
  with no trap in between.

- **Loader: support `HUNK_RELRELOC32`** (`0x3FD`) (issue #116). Same
  on-disk shape as `HUNK_RELOC32` (a `uint32` count/hunk-number/offsets
  list), but the arithmetic is PC-relative rather than absolute:
  `mem[loc] += target_hunk_addr - (this_hunk_addr + loc)`. Hit loading
  a real `rustc`-for-m68k-Amiga `-fPIC`-style binary that volamos
  previously rejected outright as an unrecognized hunk type.

- **Math libraries: condition codes extended to the basic arithmetic
  functions** (issue #113). Follows up #112's `SPCmp`/`SPTst` fix:
  verified against real Kickstart 3.1 (40.68) via Copperline that
  mathffp.library's `SPAdd`/`SPSub`/`SPMul`/`SPDiv`/`SPAbs`/`SPNeg`/
  `SPCeil`/`SPFloor` also set `N`/`Z` to match the result's actual
  sign/zero-ness, not just the compare/test functions. Added real
  regression coverage that branches on the condition codes directly
  (SAS/C's own idiom), since the existing tests only ever checked `D0`.
  IEEE single-precision arithmetic's flags were left alone pending
  follow-up — probing them on real hardware hit an unexplained crash
  partway through verification.

- **Math libraries: `Cmp`/`Tst` now set the condition codes** (issue
  #112). `IEEEDPCmp`, `IEEEDPTst`, `IEEESPCmp`, `IEEESPTst`, `SPCmp` and
  `SPTst` returned their result in `D0` only, but the real ROM also
  leaves it in the condition codes — and SAS/C's `scmieee.lib` branches
  on those without looking at `D0`, so every double comparison in a
  SAS/C program read as equal and `exp()` returned `HUGE_VAL` from its
  overflow check.

- **`--clock-mhz` reports native-handler call counts at exit**
  (issue #109). Native library handlers (`CopyMem`, `Write`, ...) run in
  zero emulated cycles — a documented limitation — but a benchmark had
  no way to see how much of its work vanished that way. A third report
  line now itemizes it: total native calls, the top handlers by count,
  and the bytes `CopyMem`/`CopyMemQuick` moved. Prompted by a real
  GCC-codegen benchmark (AmigaPorts/m68k-amigaos-gcc#89) where a memcpy
  test "improved" 49% under volamos against 5% on real hardware —
  exactly the signature of copies disappearing into a zero-cycle native
  handler, and now diagnosable from volamos's own output.

- **CLI argument parsing migrated to clap** (issue #101). User-visible
  improvements:
    - `volamos --version` works (long form only — `-V` stays
      `--volume`), prints to stdout, so `V=$(volamos --version)` does
      what you'd expect. Previously there was no version flag at all and
      the token was treated as a program path.
    - A mistyped flag before `<program>` is now a clean error with a
      did-you-mean suggestion (exit code `2`) instead of surfacing as
      `couldn't read '--sanitze': No such file or directory`. A program
      file genuinely named with a leading `-` is still expressible as
      `./-foo` or `-- -foo`.
    - `--flag=value` spellings now parse: `--clock-mhz=25` (a reported
      failure), `--stack=256K`, and friends previously fell through to
      the catch-all and were treated as the program name.
    - `--help` output is generated from the flag declarations themselves
      (so it can't drift), grouped into Logging / Filesystem / Machine /
      Execution / Instrumentation / Networking sections.

  Everything after `<program>` still reaches the guest verbatim, even
  tokens spelled like volamos's own flags; config-file precedence is
  unchanged.

    **Breaking** — command lines that worked before and now behave
    differently:

    - A program path given bare with a leading `-` (`volamos -foo`) is
      now an unknown-flag error; spell it `./-foo` or `-- -foo`. (A
      deliberate trade: every *mistyped flag* used to be silently tried
      as a program path instead of being diagnosed.)
    - A command-line parse error exits `2` (the conventional usage-error
      code) instead of `1`; `1` still means the parse succeeded but the
      run failed. Scripts testing for the specific value `1` on a bad
      invocation need updating.
    - `--help` prints to stdout (and still exits `0`); it used to print
      to stderr.
    - Diagnostic wording for parse errors is clap's, so anything
      matching the old exact stderr text needs updating.

- **`--clock-mhz` reports instruction and bus-access counts alongside
  emulated cycles** (issue #107). A second stderr line at exit —
  instructions executed, bus accesses split into read/write, and the
  derived cycles-per-instruction and accesses-per-instruction ratios —
  makes a run's memory intensity visible from the run itself. volamos
  bills no bus wait states, so its emulated time is close to real
  hardware for arithmetic-bound code and very optimistic for bus-bound
  code — measured between 1.02x and 25x against cycle-paced hardware on
  real benchmarks (issue #105), and accesses-per-instruction is the
  number that says which end of that range a given workload sits at.
  `read` includes instruction fetch, so its floor sits a little above
  1.0; `write` carries no fetch component and is the cleaner signal.

- **`--clock-mhz` reports total emulated cycles at exit** (issue #104).
  Previously the cycle counter was only reachable indirectly — a guest
  had to call `ReadEClock` and report its own elapsed time, which works
  for an instrumented benchmark but leaves a plain binary unmeasurable,
  where `vamos -v` has printed an equivalent "total cycles:" line all
  along. The new exit line goes to stderr (so a harness parsing the
  guest's own stdout never sees it) and prints even when the run ends
  in an error. Cross-checked against vamos on the same binary: within
  0.3%–1.0% agreement between the `m68k` crate's timing tables and
  Musashi's, two independently-implemented cycle models.

- **Added `--clock-mhz`: emulated time from `ReadEClock`** (issue
  #102). Makes `timer.device`'s `ReadEClock` report time derived from
  the CPU's own emulated m68k cycle count at a configurable rate,
  instead of host wall-clock time, so compiler A/B benchmarks under
  volamos are reproducible and independent of host load. Errors
  cleanly when combined with `--sanitize` (the cycle-counted run path
  never calls the sanitizer's per-instruction hooks, which would leave
  its shadow tracking silently stale) or an explicit `--jit` (the trace
  JIT never tracks a cycle count). `GetSysTime`/`TR_GETSYSTIME`/
  `DateStamp`/`CurrentTime` are untouched; real hardware derives the
  E-Clock from chipset timing, not the CPU clock, so `D0` always
  reports `ECLOCK_PAL_HZ` regardless of the configured rate.

- **`--sanitize` no longer reports a recycled heap block as
  use-after-free** (issue #95, reported by Bernie Innocenti). Once a run
  freed a block and the heap handed those same addresses back out, every
  write into the *new* allocation was reported against the *old* one's
  poison — one violation per byte, which slowed a BenchWork run from 10
  seconds to 6 minutes and looked like a hang.

  The clearing now happens inside the guest heap, at the single point
  where a block leaves its control, and the allocator takes the address
  space as a parameter so a new allocation site cannot compile without
  it. That matters because `AllocMem`/`AllocVec`/`AllocPooled` are only
  3 of some four dozen places that carve guest structures out of that
  heap — the `FileHandle` a `dos` `Open` returns, the `FileLock` a `Lock`
  returns, a `ReadArgs` argument buffer and many more — and any of them
  can be handed a poisoned block. Fixing only the exec allocators left a
  guest that freed a large block and then opened a file still reporting
  46 phantom use-after-free writes.

  Separately, `--sanitize`'s slowdown on a violation-heavy run is gone:
  the per-byte access check was deciding "did that byte report?" by
  summing the hit counts of every violation already logged, so the cost
  grew with the log. A 400,000-violation run goes from 1.33s to 0.03s,
  with identical output.

- **`--ram` above 16 MiB is now refused on a 68000/68010** (issue #98)
  instead of producing a program that dies confusingly later. Those CPUs
  have a 24-bit address bus, and the guest stack sits at the top of the
  address space, so `--ram 32M` on the default `--cpu 68000` put the
  stack pointer at an address the CPU cannot express: it wrapped, a
  `JSR` into a library pushed its return address into low memory, and
  the `RTS` popped whatever was there — surfacing several calls later as
  "continuation stub trapped at 0x000000c4 with no pending continuation",
  which points at nothing useful. It now fails up front naming both
  flags, the way an oversized `--stack` already did. Pass `--cpu 68020`
  (or later) for a 32-bit address bus. The default `--ram` is exactly
  16 MiB, so only a raised `--ram` was ever affected.

- **`pr_Arguments` is now populated on the fake `struct Process`.** `A0`/`D0`
  carry the command-line buffer at process entry, but real AmigaOS exposes
  the same string again through `pr_Arguments`, and volamos left that field
  permanently `NULL`. Found running the real
  [sidick/micropython](https://github.com/sidick/micropython) Amiga port,
  whose startup reads `argv` from `pr_Arguments` rather than `A0`/`D0`: it
  saw "no arguments" and dropped into its REPL even when a script path was
  passed.

- **`AllocDosObject`/`FreeDosObject` now support `DOS_FIB`** — a zeroed
  `struct FileInfoBlock`, the same shape as the already-supported
  `DOS_RDARGS` and `DOS_EXALLCONTROL`. Also found via the micropython port,
  whose `os.walk()` allocates its own `FileInfoBlock` this way instead of
  using `ExAll`.

- **`SetVar(name, NULL, ...)` on a variable that does not exist now succeeds
  silently**, as real `SetVar` does — and so does `DeleteVar`, which is
  implemented in terms of it. Both the local-variable path and the
  `ENV:`-backed global one were returning `ERROR_OBJECT_NOT_FOUND`. Verified
  against real Kickstart.

- **`OpenLibrary` now ignores garbage in the high word of the requested
  version**, matching the `CMP.W` real ROMs use: a caller passing
  `$30000000` in `D0` gets the library, where volamos previously compared
  all 32 bits and refused. Found via a recent AROS fix ("Frontier 2") that
  identified the same divergence in AROS; volamos had been reproducing the
  bug rather than the hardware. `D0` itself is left untouched for the `Open`
  vector, as on real hardware.

## 0.7

- **The heap detectors now cover four more allocators** (issue #83,
  tier 0): `utility.library`'s `AllocateTagItems`, `dos.library`'s
  `AllocDosObject`, and `exec.library`'s `CreateIORequest` and
  `CreateMsgPort`, plus their matching free calls. All four carve from
  the same guest heap as `AllocMem`, so redzone *space* was already
  being reserved for them whenever `--sanitize` was on — only the
  shadow marking was missing, which meant a guest overrunning a
  `FileInfoBlock`, an `RDArgs`, a `MsgPort` or a `TagItem` array that
  volamos handed it went unreported.

  It found a real bug on the first sweep: the PhxAss assembler asks
  `CreateIORequest` for 40 bytes (`sizeof(struct timerequest)`) and then
  reads two bytes one past the end. Harmless on real hardware, where
  that read lands in whatever follows on the heap, which is exactly the
  class of latent bug this exists to surface. So **PhxAss is no longer
  silent under `--sanitize`** — that one report is expected;
  `--sanitize-ignore-pc` silences it if you want a clean baseline.

  Still not covered: `exec.library/Allocate`, where the guest owns the
  memory pool, and which is where a C runtime's `malloc` actually
  sub-allocates.

- **Diagnostics now name the source location** (issue #74). Sanitizer
  violations are annotated with `file:line` when the program carries a
  `HUNK_DEBUG` `LINE` block (SAS/C's `DEBUG=LINE`, PhxAss's
  `LINEDEBUG`), falling back to `symbol+offset` from `HUNK_SYMBOL`
  otherwise, and to the bare address when a binary carries neither. The
  raw PC is always kept alongside, since that is what a disassembly
  needs.

  Worth knowing what this does and doesn't reach: m68k-amigaos-gcc
  emits *stabs* debug info, which isn't decoded, so `-g` alone doesn't
  give `file:line` — but gcc binaries do carry symbols, so a real
  gcc-built stack-smash now reports
  `expected 0x00002b40, found 0x41414141 ... (at ___main+0x3c)`.
  `static` functions never appear in a symbol table, so attribution
  inside one falls to the nearest exported symbol.

  Also new: `fixtures/linetest`, a repo-owned binary carrying real
  `LINE` data (PhxAss-built, since `amiga_asm.py` can't emit debug
  hunks), so the parser's tests don't depend on artefacts that vanish.

- **Added `--dirty-heap`** (issue #80): fills every allocation made
  without `MEMF_CLEAR` with `0xA5` instead of leaving it zeroed, so a
  guest that relies on uncleared memory being zero fails here the way it
  can on real hardware — where `AllocMem` returns whatever debris was
  there. `MEMF_CLEAR` allocations are untouched. Deliberately
  independent of `--sanitize`, because this one changes what the guest
  sees rather than only observing it, and `--sanitize`'s
  never-perturb-the-program property is worth protecting (re-verified:
  the real SAS/C compiler's output object file is still byte-identical
  under `--sanitize`).

  This also settled a verdict `--sanitize-uninit` had to leave open: the
  real PhxAss assembler reads an uninitialized field from a ~568-slot
  table, and with the fill on its output is byte-identical (checked on a
  60-symbol source as well as a trivial one), so it does not act on
  those values. Benign, not a latent bug.

- **Added `--sanitize-uninit`** (issue #68): opt-in uninitialized-read
  detection on top of `--sanitize`, plus `--sanitize-ignore-pc` for
  silencing a site you have already triaged. Byte-granular, so a
  partially-initialized structure is caught rather than being treated as
  initialized because something in it was written; `MEMF_CLEAR`
  allocations never report.

  Getting the false-positive rate usable was the bulk of the work, and
  it turned on two latent bugs that were harmless only because uninit
  reporting was off: a below-stack-pointer write forgiven by the grace
  band returned without healing the byte, so the value a `JSR` had just
  pushed read back as never-written; and stack growth blanket-marked the
  newly-exposed range uninitialized, clobbering the bytes the very
  instruction that moved the stack pointer had just written (a push
  writes as it decrements). Together those took real pLhA listing a
  102-file archive from **109,204** reports to **zero**, and every
  fixture to zero.

  Violation reports now also group by PC: a site with many violations
  collapses to one line with a count and address range, while a site
  with few prints each violation in full — so a corrupted return
  address never loses its expected/actual pair, which is its entire
  diagnostic value. Real PhxAss goes from a wall of 574 lines to 6
  legible sites, one of which accounts for 568 of them.

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
