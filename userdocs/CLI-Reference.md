# CLI Reference

```
volamos [-v|--verbose] [-s|--snoop] [-V NAME:hostdir]... [-a NAME:target[+target...]]...
        [--cwd AMIGAPATH] [--auto-assign HOSTDIR] [--defaults|--no-defaults]
        [--volumes-dir HOSTDIR] [--stack SIZE] [--ram SIZE] [--cpu MODEL]
        [--fpu|--no-fpu] [--fpsp-emulate] [--jit|--no-jit] [--clock-mhz N] [--sanitize]
        [--sanitize-uninit] [--sanitize-ignore-pc ADDR] [--dirty-heap] [--uae-debug]
        [--emu68-debug] [--net] <program> [args...]
```

`volamos --help` prints this same reference from the binary itself, and
`volamos --version` prints the version (`--version` only — `-V` is
`--volume`, as it always has been). A value-taking flag accepts both
spellings: `--stack 256K` and `--stack=256K` are equivalent.

Everything before `<program>` is a volamos flag; `<program>` is a host
path to an AmigaOS hunk executable; everything after it is passed
through verbatim as the *guest* program's own command-line arguments
(`A0`/`D0`, the real AmigaOS startup convention) — a guest program that
parses its own arguments (e.g. via `ReadArgs`) reads them from there,
unaffected by anything volamos itself understood before `<program>`.

A token before `<program>` that looks like a flag but isn't one is an
error (with a did-you-mean suggestion), not silently treated as the
program path. A program file whose name genuinely starts with `-` is
still expressible as `./-foo`, or as `-- -foo` (`--` ends flag parsing;
the next token is `<program>` no matter what it looks like).

Every flag below can also be given a default value in `~/.volamos`/
`.volamos` instead of retyping it every run — see
[Configuration Files](Configuration.md).

## Exit codes

volamos propagates the **guest program's own exit code** on a normal
run — whatever it returned in `D0` at `rts`, same as running it on a
real Amiga would produce as its process return code. There is no
volamos-specific exit code convention layered on top of that.

volamos only ever produces its own exit code when the guest never got
to run at all: `2` for a command-line parse error (an unknown or
malformed flag, a bad `-V`/`-a` value, a missing `<program>` — the
conventional usage-error code), and `1` when the command line parsed
but the run couldn't start or crashed the runtime — the program path
couldn't be read, didn't parse as a valid hunk executable, or the guest
genuinely crashed the runtime (an unimplemented library call, a stack
overflow, an invalid instruction). In every one of those cases volamos
prints a diagnostic to stderr naming exactly what went wrong before
exiting. `--help` and `--version` exit `0`.

