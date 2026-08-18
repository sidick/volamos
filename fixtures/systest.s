; systest.s -- Phase 3 stage 7 fixture for volamos (vasm mot syntax).
;
; Proves SystemTagList() (System()'s underlying LVO) really runs a
; *nested* guest program to completion: it invokes "TEST:echoargs sys
; arg" (fixtures/echoargs, an existing Phase 2 fixture, run with args
; "sys"/"arg"), checks the nested program's propagated exit code, and
; only then prints its own message and exits -- proving both that the
; nested run's stdout output interleaves correctly with the parent's own,
; and that control (and A6/dos.library access) correctly returns to the
; parent afterward.
;
; --- Calling convention this fixture relies on ---
;
; Real startup (same as filetest.s/dirtest.s/echoargs.s -- see those for
; the full explanation):
;
;   move.l  4,a6                    ; A6 = AbsExecBase
;   move.l  #dosname,a1             ; A1 = "dos.library"
;   moveq   #0,d0                    ; D0 = version 0
;   jsr     -552(a6)                ; exec.library/OpenLibrary
;   move.l  d0,a6                    ; A6 = dos.library base
;
; --- Regenerating fixtures/systest ---
;
; With vasm (vasmm68k_mot) available:
;
;   vasmm68k_mot -Fhunkexe -nosym -o fixtures/systest fixtures/systest.s
;
; Without vasm, fixtures/gen_systest.py (built on the tiny two-pass
; assembler in fixtures/amiga_asm.py) hand-assembles this exact program
; and is the authoritative, byte-identical generator; keep the two in
; sync if you change either.

        section code

start:
        move.l  4,a6                    ; A6 = AbsExecBase
        move.l  #dosname,a1
        moveq   #0,d0
        jsr     -552(a6)                ; OpenLibrary("dos.library",0)
        move.l  d0,a6                    ; A6 = dos.library base

; SystemTagList("TEST:echoargs sys arg", NULL) -- resolves "TEST:echoargs"
; through the caller's Vfs, loads it, and runs it to completion as a
; nested guest program with "sys"/"arg" as its command-line args (see
; fixtures/README.md's echoargs section: it PutStrs its own command line
; verbatim, so "sys arg\n" should appear on stdout before anything below
; this call prints). D0 on return is the nested program's own exit code
; (echoargs always exits 0), or -1 if SystemTagList couldn't even resolve/
; run it.
        move.l  #cmd,d1
        moveq   #0,d2                    ; D2 = NULL tag list
        jsr     -606(a6)                ; SystemTagList

        tst.l   d0
        beq     ok

        moveq   #99,d0                   ; nested run failed/propagated
        rts                               ; a nonzero exit code

ok:
        move.l  #okmsg,d1
        jsr     -948(a6)                 ; PutStr("after system\n")
        moveq   #42,d0                   ; distinctive success sentinel
        rts

        section data

dosname:
        dc.b    "dos.library",0
        even
cmd:
        dc.b    "TEST:echoargs sys arg",0
        even
okmsg:
        dc.b    "after system\n",0
        even
