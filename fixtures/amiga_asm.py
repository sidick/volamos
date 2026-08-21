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
        # (word_index_of_high_half, code_label, addend) -- pointers to a
        # label *in this same builder* (self-hunk), as opposed to
        # _abs32_fixups' pointers into a separate DATA hunk. Added for
        # Phase L3's testlib fixtures (see dc_l_selfptr/resolve_self).
        self._self_abs32_fixups: list[tuple[int, str, int]] = []

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

    def move_l_a_to_a(self, dst_an: int, src_an: int) -> None:
        """`move.l Asrc,Adst` (MOVEA) -- both source and dest addressing
        mode 001 (An direct), e.g. `move.l a4,a6` to swap in a saved
        library base as the target of the next `jsr disp16(a6)`, matching
        real AmigaOS's calling convention (a library function may itself
        depend on A6 holding its own base for an internal call)."""
        self.word(0x2000 | (dst_an << 9) | 0x48 | src_an)

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

    def move_b_d_to_postinc(self, an: int, dn: int) -> None:
        """`move.b Dn,(An)+`."""
        self.word(0x1000 | (an << 9) | 0xC0 | dn)

    def move_b_imm_to_postinc(self, an: int, imm: int) -> None:
        """`move.b #imm,(An)+`. Byte immediates still occupy a full
        extension word (data in the low byte)."""
        self.word(0x10FC | (an << 9))
        self.word(imm & 0xFF)

    # --- raw data emission (single-hunk fixtures: fixtures/testlib.s's
    # struct Resident/AUTOINIT table/vector table/name strings all live
    # in the CODE hunk itself, with self-targeting HUNK_RELOC32 fixups --
    # see the module's "single-hunk fixtures" section below) ---

    def dc_w(self, value: int) -> int:
        """Raw `dc.w value` -- a bare word, no fixup."""
        return self.word(value)

    def dc_l_imm(self, value: int) -> None:
        """Raw `dc.l value` -- a bare longword constant (e.g. a vector
        table's `-1` terminator), no relocation."""
        self.word((value >> 16) & 0xFFFF)
        self.word(value & 0xFFFF)

    def dc_l_selfptr(self, label: str, addend: int = 0) -> None:
        """Raw `dc.l label` -- an absolute pointer to a label *in this
        same hunk* (unlike `move_l_label_to_d/_a`, which point into a
        separate DATA hunk). A real vasm-built single-CODE-hunk library
        emits exactly this shape: a `HUNK_RELOC32` group whose target
        hunk is the hunk currently being assembled (see
        `fixtures/testlib.s`'s header comment). Resolved by
        `resolve_self`."""
        idx = self.word(0)
        self.word(0)
        self._self_abs32_fixups.append((idx, label, addend))

    def dc_bytes(self, data: bytes) -> None:
        """Raw byte data (e.g. a NUL-terminated library name string),
        packed two-per-word; pads with one zero byte if `data` has odd
        length (the `even` directive's effect in the `.s` source) so
        word-granular label offsets stay exact."""
        padded = data if len(data) % 2 == 0 else data + b"\0"
        for i in range(0, len(padded), 2):
            self.word((padded[i] << 8) | padded[i + 1])

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

    # --- added for fixtures/gen_testlib.py / fixtures/gen_libcall.py
    # (Phase L3): register-list move, displaced-EA moves, displaced ADDQ,
    # CMPI, byte CLR to (An), and a register-direct ADD. Each derived
    # directly from the M68000 PRM's encoding tables, same style/rigor as
    # every helper above.

    @staticmethod
    def _movem_mask(regs: list[str], predecrement: bool) -> int:
        """Builds a MOVEM register-list mask from names like "d0"/"a6".
        Normal (postincrement/control) addressing: bit0=D0..bit7=D7,
        bit8=A0..bit15=A7. Predecrement addressing reverses this: the
        register closest to the pointer goes in the low bits, so
        bit0=A7..bit7=A0, bit8=D7..bit15=D0 (M68000 PRM, MOVEM)."""
        mask = 0
        for r in regs:
            kind = r[0]
            n = int(r[1])
            assert kind in ("d", "a") and 0 <= n <= 7, f"bad register {r!r}"
            if predecrement:
                bit = (7 - n) if kind == "a" else (15 - n)
            else:
                bit = n if kind == "d" else (8 + n)
            mask |= 1 << bit
        return mask

    def movem_l_to_predec(self, an: int, regs: list[str]) -> None:
        """`movem.l <reglist>,-(An)`."""
        self.word(0x48E0 | an)
        self.word(self._movem_mask(regs, predecrement=True))

    def movem_l_from_postinc(self, an: int, regs: list[str]) -> None:
        """`movem.l (An)+,<reglist>`."""
        self.word(0x4CD8 | an)
        self.word(self._movem_mask(regs, predecrement=False))

    def move_w_imm_to_disp_a(self, an: int, disp: int, imm: int) -> None:
        """`move.w #imm,<disp16>(An)`. Extension word order is
        source-then-destination: the immediate word first, then the
        destination displacement word."""
        self.word(0x3000 | (an << 9) | 0x17C)
        self.word(imm & 0xFFFF)
        self.word(disp & 0xFFFF)

    def move_l_a_to_disp_a(self, dst_an: int, disp: int, src_an: int) -> None:
        """`move.l An,<disp16>(Am)` -- src=An direct, dest=d16(Am)."""
        self.word(0x2000 | (dst_an << 9) | 0x148 | src_an)
        self.word(disp & 0xFFFF)

    def move_l_d_to_disp_a(self, dst_an: int, disp: int, src_dn: int) -> None:
        """`move.l Dn,<disp16>(Am)` -- src=Dn direct, dest=d16(Am)."""
        self.word(0x2000 | (dst_an << 9) | 0x140 | src_dn)
        self.word(disp & 0xFFFF)

    def move_w_disp_a_to_d(self, an: int, disp: int, dn: int) -> None:
        """`move.w <disp16>(An),Dn` -- src=d16(An), dest=Dn direct."""
        self.word(0x3028 | (dn << 9) | an)
        self.word(disp & 0xFFFF)

    def addq_w_disp_a(self, an: int, disp: int, imm: int) -> None:
        """`addq.w #imm,<disp16>(An)` (`1 <= imm <= 8`, encoded as `imm %
        8`; dest addressing mode d16(An) = 101)."""
        assert 1 <= imm <= 8
        q = imm % 8
        self.word(0x5068 | (q << 9) | an)  # size=word(01)<<6=0x40, mode=d16(An)(101)<<3=0x28
        self.word(disp & 0xFFFF)

    def cmpi_b_imm_to_d(self, dn: int, imm: int) -> None:
        """`cmpi.b #imm,Dn`."""
        self.word(0x0C00 | dn)
        self.word(imm & 0xFF)

    def cmpi_l_imm_to_d(self, dn: int, imm: int) -> None:
        """`cmpi.l #imm,Dn` (long immediate: two extension words, hi then
        lo)."""
        self.word(0x0C80 | dn)
        self.word((imm >> 16) & 0xFFFF)
        self.word(imm & 0xFFFF)

    def clr_b_ind(self, an: int) -> None:
        """`clr.b (An)`."""
        self.word(0x4210 | an)

    def add_l_d_to_d(self, dst: int, src: int) -> None:
        """`add.l Dsrc,Ddst` -- `Ddst = Ddst + Dsrc` (ADD, long, dest=Dn
        direct, opmode 010 = "Dn + <ea> -> Dn", src=Dn direct addressing
        mode 000)."""
        self.word(0xD080 | (dst << 9) | src)

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

    def resolve_self(self) -> list[int]:
        """Like `resolve`, but for a single-hunk build with no DATA hunk
        (`fixtures/testlib.s`/`fixtures/testlib_initfail.s`'s shape):
        resolves `dc_l_selfptr` fixups against this same builder's own
        `labels` and branch fixups exactly as `resolve` does. Returns the
        list of byte offsets (within this hunk) needing a
        self-targeting `HUNK_RELOC32` entry."""
        relocs: list[int] = []
        for word_idx, label, addend in self._self_abs32_fixups:
            target = self.labels[label] * 2 + addend
            self.words[word_idx] = (target >> 16) & 0xFFFF
            self.words[word_idx + 1] = target & 0xFFFF
            relocs.append(word_idx * 2)
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


