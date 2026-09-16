#!/usr/bin/env python3
"""Generates fixtures/stacktest: the issue #65 increment 2 fixture for
volamos's `--sanitize` mode's two stack detectors (below-stack-pointer
accesses, return-address corruption). See fixtures/stacktest.s for the
equivalent assembly (assembled for real with PhxAss under volamos -- see
fixtures/README.md's "stacktest" section for that exact command) and its
own header comment for the full program flow and design rationale; this
script (using fixtures/amiga_asm.py's tiny two-pass assembler) is the
authoritative, byte-identical, toolchain-free build, since PhxAss isn't
checked into this repo (Aminet freeware living outside it) and can't be
relied on in CI.

Program flow (real startup, same convention as memtest.s/every fixture
since filetest.s/dirtest.s/echoargs.s):

1. A2 = the incoming command-line pointer (A0), copied *before* any
   library call (A0 is scratch across a jsr).
2. A4 = AbsExecBase; A6 = A4; OpenLibrary("dos.library", 0) via -552(a6)
   (unchecked); A3 = the returned dos.library base, kept in A3.
3. Command-line dispatch via the same `strmatch` subroutine memtest.s
   uses, picking one of five modes -- see the module's MODES list below
   and fixtures/README.md's "stacktest" section for what each one does
   and what --sanitize is expected to report. No match (including an
   empty command line) falls through to `mode_usage`.
4. Each mode PutStr's a short self-describing line, does its
   stack-specific thing, and exits 0 via a plain `rts`.

Run directly to (re)write fixtures/stacktest:

    python3 fixtures/gen_stacktest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "stacktest"

# LVOs used (see crates/volamos-core/src/lvos/dos.rs):
LVO_OPENLIBRARY = -552
LVO_PUTSTR = -948

# Registers (see crates/volamos-core/src/cpu.rs's DataRegister/
# AddressRegister numbering, matching real 68k D0-D7/A0-A7).
D0, D1, D2, D3 = 0, 1, 2, 3
A0, A1, A2, A3, A4, A5, A6, A7 = 0, 1, 2, 3, 4, 5, 6, 7

SMASH_UNREACHABLE_EXIT_CODE = 98  # canary: only reached if the overwrite failed
DEEP_RECURSE_COUNT = 63  # -> 64 bsr's total (this value, plus the outer call)


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")

    data.cstr("kw_below", "below")
    data.cstr("kw_smash", "smash")
    data.cstr("kw_clean", "clean")
    data.cstr("kw_pushret", "pushret")
    data.cstr("kw_deep", "deep")

    data.cstr("msg_below", "below: reading 128 bytes below the current stack pointer\n")
    data.cstr("msg_smash", "smash: corrupting a return address, landing on valid code\n")
    data.cstr("msg_clean", "clean: three nested link/unlk frames with locals\n")
    data.cstr("msg_pushret", "pushret: move.l #target,-(sp) / rts, no matching bsr\n")
    data.cstr("msg_deep", "deep: 64 levels of clean bsr/rts recursion\n")
    data.cstr("msg_usage", "usage: stacktest below|smash|clean|pushret|deep\n")

    code = CodeBuilder()

    code.label("start")
    code.move_l_a_to_a(A2, A0)  # A2 = saved command-line pointer

    code.move_l_abs4_to_a(A4)  # A4 = AbsExecBase = EXEC_LIBRARY_BASE
    code.move_l_a_to_a(A6, A4)
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)  # OpenLibrary("dos.library") -- unchecked
    code.move_l_d_to_a(A3, D0)  # A3 = dos.library base (kept)

    # --- dispatch: try each keyword in turn ---
    for keyword, mode_label in (
        ("kw_below", "mode_below"),
        ("kw_smash", "mode_smash"),
        ("kw_clean", "mode_clean"),
        ("kw_pushret", "mode_pushret"),
        ("kw_deep", "mode_deep"),
    ):
        code.move_l_a_to_a(A1, A2)  # fresh cursor -- strmatch consumes it
        code.move_l_label_to_a(A0, keyword)
        code.branch(CodeBuilder.BSR, "strmatch")
        code.tst_l_d(D0)
        code.branch(CodeBuilder.BNE, mode_label)
    code.branch(CodeBuilder.BRA, "mode_usage")

    # --- strmatch(A1=cmdline cursor, A0=candidate keyword) -> D0 (1/0) ---
    # Identical to memtest.s/gen_memtest.py's own -- see gen_memtest.py's
    # comment for the full derivation, including the real bug it caught
    # (D1/D2 must be explicitly zeroed once before the comparison loop).
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

    # --- below: read a byte 128 bytes below the current SP, without
    # moving SP. --sanitize should report a below-stack-pointer read at
    # sp-128. ---
    code.label("mode_below")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_below")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.move_b_disp_a_to_d(A7, -128, D0)  # the below-SP read itself (result discarded)
    code.moveq(D0, 0)
    code.rts()

    # --- smash: bsr into a subroutine that overwrites its own just-
    # pushed return address in place, then rts's. --sanitize should
    # report a return-address-corruption violation at the matching RTS;
    # the process must still complete and exit 0 -- see stacktest.s's
    # header comment for why the overwrite targets a real label instead
    # of garbage. ---
    code.label("mode_smash")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_smash")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.branch(CodeBuilder.BSR, "smash_victim")
    # Unreachable in the intended (bug-present) behaviour -- see
    # stacktest.s's comment at the equivalent point.
    code.moveq(D0, SMASH_UNREACHABLE_EXIT_CODE)
    code.rts()

    code.label("smash_victim")
    code.move_l_codelabel_to_ind_a(A7, "smash_landing")  # overwrite our own
    # just-pushed return address in place
    code.rts()  # "returns" to smash_landing, not mode_smash's next instruction

    code.label("smash_landing")
    code.moveq(D0, 0)
    code.rts()

    # --- clean: three levels of nested bsr/rts, each a LINK/UNLK stack
    # frame with a local variable written and read at a fixed frame-
    # pointer-relative displacement. Must produce ZERO violations. ---
    code.label("mode_clean")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_clean")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.branch(CodeBuilder.BSR, "clean_level1")
    code.moveq(D0, 0)
    code.rts()

    code.label("clean_level1")
    code.link_a(A5, -8)
    code.move_l_imm_to_disp_a(A5, -8, 0x11111111)  # local write, in-frame
    code.branch(CodeBuilder.BSR, "clean_level2")
    code.move_l_disp_a_to_d(A5, -8, D0)  # local read-back (in-frame; unused)
    code.unlk_a(A5)
    code.rts()

    code.label("clean_level2")
    code.link_a(A5, -8)
    code.move_l_imm_to_disp_a(A5, -8, 0x22222222)
    code.branch(CodeBuilder.BSR, "clean_level3")
    code.move_l_disp_a_to_d(A5, -8, D0)
    code.unlk_a(A5)
    code.rts()

    code.label("clean_level3")
    code.link_a(A5, -4)
    code.move_l_imm_to_disp_a(A5, -4, 0x33333333)
    code.move_l_disp_a_to_d(A5, -4, D0)
    code.unlk_a(A5)
    code.rts()

    # --- pushret: `move.l #target,-(sp)` / `rts`, the classic m68k
    # computed-jump idiom -- no matching JSR/BSR anywhere. The single
    # most valuable false-positive guard in this fixture. ---
    code.label("mode_pushret")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_pushret")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.move_l_codelabel_to_predec_a(A7, "pushret_target")
    code.rts()  # jumps to pushret_target -- not a return from any call

    code.label("pushret_target")
    code.moveq(D0, 0)
    code.rts()

    # --- deep: bsr-recurse 64 levels deep (each level also MOVEM-pushes/
    # pops D0, so real SP movement happens at every level) and unwind
    # cleanly. Must produce ZERO violations. ---
    code.label("mode_deep")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_deep")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, DEEP_RECURSE_COUNT)
    code.branch(CodeBuilder.BSR, "deep_recurse")
    code.moveq(D0, 0)
    code.rts()

    code.label("deep_recurse")
    code.movem_l_to_predec(A7, ["d0"])
    code.subq_l_imm_d(D0, 1)
    code.branch(CodeBuilder.BEQ, "deep_base")
    code.branch(CodeBuilder.BSR, "deep_recurse")
    code.label("deep_base")
    code.movem_l_from_postinc(A7, ["d0"])
    code.rts()

    # --- usage: no argument, or an unrecognised one ---
    code.label("mode_usage")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_usage")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, 0)
    code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
