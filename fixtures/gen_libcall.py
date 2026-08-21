#!/usr/bin/env python3
"""Generates fixtures/libcall: the Phase L3 CLI client fixture that opens
the disk-based library named on its command line and calls its real
vectors. See fixtures/libcall.s for the equivalent assembly and its own
header comment for the full program flow; this script (via
fixtures/amiga_asm.py's CodeBuilder/DataBuilder, same two-hunk CODE+DATA
shape as echoargs.s/systest.s) is the authoritative, byte-identical,
toolchain-free build since vasm isn't guaranteed to be available
everywhere this repo is built.

Run directly to (re)write fixtures/libcall:

    python3 fixtures/gen_libcall.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "libcall"

LVO_OPENLIBRARY = -552
LVO_CLOSELIBRARY = -414
LVO_PUTSTR = -948
LVO_USERFUNC = -30
LVO_ADDFUNC = -36
LIB_OPENCNT_OFFSET = 32

A5, A4, A3, A2, A1, A0, A6 = 5, 4, 3, 2, 1, 0, 6
D0, D1 = 0, 1


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.cstr("useroktxt", "user ok\n")
    data.cstr("addoktxt", "add ok\n")
    data.cstr("cntoktxt", "cnt ok\n")
    data.cstr("failtxt", "open failed\n")
    data.cstr("badtxt", "bad\n")
    data.zeros("namebuf", 64)

    code = CodeBuilder()

    code.label("start")
    code.move_l_abs4_to_a(A5)  # A5 = AbsExecBase (kept constant)

    # copy the command-line arg (A0) into namebuf up to the first '\n' --
    # must happen before any library call: A0 is a caller-clobbered
    # ("scratch") register per the RKRM calling convention (D0/D1/A0/A1),
    # not guaranteed to survive one (see libcall.s's header comment).
    code.move_l_a_to_a(A1, A0)
    code.move_l_label_to_a(A2, "namebuf")
    code.label("parseloop")
    code.move_b_postinc_to_d(D1, A1)
    code.cmpi_b_imm_to_d(D1, 10)  # '\n'
    code.branch(CodeBuilder.BEQ, "parsedone")
    code.move_b_d_to_postinc(A2, D1)
    code.branch(CodeBuilder.BRA, "parseloop")
    code.label("parsedone")
    code.clr_b_ind(A2)

    code.move_l_a_to_a(A6, A5)
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    code.move_l_d_to_a(A4, D0)  # A4 = dos.library base (kept constant)

    # first open
    code.move_l_a_to_a(A6, A5)
    code.move_l_label_to_a(A1, "namebuf")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "openfail")
    code.move_l_d_to_a(A3, D0)

    # user vector (LVO -30) -> expect 42
    code.move_l_a_to_a(A6, A3)
    code.jsr_disp16_a(A6, LVO_USERFUNC)
    code.cmpi_l_imm_to_d(D0, 42)
    code.branch(CodeBuilder.BNE, "bad")
    code.move_l_a_to_a(A6, A4)
    code.move_l_label_to_d(D1, "useroktxt")
    code.jsr_disp16_a(A6, LVO_PUTSTR)

    # add vector (LVO -36), D0=40,D1=2 -> expect 42
    code.moveq(D0, 40)
    code.moveq(D1, 2)
    code.move_l_a_to_a(A6, A3)
    code.jsr_disp16_a(A6, LVO_ADDFUNC)
    code.cmpi_l_imm_to_d(D0, 42)
    code.branch(CodeBuilder.BNE, "bad")
    code.move_l_a_to_a(A6, A4)
    code.move_l_label_to_d(D1, "addoktxt")
    code.jsr_disp16_a(A6, LVO_PUTSTR)

    # second open -> lib_OpenCnt should read 2
    code.move_l_a_to_a(A6, A5)
    code.move_l_label_to_a(A1, "namebuf")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "openfail")
    code.move_l_d_to_a(A3, D0)
    code.moveq(D0, 0)
    code.move_w_disp_a_to_d(A3, LIB_OPENCNT_OFFSET, D0)
    code.cmpi_l_imm_to_d(D0, 2)
    code.branch(CodeBuilder.BNE, "bad")
    code.move_l_a_to_a(A6, A4)
    code.move_l_label_to_d(D1, "cntoktxt")
    code.jsr_disp16_a(A6, LVO_PUTSTR)

    # close both opens -- now (phase L4) genuinely runs test.library's own
    # Close vector twice, the second one triggering a real delayed
    # expunge; see libcall.s's header comment
    code.move_l_a_to_a(A6, A5)
    code.move_l_a_to_a(A1, A3)
    code.jsr_disp16_a(A6, LVO_CLOSELIBRARY)
    code.move_l_a_to_a(A1, A3)
    code.jsr_disp16_a(A6, LVO_CLOSELIBRARY)

    code.moveq(D0, 0)
    code.rts()

    code.label("openfail")
    code.move_l_a_to_a(A6, A4)
    code.move_l_label_to_d(D1, "failtxt")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, 10)
    code.rts()

    code.label("bad")
    code.move_l_a_to_a(A6, A4)
    code.move_l_label_to_d(D1, "badtxt")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, 20)
    code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
