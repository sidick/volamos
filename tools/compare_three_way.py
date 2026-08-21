#!/usr/bin/env python3
"""Three-oracle comparison harness: volamos vs. `vamos` vs. real
Kickstart (via Simon's Copperline emulator), run against a real
AmigaOS 3.1.4 disk image.

**Local-only, never CI** -- unlike `tools/compare_vamos.py` (the
two-oracle volamos-vs-`vamos` subset, which *does* run in CI), this
script needs two things that must never appear in a public CI job:
Hyperion-copyrighted AmigaOS media (via a pre-built AmiBake `os3.1.4`
corpus -- see below) and a real Kickstart ROM image, both proprietary
and neither ever committed to this repo. See `docs/plan.md`'s
2026-08-19 "the three-way Copperline comparison stays local-only"
decision.

## Prerequisites (all external to this repo, never fetched by this
## script)

1. A built AmiBake `os3.1.4` corpus directory -- e.g.
   `cd ~/src/amibake && .venv/bin/amibake build manifests/os314.toml
   --assets assets --out /tmp/os314-build` -- pass its `os314/`
   subdirectory (the `dir` output, a real bootable-shaped Workbench
   3.1.4 filesystem tree complete with `.uaem` sidecars in the same
   format `crates/volamos-core/src/dosmeta.rs` already implements) as
   `--corpus`.
2. A real Kickstart 3.1 ROM image, as `--rom`.
3. `copperline`/`copperline-ctl` on `$PATH` (Homebrew: already the
   case on Simon's machine).
4. A working `vamos` -- `pip install amitools` alone resolves the
   newest `machine68k`, which is API-incompatible; install with the
   extras `pip install 'amitools[vamos]'` instead (pins a compatible
   version), ideally into its own venv:
   `python3 -m venv .venv-vamos && .venv-vamos/bin/pip install
   'amitools[vamos]'`, then pass `--vamos .venv-vamos/bin/vamos`.

## How the Copperline column works

Copperline's `--run` warp-launch mode has no built-in way to redirect
a program's `Output()` to a host-readable file (real AmigaOS
redirection is a *Shell* feature, applied before the program even
starts -- `--run`'s generated boot script just launches the target
binary directly). Screen-scraping a screenshot or injecting individual
keystrokes via the control protocol's `input.key` (there is no
higher-level "type this string" primitive) would both work but are
needlessly fragile for plain text capture.

Simpler and robust: `[[filesys]]` host-directory mounts are a real,
writable, host-visible filesystem the *guest* can redirect into, and
we already have to boot from one anyway (the AmiBake corpus tree is
what supplies `C:`, real dos.library, etc.). So each comparison:

1. Copies the corpus into a fresh scratch directory (never mutates the
   original -- also means each comparison gets a clean, unshared
   filesystem, same principle as `compare_vamos.py`'s fresh `TEST:`
   per run).
2. Overwrites that copy's own `S/Startup-Sequence` with exactly one
   line running the target command with output redirected to a file
   on the same (host-visible) volume, plus a captured `$RC` and a
   completion marker line -- see [`copperline_command`]'s docstring
   for the exact script.
3. Boots Copperline pointed at that scratch copy as the boot volume,
   polls the *host* filesystem directly for the result file to appear
   (no CCP screen/memory reading needed at all), reads it back, and
   tears the emulator process down.

## Usage

    python3 tools/compare_three_way.py --corpus /path/to/os314 --rom /path/to/kickstart.rom

Exits non-zero if any *untracked* corpus entry's output/exit code
disagrees between the three engines.
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

# Seeds Copperline's battery clock (--rtc-time; "implies fitting one" per
# its own --help) to a fixed, deterministic date far from the AmigaOS
# epoch (1-Jan-78) -- see issue #9's writeup (closed as not-a-bug): with
# no RTC fitted, Copperline's guest clock defaults to the same 1-Jan-78
# epoch this corpus's own .uaem sidecars are dated, so real AmigaDOS
# commands that substitute "Today"/"Yesterday" for a matching date (e.g.
# List) do so correctly -- while volamos's DateStamp() always uses the
# real host clock (crates/volamos-core/src/dosdate.rs, no override
# exists), which is essentially never 1-Jan-78. Neither side had a real
# bug; they just disagreed about "now". Seeding Copperline's RTC to any
# fixed date that isn't 1-Jan-78 makes both sides' "now" differ from the
# corpus's stored date, so neither substitutes "Today" -- resolving the
# divergence at the source rather than tracking it as KNOWN forever. The
# exact seed value doesn't matter (any non-1978, non-today date works
# just as well); this one has no other significance.
RTC_SEED = "2025-01-01 00:00:00"

# fixture (corpus-entry) name -> (reason, issue URL) -- same convention
# as compare_vamos.py's KNOWN_DIVERGENCES.
KNOWN_DIVERGENCES: dict[str, tuple[str, str]] = {}


def normalize_list(stdout: str) -> str:
    """`List`'s own header line ("Directory ... on <date>") reflects
    each engine's own real-time clock (volamos/vamos: the host clock;
    Copperline: the guest's own uninitialized RTC, which defaults to
    1-Jan-78 on a cold boot with no `--rtc`/battery-backed clock state)
    -- not a real divergence, so it's stripped before comparing. Every
    per-file row is unaffected (`.uaem` sidecars carry a fixed literal
    timestamp, not the real-time clock)."""
    lines = stdout.splitlines()
    return "\n".join(line for line in lines if not line.startswith("Directory "))


ALL_ENGINES = frozenset({"volamos", "vamos", "copperline"})

# Each entry: (name, AmigaDOS command line, normalize function, engines).
# `engines` is which of ALL_ENGINES this entry is meaningful to compare
# across -- `vamos` doesn't understand `.uaem` sidecars at all (mounting
# the same corpus directly, it lists them as ordinary Amiga files and
# falls back to host mtime/permissions for dates/protection bits instead
# of the sidecar data), so any entry whose output depends on dates or
# protection bits is volamos-vs-copperline only; a `vamos` mismatch
# there would be comparing against host-filesystem noise, not real
# dos.library behavior. Deliberately small to start beyond that --
# commands like Version/Avail/Date also depend on which Kickstart/
# RAM-size/real-time-clock each engine actually has, not on dos.library/
# exec.library correctness, so a naive comparison would flag expected
# configuration differences as bugs. Extend this list once each new
# entry's own normalization (or configuration alignment) has been worked
# out and verified by hand, same discipline `compare_vamos.py`'s own
# corpus followed.
CORPUS = [
    ("list-c", "List SYS:C", normalize_list, frozenset({"volamos", "copperline"}), None),
    # Regression guard for issue #10 (MatchFirst+CurrentDir+relative-Open
    # composition): Type of a nested-path file -- a real text file
    # (S/Shell-Startup), deliberately not a binary one, since this
    # script's Copperline output capture reads the result file as text
    # (Path.read_text(), strict UTF-8) for its line-based RC/completion-
    # marker stripping, which a binary executable's raw bytes wouldn't
    # survive. Content is unaffected by the .uaem/date issues that scope
    # other entries down to volamos-vs-copperline -- all three engines
    # are meaningful here.
    ("type-s-shell-startup", "Type SYS:S/Shell-Startup", lambda s: s, ALL_ENGINES, None),
    # Deterministic path lookup, no scratch-write. vamos has its own,
    # separate divergence here (exits 5 with no output; volamos already
    # matches real Kickstart) -- not a volamos bug, so scoped down rather
    # than tracked as a permanent KNOWN entry for a bug that isn't ours.
    ("which-list", "Which SYS:C/List", lambda s: s, frozenset({"volamos", "copperline"}), None),
    # Deterministic arithmetic -- was issue #12 (VFWritef used the wrong,
    # C-printf format grammar instead of real BCPL Writef), fixed
    # 2026-08-21; all three engines now agree.
    ("eval-2plus2", "Eval 2+2", lambda s: s, ALL_ENGINES, None),
    # Deterministic file-content operation, result written to a file
    # rather than stdout. vamos truncates its output to 1 line here (its
    # own, separate divergence) -- scoped down for the same reason as
    # which-list above.
    (
        "sort-shell-startup",
        "Sort SYS:S/Shell-Startup SYS:sorted.txt",
        lambda s: s,
        frozenset({"volamos", "copperline"}),
        "sorted.txt",
    ),
    # Was issue #13 (cli_StandardInput unpopulated + Cli() not
    # BADDR-converted + dospattern using invented, not real, dosasl.h
    # token bytes), fixed 2026-08-21. vamos has its own, separate
    # failure mode here (exits 5, no output) -- scoped down for the same
    # reason as which-list/sort-shell-startup above.
    (
        "search-shell-startup-alias",
        "Search SYS:S/Shell-Startup Alias",
        lambda s: s,
        frozenset({"volamos", "copperline"}),
        None,
    ),
    # Deterministic file-content operation, result written to a file.
    # Clean 3-way pass, no known divergences.
    (
        "join-shell-startup",
        'Join SYS:S/Shell-Startup AS SYS:joined.txt',
        lambda s: s,
        ALL_ENGINES,
        "joined.txt",
    ),
    # Deterministic file copy, result written to a file. Clean 3-way
    # pass, no known divergences -- also a real-world regression guard
    # for the Copy gap chain (see docs/plan.md's Copy work).
    (
        "copy-shell-startup",
        "Copy SYS:S/Shell-Startup SYS:copied.txt",
        lambda s: s,
        ALL_ENGINES,
        "copied.txt",
    ),
    # Deterministic rename, result (the renamed file's content) written
    # to a file under its new name. Clean 3-way pass -- also a
    # real-world regression guard for the dospattern encoding rewrite
    # (see docs/plan.md's "dospattern encoding rewrite" entry), since
    # real Rename reuses ParsePattern's output buffer as a plain
    # STRPTR.
    (
        "rename-shell-startup",
        "Rename SYS:S/Shell-Startup SYS:renamed.txt",
        lambda s: s,
        ALL_ENGINES,
        "renamed.txt",
    ),
]


def read_output_file(scratch: Path, output_file: str | None, stdout: str) -> str:
    """For a corpus entry that writes its real result to a file (`Sort`,
    `Join`, ...) rather than printing it, reads that file back from the
    engine's own scratch copy instead of using its captured stdout --
    `output_file` is a path relative to the `SYS:` volume root (e.g.
    `"sorted.txt"`). `None` means "compare stdout as normal"."""
    if output_file is None:
        return stdout
    path = scratch / output_file
    if not path.exists():
        return f"<{output_file} was never written>"
    return path.read_text()


def run_volamos(
    binary: str, corpus: Path, command: str, output_file: str | None = None
) -> tuple[str, int]:
    """Runs `command` against a fresh scratch copy of `corpus` -- some
    corpus entries (`Sort`, `Join`, ...) write an output file into the
    volume they're given, and must never do that to the shared,
    original corpus tree (mutating it would corrupt later runs, and
    two engines racing to write the same real path would cross-
    contaminate their results). Every engine gets its own independent
    copy, same principle as `run_copperline`'s own scratch copy."""
    with tempfile.TemporaryDirectory(prefix="compare-three-way-volamos-") as tmp:
        scratch = Path(tmp) / "sys"
        shutil.copytree(corpus, scratch)
        prog, *args = command.split()
        cmd = [binary, "-V", f"SYS:{scratch}", str(scratch / "C" / prog), *args]
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        return read_output_file(scratch, output_file, result.stdout), result.returncode


