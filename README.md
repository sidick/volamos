# volamos

volamos is a Rust-native, Wine-style runtime for classic AmigaOS m68k CLI
binaries. It runs 68k console programs — compilers, assemblers, linkers,
file utilities, and the like — directly on macOS and Linux by emulating the
m68k CPU and reimplementing `exec.library`/`dos.library` calls at the API
boundary on the host OS, rather than emulating an entire Amiga.

There is no full-system emulation and no GUI or custom-chip (graphics,
audio, disk hardware) support: volamos is aimed squarely at running
command-line tools, not games or Workbench applications. It is a spiritual
successor to [`vamos`](https://github.com/cnvogelg/amitools), the Python
implementation of the same idea from the `amitools` package.

**User documentation:** [sidick.github.io/volamos](https://sidick.github.io/volamos/)
(installation, CLI reference, volumes/assigns, supported libraries) is
a full [MkDocs Material](https://squidfunk.github.io/mkdocs-material/)
site built from [`userdocs/`](userdocs/) — or run `mkdocs serve`
locally (see
[`userdocs/Building-from-Source.md`](userdocs/Building-from-Source.md)).
This README stays a shorter overview.

## Platform support

volamos targets macOS and Linux equally. Headless and CI use (e.g. running
an original Amiga toolchain as part of a build) is a first-class use case,
not an afterthought — there's no dependency on a display, windowing
system, or any Amiga hardware. A Windows build compiles cleanly too
(cross-compile confirmed to `x86_64-pc-windows-gnu`, see
[`userdocs/Building-from-Source.md`](userdocs/Building-from-Source.md#cross-platform-notes)),
though it isn't CI-tested and running it under Windows/Wine hasn't been
verified.

## Building

Requires a recent stable Rust toolchain (via [rustup](https://rustup.rs)
or your OS package manager) with edition 2024 support — no other system
dependencies.

```sh
git clone https://github.com/sidick/volamos.git
cd volamos
cargo build --release
```

The binary is at `target/release/volamos`. Run it directly:

```sh
target/release/volamos fixtures/hello
```

To put `volamos` on your `PATH` instead:

```sh
cargo install --path crates/volamos --locked
volamos fixtures/hello
```

(`cargo uninstall volamos` removes it again.) The `cargo run -p volamos
--` form used throughout the rest of this README is the quick
edit-compile-run loop for development — for everyday use, build once
and run the binary directly, as above.

## Status

**Phases 1-3 — complete**: CPU + trap plumbing, `dos.library` file I/O
and volumes/assigns, and `exec.library`/`utility.library` essentials.
Since then, substantial empirical hardening against a real Workbench
3.1.4 `C:` command corpus (`List`, `Copy`, `Delete`, `Rename`, `Sort`,
`Search`, `Join`, `CPU`, `Date`, `SetDate`, `Wait`, `Break`, `Info`, and
more) plus real third-party binaries (the PhxAss assembler, the pLhA
archiver, the SAS/C 6.58 compiler, and Simon's own AmiSnap project) has
closed many further gaps beyond Phase 3's original scope, and a full
audit against [`vamos`](https://github.com/cnvogelg/amitools)'s own
library/device coverage closed the remaining ones it flagged.

A three-oracle comparison harness (`tools/compare_three_way.py`) runs
the same binaries under volamos, `vamos` and a **real Kickstart** via
[Copperline](https://github.com/sidick/copperline), booting a real
Workbench 3.1.4 filesystem and reading the guest's own redirected
output back off the host. It's local-only — it needs a real ROM and
real Workbench media, neither of which can live in this repo — and it
takes a `--model` matching the ROM you give it. Where it and `vamos`
disagree with volamos, the differences that turned out to be `vamos`'s
are documented in
[Differences from vamos](userdocs/Differences-from-vamos.md), each one
verified against real hardware.

The runtime loads an AmigaOS hunk executable, runs it on an interpreted
m68k CPU (the [`m68k`](https://crates.io/crates/m68k) crate behind a
swappable `Cpu` trait, with `--cpu`/`--fpu` to pick the emulated model),
intercepts library calls made the real AmigaOS way (`OpenLibrary` via
`AbsExecBase` at address 4, then `jsr` through the returned library
base) via A-line trap dispatch, services them with native Rust
handlers, and propagates the guest's exit code. Try it:

```sh
cargo run -p volamos -- fixtures/hello
cargo run -p volamos -- -V TEST:/tmp/some-hostdir fixtures/filetest
cargo run -p volamos -- fixtures/echoargs foo bar
cargo run -p volamos -- fixtures/exectest
cargo run -p volamos -- --stack 4096 fixtures/recurse  # demonstrates overflow detection
cargo run -p volamos -- -V TEST:/tmp/some-hostdir fixtures/runcmdtest  # LoadSeg+RunCommand+UnLoadSeg
cargo run -p volamos -- --sanitize fixtures/memtest overrun  # catches a 1-byte heap overrun
cargo run -p volamos -- --sanitize fixtures/stacktest smash  # catches a smashed return address
```

Implemented so far:

- **dos.library**: file I/O (`Open`/`Read`/`Write`/`Seek`/`Close`,
  `Input`/`Output`, `IoErr`/`SetIoErr`), locks and directory traversal
  (`Lock`/`UnLock`/`DupLock`/`Examine`/`ExNext`/`CurrentDir`/`ParentDir`),
  pattern matching (`MatchFirst`/`MatchNext`/`ParsePattern`),
  `ReadArgs`/`FreeArgs`, environment variables (`GetVar`/`SetVar` over a
  real `ENV:` volume), date/time (`StrToDate`/`DateToStr`/`DateStamp`),
  process/CLI bits (`Cli`/`GetProgramName`/`MaxCli`/`AllocDosObject`),
  the `DosList` (`LockDosList`/`FindDosEntry`/`Info`), `CheckSignal`,
  `LoadSeg`/`UnLoadSeg` (real BPTR seglists), `RunCommand`, and
  `SystemTagList`/`Execute` for tools that shell out.
- **exec.library**: `OpenLibrary`/`CloseLibrary` (unknown libraries get
  an auto-created fake base rather than failing outright, mirroring
  `vamos`), memory (`AllocMem`/`FreeMem`/`AllocVec`/`FreeVec`/`AvailMem`/
  memory pools/`Allocate`/`Deallocate` over a real `MemHeader`/
  `MemChunk` free list), real guest-visible List/Node primitives and
  minimal single-threaded message ports, task/signal basics
  (`FindTask`/`SetSignal`/`Wait`/`Signal`/`AllocSignal`/`FreeSignal`,
  including host `SIGINT`/`SIGTERM` delivered as `SIGBREAKF_CTRL_C`),
  the full `SignalSemaphore` API (`InitSemaphore`/`Obtain`/`Release`/
  `Attempt`/`Find`/`Add`/`Rem`/`ObtainSemaphoreList`/
  `ReleaseSemaphoreList`), `Alert`, `RawDoFmt`, and CPU-detection
  plumbing (`AttnFlags`, `CacheControl`, `Supervisor`).
- **utility.library**: tag-list handling (`GetTagData`/`NextTagItem`/
  `FindTagItem`/`AllocateTagItems`/`FreeTagItems`), `Stricmp`/
  `Strnicmp`/`ToUpper`/`ToLower`, 32-bit math helpers, and Amiga date
  conversions.
- **locale.library**: character classification (`IsAlpha`/`IsDigit`/
  etc.), case conversion, locale-aware `StrnCmp`, and a minimal
  `OpenLocale`/`CloseLocale` — matching `vamos`'s own scope, not a real
  multi-locale/catalog system.
- **intuition.library**: a thin stub (`DisplayAlert`/`AutoRequest`/
  `EasyRequestArgs`/`CurrentTime`) — just enough that a console tool's
  stray Intuition call doesn't crash, no real windowing/GUI.
- **Math libraries**: `mathffp`, `mathtrans`, `mathieeedoubbas`,
  `mathieeedoubtrans` — real FFP/IEEE arithmetic, not fake traps.
- **timer.device**: real time-arithmetic (`AddTime`/`SubTime`/
  `CmpTime`/`ReadEClock`/`GetSysTime`) via the documented
  `io_Device`-as-library-base idiom.
- A host-backed volume/assign filesystem (`-V`/`-a`/`--auto-assign` CLI
  flags, multi-assign search order, Amiga `:`/`/` semantics), `.uaem`
  sidecar metadata for protection bits/comments, a guest heap with
  BPTR/BSTR helpers, and configurable guest stack size and total
  address space (`--stack`/`--ram`, with overflow detection). A
  `~/.volamos`/`.volamos` config file can supply default values for
  any of the above, for repeated-use projects. Run `cargo run -p
  volamos -- --help` for the full CLI surface.

## Finding bugs in guest programs

`m68k-amigaos-gcc` has no `-fsanitize=address`, and MMU-based tools like
Enforcer and MuForce work at MMU-page granularity, so a one-byte overrun
or a read just past the end of an allocation is invisible to them.
`--sanitize` gives volamos a valgrind/ASan-style detector instead, which
it can do cheaply because it *is* the allocator and every guest memory
access already funnels through one place.

A classic stack buffer overflow in a real C program, built with
`m68k-amigaos-gcc -g` and run under `--sanitize`:

```console
$ volamos --sanitize smash
sanitizer: 1 site(s), 1 violation(s):
  return address corrupted at stack slot 0x00ffffe0:
    expected 0x00002b40, found 0x41414141 from PC 0x00002b10 (at ___main+0x3c)
```

`found 0x41414141` is the ASCII `AAAA` that overflowed the buffer, and
`___main+0x3c` comes from the binary's own symbol table. What it detects:

- **Heap overruns and underruns**, either direction, down to a single
  byte — poisoned redzones either side of every `AllocMem`/`AllocVec`/
  `AllocPooled` block, including the padding between the size a program
  asked for and the size it actually got.
- **Use-after-free**, via a free quarantine that keeps a freed address
  out of circulation, so the bug can't hide behind an address nothing
  happened to reuse yet.
- **Accesses below the stack pointer**, and **return-address
  corruption** through a shadow call stack — stack-smash detection,
  which valgrind itself doesn't offer.
- **Bad buffers handed to `dos.library` calls**, for free: host-side
  handlers write guest memory through the same checked path, so no
  per-function instrumentation was needed.
- **Uninitialized reads**, byte-granular, behind the opt-in
  `--sanitize-uninit` (a partially-initialized structure is caught, not
  waved through because something in it was written).

Violations are grouped by PC, annotated with `file:line` when the
binary carries `HUNK_DEBUG` source-line info (SAS/C's `DEBUG=LINE`,
PhxAss's `LINEDEBUG`) and with `symbol+offset` otherwise, and never
abort the guest — it's a detector, not an enforcer, so one run surfaces
every bug rather than dying at the first.

`--dirty-heap` complements it by filling every non-`MEMF_CLEAR`
allocation with `0xA5` instead of the zeros volamos's memory happens to
start as, so a program that relies on uncleared memory being zero fails
here the way it can on real hardware.

Real PhxAss, real pLhA and the real SAS/C 6.58 compiler all run under
`--sanitize` with `sc`'s output object file byte-identical to an
unsanitized run — the detector doesn't perturb what it watches. (PhxAss
does report one genuine two-byte over-read of its own `timerequest`,
found this way.) Two things it can't see: overflows *within* a single
stack frame, which need compiler instrumentation and are invisible to
valgrind too; and `malloc` inside a C runtime's own pool, which
sub-allocates via `exec.library/Allocate` rather than through the
allocators volamos guards. See the
[CLI reference](https://sidick.github.io/volamos/latest/CLI-Reference/)
for the full details.

The three-oracle parity harness against `vamos`/real Kickstart is
Phase 4+. See [`docs/plan.md`](docs/plan.md) for the full phase
breakdown, the fd/SFD licensing analysis, and current status.

## Workspace layout

- `crates/volamos-core` — library crate: CPU/memory abstractions and
  the `exec.library`/`dos.library`/`utility.library`/`locale.library`/
  `intuition.library`/math-library implementations.
- `crates/volamos` — binary crate: the `volamos` CLI.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.
