#!/usr/bin/env python3
"""Generates fixtures/exectest: the Phase 3 stage 8 fixture proving, via
real hunk-loaded execution through the actual `volamos` CLI binary, that
the Phase 3 handlers that so far only had in-crate unit tests actually
work end to end: `exec.library`'s `AllocMem`/`FreeMem`/`AllocVec`/
`FreeVec` (execmem.rs), `utility.library` opened for real via
`OpenLibrary` and its `Stricmp`/`GetTagData`/`Strnicmp` (utility.rs), and
`exec.library`'s `FindTask`/`SetSignal` plus `dos.library`'s
`CheckSignal` (exectask.rs). See fixtures/exectest.s for the equivalent
assembly and fixtures/README.md for the full description; this script
(using fixtures/amiga_asm.py's tiny two-pass assembler) is the
authoritative, byte-identical, toolchain-free build since vasm isn't
available on the machine this was authored on.

Program flow (real startup, same convention as filetest.s/dirtest.s/
echoargs.s/systest.s -- see those for the full explanation of the
OpenLibrary("dos.library") sequence):

1. A6 = *4 (AbsExecBase = EXEC_LIBRARY_BASE); OpenLibrary("dos.library",
   0) via -552(a6) (unchecked, matching every earlier fixture's own
   convention); A3 = the returned dos.library base -- kept in A3 rather
   than clobbering A6, since this fixture, unlike the earlier ones, keeps
   making *exec.library* calls afterward and needs A6 free for those (the
   trap dispatcher resolves purely from where a `jsr` physically lands,
   not from any "current A6" notion the runtime tracks -- any address
   register can hold any library base at any time; see
   crates/volamos-core/src/dispatch.rs's module docs).

2. (a) exec.library via A6 = EXEC_LIBRARY_BASE:
   - AllocMem(64, MEMF_CLEAR) -> D0; fail (exit 1) if NULL. Read the
     first byte back (should be 0, proving MEMF_CLEAR); fail (exit 2) if
     not. Write a one-byte pattern past it, then FreeMem the original
     64-byte block.
   - AllocVec(20, 0) -> D0; fail (exit 3) if NULL. FreeVec it.

3. (b) utility.library, opened for real:
   - OpenLibrary("utility.library", 0) via A6 = EXEC_LIBRARY_BASE -> D0;
     fail (exit 4) if NULL. A4 = the returned base (which this runtime
     always resolves to the fixed UTILITY_LIBRARY_BASE, since
     utility.library is registered as a real library at Runtime::new
     time -- see dispatch.rs -- never the auto-created-fake-library
     path).
   - Stricmp("AMIGA", "amiga") via A4 -> D0; expect 0 (case-insensitive
     equal); fail (exit 5) otherwise.
   - GetTagData(TAG_VAL, 0xDEAD, taglist) via A4, where `taglist` (built
     directly in this fixture's DATA hunk via amiga_asm.py's new
     `DataBuilder.u32s`) is `{TAG_VAL, 7}, {TAG_DONE, 0}` -- expect the
     planted value 7 back (not the 0xDEAD default, which would mean the
     lookup failed); fail (exit 6) otherwise.
   - Strnicmp("HELLO1", "HELLO2", 6) via A4 -> D0; expect nonzero (the
     two differ within the compared length); fail (exit 7) if it
     (wrongly) reports equal.

4. (c) exec.library task/signal, plus dos.library's CheckSignal:
   - FindTask(NULL) via A6 -> D0; fail (exit 8) if NULL.
   - SetSignal(0, 0) via A6 -- a pure read of the current tc_SigRecvd
     (0 requested/0 mask changes nothing), exercised but not checked.
   - SetSignal(1<<5, 1<<5) via A6 -- sets bit 5.
   - dos.library's CheckSignal(1<<5) via A3 -> D0; expect exactly 1<<5
     (the bit just set) back; fail (exit 9) otherwise.

5. On full success: PutStr("exec ok\n") via A3 (dos.library) and exit 0.

Every failure path PutStrs a single, fixed "ERR\n" marker (the
filetest.s convention: a fixed marker plus a distinct nonzero exit code
per failed step, rather than decoding the failure into printed text) and
exits with that step's distinctive code (1-9, see above).

Run directly to (re)write fixtures/exectest:

    python3 fixtures/gen_exectest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "exectest"

# LVOs used (see crates/volamos-core/src/lvos/{dos,exec,utility}.rs):
LVO_OPENLIBRARY = -552
LVO_ALLOCMEM = -198
LVO_FREEMEM = -210
LVO_ALLOCVEC = -684
LVO_FREEVEC = -690
LVO_STRICMP = -162
LVO_GETTAGDATA = -36
LVO_STRNICMP = -168
LVO_FINDTASK = -294
LVO_SETSIGNAL = -306
LVO_CHECKSIGNAL = -792
LVO_PUTSTR = -948

MEMF_CLEAR = 1 << 16

# Registers (see crates/volamos-core/src/cpu.rs's DataRegister/
# AddressRegister numbering, matching real 68k D0-D7/A0-A7).
D0, D1, D2, D3 = 0, 1, 2, 3
A0, A1, A3, A4, A6 = 0, 1, 3, 4, 6

TAG_VAL = 100  # arbitrary tag value, well above the TAG_DONE/IGNORE/
# MORE/SKIP (0-3) control-tag range so it can never collide with one.
TAG_PLANTED_VALUE = 7  # small enough for CodeBuilder.subq_l_imm_d (1-8)


def build_program() -> bytes:
    data = DataBuilder()
    data.cstr("dosname", "dos.library")
    data.cstr("utilname", "utility.library")
    data.cstr("str_upper", "AMIGA")
    data.cstr("str_lower", "amiga")
    data.cstr("str_a", "HELLO1")
    data.cstr("str_b", "HELLO2")
    data.u32s("taglist", [TAG_VAL, TAG_PLANTED_VALUE, 0, 0])  # {TAG_VAL,7}, {TAG_DONE,0}
    data.cstr("errmsg", "ERR\n")
    data.cstr("okmsg", "exec ok\n")

    code = CodeBuilder()

    code.label("start")
    code.move_l_abs4_to_a(A6)  # A6 = AbsExecBase = EXEC_LIBRARY_BASE
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)  # OpenLibrary("dos.library") -- unchecked
    code.move_l_d_to_a(A3, D0)  # A3 = dos.library base (kept, not A6)

    # --- (a) exec.library: AllocMem/FreeMem, AllocVec/FreeVec ---
    code.moveq(D0, 64)
    code.move_l_imm_to_d(D1, MEMF_CLEAR)
    code.jsr_disp16_a(A6, LVO_ALLOCMEM)  # AllocMem(64, MEMF_CLEAR) -> D0
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "err1")

    code.move_l_d_to_a(A0, D0)  # A0 = block addr
    code.move_l_a_to_d(D3, A0)  # D3 = block addr (saved for FreeMem)
    code.moveq(D1, 0)
    code.move_b_postinc_to_d(D1, A0)  # D1 = first byte (MEMF_CLEAR: should be 0)
    code.tst_l_d(D1)
    code.branch(CodeBuilder.BNE, "err2")

    code.move_b_imm_to_postinc(A0, 0xAB)  # write a byte pattern
    code.move_l_d_to_a(A1, D3)  # A1 = block addr (restored)
    code.moveq(D0, 64)
    code.jsr_disp16_a(A6, LVO_FREEMEM)  # FreeMem(block, 64)

    code.moveq(D0, 20)
    code.moveq(D1, 0)
    code.jsr_disp16_a(A6, LVO_ALLOCVEC)  # AllocVec(20, 0) -> D0
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "err3")
    code.move_l_d_to_a(A1, D0)
    code.jsr_disp16_a(A6, LVO_FREEVEC)  # FreeVec

    # --- (b) utility.library, opened for real ---
    code.move_l_label_to_a(A1, "utilname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)  # OpenLibrary("utility.library") -> D0
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "err4")
    code.move_l_d_to_a(A4, D0)  # A4 = utility.library base

    code.move_l_label_to_a(A0, "str_upper")
    code.move_l_label_to_a(A1, "str_lower")
    code.jsr_disp16_a(A4, LVO_STRICMP)  # Stricmp("AMIGA","amiga") -> D0 (expect 0)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BNE, "err5")

    code.move_l_label_to_a(A0, "taglist")
    code.moveq(D0, TAG_VAL)
    code.move_l_imm_to_d(D1, 0xDEAD)  # default -- shouldn't be returned
    code.jsr_disp16_a(A4, LVO_GETTAGDATA)  # GetTagData -> D0 (expect 7)
    code.subq_l_imm_d(D0, TAG_PLANTED_VALUE)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BNE, "err6")

    code.move_l_label_to_a(A0, "str_a")
    code.move_l_label_to_a(A1, "str_b")
    code.moveq(D0, 6)
    code.jsr_disp16_a(A4, LVO_STRNICMP)  # Strnicmp("HELLO1","HELLO2",6) -> D0 (expect != 0)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "err7")

    # --- (c) exec.library task/signal + dos.library CheckSignal ---
    code.move_l_imm_to_a(A1, 0)
    code.jsr_disp16_a(A6, LVO_FINDTASK)  # FindTask(NULL) -> D0
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "err8")

    code.moveq(D0, 0)
    code.moveq(D1, 0)
    code.jsr_disp16_a(A6, LVO_SETSIGNAL)  # SetSignal(0,0) -- read, unchecked

    code.moveq(D0, 32)
    code.moveq(D1, 32)
    code.jsr_disp16_a(A6, LVO_SETSIGNAL)  # SetSignal(1<<5,1<<5) -- sets bit 5

    code.moveq(D1, 32)
    code.jsr_disp16_a(A3, LVO_CHECKSIGNAL)  # dos CheckSignal(1<<5) -> D0
    code.move_l_d_to_d(D2, D0)
    code.moveq(D1, 32)
    code.sub_l_d_from_d(D2, D1)  # D2 = D0 - 32
    code.tst_l_d(D2)
    code.branch(CodeBuilder.BNE, "err9")

    # --- (d) success ---
    code.move_l_label_to_d(D1, "okmsg")
    code.jsr_disp16_a(A3, LVO_PUTSTR)  # PutStr("exec ok\n") via dos.library
    code.moveq(D0, 0)
    code.rts()

    for exit_code in range(1, 10):
        code.label(f"err{exit_code}")
        code.move_l_label_to_d(D1, "errmsg")
        code.jsr_disp16_a(A3, LVO_PUTSTR)  # PutStr("ERR\n")
        code.moveq(D0, exit_code)
        code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
