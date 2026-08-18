#!/usr/bin/env python3
"""Generates fixtures/echoargs: T14's "echo its command line" fixture.
See fixtures/echoargs.s for the equivalent assembly and
fixtures/README.md for the full description; this script is the
authoritative, byte-identical, toolchain-free build (vasm isn't
available -- see fixtures/gen_filetest.py's docstring for the same
note).

Program flow (real startup):

1. A6 = *4; OpenLibrary("dos.library", 0); A6 = returned base. (A0 is
   untouched by AbsExecBase/OpenLibrary handling, so the command-line
   pointer AmigaOS startup convention hands the program in A0 survives.)
2. PutStr(A0) directly: `crates/volamos-core/src/dispatch.rs`'s
   `Runtime::new` already leaves the guest command-line buffer '\\n'-
   terminated *and* NUL-terminated (one extra defensive NUL byte after
   the '\\n'), so it's already a valid `CString*` -- no copying needed.
3. Exit 0.

With guest args e.g. `foo bar`, this prints "foo bar\\n"; with no args,
the buffer is just "\\n" (an empty args list still gets the trailing
newline), so it prints "\\n".

Run directly to (re)write fixtures/echoargs:

    python3 fixtures/gen_echoargs.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "echoargs"

LVO_OPENLIBRARY = -552
LVO_PUTSTR = -948

A6, A1, A0 = 6, 1, 0
D0, D1 = 0, 1


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")

    code = CodeBuilder()

    code.label("start")
    code.move_l_abs4_to_a(A6)
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    code.move_l_d_to_a(A6, D0)  # A6 = dos.library base

    code.move_l_a_to_d(D1, A0)  # D1 = A0 (the command-line buffer)
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
