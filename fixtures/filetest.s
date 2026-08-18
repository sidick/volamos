; filetest.s -- Phase 2 (T14) fixture for volamos (vasm mot syntax).
;
; Writes a file, reads it back, and prints what it read -- exercising
; the real AmigaOS startup flow (OpenLibrary, not a pre-seeded A6),
; dos.library's Open/Write/Close/Read, and volume/assign path
; resolution ("TEST:out.txt").
;
; --- Calling convention this fixture relies on ---
;
; Unlike Phase 1's hello.s (which assumed a pre-seeded A6), this uses
; the real startup convention every compiled AmigaOS program uses:
;
;   move.l  4,a6                    ; A6 = AbsExecBase
;   move.l  #dosname,a1             ; A1 = "dos.library"
;   moveq   #0,d0                    ; D0 = version 0
;   jsr     -552(a6)                ; exec.library/OpenLibrary
;   move.l  d0,a6                    ; A6 = dos.library base
;
; From there, every dos.library call below goes through this A6, at its
; real negative LVO offset (see crates/volamos-core/src/lvos/dos.rs):
; Open (-30), Close (-36), Read (-42), Write (-48), PutStr (-948).
;
; On any failure (Open returning a NULL BPTR in D0), this program prints
; a fixed "ERR\n" marker via PutStr and exits with D0 = 1, rather than
; decoding IoErr() into a printed decimal number (documented as the
; deliberately simplest option in docs/plan.md's T14 entry).
;
; --- Regenerating fixtures/filetest ---
;
; With vasm (vasmm68k_mot) available:
;
;   vasmm68k_mot -Fhunkexe -nosym -o fixtures/filetest fixtures/filetest.s
;
; Without vasm, fixtures/gen_filetest.py (built on the tiny two-pass
; assembler in fixtures/amiga_asm.py) hand-assembles this exact program
; and is the authoritative, byte-identical generator; keep the two in
; sync if you change either.

MODE_OLDFILE    equ 1005
MODE_NEWFILE    equ 1006

        section code

start:
        move.l  4,a6                    ; A6 = AbsExecBase
        move.l  #dosname,a1
        moveq   #0,d0
        jsr     -552(a6)                ; OpenLibrary("dos.library",0)
        move.l  d0,a6                    ; A6 = dos.library base

; --- write phase: Open(fname, MODE_NEWFILE), Write, Close ---
        move.l  #fname,d1
        move.l  #MODE_NEWFILE,d2
        jsr     -30(a6)                 ; Open
        move.l  d0,d4                    ; D4 = handle, persists across calls
        tst.l   d0
        beq     fail

        move.l  d4,d1
        move.l  #msg,d2
        move.l  #msglen,d3
        jsr     -48(a6)                 ; Write(handle, msg, msglen)

        move.l  d4,d1
        jsr     -36(a6)                 ; Close(handle)

; --- read phase: reopen MODE_OLDFILE, Read, Close ---
        move.l  #fname,d1
        move.l  #MODE_OLDFILE,d2
        jsr     -30(a6)                 ; Open
        move.l  d0,d4
        tst.l   d0
        beq     fail

        move.l  d4,d1
        move.l  #readbuf,d2
        move.l  #msglen,d3
        jsr     -42(a6)                 ; Read(handle, readbuf, msglen)

        move.l  d4,d1
        jsr     -36(a6)                 ; Close(handle)

        move.l  #readbuf,d1
        jsr     -948(a6)                ; PutStr(readbuf) -- already NUL-
                                          ; terminated: readbuf is zeroed
                                          ; and msglen < its size.

        moveq   #0,d0
        rts

fail:
        move.l  #errmsg,d1
        jsr     -948(a6)                ; PutStr("ERR\n")
        moveq   #1,d0
        rts

        section data

dosname:
        dc.b    "dos.library",0
        even
fname:
        dc.b    "TEST:out.txt",0
        even
msg:
        dc.b    "hello from filetest\n",0
msglen  equ     20                       ; length of "hello from filetest\n"
        even
errmsg:
        dc.b    "ERR\n",0
        even
readbuf:
        dcb.b   64,0                     ; zeroed scratch read buffer
