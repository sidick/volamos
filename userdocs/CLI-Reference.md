# CLI Reference

```
volamos [-v|--verbose] [-s|--snoop] [-V NAME:hostdir]... [-a NAME:target[+target...]]...
        [--cwd AMIGAPATH] [--auto-assign HOSTDIR] [--stack SIZE] [--ram SIZE]
        [--cpu MODEL] [--fpu|--no-fpu] [--jit|--no-jit] [--sanitize]
        [--sanitize-uninit] [--sanitize-ignore-pc ADDR] <program> [args...]
```

`volamos --help` prints this same reference from the binary itself.
Everything before `<program>` is a volamos flag; `<program>` is a host
path to an AmigaOS hunk executable; everything after it is passed
through verbatim as the *guest* program's own command-line arguments
(`A0`/`D0`, the real AmigaOS startup convention) — a guest program that
parses its own arguments (e.g. via `ReadArgs`) reads them from there,
unaffected by anything volamos itself understood before `<program>`.

Every flag below can also be given a default value in `~/.volamos`/
`.volamos` instead of retyping it every run — see
[Configuration Files](Configuration.md).

## Exit codes

volamos propagates the **guest program's own exit code** on a normal
run — whatever it returned in `D0` at `rts`, same as running it on a
real Amiga would produce as its process return code. There is no
volamos-specific exit code convention layered on top of that.

volamos only ever produces its own exit code (`1`) when the guest never
got to run at all — the program path couldn't be read, didn't parse as
a valid hunk executable, a `-V`/`-a` argument was malformed, or the
guest genuinely crashed the runtime (an unimplemented library call, a
stack overflow, an invalid instruction). In every one of those cases
volamos prints a diagnostic to stderr naming exactly what went wrong
before exiting.

## `-v`, `--verbose`

Logs every emulated library call to stderr as it happens — library
name, LVO (library vector offset), and which handler serviced it:

```
volamos: dos.library(-948) -> PutStr
```

The most detailed of the two logging flags; useful for understanding
exactly what a guest program is doing, or diagnosing an unimplemented-
call failure by seeing the last few calls that succeeded first.

## `-s`, `--snoop`

A lighter-weight, `SnoopDos`-style alternative: logs only *resource-
opening* calls (`OpenLibrary`/`OldOpenLibrary`, `Open`) — what was
requested, and whether it resolved to a real/unimplemented library or
succeeded/failed for a file:

```
snoop: library "dos.library" -> base 0x00000800 (real)
```

Useful for quickly seeing which libraries and files a real, unfamiliar
binary actually touches, without the full per-call firehose `-v`
produces. If both `-v` and `-s` are given, `-v` wins — its own per-call
output already includes the same detail inline.

## `-V`, `--volume NAME:hostdir`

Maps an Amiga volume `NAME:` onto a real host directory. Repeatable —
give it multiple times to map several volumes at once. See
[Volumes and Assigns](Volumes-and-Assigns.md) for the full path-
resolution model.

```sh
volamos -V SRC:/home/me/project -V DEST:/tmp/out fixtures/hello
```

## `-a`, `--assign NAME:target[+target...]`

Assigns a logical Amiga name `NAME:` to one or more existing Amiga path
targets (each itself an already-mapped volume, or another assign),
joined with `+` for a multi-assign search order — the real AmigaDOS
`ASSIGN NAME: target1 ADD target2 ...` idiom. Repeatable. See
[Volumes and Assigns](Volumes-and-Assigns.md) for the exact search-
order and recursive-assign semantics, with worked examples.

```sh
volamos -V SYS:/home/me/amiga -a LIBS:SYS:libsA+SYS:libsB fixtures/hello
```

## `--cwd AMIGAPATH`

Sets the guest's initial current directory. Default: the first `-V`
volume's root if any `-V` was given, else the first `-a` assign's
root, else `root:` (relying on `--auto-assign` to resolve it — see
below).

```sh
volamos -V SRC:/home/me/project --cwd SRC:subdir fixtures/hello
```

## `--auto-assign HOSTDIR`

A fallback for any volume/assign name volamos doesn't otherwise know
about: `NAME:` resolves to `<HOSTDIR>/NAME` automatically, without
needing an explicit `-V`/`-a` for every name a guest program might
reference. See [Volumes and Assigns](Volumes-and-Assigns.md).

```sh
volamos --auto-assign /home/me/amiga-volumes fixtures/hello
```

## `--defaults`, `--no-defaults`

