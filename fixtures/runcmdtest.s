; runcmdtest.s -- RunCommand fixture for volamos (vasm mot syntax).
;
; Proves RunCommand() really re-runs an already-LoadSeg'd program to
; completion: it LoadSegs "TEST:echoargs" (fixtures/echoargs, an existing
; Phase 2 fixture), RunCommands it with a "run cmd" parameter buffer,
; checks the nested program's propagated exit code, UnLoadSegs the
; seglist, and only then prints its own message and exits -- proving the
; nested run's stdout output interleaves correctly with the parent's own,
; and that control (and A6/dos.library access) correctly returns to the
; parent afterward, same shape as fixtures/systest.s but exercising
; LoadSeg+RunCommand+UnLoadSeg instead of SystemTagList.
;
; --- Calling convention this fixture relies on ---
;
; Real startup (same as systest.s/filetest.s/dirtest.s/echoargs.s -- see
; those for the full explanation):
;
;   move.l  4,a6                    ; A6 = AbsExecBase
;   move.l  #dosname,a1             ; A1 = "dos.library"
;   moveq   #0,d0                    ; D0 = version 0
;   jsr     -552(a6)                ; exec.library/OpenLibrary
;   move.l  d0,a6                    ; A6 = dos.library base
;
; --- Regenerating fixtures/runcmdtest ---
;
; With vasm (vasmm68k_mot) available:
;
;   vasmm68k_mot -Fhunkexe -nosym -o fixtures/runcmdtest fixtures/runcmdtest.s
;
; Without vasm, fixtures/gen_runcmdtest.py (built on the tiny two-pass
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

; LoadSeg("TEST:echoargs") -- resolves "TEST:echoargs" through the
; caller's Vfs, reads and parses it, and builds a seglist for it. D0 =
; seglist BPTR, or 0 on failure.
        move.l  #segname,d1
        jsr     -150(a6)                ; LoadSeg
        move.l  d0,d1                    ; D1 = seglist BPTR (RunCommand's arg)

; RunCommand(seg, stack, paramptr, paramlen) -- re-runs the program the
; seglist above was loaded from as a nested guest program, with "run
; cmd" as its command-line args (see fixtures/README.md's echoargs
; section: it PutStrs its own command line verbatim, so "run cmd\n"
; should appear on stdout before anything below this call prints). D0 on
; return is the nested program's own exit code (echoargs always exits
; 0), or -1 if RunCommand couldn't even run it.
        move.l  #paramlen_val,d4
        move.l  #param,d3
        move.l  #8192,d2                 ; D2 = requested stack size
        jsr     -504(a6)                 ; RunCommand

        tst.l   d0
        beq     ok

        moveq   #99,d0                   ; nested run failed/propagated
        rts                               ; a nonzero exit code

ok:
; UnLoadSeg(seg) -- D1 still holds the seglist BPTR from above (no
; library call in between touches it).
        jsr     -156(a6)                 ; UnLoadSeg

        move.l  #okmsg,d1
        jsr     -948(a6)                 ; PutStr("after runcommand\n")
        moveq   #43,d0                   ; distinctive success sentinel
        rts

        section data

dosname:
        dc.b    "dos.library",0
        even
segname:
        dc.b    "TEST:echoargs",0
        even
param:
        dc.b    "run cmd"
paramlen_val = 7
okmsg:
        dc.b    "after runcommand\n",0
        even
