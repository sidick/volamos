#!/usr/bin/env python3
"""Prune stale `target/` build artifacts via `cargo-clean-all`.

Reducing debug info (`[profile.dev]`'s `debug = "line-tables-only"` in
the workspace `Cargo.toml`) shrinks each individual build's footprint,
but it doesn't stop `target/` from accumulating artifacts across
toolchain upgrades, dependency churn, or branch switches over time --
that's what actually drove this project's own `target/debug` to
~7.5 GB in one session (see `docs/plan.md`). This tool is the
complementary fix: wipe `target/` entirely once it hasn't been built
in a while, rather than a full `cargo clean` run by hand whenever
disk space happens to become a crisis.

Wraps the external `cargo-clean-all` crate
(<https://github.com/dnlmlr/cargo-clean-all>) rather than
reimplementing its project-discovery/age logic -- installed separately
since it's a dev-only convenience, not a build dependency:

    cargo install cargo-clean-all

Unlike `cargo-sweep` (which prunes individual stale *fingerprinted*
artifacts inside an otherwise-live `target/`, keeping incremental
builds working), `cargo-clean-all` removes a project's whole `target/`
outright once it's past the age threshold -- coarser, but simpler and
with no risk of a half-pruned incremental cache; the next build after
a sweep just recompiles from scratch. It also scans recursively, so
the same install is reusable across any other Rust project on the
machine, not just this one.

## Usage

    python3 tools/clean_stale_target.py             # wipe target/ if untouched for 14+ days
    python3 tools/clean_stale_target.py --days 30    # a different staleness threshold
    python3 tools/clean_stale_target.py --dry-run    # report what would be removed only
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--days",
        type=int,
        default=14,
        help="wipe target/ if untouched for this many days (default: 14)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="report what would be removed without deleting anything",
    )
    args = parser.parse_args()

    if shutil.which("cargo-clean-all") is None:
        print(
            "cargo-clean-all isn't installed. Install it with:\n"
            "    cargo install cargo-clean-all\n",
            file=sys.stderr,
        )
        return 1

    repo_root = Path(__file__).resolve().parent.parent
    cmd = ["cargo", "clean-all", "--keep-days", str(args.days)]
    if args.dry_run:
        cmd.append("--dry-run")
    cmd.append(str(repo_root))
    print(f"$ {' '.join(cmd)}")
    return subprocess.run(cmd, check=False).returncode


if __name__ == "__main__":
    sys.exit(main())
