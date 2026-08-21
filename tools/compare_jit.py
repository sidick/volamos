#!/usr/bin/env python3
"""Dual-mode harness for issue #18 (JIT enablement): runs volamos's own
fixture corpus twice -- once with the plain interpreter, once with
`--jit` -- and diffs stdout/exit code between the two runs.

Unlike `compare_vamos.py`, this needs no external oracle: `--jit` only
changes *how* volamos executes guest code (batched via the `m68k`
crate's trace JIT instead of one instruction at a time -- see
`crates/volamos-core/src/backend.rs`'s `M68kCpu::run`), never *what* it
computes, so any observable difference here is a real correctness bug
in the JIT path. The plain interpreter remains this runtime's
correctness reference (per its own doc comments); this harness's job is
to keep that claim honest as the pinned `m68k` crate version changes.

## Usage

    python3 tools/compare_jit.py [--volamos PATH]

Exits non-zero if any fixture's output/exit code disagrees between the
two modes.
"""

from __future__ import annotations

import argparse
import sys
import tempfile
from pathlib import Path

# Reuse the exact same corpus/setup helpers as the vamos harness -- same
# fixtures, same volume/assign requirements.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from compare_vamos import CORPUS, FIXTURES, normalize, run_one  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--volamos",
        default=str(Path(__file__).resolve().parent.parent / "target" / "release" / "volamos"),
        help="path to the volamos binary (default: target/release/volamos)",
    )
    args = parser.parse_args()

    results = []  # (fixture, status, detail)
    for fixture, guest_args, setup in CORPUS:
        fixture_path = FIXTURES / fixture
        with tempfile.TemporaryDirectory(prefix=f"compare-jit-{fixture}-") as tmp:
            test_dir = setup(Path(tmp)) if setup else None
            interp_stdout, interp_code = run_one([args.volamos], fixture_path, guest_args, test_dir)
        with tempfile.TemporaryDirectory(prefix=f"compare-jit-{fixture}-") as tmp:
            test_dir = setup(Path(tmp)) if setup else None
            jit_stdout, jit_code = run_one([args.volamos, "--jit"], fixture_path, guest_args, test_dir)

        interp_norm = normalize(fixture, interp_stdout)
        jit_norm = normalize(fixture, jit_stdout)

        if interp_norm == jit_norm and interp_code == jit_code:
            results.append((fixture, "PASS", ""))
        else:
            detail = (
                f"interpreter: stdout={interp_norm!r} exit={interp_code}\n"
                f"        jit: stdout={jit_norm!r} exit={jit_code}"
            )
            results.append((fixture, "FAIL", detail))

    width = max(len(f) for f, _, _ in results)
    for fixture, status, detail in results:
        line = f"{fixture:<{width}}  {status}"
        if detail:
            line += f"  -- {detail}"
        print(line)

    failed = [r for r in results if r[1] == "FAIL"]
    if failed:
        print(f"\n{len(failed)} interpreter/--jit divergence(s)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
