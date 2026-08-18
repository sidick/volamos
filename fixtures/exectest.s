; exectest.s -- Phase 3 stage 8 fixture for volamos (vasm mot syntax).
;
; End-to-end coverage, through a real hunk-loaded program run by the
; actual `volamos` CLI, for the Phase 3 handlers that otherwise only had
; in-crate unit tests: exec.library's AllocMem/FreeMem/AllocVec/FreeVec
; (execmem.rs), utility.library opened for real via OpenLibrary and its
; Stricmp/GetTagData/Strnicmp (utility.rs), and exec.library's
; FindTask/SetSignal plus dos.library's CheckSignal (exectask.rs).
;
; --- Calling convention this fixture relies on ---
;
; Real startup (same as filetest.s/dirtest.s/echoargs.s/systest.s):
;
;   move.l  4,a6                    ; A6 = AbsExecBase (= EXEC_LIBRARY_BASE)
;   move.l  #dosname,a1
;   moveq   #0,d0
;   jsr     -552(a6)                ; exec.library/OpenLibrary
;   move.l  d0,a3                   ; A3 = dos.library base -- NOT a6:
;                                    ; unlike the earlier fixtures, this one
;                                    ; keeps making further exec.library
;                                    ; calls afterward and needs a6 free for
;                                    ; those. The trap dispatcher resolves
;                                    ; purely from where a jsr physically
;                                    ; lands, not from any "current a6"
;                                    ; notion the runtime tracks -- any
;                                    ; address register can hold any
;                                    ; library base at any time (see
;                                    ; crates/volamos-core/src/dispatch.rs's
;                                    ; module docs).
;
; a4 later holds utility.library's base, obtained the same way via a real
; OpenLibrary("utility.library", 0) call through a6 = EXEC_LIBRARY_BASE
; (this runtime always resolves that name to the fixed
; UTILITY_LIBRARY_BASE -- it's registered as a real library at
; Runtime::new time, never the auto-created-fake-library path).
;
; On any failure, this program PutStrs a single fixed "ERR\n" marker
; (the filetest.s convention) and exits with a distinct nonzero code
; (1-9, one per checked step below) rather than decoding the failure
; into printed text. On full success it PutStrs "exec ok\n" and exits 0.
;
; --- Regenerating fixtures/exectest ---
;
; With vasm (vasmm68k_mot) available:
;
;   vasmm68k_mot -Fhunkexe -nosym -o fixtures/exectest fixtures/exectest.s
;
; Without vasm, fixtures/gen_exectest.py (built on the tiny two-pass
; assembler in fixtures/amiga_asm.py) hand-assembles this exact program
; and is the authoritative, byte-identical generator; keep the two in
; sync if you change either.

MEMF_CLEAR      equ     $10000
TAG_VAL         equ     100
TAG_PLANTED     equ     7

        section code

start:
        move.l  4,a6                    ; A6 = AbsExecBase
        move.l  #dosname,a1
        moveq   #0,d0
        jsr     -552(a6)                ; OpenLibrary("dos.library",0) -- unchecked
        move.l  d0,a3                    ; A3 = dos.library base

; --- (a) exec.library: AllocMem/FreeMem, AllocVec/FreeVec ---
        moveq   #64,d0
        move.l  #MEMF_CLEAR,d1
        jsr     -198(a6)                ; AllocMem(64, MEMF_CLEAR) -> D0
        tst.l   d0
        beq     err1                     ; NULL -> exit 1

        move.l  d0,a0                    ; A0 = block addr
        move.l  a0,d3                    ; D3 = block addr (saved for FreeMem)
        moveq   #0,d1
        move.b  (a0)+,d1                 ; D1 = first byte (MEMF_CLEAR: should be 0)
        tst.l   d1
        bne     err2                     ; nonzero -> exit 2

        move.b  #$AB,(a0)+               ; write a byte pattern
        move.l  d3,a1                    ; A1 = block addr (restored)
        moveq   #64,d0
        jsr     -210(a6)                 ; FreeMem(block, 64)

        moveq   #20,d0
        moveq   #0,d1
        jsr     -684(a6)                 ; AllocVec(20, 0) -> D0
        tst.l   d0
        beq     err3                     ; NULL -> exit 3
        move.l  d0,a1
        jsr     -690(a6)                 ; FreeVec

