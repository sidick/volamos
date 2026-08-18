#!/usr/bin/env python3
"""A tiny two-pass 68000 "assembler" helper shared by the Phase 2 (T14)
fixture generators (`gen_filetest.py`, `gen_dirtest.py`,
`gen_echoargs.py`).

This is *not* a general assembler -- it implements exactly the
instructions/addressing modes those three fixtures need, one method per
opcode shape, each derived directly from the M68000 Programmer's
Reference Manual's instruction encoding tables (same derivation style,
and same "no vasm available" rationale, as `gen_hello.py`). Two passes:

1. Build: `CodeBuilder` methods append 16-bit words to a flat list as
   instructions are emitted, and record the word-index of any label
   (`CodeBuilder.label`) as it's reached. Absolute-pointer references to
   `DataBuilder` labels (`move_l_label_to_d`/`_a`) and branch targets
   (`branch`/`dbra`) are left as placeholder words, remembered in
   `_abs32_fixups`/`_branch_fixups`.
2. Resolve (`CodeBuilder.resolve`): once every label's final (hunk-local)
   byte offset is known -- which, since hunks are packed with a fixed
   layout, is true as soon as building finishes -- patches every
   placeholder: branch/`dbra` displacements are resolved directly (both
   ends live in the same CODE hunk, so no relocation entry is needed --
   PC-relative branches are position-independent); absolute pointers into
   the DATA hunk are written as `label's offset within hunk 1` and
   recorded as a `HUNK_RELOC32` entry, exactly like `gen_hello.py`'s
   single hand-written one -- the loader adds hunk 1's actual load
   address to that stored offset at load time (see
   `crates/volamos-core/src/loader.rs`'s `load`), turning it into an
   absolute address.

All three fixtures are a single CODE hunk (hunk 0) + a single DATA hunk
(hunk 1, all strings/scratch buffers), matching `fixtures/hello.s`'s
existing two-hunk shape. `build_hunk_executable` emits that file.
"""

from __future__ import annotations

import struct


def u32(v: int) -> bytes:
    return struct.pack(">I", v & 0xFFFF_FFFF)


def u16(v: int) -> bytes:
    return struct.pack(">H", v & 0xFFFF)


# --- Hunk type identifiers (see crates/volamos-core/src/loader.rs) ---
HUNK_HEADER = 0x3F3
HUNK_CODE = 0x3E9
HUNK_DATA = 0x3EA
HUNK_RELOC32 = 0x3EC
HUNK_END = 0x3F2


class DataBuilder:
    """Builds the DATA hunk's raw bytes plus a label->byte-offset map.
    No relocations of its own (nothing in these fixtures' data needs a
    pointer *into* code or into itself)."""

    def __init__(self) -> None:
        self.bytes = bytearray()
        self.labels: dict[str, int] = {}

    def label(self, name: str) -> None:
        assert name not in self.labels, f"duplicate data label {name!r}"
        self.labels[name] = len(self.bytes)

    def cstr(self, name: str, text: str) -> None:
        """NUL-terminated string, e.g. a dos.library name or an Amiga
        path argument (`Open`'s `D1`, `Lock`'s `D1`, ... all expect
        `CString*`)."""
        self.label(name)
        self.bytes += text.encode("ascii") + b"\0"

    def zeros(self, name: str, n: int) -> None:
        """`n` zeroed bytes -- a scratch buffer (read buffer,
        `FileInfoBlock`, string-building buffer) the guest program fills
        in at run time."""
        self.label(name)
        self.bytes += bytes(n)

    def u32s(self, name: str, values: list[int]) -> None:
        """A sequence of big-endian 32-bit words -- e.g. a `TagItem`
        array (`{ti_Tag, ti_Data}` pairs, `<utility/tagitem.h>`) built
        directly in the DATA hunk for a fixture to hand `utility.library`
        calls like `GetTagData`. Added for Phase 3 stage 8's `exectest`
        fixture (`gen_exectest.py`), the first one needing raw longword
        data rather than a C string or a zeroed scratch buffer."""
        self.label(name)
        for v in values:
            self.bytes += u32(v)

    def align4(self) -> None:
        while len(self.bytes) % 4:
            self.bytes.append(0)


