; recurse.s -- Phase 3 stage 8 fixture for volamos (vasm mot syntax).
;
; Trips the guest stack-overflow guard
; (crates/volamos-core/src/exectask.rs's check_stack_bounds): an infinite
; loop that makes one cheap dos.library call (PutStr of a one-byte
; message, which is what actually re-checks the stack bounds -- the check
; runs once per *dispatched trap*, never on a bare instruction) and then
; `bsr`s back to the top of the loop, which is what actually grows the
; stack -- each bsr pushes a 4-byte return address that's never popped
; (there's no matching rts; the loop never returns). Run with a small
; --stack (e.g. 4096, volamos_core::MIN_STACK_SIZE) so the guard trips
; after only ~1000 iterations.
;
; --- Calling convention ---
;
; Real startup (as every other fixture -- see filetest.s/echoargs.s/
; systest.s for the full explanation):
;
;   move.l  4,a6
;   move.l  #dosname,a1
;   moveq   #0,d0
;   jsr     -552(a6)                ; OpenLibrary("dos.library",0)
;   move.l  d0,a6
;
; --- Regenerating fixtures/recurse ---
;
; With vasm (vasmm68k_mot) available:
;
;   vasmm68k_mot -Fhunkexe -nosym -o fixtures/recurse fixtures/recurse.s
;
; Without vasm, fixtures/gen_recurse.py (via fixtures/amiga_asm.py) is
; the authoritative, byte-identical generator; keep the two in sync.

        section code

start:
        move.l  4,a6
        move.l  #dosname,a1
        moveq   #0,d0
        jsr     -552(a6)                ; OpenLibrary("dos.library",0)
        move.l  d0,a6

loop:
        move.l  #msg,d1
        jsr     -948(a6)                 ; PutStr("x\n") -- re-checks stack bounds
        bsr     loop                     ; push a return address, never popped

        section data

dosname:
        dc.b    "dos.library",0
        even
msg:
        dc.b    "x\n",0
        even
