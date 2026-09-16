; memtest.s -- issue #65 fixture for volamos (vasm mot / PhxAss syntax).
;
; A deliberately buggy AmigaOS CLI program used to validate volamos's
; `--sanitize` memory-sanitizer mode (heap redzones + a free quarantine
; around exec.library's AllocMem/FreeMem, see
; crates/volamos-core/src/execmem.rs's module docs for the exact
; AllocMem(-198)/FreeMem(-210) register contract this program relies
; on: D0=byte size, D1=requirements for AllocMem, A1=block/D0=byte size
; for FreeMem -- and note FreeMem errors out loudly if that size doesn't
; match what was actually allocated, both rounded up to 8, so every mode
; below always passes FreeMem the exact same literal 32 it passed
; AllocMem).
;
; Takes a single leading command-line keyword selecting which behaviour
; to exercise, so a test harness can assert "a clean run reports zero
; sanitizer violations" separately from each individual bug:
;
;   clean     -- AllocMem 32 bytes, write all 32, read all 32 back,
;                FreeMem. Must produce ZERO violations -- the most
;                important mode: it guards --sanitize against false
;                positives on an entirely legitimate access pattern.
;   overrun   -- AllocMem 32 bytes, write 1 byte at offset 32 (one past
;                the end, into the trailing redzone), FreeMem. Expect a
;                heap-buffer-overflow WRITE at block+32.
;   underrun  -- AllocMem 32 bytes, read 1 byte at offset -1 (into the
;                leading redzone), FreeMem. Expect a heap-buffer-
;                overflow READ at block-1.
;   uaf       -- AllocMem 32 bytes, FreeMem it, then read 1 byte at
;                offset 0 of the now-freed block. Expect a
;                use-after-free READ at the freed block's start.
;   uninit    -- AllocMem 32 bytes *without* MEMF_CLEAR, then read 1
;                byte at offset 0 without ever writing it. Silent under
;                plain --sanitize; expect one uninitialized-read report
;                at block+0 under the (not-yet-existing as of this
;                writing) --sanitize-uninit.
;   uninitpartial -- AllocMem 32 bytes without MEMF_CLEAR, write only
;                the first 16 bytes (offsets 0..15), then read offset
;                20 (inside the still-unwritten second half). Silent
;                under plain --sanitize; expect one uninitialized-read
;                report at block+20 under --sanitize-uninit -- proves
;                the detector is byte-granular, not per-allocation,
;                since offsets 0..15 of the very same block are Valid.
;   written   -- false-positive guard. AllocMem 32 bytes without
;                MEMF_CLEAR, write all 32 bytes, read all 32 back.
;                Every byte read was written first, so this must
;                report NOTHING even under --sanitize-uninit.
;   cleared   -- second false-positive guard. AllocMem 32 bytes *with*
;                MEMF_CLEAR, then read offset 0 without ever writing
;                it. MEMF_CLEAR memory is genuinely initialized, so
;                this must report NOTHING even under --sanitize-uninit.
;   zerodep   -- issue #80 fixture for volamos's `--dirty-heap` flag.
;                AllocMem 32 bytes *without* MEMF_CLEAR, read a
;                LONGWORD at offset 0 without ever writing it, and
;                BRANCH on the value -- unlike `uninit` (which only
;                reads), this mode actually ACTS on the uninitialized
;                value, printing one of two different lines depending
;                on which way the branch went. Under plain volamos
;                today (no --dirty-heap yet) the read is always zero
;                (volamos's memory starts zeroed regardless of
;                MEMF_CLEAR), so this mode always takes the "zero
;                path" and prints the line documenting that as the
;                bug. Once --dirty-heap fills non-MEMF_CLEAR
;                allocations with a poison pattern, the same binary
;                should take the "garbage path" instead -- a different
;                line of output. Under --sanitize-uninit this read
;                should still report one uninitialized-read violation
;                at block+0, exactly like `uninit` (--dirty-heap's
;                fill must not mark the bytes initialized, so the two
;                features compose rather than cancel).
;
; No argument, or an unrecognised one, prints a usage line and exits 0.
; Every mode PutStr's a short self-describing line before doing its
; thing, so test output names what's about to happen; every mode exits
; 0 via a plain `rts` (this program never calls Exit() -- see hello.s's
; header comment for the exit-stub convention every fixture here
; relies on) -- the *sanitizer's* job is to notice the bug, not this
; program's own exit code.
;
; --- Calling convention ---
;
; Real AmigaOS CLI startup: A0 = command-line buffer pointer at entry.
; A0 is a caller-clobbered ("scratch") register across any library
; call (same RKRM class as D0/D1/A1) -- see echoargs.s's header comment
; and fixtures/gen_libcall.py's comment for the same rule -- so it's
; copied into A2 (callee-preserved) *before* the very first jsr
; (OpenLibrary below), same as every fixture past hello.s.
;
; Registers held constant across the whole program, matching
; exectest.s's real-hardware-correct convention (A6 always swapped to
; the *target* library base immediately before every jsr, per
; fixtures/README.md's exectest.s/issue #6 lesson):
;   A4 = ExecBase (AbsExecBase, read once at start) -- for AllocMem/
;        FreeMem calls.
;   A3 = dos.library base (opened once) -- for PutStr calls.
; A2 doubles as the command-line cursor during dispatch, then (once a
; mode has been chosen -- dispatch has no further use for it) as the
; AllocMem'd block pointer for that mode's body.
;
; --- strmatch: A1=cmdline cursor, A0=candidate keyword -> D0 (1/0) ---
;
; A tiny subroutine, `bsr`'d once per candidate keyword. Compares bytes
; from A1 against A0 until A0's NUL is reached (meaning the whole
; candidate keyword matched so far), then requires the very next
; command-line byte to be a space or a newline -- the keyword's real
; terminator, matching the trailing-space-before-newline convention a
; non-empty guest command line carries (echoargs.s), and the bare "\n"
; convention an empty one carries -- so "clean" can't spuriously match
; a hypothetical "cleanup". D1/D2 are explicitly zeroed once before the
; comparison loop: `move.b (An)+,Dn` only ever writes the *low* byte of
; Dn, so without this, whatever garbage sits in their high 24 bits
; (left over from earlier code) could make a later full-longword
; `tst.l`/`sub.l` compare see a false nonzero, or fail to recognise the
; candidate string's NUL terminator -- found the hard way while
; developing this fixture (the very first version had exactly this bug:
; D2 held a leftover 0x2000 from AllocMem's requirements setup, so
; `tst.l d2` never saw zero and every mode fell through to `usage`).
;
; --- Regenerating fixtures/memtest ---
;
; With PhxAss (real Amiga assembler, run under volamos itself -- Aminet
; freeware, not checked into this repo) mapping a host directory as an
; Amiga volume:
;
;   ./target/debug/volamos -V work:fixtures ~/amiga/PhxAss/PhxAss work:memtest.s
;
; PhxAss emits a hunk EXECUTABLE directly (no linker/EXE/S switch
; needed, since this file has no external references) as
; fixtures/memtest.
;
; Without an assembler available, fixtures/gen_memtest.py (via
; fixtures/amiga_asm.py) is the authoritative, byte-identical,
; toolchain-free generator; keep the two in sync -- see
; fixtures/README.md's "memtest" section for the cross-check between
; them.

        section code

