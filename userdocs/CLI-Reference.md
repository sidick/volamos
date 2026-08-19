# CLI Reference

```
volamos [-v|--verbose] [-s|--snoop] [-V NAME:hostdir]... [-a NAME:target[+target...]]...
        [--cwd AMIGAPATH] [--auto-assign HOSTDIR] [--stack SIZE] [--ram SIZE]
        [--cpu MODEL] [--fpu|--no-fpu] <program> [args...]
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

## No flags at all

If none of `-V`/`-a`/`--cwd`/`--auto-assign` are given, no volume/assign
filesystem is installed at all: `dos.library` path-based calls
(`Open`, `Lock`, `Examine`, ...) fail cleanly with an `IoErr()`, but
`Input`/`Output`/`PutStr`/`IoErr`/`SetIoErr` (and anything that doesn't
touch a path) still work — exactly what [Getting Started](Getting-Started.md)'s
first two examples rely on.
