#!/usr/bin/env python3
"""Generates fixtures/vartest: proves a `System()`ed command starts with a
copy of its caller's local shell variables (`NP_CopyVars`, default TRUE),
that `NP_CopyVars, FALSE` withholds them, that `Execute()` copies them
too, and that nothing the command does to its copy reaches the caller.

Real `SystemTagList` spawns a shell process through `CreateNewProc()`,
passing its tags on; `NP_CopyVars` is not among the tags it filters out
and defaults to TRUE ("Copy local shell variables if they exist", NDK 3.2
`dos.doc`, `SystemTagList` and `CreateNewProc`).

One program plays both parts, chosen by its command line (`A0` at entry):

- **Parent** (any command line not starting with `c`):
  1. `SetVar("VARTEST", "inherited", -1, LV_VAR)`.
  2. `SystemTagList("TEST:vartest child", NULL)`: the child should print
     `inherited`.
  3. `SystemTagList("TEST:vartest child", [NP_CopyVars, FALSE])`: the
     child should print `unset`.
  4. `Execute("TEST:vartest child", 0, Output())`: the autodoc doesn't
     say, but real Kickstart 3.1 copies the variables here too, so the
     child prints `inherited`.
  5. Prints its own `VARTEST`, which must still be `inherited`: the first
     child's `SetVar` went to its own copy.
- **Child** (command line `child`): prints `VARTEST` (or `unset`), then
  `SetVar("VARTEST", "changed", -1, LV_VAR)` on its copy.

So the whole run's stdout is `inherited\\nunset\\ninherited\\ninherited\\n`, exit code
0 -- byte-identical on real Kickstart 3.1 (40.63, A600, 68000, via
Copperline: the volume mounted as `TEST:`, an empty `ENV:` mounted for
the second child's fall-through to the global store, and
`TEST:vartest >TEST:RESULT.TXT` as the Startup-Sequence). Run directly
to (re)write fixtures/vartest:

    python3 fixtures/gen_vartest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "vartest"

# LVOs used (see crates/volamos-core/src/lvos/{dos,exec}.rs):
LVO_OUTPUT = -60
LVO_EXECUTE = -222
LVO_OPENLIBRARY = -552
LVO_SYSTEMTAGLIST = -606
LVO_SETVAR = -900
LVO_GETVAR = -906
LVO_PUTSTR = -948

# <dos/dostags.h>: NP_Dummy = TAG_USER + 1000, NP_CopyVars = NP_Dummy + 17.
NP_COPYVARS = 0x8000_0000 + 1000 + 17
TAG_END = 0
LV_VAR = 0
BUF_SIZE = 64

A0, A1, A2, A6 = 0, 1, 2, 6
D0, D1, D2, D3, D4, D7 = 0, 1, 2, 3, 4, 7


def build_program() -> bytes:
    data = DataBuilder()
    # The tag list goes first: the hunk is longword-aligned and u32s()
    # doesn't align, and a misaligned TagItem read is an address error on
    # a real 68000 (volamos doesn't model that, real Kickstart does).
    data.u32s("nocopytags", [NP_COPYVARS, 0, TAG_END])
    data.cstr("dosname", "dos.library")
    data.cstr("varname", "VARTEST")
    data.cstr("inherited", "inherited")
    data.cstr("changed", "changed")
    data.cstr("cmd", "TEST:vartest child")
    data.cstr("unsetmsg", "unset")
    data.cstr("nl", "\n")
    data.zeros("buf", BUF_SIZE)

    code = CodeBuilder()

    code.label("start")
    code.move_l_a_to_d(D7, A0)  # D7 = command line, before any call
    code.move_l_abs4_to_a(A6)  # A6 = AbsExecBase (guest addr 4)
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    code.move_l_d_to_a(A6, D0)  # A6 = dos.library base

    code.move_l_d_to_a(A2, D7)
    code.move_b_disp_a_to_d(A2, 0, D0)
    code.cmpi_b_imm_to_d(D0, ord("c"))
    code.branch(CodeBuilder.BEQ, "child")

    # --- parent ---
    code.move_l_label_to_d(D2, "inherited")
    code.branch(CodeBuilder.BSR, "setvar")

    code.move_l_label_to_d(D1, "cmd")
    code.moveq(D2, 0)  # NULL tags: NP_CopyVars defaults TRUE
    code.jsr_disp16_a(A6, LVO_SYSTEMTAGLIST)

    code.move_l_label_to_d(D1, "cmd")
    code.move_l_label_to_d(D2, "nocopytags")
    code.jsr_disp16_a(A6, LVO_SYSTEMTAGLIST)

    # Execute("TEST:vartest child", 0, Output())
    code.jsr_disp16_a(A6, LVO_OUTPUT)
    code.move_l_d_to_d(D3, D0)
    code.move_l_label_to_d(D1, "cmd")
    code.moveq(D2, 0)
    code.jsr_disp16_a(A6, LVO_EXECUTE)

    code.branch(CodeBuilder.BSR, "show")
    code.moveq(D0, 0)
    code.rts()

    # --- child ---
    code.label("child")
    code.branch(CodeBuilder.BSR, "show")
    code.move_l_label_to_d(D2, "changed")
    code.branch(CodeBuilder.BSR, "setvar")
    code.moveq(D0, 0)
    code.rts()

    # setvar: SetVar("VARTEST", D2, -1, LV_VAR)
    code.label("setvar")
    code.move_l_label_to_d(D1, "varname")
    code.moveq(D3, -1)  # NUL-terminated value
    code.moveq(D4, LV_VAR)
    code.jsr_disp16_a(A6, LVO_SETVAR)
    code.rts()

    # show: PutStr(VARTEST's value, or "unset"), then PutStr("\n")
    code.label("show")
    code.move_l_label_to_d(D1, "varname")
    code.move_l_label_to_d(D2, "buf")
    code.move_l_imm_to_d(D3, BUF_SIZE)
    code.moveq(D4, 0)  # local first, then global
    code.jsr_disp16_a(A6, LVO_GETVAR)
    code.move_l_label_to_d(D1, "buf")
    code.cmpi_l_imm_to_d(D0, -1)
    code.branch(CodeBuilder.BNE, "print")
    code.move_l_label_to_d(D1, "unsetmsg")
    code.label("print")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.move_l_label_to_d(D1, "nl")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
