#!/usr/bin/env python3
"""Generates fixtures/systest: the Phase 3 stage 7 fixture proving
`SystemTagList()` (`System()`'s underlying LVO) actually runs a *nested*
guest program to completion and propagates both its stdout output and
(indirectly, via a branch on it) its exit code back to the caller. See
fixtures/systest.s for the equivalent assembly and fixtures/README.md for
the full description; this script (using fixtures/amiga_asm.py's tiny
two-pass assembler) is the authoritative, byte-identical, toolchain-free
build since vasm isn't available on the machine this was authored on.

Program flow (real startup, same convention as filetest.s/dirtest.s/
echoargs.s):

1. A6 = *4 (AbsExecBase); OpenLibrary("dos.library", 0) via -552(a6);
   A6 = returned base.
2. SystemTagList("TEST:echoargs sys arg", NULL) via -606(a6). This
   resolves "TEST:echoargs" through the same Vfs the caller CLI installed
   (a volume mapping the test sets up), loads it as a *nested* guest
   program (crates/volamos/src/main.rs's `run_nested_program`), and runs
   it to completion with "sys"/"arg" as its guest command-line args --
   `fixtures/echoargs` PutStrs its command line verbatim (see
   fixtures/README.md's `echoargs` section), so its nested output
   ("sys arg\\n") should appear on this process's own stdout, interleaved
   *before* anything systest itself prints next.
3. D0 now holds the nested program's own exit code (echoargs always
   exits 0). tst.l d0 / bne fail branches to a distinct failure exit code
   (99) if it's ever nonzero, so a broken SystemTagList (couldn't resolve/
   run the nested program, wrong exit code propagated) fails the test
   distinctly from "SystemTagList wasn't even reached".
4. On success: PutStr("after system\\n") (proving control returned to the
   *parent* guest program after the nested run completed, with its own
   dos.library calls still working afterward) and exit 42 -- a
   distinctive, non-{0,1} sentinel so the end-to-end test can tell "really
   ran the whole flow" apart from any other exit path.

Run directly to (re)write fixtures/systest:

    python3 fixtures/gen_systest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "systest"

# LVOs used (see crates/volamos-core/src/lvos/{dos,exec}.rs):
LVO_OPENLIBRARY = -552
LVO_SYSTEMTAGLIST = -606
LVO_PUTSTR = -948

A6, A1 = 6, 1
D0, D1, D2 = 0, 1, 2

SUCCESS_EXIT_CODE = 42
NESTED_RUN_FAILED_EXIT_CODE = 99


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.cstr("cmd", "TEST:echoargs sys arg")
    data.cstr("okmsg", "after system\n")

    code = CodeBuilder()

    code.label("start")
    code.move_l_abs4_to_a(A6)  # A6 = AbsExecBase (guest addr 4)
    code.move_l_label_to_a(A1, "dosname")  # A1 = "dos.library"
    code.moveq(D0, 0)  # D0 = version 0
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)  # OpenLibrary(a6)
    code.move_l_d_to_a(A6, D0)  # A6 = dos.library base

    # SystemTagList("TEST:echoargs sys arg", NULL)
    code.move_l_label_to_d(D1, "cmd")
    code.moveq(D2, 0)  # D2 = NULL tag list (no SYS_Input/SYS_Output etc.)
    code.jsr_disp16_a(A6, LVO_SYSTEMTAGLIST)  # D0 = nested exit code, or -1

    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "ok")

    code.moveq(D0, NESTED_RUN_FAILED_EXIT_CODE)
    code.rts()

    code.label("ok")
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
