#!/usr/bin/env python3
"""Two-oracle comparison harness: runs volamos's own fixtures under both
volamos and `vamos` (amitools) and diffs stdout/exit code, catching real
functional divergences between the two independent implementations.
See also `tools/compare_amitools_suite.py`, the same idea run against
amitools' own much larger `test/bin` corpus instead of these ~6
hand-written fixtures (local-only -- needs an external GPL checkout).

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
issue -- but only while volamos's own output still equals the pinned
expected `(stdout, exit code)` recorded with the entry: the divergence
being tolerated is vamos's, not any change in volamos. This harness's
job is to keep *finding* divergences, not to block
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


def needs_test_volume_with_dir(tmpdir: Path) -> Path:
    """`TEST:` containing a populated `dir/`, which `dirtest` locks and
    lists. Without it `Lock("TEST:dir")` failed on both sides and the
    fixture only compared two `ERR`s (issue #364)."""
    test_dir = needs_test_volume(tmpdir)
    (test_dir / "dir").mkdir()
    for name in ("alpha.txt", "Beta.txt", "gamma"):
        (test_dir / "dir" / name).write_text(name)
    (test_dir / "dir" / "sub").mkdir()
    return test_dir


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
    ("dirtest", [], needs_test_volume_with_dir),
    ("exectest", [], None),
    ("systest", [], needs_test_volume_with_echoargs),
    ("runcmdtest", [], needs_test_volume_with_echoargs),
]

# fixture name -> (reason, issue URL, expected volamos (stdout, exit code)).
# See each issue for the open root-cause question -- neither is assumed to
# be "volamos is right" just because it's this project's own
# implementation. The expected pair pins what *volamos* must print: a known
# divergence from vamos is only tolerated while volamos still gives the
# answer that was checked against real Kickstart. Without it any mismatch
# on these fixtures -- a crash, garbage output, the wrong exit code -- was
# reported as KNOWN and CI stayed green (issue #364).
KNOWN_DIVERGENCES = {
    "exectest": (
        "CheckSignal after SetSignal(1<<5,1<<5) doesn't match under vamos -- "
        "confirmed via real Kickstart (Copperline) that volamos matches real "
        "hardware here; this is vamos's own divergence, not a volamos bug",
        "https://github.com/sidick/volamos/issues/6",
        ("exec ok\n", 0),
    ),
    "echoargs": (
        "vamos doesn't add the real trailing space before the command-line "
        "buffer's final '\\n' when there's at least one argument -- confirmed "
        "against real Kickstart 2.0/3.0/3.1 hardware (issue #63); vamos's own "
        "divergence, not a volamos bug",
        "https://github.com/sidick/volamos/issues/64",
        ("foo bar \n", 0),
    ),
    "systest": (
        "same root cause as 'echoargs' above, via SystemTagList's nested "
        "process launch reusing the same command-line-buffer construction",
        "https://github.com/sidick/volamos/issues/64",
        ("sys arg \nafter system\n", 42),
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
            note = ""
            if fixture in KNOWN_DIVERGENCES:
                note = "now agrees with vamos: remove it from KNOWN_DIVERGENCES"
            results.append((fixture, "PASS", note))
        elif fixture in KNOWN_DIVERGENCES:
            reason, issue, expected = KNOWN_DIVERGENCES[fixture]
            if (vol_norm, vol_code) == expected:
                results.append((fixture, "KNOWN", f"{reason} ({issue})"))
            else:
                # The tolerated divergence is vamos's. volamos itself
                # drifting from its checked output is a real failure.
                detail = (
                    f"volamos no longer gives the checked output for this known "
                    f"divergence ({issue})\n"
                    f"  expected: stdout={expected[0]!r} exit={expected[1]}\n"
                    f"  volamos:  stdout={vol_norm!r} exit={vol_code}"
                )
                results.append((fixture, "FAIL", detail))
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
