#!/usr/bin/env python3
"""Generates fixtures/recurse: a Phase 3 stage 8 fixture that trips the
guest stack-overflow guard (`crates/volamos_core::exectask::
check_stack_bounds`, `docs/plan.md`'s "stack-overflow bug class vamos is
known to hit"), for an end-to-end test that a too-small `--stack` (or
runaway guest recursion generally) is caught loudly rather than silently
corrupting guest memory.

Program flow: real startup (as every other fixture), then an infinite
loop that (a) makes one cheap dos.library call (`PutStr` of a one-byte
message) -- which is what actually exercises the check, since
`check_stack_bounds` only runs once per *dispatched trap*, never on a
bare instruction -- and then (b) `bsr`s back to the top of the loop,
which is what actually grows the stack: each `bsr` pushes a 4-byte
return address that's never popped (there's no matching `rts` -- the
loop never returns). After enough iterations `A7` has been pushed below
the current task's `tc_SPLower`, and the *next* dispatched `PutStr` call
fails loudly with `DispatchError::StackOverflow` (see dispatch.rs)
instead of running off the end of guest memory. Run with a small
`--stack` (e.g. the CLI's clamped minimum, `volamos_core::MIN_STACK_SIZE`
= 4096 bytes) so this happens after only ~1000 iterations -- fast, and
few enough lines of "x\n" output to be a reasonable thing for a test to
capture.

`amiga_asm.py`'s `CodeBuilder.branch` already supports `bsr` for free:
`BSR`'s word-format (`0110 0001 dddddddd`, i.e. opcode base `0x6100`) is
identical in shape to `bra`/`beq`/`bne`'s (`0110 cccc dddddddd`) -- only
the condition nibble differs -- so `CodeBuilder.BSR` (added alongside
`BRA`/`BEQ`/`BNE` for this fixture) needs no new resolution logic at all.

Run directly to (re)write fixtures/recurse:

    python3 fixtures/gen_recurse.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "recurse"

LVO_OPENLIBRARY = -552
LVO_PUTSTR = -948

A6, A1 = 6, 1
D0, D1 = 0, 1


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.cstr("msg", "x\n")

    code = CodeBuilder()

    code.label("start")
    code.move_l_abs4_to_a(A6)  # A6 = AbsExecBase
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)  # OpenLibrary("dos.library") -- unchecked
    code.move_l_d_to_a(A6, D0)  # A6 = dos.library base

    code.label("loop")
    code.move_l_label_to_d(D1, "msg")
    code.jsr_disp16_a(A6, LVO_PUTSTR)  # PutStr("x\n") -- the dispatched trap that
    # actually re-checks the stack bounds each iteration; see module docs.
    code.branch(CodeBuilder.BSR, "loop")  # push a return address, never popped

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