; --- (b) utility.library, opened for real ---
        move.l  #utilname,a1
        moveq   #0,d0
        jsr     -552(a6)                 ; OpenLibrary("utility.library",0) -> D0
        tst.l   d0
        beq     err4                     ; NULL -> exit 4
        move.l  d0,a4                    ; A4 = utility.library base

        move.l  #str_upper,a0
        move.l  #str_lower,a1
        jsr     -162(a4)                 ; Stricmp("AMIGA","amiga") -> D0 (expect 0)
        tst.l   d0
        bne     err5                     ; nonzero -> exit 5

        move.l  #taglist,a0
        moveq   #TAG_VAL,d0
        move.l  #$DEAD,d1                ; default -- shouldn't be returned
        jsr     -36(a4)                  ; GetTagData -> D0 (expect 7)
        subq.l  #TAG_PLANTED,d0
        tst.l   d0
        bne     err6                     ; wrong value -> exit 6

        move.l  #str_a,a0
        move.l  #str_b,a1
        moveq   #6,d0
        jsr     -168(a4)                 ; Strnicmp("HELLO1","HELLO2",6) -> D0 (expect != 0)
        tst.l   d0
        beq     err7                     ; (wrongly) equal -> exit 7

; --- (c) exec.library task/signal + dos.library CheckSignal ---
        move.l  #0,a1
        jsr     -294(a6)                 ; FindTask(NULL) -> D0
        tst.l   d0
        beq     err8                     ; NULL -> exit 8

        moveq   #0,d0
        moveq   #0,d1
        jsr     -306(a6)                 ; SetSignal(0,0) -- read, unchecked

        moveq   #32,d0
        moveq   #32,d1
        jsr     -306(a6)                 ; SetSignal(1<<5,1<<5) -- sets bit 5

        moveq   #32,d1
        jsr     -792(a3)                 ; dos CheckSignal(1<<5) -> D0
        move.l  d0,d2
        moveq   #32,d1
        sub.l   d1,d2                    ; D2 = D0 - 32
        tst.l   d2
        bne     err9                     ; wrong bit -> exit 9

; --- (d) success ---
        move.l  #okmsg,d1
        jsr     -948(a3)                 ; PutStr("exec ok\n") via dos.library
        moveq   #0,d0
        rts

err1:
        move.l  #errmsg,d1
        jsr     -948(a3)
        moveq   #1,d0
        rts
err2:
        move.l  #errmsg,d1
        jsr     -948(a3)
        moveq   #2,d0
        rts
err3:
        move.l  #errmsg,d1
        jsr     -948(a3)
        moveq   #3,d0
        rts
err4:
        move.l  #errmsg,d1
        jsr     -948(a3)
        moveq   #4,d0
        rts
err5:
        move.l  #errmsg,d1
        jsr     -948(a3)
        moveq   #5,d0
        rts
err6:
        move.l  #errmsg,d1
        jsr     -948(a3)
        moveq   #6,d0
        rts
err7:
        move.l  #errmsg,d1
        jsr     -948(a3)
        moveq   #7,d0
        rts
err8:
        move.l  #errmsg,d1
        jsr     -948(a3)
        moveq   #8,d0
        rts
err9:
        move.l  #errmsg,d1
        jsr     -948(a3)
        moveq   #9,d0
        rts

        section data

dosname:
        dc.b    "dos.library",0
        even
utilname:
        dc.b    "utility.library",0
        even
str_upper:
        dc.b    "AMIGA",0
        even
str_lower:
        dc.b    "amiga",0
        even
str_a:
        dc.b    "HELLO1",0
        even
str_b:
        dc.b    "HELLO2",0
        even
taglist:
        dc.l    TAG_VAL,TAG_PLANTED,0,0  ; {TAG_VAL,7}, {TAG_DONE,0}
errmsg:
        dc.b    "ERR\n",0
        even
okmsg:
        dc.b    "exec ok\n",0
        even
