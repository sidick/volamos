# fixtures/

Test fixtures for `volamos`'s hunk loader, trap dispatch, and
`dos.library`/`exec.library` handlers. These are hand-authored,
deliberately tiny, and not part of the normal `cargo build`; they exist
purely to be loaded and run by `volamos-core`'s tests.

## `hello`

A minimal two-hunk AmigaOS CLI executable. Source: `hello.s` (vasm mot
syntax). It:

1. Loads D1 with a pointer to a null-terminated string, `"Hello from
   volamos\n"`, held in its DATA hunk.
2. Calls **dos.library's `PutStr`** via a standard negative-offset LVO
   jump: `jsr _LVOPutStr(a6)`, i.e. `jsr -948(a6)` — offset **-948**
   (0x3B4 / 948 decimal) is `PutStr`'s real jump-table offset in
   dos.library. This is the *only* library call the program makes.
3. Sets `D0 = 0` (`moveq #0,d0`) as its process exit code.
4. Returns with a plain `rts`.

### Calling convention this fixture relies on

- **A6** is assumed to already hold a library base pointer when the
  program starts. For Phase 1 this is a fake dos.library base the
  runtime sets up specifically so the one `PutStr` LVO call can be
  trapped (illegal-instruction / A-line style) and dispatched to a
  hand-registered Rust handler. There's no real
  `OpenLibrary("dos.library",0)` call in this fixture — Phase 1 only
  fakes the single LVO it needs, per `docs/plan.md`'s T3/T4 scope.
- **Exit convention**: this program does not call `Exit()`. The runtime
  is expected to arrange, at process setup, that the return address
  sitting on the stack when the program starts points at an internal
  exit stub. A plain `rts` at the end of `main` then transfers control
  there with `D0` already holding the intended process exit code —
  the same shape a compiled C `main`'s epilogue produces.

### Byte layout of the `hello` binary

Two hunks, no relocations except one:

| Offset | Bytes | Meaning |
|---|---|---|
| `0x00` | `00 00 03F3` | `HUNK_HEADER` |
| `0x04` | `00 00 0000` | resident library name table: empty |
| `0x08` | `00 00 0002` | `table_size` = 2 hunks |
| `0x0C` | `00 00 0000` | `first_hunk` = 0 |
| `0x10` | `00 00 0001` | `last_hunk` = 1 |
| `0x14` | `00 00 0004` | hunk 0 size = 4 longwords (16 bytes) |
| `0x18` | `00 00 0005` | hunk 1 size = 5 longwords (20 bytes) |
| `0x1C` | `00 00 03E9` | `HUNK_CODE` |
| `0x20` | `00 00 0004` | code hunk size, 4 longwords |
| `0x24` | `22 3C` | `move.l #imm,d1` opcode |
| `0x26` | `00 00 0000` | immediate operand (placeholder; fixed up by the reloc below) |
| `0x2A` | `4E AE` | `jsr <disp16>(a6)` opcode |
| `0x2C` | `FC 4C` | displacement = -948 |
| `0x2E` | `70 00` | `moveq #0,d0` |
| `0x30` | `4E 75` | `rts` |
| `0x32` | `4E 71` | `nop` (alignment padding to a 4-byte hunk boundary) |
| `0x34` | `00 00 03EC` | `HUNK_RELOC32` |
| `0x38` | `00 00 0001` | 1 offset in this group |
| `0x3C` | `00 00 0001` | target hunk = 1 (the data hunk) |
| `0x40` | `00 00 0002` | offset 0x02 within hunk 0 (the immediate operand above) |
| `0x44` | `00 00 0000` | terminates the `HUNK_RELOC32` group list |
| `0x48` | `00 00 03F2` | `HUNK_END` (ends hunk 0) |
| `0x4C` | `00 00 03EA` | `HUNK_DATA` |
| `0x50` | `00 00 0005` | data hunk size, 5 longwords |
| `0x54` | `"Hello from volamos\n\0"` | 20 bytes, the message (already longword-aligned) |
| `0x68` | `00 00 03F2` | `HUNK_END` (ends hunk 1) |

Total file size: 108 bytes.

The program's entry point is the load address of hunk 0 (offset `0x24`
in the file, i.e. the `move.l` instruction).

### Regenerating

With `vasm` (`vasmm68k_mot`) available:

```sh
vasmm68k_mot -Fhunkexe -nosym -o fixtures/hello fixtures/hello.s
```

`vasm` was not available on the machine this fixture was authored on,
so the committed `fixtures/hello` binary was instead produced by hand
with `fixtures/gen_hello.py`, a heavily-commented Python script that
hand-assembles the exact same program byte-for-byte (each opcode word
and its encoding is explained inline). Regenerate it with:

```sh
python3 fixtures/gen_hello.py
```

If both `hello.s` and `gen_hello.py` exist, they're meant to describe
the *same* program; if you change one, update the other to match (or
just re-assemble with vasm and let it supersede the hand-assembled
version once vasm is available).

## Phase 2 (T14) fixtures: `filetest`, `dirtest`, `echoargs`

Three more hand-authored fixtures, in the same dual `.s` + `gen_*.py`
style as `hello`, added for Phase 2's file I/O / volumes-and-assigns
work (`docs/plan.md`'s T14, the phase's "done" criterion). Unlike
`hello` (which relies on a pre-seeded `A6`, a Phase 1 shortcut), all
three use the **real AmigaOS startup flow**: they read `AbsExecBase`
from guest address 4 (`move.l 4,a6`) and call
`OpenLibrary("dos.library", 0)` via `-552(a6)` themselves, exactly like
a real compiled program's startup code would, then use the returned
base in `A6` for every dos.library call.

### Shared assembler: `amiga_asm.py`

