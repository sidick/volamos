#!/usr/bin/env python3
"""Behavioral validation for the published container image (issue #19):
runs volamos's own fixture corpus once via the native binary and once via
`docker run <image>`, diffing stdout/exit code -- catching real container-
specific regressions (musl vs. glibc differences, VFS case-sensitivity,
nonroot filesystem permissions) rather than just checking the container
starts.

A real Amiga compiler (SAS/C, PhxAss, ...) is deliberately not part of
this check -- those toolchains are licensed and never vendored into this
repo (see fixtures/README.md) -- but the same image has been hand-verified
locally against a real SAS/C 6.58 compile, producing byte-identical
output to the native host build; see docs/plan.md's Phase 6 entry.

## Usage

    python3 tools/compare_docker.py --image ghcr.io/sidick/volamos:latest [--volamos PATH]

Exits non-zero if any fixture's output/exit code disagrees between the
native binary and the container.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from compare_vamos import CORPUS, FIXTURES, normalize  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent


def run_native(binary: str, fixture: str, args: list[str], test_dir: Path | None) -> tuple[str, int]:
    cmd = [binary]
    if test_dir is not None:
        cmd += ["-V", f"TEST:{test_dir}"]
    cmd += [str(FIXTURES / fixture), *args]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
    return result.stdout, result.returncode


def run_container(image: str, fixture: str, args: list[str], test_dir: Path | None) -> tuple[str, int]:
    cmd = ["docker", "run", "--rm"]
    docker_args = []
    if test_dir is not None:
        cmd += ["-v", f"{test_dir}:/data"]
        docker_args += ["-V", "TEST:/data"]
    cmd += [image, *docker_args, f"/fixtures/{fixture}", *args]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    return result.stdout, result.returncode


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True, help="container image to validate")
    parser.add_argument(
        "--volamos",
        default=str(REPO_ROOT / "target" / "release" / "volamos"),
        help="path to the native volamos binary (default: target/release/volamos)",
    )
    args = parser.parse_args()

    results = []  # (fixture, status, detail)
    for fixture, guest_args, setup in CORPUS:
        with tempfile.TemporaryDirectory(prefix=f"compare-docker-{fixture}-") as tmp:
            test_dir = setup(Path(tmp)) if setup else None
            native_stdout, native_code = run_native(args.volamos, fixture, guest_args, test_dir)
        with tempfile.TemporaryDirectory(prefix=f"compare-docker-{fixture}-") as tmp:
            test_dir = setup(Path(tmp)) if setup else None
            if test_dir is not None:
                # TemporaryDirectory defaults to 0700, owned by the host
                # user -- fine for the native run above, but the
                # container runs as the image's nonroot UID, which isn't
                # that owner, so it can't write into the bind mount
                # without this.
                test_dir.chmod(0o777)
            container_stdout, container_code = run_container(args.image, fixture, guest_args, test_dir)

        native_norm = normalize(fixture, native_stdout)
        container_norm = normalize(fixture, container_stdout)

        if native_norm == container_norm and native_code == container_code:
            results.append((fixture, "PASS", ""))
        else:
            detail = (
                f"native: stdout={native_norm!r} exit={native_code}\n"
                f"    container: stdout={container_norm!r} exit={container_code}"
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
        print(f"\n{len(failed)} native/container divergence(s)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