The one case where a normal run's own exit code is overridden: under
[`--sanitize`](#-sanitize), a run that reported any violation exits
`99` instead of the guest's own code, so a harness that only checks
the exit status doesn't pass a run that corrupted memory.

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
volamos --ram 8M --stack 1M fixtures/hello   # room for a much larger stack
```

The default comfortably covers the tiny CLI binaries volamos currently
targets, with plenty of headroom for a larger-than-default `--stack`.
Raise it if a guest program needs more address space than that (e.g. a
larger `--stack`, or a program that allocates a lot via `AllocMem`).

**Above 16 MiB needs `--cpu 68020` or later.** A 68000 or 68010 has a
24-bit address bus, so 16 MiB is all it can address at all. Since the
guest stack sits at the top of the address space, a larger `--ram` would
put the stack pointer at an address the CPU cannot express — so volamos
refuses the combination rather than letting the program fail confusingly
later on:

```sh
$ volamos --ram 64M fixtures/hello
volamos: --ram 67108864 is more address space than a 68000 can reach: that
CPU has a 24-bit address bus, so it can only address 16777216 bytes, and the
guest stack at the top of a larger space would be at an address it cannot
express -- ask for a 32-bit address bus with --cpu 68020 (or any later
model), or lower the size to 16777216 or less (--ram on the command line,
RAM= in a config file)

$ volamos --ram 64M --cpu 68020 fixtures/hello
Hello from volamos
```

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
for `--cpu 68020`/`68030` — earlier models have no coprocessor
interface at all, so F-line (FPU) instructions always trap on them
regardless of this flag.

```sh
volamos --cpu 68020 --fpu fixtures/hello
```

The full `68040` and `68060` always have their on-die FPU on real
hardware — unlike `68020`/`68030`, there's no plug-in 68881/68882 to
leave out. `--fpu` is therefore a harmless no-op on `--cpu 68040`/
`68060` ([issue #167](https://github.com/sidick/volamos/issues/167)).
`--no-fpu` combined with `--cpu 68040` is substituted to
`--cpu 68ec040` instead of silently having no effect, which is what it
did before this was fixed — `AttnFlags` told the guest there was no
FPU while FPU instructions still executed, a combination no real chip
has:

```console
$ volamos --cpu 68040 --no-fpu fixtures/hello
volamos: --cpu 68040 --no-fpu: the full 68040 always has its on-die FPU on real
hardware, so running as --cpu 68ec040 instead (which really has no FPU) -- pass
--cpu 68ec040 or --cpu 68lc040 directly to avoid this note
Hello from volamos
```

Pass `--cpu 68ec040`/`--cpu 68lc040` directly to skip the note.

`--cpu 68060 --no-fpu` is a different case: the `m68k` crate *can*
model an FPU-less 68060 internally (unlike the 68040, there's no
separate `CpuType` to substitute to), but its internal FPU-disabled
exception delivery bypasses this runtime's usual guest-exception
interception entirely — if the guest never installed its own FPU
handler (the ordinary case for `-m68020-60 -m68881`-compiled code,
which assumes the FPU just exists), it used to land on whatever
garbage happened to be at the real vector-11 slot, surfacing as a
baffling "continuation stub trapped ... with no pending continuation."
That slot is now pre-seeded with a dedicated stub, so this reports a
clean, attributable stop instead:

```console
$ volamos --cpu 68060 --no-fpu a-68060-fpu-binary
volamos: a-68060-fpu-binary: unexpected CPU stop at 0x00002c02: Illegal { opcode: ... }
```

A guest that installs its own real FPU-probe handler before executing
any FPU instruction (the common "catch the exception, fall back to
software float" idiom) is unaffected — that write simply overwrites
the pre-seeded stub like any other guest memory write.

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

## `--fpsp-emulate`

Software-emulates the instructions a real 68060 removed from silicon
— most FPU transcendentals (`FSIN`/`FCOS`/`FATAN`/`FETOX`/`FLOGN`/
`FMOD`/`FSCALE`/`FINT`/`FGETEXP`/`FGETMAN`/`FSINCOS`/...) plus a
handful of integer ops (`MOVEP`, `CHK2`/`CMP2`, misaligned `CAS`/
`CAS2`, 64-bit `MUL`/`DIV`) — instead of taking the real "unimplemented
instruction" exception a genuine 68060 does for them. Only meaningful
with `--cpu 68060`; a no-op for every other `--cpu` model. Default:
off — real 68060 hardware fidelity, matching every other `--cpu`
model's default behavior.

```sh
volamos --cpu 68060 --fpu --fpsp-emulate a-68040-fp-binary
```

On real hardware, that trap is exactly what lets AmigaOS's
`68060.library` install FPSP/ISP software-emulation handlers at boot
(via `SetPatch`) — which is how a 68040-FP-targeted binary keeps
working on a real 68060 machine without ever being recompiled.
volamos has no equivalent of that persistent, cross-process boot
state (every invocation is a single, fresh guest process, with no
"patch once at boot, every later process inherits it" mechanism), so
loading a real `68060.library` isn't a viable substitute here — this
flag gets you the same practical effect for a single run instead,
without needing any real `68060.library`/`fpsp.library` file at all.

CLI-only: unlike most flags on this page, `--fpsp-emulate` can't be
set in `~/.volamos`/`.volamos` — silently swapping real hardware
behavior (a trap) for software emulation is exactly the kind of thing
a stale config file shouldn't be able to turn on for every future run
in a directory.

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

## `--clock-mhz N`

Makes `timer.device`'s `ReadEClock` report **emulated** time — derived
from the CPU's own emulated cycle count at `N` MHz — instead of host
wall-clock time. `N` may be fractional (e.g. `--clock-mhz 25` or
`--clock-mhz 7.14`). Default: off (host clock, as always). This exists
for reproducible, host-load-independent compiler A/B benchmarks: two
`ReadEClock` calls bracketing some guest work measure how many m68k
cycles actually ran, not however long the host machine happened to take
around them (which varies with scheduling noise, thermal throttling,
whatever else is running on the box at the time).

```sh
volamos --clock-mhz 25 fixtures/hello
```

Cannot be combined with an explicit `--jit`: the trace JIT
(`CpuCore::run_batch`) never tracks a cycle count at all, so there would
be nothing for `--clock-mhz` to derive `ReadEClock`'s reported time
from. `--clock-mhz` implies the plain interpreter path either way
(equivalent to `--no-jit`, the default) — only an *explicit* `--jit` on
top of it is refused.

Also cannot be combined with [`--sanitize`](#-sanitize): the
cycle-counted execution path `--clock-mhz` uses
(`M68kCpu::run_via_cycles`) doesn't run the sanitizer's per-instruction
shadow-map hooks at all, so `--sanitize` would silently check nothing
while still printing a normal-looking summary — refused for the same
"an explicit error beats a silently wrong answer" reason as the `--jit`
conflict above.

Only `ReadEClock` changes. `GetSysTime`, `TR_GETSYSTIME`, `DateStamp`,
and `CurrentTime` all still read the real host clock — a benchmark run
under `--clock-mhz` doesn't think it's 1978.

When `--clock-mhz` is active, volamos also prints the run's total at
exit, on stderr:

```console
$ volamos --clock-mhz 25 ./bench
...program output...
volamos: 422158 emulated cycles, 0.016886 s at 25 MHz
volamos: 36581 instructions, 83754 bus accesses (66532 read / 17222 write), 11.54 cycles/instr, 2.29 accesses/instr
volamos: 183 native library calls ran in zero emulated cycles: exec.library/CopyMem x120, dos.library/Write x41, dos.library/Read x12, exec.library/AllocMem x6, dos.library/Open x2, and 2 more; CopyMem/CopyMemQuick moved 491520 bytes natively
```

The second line exists to make a run's **memory intensity** visible from
the run itself. volamos bills no bus wait states (see the warning below),
so its emulated time is close to real hardware for arithmetic-bound code
and very optimistic for bus-bound code — measured between 1.02x and 25x
against cycle-paced hardware, monotonic in memory intensity
([issue #105](https://github.com/sidick/volamos/issues/105)). Cycles and
seconds alone don't say which end of that range a workload sits at;
accesses per instruction does, with no second runtime to compare against.

The third line exists because native library handlers run in **zero
emulated cycles** (see the warning below): it reports how many library
calls were serviced natively — the top handlers by call count, and the
total bytes `CopyMem`/`CopyMemQuick` were asked to move, since a guest
`memcpy` routed through exec's `CopyMem` is the classic way a
benchmark's real work silently bypasses the cycle count. A run whose
copying "took no time" now says so in its own output
([issue #109](https://github.com/sidick/volamos/issues/109), prompted by
a real GCC-patch benchmark where a memcpy test "improved" 49% under
volamos against 5% on real hardware). The line is omitted entirely when
the guest made no library calls at all.

Two things to know before reading the ratios:

- **`read` includes instruction fetch.** The bus methods can't tell a
  fetch from a data read, so accesses/instr has a floor a little above
  1.0, and it's the margin *above* that floor that indicates data
  traffic. On two deliberately-opposite `-O2` loops: a byte-copy loop
  measures 2.67 accesses/instr, a register-only arithmetic loop 1.27.
- **`write` is the clean signal**, carrying no fetch component — 819308
  writes for that copy loop against 110 for the arithmetic one. That's why
  the two are reported separately rather than only as a total.

The line goes to stderr so a harness parsing the guest program's own
stdout never sees it, and it is printed even when the run ends in an
error — a benchmark that died part-way still burned the cycles it
burned. Without `--clock-mhz` nothing is printed, because on the default
execution path no cycle count exists to print (see the warning below).

This makes an *uninstrumented* binary measurable: the guest does not
have to call `ReadEClock` and report its own elapsed time. (`vamos -v`
has reported an equivalent `total cycles:` line for its own runs all
along; the two agree closely — the same `ehtest` binary measures 340,020
cycles under volamos against vamos's 339,044 at 68000, and 157,436
against 155,855 at `--cpu 68020`, despite entirely independent cycle
models.)

!!! warning "Read this before trusting the numbers"
    - **This is the slowest execution mode volamos has.** Measured with
      [CoreMark 1.0](https://github.com/eembc/coremark)
      (`~/src/external/coremark/coremark.amiga`, `-O2 -m68020
      -msoft-float`, `--cpu 68020`, host wall-clock throughput —
      completed iterations divided by real elapsed seconds, three runs
      averaged per mode, since `--clock-mhz` redefines the `ReadEClock`
      CoreMark itself times against, so its *self-reported*
      iterations/sec isn't a valid host-speed comparison once
      `--clock-mhz` is active): the default interpreter (`--no-jit`)
      manages ~217 iterations/sec, `--clock-mhz 25` (`CpuCore::
      run_for_cycles`, the `m68k` crate's transaction-exact,
      cycle-accurate path) manages ~97 — about **2.2x slower** than
      `--no-jit`, and about **7x slower** than `--jit`'s ~680. It's
      opt-in, for benchmarking, not for everyday use.
    - **volamos's memory bus has no wait states.** `FlatMemory` is a
      flat `Vec<u8>` with no `sync`/bus-timing model at all — unlike
      real Chip/Fast RAM, which real hardware (and
      [Copperline](https://github.com/sidick/copperline), volamos's own
      hardware-timing oracle) bills real contention cycles for.
      Memory-bound guest code will still report a different emulated
      time than real hardware for this reason, even with a
      correctly-configured `--clock-mhz`.
    - **Time spent inside volamos's own native-Rust library handlers is
      free.** Every `dos.library`/`exec.library`/... call this runtime
      implements natively (see [Supported Libraries](
      Supported-Libraries.md)) costs *zero* emulated cycles — it's host
      code, not guest instructions. A workload dominated by, say,
      `CopyMem` will report an emulated time implying it spent no time
      there at all, because from the CPU's point of view, it didn't.
      The native-calls line above is this caveat's counterweight: the
      zero-cycle work is itemized, so it's visible even though it's
      unbilled.
    - **So is time spent inside a guest callback volamos single-steps
      rather than runs**, for the same reason: `RawDoFmt`'s
      `PutChProc` callback and `Supervisor`'s routine are both executed
      one instruction at a time through a separate, non-cycle-counting
      path, so guest code reached that way is just as invisible to
      `--clock-mhz` as a native library handler is.
    - **A `System()`/`Execute()`-nested program gets its own, separate
      cycle counter, starting back at zero.** A parent process's
      `ReadEClock` calls bracketing a `System()` call will see that
      child's entire run as contributing (close to) zero elapsed time,
      since the child's cycles accumulate into its own nested runtime
      instance, never the parent's. This is deliberate (nesting a
      shared counter through a separately-constructed `Runtime` would
      be significant extra plumbing for a benchmarking edge case), but
      it means a benchmark that shells out mid-measurement will
      under-report.

## `--cache`

Models a direct-mapped instruction cache, sized by `--cpu` (256 bytes on
68020/68030, 4096 bytes on 68040/68060 — 68000/68010 have no on-chip
cache on real hardware, so this is a no-op there), so a loop larger than
the cache — or extra code size from `-Os` vs `-O2`, block duplication,
etc. — shows up in [`--clock-mhz`](#-clock-mhz-n) timings instead of
every instruction fetch being treated as a free cache hit
([issue #148](https://github.com/sidick/volamos/issues/148)).

```sh
volamos --clock-mhz 25 --cache fixtures/hello
```

Meaningless without `--clock-mhz`: that's the only execution path that
reports a cycle count for a modeled cache hit/miss to affect. Off by
default so existing `--clock-mhz` numbers stay comparable across runs
(opt in, as issue #148 asked).

`CacheClearU`/`CacheClearE` flush the model, matching real AmigaOS's own
cache-coherency convention for self-modifying code and freshly loaded
code.

Two known gaps, both out of scope for issue #148 and left as follow-up:

- **No effect on `--cpu 68040`.** The `m68k` crate's 68040 timing model
  doesn't consult cache state at all yet (only 68020/68030/68060 do), so
  `--cache` changes nothing measurable there besides
  `CacheClearU`/`CacheClearE` bookkeeping.
- **No data cache or memory-wait-state modeling.** The `m68k` crate has
  no timing hooks for either yet, and `CacheControl`'s `CACRF_EnableI`
  toggle isn't wired to the model — `--cache` is an unconditional
  CLI-level choice for the whole run, not something a guest can turn
  on/off at runtime.

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
- **Use-after-free and uninitialized reads through `exec.library`'s
  `Allocate`/`Deallocate`** too (issue #83) — which matters because a C
  program's `malloc` (under both libnix and newlib) takes a few big
  blocks via `AllocVec` and then sub-allocates from its own pool with
  `Allocate`, so most of a real C program's heap traffic goes through
  `Allocate`, not `AllocMem` directly. Unlike the `AllocMem` family,
  there are **no redzones here** — `Allocate` hands out memory from a
  pool the *guest* owns and walks with its own pointers, so volamos
  cannot grow a block without changing what the guest's own free-list
  walk sees. This means a single-byte overrun past the end of a
  `malloc`'d block (as opposed to a use-after-free, or a read of memory
  `malloc` never initialized) is not yet caught; see issue #83's "Tier
  2" for why that's a separate, bigger decision.

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
  computed-jump idiom, which has no matching call, on `longjmp` returning
  through a slot a later call reused, and it survives `StackSwap` (a
  program moving to an entirely different stack).

Violations are reported to stderr after the run, deduplicated by
(PC, address, kind) with a hit count, and the guest is left to continue
— this is a detector, not an enforcer, so one run surfaces every bug
rather than dying at the first. A multi-byte access that straddles into
poisoned memory reports once, at the first offending byte, rather than
once per byte.

When the run reported any violation, volamos exits with status 99
instead of the program's own, so a test harness that only checks the
exit status fails the run. A program started with `System()` or
`Execute()` from inside the run still returns its own code to its
caller.

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

## `--uae-debug`

Enables UAE's debug facilities. One flag turns on both mechanisms:
the [debug sprintf port](#the-debug-sprintf-port) at `0xBFFF00` and the
[`uaelib` debug-log trap](#the-uaelib-debug-log-trap) at `0xF0FF60`.

### The debug sprintf port

The **debug sprintf port** at `0xBFFF00` is the memory-mapped
debug channel WinUAE and amiberry expose when their `debug_mem` option
is on. A guest formats text using nothing but stores to that address (no
library call, no `dos.library`, works in code that has none of them
open), and the text is written to **stderr**, exactly as formatted: no
prefix, and no newline unless the format string has one.

```console
$ volamos --uae-debug prog
boot: stage=2 flags=$0000beef name=loader
```

Off by default, and CLI-only — it is not settable from `~/.volamos`/
`.volamos`, so a stale config file can't start diverting stores. With
the flag off, a store to `0xBFFF00` behaves exactly as it always did.
Nested `System()`/`Execute()`/`RunCommand()` programs inherit it.

### Protocol

All offsets are from `0xBFFF00`:

| Store | Effect |
|---|---|
| any size at `+0` | Push an argument (its value and width) on a 32-entry stack. Pushes beyond the 32nd are dropped. |
| **long** at `+4` | The value is a pointer to a NUL-terminated format string: format it with the pushed arguments, print it, and clear the stack. |
| **long** at `+8` | The value is a pointer to a `va_list`-style argument block in guest memory: from now on every format reads its arguments from there, packed with no padding (until the next push at `+0`). |

Byte and word stores to `+4`/`+8` are ignored. On a 68000/68010 a long
store is two word bus cycles and UAE sees (and latches) them as such, so
`+2`/`+6`/`+10` are routed too and a pair of `move.w` works the same as
one `move.l`; on a 68020 and later only the exact offsets `+0`/`+4`/`+8`
are routed. volamos picks this from `--cpu`.

### Format codes

`%%`, `%c`, `%s` (C string), `%b` (BSTR), `%p` (printed `$%08x`), and
`%x %X %d %i %u %o`, with the usual C flags (`-`, `+`, space, `#`, `0`),
width and precision between the `%` and the code.

!!! warning "Integers are 16 bits unless you say `l`"
    `%d` consumes a **16-bit** value and sign-extends it; `%x`/`%u`/`%o`
    zero-extend it. Print a 32-bit value with `%ld`/`%lx`/...; pointers
    for `%s`, `%b` and `%p` are always 32-bit. Forgetting the `l` prints
    the low half of the number — and with the `+8` argument block it
    also *misaligns every later argument*, because the block holds a
    2-byte slot for each plain integer and a 4-byte slot for each `l`
    one (and for each pointer).

An unknown or malformed directive (`%z`, `%*d`, ...) is printed
literally and a `volamos: uae-debug:` warning goes to stderr; a
directive with no argument left prints `0` (plus a warning). The
`%[CYCLES]x` form UAE offers is not supported (see below).

### Using it from a program

C, with the stack protocol (`DBG` is a `volatile` pointer so the
compiler can't merge or reorder the stores):

```c
#define DBG ((volatile unsigned long *)0xBFFF00)

static void dbg(const char *fmt) { DBG[1] = (unsigned long)fmt; }   /* +4 */

/* arguments are pushed in order, then the format prints them */
DBG[0] = stage;                      /* %d  (16-bit)  */
DBG[0] = flags;                      /* %lx (32-bit)  */
DBG[0] = (unsigned long)name;        /* %s            */
dbg("boot: stage=%d flags=$%08lx name=%s\n");
```

(On a 68000 the compiler emits a `move.l` and volamos splits it into
the word pair, so the same code works on every `--cpu`.) 68000
assembly:

```
        move.l  #$FFFE,$BFFF00      ; push a value (prints as -2 with %d)
        move.l  #msg,$BFFF04        ; print "n=%d\n"
        ...
msg:    dc.b    "n=%d",10,0
```

Or collect the arguments in memory and use the `+8` form — handy for a
logging helper that already has a `va_list`-style block:

```
        move.l  #args,$BFFF08       ; switch to memory arguments
        move.l  #msg,$BFFF04        ; msg = "n=%d name=%s\n"
        rts
args:   dc.w    5                   ; %d  (2 bytes)
        dc.l    name                ; %s  (4 bytes)
```

### What it does to the run

- **Works at any `--ram`.** The port is intercepted on the CPU's store
  path *before* any memory lookup, so it is there with `--ram 1M` (where
  `0xBFFF00` isn't RAM at all) and with the default 16 MiB (where it
  is). A claimed store never reaches memory.
- **The JIT is off.** The trace JIT writes guest memory through a raw
  pointer that would bypass the port, so `--uae-debug` forces
  `--no-jit` — silently overriding an explicit `--jit`, exactly as
  [`--sanitize`](#-sanitize) does. It can be combined with
  `--clock-mhz`.
- **The 12 bytes are kept out of the guest heap.** When `0xBFFF00` lies
  inside RAM, `AllocMem` and friends will never hand out an address in
  `0xBFFF00..0xBFFF0C`, so `AvailMem` reports 12 bytes less (and
  `MEMF_LARGEST` the larger of the two pieces on either side). If the
  program image or the stack already covers that range (e.g. `--stack
  8M` with the default RAM) the run refuses to start with an error
  rather than corrupt memory — use a bigger `--ram` or a smaller
  `--stack`.
- **`--sanitize` is not bothered.** Stores to the port are legitimate
  device accesses and are never reported. Format strings and `%s`/`%b`
  arguments are read through the checked path, so passing a pointer to
  freed or out-of-bounds memory *is* reported.
- **Reads are not intercepted** (the port is write-only in UAE too): a
  read of `0xBFFF00..` returns whatever is in memory there.
- **Bounded.** Strings are read up to 4095 bytes (an unmapped byte
  ends one), and one message is at most 4095 bytes; nothing the guest
  stores can hang or crash volamos.

!!! note "Differences from UAE"
    volamos mirrors UAE's protocol, including its quirks, with these
    exceptions: missing arguments read as `0` (UAE prints whatever stale
    values earlier messages left on its stack); malformed directives
    are printed literally with a warning (UAE passes them to the host
    `printf`); `%[CYCLES]x` — which prints emulated cycles since the
    previous message — prints nothing and warns, as volamos has no
    cycle counter on this path; on a 68000 the `+8` pair uses the full
    combined pointer (UAE keeps only its low word).

### The `uaelib` debug-log trap

UAE also exposes a *function*: a call to the address `0xF0FF60` enters
its `uaelib` demultiplexer. volamos implements one mode of it, **mode
86**, which logs a string — the call the vscode-amiga-debug toolchain
uses. It is declared in amiberry's `uae_pragmas.h` as a C varargs
function, so from C:

```c
static int (*calltrap)(int arg0, ...) = (int (*)(int arg0, ...))0xF0FF60;

calltrap(86, "reached stage 2");
```

writes this line to **stderr** and returns `1`:

```console
$ volamos --uae-debug prog
DBG: reached stage 2
```

The convention is the plain C stack one, as in UAE: on entry `4(sp)`
is the mode and `8(sp)` the string pointer (more arguments follow at
`12(sp)`, ...); the result is returned in `D0` and the caller pops the
arguments. In assembly:

```
        pea     msg
        move.l  #86,-(sp)
        jsr     $F0FF60
        addq.l  #8,sp               ; D0 = 1
        ...
msg:    dc.b    "reached stage 2",0
```

- **Bad pointers return `0`** and log nothing: a pointer outside RAM.
  Like UAE, address `0` counts as valid and is read as a string (in
  volamos that is the start of the trap table, so a NULL pointer logs
  meaningless bytes rather than failing). Strings are read through the
  checked path up to 4095 bytes.
- **Other modes return `0` and warn.** Any mode other than 86 returns 0
  — what "not supported" looks like to a program probing for UAE — and
  prints one `volamos: uae-debug: unsupported uaelib mode N ...` line
  to stderr, once per distinct mode, so the probe is visible.
- **Needs `--ram` of at least 16 MiB** (the default). The trap lives at
  `0xF0FF60`, a code address that has to be inside RAM. With a smaller
  `--ram` a startup warning says the trap is inactive (the sprintf port
  still works) and a `jsr` there fails like any jump outside memory.
  Works on every `--cpu`, including the 24-bit 68000/68010.
- **Heap and stack.** The two bytes of the trap opcode are kept out of
  the guest heap; if the program image or the stack covers
  `0xF0FF60` (e.g. `--stack 1M` or more with the default 16 MiB RAM)
  the run refuses to start with an error.
- **JIT and `--sanitize`.** As above, the flag as a whole forces
  `--no-jit`. The trap's own opcode is never a `--sanitize` violation;
  the string is read through the checked path, so a pointer into freed
  memory *is* reported.
- Nested programs inherit it, and with the flag off nothing is planted:
  a `jsr` to `0xF0FF60` behaves as it always did.

## `--emu68-debug`

Enables **Emu68's debug byte port**. Emu68 (the PiStorm JIT) treats a
**byte-sized store to the absolute address `0xDEADBEEF`** as "print this
character" on its debug console; its own `kprintf` is built on it, so
software written for PiStorm often emits text this way. With the flag,
that byte is written to **stderr**, raw and exactly as stored — no
newline, no prefix, nothing buffered.

```console
$ volamos --emu68-debug --cpu 68020 prog
Hi
```

Off by default, and CLI-only — it is not settable from `~/.volamos`/
`.volamos`, so a stale config file can't start diverting stores. With
the flag off, a store to `0xDEADBEEF` behaves exactly as it always did
(a silently dropped out-of-range write). Nested `System()`/`Execute()`/
`RunCommand()` programs inherit it.

!!! warning "Needs a 32-bit CPU: use `--cpu 68020` or later"
    The 68000 and 68010 have a 24-bit address bus, and the CPU core
    presents `0xDEADBEEF` to memory as `0x00ADBEEF`, so the port never
    triggers there. Emu68 itself only targets 32-bit CPUs. Since
    `--cpu` defaults to 68000, `--emu68-debug` alone prints nothing —
    volamos prints a one-line warning to stderr at startup saying so
    rather than refusing to run.

- **Byte stores only.** Word and long stores to `0xDEADBEEF` are not
  special in Emu68 and are not here either; they fall through exactly as
  without the flag. Loads are not intercepted.
- **The address is fixed.** Emu68 also publishes it in its device tree
  for future discovery; volamos has no device tree.
- **The JIT is off.** Like [`--uae-debug`](#-uae-debug) and
  [`--sanitize`](#-sanitize), the flag forces `--no-jit` (silently
  overriding `--jit`), since the JIT's raw-pointer stores would bypass
  the port.
- **Works at any `--ram`.** The store is intercepted before any memory
  lookup. A `--ram` large enough to cover `0xDEADBEEF` (about 3.5 GiB)
  has that one byte kept out of the guest heap.
- **`--sanitize` is not bothered.** Stores to the port are never
  reported.
- **Combinable with [`--uae-debug`](#-uae-debug)**, which is a
  different mechanism (formatted text through stores to `0xBFFF00`); the
  two ports are independent.

### Using it from a program

```c
static void dbg_putc(char c) { *(volatile unsigned char *)0xDEADBEEF = c; }

static void dbg_puts(const char *s) { while (*s) dbg_putc(*s++); }
```

or in assembly:

```
        move.b  #'H',$DEADBEEF
        move.b  #'i',$DEADBEEF
        move.b  #10,$DEADBEEF
```

run with `volamos --emu68-debug --cpu 68020 prog`. Use `volatile` so the
compiler cannot merge or drop the stores.

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
