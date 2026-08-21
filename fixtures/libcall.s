; libcall.s -- Phase L3 fixture for volamos: a CLI client program that
; genuinely OpenLibrary()s the disk-based library named on its command
; line (real AmigaOS startup convention: A0 = command-line buffer, '\n'-
; terminated -- see echoargs.s), exercising both a bare-name open
; (resolved via LIBS:, e.g. "test.library") and a full-path open (e.g.
; "SYS:libs/test.library") with the *same* binary, since OpenLibrary's
; own name-resolution logic (not this fixture) is what tells the two
; apart (library-device-loading-plan.md's issue-#15 resolution logic,
; reused verbatim by the disk-load path).
;
; Registers held constant across the whole program (real-hardware-correct
; convention -- A6 is always swapped to the *target* base immediately
; before every jsr, per fixtures/README.md's exectest.s lesson):
;   A5 = ExecBase (AbsExecBase, read once at start)
;   A4 = dos.library base (opened once, used for every PutStr)
;   A3 = the loaded library's own base (test.library)
;
; Flow:
;   1. Real startup: A5 = AbsExecBase.
;   2. Copy the command-line buffer (A0) into `namebuf` FIRST, before any
;      library call -- stopping at the first '\n' (the guest command-line
;      buffer is always '\n'-terminated -- see echoargs.s -- and this
;      fixture is only ever invoked with exactly one argument, so a
;      newline is the only terminator that matters), and NUL-terminate it
;      there. This must happen before OpenLibrary("dos.library") below:
;      A0/D0/D1/A1 are the only caller-clobbered ("scratch") registers in
;      the AmigaOS calling convention (RKRM), so real exec's OpenLibrary
;      is entitled to trash A0 -- reading the command-line buffer from A0
;      *after* any library call is real-hardware-unsafe, even though it
;      happens to survive under volamos today because the registry-hit
;      OpenLibrary path doesn't touch A0 (same class of bug as the
;      exectest.s A6-convention lesson in fixtures/README.md).
;   3. OpenLibrary("dos.library", 0) via A6=A5 -> A4.
;   4. OpenLibrary(namebuf, 0) via A6=A5. NULL -> PutStr("open failed\n")
;      via A4, exit 10.
;   5. A3 = the returned base. Call LVO -30 (UserFunc) via A6=A3: expect
;      D0==42, else PutStr("bad\n") via A4, exit 20. Else PutStr("user
;      ok\n").
;   6. Call LVO -36 (AddFunc) via A6=A3 with D0=40, D1=2: expect D0==42,
;      else "bad\n"/exit 20. Else PutStr("add ok\n").
;   7. OpenLibrary(namebuf, 0) again via A6=A5 (a second, real open of the
;      same library -- exercises the Loaded-repeat-open/lib_OpenCnt path,
;      library-device-loading-plan.md's Test 3). A3 = the (same) returned
;      base. Read lib_OpenCnt (word, base+32): expect 2, else "bad\n"/
;      exit 20. Else PutStr("cnt ok\n").
;   8. CloseLibrary both opens (A1=A3, A6=A5, LVO -414), once per open.
;      Currently a no-op for a Loaded library (L4 wires the real Close
;      vector) -- calling it here now is deliberate forward-compatibility,
;      not a functional requirement of this fixture's own assertions.
;   9. Exit 0.
;
; Regenerating: `vasmm68k_mot -Fhunkexe -nosym -o fixtures/libcall fixtures/libcall.s`

LVO_OPENLIBRARY equ -552
LVO_CLOSELIBRARY equ -414
LVO_PUTSTR      equ -948
LIB_OPENCNT_OFFSET equ 32

        section code

start:
        move.l  4.w,a5                  ; A5 = AbsExecBase (kept constant)

        ; copy the command-line arg (A0) into namebuf up to the first '\n'
        ; -- must happen before any library call: A0 is a scratch
        ; register (RKRM calling convention), not guaranteed to survive
        ; one (see header comment).
        move.l  a0,a1
        move.l  #namebuf,a2
parseloop:
        move.b  (a1)+,d1
        cmpi.b  #10,d1                  ; '\n'
        beq     parsedone
        move.b  d1,(a2)+
        bra     parseloop
parsedone:
        clr.b   (a2)

        move.l  a5,a6
        move.l  #dosname,a1
        moveq   #0,d0
        jsr     LVO_OPENLIBRARY(a6)     ; OpenLibrary("dos.library",0)
        move.l  d0,a4                   ; A4 = dos.library base (kept constant)

        ; first open
        move.l  a5,a6
        move.l  #namebuf,a1
        moveq   #0,d0
        jsr     LVO_OPENLIBRARY(a6)
        tst.l   d0
        beq     openfail
        move.l  d0,a3

        ; user vector (LVO -30) -> expect 42
        move.l  a3,a6
        jsr     -30(a6)
        cmpi.l  #42,d0
        bne     bad
        move.l  a4,a6
        move.l  #useroktxt,d1
        jsr     LVO_PUTSTR(a6)

        ; add vector (LVO -36), D0=40,D1=2 -> expect 42
        moveq   #40,d0
        moveq   #2,d1
        move.l  a3,a6
        jsr     -36(a6)
        cmpi.l  #42,d0
        bne     bad
        move.l  a4,a6
        move.l  #addoktxt,d1
        jsr     LVO_PUTSTR(a6)

        ; second open -> lib_OpenCnt should read 2
        move.l  a5,a6
        move.l  #namebuf,a1
        moveq   #0,d0
        jsr     LVO_OPENLIBRARY(a6)
        tst.l   d0
        beq     openfail
        move.l  d0,a3
        moveq   #0,d0
        move.w  LIB_OPENCNT_OFFSET(a3),d0
        cmpi.l  #2,d0
        bne     bad
        move.l  a4,a6
        move.l  #cntoktxt,d1
        jsr     LVO_PUTSTR(a6)

        ; close both opens (forward-compatible no-op today, see header)
        move.l  a5,a6
        move.l  a3,a1
        jsr     LVO_CLOSELIBRARY(a6)
        move.l  a3,a1
        jsr     LVO_CLOSELIBRARY(a6)

        moveq   #0,d0
        rts

openfail:
        move.l  a4,a6
        move.l  #failtxt,d1
        jsr     LVO_PUTSTR(a6)
        moveq   #10,d0
        rts

bad:
        move.l  a4,a6
        move.l  #badtxt,d1
        jsr     LVO_PUTSTR(a6)
        moveq   #20,d0
        rts

        section data

dosname:
        dc.b    "dos.library",0
        even
useroktxt:
        dc.b    "user ok",10,0
        even
addoktxt:
        dc.b    "add ok",10,0
        even
cntoktxt:
        dc.b    "cnt ok",10,0
        even
failtxt:
        dc.b    "open failed",10,0
        even
badtxt:
        dc.b    "bad",10,0
        even
namebuf:
        ds.b    64
        even
