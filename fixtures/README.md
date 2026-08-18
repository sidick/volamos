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
   Each iteration copies `fib_FileName` (a BSTR at `fib+8`: one length
   byte then that many data bytes, *not* NUL-terminated) into a scratch
   buffer as `"<name>\n\0"`, and `PutStr`s it.
4. `UnLock(lock)`, exit 0.

Run with a volume mapping providing a `TEST:dir` directory, e.g.
`volamos -V TEST:/some/hostdir fixtures/dirtest` (with a `dir`
subdirectory under `hostdir`); it prints one line per entry. Directory
enumeration order matches `crate::doslock`'s own (sorted byte-wise, for
deterministic output).

### `echoargs`

Source: `echoargs.s`; generator: `gen_echoargs.py`.

1. Real startup (as above; `OpenLibrary`'s own calling convention -- `A1`
   = name, `D0` = version -- doesn't touch `A0`, so the command-line
   pointer AmigaOS startup convention hands the program survives).
2. `PutStr(a0)` directly: the runtime (`Runtime::new` in
   `crates/volamos-core/src/dispatch.rs`) already leaves the guest
   command-line buffer `'\n'`-terminated *and* NUL-terminated, so `A0` is
   already a valid `CString*` -- no copying needed.
3. Exit 0.

`volamos fixtures/echoargs foo bar` prints `foo bar\n`; with no guest
args, the buffer is still just `"\n"` (the trailing newline is
unconditional), so it prints `\n`.

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
