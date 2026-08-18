#!/usr/bin/env python3
"""Generate a `crates/volamos-core/src/lvos/*.rs` LVO metadata table from an
AROS library interface description (originally written for `dos.library`;
generalized in T12 to also emit `exec.library`'s table -- any AROS `.conf`
with a `##begin functionlist` block works via `--table-name`/`--library-doc`/
`--module-doc`/`--sanity`).

This is a **one-shot codegen tool**, not a build-time dependency. Run it by
hand when the upstream interface description changes, review the diff, and
commit the regenerated `.rs` file like any other source change.

## Source format

AROS's own repository does not check in generated `.sfd`/`.fd` files for
its ROM-resident libraries; those are produced at build time by AROS's
`genmodule` tooling from a `.conf` file per library
(`rom/dos/dos.conf` for `dos.library`). That `.conf` file's
`##begin functionlist` / `##end functionlist` block is the single source of
truth `dos_lib.sfd` would otherwise be generated from, and it encodes the
exact same facts an SFD does: function name, LVO bias (offset), and the
argument-register calling convention, using its own directive spellings:

  - `<decl> Name(args) (REGLIST)` -- one function, in bias order. Bias
    starts at 6 and increases by 6 for every slot consumed (a real
    function *or* a skipped one), so `LVO = -bias`. `REGLIST` is a
    comma-separated list of `Dn`/`An` register names in call order (or
    empty for a no-argument call).
  - `.private` -- applies to the immediately preceding function; marks it
    as an internal vector not meant for guest code to call directly
    (AROS uses this for `OpenLib`/`CloseLib`, which real guest code never
    calls via LVO -- that's `exec.library`'s `OpenLibrary`/`CloseLibrary`
    job). We still emit these entries (flagged `private: true`) rather
    than silently dropping bias-consuming slots.
  - `.skip N` -- reserves N consecutive slots with no function attached
    (advances bias by `6*N`, emits nothing). AROS uses this for
    intentionally-unimplemented, historical, or vendor-reserved (MorphOS
    compatibility) slots.
  - `.version NN` -- informational only ("this and subsequent entries
    were added in library version NN"); does not affect bias. Ignored by
    this tool -- we don't currently track a per-call minimum version.
  - `.novararg` -- informational only (tells AROS's stub generator not to
    emit a variadic amiga.lib wrapper for the preceding function); has no
    effect on the LVO/register ABI. Ignored.
  - `#`-prefixed lines and blank lines -- comments, ignored.

No line in the emitted output is copied from the source file: this script
extracts only the (uncopyrightable) interface facts -- names, offsets,
register assignments -- into a fresh Rust literal; see the provenance
header this script writes into the generated file for the exact source
commit and license note.

GUARDRAIL FOR FUTURE CHANGES: keep it that way. The `.conf` source (and
the `.fd`/`.sfd` formats it stands in for) carries comments, typed
argument names, and version annotations alongside the bare facts -- if
you extend `render()` to emit any of that (argument names, descriptive
text, `.version`/comment content), you've crossed from extracting facts
to copying expression, which is the whole basis of this tool's licensing
position (see docs/plan.md's "fd/SFD metadata decision" section). Only
name, LVO offset, and register letters belong in the output.

## Usage

    python3 tools/gen_lvos.py \\
        --input /path/to/dos.conf \\
        --source-url https://raw.githubusercontent.com/aros-development-team/AROS/<sha>/rom/dos/dos.conf \\
        --commit <sha> \\
        --output crates/volamos-core/src/lvos/dos.rs

`--input` may be omitted to fetch `--source-url` directly (stdlib
`urllib` only -- no third-party dependencies). Output is fully
deterministic for a given input: re-running with the same `--input` and
the same `--source-url`/`--commit`/`--generated` values byte-for-byte
reproduces the file.

## The implicit reserved-vector header (`--start-bias`)

Every AmigaOS library reserves four jump-table slots ahead of its public
functions -- LVO -6/-12/-18/-24 for Open/Close/Expunge/reserved, called by
`exec.library`'s `OpenLibrary`/`CloseLibrary`/`RemLibrary`/expunge
machinery, not by guest code directly -- before the public API starts at
-30. AROS's own `genmodule` tool (`tools/genmodule/config.c`,
`cfg->firstlvo`) reserves these four slots for every `modtype=library`
`.conf` unconditionally, whether or not the `.conf`'s own
`##begin functionlist` block spells them out. `dos.conf`/`exec.conf`
happen to spell them out explicitly (`OpenLib`/`CloseLib`/`.skip 2` as
the first three functionlist directives), so this script's own
bias-starts-at-0 parser reproduces the correct real LVOs for them without
any extra help. Other libraries' `.conf` (e.g. `utility.conf`) omit that
preamble and rely on genmodule's implicit default -- for those, pass
`--start-bias 24` so the first parsed entry still lands on the correct
real bias (30, not 6). Verify against a couple of independently known
LVOs from public AmigaOS documentation before trusting the result either
way; a wrong `--start-bias` silently produces a self-consistent but
uniformly-offset table.
"""

