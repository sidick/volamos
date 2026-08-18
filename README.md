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

## Platform support

volamos targets macOS and Linux equally. Headless and CI use (e.g. running
an original Amiga toolchain as part of a build) is a first-class use case,
not an afterthought — there's no dependency on a display, windowing
system, or any Amiga hardware.

## Status

**Phases 1-3 — complete**: CPU + trap plumbing, `dos.library` file I/O
and volumes/assigns, and `exec.library`/`utility.library` essentials.

The runtime loads an AmigaOS hunk executable, runs it on an interpreted
m68k CPU (the [`m68k`](https://crates.io/crates/m68k) crate behind a
swappable `Cpu` trait), intercepts library calls made the real AmigaOS
way (`OpenLibrary` via `AbsExecBase` at address 4, then `jsr` through the
returned library base) via A-line trap dispatch, services them with
native Rust handlers, and propagates the guest's exit code. Try it:

```sh
cargo run -p volamos -- fixtures/hello
cargo run -p volamos -- -V TEST:/tmp/some-hostdir fixtures/filetest
cargo run -p volamos -- fixtures/echoargs foo bar
cargo run -p volamos -- fixtures/exectest
cargo run -p volamos -- --stack 4096 fixtures/recurse  # demonstrates overflow detection
```

Implemented so far:

- **dos.library**: file I/O (`Open`/`Read`/`Write`/`Seek`/`Close`,
  `Input`/`Output`, `IoErr`/`SetIoErr`), locks and directory traversal
  (`Lock`/`UnLock`/`DupLock`/`Examine`/`ExNext`/`CurrentDir`/`ParentDir`),
  `CheckSignal`, `LoadSeg`/`UnLoadSeg` (real BPTR seglists), and
  `SystemTagList`/`Execute` for tools that shell out.
- **exec.library**: `OpenLibrary`/`CloseLibrary` (unknown libraries get
  an auto-created fake base rather than failing outright, mirroring
  `vamos`), `AllocMem`/`FreeMem`/`AllocVec`/`FreeVec`/`AvailMem` over a
  flat guest heap, real guest-visible List/Node primitives and minimal
  single-threaded message ports, and task/signal basics
  (`FindTask`/`SetSignal`/`Wait`/`Signal`/`AllocSignal`/`FreeSignal`)
  including host `SIGINT`/`SIGTERM` delivered to the guest as
  `SIGBREAKF_CTRL_C`.
- **utility.library**: tag-list handling (`GetTagData`/`NextTagItem`/
  `FindTagItem`), `Stricmp`/`Strnicmp`/`ToUpper`/`ToLower`, and Amiga
  date conversions.
- A host-backed volume/assign filesystem (`-V`/`-a`/`--auto-assign` CLI
  flags, multi-assign search order, Amiga `:`/`/` semantics), a guest
  heap with BPTR/BSTR helpers, and configurable guest stack size
  (`--stack`, with overflow detection). Run `cargo run -p volamos --
  --help` for the full CLI surface.

Math libraries (`mathffp`, `mathieee*`), `ReadArgs`, and the three-oracle
parity harness against `vamos`/real Kickstart are Phase 4+. See
[`docs/plan.md`](docs/plan.md) for the full phase breakdown, the
fd/SFD licensing analysis, and current status.

## Workspace layout

- `crates/volamos-core` — library crate: CPU/memory abstractions, and
  (later) the `exec.library`/`dos.library` implementations.
- `crates/volamos` — binary crate: the `volamos` CLI.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.
