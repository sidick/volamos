#!/usr/bin/env python3
"""Generates fixtures/termtest: writes Amiga console output a host
terminal can't show as it is -- single-byte CSI (`$9B`) control
sequences, an Amiga-only one, `SO`/`SI` shifting, and Latin-1 text --
in two `Write(Output(), ...)` calls that split one sequence between
them (issues #426/#427).

The bytes, with the split marked `|`:

    ab 9B "K" cd 9B "1;3" | "2m" " caf" E9 " " 0E "i" 0F 9B "0 p" 0A

That is: "ab", erase to end of line, "cd", bold green, " café ", a
shifted `i` (Latin-1 `$E9` again), cursor off, newline. Each `Write`
must return its own length (exit 10 otherwise); exit 0.

Run directly to (re)write fixtures/termtest:

    python3 fixtures/gen_termtest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "termtest"

# LVOs used (see crates/volamos-core/src/lvos/{dos,exec}.rs):
LVO_OPENLIBRARY = -552
LVO_OUTPUT = -60
LVO_WRITE = -48

PART1 = b"ab\x9bKcd\x9b1;3"
PART2 = b"2m caf\xe9 \x0ei\x0f\x9b0 p\n"

A6 = 6
D0, D1, D2, D3, D4 = 0, 1, 2, 3, 4


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.raw("part1", PART1)
    data.raw("part2", PART2)

    code = CodeBuilder()

    code.label("start")
    code.move_l_abs4_to_a(A6)
    code.move_l_label_to_a(1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    code.move_l_d_to_a(A6, D0)  # A6 = dos.library

    code.jsr_disp16_a(A6, LVO_OUTPUT)
    code.move_l_d_to_d(D4, D0)  # D4 = Output()

    for label, part in [("part1", PART1), ("part2", PART2)]:
        code.move_l_d_to_d(D1, D4)
        code.move_l_label_to_d(D2, label)
        code.move_l_imm_to_d(D3, len(part))
        code.jsr_disp16_a(A6, LVO_WRITE)
        code.cmpi_l_imm_to_d(D0, len(part))
        code.branch(CodeBuilder.BNE, "short")

    code.moveq(D0, 0)
    code.rts()

    code.label("short")
    code.moveq(D0, 10)
    code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
