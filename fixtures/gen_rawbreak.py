#!/usr/bin/env python3
"""Generates fixtures/rawbreak and fixtures/rawwait: issue #315's
console-break fixtures.
See fixtures/README.md; like its siblings this is the authoritative,
toolchain-free build (no vasm available -- see gen_echoargs.py).

In raw mode the console turns Ctrl+D/E/F into break signals instead of
input bytes. This guest:

1. SetMode(Input(), 1) and prints READY;
2. Read(Input(), buf, 1): one *key* -- any Ctrl+D/E/F typed first are not
   keys, so the read goes on waiting for the next byte;
3. SetSignal(0, 0): the signals now pending;
4. exits with (signals >> 13) & 7: bit 0 = SIGBREAKF_CTRL_D, bit 1 =
   SIGBREAKF_CTRL_E, bit 2 = SIGBREAKF_CTRL_F.

`rawwait` is the same without the Read: step 2 is Wait(SIGBREAKF_CTRL_D)
and step 3 uses the signals Wait returns. The console raises the break
whatever the guest is doing, so it must arrive without any Read.

    python3 fixtures/gen_rawbreak.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

OUT_DIR = pathlib.Path(__file__).resolve().parent

LVO_OPENLIBRARY = -552
LVO_SETSIGNAL = -306
LVO_WAIT = -318
LVO_INPUT = -54
LVO_READ = -42
LVO_SETMODE = -426
LVO_PUTSTR = -948

A0, A1, A6 = 0, 1, 6
D0, D1, D2, D3, D5 = 0, 1, 2, 3, 5

LSR_L_8_D0 = 0xE088  # lsr.l #8,d0
LSR_L_5_D0 = 0xEA88  # lsr.l #5,d0
ANDI_L_D0 = 0x0280  # andi.l #imm32,d0


def build_program(wait: bool) -> bytes:
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

    if wait:
        c.move_l_abs4_to_a(A6)  # A6 = exec.library
        c.move_l_imm_to_d(D0, 1 << 13)  # SIGBREAKF_CTRL_D
        c.jsr_disp16_a(A6, LVO_WAIT)  # D0 = the signals received
    else:
        c.move_l_d_to_d(D1, D5)
        c.move_l_label_to_d(D2, "buf")
        c.moveq(D3, 1)
        c.jsr_disp16_a(A6, LVO_READ)

        c.move_l_abs4_to_a(A6)  # A6 = exec.library
        c.moveq(D0, 0)
        c.moveq(D1, 0)
        c.jsr_disp16_a(A6, LVO_SETSIGNAL)  # D0 = the signals pending
    c.dc_w(LSR_L_8_D0)
    c.dc_w(LSR_L_5_D0)  # D0 >>= 13
    c.dc_w(ANDI_L_D0)
    c.dc_w(0)
    c.dc_w(7)  # D0 &= 7
    c.rts()

    return build_hunk_executable(c, data)


def main() -> None:
    for name, wait in (("rawbreak", False), ("rawwait", True)):
        program = build_program(wait)
        path = OUT_DIR / name
        path.write_bytes(program)
        print(f"wrote {path} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
