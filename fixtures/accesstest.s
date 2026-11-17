; accesstest.s -- issue #71/#72 fixture for volamos (vasm mot /
; PhxAss syntax).
;
; A tiny AmigaOS CLI program used to validate volamos's
; `--sanitize-lowmem` (NULL / low-memory access) and `--sanitize-wild`
; (wild-pointer access) detectors. Same `strmatch` dispatch convention as
; `stacktest.s`/`memtest.s` (see memtest.s's header comment for the full
; mechanism -- copied verbatim here, not re-derived). Each mode prints a
; line, does exactly one thing, and exits 0: noticing the bug is the
; sanitizer's job, not the guest's.
;
;   clean    -- what every real program does; must report NOTHING with
;               both detectors on: read the AbsExecBase longword at
;               address 4, then lib_Version out of ExecBase and
;               dos.library.
;   null     -- write a longword through a NULL pointer (address 0), then
;               read a field at offset 8 of it.
;   vector   -- write a byte at $0101, inside the exception vector table.
;   abs4word -- read a WORD at address 4 (only the longword read is the
;               allowed AbsExecBase idiom).
;   wild     -- read a longword through the garbage pointer $AAAA4444.
;   hardware -- read VHPOSR ($DFF006), a custom-chip register volamos
;               does not model.
;
; Assemble with the real PhxAss under volamos -- see fixtures/README.md.
; gen_accesstest.py is the authoritative, toolchain-free build.

LVOOpenLibrary  EQU     -552
LVOPutStr       EQU     -948

        SECTION code,CODE

start:
        move.l  a0,a2                    ; A2 = saved command-line pointer

        move.l  4,a4                     ; A4 = AbsExecBase = EXEC_LIBRARY_BASE
        move.l  a4,a6
        move.l  #dosname,a1
        moveq   #0,d0
        jsr     LVOOpenLibrary(a6)       ; OpenLibrary("dos.library",0) -- unchecked
        move.l  d0,a3                    ; A3 = dos.library base (kept)

        ; --- dispatch: try each keyword in turn ---
        move.l  a2,a1
        move.l  #kw_clean,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_clean

        move.l  a2,a1
        move.l  #kw_null,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_null

        move.l  a2,a1
        move.l  #kw_vector,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_vector

        move.l  a2,a1
        move.l  #kw_abs4word,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_abs4word

        move.l  a2,a1
        move.l  #kw_wild,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_wild

        move.l  a2,a1
        move.l  #kw_hardware,a0
        bsr     strmatch
        tst.l   d0
        bne     mode_hardware

        bra     mode_usage

; --- strmatch(A1=cmdline cursor, A0=candidate keyword) -> D0 (1/0) ---
; See memtest.s's header comment for the full derivation.
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
        move.b  (a1)+,d1                 ; keyword must end at ' ' or '\n'
        cmp.b   #32,d1
        beq     strmatch_ok
        cmp.b   #10,d1
        beq     strmatch_ok
strmatch_fail:
        moveq   #0,d0
        rts
strmatch_ok:
        moveq   #1,d0
        rts

mode_clean:
        move.l  a3,a6
        move.l  #msg_clean,d1
        jsr     LVOPutStr(a6)
        move.l  #4,a0
        move.l  0(a0),d0                 ; the AbsExecBase idiom: move.l 4,d0
        move.w  20(a4),d0                ; SysBase->LibNode.lib_Version
        move.w  20(a3),d0                ; DOSBase->lib_Version
        moveq   #0,d0
        rts

mode_null:
        move.l  a3,a6
        move.l  #msg_null,d1
        jsr     LVOPutStr(a6)
        moveq   #0,d0
        move.l  #0,a0                    ; the NULL pointer
        move.l  d0,0(a0)                 ; *p = ...   (write at 0)
        move.l  8(a0),d1                 ; p->field at offset 8 (read at 8)
        moveq   #0,d0
        rts

mode_vector:
        move.l  a3,a6
        move.l  #msg_vector,d1
        jsr     LVOPutStr(a6)
        move.l  #$100,a0
        move.b  #$55,1(a0)               ; write at $0101
        moveq   #0,d0
        rts

mode_abs4word:
        move.l  a3,a6
        move.l  #msg_abs4word,d1
        jsr     LVOPutStr(a6)
        move.l  #4,a0
        move.w  0(a0),d0                 ; word read at 4: not the AbsExecBase idiom
        moveq   #0,d0
        rts

mode_wild:
        move.l  a3,a6
        move.l  #msg_wild,d1
        jsr     LVOPutStr(a6)
        move.l  #$AAAA4444,a0
        move.l  0(a0),d0
        moveq   #0,d0
        rts

mode_hardware:
        move.l  a3,a6
        move.l  #msg_hardware,d1
        jsr     LVOPutStr(a6)
        move.l  #$DFF000,a0
        move.w  6(a0),d0                 ; VHPOSR
        moveq   #0,d0
        rts

mode_usage:
        move.l  a3,a6
        move.l  #msg_usage,d1
        jsr     LVOPutStr(a6)
        moveq   #0,d0
        rts

        SECTION data,DATA

dosname:        dc.b    "dos.library",0

kw_clean:       dc.b    "clean",0
kw_null:        dc.b    "null",0
kw_vector:      dc.b    "vector",0
kw_abs4word:    dc.b    "abs4word",0
kw_wild:        dc.b    "wild",0
kw_hardware:    dc.b    "hardware",0
msg_clean:      dc.b    "clean: AbsExecBase read + ExecBase/dos.library field reads",10,0
msg_null:       dc.b    "null: longword write through a NULL pointer, then read at +8",10,0
msg_vector:     dc.b    "vector: byte write at $0101",10,0
msg_abs4word:   dc.b    "abs4word: WORD read at address 4",10,0
msg_wild:       dc.b    "wild: longword read through $AAAA4444",10,0
msg_hardware:   dc.b    "hardware: word read of VHPOSR ($DFF006)",10,0
msg_usage:      dc.b    "usage: accesstest clean|null|vector|abs4word|wild|hardware",10,0
        even
