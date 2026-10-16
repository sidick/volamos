#!/usr/bin/env python3
"""Generates fixtures/looper: a deliberately uncooperative guest for
issue #151's "SIGTERM/SIGINT never stop a guest that ignores breaks"
regression test (`crates/volamos/tests/signal_e2e.rs`).

Program flow: *no* startup at all -- not even `OpenLibrary` -- just an
infinite `bra` back to itself. This is the worst case the issue
describes: a guest that never makes a single library call has no
dispatch-trap boundary at which `exectask::fold_pending_host_break`
could ever run, so it can never be asked to break cooperatively no
matter how long it runs. Forced termination (a host `SIGTERM`, or a
second `SIGINT`) is the only thing that can ever stop it -- which is
exactly what this fixture exists to prove.

Run directly to (re)write fixtures/looper:

    python3 fixtures/gen_looper.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, build_single_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "looper"


def build_program() -> bytes:
    code = CodeBuilder()
    code.label("spin")
    code.branch(CodeBuilder.BRA, "spin")
    return build_single_hunk_executable(code)


def main() -> None:
    OUT_PATH.write_bytes(build_program())
    print(f"wrote {OUT_PATH} ({OUT_PATH.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
