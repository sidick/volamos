; echoargs.s -- Phase 2 (T14) fixture for volamos (vasm mot syntax).
;
; Echoes its own AmigaOS command-line buffer, exercising T12's
; A0/D0 argument-passing convention (see
; crates/volamos-core/src/dispatch.rs's Runtime::new docs) end to end.
;
; --- Calling convention ---
;
; AmigaOS CLI startup convention: A0 = pointer to the command-line
; buffer (args joined with spaces, '\n'-terminated), D0 = its length.
; volamos's runtime additionally NUL-terminates the buffer (one extra
; byte after the '\n', for code that scans for a terminator instead of
; trusting D0) -- so A0 is already usable directly as PutStr's
; CString* argument, no copying needed.
;
; **Found the hard way, via real Kickstart hardware (issue #63)**: A0
; is a *scratch* register across any library call, same class as
; D0/D1/A1 -- OpenLibrary's own *documented* convention only says A1 =
; name, D0 = version (it doesn't promise A0 is left alone). volamos's
; own OpenLibrary handler happens not to touch A0, but real Kickstart's
; does, so this fixture copies the args pointer into A2 (a
; callee-preserved register) *before* the OpenLibrary call, and reads
; it back from A2 afterward, instead of assuming A0 survives.
;
; With guest args "foo bar", this prints "foo bar \n" (a trailing space
; before the newline -- real AmigaOS's own convention, confirmed via
; real Kickstart hardware, issue #63); with none, the buffer is still
; just "\n" (no leading space, and the trailing newline is
; unconditional), so it prints "\n".
;
; --- Regenerating fixtures/echoargs ---
;
; With vasm (vasmm68k_mot) available:
;
;   vasmm68k_mot -Fhunkexe -nosym -o fixtures/echoargs fixtures/echoargs.s
;
; Without vasm, fixtures/gen_echoargs.py (via fixtures/amiga_asm.py) is
; the authoritative, byte-identical generator; keep the two in sync.

        section code

start:
        move.l  a0,a2                    ; save the command-line buffer -- A0
                                          ; is scratch across the call below
        move.l  4,a6
        move.l  #dosname,a1
        moveq   #0,d0
        jsr     -552(a6)                ; OpenLibrary("dos.library",0)
        move.l  d0,a6

        move.l  a2,d1                    ; D1 = the command-line buffer
        jsr     -948(a6)                 ; PutStr(d1)

        moveq   #0,d0
        rts

        section data

dosname:
        dc.b    "dos.library",0
        even
