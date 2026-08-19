#!/usr/bin/env python3
"""Generates fixtures/runcmdtest: proves `RunCommand()` actually re-runs
an already-`LoadSeg`'d program to completion and propagates both its
stdout output and (indirectly, via a branch on it) its exit code back to
the caller -- the `LoadSeg`+`RunCommand`+`UnLoadSeg` counterpart to
fixtures/systest's `SystemTagList()` test. See fixtures/runcmdtest.s for
the equivalent assembly and fixtures/README.md for the full description;
this script (using fixtures/amiga_asm.py's tiny two-pass assembler) is
the authoritative, byte-identical, toolchain-free build since vasm isn't
available on the machine this was authored on.

Program flow (real startup, same convention as systest.s/echoargs.s):

1. A6 = *4 (AbsExecBase); OpenLibrary("dos.library", 0) via -552(a6);
   A6 = returned base.
2. LoadSeg("TEST:echoargs") via -150(a6): resolves "TEST:echoargs"
   through the same Vfs the caller CLI installed (a volume mapping the
   test sets up), reads and parses it, and builds a seglist. D0 = the
   seglist's own BPTR.
3. RunCommand(seg, stack=8192, paramptr="run cmd", paramlen=7) via
   -504(a6): re-runs the program the seglist above was loaded from as a
   *nested* guest program (crates/volamos/src/main.rs's
   `run_nested_program`, via `DosState::run_command`), with "run"/"cmd"
   as its guest command-line args -- fixtures/echoargs PutStrs its
   command line verbatim (see fixtures/README.md's `echoargs` section),
   so its nested output ("run cmd\\n") should appear on this process's
   own stdout, interleaved *before* anything runcmdtest itself prints
   next.
4. D0 now holds the nested program's own exit code (echoargs always
   exits 0). tst.l d0 / bne fail branches to a distinct failure exit code
   (99) if it's ever nonzero, same convention as systest.
5. On success: UnLoadSeg(seg) via -156(a6) (D1 still holds the seglist
   BPTR -- nothing between steps 3 and 5 touches it), PutStr("after
   runcommand\\n"), and exit 43 -- a distinctive, non-{0,1,42} sentinel
   so the end-to-end test can tell "really ran the whole flow" apart
   from any other exit path (42 is systest's own, kept distinct so the
   two fixtures' exit codes never collide in a shared test file).

Run directly to (re)write fixtures/runcmdtest:

    python3 fixtures/gen_runcmdtest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "runcmdtest"

# LVOs used (see crates/volamos-core/src/lvos/{dos,exec}.rs):
LVO_OPENLIBRARY = -552
LVO_LOADSEG = -150
LVO_RUNCOMMAND = -504
LVO_UNLOADSEG = -156
LVO_PUTSTR = -948

A6, A1 = 6, 1
D0, D1, D2, D3, D4 = 0, 1, 2, 3, 4

SUCCESS_EXIT_CODE = 43
NESTED_RUN_FAILED_EXIT_CODE = 99

PARAM_TEXT = "run cmd"


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.cstr("segname", "TEST:echoargs")
    data.cstr("param", PARAM_TEXT)
    data.cstr("okmsg", "after runcommand\n")

    code = CodeBuilder()

    code.label("start")
    code.move_l_abs4_to_a(A6)  # A6 = AbsExecBase (guest addr 4)
    code.move_l_label_to_a(A1, "dosname")  # A1 = "dos.library"
    code.moveq(D0, 0)  # D0 = version 0
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)  # OpenLibrary(a6)
    code.move_l_d_to_a(A6, D0)  # A6 = dos.library base

    # LoadSeg("TEST:echoargs")
    code.move_l_label_to_d(D1, "segname")
    code.jsr_disp16_a(A6, LVO_LOADSEG)  # D0 = seglist BPTR
    code.move_l_d_to_d(D1, D0)  # D1 = seglist BPTR (RunCommand's arg)

    # RunCommand(seg, stack, paramptr, paramlen)
    code.move_l_label_to_d(D3, "param")
    code.move_l_imm_to_d(D4, len(PARAM_TEXT))
    code.move_l_imm_to_d(D2, 8192)  # D2 = requested stack size
    code.jsr_disp16_a(A6, LVO_RUNCOMMAND)  # D0 = nested exit code, or -1

    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "ok")

    code.moveq(D0, NESTED_RUN_FAILED_EXIT_CODE)
    code.rts()

    code.label("ok")
    # D1 still holds the seglist BPTR -- no library call in between
    # touched it.
    code.jsr_disp16_a(A6, LVO_UNLOADSEG)

    code.move_l_label_to_d(D1, "okmsg")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, SUCCESS_EXIT_CODE)
    code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
