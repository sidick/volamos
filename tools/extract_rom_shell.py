#!/usr/bin/env python3
"""Extracts the AmigaDOS Shell from a Kickstart ROM as a hunk executable
volamos can run with `--shell` (issue #20).

In Kickstart 3.x the Shell is not a disk file (`L:Shell-Seg` only
appeared on disk with later releases): it is the resident module
`shell`, linked into the ROM. This tool cuts that module out of a ROM
image *you supply* and rebuilds the hunk file it was linked from. No ROM
or Shell is shipped with volamos, and this reads yours.

What it relies on, and checks:

- The module is a "Shell-Seg": a segment with BCPL linkage that starts
  with two longwords (0 and a small number) and has the BCPL call-in at
  +8, followed some way in by its `struct Resident` (`0x4AFC`). The ROM
  build leaves a pointer to the segment's start just before that struct,
  which is how the tool finds where the module begins (the header is
  0x34 bytes in `shell 46.10` of the 3.1.4 ROM and 0x58 in `shell 40.2` of
  the 3.1 ROM). The same layout is in the `L:Shell-Seg` of later disk
  releases.
- ROM modules have their absolute addresses resolved at ROM-build time,
  where a hunk file stores them relative to the hunk and lists them in
  a `HUNK_RELOC32`. So every longword in the module that points into
  the module itself is turned back into an offset and listed. The scan
  steps by two bytes, not four: the Shell's lookup tables of names and
  built-in command entry points are word-aligned (a 68020 reads such
  longwords without complaint). A false positive needs a pair of words in
  the module that happen to read `0x00FC` followed by an address inside
  it.
- Pointers out of the module (into other ROM modules) are left alone: the
  Shell reaches the rest of the system through library bases.

    python3 tools/extract_rom_shell.py kick.rom Shell-Seg
"""

import argparse
import struct
import sys

HUNK_HEADER = 0x3F3
HUNK_CODE = 0x3E9
HUNK_RELOC32 = 0x3EC
HUNK_END = 0x3F2

def be32(data: bytes, off: int) -> int:
    return struct.unpack_from(">I", data, off)[0]


def find_residents(rom: bytes, base: int):
    """Yields (offset, endskip_offset, name) for each struct Resident."""
    for off in range(0, len(rom) - 26, 2):
        if rom[off : off + 2] != b"\x4a\xfc":
            continue
        if be32(rom, off + 2) != base + off:  # RT_MATCHTAG points at itself
            continue
        end = be32(rom, off + 6) - base
        name_ptr = be32(rom, off + 14) - base
        if not (0 <= name_ptr < len(rom)) or not (off < end <= len(rom)):
            continue
        name = rom[name_ptr : name_ptr + 32].split(b"\0")[0].decode("latin-1")
        yield off, end, name


def guess_base(rom: bytes) -> int:
    """The ROM's base address: the first struct Resident's RT_MATCHTAG is
    its own address, so base = matchtag - offset."""
    for off in range(0, min(len(rom), 0x1000), 2):
        if rom[off : off + 2] == b"\x4a\xfc":
            tag = be32(rom, off + 2)
            base = tag - off
            if base % 0x10000 == 0 and 0xE00000 <= base <= 0xFF0000:
                return base
    raise SystemExit("no struct Resident found near the start: not a Kickstart ROM?")


def build_hunk(payload: bytes, relocs: list[int]) -> bytes:
    assert len(payload) % 4 == 0
    longs = len(payload) // 4
    out = struct.pack(">IIIII", HUNK_HEADER, 0, 1, 0, 0)
    out += struct.pack(">I", longs)
    out += struct.pack(">II", HUNK_CODE, longs) + payload
    if relocs:
        out += struct.pack(">III", HUNK_RELOC32, len(relocs), 0)
        out += b"".join(struct.pack(">I", r) for r in relocs)
        out += struct.pack(">I", 0)
    out += struct.pack(">I", HUNK_END)
    return out


def extract(rom: bytes, module: str = "shell") -> tuple[bytes, dict]:
    base = guess_base(rom)
    matches = [r for r in find_residents(rom, base) if r[2] == module]
    if len(matches) != 1:
        raise SystemExit(f"expected one {module!r} resident, found {len(matches)}")
    res_off, end, _ = matches[0]
    # The two longwords just before the struct Resident are pointers the
    # ROM build left there: the start of the segment, and a second address
    # inside it. That makes the start independent of how long a given
    # release's header is (0x34 bytes in shell 46.10, 0x58 in 40.2).
    seg_start = be32(rom, res_off - 8) - base
    second = be32(rom, res_off - 4) - base
    if not (0 <= seg_start < res_off and seg_start % 4 == 0 and seg_start <= second < end):
        raise SystemExit(
            f"{module!r}: the pointers before its struct Resident do not describe a "
            "segment: this is not the Shell layout the tool knows"
        )
    start = seg_start
    payload = bytearray(rom[start:end])
    if len(payload) % 4:
        raise SystemExit(
            f"the {module!r} module is {len(payload)} bytes, not a whole number of "
            "longwords: this is not the Shell layout the tool knows"
        )
    # Shell-Seg signature: the segment starts with a zero longword and a
    # small one (BCPL linkage), then the call-in at +8.
    if be32(payload, 0) != 0 or be32(payload, 4) > 0x100:
        raise SystemExit(f"{module!r} does not have a Shell-Seg header")
    lo, hi = base + start, base + end
    relocs = []
    off = 0
    while off + 4 <= len(payload):
        value = be32(payload, off)
        if lo <= value <= hi:
            relocs.append(off)
            struct.pack_into(">I", payload, off, value - lo)
            off += 4  # a pointer's bytes are not the start of another
        else:
            off += 2  # tables are word-aligned, not longword-aligned
    info = {
        "base": base,
        "start": start,
        "end": end,
        "size": len(payload),
        "relocs": len(relocs),
    }
    return build_hunk(bytes(payload), relocs), info


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("rom", help="Kickstart ROM image (512K or 256K)")
    ap.add_argument("out", help="hunk executable to write")
    ap.add_argument("--module", default="shell", help="resident to extract")
    args = ap.parse_args()
    with open(args.rom, "rb") as f:
        rom = f.read()
    hunk, info = extract(rom, args.module)
    with open(args.out, "wb") as f:
        f.write(hunk)
    print(
        f"wrote {args.out}: {info['size']} bytes from ROM offset {info['start']:#x} "
        f"(base {info['base']:#x}), {info['relocs']} relocations",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
