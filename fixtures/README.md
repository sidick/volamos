# fixtures/

Test fixtures for `volamos`'s Phase 1 hunk loader and (later) trap
dispatch. These are hand-authored, deliberately tiny, and not part of
the normal `cargo build`; they exist purely to be loaded and run by
`volamos-core`'s tests.

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
