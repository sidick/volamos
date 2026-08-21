#!/usr/bin/env python3
"""Generates fixtures/testlib: the Phase L3 fixture proving `OpenLibrary`
really loads, `MakeLibrary`s, and inits a disk-based `RTF_AUTOINIT`
library, then calls its own real vectors natively. See fixtures/testlib.s
for the equivalent assembly (its header comment documents the exact
Resident/AUTOINIT/vector-table layout and what each vector proves) and
fixtures/README.md for the summary; this script (via fixtures/amiga_asm.py's
CodeBuilder, extended for this fixture with raw dc.w/dc.l/dc.l-selfptr/
dc.bytes data emission plus a handful of new instruction encodings --
movem, displaced moves, displaced addq, cmpi, byte clr, register add) is
the authoritative, byte-identical, toolchain-free build since vasm isn't
guaranteed to be available everywhere this repo is built.

Unlike every earlier fixture (a two-hunk CODE+DATA program), testlib is a
*single* CODE hunk: the struct Resident, AUTOINIT table, vector table, and
name strings all live in the same hunk as the code, exactly like a real
vasm-built `.library` file's `HUNK_RELOC32` group targets its own hunk
(see fixtures/testlib.s's header comment) -- so this uses
`amiga_asm.build_single_hunk_executable` instead of `build_hunk_executable`.

Run directly to (re)write fixtures/testlib:

    python3 fixtures/gen_testlib.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, build_single_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "testlib"

LIB_REVISION_OFFSET = 22
LIB_OPENCNT_OFFSET = 32
SEGLIST_MARKER_OFFSET = 36
ALLOCMEM_MARKER_OFFSET = 40
LIB_DATA_SIZE = 44  # >= 34 (struct Library) + 2 marker longwords
INIT_MARKER = 0x2A2A
LVO_ALLOCMEM = -198

D0, D1, A0, A1, A2, A6 = 0, 1, 0, 1, 2, 6


def build_program() -> bytes:
    code = CodeBuilder()

    code.label("start")
    code.moveq(D0, 0)
    code.rts()

    code.label("Resident")
    code.dc_w(0x4AFC)  # RTC_MATCHWORD
    code.dc_l_selfptr("Resident")  # RT_MATCHTAG
    code.dc_l_selfptr("EndCode")  # RT_ENDSKIP (unused by execlib.rs)
    code.dc_w((0x80 << 8) | 1)  # RT_FLAGS=RTF_AUTOINIT(0x80), RT_VERSION=1
    code.dc_w((9 << 8) | 0)  # RT_TYPE=NT_LIBRARY(9), RT_PRI=0
    code.dc_l_selfptr("LibName")  # RT_NAME
    code.dc_l_selfptr("LibIdString")  # RT_IDSTRING
    code.dc_l_selfptr("AutoInitTab")  # RT_INIT

    code.label("AutoInitTab")
    code.dc_l_imm(LIB_DATA_SIZE)  # dSize
    code.dc_l_selfptr("VecTable")  # vectors (absolute-pointer form)
    code.dc_l_imm(0)  # structure (NULL -- no InitStruct)
    code.dc_l_selfptr("InitFunc")  # initFunc

    code.label("VecTable")
    code.dc_l_selfptr("OpenFunc")
    code.dc_l_selfptr("CloseFunc")
    code.dc_l_selfptr("ExpungeFunc")
    code.dc_l_selfptr("ReservedFunc")
    code.dc_l_selfptr("UserFunc")
    code.dc_l_selfptr("AddFunc")
    code.dc_l_imm(-1)  # terminator

    code.label("InitFunc")
    # D0=libBase, A0=segList (BPTR), A6=ExecBase.
    # libBase is kept in A2 (callee-saved), not A1 -- A1 is a
    # caller-clobbered ("scratch") register per the RKRM calling
    # convention (D0/D1/A0/A1) and can't be trusted to survive the
    # AllocMem call below on real hardware; see testlib.s's comment.
    code.movem_l_to_predec(7, ["d0", "a0", "a2", "a6"])  # -(sp): d0/a0/a2/a6
    code.move_l_d_to_a(A2, D0)  # move.l d0,a2 (a2 = libBase)
    code.move_w_imm_to_disp_a(A2, LIB_REVISION_OFFSET, INIT_MARKER)
    code.move_l_a_to_disp_a(A2, SEGLIST_MARKER_OFFSET, A0)
    code.moveq(D0, 4)  # AllocMem(4, MEMF_ANY) -- proves the trampoline
    code.moveq(D1, 0)  # supports a nested library call mid-initFunc
    code.jsr_disp16_a(A6, LVO_ALLOCMEM)
    code.move_l_d_to_disp_a(A2, ALLOCMEM_MARKER_OFFSET, D0)
    code.movem_l_from_postinc(7, ["d0", "a0", "a2", "a6"])  # restore D0/A0/A2/A6
    code.rts()  # return libBase (D0) per MakeLibrary's contract

    code.label("OpenFunc")
    code.addq_w_disp_a(A6, LIB_OPENCNT_OFFSET, 1)
    code.move_l_a_to_d(D0, A6)  # move.l a6,d0
    code.rts()

    code.label("CloseFunc")
    # Real delayed-expunge idiom (RKRM ch. 18 / plan §2.4, phase L4): see
    # testlib.s's CloseFunc comment for the full rationale.
    code.subq_w_disp_a(A6, LIB_OPENCNT_OFFSET, 1)
    code.branch(code.BNE, "CloseStillOpen")
    code.move_l_disp_a_to_d(A6, SEGLIST_MARKER_OFFSET, D0)
    code.rts()
    code.label("CloseStillOpen")
    code.moveq(D0, 0)
    code.rts()

    code.label("ExpungeFunc")
    code.moveq(D0, 0)
    code.rts()

    code.label("ReservedFunc")
    code.moveq(D0, 0)
    code.rts()

    code.label("UserFunc")
    code.moveq(D0, 42)
    code.rts()

    code.label("AddFunc")
    code.add_l_d_to_d(D0, D1)  # D0 = D0 + D1
    code.rts()

    code.label("LibName")
    code.dc_bytes(b"test.library\0")
    code.label("LibIdString")
    code.dc_bytes(b"test.library 1.0\0")
    code.label("EndCode")

    return build_single_hunk_executable(code)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
