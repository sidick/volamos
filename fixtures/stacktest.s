; stacktest.s -- issue #65 increment 2 fixture for volamos (vasm mot /
; PhxAss syntax).
;
; A deliberately buggy (and, just as importantly, deliberately *clean* in
; several ways) AmigaOS CLI program used to validate volamos's
; `--sanitize` mode's two stack detectors:
;
;   1. below-SP accesses -- reading or writing memory below the current
;      stack pointer (dead, released stack space).
;   2. return-address corruption -- a shadow call stack records the
;      return address each JSR/BSR pushes and verifies it at the
;      matching RTS.
;
; Takes a single leading command-line keyword selecting which mode to
; run, same `strmatch` dispatch convention as `memtest.s` (see that
; file's header comment for the full mechanism -- copied verbatim here,
; not re-derived).
;
;   below    -- read a byte 128 bytes below the current SP, without
;               moving SP. Expect a below-SP violation (read).
;   smash    -- `bsr` into a subroutine that overwrites its own just-
;               pushed return address in place with the address of a
;               real, valid label, then `rts`s. Expect a return-address-
;               corruption violation reported at the matching RTS --
;               *and* the process must still exit 0, since the corrupted
;               return still lands on valid code (see "the smash design
;               constraint" below).
;
;   The following three modes are false-positive guards: each MUST
;   produce ZERO violations. A sanitizer that flags correct code is
;   worse than none, so these matter more than the two detection modes
;   above.
;
;   clean    -- three levels of nested bsr/rts, each establishing a
;               LINK/UNLK stack frame and writing+reading a local
;               variable at a fixed frame-pointer-relative displacement.
;               Ordinary, textbook-correct code.
;   pushret  -- the classic m68k computed-jump idiom: `move.l
;               #target,-(sp)` followed by `rts`, transferring control to
;               `target` with **no matching JSR/BSR anywhere**. This is
;               legitimate, extremely common Amiga code (library
;               trampolines, tail-call-style dispatch) and is the single
;               most likely source of a false positive in the whole
;               increment: the shadow call stack must recognise there is
;               no call frame to match this RTS against and stay silent,
;               not treat an empty/exhausted shadow stack as corruption.
;   deep     -- bsr-recurses 64 levels deep (each level also pushes/pops
;               D0 via MOVEM, so real SP movement happens at every level)
;               and unwinds cleanly via matching RTSes all the way back
;               out. Exercises the shadow call stack's depth handling and
;               the below-SP tracker's delta bookkeeping under a lot of
;               genuine SP movement, all fully balanced.
;
; No argument, or an unrecognised one, prints a usage line (via PutStr,
; LVO -948) and exits 0. Every mode PutStr's a short self-describing line
; first, then exits 0 via a plain `rts` (this program never calls
; Exit() -- see hello.s's header comment for the exit-stub convention
; every fixture here relies on) -- the *sanitizer's* job is to notice a
; bug, not this program's own exit code.
;
; --- the "smash" design constraint ---
;
; `smash`'s corrupted return address is the address of a real code
; label (`smash_landing`), not garbage like $DEADBEEF. Overwriting a
; return address with an arbitrary/invalid address would send the CPU's
; PC into unmapped or nonsense memory and likely crash the run before it
; finished -- useless for an automated test that wants to assert BOTH "the
; process ran to completion and exited 0" AND "the sanitizer reported a
; violation for the corrupted slot". Landing on a real label keeps the
; *program* well-behaved while still corrupting the *return address slot*
; the shadow call stack is watching -- exactly the shape the detector
; needs to catch, and the only shape a fully-automated test can assert
; against without also racing a segfault.
;
; --- Calling convention ---
;
; Real AmigaOS CLI startup, same convention as every fixture since
; filetest.s/memtest.s: A0 = command-line buffer pointer at entry, copied
; into A2 (callee-preserved) before the very first jsr (OpenLibrary),
; since A0 is scratch across any library call.
;
;   A4 = ExecBase (AbsExecBase, read once at start) -- kept, though this
;        fixture makes no exec.library calls of its own (no heap
;        allocation here; that's memtest.s's job) -- read anyway to match
;        every other fixture's real-startup shape and leave A4 free for
;        future extension.
;   A3 = dos.library base (opened once) -- for PutStr calls.
;   A6 = swapped to whichever library base is about to be called,
;        immediately before every jsr (the exectest.s/issue #6
;        real-hardware-correct convention).
;   A2 = command-line cursor during dispatch (dispatch has no further use
;        for it once a mode is chosen).
;   A5 = frame pointer for `clean`'s LINK/UNLK frames (deliberately not
;        A6, which stays reserved for library-base swapping throughout).
;
; --- strmatch: A1=cmdline cursor, A0=candidate keyword -> D0 (1/0) ---
;
; Identical subroutine to memtest.s's own (see that file's header comment
; for the full derivation, including the real bug it caught: D1/D2 must
; be explicitly zeroed once before the comparison loop, since
; `move.b (An)+,Dn` only ever writes Dn's low byte).
;
; --- Regenerating fixtures/stacktest ---
;
; With PhxAss (real Amiga assembler, run under volamos itself -- Aminet
; freeware, not checked into this repo) mapping a host directory as an
; Amiga volume:
;
;   mkdir -p /tmp/phx && cp fixtures/stacktest.s /tmp/phx/
;   ./target/debug/volamos -V work:/tmp/phx ~/amiga/PhxAss/PhxAss work:stacktest.s
;   cp /tmp/phx/stacktest fixtures/stacktest
;
; PhxAss emits a hunk EXECUTABLE directly (no linker/EXE/S switch needed,
; since this file has no external references).
;
; Without an assembler available, fixtures/gen_stacktest.py (via
; fixtures/amiga_asm.py) is the authoritative, byte-identical (for its
; own encoders -- not byte-identical to PhxAss's whole-program output;
; see fixtures/README.md's "stacktest" section, same branch-optimization
; caveat as memtest.s), toolchain-free generator; keep the two in sync.

        section code

