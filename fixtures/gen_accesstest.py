#!/usr/bin/env python3
"""Generates fixtures/accesstest: the issue #71/#72 fixture for volamos's
`--sanitize-lowmem` (NULL / low-memory access) and `--sanitize-wild`
(wild-pointer access) detectors. See fixtures/accesstest.s for the
equivalent PhxAss assembly; this script (using fixtures/amiga_asm.py's
tiny two-pass assembler) is the authoritative, toolchain-free build.

Command-line modes (each prints a line, does exactly one thing, exits 0):

- clean:    the things every real program does, which must produce ZERO
            violations with both detectors on: read the AbsExecBase
            longword at address 4, then read lib_Version out of ExecBase
            and dos.library.
- null:     write a longword through a NULL pointer (address 0), then
            read a field at offset 8 of it -- the classic
            struct-pointer-is-NULL bug.
- vector:   write a byte at $0101, inside the exception vector table.
- abs4word: read a WORD at address 4. Only the longword read there is
            allowed (AbsExecBase); this is the guard that the exemption
            is not "anything at address 4".
- wild:     read a longword through the garbage pointer $AAAA4444.
- hardware: read VHPOSR ($DFF006), a custom-chip register volamos does
            not model.

Run directly to (re)write fixtures/accesstest:

    python3 fixtures/gen_accesstest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "accesstest"

# LVOs used (see crates/volamos-core/src/lvos/dos.rs):
LVO_OPENLIBRARY = -552
LVO_PUTSTR = -948

# Registers (see crates/volamos-core/src/cpu.rs's DataRegister/
# AddressRegister numbering, matching real 68k D0-D7/A0-A7).
D0, D1, D2, D3 = 0, 1, 2, 3
A0, A1, A2, A3, A4, A5, A6, A7 = 0, 1, 2, 3, 4, 5, 6, 7


LVO_LIBVERSION_OFFSET = 20  # struct Library.lib_Version (UWORD)


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")

    modes = ("clean", "null", "vector", "abs4word", "wild", "hardware")
    for m in modes:
        data.cstr(f"kw_{m}", m)
    data.cstr("msg_clean", "clean: AbsExecBase read + ExecBase/dos.library field reads\n")
    data.cstr("msg_null", "null: longword write through a NULL pointer, then read at +8\n")
    data.cstr("msg_vector", "vector: byte write at $0101\n")
    data.cstr("msg_abs4word", "abs4word: WORD read at address 4\n")
    data.cstr("msg_wild", "wild: longword read through $AAAA4444\n")
    data.cstr("msg_hardware", "hardware: word read of VHPOSR ($DFF006)\n")
    data.cstr("msg_usage", "usage: accesstest clean|null|vector|abs4word|wild|hardware\n")

    code = CodeBuilder()

    code.label("start")
    code.move_l_a_to_a(A2, A0)  # A2 = saved command-line pointer

    code.move_l_abs4_to_a(A4)  # A4 = AbsExecBase = EXEC_LIBRARY_BASE
    code.move_l_a_to_a(A6, A4)
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)  # OpenLibrary("dos.library") -- unchecked
    code.move_l_d_to_a(A3, D0)  # A3 = dos.library base (kept)

    for m in modes:
        code.move_l_a_to_a(A1, A2)  # fresh cursor -- strmatch consumes it
        code.move_l_label_to_a(A0, f"kw_{m}")
        code.branch(CodeBuilder.BSR, "strmatch")
        code.tst_l_d(D0)
        code.branch(CodeBuilder.BNE, f"mode_{m}")
    code.branch(CodeBuilder.BRA, "mode_usage")

    # --- strmatch(A1=cmdline cursor, A0=candidate keyword) -> D0 (1/0) ---
    # Identical to stacktest.s/memtest.s's own.
    code.label("strmatch")
    code.moveq(D1, 0)
    code.moveq(D2, 0)
    code.label("strmatch_loop")
    code.move_b_postinc_to_d(D2, A0)
    code.tst_l_d(D2)
    code.branch(CodeBuilder.BEQ, "strmatch_end")
    code.move_b_postinc_to_d(D1, A1)
    code.move_l_d_to_d(D3, D1)
    code.sub_l_d_from_d(D3, D2)
    code.tst_l_d(D3)
    code.branch(CodeBuilder.BNE, "strmatch_fail")
    code.branch(CodeBuilder.BRA, "strmatch_loop")
    code.label("strmatch_end")
    code.move_b_postinc_to_d(D1, A1)
    code.cmpi_b_imm_to_d(D1, 32)  # ' '
    code.branch(CodeBuilder.BEQ, "strmatch_ok")
    code.cmpi_b_imm_to_d(D1, 10)  # '\n'
    code.branch(CodeBuilder.BEQ, "strmatch_ok")
    code.label("strmatch_fail")
    code.moveq(D0, 0)
    code.rts()
    code.label("strmatch_ok")
    code.moveq(D0, 1)
    code.rts()

    def announce(mode: str) -> None:
        code.move_l_a_to_a(A6, A3)
        code.move_l_label_to_d(D1, f"msg_{mode}")
        code.jsr_disp16_a(A6, LVO_PUTSTR)

    # --- clean ---
    code.label("mode_clean")
    announce("clean")
    code.move_l_imm_to_a(A0, 4)
    code.move_l_disp_a_to_d(A0, 0, D0)  # the AbsExecBase idiom: move.l 4,d0
    code.move_w_disp_a_to_d(A4, LVO_LIBVERSION_OFFSET, D0)  # SysBase->LibNode.lib_Version
    code.move_w_disp_a_to_d(A3, LVO_LIBVERSION_OFFSET, D0)  # DOSBase->lib_Version
    code.moveq(D0, 0)
    code.rts()

    # --- null ---
    code.label("mode_null")
    announce("null")
    code.moveq(D0, 0)
    code.move_l_imm_to_a(A0, 0)  # the NULL pointer
    code.move_l_d_to_disp_a(A0, 0, D0)  # *p = ... (write at 0)
    code.move_l_disp_a_to_d(A0, 8, D1)  # p->field at offset 8 (read at 8)
    code.moveq(D0, 0)
    code.rts()

    # --- vector ---
    code.label("mode_vector")
    announce("vector")
    code.move_l_imm_to_a(A0, 0x100)
    code.move_b_imm_to_disp_a(A0, 1, 0x55)  # write at $0101
    code.moveq(D0, 0)
    code.rts()

    # --- abs4word ---
    code.label("mode_abs4word")
    announce("abs4word")
    code.move_l_imm_to_a(A0, 4)
    code.move_w_disp_a_to_d(A0, 0, D0)  # word read at 4: not the AbsExecBase idiom
    code.moveq(D0, 0)
    code.rts()

    # --- wild ---
    code.label("mode_wild")
    announce("wild")
    code.move_l_imm_to_a(A0, 0xAAAA4444)
    code.move_l_disp_a_to_d(A0, 0, D0)
    code.moveq(D0, 0)
    code.rts()

    # --- hardware ---
    code.label("mode_hardware")
    announce("hardware")
    code.move_l_imm_to_a(A0, 0xDFF000)
    code.move_w_disp_a_to_d(A0, 6, D0)  # VHPOSR
    code.moveq(D0, 0)
    code.rts()

    code.label("mode_usage")
    announce("usage")
    code.moveq(D0, 0)
    code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    OUT_PATH.write_bytes(build_program())
    print(f"wrote {OUT_PATH} ({OUT_PATH.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
