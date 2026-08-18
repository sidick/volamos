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

**Phase 1: CPU + trap plumbing — complete.**

The runtime can load a simple AmigaOS hunk executable, run it on an
interpreted m68k CPU (the [`m68k`](https://crates.io/crates/m68k) crate
behind a swappable `Cpu` trait), intercept a library call made the real
AmigaOS way (`jsr _LVOPutStr(a6)` through a fake `dos.library` base) via
A-line trap dispatch, service it with a native Rust handler, and propagate
the guest's exit code. Try it:

```sh
cargo run -p volamos -- fixtures/hello
```

Only one library call (`dos.library/PutStr`) is implemented so far; real
file I/O, volumes/assigns, and the rest of `dos.library`/`exec.library`
are Phase 2+.

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