start:
        move.l  a0,a2                    ; A2 = saved command-line pointer
                                          ; (A0 is scratch across the jsr below)

        move.l  4,a4                     ; A4 = AbsExecBase = EXEC_LIBRARY_BASE
        move.l  a4,a6
        move.l  #dosname,a1
        moveq   #0,d0
        jsr     -552(a6)                 ; OpenLibrary("dos.library",0) -- unchecked
        move.l  d0,a3                    ; A3 = dos.library base (kept)

        ; --- dispatch: try each keyword in turn ---
        move.l  a2,a1
        move.l  #kw_clean,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_clean

        move.l  a2,a1
        move.l  #kw_overrun,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_overrun

        move.l  a2,a1
        move.l  #kw_underrun,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_underrun

        move.l  a2,a1
        move.l  #kw_uaf,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_uaf

        move.l  a2,a1
        move.l  #kw_uninit,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_uninit

        move.l  a2,a1
        move.l  #kw_uninitpartial,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_uninitpartial

        move.l  a2,a1
        move.l  #kw_written,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_written

        move.l  a2,a1
        move.l  #kw_cleared,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_cleared

        move.l  a2,a1
        move.l  #kw_zerodep,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_zerodep

        bra     mode_usage

; --- strmatch(A1=cmdline cursor, A0=candidate keyword) -> D0 (1/0) ---
strmatch:
        moveq   #0,d1
        moveq   #0,d2
