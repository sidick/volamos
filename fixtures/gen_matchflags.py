#!/usr/bin/env python3
"""Generates fixtures/matchflags: the issue #58 fixture distinguishing
whether APF_DirChanged (bit 6, value 64) is set once per real directory
transition during a MatchFirst/MatchNext scan (correct) or on every
single entry (too eager) -- a question a *flat* single-directory scan
can't answer, since it has no directory transitions to distinguish by.
See fixtures/matchflags.s for the equivalent assembly (assembled for
real with PhxAss under volamos -- see fixtures/README.md's
"matchflags" section for that exact command) and its own header comment
for the full program flow; this script (using fixtures/amiga_asm.py's
tiny two-pass assembler) is the authoritative, byte-identical,
toolchain-free build, since PhxAss isn't checked into this repo (Aminet
freeware living outside it) and can't be relied on in CI.

Program flow (real startup, same convention as every fixture since
filetest.s/dirtest.s/echoargs.s/memtest.s):

1. A2 = the incoming command-line pointer (A0), copied *before* any
   library call (A0 is scratch across a jsr).
2. A4 = AbsExecBase; A6 = A4; OpenLibrary("dos.library", 0) via
   -552(a6) (unchecked); A3 = the returned dos.library base, kept.
3. Copy the leading command-line token (up to, not including, the first
   space/newline/NUL) into `dirarg`. An empty token (no argument at
   all) prints a usage line and exits 0.
4. AllocMem(536, MEMF_CLEAR) via A6 = ExecBase -- 536 = 280 (the fixed
   AnchorPath header, up to and including ap_Info) + 256 (an ap_Buf
   tail). NULL result -> "AllocMem failed" + exit 20.
5. ap_Strlen (word at +18) = 256 (so ap_Buf gets filled in);
   ap_Flags (byte at +16) = APF_DODIR (4), so the scan descends into
   subdirectories -- the whole point: a multi-level tree, not a flat
   one.
6. MatchFirst(dirarg, ap) (-822). Nonzero D0 -> "MatchFirst failed" +
   exit 10.
7. Loop: print_entry (below), then MatchNext (-828) until it returns
   nonzero. MatchEnd (-834), FreeMem the AnchorPath, exit 0.

print_entry prints "flags=%ld name='%s' buf='%s'\\n" via VPrintf (-954,
D1=format, D2=argarray) for the AnchorPath's *current* entry:
ap_Flags (a BYTE at +16, explicitly zero-extended -- not read as a
longword), fib_FileName (a NUL-terminated C string at
ap_Info+8 == AnchorPath+28), and ap_Buf (AnchorPath+280). The three
values are written into a 3-longword `argarray` scratch buffer in the
data hunk (flags, &fib_FileName, &ap_Buf) via two new `lea`-computed
address-register pointers (`CodeBuilder.lea_disp_a_to_a`, added for
this fixture) before the VPrintf call -- both fib_FileName and ap_Buf
live at a fixed displacement off the AllocMem'd (so only known at
runtime) AnchorPath base, so their addresses can't be baked in as
DATA-hunk-relocated immediates the way every earlier fixture's string
pointers are.

Run directly to (re)write fixtures/matchflags:

    python3 fixtures/gen_matchflags.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "matchflags"

# LVOs used (see crates/volamos-core/src/lvos/dos.rs / execmem.rs):
LVO_OPENLIBRARY = -552
LVO_ALLOCMEM = -198
LVO_FREEMEM = -210
LVO_PUTSTR = -948
LVO_VPRINTF = -954
LVO_MATCHFIRST = -822
LVO_MATCHNEXT = -828
LVO_MATCHEND = -834

APF_DODIR = 4
MEMF_CLEAR = 0x10000
AP_SIZE = 536  # 280 (fixed AnchorPath header, through ap_Info) + 256 (ap_Buf tail)
AP_FLAGS_OFF = 16
AP_STRLEN_OFF = 18
AP_BUF_STRLEN = 256
FIB_FILENAME_OFF = 28  # ap_Info(+20) + fib_FileName(+8)
AP_BUF_OFF = 280
DIRARG_SIZE = 256

ALLOCFAIL_EXIT_CODE = 20
MATCHFIRSTFAIL_EXIT_CODE = 10

# Registers (see crates/volamos-core/src/cpu.rs's DataRegister/
# AddressRegister numbering, matching real 68k D0-D7/A0-A7).
D0, D1, D2 = 0, 1, 2
A0, A1, A2, A3, A4, A5, A6 = 0, 1, 2, 3, 4, 5, 6


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.cstr("fmt", "flags=%ld name='%s' buf='%s'\n")
    data.cstr("msg_usage", "usage: matchflags <dir>\n")
    data.cstr("msg_allocfail", "AllocMem failed\n")
    data.cstr("msg_matchfirstfail", "MatchFirst failed\n")
    # align4() here matters, unlike every earlier fixture's DataBuilder use:
    # argarray is the first DATA-hunk label any fixture takes a raw address
    # of and then writes with a real CPU move.l -- a real 68000 faults
    # (Address Error) on a long-word access at an odd address. DataBuilder's
    # cstr() doesn't auto-pad to even (unlike a real assembler's "even"
    # directive, which matchflags.s already uses after every dc.b -- see
    # its header comment), so without this, an odd-length preceding string
    # (e.g. adding/editing any msg_* text above) could silently misalign
    # dirarg/argarray and corrupt guest memory on the very first long-word
    # write to argarray. Found the hard way: this exact bug reproduced with
    # a single extra 3-byte cstr inserted before dirarg, crashing the CPU
    # inside print_entry's first move.l to argarray -- verified with
    # --sanitize and a battery of bisection micro-fixtures before finding
    # the true cause (fixtures/matchflags.s's own PhxAss build was never
    # affected, since PhxAss's "even" directives keep it aligned already).
    data.align4()
    data.zeros("dirarg", DIRARG_SIZE)
    data.zeros("argarray", 12)  # 3 longwords: flags, &fib_FileName, &ap_Buf

    code = CodeBuilder()

    code.label("start")
    code.move_l_a_to_a(A2, A0)  # A2 = saved command-line pointer

    code.move_l_abs4_to_a(A4)  # A4 = AbsExecBase = EXEC_LIBRARY_BASE
    code.move_l_a_to_a(A6, A4)
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)  # OpenLibrary("dos.library") -- unchecked
    code.move_l_d_to_a(A3, D0)  # A3 = dos.library base (kept)

    # --- copy the leading command-line token into dirarg, stopping at
    # the first space/newline/NUL (not copied) ---
    code.move_l_a_to_a(A1, A2)
    code.move_l_label_to_a(A0, "dirarg")
    code.label("copyloop")
    code.move_b_postinc_to_d(D0, A1)
    code.cmpi_b_imm_to_d(D0, 0)
    code.branch(CodeBuilder.BEQ, "copydone")
    code.cmpi_b_imm_to_d(D0, 10)
    code.branch(CodeBuilder.BEQ, "copydone")
    code.cmpi_b_imm_to_d(D0, 32)
    code.branch(CodeBuilder.BEQ, "copydone")
    code.move_b_d_to_postinc(A0, D0)
    code.branch(CodeBuilder.BRA, "copyloop")
    code.label("copydone")
    code.move_b_imm_to_postinc(A0, 0)  # NUL-terminate dirarg

    # empty argument (nothing copied) -> usage
    code.move_l_label_to_a(A1, "dirarg")
    code.move_b_disp_a_to_d(A1, 0, D0)
    code.cmpi_b_imm_to_d(D0, 0)
    code.branch(CodeBuilder.BEQ, "usage")

    # --- AllocMem(AP_SIZE, MEMF_CLEAR) ---
    code.move_l_a_to_a(A6, A4)
    code.move_l_imm_to_d(D0, AP_SIZE)
    code.move_l_imm_to_d(D1, MEMF_CLEAR)
    code.jsr_disp16_a(A6, LVO_ALLOCMEM)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "allocfail")
    code.move_l_d_to_a(A5, D0)  # A5 = AnchorPath address (kept)

    code.move_w_imm_to_disp_a(A5, AP_STRLEN_OFF, AP_BUF_STRLEN)  # ap_Strlen = 256
    code.move_b_imm_to_disp_a(A5, AP_FLAGS_OFF, APF_DODIR)  # ap_Flags = APF_DODIR

    # --- MatchFirst(dirarg, ap) ---
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "dirarg")
    code.move_l_a_to_d(D2, A5)
    code.jsr_disp16_a(A6, LVO_MATCHFIRST)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BNE, "matchfirstfail")

    code.label("matchloop")
    code.branch(CodeBuilder.BSR, "print_entry")

    # Re-set APF_DODIR before every MatchNext: per crates/volamos-core/
    # src/dosanchor.rs's module docs, a directory descent only happens
    # when "the previous return was a directory AND the caller has SINCE
    # set APF_DODIR" -- MatchNext clears the bit itself once consumed for
    # a descent, so a caller wanting to descend into *every* directory
    # level (the whole point of this fixture -- a flat single-directory
    # scan can't distinguish per-entry from per-transition APF_DirChanged)
    # must re-request it before every call, exactly like the NDK's own
    # ScanDirectories() worked example. Overwriting the whole byte (rather
    # than a read-modify-write OR) is fine here: ap_Flags was already read
    # and printed for this entry by print_entry just above, so there's no
    # other bit left to preserve before the next call recomputes it.
    code.move_b_imm_to_disp_a(A5, AP_FLAGS_OFF, APF_DODIR)

    code.move_l_a_to_d(D1, A5)
    code.jsr_disp16_a(A6, LVO_MATCHNEXT)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "matchloop")  # D0 == 0: another entry, print it too

    # D0 != 0: no more entries (or an error) -- either way, done
    code.move_l_a_to_d(D1, A5)
    code.jsr_disp16_a(A6, LVO_MATCHEND)

    code.move_l_a_to_a(A6, A4)
    code.move_l_a_to_a(A1, A5)
    code.move_l_imm_to_d(D0, AP_SIZE)
    code.jsr_disp16_a(A6, LVO_FREEMEM)  # FreeMem(ap, AP_SIZE)
    code.moveq(D0, 0)
    code.rts()

    # --- print_entry: prints "flags=%ld name='%s' buf='%s'\n" for the
    # entry currently in *A5. Builds a 3-longword VPrintf argarray in
    # the data hunk. ---
    code.label("print_entry")
    code.moveq(D0, 0)
    code.move_b_disp_a_to_d(A5, AP_FLAGS_OFF, D0)  # D0 = ap_Flags, zero-extended
    code.move_l_label_to_a(A1, "argarray")
    code.move_l_d_to_disp_a(A1, 0, D0)  # argarray[0] = flags

    code.lea_disp_a_to_a(A0, FIB_FILENAME_OFF, A5)  # A0 = &fib_FileName
    code.move_l_a_to_disp_a(A1, 4, A0)  # argarray[1] = fib_FileName ptr

    code.lea_disp_a_to_a(A0, AP_BUF_OFF, A5)  # A0 = &ap_Buf
    code.move_l_a_to_disp_a(A1, 8, A0)  # argarray[2] = ap_Buf ptr

    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "fmt")
    code.move_l_a_to_d(D2, A1)
    code.jsr_disp16_a(A6, LVO_VPRINTF)  # VPrintf(fmt, argarray)
    code.rts()

    code.label("usage")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_usage")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, 0)
    code.rts()

    code.label("allocfail")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_allocfail")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, ALLOCFAIL_EXIT_CODE)
    code.rts()

    code.label("matchfirstfail")
    code.move_l_a_to_a(A6, A3)
    code.move_l_label_to_d(D1, "msg_matchfirstfail")
    code.jsr_disp16_a(A6, LVO_PUTSTR)
    code.moveq(D0, MATCHFIRSTFAIL_EXIT_CODE)
    code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