Whether the built-in standard-volume defaults (`SYS:`, `RAM:`, and the
standard `C:`/`S:`/`LIBS:`/`DEVS:`/`ENVARC:`/`T:`/`ENV:` assigns onto
them — see [Volumes and Assigns](Volumes-and-Assigns.md#standard-defaults))
apply at all. On by default; `--no-defaults` turns them off, restoring
the original "nothing configured means no filesystem at all" behavior:

```sh
volamos --no-defaults fixtures/hello
```

An explicit `-V`/`-a` for a name a default would otherwise supply
always wins — `-V SYS:~/amiga/wb31` shadows the default `SYS:`
entirely, and its own real `C:`/`Libs:`/etc. come along with it, same
as a real boot volume.

## `--volumes-dir HOSTDIR`

Overrides where the default `SYS:` volume's host directory lives
(default `~/.volamos.d/volumes`). Ignored if `--no-defaults` is given.

```sh
volamos --volumes-dir /var/lib/volamos/volumes fixtures/hello
```

## `--stack SIZE`

Overrides the guest stack region's size — default 64 KiB (65536
bytes). `SIZE` is a plain byte count, optionally suffixed `K`/`k`
(KiB) or `M`/`m` (MiB):

```sh
volamos --stack 256K fixtures/hello
volamos --stack 524288 fixtures/hello   # equivalent to --stack 512K
```

A value below the runtime's own minimum is silently clamped up to it,
mirroring real AmigaOS's own stack-size clamp behavior rather than
erroring. See [Getting Started](Getting-Started.md#4-see-a-runtime-safety-check-in-action)
for what happens when a guest program actually overflows its stack.

If `--stack` is close to or exceeds `--ram` (below), leaving no room
for the loaded program and the runtime's own guest heap, volamos fails
with a clear error rather than running at all:

```sh
$ volamos --ram 8K --stack 8K fixtures/hello
volamos: --stack 8192 is too large for --ram 8192: the loaded program ends
at 0x2224, and there must be room for the stack plus at least 4096 bytes
of guest heap after that -- increase --ram or decrease --stack
```

## `--ram SIZE`

Overrides the total guest address space — default 16 MiB (16777216
bytes). Same `SIZE` syntax as `--stack`: a plain byte count, optionally
suffixed `K`/`k` (KiB) or `M`/`m` (MiB):

```sh
volamos --ram 4M fixtures/hello
volamos --ram 64M --stack 1M fixtures/hello   # room for a much larger stack
```

The default comfortably covers the tiny CLI binaries volamos currently
targets, with plenty of headroom for a larger-than-default `--stack`.
Raise it if a guest program needs more address space than that (e.g. a
larger `--stack`, or a program that allocates a lot via `AllocMem`).

## `--cpu MODEL`

Picks the emulated CPU model. Default `68000` — the lowest common
denominator every Kickstart 3.1 machine shares. One of: `68000`,
`68010`, `68020`, `68ec020`, `68030`, `68ec030`, `68040`, `68ec040`,
`68lc040`, `68060`, `scc68070`.

```sh
volamos --cpu 68020 fixtures/hello
```

A binary compiled for a CPU floor higher than the emulated model (e.g.
`m68k-amigaos-gcc -m68020`, run under the default `--cpu 68000`) will
typically fail its own startup-time CPU check rather than running with
subtly wrong behavior — real toolchain startup code (`libnix` and
similar) checks `ExecBase.AttnFlags` and calls `Alert()` if the
running CPU doesn't meet what the binary was compiled for, and volamos
implements that check faithfully. If a real binary you're running
fails immediately with an `Alert` diagnostic, check what CPU floor it
was actually compiled for.

## `--fpu`, `--no-fpu`

Whether a coprocessor FPU is fitted. Default: no FPU. Only meaningful
for `--cpu 68020` and later — earlier models have no coprocessor
interface at all, so F-line (FPU) instructions always trap on them
regardless of this flag.

```sh
volamos --cpu 68020 --fpu fixtures/hello
```

!!! note "Performance: software math libraries are faster than a real FPU"
    Programs that do floating-point math through `mathffp.library`/
    `mathtrans.library`/`mathieeedoubbas.library` (the common path for
    code that doesn't require a real 68881/882 — e.g. most `--cpu 68000`
    binaries) run at full host CPU speed under volamos, since those
    libraries are implemented as genuine host `f64`/`f32` arithmetic,
    not emulated 1980s FPU hardware. Programs that instead execute real
    F-line FPU instructions (needing `--cpu 68020`+ and `--fpu`) go
    through the `m68k` crate's *interpreted* FPU emulation instead,
    which is considerably slower — confirmed with a real BYTEmark 2.2
    Fourier run: `--cpu 68000` (library math) scored an index of 1.46
    (faster than a 233 MHz AMD K6 reference machine); the same binary's
    real-FPU path under `--cpu 68020 --fpu` scored 0.09, in line with
    the rest of the suite's plain-interpreted-CPU results. `--fpu`
    doesn't force a program to use one path or the other — that's
    determined by how the program itself was written/compiled — it
    just decides whether real F-line instructions execute instead of
    trapping.

## `--jit`, `--no-jit`

Whether guest code runs through the `m68k` crate's trace JIT
(`CpuCore::run_batch`) instead of stepping one instruction at a time.
Default: off — the plain interpreter is this runtime's correctness
reference, and every emulated library call still traps out to volamos
identically either way, so `--jit` never changes a program's observable
behavior, only its speed.

```sh
volamos --jit fixtures/hello
```

!!! note "Where the win actually shows up"
    volamos is a Wine-style HLE runtime: guest code traps out to Rust on
    essentially every AmigaOS library call, and the JIT only accelerates
    the CPU-bound work *between* those traps (compiling hot
    backward-branch loops). For trap-dense glue code that spends most of
    its time inside library calls, `--jit` has little to offer. For
    CPU-bound guest work — the case that actually motivated this flag —
    compiling a real 577-line C source (`sc guiprof.c`, SAS/C 6.58)
    dropped from ~3.1s to ~2.3s of CPU time (a ~25% reduction) with
    `--jit`, though wall-clock barely moved, since most of `sc`'s wall
    time isn't CPU-bound at all. Expect `--jit`'s benefit to scale with
    how CPU-heavy the guest program's own work is, not with how many
    library calls it makes. A purely CPU-bound benchmark shows the full
    effect: [CoreMark 1.0](https://github.com/eembc/coremark) (`-O2
    -m68020 -msoft-float`, `--cpu 68020`) scores 198.2 iterations/sec
    interpreted and 537.1 with `--jit` (volamos 0.3) — for comparison,
    `vamos` scores 270.6 on the same binary with no JIT of its own.

    | Runtime | CoreMark 1.0 |
    | --- | --- |
    | `vamos` | 270.6 |
    | volamos 0.2, interpreter | 105.9 |
    | volamos 0.2, `--jit` | 445.1 |
    | volamos 0.3, interpreter | 198.2 (~1.9x faster than 0.2) |
    | volamos 0.3, `--jit` | 537.1 (~1.2x faster than 0.2) |

## `--sanitize`

Turns on shadow-memory checking of every guest memory access, to catch
heap bugs that would otherwise corrupt memory silently. Default: off.

`m68k-amigaos-gcc` has no `-fsanitize=address`, and MMU-based tools like
MuForce work at page granularity, so a one-byte overrun or a read just
past the end of an allocation is invisible to them. volamos can do
better because it *is* the allocator and every access already funnels
through one place.

```sh
volamos --sanitize fixtures/memtest overrun
```
```
overrun: writing 1 byte past a 32-byte block
sanitizer: 1 distinct violation(s):
  invalid 1-byte write at 0x00002ee0 (heap redzone) from PC 0x00002af6
```

What it catches:

- **Heap overruns and underruns**, in either direction and down to a
  single byte, by placing poisoned redzones either side of every
  `AllocMem`/`AllocVec`/`AllocPooled` block — including the padding
  between the size a program asked for and the 8-byte-rounded size it
  actually got.
- **Use-after-free**, by poisoning a block on `FreeMem` and holding its
  address out of circulation in a free quarantine, so the bug doesn't
  hide behind an address that happens not to have been reused yet.
- **Bad buffers handed to library calls**, for free: host-side handlers
  write guest memory through the same checked path, so passing a
  too-small buffer to a `dos.library` call is caught without any
  per-function instrumentation.

It also catches two classes of stack bug:

- **Accesses below the stack pointer** — reading or writing memory a
  program has already released, or running off the bottom of the live
  stack. Accesses within 64 bytes below the stack pointer are forgiven,
  because on m68k a push *writes* below the stack pointer by definition
  (`move.l d0,-(sp)` decrements as part of the store, and `MOVEM` can
  move 64 bytes at once), so a stricter rule reports every subroutine
  call any program makes. valgrind forgives the same window for the
  same reason.
- **Return-address corruption**, via a shadow call stack that records
  the address each `JSR`/`BSR` pushes and verifies it at the matching
  return:

  ```
  return address corrupted at stack slot 0x00fffff8: expected 0x00002ace, found 0x00002ada from PC 0x00002ad8
  ```

  This is stack-smash detection, and it is something valgrind does not
  offer. It stays quiet on the legitimate `move.l #target,-(sp)` + `rts`
  computed-jump idiom, which has no matching call, and it survives
  `StackSwap` (a program moving to an entirely different stack).

Violations are reported to stderr after the run, deduplicated by
(PC, address, kind) with a hit count, and the guest is left to continue
— this is a detector, not an enforcer, so one run surfaces every bug
rather than dying at the first. A multi-byte access that straddles into
poisoned memory reports once, at the first offending byte, rather than
once per byte.

!!! note "Verified against real software"
    `--sanitize` runs the real PhxAss assembler, real pLhA listing a
    102-file archive, and the real SAS/C 6.58 compiler with **zero**
    violations, and `sc`'s output object file is byte-identical to an
    unsanitized run's. Getting there took fixing several false-positive
    sources that unit tests could never have surfaced — a sanitizer that
    flags correct code is worse than no sanitizer, so if you do see a
    report from a program you believe is correct, it is worth filing.

!!! note "It forces the interpreter"
    `--sanitize` turns the JIT off even if `--jit` was also given. The
    JIT's fast path accesses guest memory through a raw pointer that
    bypasses the checks entirely, so the two cannot be combined — a
    sanitized JIT run would report a clean bill of health no matter what
    the program did. Expect a sanitized run to be slower accordingly.

## `--sanitize-uninit`

Adds reporting of **uninitialized reads** — memory that was allocated
but never written — on top of `--sanitize`, which it implies. Off by
default even when `--sanitize` is on.

```console
$ volamos --sanitize-uninit fixtures/memtest uninitpartial
uninitpartial: alloc 32 bytes, write offsets 0-15, read offset 20
sanitizer: 1 site(s), 1 violation(s):
  uninitialized 1-byte read at 0x00003154 from PC 0x00002c08
```

Detection is **byte-granular**: that example writes the first 16 bytes
of a 32-byte block and is still caught reading offset 20, so a partially
initialized structure is detected rather than being treated as
initialized because something in it was written. `AllocMem` with
`MEMF_CLEAR` is genuinely initialized memory and never reports.

!!! note "Why this is a separate flag"
    Uninitialized-read detection is the noisiest class in any
    sanitizer, and it is the one where legitimate patterns trip it: a
    whole-struct copy that includes padding bytes, or a table scan that
    touches never-used slots, are both real, correct code that reads
    bytes nobody wrote. valgrind ships suppression files precisely for
    this. Keeping `--sanitize` quiet by default is what makes it
    trustworthy, so this is opt-in.

    For calibration: with this flag, real pLhA listing a 102-file
    archive reports nothing at all, and the real PhxAss assembler
    reports 6 sites — of which one instruction walking a table accounts
    for 568 of the 574 individual violations.

## `--sanitize-ignore-pc ADDR`

Suppresses violations reported at guest PC `ADDR` (decimal, or hex with
a `0x` prefix). Repeatable.

This is for silencing a site you have already looked at and decided is
not a bug, without needing a suppression file. Combined with the
report's per-PC grouping it makes a noisy program tractable:

```console
$ volamos --sanitize-uninit ... PhxAss ...
sanitizer: 6 site(s), 574 violation(s):
  ...
  PC 0x0000e14a: 568 uninitialized 4-byte reads, addresses 0x00038068-0x0003a3d8
  ...

$ volamos --sanitize-uninit --sanitize-ignore-pc 0xe14a ... PhxAss ...
sanitizer: 5 site(s), 6 violation(s):
  ...
```

Note the report groups by PC in general: a site with many violations
collapses to one line with a count and an address range, while a site
with only a few prints each violation in full. That is deliberate — one
instruction in a loop producing hundreds of near-identical lines is
noise, but a single violation's detail (for a corrupted return address,
the expected and actual addresses) is the whole diagnostic value and is
never collapsed away.

## Source locations in diagnostics

When a program carries debug information, volamos annotates sanitizer
violations with where in the source they happened, instead of only an
address:

```console
$ volamos --sanitize memtest overrun
sanitizer: 1 site(s), 1 violation(s):
  invalid 1-byte write at 0x000032a0 (heap redzone) from PC 0x00002b38 (at work:memtest.s:281)
```

Nothing needs enabling — it is used automatically when present. Two
sources are consulted, best first:

1. **`file:line`**, from a `HUNK_DEBUG` `LINE` block. Emitted by SAS/C
   when asked (`sc DEBUG=LINE`) and by PhxAss (`LINEDEBUG`).
2. **`symbol+offset`**, from the binary's `HUNK_SYMBOL` table, when
   there is no line coverage:

   ```
   return address corrupted at stack slot 0x00ffffe0: expected 0x00002b40,
     found 0x41414141 from PC 0x00002b10 (at ___main+0x3c)
   ```

The raw PC is always kept alongside, because that is what you need to
find the instruction in a disassembly.

!!! note "What each toolchain gives you"
    **m68k-amigaos-gcc** emits *stabs* debug info, which volamos does
    not decode, so `-g` alone does not produce `file:line` here. gcc
    binaries do carry a symbol table, though, so they get
    `symbol+offset` — the example above is a real gcc-built program.

    **SAS/C** and **PhxAss** produce `LINE` blocks when asked, and those
    give true `file:line`.

!!! warning "Two honest limits on attribution"
    **Line numbers point at statements, not instructions.** `LINE` data
    is sparse — one entry per source line — so a PC between two entries
    is reported against the earlier one. That is the right answer for
    "which statement", not "which instruction".

    **Symbol attribution is only as good as the symbol table.** `static`
    functions are not exported and so do not appear in `HUNK_SYMBOL` at
    all; a violation inside one is reported against the nearest
    preceding *exported* symbol, which can be a surprising name with a
    large offset. Prefer `file:line` where your toolchain can produce
    it.

## `--dirty-heap`

Fills every `AllocMem`/`AllocVec`/`AllocPooled` block allocated
**without** `MEMF_CLEAR` with the byte `0xA5`, instead of leaving it as
the zeros volamos's guest memory happens to start as.

```console
$ volamos fixtures/memtest zerodep
zerodep: read 0 -- took the zero path (this is the bug: it only works because the memory happened to be zero)

$ volamos --dirty-heap fixtures/memtest zerodep
zerodep: read non-zero -- took the garbage path
```

Real `AllocMem` without `MEMF_CLEAR` returns whatever debris was in
that memory — usually a previous allocation's leftovers. volamos's
memory starts zeroed, so a program that relies on uncleared memory
being zero works perfectly here while failing sporadically on real
hardware, depending on what ran before it. This flag makes that debris
real, so the bug shows up as a reproducible failure instead of hiding.

`MEMF_CLEAR` allocations are untouched: that is a documented guarantee
of zeroed memory, and a program that asks for it gets zeros whatever
debugging flags the host was given.

`0xA5` is the conventional debug poison, and it is chosen over `0x00`
or `0xFF` for two reasons: it is unmistakable in a memory dump, and
`0xA5A5A5A5` is an **odd** address — so a guest that reads it out of an
uninitialized field and dereferences it as a pointer takes an address
error on a 68000 immediately, rather than quietly reading somewhere
plausible. A bug that announces itself beats one that limps on.

!!! note "Independent of `--sanitize`"
    This flag **changes what the guest sees**, whereas everything under
    `--sanitize` only observes. That distinction is deliberate and
    worth keeping: `--sanitize` can be trusted not to alter a program's
    behaviour (the real SAS/C compiler's output object file is
    byte-identical under it), so a behaviour change does not belong
    inside it.

    The two compose, though — `--dirty-heap --sanitize-uninit` both
    fills the memory *and* reports the reads — and `--dirty-heap` works
    on its own as a plain bug-shaker, with no shadow map and no
    slowdown.

!!! warning "What it cannot see"
    Overflows *within* a stack frame — a 16-byte local overflowing into
    the local next to it — need compiler instrumentation to detect, and
    are invisible here for the same reason they're invisible to
    valgrind. Such corruption becomes visible only once it reaches
    something tracked: a return address, a heap redzone, or memory below
    the stack pointer.

    `AvailMem` also legitimately reports less free memory under
    `--sanitize`, because redzone and quarantined bytes genuinely aren't
    available.

## No flags at all

With the [built-in defaults](Volumes-and-Assigns.md#standard-defaults)
active (the default), `SYS:`, `C:`, `S:`, `LIBS:`, `DEVS:`, `ENVARC:`,
`RAM:`, `T:`, and `ENV:` all resolve out of the box even with no `-V`/
`-a`/`--cwd`/`--auto-assign` given — backed by empty host directories
created only on first actual use, so [Getting Started](Getting-Started.md)'s
first two examples (which never touch the filesystem at all) leave no
trace on disk either way. Any other name still fails cleanly with an
`IoErr()` — a typo isn't silently treated as a new empty volume.

With `--no-defaults`, or if none of the above apply, no volume/assign
filesystem is installed at all: `dos.library` path-based calls
(`Open`, `Lock`, `Examine`, ...) fail cleanly with an `IoErr()`, but
`Input`/`Output`/`PutStr`/`IoErr`/`SetIoErr` (and anything that doesn't
touch a path) still work.
