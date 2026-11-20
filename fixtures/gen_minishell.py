#!/usr/bin/env python3
"""Generates fixtures/minishell: issue #20's stand-in for a Shell.
See fixtures/README.md; like its siblings this is the authoritative,
toolchain-free build (no vasm available -- see gen_echoargs.py).

`--shell` starts a Shell segment at its BCPL call-in, 8 bytes into the
first segment's code, with the startup `DosPacket` as a `BPTR` in `D1`.
This does what the real Shell does with it, minimally, so CI can test the
start without a Kickstart ROM:

1. checks the packet is what the AmigaDOS Manual says a regular shell
   startup is: `dp_Type` non-zero (1, `ACTION_RUN`), `dp_Res1` and
   `dp_Res2` both 0;
2. replies it with `ReplyPkt(pkt, rc, 0)`: `rc` is 7 if the packet was
   right and 99 if not, so the process's exit code (the Shell's reply
   code) says which.

    python3 fixtures/gen_minishell.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

OUT_PATH = pathlib.Path(__file__).resolve().parent / "minishell"

LVO_OPENLIBRARY = -552
LVO_REPLYPKT = -258

A0, A1, A2, A6 = 0, 1, 2, 6
D0, D1, D2, D3, D5 = 0, 1, 2, 3, 5

LSL_L_2_D1 = 0xE589  # lsl.l #2,d1


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")

    c = CodeBuilder()
    # The 8 bytes before the call-in: what a Shell-Seg has there.
    c.dc_l_imm(0)
    c.dc_l_imm(0x0A)
    c.label("start")
    c.dc_w(LSL_L_2_D1)  # D1 = BPTR of the packet -> its address
    c.move_l_d_to_a(A2, D1)  # A2 = packet

    c.move_l_disp_a_to_d(A2, 8, D5)  # dp_Type
    c.tst_l_d(D5)
    c.branch(CodeBuilder.BEQ, "bad")
    c.move_l_disp_a_to_d(A2, 12, D5)  # dp_Res1
    c.tst_l_d(D5)
    c.branch(CodeBuilder.BNE, "bad")
    c.move_l_disp_a_to_d(A2, 16, D5)  # dp_Res2
    c.tst_l_d(D5)
    c.branch(CodeBuilder.BNE, "bad")
    c.moveq(D2, 7)
    c.branch(CodeBuilder.BRA, "reply")
    c.label("bad")
    c.moveq(D2, 99)

    c.label("reply")
    c.move_l_abs4_to_a(A6)
    c.move_l_label_to_a(A1, "dosname")
    c.moveq(D0, 0)
    c.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    c.move_l_d_to_a(A6, D0)  # A6 = dos.library
    c.move_l_a_to_d(D1, A2)  # D1 = the packet
    c.moveq(D3, 0)  # dp_Res2
    c.jsr_disp16_a(A6, LVO_REPLYPKT)
    c.rts()

    return build_hunk_executable(c, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