def run_vamos(
    binary: str, corpus: Path, command: str, output_file: str | None = None
) -> tuple[str, int]:
    """As [`run_volamos`], but for `vamos -q`."""
    with tempfile.TemporaryDirectory(prefix="compare-three-way-vamos-") as tmp:
        scratch = Path(tmp) / "sys"
        shutil.copytree(corpus, scratch)
        prog, *args = command.split()
        cmd = [binary, "-q", "-V", f"SYS:{scratch}", str(scratch / "C" / prog), *args]
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        return read_output_file(scratch, output_file, result.stdout), result.returncode


def copperline_command(command: str) -> str:
    """The scratch copy's replacement `S/Startup-Sequence`: run
    `command` with output redirected to `SYS:RESULT.TXT` (the scratch
    volume itself -- host-visible, so the harness can just poll for
    the file), then append the command's own `$RC` (AmigaDOS Shell
    variable interpolation for the last command's return code) and a
    fixed completion marker line, both on the same file so a single
    read recovers everything."""
    return (
        f"C:{command} >SYS:RESULT.TXT\n"
        f'C:Echo "RC=$RC" >>SYS:RESULT.TXT\n'
        f'C:Echo "COPPERLINE_DONE" >>SYS:RESULT.TXT\n'
    )


def run_copperline(
    copperline_bin: str,
    copperline_ctl_bin: str,
    rom: Path,
    corpus: Path,
    command: str,
    output_file: str | None = None,
    timeout_seconds: int = 60,
) -> tuple[str, int]:
    with tempfile.TemporaryDirectory(prefix="compare-three-way-copperline-") as tmp:
        scratch = Path(tmp) / "sys"
        shutil.copytree(corpus, scratch)
        (scratch / "S" / "Startup-Sequence").write_text(copperline_command(command))

        config_path = Path(tmp) / "copperline.toml"
        info_path = Path(tmp) / "control-info.json"
        config_path.write_text(
            f'rom = "{rom}"\n\n'
            f"[cpu]\n"
            f'model = "68020"\n'
            f"fpu = true\n\n"
            f"[[filesys]]\n"
            f'path = "{scratch}"\n'
            f'volume = "SYS"\n'
            f"bootpri = 6\n"
        )

        proc = subprocess.Popen(
            [
                copperline_bin,
                "--model",
                "A1200",
                "--fast",
                "8M",
                "--config",
                str(config_path),
                "--control",
                ":0",
                "--control-info",
                str(info_path),
                "--rtc-time",
                RTC_SEED,
                "--noaudio",
                "--windowed",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            result_path = scratch / "RESULT.TXT"
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
                    if "COPPERLINE_DONE" in text:
                        break
                emulated_seconds += 10
            else:
                return "", -1

            lines = text.splitlines()
            # Drop the trailing "RC=<n>" / "COPPERLINE_DONE" marker
            # lines this module's own Startup-Sequence appended --
            # they're bookkeeping, not the command's real output.
            rc_line = next(line for line in lines if line.startswith("RC="))
            rc = int(rc_line[len("RC="):])
            body_lines = lines[: lines.index(rc_line)]
            stdout = "\n".join(body_lines) + ("\n" if body_lines else "")
            # The scratch volume is a real host directory (the same
            # `[[filesys]]` mount `RESULT.TXT` itself landed in), so an
            # output-file entry can just be read directly -- no need to
            # `Type` it back through the guest.
            return read_output_file(scratch, output_file, stdout), rc
        finally:
            proc.kill()
            proc.wait(timeout=10)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--corpus",
        required=True,
        help="path to a built AmiBake os3.1.4 corpus directory (the 'dir' output)",
    )
    parser.add_argument("--rom", required=True, help="path to a real Kickstart 3.1 ROM image")
    parser.add_argument(
        "--volamos",
        default=str(REPO_ROOT / "target" / "release" / "volamos"),
        help="path to the volamos binary (default: target/release/volamos)",
    )
    parser.add_argument(
        "--vamos",
        default="vamos",
        help="path to the vamos binary (needs amitools[vamos] -- plain 'pip install amitools' "
        "resolves an incompatible machine68k)",
    )
    parser.add_argument("--copperline", default="copperline", help="path to the copperline binary")
    parser.add_argument(
        "--copperline-ctl", default="copperline-ctl", help="path to the copperline-ctl binary"
    )
    args = parser.parse_args()

    corpus = Path(args.corpus).resolve()
    rom = Path(args.rom).resolve()

    runners = {
        "volamos": lambda command, output_file: run_volamos(
            args.volamos, corpus, command, output_file
        ),
        "vamos": lambda command, output_file: run_vamos(args.vamos, corpus, command, output_file),
        "copperline": lambda command, output_file: run_copperline(
            args.copperline, args.copperline_ctl, rom, corpus, command, output_file
        ),
    }

    results = []
    for name, command, normalize, engines, output_file in CORPUS:
        outputs = {engine: runners[engine](command, output_file) for engine in engines}
        normalized = {engine: (normalize(stdout), code) for engine, (stdout, code) in outputs.items()}
        skipped = ALL_ENGINES - engines

        if len(set(normalized.values())) == 1:
            detail = f"skipped {', '.join(sorted(skipped))}" if skipped else ""
            results.append((name, "PASS", detail))
        elif name in KNOWN_DIVERGENCES:
            reason, issue = KNOWN_DIVERGENCES[name]
            results.append((name, "KNOWN", f"{reason} ({issue})"))
        else:
            detail = "\n  ".join(
                f"{engine}: exit={code} stdout={stdout!r}"
                for engine, (stdout, code) in sorted(normalized.items())
            )
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