start:
        move.l  a0,a2                    ; A2 = saved command-line pointer
                                          ; (A0 is scratch across the jsr below)

        move.l  4,a4                     ; A4 = AbsExecBase = EXEC_LIBRARY_BASE
        move.l  a4,a6
        move.l  #dosname,a1
        moveq   #0,d0
        jsr     -552(a6)                 ; OpenLibrary("dos.library",0) -- unchecked
        move.l  d0,a3                    ; A3 = dos.library base (kept)

        ; --- dispatch: try each keyword in turn ---
        move.l  a2,a1
        move.l  #kw_below,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_below

        move.l  a2,a1
        move.l  #kw_smash,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_smash

        move.l  a2,a1
        move.l  #kw_clean,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_clean

        move.l  a2,a1
        move.l  #kw_pushret,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_pushret

        move.l  a2,a1
        move.l  #kw_deep,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_deep

        bra     mode_usage

; --- strmatch(A1=cmdline cursor, A0=candidate keyword) -> D0 (1/0) ---
; See memtest.s's header comment for the full derivation.
strmatch:
        moveq   #0,d1
        moveq   #0,d2
strmatch_loop:
        move.b  (a0)+,d2                 ; D2 = next candidate-keyword byte
        tst.l   d2
        beq     strmatch_end             ; candidate exhausted -> matched so far
        move.b  (a1)+,d1                 ; D1 = next actual command-line byte
        move.l  d1,d3
        sub.l   d2,d3                    ; D3 = D1 - D2
        tst.l   d3
        bne     strmatch_fail
        bra     strmatch_loop
strmatch_end:
        move.b  (a1)+,d1                 ; the byte right after the matched prefix
        cmpi.b  #32,d1                   ; ' '
        beq     strmatch_ok
        cmpi.b  #10,d1                   ; '\n'
        beq     strmatch_ok
strmatch_fail:
        moveq   #0,d0
        rts
strmatch_ok:
        moveq   #1,d0
        rts

; --- below: read a byte 128 bytes below the current SP, without moving
; SP. Everything below SP is dead/released stack space -- --sanitize
; should report a below-stack-pointer violation (read) at sp-128. ---
mode_below:
        move.l  a3,a6
        move.l  #msg_below,d1
        jsr     -948(a6)                 ; PutStr
        move.b  -128(sp),d0              ; the below-SP read itself (result discarded)
        moveq   #0,d0
        rts

