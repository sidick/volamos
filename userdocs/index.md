# volamos

volamos is a Rust-native, Wine-style runtime for classic AmigaOS m68k
CLI binaries. It runs 68k console programs — compilers, assemblers,
linkers, file utilities, and the like — directly on macOS and Linux by
emulating the m68k CPU and reimplementing `exec.library`/`dos.library`
calls at the API boundary on the host OS, rather than emulating an
entire Amiga.

There is no full-system emulation and no GUI or custom-chip (graphics,
audio, disk hardware) support: volamos is aimed squarely at running
command-line tools, not games or Workbench applications. It is a
spiritual successor to [`vamos`](https://github.com/cnvogelg/amitools),
the Python implementation of the same idea from the `amitools` package
— volamos aims for the same job, done in Rust, and (per its own gap
audit against vamos's coverage) at least as complete for the
console-tool use case.

## Why volamos, not full emulation

- **No display, no boot disk, no ROM image.** A real emulator
  (Amiberry, WinUAE, FS-UAE, Copperline) needs a Kickstart ROM, a
  Workbench disk, and a virtual screen even to run one command-line
  tool. volamos loads a hunk executable directly and starts running it
  in milliseconds — no boot sequence, no disk image to maintain.
- **The host filesystem is the Amiga filesystem.** `-V`/`-a` map Amiga
  volumes and assigns straight onto host directories — no `.adf`
  images to mount, no intermediate copy step. A guest program's
  `Open("SRC:foo.c")` reads a real file on your Mac or Linux box.
- **Fast, on purpose.** There's no cycle-accurate timing, no custom-chip
  DMA to keep in sync with a virtual clock — `Runtime::run` steps the
  CPU as fast as the host allows. That's the right trade-off for a
  compiler/assembler/linker you're running as part of a build, not a
  game that depends on real-time hardware behavior.
- **Headless and CI-first.** No dependency on a display, windowing
  system, or Amiga hardware at all — running an original Amiga
  toolchain as part of a host build (cross-compilation the other way
  around) is a first-class use case.

## Where to start

- [Installation](Installation.md) — prebuilt release binaries, the
  container image, or building from source.
- [Getting Started](Getting-Started.md) — running your first guest
  program, and mapping a real directory onto an Amiga volume.
- [CLI Reference](CLI-Reference.md) — every flag.
- [Configuration Files](Configuration.md) — `~/.volamos`/`.volamos` to
  stop retyping the same flags for a repeated-use project.
- [Volumes and Assigns](Volumes-and-Assigns.md) — how `-V`/`-a` map the
  host filesystem onto Amiga paths, including multi-assign search
  order.
- [Supported Libraries](Supported-Libraries.md) — exactly which
  `exec.library`/`dos.library`/`utility.library`/etc. calls are
  implemented today.
- [Examples](Examples.md) — task-oriented recipes: nested program
  execution, directory listing, scripting volamos in CI, diagnosing an
  unfamiliar binary.

## A note on where volamos is today

volamos is pre-1.0, actively developed software. The core CPU/trap
plumbing, `dos.library` file I/O and volumes/assigns, and
`exec.library`/`utility.library` essentials are complete and tested —
host unit/end-to-end tests, plus extensive empirical hardening against
a real Workbench 3.1.4 `C:` command corpus (`List`, `Copy`, `Delete`,
`Rename`, `Sort`, `Search`, `Join`, `CPU`, `Date`, and more) and real
third-party binaries (the PhxAss assembler, and a real backup-tool
project). A full audit against `vamos`'s own library/device coverage
closed the remaining gaps it flagged, including `ReadArgs`/`FreeArgs`,
the full `SignalSemaphore` API, the math libraries, `locale.library`,
and a thin `intuition.library` stub.

What hasn't happened yet: a formal, CI-integrated three-oracle parity
pass (volamos vs. `vamos` vs. real Kickstart, on a shared corpus) — a
local-only harness exists and has already caught and fixed real
divergences, but it isn't wired into CI yet. If a guest program hits
an unimplemented call, volamos fails
loudly with a clear diagnostic naming the exact library/function
rather than silently misbehaving — see
[Supported Libraries](Supported-Libraries.md) for what's covered, and
file an issue for anything missing.
