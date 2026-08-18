#!/usr/bin/env python3
"""Cross-check a generated `crates/volamos-core/src/lvos/*.rs` LVO table
against the *official* Amiga NDK's own `.fd` files.

## Why this tool exists, and why it is NOT a codegen pipeline

`tools/gen_lvos.py` generates volamos's committed LVO tables from AROS's
`.conf` interface descriptions (see that script's module doc and
`docs/plan.md`'s "fd/SFD metadata decision" section for the full
reasoning). That source was chosen deliberately over the official
Commodore/Hyperion NDK `.fd`/`.sfd` files: the NDK archives carry no
stated redistribution license at all (verified directly against the
NDK 3.2 R4 `ReadMe-NDK.txt` and the `.fd`/`.sfd` files themselves -- no
copyright header, no license, no grant of any kind), whereas AROS's
`.conf` files are covered by the AROS Public License, an open,
non-litigious counterparty. Both sources describe the *same facts*
(function name, LVO offset, argument registers) -- by construction,
since binary compatibility with real Amiga software requires it -- so
using AROS as the primary, generated, committed source carries the
lower practical risk without sacrificing accuracy... *except* where
AROS's own API has drifted past genuine Kickstart/Workbench 3.1 (the
project's first-stage compatibility target, see docs/plan.md), which is
exactly the gap this tool exists to catch.

This script therefore does the opposite of `gen_lvos.py`: it does not
generate or overwrite any committed file. It reads a **local copy of the
official NDK** that you must supply yourself (you need to have legally
obtained it -- Hyperion distributes NDK 3.2 gratis from amigaos.net /
Aminet; NDK 3.1 exists in various community archives) and are legally
comfortable using, parses its `.fd` files for the same bare ABI facts
`gen_lvos.py` extracts from AROS, and prints a diff against the
already-committed Rust table. Nothing from the NDK is ever written into
this repository by this tool: no generated file, no cache, no vendored
copy. Point `--ndk-dir` at a directory *outside* this repo; the tool
will refuse to run (see `_check_ndk_dir_outside_repo`) if it detects the
path is inside the repo working tree, as a defense against accidentally
committing NDK content.

## `.fd` file format (as actually observed in NDK 3.1/3.2 archives)

    ##base _DOSBase
    ##bias 30
    ##public
    Open(name,accessMode)(d1/d2)
    Close(file)(d1)
    ...
    ##private
    dosPrivate1()()
    ##public
    ...
    *--- (1 function slot reserved here) ---
    ##bias 492
    ...
    ##end

- `##bias N` sets the LVO (as a positive bias; the real offset is `-N`)
  of the *next* function line. It both establishes the initial bias and
  resynchronizes after a reserved-slot gap (marked only by a `*`-comment,
  which carries no machine-readable slot count -- the following explicit
  `##bias` restates the correct value, so gaps never need to be counted).
- Each function line consumes one slot: `bias -= 0` is wrong -- rather,
  the *current* bias is used as that function's offset, then bias
  increases by 6 before the next line (private slots consume a slot the
  same as public ones).
- `##public`/`##private` toggle the visibility of subsequent entries
  until the next such directive (not per-entry like AROS's `.private`
  suffix).
- Register lists use both `/` and `,` as separators inconsistently in
  the wild (e.g. exec.library's `MakeLibrary(...)(a0/a1/a2,d0/d1)`) --
  this parser treats both as equivalent.
- `*`-prefixed lines are comments. A useful minority of them record a
  minimum AmigaOS version in a handful of recurring phrasings (e.g.
  "added for V39 dos", "unimplemented until dos 36.147") -- see
  `SINCE_VERSION_PATTERNS` below. These are reported separately as
  *candidates* for a future `since`-version field (see docs/plan.md's
  Kickstart/Workbench 3.1 compatibility-target note); this tool
  extracts only the version number from a recognized pattern, never the
  surrounding comment text, and unrecognized comments are not
  extracted at all.

## Usage

    python3 tools/ndk_verify.py \\
        --ndk-fd /path/to/NDK/FD/dos_lib.fd \\
        --our-table crates/volamos-core/src/lvos/dos.rs

Exit code is non-zero if any function present in both sources has a
conflicting LVO offset or register list -- i.e. an actual disagreement,
not just coverage differences (the NDK having entries we haven't
implemented yet, or AROS-derived entries the NDK's version doesn't
have, are both reported but do not fail the run).
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

BIAS_RE = re.compile(r"^##bias\s+(\d+)\s*$")
FUNC_RE = re.compile(r"^(?P<name>[A-Za-z_][A-Za-z0-9_]*)\((?P<args>[^()]*)\)\s*\((?P<regs>[^()]*)\)\s*$")
REG_RE = re.compile(r"^([DdAa])([0-7])$")

# Recognized, narrow phrasings that name a minimum AmigaOS/library version.
# Only the captured version token is ever extracted -- the surrounding
# comment sentence is discarded, never stored or printed as extracted
# data (it's only shown as context in the human-readable report).
SINCE_VERSION_PATTERNS = [
    re.compile(r"added (?:for|in|with) V(\d+)", re.IGNORECASE),
    re.compile(r"added (?:for|with) dos (\d+)\.(\d+)", re.IGNORECASE),
    re.compile(r"unimplemented until dos (\d+)\.(\d+)", re.IGNORECASE),
    re.compile(r"did not exist before ks (\d+)\.(\d+)", re.IGNORECASE),
    re.compile(r"functions? in V(\d+) or higher", re.IGNORECASE),
]


@dataclass
class NdkEntry:
    name: str
    lvo: int
    regs: list[str] = field(default_factory=list)
    private: bool = False


@dataclass
class SinceHint:
    name: str
    raw_comment: str
    version_token: str


def parse_fd(text: str, *, source_label: str) -> tuple[list[NdkEntry], list[SinceHint]]:
    entries: list[NdkEntry] = []
    hints: list[SinceHint] = []
    bias: int | None = None
    private = False
    pending_comment: str | None = None

    for lineno, raw_line in enumerate(text.splitlines(), 1):
        line = raw_line.strip()
        if not line:
            continue
        if line == "##end":
            break
        if line == "##public":
            private = False
            continue
        if line == "##private":
            private = True
            continue
        if line.startswith("##base"):
            continue
        m = BIAS_RE.match(line)
        if m:
            bias = int(m.group(1))
            continue
        if line.startswith("*"):
            pending_comment = line.lstrip("*").strip()
            continue

        m = FUNC_RE.match(line)
        if not m:
            raise SystemExit(f"{source_label}:{lineno}: unrecognized .fd line: {line!r}")
        if bias is None:
            raise SystemExit(f"{source_label}:{lineno}: function before any ##bias directive")

        name = m.group("name")
        regs_str = m.group("regs").strip()
        regs = [r.strip() for r in re.split(r"[,/]", regs_str) if r.strip()]
        for r in regs:
            if not REG_RE.match(r):
                raise SystemExit(f"{source_label}:{lineno}: unrecognized register {r!r} in {line!r}")

        entries.append(NdkEntry(name=name, lvo=-bias, regs=[r.upper() for r in regs], private=private))
        bias += 6

        if pending_comment:
            for pat in SINCE_VERSION_PATTERNS:
                cm = pat.search(pending_comment)
                if cm:
                    hints.append(SinceHint(name=name, raw_comment=pending_comment, version_token=cm.group(0)))
                    break
        pending_comment = None

    return entries, hints


RUST_ENTRY_RE = re.compile(
    r'LvoEntry\s*\{\s*name:\s*"(?P<name>[^"]+)",\s*lvo:\s*(?P<lvo>-?\d+),\s*'
    r"args:\s*&\[(?P<args>[^\]]*)\],\s*private:\s*(?P<private>true|false),?\s*\}",
    re.DOTALL,
)
RUST_REG_RE = re.compile(r"ArgReg::([DA])\((?:DataRegister|AddressRegister)\((\d)\)\)")


@dataclass
class OurEntry:
    name: str
    lvo: int
    regs: list[str]
    private: bool


def parse_our_table(text: str) -> list[OurEntry]:
    out = []
    for m in RUST_ENTRY_RE.finditer(text):
        regs = [f"{kind}{num}" for kind, num in RUST_REG_RE.findall(m.group("args"))]
        out.append(
            OurEntry(
                name=m.group("name"),
                lvo=int(m.group("lvo")),
                regs=regs,
                private=m.group("private") == "true",
            )
        )
    return out


def _check_ndk_dir_outside_repo(path: Path) -> None:
    repo_root = Path(__file__).resolve().parent.parent
    resolved = path.resolve()
    if resolved == repo_root or repo_root in resolved.parents:
        raise SystemExit(
            f"refusing to read {resolved}: it is inside the volamos repository "
            f"({repo_root}). NDK content must never be placed inside the repo "
            "tree -- point --ndk-fd at a copy kept elsewhere on disk. See this "
            "script's module doc for why."
        )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ndk-fd", required=True, type=Path, help="path to an official NDK *_lib.fd file")
    ap.add_argument("--our-table", required=True, type=Path, help="path to our generated lvos/*.rs file")
    args = ap.parse_args()

    _check_ndk_dir_outside_repo(args.ndk_fd)

    ndk_text = args.ndk_fd.read_text()
    ndk_entries, hints = parse_fd(ndk_text, source_label=str(args.ndk_fd))
    ndk_by_name = {e.name: e for e in ndk_entries}

    our_text = args.our_table.read_text()
    our_entries = parse_our_table(our_text)
    our_by_name = {e.name: e for e in our_entries}
    if not our_entries:
        raise SystemExit(f"{args.our_table}: parsed zero LvoEntry records -- wrong file or format changed?")

    mismatches = []
    matches = 0
    for name, ndk_e in ndk_by_name.items():
        our_e = our_by_name.get(name)
        if our_e is None:
            continue
        if our_e.lvo != ndk_e.lvo:
            mismatches.append(
                f"  {name}: LVO offset differs -- ours {our_e.lvo}, NDK {ndk_e.lvo}"
            )
            continue
        if our_e.regs != ndk_e.regs:
            mismatches.append(
                f"  {name}: register list differs -- ours {our_e.regs}, NDK {ndk_e.regs}"
            )
            continue
        matches += 1

    ndk_only = sorted(set(ndk_by_name) - set(our_by_name))
    our_only = sorted(set(our_by_name) - set(ndk_by_name))

    print(f"NDK source:  {args.ndk_fd} ({len(ndk_entries)} entries)")
    print(f"Our table:   {args.our_table} ({len(our_entries)} entries)")
    print()
    print(f"Agree (name+offset+registers match): {matches}")
    print()

    if mismatches:
        print(f"DISAGREEMENTS ({len(mismatches)}) -- these need investigation:")
        for m in mismatches:
            print(m)
        print()

    if ndk_only:
        print(f"In NDK but not in our table ({len(ndk_only)}): not yet implemented, informational only.")
        print(f"  {', '.join(ndk_only[:20])}{' ...' if len(ndk_only) > 20 else ''}")
        print()

    if our_only:
        print(f"In our table but not in this NDK file ({len(our_only)}): worth understanding why")
        print("  (AROS-only extension beyond this NDK version's API? naming mismatch?):")
        print(f"  {', '.join(our_only[:20])}{' ...' if len(our_only) > 20 else ''}")
        print()

    if hints:
        print(f"Version-introduced candidates found in NDK comments ({len(hints)}):")
        print("  (raw comment shown for human review only -- nothing here is auto-applied;")
        print("  cross-check against RKRM/Autodocs before recording a `since` value)")
        for h in hints:
            print(f'  {h.name}: "{h.raw_comment}" (matched: {h.version_token})')
        print()

    if mismatches:
        print(f"FAILED: {len(mismatches)} disagreement(s) between our table and the official NDK.")
        return 1

    print("OK: no disagreements between our table and the official NDK for overlapping entries.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
