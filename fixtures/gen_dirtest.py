#!/usr/bin/env python3
"""Generates fixtures/dirtest: T14's "Lock + Examine/ExNext a directory
and print entries" fixture. See fixtures/dirtest.s for the equivalent
assembly and fixtures/README.md for the full description; this script is
the authoritative, byte-identical, toolchain-free build (vasm isn't
available -- see fixtures/gen_filetest.py's docstring for the same
note).

Program flow (real startup -- OpenLibrary, not a pre-seeded A6):

1. A6 = *4; OpenLibrary("dos.library", 0); A6 = returned base.
2. Lock("TEST:dir", SHARED_LOCK); on failure (D0 == 0), print "ERR\\n"
   and exit 1.
3. Examine(lock, fib) to (re)initialize the ExNext iterator.
4. Loop: ExNext(lock, fib); on DOSFALSE (no more entries), fall out of
   the loop. Otherwise, copy `fib_FileName` (a BSTR: one length byte then
   that many data bytes, at fib+8) into a scratch buffer, append '\\n'
   and a NUL, and PutStr it.
5. UnLock(lock); exit 0.

Run directly to (re)write fixtures/dirtest:

    python3 fixtures/gen_dirtest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "dirtest"

FIB_SIZE = 260  # sizeof(struct FileInfoBlock)
FIB_FILENAME_OFFSET = 8
NAMEBUF_SIZE = 116  # 1 (BSTR len, unused here) + up to 107 chars + '\n' + NUL + slack

LVO_OPENLIBRARY = -552
LVO_LOCK = -84
LVO_UNLOCK = -90
LVO_EXAMINE = -102
LVO_EXNEXT = -108
LVO_PUTSTR = -948

SHARED_LOCK = -2
DOSFALSE = 0

A6, A1, A2, A3 = 6, 1, 2, 3
D0, D1, D2, D4, D6 = 0, 1, 2, 4, 6


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.cstr("dirname", "TEST:dir")
    data.cstr("errmsg", "ERR\n")
    data.zeros("fib", FIB_SIZE)
    data.zeros("namebuf", NAMEBUF_SIZE)

    code = CodeBuilder()

    code.label("start")
    code.move_l_abs4_to_a(A6)
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    code.move_l_d_to_a(A6, D0)  # A6 = dos.library base

    # Lock("TEST:dir", SHARED_LOCK)
    code.move_l_label_to_d(D1, "dirname")
    code.moveq(D2, SHARED_LOCK)
    code.jsr_disp16_a(A6, LVO_LOCK)
    code.move_l_d_to_d(D4, D0)  # D4 = lock (persists across calls)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "fail")

    # Examine(lock, fib)
    code.move_l_d_to_d(D1, D4)
    code.move_l_label_to_d(D2, "fib")
    code.jsr_disp16_a(A6, LVO_EXAMINE)

    # --- ExNext loop ---
    code.label("exloop")
    code.move_l_d_to_d(D1, D4)
    code.move_l_label_to_d(D2, "fib")
    code.jsr_disp16_a(A6, LVO_EXNEXT)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "done")  # DOSFALSE -> no more entries

    # Copy the fib_FileName BSTR (fib+8: 1 length byte, then that many
    # chars) into namebuf as a C string: "<name>\n\0". Known limitation
    # (documented in fixtures/README.md): this assumes a nonempty name,
    # which every real directory entry has -- dbra's "loop at least
    # once" shape would mis-copy a zero-length name.
    code.move_l_label_to_a(A2, "fib", addend=FIB_FILENAME_OFFSET)
    code.move_l_label_to_a(A3, "namebuf")
    code.clr_l_d(D6)
    code.move_b_postinc_to_d(D6, A2)  # D6 = BSTR length byte; A2 -> chars
    code.subq_l_imm_d(D6, 1)  # D6 = length - 1, for dbra's count
    code.label("copyloop")
    code.move_b_postinc_to_postinc(A3, A2)
    code.dbra(D6, "copyloop")
    code.move_b_imm_to_postinc(A3, 10)  # '\n'
    code.move_b_imm_to_postinc(A3, 0)  # NUL terminator

    code.move_l_label_to_d(D1, "namebuf")
    code.jsr_disp16_a(A6, LVO_PUTSTR)

    code.branch(CodeBuilder.BRA, "exloop")

    code.label("done")
    code.move_l_d_to_d(D1, D4)
    code.jsr_disp16_a(A6, LVO_UNLOCK)
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
