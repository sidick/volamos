#!/usr/bin/env python3
"""Generates fixtures/memtest: the issue #65 fixture for volamos's
`--sanitize` heap-redzone/free-quarantine memory sanitizer. See
fixtures/memtest.s for the equivalent assembly (assembled for real with
PhxAss under volamos -- see fixtures/README.md's "memtest" section for
that exact command) and its own header comment for the full program
flow; this script (using fixtures/amiga_asm.py's tiny two-pass
assembler) is the authoritative, byte-identical, toolchain-free build,
since PhxAss isn't checked into this repo (it's Aminet freeware living
outside it) and can't be relied on in CI.

Program flow (real startup, same convention as every fixture since
filetest.s/dirtest.s/echoargs.s):

1. A2 = the incoming command-line pointer (A0), copied *before* any
   library call -- A0 is a caller-clobbered ("scratch") register per the
   RKRM calling convention, not guaranteed to survive one (see
   gen_libcall.py's comment for the same rule, and echoargs.s's header
   comment for the trailing-space-before-newline convention real
   AmigaOS's command-line buffer follows).
2. A4 = AbsExecBase (*4); A6 = A4; OpenLibrary("dos.library", 0) via
   -552(a6) (unchecked, matching every earlier fixture); A3 = the
   returned dos.library base, kept in A3 as *storage* -- this fixture
   interleaves exec.library calls (AllocMem/FreeMem, needing A6 =
   ExecBase) with dos.library's PutStr (needing A6 = dos.library's own
   base), so A6 is always swapped to the right base immediately before
   every `jsr`, never left pointing at the wrong library across a call
   (the exectest.s/issue #6 lesson).
3. Command-line dispatch: a small `strmatch` subroutine (A1 = the
   command-line cursor, A0 = the candidate keyword's data label,
   D0/D1/D2/D3 scratch; returns D0=1/0) is `bsr`'d once per candidate
   keyword ("clean", "overrun", "underrun", "uaf"), each time re-copying
   A2 into A1 first (the subroutine consumes A1 via postincrement).
   `strmatch` requires the keyword to be followed immediately by a space
   or newline, so "clean" doesn't spuriously match a hypothetical
   "cleanup" -- it stops comparing once the *candidate* string's NUL is
   reached, then checks that the next actual command-line byte is one of
   those two terminators.
4. Whichever mode matches jumps to that mode's body -- see the module's
   MODES list below and fixtures/README.md's "memtest" section for what
   each one does and what --sanitize is expected to report for it. No
   match (including an empty command line, which is just "\\n") falls
   through to `mode_usage`, which PutStrs a usage line and exits 0.
5. Every mode body reuses A2 (no longer needed once dispatch has
   committed) as the AllocMem'd block pointer. AllocMem's exec.library
   LVO is -198 (D0 = byte size, D1 = requirements, D0 returns the
   address or 0 on failure); FreeMem's is -210 (A1 = block, D0 = byte
   size -- and per crates/volamos-core/src/execmem.rs's module docs,
   FreeMem errors out loudly if that size doesn't match what was
   actually allocated, both rounded up to 8, so every mode here passes
   FreeMem the exact same literal 32 it passed AllocMem). A NULL
   AllocMem result (exit code 20, arbitrary and distinct from every
   mode's own exit 0) is handled by every mode identically via the
   shared `allocfail` label, rather than dereferencing NULL.

Run directly to (re)write fixtures/memtest:

    python3 fixtures/gen_memtest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "memtest"

# LVOs used (see crates/volamos-core/src/lvos/{dos,exec}.rs):
LVO_OPENLIBRARY = -552
LVO_ALLOCMEM = -198
LVO_FREEMEM = -210
LVO_PUTSTR = -948

BLOCK_SIZE = 32

# Registers (see crates/volamos-core/src/cpu.rs's DataRegister/
# AddressRegister numbering, matching real 68k D0-D7/A0-A7).
D0, D1, D2, D3 = 0, 1, 2, 3
A0, A1, A2, A3, A4, A6 = 0, 1, 2, 3, 4, 6

ALLOCFAIL_EXIT_CODE = 20


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")

    data.cstr("kw_clean", "clean")
    data.cstr("kw_overrun", "overrun")
    data.cstr("kw_underrun", "underrun")
    data.cstr("kw_uaf", "uaf")

    data.cstr("msg_clean", "clean: alloc 32 bytes, write+read all 32, free\n")
    data.cstr("msg_overrun", "overrun: writing 1 byte past a 32-byte block\n")
    data.cstr("msg_underrun", "underrun: reading 1 byte before a 32-byte block\n")
    data.cstr("msg_uaf", "uaf: reading a freed 32-byte block\n")
    data.cstr("msg_usage", "usage: memtest clean|overrun|underrun|uaf\n")
    data.cstr("msg_allocfail", "AllocMem failed\n")

    code = CodeBuilder()

    code.label("start")
    code.move_l_a_to_a(A2, A0)  # A2 = saved command-line pointer

    code.move_l_abs4_to_a(A4)  # A4 = AbsExecBase = EXEC_LIBRARY_BASE (kept)
    code.move_l_a_to_a(A6, A4)
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)  # OpenLibrary("dos.library") -- unchecked
    code.move_l_d_to_a(A3, D0)  # A3 = dos.library base (kept)

    # --- dispatch: try each keyword in turn ---
    for keyword, mode_label in (
        ("kw_clean", "mode_clean"),
        ("kw_overrun", "mode_overrun"),
        ("kw_underrun", "mode_underrun"),
        ("kw_uaf", "mode_uaf"),
    ):
        code.move_l_a_to_a(A1, A2)  # fresh cursor -- strmatch consumes it
        code.move_l_label_to_a(A0, keyword)
        code.branch(CodeBuilder.BSR, "strmatch")
        code.tst_l_d(D0)
        code.branch(CodeBuilder.BNE, mode_label)
    code.branch(CodeBuilder.BRA, "mode_usage")

    # --- strmatch(A1=cmdline cursor, A0=candidate keyword) -> D0 (1/0) ---
    # Compares bytes from A1 against A0 until A0's NUL is reached (a
    # match of the whole candidate keyword so far), then checks that the
    # very next command-line byte is a space or a newline -- the
    # keyword's real terminator, matching echoargs.s's documented
    # trailing-space-before-newline convention for a non-empty guest
    # command line, and the bare "\n" convention for an empty one.
    code.label("strmatch")
    # D1/D2 must start at exactly 0: move_b_postinc_to_d only ever writes
    # the *low* byte, so whatever garbage sits in their high 24 bits
    # (left over from earlier code -- D2 in particular still held 0x2000
    # from AllocMem's MEMF_CLEAR-requirements setup on the very first
    # dispatch call) would otherwise make the full-32-bit `tst_l_d`/
    # `sub_l_d_from_d` checks below see a false nonzero even when the two
    # bytes being compared are equal, or fail to recognise the candidate
    # keyword's NUL terminator. Cleared once, here, before the loop: the
    # loop body never touches D1/D2's upper 24 bits again, so they stay
    # zero across every iteration.
    code.moveq(D1, 0)
    code.moveq(D2, 0)
    code.label("strmatch_loop")
    code.move_b_postinc_to_d(D2, A0)  # D2 = next candidate-keyword byte
    code.tst_l_d(D2)
    code.branch(CodeBuilder.BEQ, "strmatch_end")  # candidate exhausted -> matched so far
    code.move_b_postinc_to_d(D1, A1)  # D1 = next actual command-line byte
    code.move_l_d_to_d(D3, D1)
    code.sub_l_d_from_d(D3, D2)  # D3 = D1 - D2
    code.tst_l_d(D3)
    code.branch(CodeBuilder.BNE, "strmatch_fail")
    code.branch(CodeBuilder.BRA, "strmatch_loop")
    code.label("strmatch_end")
    code.move_b_postinc_to_d(D1, A1)  # the byte right after the matched prefix
    code.cmpi_b_imm_to_d(D1, 32)  # ' '
    code.branch(CodeBuilder.BEQ, "strmatch_ok")
    code.cmpi_b_imm_to_d(D1, 10)  # '\n'
    code.branch(CodeBuilder.BEQ, "strmatch_ok")
    code.label("strmatch_fail")
    code.moveq(D0, 0)
    code.rts()
    code.label("strmatch_ok")
    code.moveq(D0, 1)
    code.rts()

    # --- clean: alloc 32, write all 32, read all 32 back, free. Must
    # produce zero sanitizer violations -- the false-positive guard. ---
    code.label("mode_clean")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_clean")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.move_l_a_to_a(A6, A4)
    code.moveq(D0, BLOCK_SIZE)
    code.moveq(D1, 0)
    code.jsr_disp16_a(A6, LVO_ALLOCMEM)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "allocfail")
    code.move_l_d_to_a(A2, D0)  # A2 = block ptr (dispatch's own use of A2 is done)

    code.move_l_a_to_a(A1, A2)
    code.moveq(D1, BLOCK_SIZE - 1)  # dbra runs count+1 = 32 times
    code.label("clean_write_loop")
    code.move_b_d_to_postinc(A1, D1)  # in-bounds write, offsets 0..31
    code.dbra(D1, "clean_write_loop")

    code.move_l_a_to_a(A1, A2)
    code.moveq(D1, BLOCK_SIZE - 1)
    code.label("clean_read_loop")
    code.move_b_postinc_to_d(D0, A1)  # in-bounds read, offsets 0..31
    code.dbra(D1, "clean_read_loop")

    code.move_l_a_to_a(A6, A4)
    code.move_l_a_to_a(A1, A2)
    code.moveq(D0, BLOCK_SIZE)
    code.jsr_disp16_a(A6, LVO_FREEMEM)
    code.moveq(D0, 0)
    code.rts()

    # --- overrun: alloc 32, write 1 byte at offset 32 (one past the
    # end -- the trailing redzone), then free. --sanitize should report
    # a heap-buffer-overflow write at block+32. ---
    code.label("mode_overrun")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_overrun")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.move_l_a_to_a(A6, A4)
    code.moveq(D0, BLOCK_SIZE)
    code.moveq(D1, 0)
    code.jsr_disp16_a(A6, LVO_ALLOCMEM)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "allocfail")
    code.move_l_d_to_a(A2, D0)

    code.move_b_imm_to_disp_a(A2, BLOCK_SIZE, 0xAB)  # the overrun write itself

    code.move_l_a_to_a(A6, A4)
    code.move_l_a_to_a(A1, A2)
    code.moveq(D0, BLOCK_SIZE)
    code.jsr_disp16_a(A6, LVO_FREEMEM)
    code.moveq(D0, 0)
    code.rts()

    # --- underrun: alloc 32, read 1 byte at offset -1 (the leading
    # redzone), then free. --sanitize should report a heap-buffer-
    # overflow read at block-1. ---
    code.label("mode_underrun")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_underrun")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.move_l_a_to_a(A6, A4)
    code.moveq(D0, BLOCK_SIZE)
    code.moveq(D1, 0)
    code.jsr_disp16_a(A6, LVO_ALLOCMEM)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "allocfail")
    code.move_l_d_to_a(A2, D0)

    code.move_b_disp_a_to_d(A2, -1, D0)  # the underrun read itself (result discarded)

    code.move_l_a_to_a(A6, A4)
    code.move_l_a_to_a(A1, A2)
    code.moveq(D0, BLOCK_SIZE)
    code.jsr_disp16_a(A6, LVO_FREEMEM)
    code.moveq(D0, 0)
    code.rts()

    # --- uaf: alloc 32, free it, then read 1 byte at offset 0 of the
    # now-freed block. --sanitize should report a use-after-free read at
    # the freed block's start (found via the free quarantine, since the
    # bytes are never reused for anything else in this same run). ---
    code.label("mode_uaf")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_uaf")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.move_l_a_to_a(A6, A4)
    code.moveq(D0, BLOCK_SIZE)
    code.moveq(D1, 0)
    code.jsr_disp16_a(A6, LVO_ALLOCMEM)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "allocfail")
    code.move_l_d_to_a(A2, D0)

    code.move_l_a_to_a(A1, A2)
    code.moveq(D0, BLOCK_SIZE)
    code.jsr_disp16_a(A6, LVO_FREEMEM)

    code.move_b_disp_a_to_d(A2, 0, D0)  # the use-after-free read itself

    code.moveq(D0, 0)
    code.rts()

    # --- usage: no argument, or an unrecognised one ---
    code.label("mode_usage")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_usage")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, 0)
    code.rts()

    # --- shared AllocMem-returned-NULL path ---
    code.label("allocfail")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_allocfail")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, ALLOCFAIL_EXIT_CODE)
    code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
