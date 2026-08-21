#!/usr/bin/env python3
"""Generates fixtures/testlib_initfail: the second Phase L3 fixture, a
tiny RTF_AUTOINIT library whose initFunc unconditionally refuses the open
(NULL). See fixtures/testlib_initfail.s for the equivalent assembly and
its own header comment for exactly what this proves (execlib.rs's
`after_init` NULL-init-result cleanup path); this script is the
authoritative, byte-identical, toolchain-free build (same rationale as
gen_testlib.py, sharing its single-CODE-hunk shape and
`amiga_asm.build_single_hunk_executable`).

Run directly to (re)write fixtures/testlib_initfail:

    python3 fixtures/gen_testlib_initfail.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, build_single_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "testlib_initfail"

LIB_DATA_SIZE = 34  # == sizeof(struct Library), no extra markers needed

D0, A6 = 0, 6


def build_program() -> bytes:
    code = CodeBuilder()

    code.label("start")
    code.moveq(D0, 0)
    code.rts()

    code.label("Resident")
    code.dc_w(0x4AFC)
    code.dc_l_selfptr("Resident")
    code.dc_l_selfptr("EndCode")
    code.dc_w((0x80 << 8) | 1)  # RTF_AUTOINIT, RT_VERSION=1
    code.dc_w((9 << 8) | 0)  # NT_LIBRARY, RT_PRI=0
    code.dc_l_selfptr("LibName")
    code.dc_l_selfptr("LibIdString")
    code.dc_l_selfptr("AutoInitTab")

    code.label("AutoInitTab")
    code.dc_l_imm(LIB_DATA_SIZE)
    code.dc_l_selfptr("VecTable")
    code.dc_l_imm(0)
    code.dc_l_selfptr("InitFunc")

    code.label("VecTable")
    code.dc_l_selfptr("OpenFunc")
    code.dc_l_selfptr("CloseFunc")
    code.dc_l_selfptr("ExpungeFunc")
    code.dc_l_selfptr("ReservedFunc")
    code.dc_l_imm(-1)

    code.label("InitFunc")
    # Unconditionally refuses the open, per real MakeLibrary's own
    # contract ("initFunction... returns NULL if it fails").
    code.moveq(D0, 0)
    code.rts()

    code.label("OpenFunc")
    code.move_l_a_to_d(D0, A6)  # move.l a6,d0
    code.rts()

    code.label("CloseFunc")
    code.moveq(D0, 0)
    code.rts()

    code.label("ExpungeFunc")
    code.moveq(D0, 0)
    code.rts()

    code.label("ReservedFunc")
    code.moveq(D0, 0)
    code.rts()

    code.label("LibName")
    code.dc_bytes(b"initfail.library\0")
    code.label("LibIdString")
    code.dc_bytes(b"initfail.library 1.0\0")
    code.label("EndCode")

    return build_single_hunk_executable(code)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
