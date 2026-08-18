; dirtest.s -- Phase 2 (T14) fixture for volamos (vasm mot syntax).
;
; Locks a directory, then Examine/ExNext's its way through every entry,
; printing each entry's name -- exercising Lock/UnLock, Examine/ExNext,
; and BSTR handling (fib_FileName), on top of the same real OpenLibrary
; startup flow as filetest.s.
;
; --- Calling convention ---
;
; Same real-startup OpenLibrary sequence as filetest.s (see its header
; comment); dos.library LVOs used here: Lock (-84), UnLock (-90),
; Examine (-102), ExNext (-108), PutStr (-948).
;
; If the initial Lock("TEST:dir", SHARED_LOCK) fails, prints "ERR\n" and
; exits with D0 = 1 (same convention as filetest.s).
;
; --- fib_FileName BSTR -> C string conversion ---
;
; struct FileInfoBlock's fib_FileName (offset 8) is a BSTR: one length
; byte followed by that many data bytes, *not* NUL-terminated. This
; program copies it byte-for-byte into a scratch buffer, appends '\n'
; then a NUL, and PutStr's that -- the simplest way to turn a BSTR into
; something PutStr (which wants a C string) can print. The copy loop
; uses DBRA (decrement-and-branch-unless--1) seeded with (length - 1),
; so it always runs at least once: this assumes every entry's name is
; non-empty, true for any real directory entry.
;
; --- Regenerating fixtures/dirtest ---
;
; With vasm (vasmm68k_mot) available:
;
;   vasmm68k_mot -Fhunkexe -nosym -o fixtures/dirtest fixtures/dirtest.s
;
; Without vasm, fixtures/gen_dirtest.py (via fixtures/amiga_asm.py) is
; the authoritative, byte-identical generator; keep the two in sync.

SHARED_LOCK     equ -2
FIB_FILENAME    equ 8

        section code

start:
        move.l  4,a6
        move.l  #dosname,a1
        moveq   #0,d0
        jsr     -552(a6)                ; OpenLibrary("dos.library",0)
        move.l  d0,a6

        move.l  #dirname,d1
        moveq   #SHARED_LOCK,d2
        jsr     -84(a6)                 ; Lock("TEST:dir", SHARED_LOCK)
        move.l  d0,d4                    ; D4 = lock, persists across calls
        tst.l   d0
        beq     fail

        move.l  d4,d1
        move.l  #fib,d2
        jsr     -102(a6)                ; Examine(lock, fib)

exloop:
        move.l  d4,d1
        move.l  #fib,d2
        jsr     -108(a6)                ; ExNext(lock, fib)
        tst.l   d0
        beq     done                     ; DOSFALSE: no more entries

        move.l  #fib+FIB_FILENAME,a2    ; A2 -> BSTR (len byte, then chars)
        move.l  #namebuf,a3             ; A3 -> destination C string
        clr.l   d6
        move.b  (a2)+,d6                 ; D6 = BSTR length; A2 -> chars
        subq.l  #1,d6                    ; D6 = length - 1, for dbra

copyloop:
        move.b  (a2)+,(a3)+
        dbra    d6,copyloop
        move.b  #10,(a3)+                ; '\n'
        move.b  #0,(a3)+                 ; NUL terminator

        move.l  #namebuf,d1
        jsr     -948(a6)                ; PutStr(namebuf)

        bra     exloop

done:
        move.l  d4,d1
        jsr     -90(a6)                  ; UnLock(lock)
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
dirname:
        dc.b    "TEST:dir",0
        even
errmsg:
        dc.b    "ERR\n",0
        even
fib:
        dcb.b   260,0                    ; struct FileInfoBlock, zeroed
namebuf:
        dcb.b   116,0                    ; scratch: up to 107 chars + \n + NUL
