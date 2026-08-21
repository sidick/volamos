# Installation

## Requirements

- **macOS or Linux.** These are the two officially supported and
  CI-tested host platforms — see [the index page](index.md). A Windows
  build is possible too — `cargo build --release` cross-compiles
  cleanly to a real Windows executable (see
  [Building from Source](Building-from-Source.md#cross-platform-notes))
  — but running it under Windows/Wine hasn't been verified, so treat
  it as untested at runtime even though the build itself works. A
  prebuilt Windows zip is published on each release (see below) purely
  as a cross-compiled convenience binary, with the same caveat.
- **No Amiga hardware, ROM image, or Workbench disk of any kind.**
  volamos doesn't emulate a whole machine — it loads and runs a single
  AmigaOS hunk executable directly. The guest program itself is
  whatever compiler/assembler/tool you want to run; volamos doesn't
  ship one.

## Getting the binary

Pick whichever of these fits your workflow:

### Prebuilt release binary

Each tagged release publishes standalone, statically-linked binaries
for `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`aarch64-apple-darwin` (Apple Silicon), and `x86_64-pc-windows-gnu` —
download the archive for your platform from the
[GitHub Releases page](https://github.com/sidick/volamos/releases),
extract it, and put `volamos` on your `PATH`. No Rust toolchain or
build step required. Intel Mac (`x86_64-apple-darwin`) isn't published
— build from source instead if you need it.

### Container image

A multi-arch (`linux/amd64`, `linux/arm64`) image is published to
GHCR on every release:

```sh
docker run --rm ghcr.io/sidick/volamos:latest --help
```

or pin a specific version instead of `latest` (e.g.
`ghcr.io/sidick/volamos:v0.1`). It's a static binary on a
[distroless](https://github.com/GoogleContainerTools/distroless)
`nonroot` base — no shell, no libc, runs as an unprivileged user by
default. To run a guest program against host files, bind-mount a
directory and map it as a volume with `-V`:

```sh
docker run --rm -v "$PWD/work:/data" ghcr.io/sidick/volamos:latest \
  -V TEST:/data /fixtures/hello
```

!!! warning "Mounted directories must be writable by the container's user"
    Because the image runs as `nonroot` rather than root, it can't
    write into a bind-mounted host directory unless that directory's
    permissions actually allow it — a directory created with a
    restrictive default mode (e.g. `0700`, owned by your host user) is
    invisible to the container's UID even though it works fine when
    running the binary natively. If a guest program's `Open()` for
    writing mysteriously fails only under Docker, this is the first
    thing to check — `chmod 777` (or otherwise open up) the directory
    you're mounting before running the container.

### Build from source

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

Requires a recent stable Rust toolchain (edition 2024 support). See
[Building from Source](Building-from-Source.md) for the full details
(running the test suite, cross-platform notes).

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
