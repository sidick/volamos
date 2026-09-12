#!/usr/bin/env python3
"""Multi-Kickstart-version comparison harness: runs volamos's own
fixture corpus (`fixtures/*`, hand-authored, MIT/Apache-licensed --
unlike `compare_amitools_suite.py`'s GPL corpus or
`compare_three_way.py`'s proprietary AmiBake corpus) against every real
Kickstart ROM found in `assets/*.rom` via Copperline's `copperhf.device`
(`[copperhf]`), plus `volamos` itself as a baseline oracle.

**Local-only, never CI** -- like `compare_three_way.py`, this needs real
Kickstart ROM images, Hyperion-copyrighted and never committed to this
repo (see `assets/README.md`). `assets/*.rom` is gitignored; nothing
here fetches or vendors a ROM.

## Why this harness, distinct from `compare_three_way.py`

`compare_three_way.py` proves volamos against *one* real Kickstart (3.1)
using a full proprietary AmiBake Workbench corpus. This harness instead
catches real *version-skew* bugs -- a call that behaves differently
across OS versions, or a fixture that only happens to pass against the
one Kickstart version it was written/tested against -- by running the
*same* small, self-contained fixture corpus against every available
version and requiring them to agree with each other, not just with
volamos. See issue #60's discussion for the full rationale.

## Why `copperhf.device`, not `[lide]`/`[ide]`

`[ide]` needs a real Gayle/A4000 IDE port (so a fixed machine model);
`[lide]` is a real Zorro II board with real-hardware ROM-banking
quirks. `copperhf.device` is Copperline's own emulator-only virtual
hardfile controller (`docs/internals/copperhf.md` in the Copperline
repo) -- no ROM image, no real-hardware register timing, and its own
test suite explicitly covers Kickstart 1.3/3.1/3.2 RDB and bare-OFS
autoboot. A host directory mounted as `filesystem = "ofs"` needs no
guest-side setup on any Kickstart version (OFS has been ROM-resident
since 1.0, unlike FFS which needs loading from disk before 2.0) -- so
the exact same `[copperhf]` config works unchanged across every ROM,
which is the entire point of this harness. **Requires Copperline
0.20+**: `copperhf.device` was not considered stable before that.

**Important limitation, found by testing against real hardware, not
assumed**: `copperhf` (like `[lide]`/`[ide]`) directory-mounts a host
path as an **in-memory** volume -- guest writes are never synced back
to the host, confirmed directly in Copperline's own log output
(`"guest writes to it are NOT written back to the host and are lost at
exit"`). `compare_three_way.py`'s `[[filesys]]`-based approach (a real,
live, host-writable mount) doesn't have this problem, but isn't used
here for the boot volume itself since it's a different Zorro board
class than what's actually being tested. So this harness uses *both*:
`copperhf` for the boot volume (holding the fixture binaries and
`S/Startup-Sequence`, exactly what's under test), plus a second, live
`[[filesys]]` mount (`OUT:`) purely as a host-visible output channel --
the fixture's own `Startup-Sequence` redirects into `OUT:`, not `SYS:`.

## How output capture works (no C: needed at all)

Unlike `compare_three_way.py`'s AmiBake corpus, this harness's scratch
volume has no `C:` directory -- the fixtures this harness runs
(`exectest`, `echoargs`) are self-contained CLI executables invoked
directly by full path, and AmigaDOS's Startup-Sequence redirection
(`>`/`>>`) is CLI-level syntax, not a `C:` command. So there's no need
for a real `C:Echo` to mark completion either: a second, argument-less
run of `fixtures/exectest` (always prints a fixed `"exec ok\n"` with no
command-line-argument dependency -- see below for why that
matters) doubles as the completion marker, appended after the entry
under test. The full `S/Startup-Sequence`:

    SYS:<fixture> [args] >OUT:RESULT.TXT
    SYS:exectest >>OUT:RESULT.TXT

This only compares *printed text*, not the guest process's real numeric
exit code (there's no `C:Echo`/`$RC` substitution to capture it without
a real Workbench `C:` -- see `compare_three_way.py`'s `copperline_command`
for that trick). Every corpus entry below is chosen to make its pass/
fail distinction visible in its printed text (each fixture's own `ERR`-
marker convention -- see `fixtures/README.md`), so this is a real
limitation but not currently a blind spot for anything this harness
runs.

**Why the marker isn't `echoargs` itself** (found the hard way, while
wiring this up): `fixtures/echoargs` reads its command-line arguments
via a plain `PutStr(a0)`, trusting `A0`'s buffer to be NUL-terminated
rather than reading `D0`'s length -- deliberately exercising volamos's
own defensive convenience, which pads one extra `NUL` byte onto the
real convention's buffer "for anything that scans for one instead of
trusting the length" (`Runtime::new`'s own doc comment, `crate::
dispatch`). Against a real Kickstart ROM (verified directly, on every
Kickstart version this harness reaches), `echoargs` with an actual
argument prints *nothing* rather than echoing it back or garbage --
confirming real hardware's buffer isn't reliably NUL-terminated (as
expected), but the specific empty-not-garbage shape isn't explained by
that alone and hasn't been root-caused (would need a CCP breakpoint at
`echoargs`'s entry point to inspect `A0`'s real buffer directly). Filed
as issue #63. Tracked as its own `echoargs` corpus entry below
(deliberately still included, not swept under the marker mechanism)
or just this one hand-authored fixture's own assumption.

## Usage

    python3 tools/compare_kickstart_versions.py

Auto-discovers every `assets/*.rom`, labeling each by filename stem
(e.g. `assets/kickstart-34.5.rom` -> label `kickstart-34.5`). Override
or add explicit labels with `--rom LABEL=PATH` (repeatable). Exits
non-zero if any corpus entry disagrees across the available oracles.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
FIXTURES = REPO_ROOT / "fixtures"
ASSETS = REPO_ROOT / "assets"

# Machine model: an A1200 has a real Zorro II bus (no board-fitment
# ambiguity, unlike A500/A600's non-Zorro trapdoor slot), and is the
# same model `compare_three_way.py` already uses. `copperhf.device`'s
# own test suite covers Kickstart 1.3 on the default model it exercises,
# so mixing an old ROM with a later-hardware profile here is a validated
# combination, not a novel risk.
MACHINE_ARGS = ["--model", "A1200", "--fast", "8M"]

# corpus entry name -> (fixture binary, extra command-line args)
CORPUS: list[tuple[str, str, list[str]]] = [
    ("exectest", "exectest", []),
    ("echoargs", "echoargs", ["foo", "bar"]),
]

# corpus entry name -> (reason, issue URL), same convention as
# compare_vamos.py's KNOWN_DIVERGENCES. Empty until a real, understood
# version-skew divergence is found and triaged.
KNOWN_DIVERGENCES: dict[str, tuple[str, str]] = {
    "echoargs": (
        "volamos prints the real args ('foo bar'); every real Kickstart ROM "
        "this harness reaches prints nothing -- see this module's docstring "
        "('Why the marker isn't echoargs itself') for the args-ABI mismatch "
        "found while wiring this harness up. Not yet root-caused.",
        "https://github.com/sidick/volamos/issues/63",
    ),
}

# ROM label substring -> reason this oracle is excluded from the
# required-agreement set (its raw output is still shown, never silently
# dropped -- see main()). Found while wiring this harness up: booting
# Kickstart 1.3 (34.5) via `copperhf` hits a real "Software error --
# task held" system requester before Startup-Sequence ever runs.
# Copperline's own docs (docs/internals/copperhf.md) mark its 1.3
# `copperhf` autoboot integration tests `#[ignore]`d with the explicit
# caveat "a skipped test is not evidence that its configuration works"
# -- this looks like a Copperline-side gap in 1.3 support, not a
# volamos bug, so it's excluded here rather than worked around. Revisit
# once Copperline's own 1.3 copperhf coverage is confirmed working (or
# swap this one ROM to a different boot mechanism, e.g. a real floppy
# image via `[floppy.df0]`, which doesn't share this gap).
SKIP_ORACLES: dict[str, str] = {
    "34.5": "Kickstart 1.3 hits a real boot-time system requester via "
    "copperhf -- see this module's SKIP_ORACLES comment",
}


def skip_reason(label: str) -> str | None:
    for substring, reason in SKIP_ORACLES.items():
        if substring in label:
            return reason
    return None


def discover_roms(explicit: dict[str, str]) -> dict[str, Path]:
    roms = {path.stem: path for path in sorted(ASSETS.glob("*.rom"))}
    roms.update({label: Path(path).resolve() for label, path in explicit.items()})
    return roms


def run_volamos(binary: str, fixture: str, args: list[str]) -> str:
    cmd = [binary, str(FIXTURES / fixture), *args]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
    return result.stdout


DONE_MARKER_LINE = "exec ok\n"


def copperhf_startup_sequence(fixture: str, args: list[str]) -> str:
    arg_str = f" {' '.join(args)}" if args else ""
    return (
        f"SYS:{fixture}{arg_str} >OUT:RESULT.TXT\n"
        f"SYS:exectest >>OUT:RESULT.TXT\n"
    )


def run_copperline(
    copperline_bin: str,
    copperline_ctl_bin: str,
    rom: Path,
    fixture: str,
    args: list[str],
    timeout_seconds: int = 60,
) -> str:
    """Boots `rom` with a scratch `SYS:` volume (this repo's own fixture
    binaries, no `C:`/Workbench needed at all) attached via
    `copperhf.device`, runs `fixture`, and reads its captured output
    back from the host directory backing that volume -- same
    poll-the-host-filesystem approach as `compare_three_way.py`'s
    `run_copperline`, just via `[copperhf]` instead of `[[filesys]]`."""
    with tempfile.TemporaryDirectory(prefix="compare-ks-copperline-") as tmp:
        scratch = Path(tmp) / "sys"
        scratch.mkdir()
        shutil.copy(FIXTURES / fixture, scratch / fixture)
        if fixture != "exectest":
            shutil.copy(FIXTURES / "exectest", scratch / "exectest")
        (scratch / "S").mkdir()
        (scratch / "S" / "Startup-Sequence").write_text(
            copperhf_startup_sequence(fixture, args)
        )

        # Live, host-writable output channel -- see the module docstring's
        # "Important limitation" note on why RESULT.TXT can't live on the
        # copperhf-mounted SYS: volume itself.
        out_dir = Path(tmp) / "out"
        out_dir.mkdir()

        config_path = Path(tmp) / "copperline.toml"
        info_path = Path(tmp) / "control-info.json"
        config_path.write_text(
            f'rom = "{rom}"\n\n'
            f"[cpu]\n"
            f'model = "68020"\n'
            f"fpu = true\n\n"
            f"[copperhf]\n"
            f'unit0 = {{ path = "{scratch}", name = "SYS", bootpri = 6, filesystem = "ofs" }}\n\n'
            f"[[filesys]]\n"
            f'path = "{out_dir}"\n'
            f'volume = "OUT"\n'
            f"bootpri = -128\n"
        )

        proc = subprocess.Popen(
            [
                copperline_bin,
                *MACHINE_ARGS,
                "--config",
                str(config_path),
                "--control",
                ":0",
                "--control-info",
                str(info_path),
                "--noaudio",
                "--windowed",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            result_path = out_dir / "RESULT.TXT"
            deadline = time.monotonic() + timeout_seconds
            emulated_seconds = 10
            while time.monotonic() < deadline:
                if not info_path.exists():
                    time.sleep(0.2)
                    continue
                subprocess.run(
                    [
                        copperline_ctl_bin,
                        "--info",
                        str(info_path),
                        "run_until",
                        f'{{"seconds": {emulated_seconds}}}',
                    ],
                    capture_output=True,
                    timeout=30,
                )
                if result_path.exists():
                    text = result_path.read_text()
                    if text.endswith(DONE_MARKER_LINE):
                        break
                emulated_seconds += 10
            else:
                return "<timed out waiting for the completion marker>"

            return text[: -len(DONE_MARKER_LINE)]
        finally:
            proc.kill()
            proc.wait(timeout=10)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--rom",
        action="append",
        default=[],
        metavar="LABEL=PATH",
        help="add or override a Kickstart ROM under test (repeatable); "
        "otherwise every assets/*.rom is auto-discovered",
    )
    parser.add_argument(
        "--volamos",
        default=str(REPO_ROOT / "target" / "release" / "volamos"),
        help="path to the volamos binary (default: target/release/volamos)",
    )
    parser.add_argument("--copperline", default="copperline", help="path to the copperline binary")
    parser.add_argument(
        "--copperline-ctl", default="copperline-ctl", help="path to the copperline-ctl binary"
    )
    args = parser.parse_args()

    explicit = dict(item.split("=", 1) for item in args.rom)
    roms = discover_roms(explicit)
    if not roms:
        print(
            f"No ROMs found under {ASSETS} and none given via --rom; nothing to compare.",
            file=sys.stderr,
        )
        return 1

    oracle_names = ["volamos", *sorted(roms)]
    print(f"Oracles: {', '.join(oracle_names)}\n")

    results = []
    for name, fixture, fixture_args in CORPUS:
        outputs = {"volamos": run_volamos(args.volamos, fixture, fixture_args)}
        for label, rom in sorted(roms.items()):
            outputs[label] = run_copperline(
                args.copperline, args.copperline_ctl, rom, fixture, fixture_args
            )

        skipped = {label: out for label, out in outputs.items() if skip_reason(label)}
        required = {label: out for label, out in outputs.items() if label not in skipped}
        skip_detail = "; ".join(f"{label} skipped ({out!r})" for label, out in sorted(skipped.items()))

        if len(set(required.values())) == 1:
            results.append((name, "PASS", skip_detail))
        elif name in KNOWN_DIVERGENCES:
            reason, issue = KNOWN_DIVERGENCES[name]
            detail = f"{reason} ({issue})"
            if skip_detail:
                detail += f"; {skip_detail}"
            results.append((name, "KNOWN", detail))
        else:
            detail = "\n  ".join(f"{engine}: {out!r}" for engine, out in sorted(required.items()))
            if skip_detail:
                detail += f"\n  {skip_detail}"
            results.append((name, "FAIL", detail))

    width = max(len(n) for n, _, _ in results)
    for name, status, detail in results:
        line = f"{name:<{width}}  {status}"
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
