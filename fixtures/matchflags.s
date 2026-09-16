; matchflags.s -- issue #58 fixture for volamos (vasm mot / PhxAss syntax).
;
; Scans a directory tree via MatchFirst/MatchNext (AnchorPath, with
; APF_DODIR set so the scan descends into subdirectories -- a *nested*,
; multi-level scan, not a flat single-directory one) and prints, for
; every entry reported, three fields: ap_Flags (decimal), fib_FileName,
; and ap_Buf. This distinguishes two possibilities for how
; APF_DirChanged (bit 6, value 64) behaves across a real dos.library
; implementation, volamos, and vamos: set once per real directory
; transition (correct), or set on every single entry (too eager). A
; flat scan can't tell these apart; this one can.
;
; --- Calling convention ---
;
; Real startup (as every fixture since filetest.s): A0 = command-line
; buffer pointer at entry, copied into A2 (callee-preserved) before the
; first jsr (OpenLibrary) -- A0 is scratch across any library call (see
; echoargs.s's header comment). A6 always swapped to the target library
; base immediately before every jsr (the exectest.s/issue #6
; real-hardware-correct convention).
;
;   A3 = dos.library base (opened once).
;   A4 = ExecBase (AbsExecBase, read once) -- for AllocMem/FreeMem.
;   A5 = the AllocMem'd AnchorPath's address, kept for the whole scan.
;   A2 = command-line cursor during argument parsing.
;
; Takes a single command-line token (the directory to scan), copied up
; to (not including) the first space/newline/NUL into `dirarg` -- same
; token-parsing shape as gen_memtest.py/gen_stacktest.py's own command
; parsing, just without the multi-keyword dispatch. A missing/empty
; argument prints a usage line and exits 0.
;
; struct AnchorPath layout (crates/volamos-core/src/dosanchor.rs's
; module docs -- NDK dos/dosasl.h):
;
;   ap_Flags   BYTE  at +16
;   ap_Strlen  WORD  at +18
;   ap_Info    FileInfoBlock (260 bytes) at +20
;     fib_FileName TEXT[108] at ap_Info+8 = AnchorPath+28
;   ap_Buf     TEXT[1]  at +280 (caller-allocated tail)
;
; This fixture AllocMem's 536 bytes (280 + 256, MEMF_CLEAR) for the
; AnchorPath, sets ap_Strlen=256 (so ap_Buf gets filled in) and
; ap_Flags=APF_DODIR (4) before MatchFirst, per the runtime's own
; documented ap_Strlen/ap_Buf convention.
;
; --- Regenerating fixtures/matchflags ---
;
; With PhxAss (real Amiga assembler, run under volamos itself -- Aminet
; freeware, not checked into this repo) mapping a host directory as an
; Amiga volume:
;
;   mkdir -p /tmp/phxm && cp fixtures/matchflags.s /tmp/phxm/
;   ./target/debug/volamos -V work:/tmp/phxm ~/amiga/PhxAss/PhxAss work:matchflags.s
;
; Without an assembler available, fixtures/gen_matchflags.py (via
; fixtures/amiga_asm.py) is the authoritative, byte-identical,
; toolchain-free generator; keep the two in sync.

APF_DODIR       equ 4
MEMF_CLEAR      equ $10000
AP_SIZE         equ 536                 ; 280 (fixed AnchorPath header) + 256 (ap_Buf)
AP_FLAGS_OFF    equ 16
AP_STRLEN_OFF   equ 18
AP_BUF_STRLEN   equ 256
FIB_FILENAME_OFF equ 28                 ; ap_Info(+20) + fib_FileName(+8)
AP_BUF_OFF      equ 280

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

        ; --- copy the leading command-line token into dirarg, stopping
        ; at the first space/newline/NUL (not copied) ---
        move.l  a2,a1
        move.l  #dirarg,a0
copyloop:
        move.b  (a1)+,d0
        cmpi.b  #0,d0
        beq     copydone
        cmpi.b  #10,d0
        beq     copydone
        cmpi.b  #32,d0
        beq     copydone
        move.b  d0,(a0)+
        bra     copyloop
copydone:
        move.b  #0,(a0)+                 ; NUL-terminate dirarg

        ; empty argument (nothing copied) -> usage
        move.l  #dirarg,a1
        move.b  (a1),d0
        cmpi.b  #0,d0
        beq     usage

        ; --- AllocMem(AP_SIZE, MEMF_CLEAR) ---
        move.l  a4,a6
        move.l  #AP_SIZE,d0
        move.l  #MEMF_CLEAR,d1
        jsr     -198(a6)                 ; AllocMem
        tst.l   d0
        beq     allocfail
        move.l  d0,a5                    ; A5 = AnchorPath address (kept)

        move.w  #AP_BUF_STRLEN,AP_STRLEN_OFF(a5)   ; ap_Strlen = 256
        move.b  #APF_DODIR,AP_FLAGS_OFF(a5)         ; ap_Flags = APF_DODIR

        ; --- MatchFirst(dirarg, ap) ---
        move.l  a3,a6
        move.l  #dirarg,d1
        move.l  a5,d2
        jsr     -822(a6)                 ; MatchFirst
        tst.l   d0
        bne     matchfirstfail

matchloop:
        bsr     print_entry

        ; Re-set APF_DODIR before every MatchNext: MatchNext clears the
        ; bit itself once consumed for a descent (see
        ; crates/volamos-core/src/dosanchor.rs's module docs), so
        ; descending into every level -- the whole point of this fixture
        ; -- means re-requesting it every time, same as the NDK's own
        ; ScanDirectories() worked example. A plain overwrite is fine:
        ; ap_Flags was already read and printed by print_entry just above.
        move.b  #APF_DODIR,AP_FLAGS_OFF(a5)

        move.l  a5,d1
        jsr     -828(a6)                 ; MatchNext
        tst.l   d0
        beq     matchloop                ; D0 == 0: another entry, print it too

        ; D0 != 0: no more entries (or an error) -- either way, done
        move.l  a5,d1
        jsr     -834(a6)                 ; MatchEnd

        move.l  a4,a6
        move.l  a5,a1
        move.l  #AP_SIZE,d0
        jsr     -210(a6)                 ; FreeMem(ap, AP_SIZE)
        moveq   #0,d0
        rts

; --- print_entry: prints "flags=%ld name='%s' buf='%s'\n" for the
; entry currently in *A5 (the AnchorPath just filled in by MatchFirst/
; MatchNext). Builds a 3-longword VPrintf argarray in the data hunk. ---
print_entry:
        moveq   #0,d0
        move.b  AP_FLAGS_OFF(a5),d0      ; D0 = ap_Flags, zero-extended (a BYTE, not a longword)
        move.l  #argarray,a1
        move.l  d0,0(a1)                 ; argarray[0] = flags

        lea     FIB_FILENAME_OFF(a5),a0  ; A0 = &fib_FileName
        move.l  a0,4(a1)                 ; argarray[1] = fib_FileName ptr

        lea     AP_BUF_OFF(a5),a0        ; A0 = &ap_Buf
        move.l  a0,8(a1)                 ; argarray[2] = ap_Buf ptr

        move.l  a3,a6
        move.l  #fmt,d1
        move.l  a1,d2
        jsr     -954(a6)                 ; VPrintf(fmt, argarray)
        rts

usage:
        move.l  a3,a6
        move.l  #msg_usage,d1
        jsr     -948(a6)                 ; PutStr
        moveq   #0,d0
        rts

allocfail:
        move.l  a3,a6
        move.l  #msg_allocfail,d1
        jsr     -948(a6)                 ; PutStr
        moveq   #20,d0
        rts

matchfirstfail:
        move.l  a3,a6
        move.l  #msg_matchfirstfail,d1
        jsr     -948(a6)                 ; PutStr
        moveq   #10,d0
        rts

        section data,data

dosname:
        dc.b    "dos.library",0
        even

fmt:
        dc.b    "flags=%ld name='%s' buf='%s'",10,0
        even

msg_usage:
        dc.b    "usage: matchflags <dir>",10,0
        even
msg_allocfail:
        dc.b    "AllocMem failed",10,0
        even
msg_matchfirstfail:
        dc.b    "MatchFirst failed",10,0
        even

dirarg:
        dcb.b   256,0                    ; scratch: the one command-line token
argarray:
        dcb.b   12,0                     ; 3 longwords: flags, fib_FileName ptr, ap_Buf ptr
