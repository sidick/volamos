# Building from Source

```sh
git clone https://github.com/sidick/volamos.git
cd volamos
```

## Building the CLI

```sh
cargo build --release
```

produces `target/release/volamos` — a single self-contained binary, no
runtime dependencies beyond libc. See [Installation](Installation.md)
for putting it on your `PATH`.

For local development, `cargo run -p volamos -- <args>` is the quicker
edit-compile-run loop instead of a separate build step.

## Running the test suite

```sh
cargo test --all
```

Runs every unit and end-to-end test across both crates — no Amiga
toolchain or emulator needed, since the test fixtures (`fixtures/`) are
pre-built AmigaOS hunk executables checked into the repository.

Regenerating a fixture (if you're changing `fixtures/*.s` or its
Python generator) doesn't need `vasm` or a cross-compiler either —
each fixture has a matching `fixtures/gen_*.py` script built on a
small, purpose-built two-pass assembler
(`fixtures/amiga_asm.py`) that hand-assembles the exact bytes:

```sh
python3 fixtures/gen_hello.py   # regenerates fixtures/hello
```

## Linting and formatting

Both are enforced clean before every commit in this project's own
workflow:

```sh
cargo clippy --all-targets
cargo fmt --all -- --check
```

## Building this documentation

The MkDocs site (this page included) lives in `userdocs/`:

```sh
pip install mkdocs-material mike
mkdocs serve
```

## Cross-platform notes

volamos is pure Rust with no platform-specific dependencies beyond a
`SIGINT`/`SIGTERM` -> guest-signal handler that's already conditionally
compiled out on non-Unix targets (`#[cfg(unix)]`, with a documented
no-op fallback elsewhere). macOS and Linux are the two officially
supported and CI-tested platforms; Windows isn't CI-tested, but the
build itself is confirmed real — `cargo build --release` cross-compiled
cleanly to `x86_64-pc-windows-gnu` and produced a genuine Windows PE32+
executable. Running it under Windows/Wine hasn't been verified, so
treat a Windows build as untested at runtime even though it compiles.

### Building on Windows directly

```sh
cargo build --release
```

produces `target\release\volamos.exe`, same as any other platform —
no Windows-specific setup beyond a working Rust toolchain (MSVC or GNU)
via [rustup](https://rustup.rs).

### Cross-compiling for Windows from macOS or Linux

Without a Windows machine, the GNU target cross-compiles cleanly using
[mingw-w64](https://www.mingw-w64.org/) as the linker (the MSVC target,
`x86_64-pc-windows-msvc`, needs the real MSVC linker and can't be
cross-compiled from macOS/Linux):

```sh
rustup target add x86_64-pc-windows-gnu

# macOS (Homebrew):
brew install mingw-w64
# Debian/Ubuntu:
sudo apt install mingw-w64

cargo build --release --target x86_64-pc-windows-gnu -p volamos
```

produces `target/x86_64-pc-windows-gnu/release/volamos.exe`.

## Project structure

- `crates/volamos-core` — library crate: the CPU/memory abstractions
  and every `exec.library`/`dos.library`/`utility.library`/
  `locale.library`/`intuition.library`/math-library implementation.
- `crates/volamos` — binary crate: the `volamos` CLI itself (argument
  parsing, volume/assign setup, the nested-run callback for `System()`/
  `Execute()`/`RunCommand`).
- `fixtures/` — pre-built AmigaOS test programs plus their `.s`
  source and Python generators.
- `docs/plan.md` — developer design notes and the full project history
  (not part of this user-facing documentation site).

See [`docs/plan.md`](https://github.com/sidick/volamos/blob/main/docs/plan.md)
in the repository for the full phase-by-phase build history, design
decisions, and licensing analysis behind the current architecture, if
you're contributing code rather than just using the tool.