strmatch_loop:
        move.b  (a0)+,d2                 ; D2 = next candidate-keyword byte
        tst.l   d2
        beq     strmatch_end             ; candidate exhausted -> matched so far
        move.b  (a1)+,d1                 ; D1 = next actual command-line byte
        move.l  d1,d3
        sub.l   d2,d3                    ; D3 = D1 - D2
        tst.l   d3
        bne     strmatch_fail
        bra     strmatch_loop
strmatch_end:
        move.b  (a1)+,d1                 ; the byte right after the matched prefix
        cmpi.b  #32,d1                   ; ' '
        beq     strmatch_ok
        cmpi.b  #10,d1                   ; '\n'
        beq     strmatch_ok
strmatch_fail:
        moveq   #0,d0
        rts
strmatch_ok:
        moveq   #1,d0
        rts

; --- clean: alloc 32, write all 32, read all 32 back, free. Must
; produce zero sanitizer violations -- the false-positive guard. ---
mode_clean:
        move.l  a3,a6
        move.l  #msg_clean,d1
        jsr     -948(a6)                 ; PutStr
        move.l  a4,a6
        moveq   #32,d0
        moveq   #0,d1
        jsr     -198(a6)                 ; AllocMem(32,0) -> D0
        tst.l   d0
        beq     allocfail
        move.l  d0,a2                    ; A2 = block ptr (dispatch's own use of A2 is done)

        move.l  a2,a1
        moveq   #31,d1                   ; dbra runs count+1 = 32 times
clean_write_loop:
        move.b  d1,(a1)+                 ; in-bounds write, offsets 0..31
        dbra    d1,clean_write_loop

        move.l  a2,a1
        moveq   #31,d1
clean_read_loop:
        move.b  (a1)+,d0                 ; in-bounds read, offsets 0..31
        dbra    d1,clean_read_loop

        move.l  a4,a6
        move.l  a2,a1
        moveq   #32,d0
        jsr     -210(a6)                 ; FreeMem(block,32)
        moveq   #0,d0
        rts

; --- overrun: alloc 32, write 1 byte at offset 32 (one past the end --
; the trailing redzone), then free. --sanitize should report a
; heap-buffer-overflow write at block+32. ---
mode_overrun:
        move.l  a3,a6
        move.l  #msg_overrun,d1
        jsr     -948(a6)                 ; PutStr
        move.l  a4,a6
        moveq   #32,d0
        moveq   #0,d1
        jsr     -198(a6)                 ; AllocMem(32,0) -> D0
        tst.l   d0
        beq     allocfail
        move.l  d0,a2

        move.b  #$ab,32(a2)              ; the overrun write itself

        move.l  a4,a6
        move.l  a2,a1
        moveq   #32,d0
        jsr     -210(a6)                 ; FreeMem(block,32)
        moveq   #0,d0
        rts

; --- underrun: alloc 32, read 1 byte at offset -1 (the leading
; redzone), then free. --sanitize should report a heap-buffer-overflow
; read at block-1. ---
mode_underrun:
        move.l  a3,a6
        move.l  #msg_underrun,d1
        jsr     -948(a6)                 ; PutStr
        move.l  a4,a6
        moveq   #32,d0
        moveq   #0,d1
        jsr     -198(a6)                 ; AllocMem(32,0) -> D0
        tst.l   d0
        beq     allocfail
        move.l  d0,a2

        move.b  -1(a2),d0                ; the underrun read itself (result discarded)

        move.l  a4,a6
        move.l  a2,a1
        moveq   #32,d0
        jsr     -210(a6)                 ; FreeMem(block,32)
        moveq   #0,d0
        rts

; --- uaf: alloc 32, free it, then read 1 byte at offset 0 of the
; now-freed block. --sanitize should report a use-after-free read at
; the freed block's start (found via the free quarantine, since the
; bytes are never reused for anything else in this same run). ---
mode_uaf:
        move.l  a3,a6
        move.l  #msg_uaf,d1
        jsr     -948(a6)                 ; PutStr
        move.l  a4,a6
        moveq   #32,d0
        moveq   #0,d1
        jsr     -198(a6)                 ; AllocMem(32,0) -> D0
        tst.l   d0
        beq     allocfail
        move.l  d0,a2

        move.l  a2,a1
        moveq   #32,d0
        jsr     -210(a6)                 ; FreeMem(block,32)

        move.b  0(a2),d0                 ; the use-after-free read itself

        moveq   #0,d0
        rts

