# Installation

## Requirements

- **macOS or Linux.** These are the two officially supported and
  CI-tested host platforms — see [the index page](index.md). A Windows
  build is possible too — `cargo build --release` cross-compiles
  cleanly to a real Windows executable (see
  [Building from Source](Building-from-Source.md#cross-platform-notes))
  — but running it under Windows/Wine hasn't been verified, so treat
  it as untested at runtime even though the build itself works.
- **A recent stable Rust toolchain** (edition 2024 support) to build
  it — see [Building from Source](Building-from-Source.md). volamos
  has not yet had a tagged release, so building from source is
  currently the only way to get the binary (see
  [the index page](index.md)'s note on project status).
- **No Amiga hardware, ROM image, or Workbench disk of any kind.**
  volamos doesn't emulate a whole machine — it loads and runs a single
  AmigaOS hunk executable directly. The guest program itself is
  whatever compiler/assembler/tool you want to run; volamos doesn't
  ship one.

## Getting the binary

Until a tagged release exists, build it yourself:

```sh
git clone https://github.com/sidick/volamos.git
cd volamos
cargo build --release
```

The binary is at `target/release/volamos`, a single self-contained
executable — copy it anywhere on your `PATH`, or install it directly
with Cargo:

```sh
cargo install --path crates/volamos --locked
```

See [Building from Source](Building-from-Source.md) for the full
details (running the test suite, cross-platform notes).

## Checking it works

volamos ships a handful of pre-built test fixtures — tiny AmigaOS
executables used by its own test suite — that double as a quick sanity
check:

```console
$ volamos fixtures/hello
Hello from volamos
```

If that prints the greeting, volamos is running correctly: it loaded a
real AmigaOS hunk executable, executed real 68k instructions on the
emulated CPU, dispatched its `dos.library` `PutStr` call, and
propagated its exit code (`0`) back to your shell.

```sh
echo $?   # 0
```

## Next steps

- [Getting Started](Getting-Started.md) — run a guest program that
  actually touches the filesystem, by mapping a host directory onto an
  Amiga volume.
- [CLI Reference](CLI-Reference.md) — every flag `volamos` accepts.