Hand-assembling three programs with branches and a loop (byte-exact,
without vasm) by literally computing every displacement by hand -- the
way `gen_hello.py` computes its one reloc -- doesn't scale. `amiga_asm.py`
is a tiny, purpose-built two-pass "assembler" (not general-purpose: one
method per instruction shape these three fixtures actually use, each
derived from the M68000 Programmer's Reference Manual's encoding tables,
same derivation style as `gen_hello.py`'s inline comments) that the three
`gen_*.py` scripts share: it tracks code/data labels, resolves branch
displacements (PC-relative, no relocation needed) and absolute pointers
into the data hunk (emitted as `HUNK_RELOC32` entries, exactly like
`gen_hello.py`'s single hand-written one) once every label's final
hunk-local offset is known. See its module docstring for the full
design.

Each fixture is a single CODE hunk + a single DATA hunk (same two-hunk
shape as `hello`).

### `filetest`

Source: `filetest.s`; generator: `gen_filetest.py`.

1. Real startup: `AbsExecBase` -> `OpenLibrary("dos.library", 0)` -> `A6`.
2. `Open("TEST:out.txt", MODE_NEWFILE)`. On failure (`D0 == 0`), PutStr
   a fixed `"ERR\n"` marker and exit with `D0 = 1` -- the simplest
   documented option in `docs/plan.md`'s T14 entry, rather than decoding
   `IoErr()` into a printed decimal number.
3. `Write` a fixed message string to it, `Close` it.
4. Reopen the same path `MODE_OLDFILE`, `Read` the same number of bytes
   back into a zeroed 64-byte scratch buffer, `Close` it.
5. `PutStr` the read-back buffer (already NUL-terminated -- the buffer
   is zero-filled and the message is well under 64 bytes) and exit 0.

Run with a volume mapping for `TEST:`, e.g.
`volamos -V TEST:/some/hostdir fixtures/filetest`, it prints the message
it wrote and reads back, and leaves `out.txt` on the host containing the
same bytes. Without any `-V`/`-a`/`--cwd`/`--auto-assign` flag at all (no
`Vfs` installed), `Open` always fails, so it prints `ERR` and exits 1 --
this is also how the fixture demonstrates `IoErr()`-driven failure.

### `dirtest`

Source: `dirtest.s`; generator: `gen_dirtest.py`.

1. Real startup (as above).
2. `Lock("TEST:dir", SHARED_LOCK)`. On failure, `"ERR\n"` + exit 1 (same
   convention as `filetest`).
3. `Examine(lock, fib)` to initialize the `ExNext` iterator, then loop:
   `ExNext(lock, fib)` until it returns `DOSFALSE` (no more entries).
   Each iteration copies `fib_FileName` (a NUL-terminated C string,
   `TEXT[108]`, at `fib+8` -- NDK `dos/dos.h`) into a scratch buffer as
   `"<name>\n\0"`, and `PutStr`s it.
4. `UnLock(lock)`, exit 0.

Run with a volume mapping providing a `TEST:dir` directory, e.g.
`volamos -V TEST:/some/hostdir fixtures/dirtest` (with a `dir`
subdirectory under `hostdir`); it prints one line per entry. Directory
enumeration order matches `crate::doslock`'s own (sorted byte-wise, for
deterministic output).

### `echoargs`

Source: `echoargs.s`; generator: `gen_echoargs.py`.

1. Real startup, saving the command-line pointer into `A2` *before* the
   `OpenLibrary` call, then reading it back from `A2` afterward --
   `OpenLibrary`'s own *documented* convention only says `A1` = name,
   `D0` = version, but `A0` is still a scratch register across any
   library call, and real Kickstart's `OpenLibrary` does clobber it even
   though volamos's own doesn't. **Found the hard way, via real
   Kickstart hardware (issue #63)**: an earlier revision read the
   command-line pointer back from `A0` after the call, assuming it
   survived -- worked under volamos, produced empty output on every real
   Kickstart ROM tested.
2. `PutStr(a2)`: the runtime (`Runtime::new` in
   `crates/volamos-core/src/dispatch.rs`) already leaves the guest
   command-line buffer `'\n'`-terminated *and* NUL-terminated, so it's
   already a valid `CString*` -- no copying needed.
3. Exit 0.

`volamos fixtures/echoargs foo bar` prints `foo bar \n` (a trailing
space before the newline -- real AmigaOS's own convention, also
confirmed via real Kickstart hardware, issue #63, and now matched here
too); with no guest args, the buffer is still just `"\n"` (no leading
space, and the trailing newline is unconditional), so it prints `\n`.

### Regenerating

Same rule as `hello`: with `vasm` (`vasmm68k_mot`) available,

```sh
vasmm68k_mot -Fhunkexe -nosym -o fixtures/filetest fixtures/filetest.s
vasmm68k_mot -Fhunkexe -nosym -o fixtures/dirtest  fixtures/dirtest.s
vasmm68k_mot -Fhunkexe -nosym -o fixtures/echoargs fixtures/echoargs.s
```

`vasm` was not available on the machine these fixtures were authored on
(same as `hello`), so each committed binary was produced instead by its
`gen_*.py` script:

```sh
python3 fixtures/gen_filetest.py
python3 fixtures/gen_dirtest.py
python3 fixtures/gen_echoargs.py
```

If you change a `.s` file, update its `gen_*.py` counterpart to match
(they're meant to describe the same program), or re-assemble with vasm
and let it supersede the hand-assembled version once vasm is available.

## Phase 3 (stage 7) fixture: `systest`

Source: `systest.s`; generator: `gen_systest.py` (same dual convention
and `amiga_asm.py` assembler as the Phase 2 fixtures).

1. Real startup (as above).
2. `SystemTagList("TEST:echoargs sys arg", NULL)` (`-606(a6)`, `D1` =
   command string, `D2` = `NULL` tag list): the runtime's host-side
   system runner resolves `TEST:echoargs` through the `Vfs`, loads it,
   and runs it to completion as a *nested* guest program -- its output
   (`sys arg\n`, see the `echoargs` section above) appears on stdout
   before anything the parent prints afterward.
3. If `SystemTagList`'s `D0` (the nested program's exit code, or -1 on
   failure to invoke) is nonzero, exit 99.
4. Otherwise `PutStr("after system\n")` and exit with the distinctive
   success code 42.

Run e.g. `volamos -V TEST:/dir/containing/echoargs fixtures/systest`;
`crates/volamos/tests/dosseg_e2e.rs` drives exactly that. Regenerate
with `python3 fixtures/gen_systest.py` (or vasm, same rule as above).

## vamos gap audit fixture: `runcmdtest`

Source: `runcmdtest.s`; generator: `gen_runcmdtest.py` (same dual
convention and `amiga_asm.py` assembler as the other fixtures above).
The `LoadSeg`+`RunCommand`+`UnLoadSeg` counterpart to `systest`'s
`SystemTagList()` test, added implementing `RunCommand` as part of
closing gaps found comparing volamos's `dos.library`/`exec.library`
coverage against vamos's own (`docs/plan.md`'s dated entry).

1. Real startup (as above).
2. `LoadSeg("TEST:echoargs")` (`-150(a6)`, `D1` = name string): resolves
   `TEST:echoargs` through the `Vfs`, reads and parses it, and builds a
   seglist. `D0` = the seglist's own `BPTR`, saved to `D1`.
3. `RunCommand(seg, stack=8192, paramptr="run cmd", paramlen=7)`
   (`-504(a6)`): the runtime's host-side system runner re-runs the
   program the seglist was loaded from as a *nested* guest program (the
   same nested-execution path `SystemTagList` uses, via
   `DosState::run_command` -- see `crate::dosseg`'s module docs), with
   `run`/`cmd` as its guest command-line args -- its output (`run
   cmd\n`, see the `echoargs` section above) appears on stdout before
   anything the parent prints afterward.
4. If `RunCommand`'s `D0` (the nested program's exit code, or -1 on
   failure to invoke) is nonzero, exit 99.
5. Otherwise `UnLoadSeg(seg)` (`-156(a6)`, `D1` still holds the seglist
   `BPTR`), `PutStr("after runcommand\n")`, and exit with the
   distinctive success code 43.

Run e.g. `volamos -V TEST:/dir/containing/echoargs fixtures/runcmdtest`;
`crates/volamos/tests/runcmdtest_e2e.rs` drives exactly that. Regenerate
with `python3 fixtures/gen_runcmdtest.py` (or vasm, same rule as above).

## Phase 3 (stage 8) fixtures: `exectest`, `recurse`

Two more fixtures, in the same dual `.s` + `gen_*.py` style, added for
Phase 3 stage 8 (`docs/plan.md`'s "fixtures + end-to-end tests" done
criterion): CLI-level coverage, through real hunk-loaded execution, for
the Phase 3 handlers that otherwise only had in-crate unit tests --
`exec.library`'s `AllocMem`/`FreeMem`/`AllocVec`/`FreeVec`
(`execmem.rs`), `utility.library` opened for real via `OpenLibrary`
(`utility.rs`), `exec.library`'s `FindTask`/`SetSignal` plus
`dos.library`'s `CheckSignal` (`exectask.rs`), and the guest
stack-overflow guard (also `exectask.rs`).

### `exectest`

Source: `exectest.s`; generator: `gen_exectest.py`.

1. Real startup: `AbsExecBase` -> `OpenLibrary("dos.library", 0)`
   (unchecked, matching every earlier fixture) -> `A3`, kept as
   *storage* only (not left as the active library base for calls --
   this fixture interleaves `exec.library` calls, needing `A6` =
   `EXEC_LIBRARY_BASE`, with `dos.library`/`utility.library` calls,
   needing `A6` = that library's own base; `A6` is swapped to the
   right base immediately before every `jsr`). **Found the hard way
   (2026-08-20, issue #6)**: an earlier revision left `A6` = `ExecBase`
   throughout and called `dos.library`/`utility.library` functions with
   their base in `A3`/`A4` directly, relying on volamos's trap
   dispatcher resolving purely from where a `jsr` physically lands
   rather than any "current A6" notion -- that worked under volamos,
   but crashed on real Kickstart: real `utility.library`'s
   `GetTagData` (confirmed via Copperline against a real ROM)
   internally depends on `A6` holding its own base for a nested call
   of its own. Always swapping `A6` to the real target base before
   every library call, like every other fixture already does, is the
   real-hardware-correct convention.
2. `AllocMem(64, MEMF_CLEAR)` via `A6` = `EXEC_LIBRARY_BASE`: checks
   non-NULL (exit 1 on failure) and that the first byte reads `0` (exit
   2 on failure), writes a byte pattern past it, then `FreeMem`s the
   original 64-byte block. `AllocVec(20, 0)`/`FreeVec` round trip (exit
   3 if `AllocVec` returns NULL).
3. `OpenLibrary("utility.library", 0)` via `A6` (exit 4 if NULL; this
   runtime always resolves that name to the fixed `UTILITY_LIBRARY_BASE`
   -- it's registered as a real library at `Runtime::new` time, never
   the auto-created-fake-library path) -> `A4`, then `A6` = `A4` for
   the calls below. `Stricmp("AMIGA", "amiga")` via `A6` (exit 5 if
   nonzero). `GetTagData` on a tag list built directly in the DATA hunk
   (`{TAG_VAL, 7}, {TAG_DONE, 0}`, via `amiga_asm.py`'s
   `DataBuilder.u32s`, expects `7` back, exit 6 otherwise).
   `Strnicmp("HELLO1", "HELLO2", 6)` (expects nonzero -- exit 7 if it
   wrongly reports equal). `A6` is then restored to `ExecBase` before
   step 4.
4. `FindTask(NULL)` via `A6` (exit 8 if NULL). `SetSignal(0, 0)`
   (unchecked read), then `SetSignal(1<<5, 1<<5)` to set bit 5, then
   `A6` = `A3` (dos.library's base) for `CheckSignal(1<<5)` (expects
   exactly `1<<5` back -- exit 9 otherwise).
5. On full success: `PutStr("exec ok\n")` via `A6` (still dos.library's
   base from the `CheckSignal` swap) and exit `0`.

Every failure path `PutStr`s a single fixed `"ERR\n"` marker (the
`filetest.s` convention) with a distinct nonzero exit code (1-9) per
checked step, rather than decoding the failure into printed text.

Run `volamos fixtures/exectest` -- no `-V`/`-a` flags needed, nothing
here touches the filesystem; it prints `exec ok` and exits `0`.
`crates/volamos/tests/phase3_e2e.rs` drives exactly that.

### `recurse`

Source: `recurse.s`; generator: `gen_recurse.py`.

An infinite loop: one cheap `dos.library` call (`PutStr` of a one-byte
message) per iteration -- the call that actually re-checks the guest
stack bounds, since `crate::exectask::check_stack_bounds` only runs once
per *dispatched trap*, never on a bare instruction -- followed by a
`bsr` back to the top of the loop, which is what actually grows the
stack: each `bsr` pushes a 4-byte return address that's never popped
(there's no matching `rts`; the loop never returns). Needs no new
addressing-mode support from `amiga_asm.py`: `BSR`'s word format
(`0110 0001 dddddddd`) is identical in shape to `BRA`/`BEQ`/`BNE`'s, so
`CodeBuilder.branch` handles it already -- `CodeBuilder.BSR` (added
alongside `BRA`/`BEQ`, plus a new `BNE`, for these two fixtures) is
just the right opcode-base constant, no new fixup logic.

Run with a small `--stack`, e.g. `volamos --stack 4096 fixtures/recurse`
(4096 is `volamos_core::MIN_STACK_SIZE`, the CLI's own clamp floor): it
prints roughly a thousand `x` lines, then exits nonzero with a "stack
overflow" diagnostic on stderr once `A7` runs below the current task's
stack bounds -- proving the guard (`docs/plan.md`'s "stack-overflow bug
class vamos is known to hit") fires loudly instead of letting the guest
silently corrupt memory past its stack. `crates/volamos/tests/
phase3_e2e.rs` drives exactly that.

### Regenerating

Same rule as the earlier fixtures: with `vasm` (`vasmm68k_mot`)
available,

```sh
vasmm68k_mot -Fhunkexe -nosym -o fixtures/exectest fixtures/exectest.s
vasmm68k_mot -Fhunkexe -nosym -o fixtures/recurse  fixtures/recurse.s
```

Without vasm (as on the machine these were authored on), regenerate the
hand-assembled versions with:

```sh
python3 fixtures/gen_exectest.py
python3 fixtures/gen_recurse.py
```

If you change a `.s` file, update its `gen_*.py` counterpart to match.

## Phase L3 fixtures: `testlib`, `testlib_initfail`, `libcall`

Added for `library-device-loading-plan.md`'s phase L3 (real disk-based
`OpenLibrary`): a hand-authored `RTF_AUTOINIT` library pair plus a CLI
client that opens and calls one of them. Unlike every fixture above,
`testlib`/`testlib_initfail` are **not run directly** -- they're loaded
via the real `OpenLibrary` disk-load path (`crates/volamos-core/src/
execlib.rs`), exactly like a real `.library` file on `LIBS:`.

### `testlib`

Source: `testlib.s`; generator: `gen_testlib.py`.

A tiny, genuine `struct Resident`-headed `RTF_AUTOINIT`/`NT_LIBRARY`
library, **all in a single CODE hunk** (the struct Resident, AUTOINIT
table, absolute-pointer vector table, and name strings all live in the
same hunk as the code -- the real on-disk shape of a vasm-built `.library`
file; see the `.s` file's header comment for the full byte layout). Six
vectors: Open (increments `lib_OpenCnt`, a real library's own job, not
volamos's), Close/Expunge/Reserved (trivial), and two user vectors --
`UserFunc` (LVO -30, `moveq #42,d0`) and `AddFunc` (LVO -36, `D0 = D0 +
D1`) -- executed *natively* by the CPU backend once opened, no host
dispatch involved. `InitFunc` proves it really ran (writes a marker into
`lib_Revision`), that `A0`/`D0` were passed per the AUTOINIT calling
convention (two marker longwords past `struct Library`'s own 34 bytes),
and that the L2 trampoline supports a *nested* library call mid-init (an
`AllocMem` call, its result also stored as a marker).

`crates/volamos-core/src/execlib.rs`'s `loaded_library_e2e` test module
drives this fixture through real A-line trap dispatch end to end.

### `testlib_initfail`

Source: `testlib_initfail.s`; generator: `gen_testlib_initfail.py`.

Identical shape to `testlib`, except its `initFunc` unconditionally
returns `NULL` (refuses the open). Exercises `execlib.rs`'s `after_init`
NULL-init-result cleanup path (seglist unload + `make_library` allocation
freed, nothing leaked -- see `loaded_library_e2e`'s heap-free-bytes test).

### `libcall`

Source: `libcall.s`; generator: `gen_libcall.py`.

A CLI client, real startup convention (`.s`'s header comment has the full
flow): reads its one command-line argument (the library name) into a
scratch buffer, `OpenLibrary`s it (works for either a bare name via
`LIBS:` or a full path -- the same binary exercises both, since
`OpenLibrary`'s own name-resolution logic is what tells them apart), calls
both `testlib` user vectors and checks their results, re-opens the same
library to check `lib_OpenCnt` reads back `2`, `CloseLibrary`s both opens
(currently a no-op for a loaded library -- forward-compatible, L4 wires
the real Close vector), and exits `0`. Prints `user ok\n`/`add ok\n`/
`cnt ok\n` on success; `open failed\n` + exit 10 if `OpenLibrary` returns
NULL; `bad\n` + exit 20 if any check fails.

Run e.g.
`volamos -V SYS:/dir/with/libs -a LIBS:SYS:libs fixtures/libcall test.library`
(with `dir/libs/test.library` present) or with `SYS:libs/test.library` as
the argument instead, to exercise the full-path open.
`crates/volamos/tests/libcall_e2e.rs` drives both, plus the
library-missing failure path.

### Regenerating

```sh
vasmm68k_mot -Fhunkexe -nosym -o fixtures/testlib          fixtures/testlib.s
vasmm68k_mot -Fhunkexe -nosym -o fixtures/testlib_initfail fixtures/testlib_initfail.s
vasmm68k_mot -Fhunkexe -nosym -o fixtures/libcall           fixtures/libcall.s
```

or, without vasm, the authoritative toolchain-free build:

```sh
python3 fixtures/gen_testlib.py
python3 fixtures/gen_testlib_initfail.py
python3 fixtures/gen_libcall.py
```

`testlib`/`testlib_initfail` use `amiga_asm.py`'s
`build_single_hunk_executable` (added for this phase) instead of
`build_hunk_executable`, since they're single-CODE-hunk files with
self-targeting `HUNK_RELOC32` fixups rather than a separate DATA hunk --
see `gen_testlib.py`'s module docstring and `amiga_asm.py`'s
`dc_w`/`dc_l_imm`/`dc_l_selfptr`/`dc_bytes`/`resolve_self` for the
mechanics, and the handful of new `CodeBuilder` instruction encodings
(`movem_l_to_predec`/`movem_l_from_postinc`, `move_w_imm_to_disp_a`,
`move_l_a_to_disp_a`/`move_l_d_to_disp_a`, `move_w_disp_a_to_d`,
`addq_w_disp_a`, `cmpi_b_imm_to_d`/`cmpi_l_imm_to_d`, `clr_b_ind`,
`add_l_d_to_d`) they needed. If you change a `.s` file, update its
`gen_*.py` counterpart to match.

## issue #65 fixture: `memtest`

Source: `memtest.s`; generator: `gen_memtest.py`. Added to validate
volamos's `--sanitize` memory-sanitizer mode (heap redzones + a free
quarantine around `exec.library`'s `AllocMem`/`FreeMem`) end to end: a
deliberately buggy CLI program whose bugs `--sanitize` must catch, and
whose one *correct* mode it must not flag.

### What it does

Real startup (`AbsExecBase` -> `OpenLibrary("dos.library", 0)` via
`-552(a6)`, unchecked, same convention as every fixture since
`filetest.s`). The command-line pointer (`A0`) is copied into `A2`
*before* that first library call, since `A0` is scratch across a `jsr`
(see `gen_libcall.py`'s comment, and `echoargs.s`'s header comment for
the trailing-space-before-newline convention a non-empty guest command
line carries -- both matter for correctly recognising a keyword's end).

A small `strmatch` subroutine (`bsr`'d once per candidate keyword,
`A1`=command-line cursor/`A0`=candidate keyword, returns `D0`=1/0) picks
one of eight modes by comparing the leading command-line word against
`"clean"`/`"overrun"`/`"underrun"`/`"uaf"`/`"uninit"`/`"uninitpartial"`/
`"written"`/`"cleared"`, requiring a space or newline immediately after
the match (so `"clean"` can't spuriously match a hypothetical
`"cleanup"` -- and, since `"uninit"` is a literal prefix of
`"uninitpartial"`, the same rule is what stops `"uninit"`'s own
candidate check from spuriously matching a `"uninitpartial"` command
line: the byte right after the `"uninit"` prefix is `'p'`, not a space
or newline, so `strmatch` correctly reports no match and dispatch falls
through to the `"uninitpartial"` candidate). No match -- including an
empty command line, which is just `"\n"` -- falls through to a usage
line.

Every mode `PutStr`s (`-948(a6)`) a short line naming what it's about to
do, then (except `usage`) calls `exec.library`'s `AllocMem` (`-198(a6)`,
`D0`=byte size, `D1`=requirements, returns `D0`=address or 0) for a
32-byte block, does its mode-specific access, then `FreeMem`s
(`-210(a6)`, `A1`=block, `D0`=byte size) the *same* 32 it allocated --
`crates/volamos-core/src/execmem.rs`'s `FreeMem` errors out loudly if
that size doesn't match what was actually allocated (both rounded up to
8), so getting this wrong would mask the fixture's own intended bug
behind an unrelated crash. A NULL `AllocMem` result is handled
uniformly (prints a failure line, exits 20) instead of dereferencing
NULL.

### Modes and expected `--sanitize` behaviour

| mode | what it does | expected sanitizer report |
|---|---|---|
| `clean` | alloc 32, write all 32 bytes, read all 32 back, free | **zero violations** -- the false-positive guard |
| `overrun` | alloc 32, write 1 byte at offset 32 (into the trailing redzone), free | heap-buffer-overflow **write** at `block+32` |
| `underrun` | alloc 32, read 1 byte at offset -1 (into the leading redzone), free | heap-buffer-overflow **read** at `block-1` |
| `uaf` | alloc 32, free it, read 1 byte at offset 0 of the freed block | **use-after-free read** at the freed block's start |

(no argument, or an unrecognised one) prints a usage line and exits 0.
Every mode exits `0` via a plain `rts` regardless of which bug it just
committed -- the *sanitizer's* job is to notice, not this program's own
exit code (`--sanitize` doesn't exist yet as of this writing; this
fixture and its expected-violations table above are what
`crates/volamos/tests/` will assert against once it lands).

Run e.g. `volamos fixtures/memtest clean` (no `-V`/`-a` needed -- nothing
here touches the filesystem).

### Uninitialized-read modes (issue #68) and expected `--sanitize-uninit` behaviour

Four more modes, added for issue #68's opt-in `--sanitize-uninit`
extra (uninitialized-heap-read detection, layered on top of
`--sanitize`'s existing redzone/free-quarantine checks -- see that
issue for why it's a separate flag rather than folded into
`--sanitize`: uninitialized-read detection is the noisiest class in
any sanitizer, and false-positive guards matter as much as the
positive cases). All four alloc without freeing anything unexpected
(same `AllocMem`/... /`FreeMem` shape as the other modes) and, per
`crates/volamos-core/src/execmem.rs`'s `MEMF_CLEAR` constant
(`1 << 16` = 65536), only `cleared` passes it:

| mode | what it does | expected under plain `--sanitize` | expected under `--sanitize-uninit` |
|---|---|---|---|
| `uninit` | alloc 32 bytes *without* `MEMF_CLEAR`, read offset 0 without ever writing it | nothing | one uninitialized-read report at `block+0` |
| `uninitpartial` | alloc 32 bytes without `MEMF_CLEAR`, write only offsets 0-15, read offset 20 (in the unwritten half) | nothing | one uninitialized-read report at `block+20` -- proves the detector is byte-granular, not per-allocation, since offsets 0-15 of this same block are genuinely `Valid` by the time of the read |
| `written` | alloc 32 bytes without `MEMF_CLEAR`, write all 32, read all 32 back | nothing | **nothing** -- false-positive guard: every byte read was written first |
| `cleared` | alloc 32 bytes *with* `MEMF_CLEAR`, read offset 0 without ever writing it | nothing | **nothing** -- second false-positive guard: `MEMF_CLEAR` memory is genuinely initialized (`exec.library` zeroed it), so it must never be reported as uninitialized even though this program itself never wrote it |

All four are silent under plain `--sanitize` today (verified by
actually running them -- see below), since `report_uninit` defaults
off; that silence is itself a useful check, proving these new modes
don't spuriously trip the *existing* redzone/free-quarantine
detectors. `--sanitize-uninit` did not exist yet as of this writing
(issue #68 is only adding the fixture coverage, not the flag itself),
so the `--sanitize-uninit` column above is the expected behaviour once
it lands -- not something observed directly.

Run e.g. `volamos fixtures/memtest uninit` or
`volamos --sanitize fixtures/memtest uninitpartial`.

### New `amiga_asm.py` encoders

Two new `CodeBuilder` instructions, added in the same style/rigor as
their neighbours: `move_b_imm_to_disp_a` (`move.b #imm,<disp16>(An)`,
same extension-word shape as `move_w_imm_to_disp_a` with the byte-size
opcode base) for `overrun`'s offset-32 poke, and `move_b_disp_a_to_d`
(`move.b <disp16>(An),Dn`, same shape as `move_w_disp_a_to_d`, byte-size
base) for `underrun`'s offset -1 read and `uaf`'s offset-0 read after
`FreeMem`.

### A real bug found while developing this fixture

The first version of `strmatch` used `tst.l d2`/`sub.l d2,d3` (full
32-bit compares) to test bytes loaded with `move.b (a0)+,d2` (which only
ever writes the *low* byte of `D2`). `D2` still held a leftover `0x2000`
from the `AllocMem` requirements setup earlier in a previous dispatch
attempt, so its high 24 bits were never actually zero -- `tst.l d2`
never saw a true zero at the candidate keyword's NUL terminator, so
*every* mode silently fell through to `usage` (caught by running the
fixture under `vamos` before trusting it: every one of `clean`/
`overrun`/`underrun`/`uaf` printed the usage line instead of its own
message). Fixed by explicitly zeroing `D1`/`D2` once before `strmatch`'s
comparison loop, so their untouched high bits stay zero throughout.

### Regenerating

`memtest.s` is written for, and was actually assembled with, the real
**PhxAss 4.40** assembler (Aminet freeware, not checked into this repo)
running *under `volamos` itself*, mapping a host directory as an Amiga
volume so PhxAss can read the source and write the binary:

```sh
mkdir -p /tmp/phxass_work && cp fixtures/memtest.s /tmp/phxass_work/
./target/debug/volamos -V work:/tmp/phxass_work ~/amiga/PhxAss/PhxAss work:memtest.s
cp /tmp/phxass_work/memtest fixtures/memtest
```

PhxAss emits a hunk **executable** directly (no linker or `EXE/S`
switch needed, since this program has no external references).

Since PhxAss isn't part of this repo and can't be relied on in CI (or
on a machine without it fetched from Aminet), `gen_memtest.py` (via
`amiga_asm.py`) remains the authoritative, byte-identical,
toolchain-free build actually committed as `fixtures/memtest`:

```sh
python3 fixtures/gen_memtest.py
```

**Cross-checked, not byte-identical, confirmed equivalent.** Both paths
were built and run (via `vamos` and via real `volamos`) for this issue;
they agree on every mode's output and exit code, but the *raw bytes*
differ: PhxAss's `(N)ormal Optimization` collapses several `bra`/`beq`/
`bne`/`bsr` word-form branches (`amiga_asm.py`'s `CodeBuilder.branch`
always emits the full displacement-word form, matching every other
fixture's hand-assembler convention) into their one-word short (8-bit
displacement) forms -- PhxAss itself reported "Bytes gained by
optimization: 28" for this exact source. Structurally the two binaries
are identical (`HUNK_HEADER`/`CODE`/`RELOC32`/`END`/`DATA`/`END`, same
number of relocations, same relocation targets once `memtest.s`'s data
section was declared `section data,data` -- PhxAss's `SECTION` directive
defaults an unqualified `section data` to a **CODE** section, unlike
`vasm`; the plain `section code`/`section data` pairing every other
`.s` file here uses is a `vasm`-specific convention that doesn't carry
over) -- only the branch-instruction encodings and the resulting code
hunk size (`gen_memtest.py`: 784 bytes total; PhxAss: 764 bytes) differ,
exactly as expected for "same program, different assembler
optimization level," not a logic bug. If you change `memtest.s`, update
`gen_memtest.py` to match (or vice versa), and re-run both builds plus
the `vamos`/`volamos` mode sweep above before trusting the result.

Re-verified for issue #68's four new modes (`uninit`/`uninitpartial`/
`written`/`cleared`): after PhxAss reported "0 errors" and "Bytes
gained by optimization: 36" (`gen_memtest.py`: 1452 bytes total;
PhxAss: 1420 bytes -- both grew from the prior 784/764 by the same
four new modes' worth of code and data), all ten command lines
(`clean`/`overrun`/`underrun`/`uaf`/`uninit`/`uninitpartial`/`written`/
`cleared`/empty/an unrecognised keyword) were run against both builds
under plain `volamos` and produced byte-identical stdout and exit code
0 in every case, and against both builds under `volamos --sanitize`:
`overrun`/`underrun`/`uaf` still reported their existing redzone/
free-quarantine violations, and the four new modes reported nothing
(expected, since `--sanitize-uninit` doesn't exist yet -- see the
"Uninitialized-read modes" section above).

## issue #65 increment 2 fixture: `stacktest`

Source: `stacktest.s`; generator: `gen_stacktest.py` (same dual
convention, shared `strmatch` dispatch idiom, and `amiga_asm.py`
assembler as `memtest.s`/`gen_memtest.py`). Added to validate
volamos's `--sanitize` mode's two **stack** detectors, on top of
`memtest`'s heap coverage (kept separate rather than growing `memtest`,
since `memtest`'s own two build paths are already in sync and this is a
distinct concern):

1. **Below-stack-pointer accesses** -- everything below the current SP
   is dead memory; reading or writing there means a program is using a
   released stack frame or overrunning downward.
2. **Return-address corruption** -- a shadow call stack records the
   return address each `JSR`/`BSR` pushes and verifies it at the
   matching `RTS`.

### What it does

Real startup (`AbsExecBase` -> `OpenLibrary("dos.library", 0)` via
`-552(a6)`, unchecked), identical command-line-cursor-in-`A2` and
`strmatch` dispatch mechanism as `memtest.s` (see that section above for
the full derivation -- copied verbatim into `stacktest.s`, not
re-derived). `A3` = dos.library base, `A4` = `ExecBase` (read but
otherwise unused -- this fixture makes no `exec.library` calls; that's
`memtest`'s job), `A6` swapped to the target library base immediately
before every `jsr` (the `exectest.s`/issue #6 convention), `A5` used as
the frame pointer for `clean`'s `LINK`/`UNLK` frames (deliberately not
`A6`, which stays reserved for library-base swapping).

### Modes and expected `--sanitize` behaviour

| mode | what it does | expected sanitizer report |
|---|---|---|
| `below` | read 1 byte at `sp-128`, without moving `sp` | below-stack-pointer **read** violation at `sp-128` |
| `smash` | `bsr` into a subroutine that overwrites its own just-pushed return address with the address of a real label, then `rts`'s | return-address-corruption violation reported at the matching `rts`, **and** the process still completes normally (see design note below) |
| `clean` | three nested `bsr`/`rts` levels, each a `link`/`unlk` frame with a local variable written and read at a fixed frame-pointer-relative displacement | **zero violations** -- ordinary, textbook-correct code |
| `pushret` | `move.l #target,-(sp)` / `rts`, the classic m68k computed-jump idiom, with **no matching `jsr`/`bsr` anywhere** | **zero violations** -- the single most important false-positive guard in this fixture; the shadow call stack must recognise an `rts` with nothing to match on the call stack as legitimate, not as corruption |
| `deep` | `bsr`-recurse 64 levels deep (each level also `movem.l`-pushes/pops `D0`, so real `SP` movement happens at every level), unwind cleanly | **zero violations** -- exercises call-stack depth bookkeeping and below-SP delta tracking under a lot of genuine, fully-balanced `SP` movement |

(no argument, or an unrecognised one) prints a usage line and exits 0.
Every mode exits `0` via a plain `rts` (`--sanitize`'s stack detectors
don't exist yet as of this writing -- this fixture and the table above
are what a future `crates/volamos/tests/` case will assert against once
they land; **not observed firing**, only reasoned about from the
program's own construction).

Run e.g. `volamos fixtures/stacktest clean` (no `-V`/`-a` needed --
nothing here touches the filesystem). Verbatim output for every mode,
from the current build:

```
$ ./target/debug/volamos fixtures/stacktest
usage: stacktest below|smash|clean|pushret|deep
$ ./target/debug/volamos fixtures/stacktest below
below: reading 128 bytes below the current stack pointer
$ ./target/debug/volamos fixtures/stacktest smash
smash: corrupting a return address, landing on valid code
$ ./target/debug/volamos fixtures/stacktest clean
clean: three nested link/unlk frames with locals
$ ./target/debug/volamos fixtures/stacktest pushret
pushret: move.l #target,-(sp) / rts, no matching bsr
$ ./target/debug/volamos fixtures/stacktest deep
deep: 64 levels of clean bsr/rts recursion
```

Every invocation above exits `0`, including `smash` -- confirming the
overwritten return address really did land on `smash_landing` rather
than falling through to the canary exit code 98 that only runs if the
overwrite silently failed.

### The `smash` design constraint

`smash`'s corrupted return address is the address of a real code label
(`smash_landing`), never garbage like `$DEADBEEF`. Overwriting a return
address with an arbitrary/invalid address would send the CPU's PC into
unmapped or nonsense memory and likely crash the run before it
finished -- useless for an automated test that wants to assert *both*
"the process ran to completion and exited 0" *and* "the sanitizer
reported a violation for the corrupted slot." Landing on a real label
keeps the *program* well-behaved while still corrupting the *return
address slot* the shadow call stack is watching -- exactly the shape the
detector needs to catch, and the only shape a fully-automated test can
assert against without also racing a segfault.

### New `amiga_asm.py` encoders

Five new `CodeBuilder` instructions, added in the same style/rigor as
their neighbours:

- `link_a`/`unlk_a` -- `LINK An,#disp16`/`UNLK An`, for `clean`'s stack
  frames.
- `move_l_imm_to_disp_a` -- `move.l #imm,<disp16>(An)`, the long-sized
  sibling of the existing `move_w_imm_to_disp_a`/`move_b_imm_to_disp_a`
  (same extension-word order: immediate first, then displacement), for
  writing a `link`-frame local.
- `move_l_codelabel_to_ind_a` -- `move.l #label,(An)`, where `label` is
  a **code** label (resolved against the CODE hunk itself, hunk 0,
  unlike every earlier `move_l_label_to_*` helper's DATA-hunk, hunk 1,
  labels). Used by `smash` to overwrite a just-pushed return address in
  place.
- `move_l_codelabel_to_predec_a` -- `move.l #label,-(An)`, same
  code-label-pointer mechanism with predecrement addressing; for `An=7`
  this is the well-known `0x2F3C <imm32>` encoding real Amiga
  trampolines use. Used by `pushret`.

Since two of these need an absolute pointer into the *code* hunk rather
than the data hunk, `CodeBuilder.resolve`/`build_hunk_executable` were
generalized (backward-compatibly -- every existing fixture generator
was re-run and produces byte-identical output, see below) to support
`HUNK_RELOC32` groups targeting more than one hunk, since the format
already supports multiple `(count, hunk, offsets...)` groups before the
terminating zero count.

**Verified byte-identical against real PhxAss's own output**, per this
issue's own requirement (same cross-check discipline as `memtest`'s
encoders). A tiny probe source exercising all five new instructions was
assembled through PhxAss under `volamos`:

```
        section code
start:
        link    a5,#-8
        move.l  #$11111111,-8(a5)
        move.l  #target,(a7)
        move.l  #target,-(a7)
        unlk    a5
        rts
target:
        moveq   #0,d0
        rts
        section data,data
dummy:
        dc.b    0
        even
```

PhxAss's code-hunk payload (32 bytes, offset `0x24` of the output file):

```
4e55 fff8 2b7c 1111 1111 fff8 2ebc 0000 001c 2f3c 0000 001c 4e5d 4e75 7000 4e75
```

`amiga_asm.py`'s `CodeBuilder` (`link_a(5,-8)`,
`move_l_imm_to_disp_a(5,-8,0x11111111)`,
`move_l_codelabel_to_ind_a(7,"target")`,
`move_l_codelabel_to_predec_a(7,"target")`, `unlk_a(5)`, `rts()`,
`moveq(0,0)`, `rts()`), run through the same `build_hunk_executable`,
produced the identical 32-byte code-hunk payload byte-for-byte:

```
4e55 fff8 2b7c 1111 1111 fff8 2ebc 0000 001c 2f3c 0000 001c 4e5d 4e75 7000 4e75
```

`4e55`=`link a5,#-8`; `2b7c`=`move.l #imm,-8(a5)` (long-size MOVE with
dest mode `d16(An)`); `2ebc`/`2f3c`=`move.l #imm,(a7)`/`move.l
#imm,-(a7)` respectively, both pointing at the same `target` offset
(`0x1c` bytes into the hunk, confirming the code-label fixup resolved
correctly); `4e5d`=`unlk a5`. Match confirmed with a byte-for-byte
Python comparison, not eyeballing.

### Regenerating

`stacktest.s` is written for, and was actually assembled with, the real
**PhxAss 4.40** assembler running *under `volamos` itself*, same
convention as `memtest.s`:

```sh
mkdir -p /tmp/phx && cp fixtures/stacktest.s /tmp/phx/
./target/debug/volamos -V work:/tmp/phx ~/amiga/PhxAss/PhxAss work:stacktest.s
cp /tmp/phx/stacktest fixtures/stacktest
```

PhxAss emits a hunk **executable** directly (no linker or `EXE/S`
switch needed, since this program has no external references).
PhxAss reported "Bytes gained by optimization: 36" for this source, no
errors.

Since PhxAss isn't part of this repo and can't be relied on in CI (or
on a machine without it fetched from Aminet), `gen_stacktest.py` (via
`amiga_asm.py`) remains the authoritative, byte-identical (for its own
instruction encodings; see below), toolchain-free build actually
committed as `fixtures/stacktest`:

```sh
python3 fixtures/gen_stacktest.py
```

**Cross-checked, not byte-identical, confirmed equivalent** -- same
relationship as `memtest`'s two builds. Both paths were built and every
mode run through `volamos` for this issue; they agree on every mode's
output and exit code (including `smash` exiting `0`, not its canary 98,
under both builds). The raw bytes differ for the same reason as
`memtest`: PhxAss's optimizer collapses several word-form `bra`/`beq`/
`bne`/`bsr` branches into short 8-bit-displacement forms that
`amiga_asm.py`'s `CodeBuilder.branch` never emits. Structurally the two
binaries are identical -- same hunk count and order
(`HUNK_HEADER`/`CODE`/`RELOC32`/`END`/`DATA`/`END`), same 90-longword
(360-byte) data hunk, and the exact same `HUNK_RELOC32` group split (2
pointers targeting hunk 0 -- the two code-label pointers `smash`/
`pushret` need -- and 12 pointers targeting hunk 1, the data hunk) --
only the code hunk's own size (`gen_stacktest.py`: 98 longwords/392
bytes; PhxAss: 90 longwords/360 bytes) differs, exactly as expected for
"same program, different assembler optimization level." If you change
`stacktest.s`, update `gen_stacktest.py` to match (or vice versa), and
re-run both builds plus the mode sweep above before trusting the
result.

## issue #58 fixture: `matchflags`

Source: `matchflags.s`; generator: `gen_matchflags.py` (same dual
convention and `amiga_asm.py` assembler as `memtest.s`/`stacktest.s`).
Added to settle a one-bit divergence between `volamos` and `vamos`
during a `MatchFirst`/`MatchNext` directory scan: `APF_DirChanged` (bit
6, value 64) in `ap_Flags` -- `volamos` sets it, `vamos` never does.
Real `dos.library`'s intent is that `dos.library` sets this flag itself
to tell the caller the reported directory has changed since the
previous call. Every earlier fixture that exercises `AnchorPath`
(there isn't one before this) would only ever scan a single, flat
directory, which can't distinguish "set once per real directory
transition" (correct) from "set on every entry" (too eager) -- there's
only one directory to have a flag about. `matchflags` scans a real
**multi-level** tree (`APF_DODIR` set, re-requested before every
`MatchNext` -- see below) specifically so the two possibilities produce
visibly different output.

### What it does

Real startup (`AbsExecBase` -> `OpenLibrary("dos.library", 0)` via
`-552(a6)`, unchecked). Takes one command-line argument (the directory
to scan), copied up to (not including) the first space/newline/NUL into
a scratch buffer (same token-copy idiom as `memtest.s`'s/`stacktest.s`'s
command parsing, without their multi-keyword dispatch); an empty/missing
argument prints a usage line and exits 0.

`AllocMem(536, MEMF_CLEAR)` (536 = 280, the fixed `AnchorPath` header
through `ap_Info`, plus a 256-byte `ap_Buf` tail) for the `AnchorPath`;
NULL exits 20. `ap_Strlen` (word at `+18`) is set to 256 so `ap_Buf`
gets filled in; `ap_Flags` (byte at `+16`) is set to `APF_DODIR` (4) so
the scan descends into subdirectories. `MatchFirst(dirarg, ap)` (`-822`);
nonzero `D0` prints "MatchFirst failed" and exits 10.

Loop: for the entry currently in `*ap`, print `flags=%ld name='%s'
buf='%s'\n` via `VPrintf` (`-954`, `D1`=format, `D2`=a small 3-longword
array built fresh each time: `ap_Flags` zero-extended from a **byte**
read at `+16` -- not a longword read -- plus `&fib_FileName`
(`ap_Info+8` == `AnchorPath+28`) and `&ap_Buf` (`AnchorPath+280`), both
computed with `lea` since they're a fixed displacement off the
`AllocMem`'d, so only known at runtime, `AnchorPath` base). Quoting the
strings (`name='%s'`) matters: a blank `fib_FileName` is itself
significant here (a volume-root `MatchFirst` report has one, per issue
#58's own Finding 1), and an unquoted empty field would be invisible in
a diff. Then re-sets `ap_Flags` to `APF_DODIR` (a plain overwrite, not a
read-modify-write OR -- ap_Flags was already read and printed for this
entry, so there's nothing else left to preserve) before calling
`MatchNext` (`-828`) again: `MatchNext` clears `APF_DODIR` itself once
consumed for a descent (see `crates/volamos-core/src/dosanchor.rs`'s
module docs), so getting a *multi-level* scan -- the entire point of
this fixture -- means re-requesting it before every call, exactly like
the NDK's own `ScanDirectories()` worked example. Loops until
`MatchNext` returns nonzero, then `MatchEnd` (`-834`), `FreeMem`s the
`AnchorPath`, and exits 0.

### Verbatim output

Test tree:

```
/tmp/mftree/a/aa/file1
/tmp/mftree/a/ab/file2
/tmp/mftree/b/file3
/tmp/mftree/file4
```

```
$ ./target/debug/volamos -V TEST:/tmp/mftree fixtures/matchflags TEST:
flags=4 name='' buf='TEST:'
flags=64 name='a' buf='TEST:a'
flags=64 name='aa' buf='TEST:a/aa'
flags=64 name='file1' buf='TEST:a/aa/file1'
flags=76 name='aa' buf='TEST:a/aa/'
flags=4 name='ab' buf='TEST:a/ab'
flags=64 name='file2' buf='TEST:a/ab/file2'
flags=76 name='ab' buf='TEST:a/ab/'
flags=76 name='a' buf='TEST:a/'
flags=4 name='b' buf='TEST:b'
flags=64 name='file3' buf='TEST:b/file3'
flags=76 name='b' buf='TEST:b/'
flags=4 name='file4' buf='TEST:file4'
flags=76 name='' buf='TEST:'
```

(exit `0`). `64` = `APF_DirChanged` alone; `76` = `64 | 8 | 4`
(`APF_DirChanged | APF_DIDDIR`, plus this fixture's own literal
`APF_DODIR` re-request still sitting in the byte, unconsumed by a pop).
Under `volamos`, `APF_DirChanged` appears **exactly once per real
directory transition** -- e.g. `b`'s three entries (only one here,
`file3`) never repeat it, and stepping sideways from the exhausted `aa`
back up and across into the sibling `ab` (`flags=4`, no `64`) does not
set it either -- never once per *entry*. A flat directory can't show
this at all (there's only one directory, so `APF_DirChanged` can only
ever fire on the very first entry or never): run for comparison against
a flat directory of three files:

```
$ ./target/debug/volamos -V TEST:/tmp/flatdir fixtures/matchflags TEST:
flags=4 name='' buf='TEST:'
flags=64 name='one' buf='TEST:one'
flags=4 name='three' buf='TEST:three'
flags=4 name='two' buf='TEST:two'
flags=76 name='' buf='TEST:'
```

(`/tmp/flatdir` containing `one`/`two`/`three`, no subdirectories --
`APF_DirChanged` fires once, on the first real child, then never again
for its two siblings, exactly as the nested tree's own siblings behave.)

```
$ ./target/debug/volamos fixtures/matchflags
usage: matchflags <dir>
$ ./target/debug/volamos -V TEST:/tmp/mftree fixtures/matchflags TEST:doesnotexist
MatchFirst failed
```

(both exit `0` and `10` respectively). This says nothing about whether
`volamos`'s "once per transition" behavior or `vamos`'s "never" is the
one matching real `dos.library` -- that's what this fixture exists to
let a real-Kickstart-hardware run (Copperline) settle.

### A real bug found while developing this fixture

`gen_matchflags.py`'s `DataBuilder.cstr()` (used by every fixture's
generator) never pads a string to an even offset the way a real
assembler's `even` directive does -- harmless for every earlier fixture,
since none of them ever took a raw address of a DATA-hunk label and
issued a real CPU long-word `move.l` into it directly (heap-allocated
pointers, the only prior target of such writes, are always aligned by
the allocator). `matchflags` is the first to do exactly that (writing
`ap_Flags`/`&fib_FileName`/`&ap_Buf` into `argarray`), so an odd-length
string placed earlier in the data hunk -- even a single added 3-byte
`cstr` -- could silently misalign `argarray` onto an odd address, and a
real 68000 long-word write there is an Address Error: the CPU stopped
with an "Illegal" opcode fault deep inside `print_entry`, at a PC that
didn't correspond to this program's own code at all. Found via
`--sanitize` (which flagged the resulting corruption as invalid heap-
redzone writes from a nonsensical PC) plus a battery of bisection micro-
fixtures narrowing it down to exactly this cause; `matchflags.s`'s own
PhxAss build was never affected, since PhxAss's `even` directives (after
every `dc.b` block) keep everything aligned automatically. Fixed with an
explicit `data.align4()` right before `dirarg`/`argarray` in
`gen_matchflags.py` -- see that call's own comment for the full
derivation.

### New `amiga_asm.py` encoder

One new `CodeBuilder` instruction: `lea_disp_a_to_a` (`lea
<disp16>(An_src),An_dest`), needed because `&fib_FileName`/`&ap_Buf` are
a fixed displacement off the `AllocMem`'d (so only known at runtime)
`AnchorPath` base -- an address to *compute*, not a DATA-hunk label a
`move_l_label_to_a`-style relocation could bake in. **Verified
byte-identical against real PhxAss's own output**: a probe source

```
        section code
start:
        lea     28(a5),a0
        lea     280(a5),a1
        rts
        section data,data
dummy:
        dc.b    0
        even
```

assembled through PhxAss under `volamos`, whose code-hunk payload (10
bytes) is `41ed 001c 43ed 0118 4e75 4e71`; `amiga_asm.py`'s
`CodeBuilder` (`lea_disp_a_to_a(0,28,5)`, `lea_disp_a_to_a(1,280,5)`)
produces the identical words `41ed 001c 43ed 0118` for the two `lea`
instructions themselves, byte-for-byte (`4e75`/`4e71` are the probe's
own `rts`/pad, not part of the encoder being checked).

### Regenerating

`matchflags.s` is written for, and was actually assembled with, the real
**PhxAss 4.40** assembler running *under `volamos` itself*, same
convention as `memtest.s`/`stacktest.s`:

```sh
mkdir -p /tmp/phxm && cp fixtures/matchflags.s /tmp/phxm/
./target/debug/volamos -V work:/tmp/phxm ~/amiga/PhxAss/PhxAss work:matchflags.s
cp /tmp/phxm/matchflags fixtures/matchflags
```

PhxAss emits a hunk **executable** directly (no linker/`EXE/S` switch
needed), reporting "Bytes gained by optimization: 20" for this source,
no errors.

Since PhxAss isn't checked into this repo (Aminet freeware living
outside it) and can't be relied on in CI, `gen_matchflags.py` (via
`amiga_asm.py`) remains the authoritative, byte-identical, toolchain-
free build actually committed as `fixtures/matchflags`:

```sh
python3 fixtures/gen_matchflags.py
```

**Cross-checked, not byte-identical, confirmed equivalent** -- same
relationship as `memtest`/`stacktest`'s two builds: both paths were
built and run through `volamos` against both the nested tree and the
flat directory above, and produce byte-identical stdout and the same
exit codes under both builds. The raw bytes differ for the same
optimization-level reason as the other two fixtures: `fixtures/
matchflags` (the toolchain-free build) is 760 bytes; PhxAss's own build
is 744 bytes. Structurally identical
(`HUNK_HEADER`/`CODE`/`RELOC32`/`END`/`DATA`/`END`) -- only
`amiga_asm.py`'s always-word-form branches (`CodeBuilder.branch` never
emits PhxAss's short 8-bit-displacement forms) account for the size
difference. If you change `matchflags.s`, update `gen_matchflags.py` to
match (or vice versa), and re-run both builds plus the tree/flat/usage/
bad-path sweep above before trusting the result.