; --- uninit: alloc 32 bytes *without* MEMF_CLEAR, then read 1 byte at
; offset 0 without ever writing it. Under plain --sanitize this is
; silent (the byte is in-bounds, still allocated, not freed); under
; --sanitize-uninit it should report one uninitialized-read at
; block+0. ---
mode_uninit:
        move.l  a3,a6
        move.l  #msg_uninit,d1
        jsr     -948(a6)                 ; PutStr
        move.l  a4,a6
        moveq   #32,d0
        moveq   #0,d1                    ; no MEMF_CLEAR -- block starts fully Uninit
        jsr     -198(a6)                 ; AllocMem(32,0) -> D0
        tst.l   d0
        beq     allocfail
        move.l  d0,a2

        move.b  0(a2),d0                 ; the uninitialized read itself

        move.l  a4,a6
        move.l  a2,a1
        moveq   #32,d0
        jsr     -210(a6)                 ; FreeMem(block,32)
        moveq   #0,d0
        rts

; --- uninitpartial: alloc 32 bytes without MEMF_CLEAR, write only the
; first 16 bytes (offsets 0..15), then read offset 20 -- inside the
; still-unwritten second half. The more realistic bug shape: proves
; the detector is byte-granular, not per-allocation, since offsets
; 0..15 of this very same block are genuinely Valid by the time of the
; read. Expect one uninitialized-read at block+20 under
; --sanitize-uninit, nothing under plain --sanitize. ---
mode_uninitpartial:
        move.l  a3,a6
        move.l  #msg_uninitpartial,d1
        jsr     -948(a6)                 ; PutStr
        move.l  a4,a6
        moveq   #32,d0
        moveq   #0,d1                    ; no MEMF_CLEAR
        jsr     -198(a6)                 ; AllocMem(32,0) -> D0
        tst.l   d0
        beq     allocfail
        move.l  d0,a2

        move.l  a2,a1
        moveq   #15,d1                   ; dbra runs count+1 = 16 times -> offsets 0..15
uninitpartial_write_loop:
        move.b  d1,(a1)+                 ; in-bounds write, offsets 0..15 only
        dbra    d1,uninitpartial_write_loop

        move.b  20(a2),d0                ; read offset 20 -- never written

        move.l  a4,a6
        move.l  a2,a1
        moveq   #32,d0
        jsr     -210(a6)                 ; FreeMem(block,32)
        moveq   #0,d0
        rts