class CodeBuilder:
    """Builds the CODE hunk's word stream. See module docs for the
    two-pass fixup design."""

    def __init__(self) -> None:
        self.words: list[int] = []
        self.labels: dict[str, int] = {}
        # (word_index_of_high_half, data_label, addend)
        self._abs32_fixups: list[tuple[int, str, int]] = []
        # (word_index_of_disp_word, code_label)
        self._branch_fixups: list[tuple[int, str]] = []

    def word(self, value: int) -> int:
        self.words.append(value & 0xFFFF)
        return len(self.words) - 1

    def label(self, name: str) -> None:
        assert name not in self.labels, f"duplicate code label {name!r}"
        self.labels[name] = len(self.words)

    # --- data movement ---

    def move_l_imm_to_d(self, dn: int, imm: int) -> None:
        """`move.l #imm,Dn` -- MOVE, dest=Dn direct, src=immediate long."""
        self.word(0x203C | (dn << 9))
        self.word((imm >> 16) & 0xFFFF)
        self.word(imm & 0xFFFF)

    def move_l_imm_to_a(self, an: int, imm: int) -> None:
        """`move.l #imm,An` (MOVEA.L) -- dest=An direct, src=immediate
        long."""
        self.word(0x207C | (an << 9))
        self.word((imm >> 16) & 0xFFFF)
        self.word(imm & 0xFFFF)

    def move_l_label_to_d(self, dn: int, label: str, addend: int = 0) -> None:
        """`move.l #label(+addend),Dn`, with the immediate patched by
        [`CodeBuilder.resolve`] to `label`'s (eventually absolute, via
        HUNK_RELOC32) address."""
        idx = self.word(0x203C | (dn << 9))
        self.word(0)
        self.word(0)
        self._abs32_fixups.append((idx + 1, label, addend))

    def move_l_label_to_a(self, an: int, label: str, addend: int = 0) -> None:
        """As [`CodeBuilder.move_l_label_to_d`], but into an address
        register (MOVEA)."""
        idx = self.word(0x207C | (an << 9))
        self.word(0)
        self.word(0)
        self._abs32_fixups.append((idx + 1, label, addend))

    def move_l_d_to_d(self, dst: int, src: int) -> None:
        """`move.l Dsrc,Ddst`."""
        self.word(0x2000 | (dst << 9) | src)

    def move_l_a_to_d(self, dst_dn: int, src_an: int) -> None:
        """`move.l Asrc,Ddst` -- src addressing mode 001 (An direct)."""
        self.word(0x2000 | (dst_dn << 9) | 0x08 | src_an)

    def move_l_d_to_a(self, dst_an: int, src_dn: int) -> None:
        """`move.l Dsrc,Adst` (MOVEA) -- dest addressing mode 001 (An
        direct), e.g. `move.l d0,a6` to install a just-returned library
        base."""
        self.word(0x2000 | (dst_an << 9) | 0x40 | src_dn)

    def move_l_abs4_to_a(self, an: int) -> None:
        """`move.l 4.w,An` -- reads `AbsExecBase` (guest address 4;
        absolute-short addressing mode 111/000)."""
        self.word(0x2078 | (an << 9))
        self.word(0x0004)

    def moveq(self, dn: int, imm: int) -> None:
        """`moveq #imm,Dn` (`-128 <= imm <= 127`)."""
        assert -128 <= imm <= 127, f"moveq immediate {imm} out of range"
        self.word(0x7000 | (dn << 9) | (imm & 0xFF))

    def move_b_postinc_to_d(self, dn: int, an: int) -> None:
        """`move.b (An)+,Dn`."""
        self.word(0x1000 | (dn << 9) | 0x18 | an)

    def move_b_postinc_to_postinc(self, dst_an: int, src_an: int) -> None:
        """`move.b (Asrc)+,(Adst)+`."""
        self.word(0x1000 | (dst_an << 9) | 0xD8 | src_an)

    def move_b_imm_to_postinc(self, an: int, imm: int) -> None:
        """`move.b #imm,(An)+`. Byte immediates still occupy a full
        extension word (data in the low byte)."""
        self.word(0x10FC | (an << 9))
        self.word(imm & 0xFF)

    # --- control flow / calls ---

    def jsr_disp16_a(self, an: int, disp: int) -> None:
        """`jsr <disp16>(An)`."""
        self.word(0x4EA8 | an)
        self.word(disp & 0xFFFF)

    def rts(self) -> None:
        self.word(0x4E75)

    def tst_l_d(self, dn: int) -> None:
        self.word(0x4A80 | dn)

    def clr_l_d(self, dn: int) -> None:
        self.word(0x4280 | dn)

    def subq_l_imm_d(self, dn: int, imm: int) -> None:
        """`subq.l #imm,Dn` (`1 <= imm <= 8`, encoded as `imm % 8`)."""
        assert 1 <= imm <= 8
        q = imm % 8
        self.word(0x5100 | (q << 9) | 0x80 | dn)

    def sub_l_d_from_d(self, dst: int, src: int) -> None:
        """`sub.l Dsrc,Ddst` -- `Ddst = Ddst - Dsrc` (SUB, long, dest=Dn
        direct with the "Dn - <ea> -> Dn" opmode `100`, src=Dn direct
        addressing mode `000`). Verified against
        `crates/volamos-core/src/execmem.rs`'s own hand-assembled test
        (`0x9480` there is commented `sub.l D0,D2`, i.e. `dst=2, src=0`:
        `0x9080 | (2 << 9) | 0 == 0x9480`, confirming this formula)."""
        self.word(0x9080 | (dst << 9) | src)

    # Word-form branch opcode bases (displacement byte = 0x00, so a
    # 16-bit displacement word always follows).
    BRA = 0x6000
    BSR = 0x6100
    BEQ = 0x6700
    BNE = 0x6600

    def branch(self, opcode_base: int, label: str) -> None:
        self.word(opcode_base)
        idx = self.word(0)
        self._branch_fixups.append((idx, label))

    def dbra(self, dn: int, label: str) -> None:
        """`dbra Dn,label` (DBF: decrement-and-branch-unless--1)."""
        self.word(0x51C8 | dn)
        idx = self.word(0)
        self._branch_fixups.append((idx, label))

    # --- resolution ---

    def resolve(self, data: DataBuilder) -> list[tuple[int, int]]:
        """Patches every placeholder word. Returns the list of
        `(byte_offset_in_code_hunk, target_hunk)` HUNK_RELOC32 entries
        `build_hunk_executable` should emit (target_hunk is always `1`,
        the data hunk, for these fixtures)."""
        relocs: list[tuple[int, int]] = []
        for word_idx, label, addend in self._abs32_fixups:
            # word_idx is the index of the *high* half of the 32-bit
            # immediate (the word right after the opcode word -- see
            # move_l_label_to_d/_a); word_idx + 1 is the low half.
            target = data.labels[label] + addend
            self.words[word_idx] = (target >> 16) & 0xFFFF
            self.words[word_idx + 1] = target & 0xFFFF
            relocs.append((word_idx * 2, 1))
        for word_idx, label in self._branch_fixups:
            target_addr = self.labels[label] * 2
            disp_word_addr = word_idx * 2
            disp = target_addr - disp_word_addr
            assert -32768 <= disp <= 32767, "branch displacement out of word range"
            self.words[word_idx] = disp & 0xFFFF
        return relocs

    def to_bytes(self) -> bytes:
        out = bytearray()
        for w in self.words:
            out += u16(w)
        return bytes(out)


