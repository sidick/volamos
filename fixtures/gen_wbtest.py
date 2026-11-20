#!/usr/bin/env python3
"""Generates fixtures/wbtest: issue #345's Workbench-startup fixture.
See fixtures/README.md; like its siblings this is the authoritative,
toolchain-free build (no vasm available -- see gen_echoargs.py).

It does what the C runtimes' Workbench startup code does, by hand:

1. A2 = ThisTask. If pr_CLI (offset 172) is non-zero it was started like
   a shell command: exit 99.
2. WaitPort(&pr_MsgPort) and GetMsg: the WBStartup message (A3).
3. wa_Lock of sm_ArgList[0] must be non-zero, else exit 98.
4. PutStr the tool's wa_Name, then, if sm_NumArgs is exactly 2, the
   project's wa_Name, each followed by a newline.
5. ReplyMsg the message, and exit with sm_NumArgs as the return code.

    python3 fixtures/gen_wbtest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

OUT_PATH = pathlib.Path(__file__).resolve().parent / "wbtest"

LVO_OPENLIBRARY = -552
LVO_PUTSTR = -948
LVO_GETMSG = -372
LVO_REPLYMSG = -378
LVO_WAITPORT = -384

THISTASK = 276  # ExecBase.ThisTask
PR_MSGPORT = 92  # struct Process.pr_MsgPort
PR_CLI = 172  # struct Process.pr_CLI
SM_NUMARGS = 28  # struct WBStartup.sm_NumArgs
SM_ARGLIST = 36  # struct WBStartup.sm_ArgList

A0, A1, A2, A3, A4, A5, A6 = 0, 1, 2, 3, 4, 5, 6
D0, D1, D2, D3, D4, D5, D6 = 0, 1, 2, 3, 4, 5, 6


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.cstr("nl", "\n")

    c = CodeBuilder()
    c.label("start")
    c.move_l_abs4_to_a(A6)
    c.move_l_disp_a_to_d(A6, THISTASK, D2)
    c.move_l_d_to_a(A2, D2)
    c.move_l_disp_a_to_d(A2, PR_CLI, D3)
    c.tst_l_d(D3)
    c.branch(CodeBuilder.BNE, "bad_cli")

    c.lea_disp_a_to_a(A0, PR_MSGPORT, A2)
    c.jsr_disp16_a(A6, LVO_WAITPORT)
    c.lea_disp_a_to_a(A0, PR_MSGPORT, A2)
    c.jsr_disp16_a(A6, LVO_GETMSG)
    c.move_l_d_to_a(A3, D0)  # A3 = WBStartup

    c.move_l_disp_a_to_d(A3, SM_ARGLIST, D4)
    c.move_l_d_to_a(A5, D4)  # A5 = sm_ArgList
    c.move_l_disp_a_to_d(A5, 0, D5)  # wa_Lock of entry 0
    c.tst_l_d(D5)
    c.branch(CodeBuilder.BEQ, "bad_lock")
    c.move_l_disp_a_to_d(A3, SM_NUMARGS, D6)

    c.move_l_label_to_a(A1, "dosname")
    c.moveq(D0, 0)
    c.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    c.move_l_d_to_a(A6, D0)  # A6 = dos.library

    c.move_l_disp_a_to_d(A5, 4, D1)  # entry 0 wa_Name
    c.jsr_disp16_a(A6, LVO_PUTSTR)
    c.move_l_label_to_d(D1, "nl")
    c.jsr_disp16_a(A6, LVO_PUTSTR)

    c.cmpi_l_imm_to_d(D6, 2)
    c.branch(CodeBuilder.BNE, "reply")
    c.move_l_disp_a_to_d(A5, 12, D1)  # entry 1 wa_Name
    c.jsr_disp16_a(A6, LVO_PUTSTR)
    c.move_l_label_to_d(D1, "nl")
    c.jsr_disp16_a(A6, LVO_PUTSTR)

    c.label("reply")
    c.move_l_abs4_to_a(A6)
    c.move_l_a_to_a(A1, A3)
    c.jsr_disp16_a(A6, LVO_REPLYMSG)
    c.move_l_d_to_d(D0, D6)
    c.rts()

    c.label("bad_cli")
    c.moveq(D0, 99)
    c.rts()
    c.label("bad_lock")
    c.moveq(D0, 98)
    c.rts()

    return build_hunk_executable(c, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