; --- smash: bsr into a subroutine that overwrites its own just-pushed
; return address in place, then rts's. --sanitize should report a
; return-address-corruption violation at the matching RTS; the process
; must still complete and exit 0 -- see the module docstring's "smash
; design constraint" for why the overwrite targets a real label instead
; of garbage. ---
mode_smash:
        move.l  a3,a6
        move.l  #msg_smash,d1
        jsr     -948(a6)                 ; PutStr
        bsr     smash_victim
        ; unreachable in the intended (bug-present) behaviour: the
        ; overwritten return address sends control straight to
        ; smash_landing instead of back here. If this DOES run, the
        ; overwrite silently failed -- a distinctive canary exit code so
        ; that failure mode is itself visible, not our own bug fixture's
        ; job to detect.
        moveq   #98,d0
        rts

smash_victim:
        move.l  #smash_landing,(sp)      ; overwrite our own just-pushed
                                          ; return address in place
        rts                              ; "returns" to smash_landing, not
                                          ; mode_smash's next instruction

smash_landing:
        moveq   #0,d0
        rts

; --- clean: three levels of nested bsr/rts, each a LINK/UNLK stack
; frame with a local variable written and read at a fixed frame-
; pointer-relative displacement. Ordinary, correct code -- must produce
; ZERO violations. ---
mode_clean:
        move.l  a3,a6
        move.l  #msg_clean,d1
        jsr     -948(a6)                 ; PutStr
        bsr     clean_level1
        moveq   #0,d0
        rts

clean_level1:
        link    a5,#-8
        move.l  #$11111111,-8(a5)        ; local write, in-frame (above new SP)
        bsr     clean_level2
        move.l  -8(a5),d0                ; local read-back (in-frame; result unused)
        unlk    a5
        rts

clean_level2:
        link    a5,#-8
        move.l  #$22222222,-8(a5)
        bsr     clean_level3
        move.l  -8(a5),d0
        unlk    a5
        rts

clean_level3:
        link    a5,#-4
        move.l  #$33333333,-4(a5)
        move.l  -4(a5),d0
        unlk    a5
        rts

; --- pushret: `move.l #target,-(sp)` / `rts`, the classic m68k
; computed-jump idiom -- no matching JSR/BSR anywhere. Legitimate,
; extremely common Amiga code; the shadow call stack must stay silent.
; The single most valuable false-positive guard in this fixture. ---
mode_pushret:
        move.l  a3,a6
        move.l  #msg_pushret,d1
        jsr     -948(a6)                 ; PutStr
        move.l  #pushret_target,-(sp)
        rts                              ; jumps to pushret_target -- not a
                                          ; return from any call

pushret_target:
        moveq   #0,d0
        rts

; --- deep: bsr-recurse 64 levels deep (each level also MOVEM-pushes/
; pops D0, so real SP movement happens at every level) and unwind
; cleanly via matching RTSes. Must produce ZERO violations -- exercises
; call-stack depth and below-SP delta tracking under a lot of balanced
; SP movement. ---
mode_deep:
        move.l  a3,a6
        move.l  #msg_deep,d1
        jsr     -948(a6)                 ; PutStr
        moveq   #63,d0                   ; 63 recursive calls below this
                                          ; one -> 64 bsr's total
        bsr     deep_recurse
        moveq   #0,d0
        rts

deep_recurse:
        movem.l d0,-(sp)
        subq.l  #1,d0
        beq     deep_base
        bsr     deep_recurse
deep_base:
        movem.l (sp)+,d0
        rts

; --- usage: no argument, or an unrecognised one ---
mode_usage:
        move.l  a3,a6
        move.l  #msg_usage,d1
        jsr     -948(a6)                 ; PutStr
        moveq   #0,d0
        rts

        section data,data

dosname:
        dc.b    "dos.library",0
        even

kw_below:
        dc.b    "below",0
        even
kw_smash:
        dc.b    "smash",0
        even
kw_clean:
        dc.b    "clean",0
        even
kw_pushret:
        dc.b    "pushret",0
        even
kw_deep:
        dc.b    "deep",0
        even

msg_below:
        dc.b    "below: reading 128 bytes below the current stack pointer",10,0
        even
msg_smash:
        dc.b    "smash: corrupting a return address, landing on valid code",10,0
        even
msg_clean:
        dc.b    "clean: three nested link/unlk frames with locals",10,0
        even
msg_pushret:
        dc.b    "pushret: move.l #target,-(sp) / rts, no matching bsr",10,0
        even
msg_deep:
        dc.b    "deep: 64 levels of clean bsr/rts recursion",10,0
        even
msg_usage:
        dc.b    "usage: stacktest below|smash|clean|pushret|deep",10,0
        even
