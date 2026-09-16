; linetest.s -- fixture for volamos's HUNK_DEBUG "LINE" block parser
; (issue #74). PhxAss syntax, NOT vasm mot syntax (see "Regenerating"
; in fixtures/README.md for why this one is special).
;
; The program itself is deliberately trivial, in the same shape as
; fixtures/hello.s: it PutStr's a short message via dos.library's
; PutStr (LVO -948, i.e. `jsr -948(a6)`), then sets D0 = 0 and RTS's
; back to the runtime's exit stub, exactly like hello.s. There is no
; OpenLibrary call -- see hello.s's own header comment for the full
; calling-convention rationale (A6 is pre-seeded by the runtime with
; a fake dos.library base for the one LVO call below).
;
; What actually matters here is *not* the program's behaviour but its
; HUNK_DEBUG LINE block: the three instructions below sit on lines
; 20, 25, 30. See fixtures/README.md's "linetest" section for the
; exact expected (line, offset) pairs.
    section code,code

start:
    move.l  #msg,d1         ; line 20: D1 = pointer to the message

; padding
; padding

    jsr     -948(a6)        ; line 25: call dos.library/PutStr

; padding
; padding

    moveq   #0,d0            ; line 30: D0 = process exit code (0)
    rts                       ; return to the runtime's exit stub

    section data,data

msg:
    dc.b    "Hello from linetest\n",0
    even