def build_hunk_executable(code: CodeBuilder, data: DataBuilder) -> bytes:
    """Resolves `code` against `data`'s labels and emits a two-hunk
    (CODE + DATA) hunk executable, in the same shape as
    `gen_hello.py`'s `build()`."""
    # Pad the code hunk out to a whole number of longwords with a NOP
    # (0x4E71) if needed -- added after every real instruction/label, so
    # it can't shift any label's resolved offset.
    if len(code.words) % 2 != 0:
        code.word(0x4E71)
    relocs = code.resolve(data)
    code_bytes = code.to_bytes()
    assert len(code_bytes) % 4 == 0, "code hunk must be a whole number of longwords"
    data.align4()
    data_bytes = bytes(data.bytes)
    assert len(data_bytes) % 4 == 0, "data hunk must be a whole number of longwords"

    code_longwords = len(code_bytes) // 4
    data_longwords = len(data_bytes) // 4

    out = bytearray()
    out += u32(HUNK_HEADER)
    out += u32(0)  # no resident library names
    out += u32(2)  # table_size: 2 hunks
    out += u32(0)  # first_hunk
    out += u32(1)  # last_hunk
    out += u32(code_longwords)
    out += u32(data_longwords)

    out += u32(HUNK_CODE)
    out += u32(code_longwords)
    out += code_bytes
    if relocs:
        out += u32(HUNK_RELOC32)
        out += u32(len(relocs))
        out += u32(1)  # target hunk (data) -- all fixups target hunk 1
        for offset, _target_hunk in relocs:
            out += u32(offset)
        out += u32(0)  # terminate RELOC32 groups
    out += u32(HUNK_END)

    out += u32(HUNK_DATA)
    out += u32(data_longwords)
    out += data_bytes
    out += u32(HUNK_END)

    return bytes(out)
