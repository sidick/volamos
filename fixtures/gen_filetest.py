#!/usr/bin/env python3
"""Generates fixtures/filetest: T14's "write a file, then read it back
and print it" fixture. See fixtures/filetest.s for the equivalent
assembly and fixtures/README.md for the full description; this script
(using fixtures/amiga_asm.py's tiny two-pass assembler) is the
authoritative, byte-identical, toolchain-free build since vasm isn't
available on the machine this was authored on.

Program flow (real startup, no pre-seeded A6 -- see the module docs on
fixtures/amiga_asm.py and fixtures/README.md's "Real startup flow"
section):

1. A6 = *4 (AbsExecBase); OpenLibrary("dos.library", 0) via -552(a6);
   A6 = returned base.
2. Open("TEST:out.txt", MODE_NEWFILE); on failure (D0 == 0), print
   "ERR\\n" via PutStr and exit 1.
3. Write the fixed message string to it, Close it.
4. Reopen the same path MODE_OLDFILE, Read the same number of bytes back
   into a zeroed scratch buffer, Close it.
5. PutStr the read-back buffer (already NUL-terminated by the
   zero-filled buffer past the bytes actually read) and exit 0.

Run directly to (re)write fixtures/filetest:

    python3 fixtures/gen_filetest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "filetest"

MESSAGE = "hello from filetest\n"
MSG_LEN = len(MESSAGE)  # bytes actually written/read; the buffer itself
# is generously oversized and zero-filled, so read-back is already
# NUL-terminated for PutStr regardless of MSG_LEN.
READBUF_SIZE = 64

# LVOs used (see crates/volamos-core/src/lvos/{dos,exec}.rs):
LVO_OPENLIBRARY = -552
LVO_OPEN = -30
LVO_CLOSE = -36
LVO_READ = -42
LVO_WRITE = -48
LVO_PUTSTR = -948

MODE_NEWFILE = 1006
MODE_OLDFILE = 1005

A6, A1, A0 = 6, 1, 0
D0, D1, D2, D3, D4 = 0, 1, 2, 3, 4


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.cstr("fname", "TEST:out.txt")
    data.cstr("msg", MESSAGE)
    data.cstr("errmsg", "ERR\n")
    data.zeros("readbuf", READBUF_SIZE)

    code = CodeBuilder()

    code.label("start")
    code.move_l_abs4_to_a(A6)  # A6 = AbsExecBase (guest addr 4)
    code.move_l_label_to_a(A1, "dosname")  # A1 = "dos.library"
    code.moveq(D0, 0)  # D0 = version 0
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)  # OpenLibrary(a6)
    code.move_l_d_to_a(A6, D0)  # A6 = dos.library base

    # --- write phase: Open(fname, MODE_NEWFILE) ---
    code.move_l_label_to_d(D1, "fname")
    code.move_l_imm_to_d(D2, MODE_NEWFILE)
    code.jsr_disp16_a(A6, LVO_OPEN)
    code.move_l_d_to_d(D4, D0)  # D4 = handle (persists across calls)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "fail")

    # Write(handle, msg, MSG_LEN)
    code.move_l_d_to_d(D1, D4)
    code.move_l_label_to_d(D2, "msg")
    code.move_l_imm_to_d(D3, MSG_LEN)
    code.jsr_disp16_a(A6, LVO_WRITE)

    # Close(handle)
    code.move_l_d_to_d(D1, D4)
    code.jsr_disp16_a(A6, LVO_CLOSE)

    # --- read phase: Open(fname, MODE_OLDFILE) ---
    code.move_l_label_to_d(D1, "fname")
    code.move_l_imm_to_d(D2, MODE_OLDFILE)
    code.jsr_disp16_a(A6, LVO_OPEN)
    code.move_l_d_to_d(D4, D0)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "fail")

    # Read(handle, readbuf, MSG_LEN)
    code.move_l_d_to_d(D1, D4)
    code.move_l_label_to_d(D2, "readbuf")
    code.move_l_imm_to_d(D3, MSG_LEN)
    code.jsr_disp16_a(A6, LVO_READ)

    # Close(handle)
    code.move_l_d_to_d(D1, D4)
    code.jsr_disp16_a(A6, LVO_CLOSE)

    # PutStr(readbuf)
    code.move_l_label_to_d(D1, "readbuf")
    code.jsr_disp16_a(A6, LVO_PUTSTR)

    code.moveq(D0, 0)
    code.rts()

    code.label("fail")
    code.move_l_label_to_d(D1, "errmsg")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, 1)
    code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