def build_single_hunk_executable(code: CodeBuilder) -> bytes:
    """Emits a *single*-CODE-hunk executable from `code` alone -- no DATA
    hunk, with self-targeting `HUNK_RELOC32` fixups only (via
    `CodeBuilder.resolve_self`). This is the real on-disk shape of a
    hand-authored `RTF_AUTOINIT` library like `fixtures/testlib.s`: the
    `struct Resident`/AUTOINIT table/vector table/name strings all live in
    the same hunk as the code, and every absolute pointer among them
    relocates against that same hunk (see `fixtures/testlib.s`'s header
    comment; a real vasm build of a one-hunk `.s` file emits exactly this
    shape too)."""
    if len(code.words) % 2 != 0:
        code.word(0x4E71)
    relocs = code.resolve_self()
    code_bytes = code.to_bytes()
    assert len(code_bytes) % 4 == 0, "code hunk must be a whole number of longwords"

    code_longwords = len(code_bytes) // 4

    out = bytearray()
    out += u32(HUNK_HEADER)
    out += u32(0)  # no resident library names
    out += u32(1)  # table_size: 1 hunk
    out += u32(0)  # first_hunk
    out += u32(0)  # last_hunk
    out += u32(code_longwords)

    out += u32(HUNK_CODE)
    out += u32(code_longwords)
    out += code_bytes
    if relocs:
        out += u32(HUNK_RELOC32)
        out += u32(len(relocs))
        out += u32(0)  # target hunk: this same hunk (self-relocated)
        for offset in relocs:
            out += u32(offset)
        out += u32(0)  # terminate RELOC32 groups
    out += u32(HUNK_END)

    return bytes(out)
