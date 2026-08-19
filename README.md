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
system, or any Amiga hardware.

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
more) plus real third-party binaries (the PhxAss assembler, and
Simon's own AmiSnap project) has closed many further gaps beyond
Phase 3's original scope, and a full audit against
[`vamos`](https://github.com/cnvogelg/amitools)'s own library/device
coverage closed the remaining ones it flagged. Phase 4 (the
three-oracle parity harness against `vamos`/real Kickstart) hasn't
formally started yet.

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
  address space (`--stack`/`--ram`, with overflow detection). Run
  `cargo run -p volamos -- --help` for the full CLI surface.

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
