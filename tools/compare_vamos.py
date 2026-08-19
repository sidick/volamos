#!/usr/bin/env python3
"""Two-oracle comparison harness: runs volamos's own fixtures under both
volamos and `vamos` (amitools) and diffs stdout/exit code, catching real
functional divergences between the two independent implementations.

This is the two-oracle *subset* of `docs/plan.md`'s Phase 4 three-oracle
harness (volamos vs. `vamos` vs. real Kickstart) -- the third column
(real hardware via Copperline) stays blocked on Simon's own AmiBake tool
reaching its OS 3.x-base milestone, but this half needs nothing beyond
`vamos` itself (pip-installable, ROM-free by design -- same
high-level-emulation approach as volamos, not a full-system emulator).
Run it directly, or inside `ghcr.io/sidick/amiga-dev:1` (which has
`vamos` preinstalled) via `.github/workflows/ci.yml`'s `compare` job.

## Corpus

Only fixtures using the real `AbsExecBase`/`OpenLibrary` AmigaOS startup
convention are meaningful to compare -- see CORPUS below for the exact
list and why `hello`/`recurse` are deliberately excluded (a Phase-1-only
fake-A6 convention, and a deliberate stack-overflow trip respectively --
neither has a shared expected output to diff against).

## Known divergences

A mismatch against an entry in KNOWN_DIVERGENCES prints as tracked
(`KNOWN`) rather than failing the run -- see each entry's linked GitHub
issue. This harness's job is to keep *finding* divergences, not to block
on root-causing every one before it can run in CI; removing an entry
here (because the underlying bug got fixed and the two sides now agree)
is the "verified fixed" signal.

## Usage

    python3 tools/compare_vamos.py [--volamos PATH] [--vamos PATH]

Exits non-zero if any *untracked* fixture's output/exit code disagrees
between volamos and `vamos`.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
FIXTURES = REPO_ROOT / "fixtures"


def needs_test_volume(tmpdir: Path) -> Path:
    """Sets up a fresh `TEST:` host directory."""
    (tmpdir / "test").mkdir()
    return tmpdir / "test"


def needs_test_volume_with_echoargs(tmpdir: Path) -> Path:
    """Sets up a fresh `TEST:` host directory containing a copy of
    `fixtures/echoargs`, for fixtures that `System()`/`RunCommand()`
    `TEST:echoargs`."""
    test_dir = needs_test_volume(tmpdir)
    shutil.copy(FIXTURES / "echoargs", test_dir / "echoargs")
    (test_dir / "echoargs").chmod(0o755)
    return test_dir


# Each entry: (fixture name, guest args, setup function or None).
# setup(tmpdir) -> host directory to map as TEST:, or None if the
# fixture needs no volume at all.
CORPUS = [
    ("echoargs", ["foo", "bar"], None),
    ("filetest", [], needs_test_volume),
    ("dirtest", [], needs_test_volume),
    ("exectest", [], None),
    ("systest", [], needs_test_volume_with_echoargs),
    ("runcmdtest", [], needs_test_volume_with_echoargs),
]

# fixture name -> (reason, issue URL). See each issue for the open
# root-cause question -- neither is assumed to be "volamos is right"
# just because it's this project's own implementation.
KNOWN_DIVERGENCES = {
    "exectest": (
        "CheckSignal after SetSignal(1<<5,1<<5) doesn't match under vamos",
        "https://github.com/sidick/volamos/issues/6",
    ),
    "runcmdtest": (
        "missing newline between nested-run output and parent output under vamos",
        "https://github.com/sidick/volamos/issues/7",
    ),
}


def normalize(fixture: str, stdout: str) -> str:
    """Fixture-specific output normalization applied identically to both
    sides before comparing -- currently just `dirtest`'s directory
    listing, whose order isn't guaranteed by real AmigaOS and differs
    between host filesystems (macOS vs. Linux) here, not a real
    divergence."""
    if fixture == "dirtest":
        return "\n".join(sorted(stdout.splitlines()))
    return stdout


def run_one(binary: list[str], fixture_path: Path, args: list[str], test_dir: Path | None) -> tuple[str, int]:
    cmd = list(binary)
    if test_dir is not None:
        cmd += ["-V", f"TEST:{test_dir}"]
    cmd += [str(fixture_path), *args]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
    return result.stdout, result.returncode


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--volamos",
        default=str(REPO_ROOT / "target" / "release" / "volamos"),
        help="path to the volamos binary (default: target/release/volamos)",
    )
    parser.add_argument(
        "--vamos",
        default="vamos",
        help="path to the vamos binary (default: 'vamos' on $PATH)",
    )
    args = parser.parse_args()

    results = []  # (fixture, status, detail)
    for fixture, guest_args, setup in CORPUS:
        fixture_path = FIXTURES / fixture
        with tempfile.TemporaryDirectory(prefix=f"compare-vamos-{fixture}-") as tmp:
            test_dir = setup(Path(tmp)) if setup else None
            vol_stdout, vol_code = run_one([args.volamos], fixture_path, guest_args, test_dir)
        with tempfile.TemporaryDirectory(prefix=f"compare-vamos-{fixture}-") as tmp:
            test_dir = setup(Path(tmp)) if setup else None
            vam_stdout, vam_code = run_one([args.vamos, "-q"], fixture_path, guest_args, test_dir)

        vol_norm = normalize(fixture, vol_stdout)
        vam_norm = normalize(fixture, vam_stdout)

        if vol_norm == vam_norm and vol_code == vam_code:
            results.append((fixture, "PASS", ""))
        elif fixture in KNOWN_DIVERGENCES:
            reason, issue = KNOWN_DIVERGENCES[fixture]
            results.append((fixture, "KNOWN", f"{reason} ({issue})"))
        else:
            detail = (
                f"volamos: stdout={vol_norm!r} exit={vol_code}\n"
                f"  vamos: stdout={vam_norm!r} exit={vam_code}"
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
        print(f"\n{len(failed)} untracked divergence(s)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
