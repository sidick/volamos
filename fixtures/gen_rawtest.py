#!/usr/bin/env python3
"""Generates fixtures/rawtest: issue #315's `SetMode` raw-mode fixture.
See fixtures/README.md; like its siblings this is the authoritative,
toolchain-free build (no vasm available -- see gen_echoargs.py).

It is what an interactive guest (a REPL) does, by hand:

1. SetMode(Input(), 1): raw input.
2. PutStr("READY\\n"): tells the test driver the terminal is now raw.
3. Read(Input(), buf, 1): one key, with no Return needed.
4. Exits with that byte as its return code *without* putting the mode
   back: the terminal must be restored by volamos itself.

    python3 fixtures/gen_rawtest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

OUT_PATH = pathlib.Path(__file__).resolve().parent / "rawtest"

LVO_OPENLIBRARY = -552
LVO_INPUT = -54
LVO_READ = -42
LVO_SETMODE = -426
LVO_PUTSTR = -948

A0, A1, A6 = 0, 1, 6
D0, D1, D2, D3, D5 = 0, 1, 2, 3, 5


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.cstr("ready", "READY\n")
    data.zeros("buf", 4)

    c = CodeBuilder()
    c.label("start")
    c.move_l_abs4_to_a(A6)
    c.move_l_label_to_a(A1, "dosname")
    c.moveq(D0, 0)
    c.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    c.move_l_d_to_a(A6, D0)  # A6 = dos.library

    c.jsr_disp16_a(A6, LVO_INPUT)
    c.move_l_d_to_d(D5, D0)  # D5 = Input()

    c.move_l_d_to_d(D1, D5)
    c.moveq(D2, 1)  # mode 1 = RAW:
    c.jsr_disp16_a(A6, LVO_SETMODE)

    c.move_l_label_to_d(D1, "ready")
    c.jsr_disp16_a(A6, LVO_PUTSTR)

    c.move_l_d_to_d(D1, D5)
    c.move_l_label_to_d(D2, "buf")
    c.moveq(D3, 1)
    c.jsr_disp16_a(A6, LVO_READ)

    c.move_l_label_to_a(A0, "buf")
    c.clr_l_d(D0)
    c.move_b_disp_a_to_d(A0, 0, D0)  # exit code = the key
    c.rts()

    return build_hunk_executable(c, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