from __future__ import annotations

import argparse
import re
import sys
import urllib.request
from dataclasses import dataclass, field

FUNCTIONLIST_BEGIN = "##begin functionlist"
FUNCTIONLIST_END = "##end functionlist"

# `<decl> Name(args) (REGLIST)`, e.g.:
#   "struct DosLibrary *OpenLib(ULONG version) (D0)"
#   "LONG Read(BPTR file, APTR buffer, LONG length) (D1, D2, D3)"
#   "BPTR Input() ()"
FUNC_RE = re.compile(r"^(?P<decl>.+)\((?P<args>[^()]*)\)\s*\((?P<regs>[^()]*)\)\s*$")
SKIP_RE = re.compile(r"^\.skip\s+(\d+)\b")
VERSION_RE = re.compile(r"^\.version\s+\S+")
REG_RE = re.compile(r"^([DA])([0-7])$")


@dataclass
class LvoEntry:
    name: str
    lvo: int
    regs: list[str] = field(default_factory=list)
    private: bool = False


def parse_functionlist(text: str, *, source_label: str, start_bias: int = 0) -> list[LvoEntry]:
    try:
        start = text.index(FUNCTIONLIST_BEGIN) + len(FUNCTIONLIST_BEGIN)
        end = text.index(FUNCTIONLIST_END, start)
    except ValueError as e:
        raise SystemExit(
            f"{source_label}: could not find {FUNCTIONLIST_BEGIN}/{FUNCTIONLIST_END} markers"
        ) from e
    block = text[start:end]

    entries: list[LvoEntry] = []
    bias = start_bias
    for lineno, raw_line in enumerate(block.splitlines(), 1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line == ".private":
            if not entries:
                raise SystemExit(f"{source_label}:{lineno}: .private with no preceding function")
            entries[-1].private = True
            continue
        if line == ".novararg":
            continue
        if VERSION_RE.match(line):
            continue
        m = SKIP_RE.match(line)
        if m:
            bias += 6 * int(m.group(1))
            continue

        m = FUNC_RE.match(line)
        if not m:
            raise SystemExit(f"{source_label}:{lineno}: unrecognized functionlist line: {line!r}")

        decl = m.group("decl").strip()
        name = decl.split()[-1].lstrip("*")
        regs_str = m.group("regs").strip()
        regs = [r.strip() for r in regs_str.split(",")] if regs_str else []
        for r in regs:
            if not REG_RE.match(r):
                raise SystemExit(
                    f"{source_label}:{lineno}: unrecognized register {r!r} in {line!r}"
                )

        bias += 6
        entries.append(LvoEntry(name=name, lvo=-bias, regs=regs))

    return entries


def reg_literal(reg: str) -> str:
    kind, num = REG_RE.match(reg).groups()  # type: ignore[union-attr]
    ty = "DataRegister" if kind == "D" else "AddressRegister"
    variant = "D" if kind == "D" else "A"
    return f"ArgReg::{variant}({ty}({num}))"


def render(
    entries: list[LvoEntry],
    *,
    source_url: str,
    commit: str,
    generated: str,
    generator: str,
    library_doc: str,
    conf_path: str,
    table_name: str,
    sanity: list[tuple[str, int]],
) -> str:
    lines: list[str] = []
    lines.append(f"//! Generated `{library_doc}` LVO (library vector offset) metadata table.")
    lines.append("//!")
    lines.append("//! # Provenance")
    lines.append("//!")
    lines.append(f"//! Derived from AROS's `{library_doc}` interface description")
    lines.append(f"//! (`{conf_path}`, the `##begin functionlist` block AROS's own build")
    lines.append("//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for")
    lines.append("//! why this repo reads the `.conf` directly rather than a generated `.sfd`).")
    lines.append("//!")
    lines.append(f"//! - Source URL: <{source_url}>")
    lines.append(f"//! - Source commit: {commit}")
    lines.append(f"//! - Generated: {generated}")
    lines.append(f"//! - Generator: `{generator}`")
    lines.append("//!")
    lines.append("//! Only uncopyrightable interface facts were extracted from the source --")
    lines.append("//! function names, LVO offsets, and argument-register assignments -- as")
    lines.append("//! bare data; no descriptive text, comments, or file structure from the")
    lines.append("//! source was copied. This file is licensed under the same terms as the")
    lines.append("//! rest of this repository: MIT OR Apache-2.0.")
    lines.append("//!")
    lines.append("//! DO NOT EDIT BY HAND. Regenerate with `tools/gen_lvos.py`.")
    lines.append("")
    lines.append("use crate::cpu::{AddressRegister, DataRegister};")
    lines.append("use crate::lvos::{ArgReg, LvoEntry};")
    lines.append("")
    lines.append(f"/// The full `{library_doc}` LVO table (all known functions, not just the")
    lines.append("/// ones this runtime currently implements handlers for -- this way")
    lines.append("/// unknown-call diagnostics can print a real function name for any of")
    lines.append("/// them, not just the handful we emulate).")
    lines.append(f"pub static {table_name}: &[LvoEntry] = &[")
    for e in entries:
        regs = ", ".join(reg_literal(r) for r in e.regs)
        private = "true" if e.private else "false"
        lines.append(
            f'    LvoEntry {{ name: "{e.name}", lvo: {e.lvo}, '
            f"args: &[{regs}], private: {private} }},"
        )
    lines.append("];")
    lines.append("")
    lines.append("#[cfg(test)]")
    lines.append("mod tests {")
    lines.append("    use super::*;")
    lines.append("    use crate::lvos::find_by_name;")
    lines.append("")
    lines.append("    // Sanity-check a handful of well-known LVOs against published AmigaOS")
    lines.append(f"    // {library_doc} values (see docs/plan.md's T7/T12 entries).")
    lines.append("    #[test]")
    lines.append("    fn known_lvos_match_amigaos() {")
    lines.append("        let cases: &[(&str, i32)] = &[")
    for name, lvo in sanity:
        lines.append(f'            ("{name}", {lvo}),')
    lines.append("        ];")
    lines.append("        for (name, lvo) in cases {")
    lines.append(f'            let entry = find_by_name({table_name}, name)')
    lines.append('                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));')
    lines.append("            assert_eq!(entry.lvo, *lvo, \"{name} LVO mismatch\");")
    lines.append("        }")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    return "\n".join(lines)


# Sanity cases baked in per-library so a plain `--library dos`/`--library
# exec` invocation (no need to spell out every `--sanity` flag by hand)
# reproduces the exact tables this repo committed. Extra `--sanity`
# flags on the command line are appended to whichever of these applies.
_BUILTIN_SANITY: dict[str, list[tuple[str, int]]] = {
    "dos": [
        ("Open", -30),
        ("Close", -36),
        ("Read", -42),
        ("Write", -48),
        ("Input", -54),
        ("Output", -60),
        ("Seek", -66),
        ("Lock", -84),
        ("Examine", -102),
        ("ExNext", -108),
        ("CurrentDir", -126),
        ("IoErr", -132),
        ("ParentDir", -210),
        ("PutStr", -948),
    ],
    "exec": [
        ("OpenLibrary", -552),
        ("OldOpenLibrary", -408),
        ("CloseLibrary", -414),
        ("AllocMem", -198),
        ("FreeMem", -210),
        ("FindTask", -294),
    ],
}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--input", help="local path to the .conf file (fetched from --source-url if omitted)")
    ap.add_argument("--source-url", required=True, help="canonical raw URL of the source file")
    ap.add_argument("--commit", required=True, help="git commit hash of the source file version used")
    ap.add_argument("--generated", required=True, help="generation date, e.g. 2026-08-18")
    ap.add_argument("--generator", default="tools/gen_lvos.py", help="path to this script, for the provenance header")
    ap.add_argument("--output", required=True, help="path to write the generated .rs file")
    ap.add_argument(
        "--library",
        choices=sorted(_BUILTIN_SANITY),
        help="shorthand: fills in --table-name/--library-doc/--conf-path/--sanity defaults for a known library",
    )
    ap.add_argument("--table-name", help='Rust static name, e.g. "EXEC_LVOS" (default derived from --library)')
    ap.add_argument("--library-doc", help='display name, e.g. "exec.library" (default derived from --library)')
    ap.add_argument("--conf-path", help='source path shown in the doc comment, e.g. "rom/exec/exec.conf"')
    ap.add_argument(
        "--sanity",
        action="append",
        default=[],
        metavar="NAME=LVO",
        help="extra sanity-check case to append to the generated test (repeatable)",
    )
    ap.add_argument(
        "--start-bias",
        type=int,
        default=0,
        help=(
            "initial bias before the first entry in the functionlist block "
            "(default 0; see the module docstring's 'implicit reserved-vector "
            "header' section -- pass 24 for a library .conf that omits the "
            "OpenLib/CloseLib/.skip 2 preamble dos.conf/exec.conf spell out "
            "explicitly)"
        ),
    )
    args = ap.parse_args()

    table_name = args.table_name or (f"{args.library.upper()}_LVOS" if args.library else None)
    library_doc = args.library_doc or (f"{args.library}.library" if args.library else None)
    conf_path = args.conf_path or (f"rom/{args.library}/{args.library}.conf" if args.library else None)
    if not (table_name and library_doc and conf_path):
        raise SystemExit("--library, or all of --table-name/--library-doc/--conf-path, is required")

    sanity = list(_BUILTIN_SANITY.get(args.library, []))
    for item in args.sanity:
        name, _, lvo = item.partition("=")
        if not _:
            raise SystemExit(f"--sanity {item!r} must be NAME=LVO")
        sanity.append((name, int(lvo)))

    if args.input:
        with open(args.input, "r", encoding="utf-8") as f:
            text = f.read()
        source_label = args.input
    else:
        with urllib.request.urlopen(args.source_url) as resp:  # noqa: S310
            text = resp.read().decode("utf-8")
        source_label = args.source_url

    entries = parse_functionlist(text, source_label=source_label, start_bias=args.start_bias)
    if not entries:
        raise SystemExit(f"{source_label}: no entries parsed")

    # Determinism / sanity: no duplicate names, no duplicate LVOs.
    names = [e.name for e in entries]
    if len(names) != len(set(names)):
        dupes = sorted({n for n in names if names.count(n) > 1})
        raise SystemExit(f"duplicate function names in source: {dupes}")
    lvos = [e.lvo for e in entries]
    if len(lvos) != len(set(lvos)):
        raise SystemExit("duplicate LVO offsets computed -- parser bug or malformed source")

    out = render(
        entries,
        source_url=args.source_url,
        commit=args.commit,
        generated=args.generated,
        generator=args.generator,
        library_doc=library_doc,
        conf_path=conf_path,
        table_name=table_name,
        sanity=sanity,
    )
    with open(args.output, "w", encoding="utf-8") as f:
        f.write(out)

    print(f"wrote {len(entries)} entries to {args.output}", file=sys.stderr)


if __name__ == "__main__":
    main()