; --- written: false-positive guard. Alloc 32 bytes without
; MEMF_CLEAR, write all 32, read all 32 back -- every byte read was
; written first, so this must report NOTHING even under
; --sanitize-uninit (same access shape as `clean`, but deliberately
; starting from an un-cleared allocation, to isolate the uninit
; detector's own false-positive behaviour from the heap-redzone
; detector's). ---
mode_written:
        move.l  a3,a6
        move.l  #msg_written,d1
        jsr     -948(a6)                 ; PutStr
        move.l  a4,a6
        moveq   #32,d0
        moveq   #0,d1                    ; no MEMF_CLEAR
        jsr     -198(a6)                 ; AllocMem(32,0) -> D0
        tst.l   d0
        beq     allocfail
        move.l  d0,a2

        move.l  a2,a1
        moveq   #31,d1                   ; dbra runs count+1 = 32 times
written_write_loop:
        move.b  d1,(a1)+                 ; in-bounds write, offsets 0..31
        dbra    d1,written_write_loop

        move.l  a2,a1
        moveq   #31,d1
written_read_loop:
        move.b  (a1)+,d0                 ; in-bounds read, offsets 0..31
        dbra    d1,written_read_loop

        move.l  a4,a6
        move.l  a2,a1
        moveq   #32,d0
        jsr     -210(a6)                 ; FreeMem(block,32)
        moveq   #0,d0
        rts

; --- cleared: second false-positive guard. Alloc 32 bytes *with*
; MEMF_CLEAR, then read offset 0 without ever writing it. MEMF_CLEAR
; memory is genuinely initialized (exec.library zeroed it), so this
; must report NOTHING even under --sanitize-uninit -- distinguishing
; "never written" from "never written by this program but zeroed by
; AllocMem itself". ---
mode_cleared:
        move.l  a3,a6
        move.l  #msg_cleared,d1
        jsr     -948(a6)                 ; PutStr
        move.l  a4,a6
        moveq   #32,d0
        move.l  #65536,d1                ; MEMF_CLEAR -- doesn't fit moveq's range
        jsr     -198(a6)                 ; AllocMem(32,MEMF_CLEAR) -> D0
        tst.l   d0
        beq     allocfail
        move.l  d0,a2

        move.b  0(a2),d0                 ; read of MEMF_CLEAR'd, never-written byte

        move.l  a4,a6
        move.l  a2,a1
        moveq   #32,d0
        jsr     -210(a6)                 ; FreeMem(block,32)
        moveq   #0,d0
        rts

; --- zerodep: alloc 32 bytes *without* MEMF_CLEAR, read a longword at
; offset 0 without ever writing it, and BRANCH on the value -- see the
; header comment above for the full rationale. ---
mode_zerodep:
        move.l  a3,a6
        move.l  #msg_zerodep,d1
        jsr     -948(a6)                 ; PutStr
        move.l  a4,a6
        moveq   #32,d0
        moveq   #0,d1                    ; no MEMF_CLEAR -- block starts fully Uninit
        jsr     -198(a6)                 ; AllocMem(32,0) -> D0
        tst.l   d0
        beq     allocfail
        move.l  d0,a2

        move.l  0(a2),d0                 ; the uninitialized read itself
        tst.l   d0
        beq     zerodep_zero

zerodep_nonzero:
        move.l  a3,a6
        move.l  #msg_zerodep_nonzero,d1
        jsr     -948(a6)                 ; PutStr
        bra     zerodep_done

zerodep_zero:
        move.l  a3,a6
        move.l  #msg_zerodep_zero,d1
        jsr     -948(a6)                 ; PutStr

zerodep_done:
        move.l  a4,a6
        move.l  a2,a1
        moveq   #32,d0
        jsr     -210(a6)                 ; FreeMem(block,32)
        moveq   #0,d0
        rts

; --- usage: no argument, or an unrecognised one ---
mode_usage:
        move.l  a3,a6
        move.l  #msg_usage,d1
        jsr     -948(a6)                 ; PutStr
        moveq   #0,d0
        rts

; --- shared AllocMem-returned-NULL path ---
allocfail:
        move.l  a3,a6
        move.l  #msg_allocfail,d1
        jsr     -948(a6)                 ; PutStr
        moveq   #20,d0
        rts

        section data,data

dosname:
        dc.b    "dos.library",0
        even

kw_clean:
        dc.b    "clean",0
        even
kw_overrun:
        dc.b    "overrun",0
        even
kw_underrun:
        dc.b    "underrun",0
        even
kw_uaf:
        dc.b    "uaf",0
        even
kw_uninit:
        dc.b    "uninit",0
        even
kw_uninitpartial:
        dc.b    "uninitpartial",0
        even
kw_written:
        dc.b    "written",0
        even
kw_cleared:
        dc.b    "cleared",0
        even
kw_zerodep:
        dc.b    "zerodep",0
        even

msg_clean:
        dc.b    "clean: alloc 32 bytes, write+read all 32, free",10,0
        even
msg_overrun:
        dc.b    "overrun: writing 1 byte past a 32-byte block",10,0
        even
msg_underrun:
        dc.b    "underrun: reading 1 byte before a 32-byte block",10,0
        even
msg_uaf:
        dc.b    "uaf: reading a freed 32-byte block",10,0
        even
msg_uninit:
        dc.b    "uninit: alloc 32 bytes without MEMF_CLEAR, read offset 0 unwritten",10,0
        even
msg_uninitpartial:
        dc.b    "uninitpartial: alloc 32, write bytes 0-15, read offset 20 unwritten",10,0
        even
msg_written:
        dc.b    "written: alloc 32 bytes without MEMF_CLEAR, write+read all 32",10,0
        even
msg_cleared:
        dc.b    "cleared: alloc 32 bytes with MEMF_CLEAR, read offset 0 unwritten",10,0
        even
msg_zerodep:
        dc.b    "zerodep: alloc 32 bytes without MEMF_CLEAR, read+branch on offset 0 unwritten",10,0
        even
msg_zerodep_zero:
        dc.b    "zerodep: read 0 -- took the zero path (this is the bug: it only works because the memory happened to be zero)",10,0
        even
msg_zerodep_nonzero:
        dc.b    "zerodep: read non-zero -- took the garbage path",10,0
        even
msg_usage:
        dc.b    "usage: memtest clean|overrun|underrun|uaf|uninit|uninitpartial|written|cleared|zerodep",10,0
        even
msg_allocfail:
        dc.b    "AllocMem failed",10,0
        even
