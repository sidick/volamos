#!/usr/bin/env python3
"""Hand-assembles fixtures/hello.s into an AmigaOS hunk executable.

This exists because `vasm` (vasmm68k_mot) was not available on the
machine this fixture was authored on. It emits, byte for byte, the same
program `fixtures/hello.s` describes: hand-assembled 68000 machine code
for a two-hunk (CODE + DATA) hunk executable, with a HUNK_RELOC32 fixup
for the one address reference (the string pointer loaded into D1).

If vasm becomes available later, prefer regenerating with:

    vasmm68k_mot -Fhunkexe -nosym -o fixtures/hello fixtures/hello.s

and only fall back to this script if that's not an option. If you change
hello.s, update this script to match (or replace this whole file with a
vasm build once it's available).

Run directly to (re)write fixtures/hello:

    python3 fixtures/gen_hello.py
"""

import pathlib
import struct

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "hello"


def u32(value: int) -> bytes:
    """Packs `value` as a big-endian 32-bit word (a hunk-format "longword").

    All 68k / hunk-format multi-byte values are big-endian, regardless of
    the host machine building this fixture.
    """
    return struct.pack(">I", value & 0xFFFF_FFFF)


def u16(value: int) -> bytes:
    """Packs `value` as a big-endian 16-bit word."""
    return struct.pack(">H", value & 0xFFFF)


# --- Hunk type identifiers (AmigaOS hunk file format) ---
HUNK_HEADER = 0x3F3
HUNK_CODE = 0x3E9
HUNK_DATA = 0x3EA
HUNK_RELOC32 = 0x3EC
HUNK_END = 0x3F2


# ---------------------------------------------------------------------
# Hunk 1 (DATA): the null-terminated message string PutStr will print.
# ---------------------------------------------------------------------
#
# "Hello from volamos\n" is 19 characters; plus the NUL terminator that's
# 20 bytes, which is already a multiple of 4 -- no padding needed to keep
# the hunk longword-aligned (the hunk format requires hunk sizes to be a
# whole number of longwords).
MESSAGE = b"Hello from volamos\n\x00"
assert len(MESSAGE) % 4 == 0, "data hunk must be a whole number of longwords"
DATA_HUNK_LONGWORDS = len(MESSAGE) // 4

# ---------------------------------------------------------------------
# Hunk 0 (CODE): the program itself.
# ---------------------------------------------------------------------
#
# Equivalent assembly (see fixtures/hello.s for the fully commented
# version and the calling convention this relies on):
#
#     move.l  #msg,d1     ; D1 = pointer to the message string
#     jsr     -948(a6)    ; call dos.library/PutStr (_LVOPutStr) via A6
#     moveq   #0,d0        ; D0 = exit code 0
#     rts                   ; return to the runtime's exit stub
#     nop                   ; padding, see below
#
# Each instruction's encoding, worked out against the M68000 Programmer's
# Reference Manual instruction set summary:
#
# 1. `move.l #imm,d1` -- MOVE encoding is:
#        15-14: 00            (MOVE opcode class)
#        13-12: size = 10     (long)
#        11-9 : dest reg =  001 (D1)
#        8-6  : dest mode = 000 (data register direct)
#        5-3  : src mode  = 111 (immediate/other addressing modes)
#        2-0  : src "register" = 100 (this selects immediate data
#                                      specifically, within mode 111)
#    Bits: 00 10 001 000 111 100 -> grouped into nibbles: 0010 0010 0011
#    1100 = 0x223C. Followed by the 32-bit immediate operand itself (the
#    address of `msg`), which needs a HUNK_RELOC32 fixup since the
#    linker/loader -- not this script -- decides where hunk 1 actually
#    lands in memory.
#
# 2. `jsr -948(a6)` -- JSR with an address-register-indirect-with-
#    displacement operand:
#        15-6: 0100111010 (JSR opcode, effective-address mode fixed at
#              "control addressing" dispatch)
#        5-3 : mode = 101 (address register indirect with 16-bit
#              displacement)
#        2-0 : register = 110 (A6)
#    Bits: 0100 1110 1010 1110 = 0x4EAE, followed by a 16-bit
#    displacement word. -948 decimal (dos.library's PutStr LVO offset)
#    as a 16-bit two's-complement value is 0x10000 - 948 = 0xFC4C.
#
# 3. `moveq #0,d0` -- MOVEQ encoding:
#        15-12: 0111 (MOVEQ opcode)
#        11-9 : register = 000 (D0)
#        8    : 0
#        7-0  : 8-bit signed immediate data = 0x00
#    Bits: 0111 000 0 00000000 = 0x7000.
#
# 4. `rts` -- fixed encoding 0x4E75.
#
# 5. A single `nop` (0x4E71) pads the hunk out from 14 to 16 bytes (4
#    longwords) -- the hunk format requires hunk sizes to be a whole
#    number of longwords, and this keeps the entry point 4-byte aligned
#    without disturbing the real instructions before it.
CODE = b"".join(
    [
        u16(0x223C),  # move.l #<imm32>,d1  (opcode word)
        u32(0x0000_0000),  # placeholder immediate; fixed up by HUNK_RELOC32 below
        u16(0x4EAE),  # jsr <disp16>(a6)    (opcode word)
        u16(0xFC4C),  # -948 as a 16-bit displacement
        u16(0x7000),  # moveq #0,d0
        u16(0x4E75),  # rts
        u16(0x4E71),  # nop (alignment padding)
    ]
)
assert len(CODE) % 4 == 0, "code hunk must be a whole number of longwords"
CODE_HUNK_LONGWORDS = len(CODE) // 4

# Offset, within the code hunk, of the 32-bit immediate that needs hunk
# 1's (the data hunk's) load address added to it: 2 bytes into the code
# (right after the `move.l` opcode word).
RELOC_OFFSET = 2


def build() -> bytes:
    """Assembles the full two-hunk executable and returns its bytes."""
    out = bytearray()

    # --- HUNK_HEADER ---
    out += u32(HUNK_HEADER)
    out += u32(0)  # no resident library name table
    out += u32(2)  # table_size: 2 hunks total
    out += u32(0)  # first_hunk to load (no overlays: whole table)
    out += u32(1)  # last_hunk to load
    out += u32(CODE_HUNK_LONGWORDS)  # hunk 0 size, in longwords
    out += u32(DATA_HUNK_LONGWORDS)  # hunk 1 size, in longwords

    # --- Hunk 0: HUNK_CODE ---
    out += u32(HUNK_CODE)
    out += u32(CODE_HUNK_LONGWORDS)
    out += CODE
    # One HUNK_RELOC32 group: 1 offset, targeting hunk 1, at RELOC_OFFSET.
    out += u32(HUNK_RELOC32)
    out += u32(1)  # one offset in this group
    out += u32(1)  # target hunk index (the data hunk)
    out += u32(RELOC_OFFSET)
    out += u32(0)  # terminate the list of RELOC32 groups
    out += u32(HUNK_END)

    # --- Hunk 1: HUNK_DATA ---
    out += u32(HUNK_DATA)
    out += u32(DATA_HUNK_LONGWORDS)
    out += MESSAGE
    out += u32(HUNK_END)

    return bytes(out)


def main() -> None:
    data = build()
    OUT_PATH.write_bytes(data)
    print(f"wrote {OUT_PATH} ({len(data)} bytes)")


if __name__ == "__main__":
    main()
