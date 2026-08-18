; hello.s -- Phase 1 test fixture for volamos (vasm mot syntax).
;
; A tiny AmigaOS CLI program that makes exactly one library call and
; exits. It exists to exercise the hunk loader and (in a later stage)
; the trap-and-dispatch mechanism end to end.
;
; --- Calling convention assumed by this fixture ---
;
; * A6 holds a library base pointer at program start. For Phase 1 this
;   is a *fake* dos.library base the runtime sets up purely so the one
;   LVO call below can be trapped and dispatched to a hand-registered
;   Rust handler -- there is no real OpenLibrary("dos.library", 0) call
;   in this fixture (Phase 1 only fakes the single LVO it needs).
;
; * The library call is dos.library's PutStr, at its standard negative
;   jump-table offset -948 (0x3B4 = 948 decimal), invoked the normal
;   AmigaOS way: `jsr _LVOPutStr(a6)`, i.e. `jsr -948(a6)`. Its argument
;   (a pointer to a null-terminated string) is passed in D1, matching
;   dos.library's real PutStr(string) calling convention.
;
; * Exit convention: the runtime is expected to arrange the return
;   address on the stack at program start so it lands in an internal
;   exit stub. That means this program does *not* call Exit(); it just
;   sets D0 to the desired process return code and executes a plain
;   RTS, exactly like falling off the end of a normal AmigaOS C `main`
;   compiled without ixemul/libnix startup code doing the DOS-level
;   Exit() call for you. The runtime's exit stub is responsible for
;   turning that RTS + D0 into an actual process exit code.
;
; --- Regenerating fixtures/hello ---
;
; With vasm (vasmm68k_mot) available:
;
;   vasmm68k_mot -Fhunkexe -nosym -o fixtures/hello fixtures/hello.s
;
; Without vasm, fixtures/gen_hello.py hand-assembles this exact program
; (same opcodes, same layout) and emits a byte-identical hunk executable
; without needing a toolchain; see that file for the encoding of each
; instruction. Keep the two in sync if you change this source.

    section code

start:
    move.l  #msg,d1         ; D1 = pointer to the string argument for PutStr
    jsr     -948(a6)        ; call dos.library/PutStr (_LVOPutStr) through A6
    moveq   #0,d0            ; D0 = process exit code (0 = success)
    rts                       ; return to the runtime's exit stub; D0 is the exit code

    section data

msg:
    dc.b    "Hello from volamos\n",0
    even
