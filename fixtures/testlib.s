; testlib.s -- Phase L3 fixture for volamos: a tiny hand-authored,
; genuinely RTF_AUTOINIT `struct Resident` library (vasm mot syntax),
; built to be `LoadSeg`ed and opened via the real disk-library-loading
; path (library-device-loading-plan.md, phase L3), not run directly as
; a CLI program.
;
; Layout (all in one CODE hunk -- vasm emits self-hunk HUNK_RELOC32
; fixups for the absolute `dc.l Label` references below, exactly as it
; would for any other absolute pointer; find_resident/read_resident/
; read_autoinit/read_vectors in execlib.rs only ever look at already-
; relocated guest memory, so which hunk a pointer's target lives in is
; irrelevant to them):
;
;   start        -- a harmless "safety net" instruction ahead of the
;                    Resident, matching the real-disk-library convention
;                    execlib.rs's module docs describe (never executed
;                    by anything this fixture's tests do).
;   Resident     -- a real struct Resident, RTF_AUTOINIT set, NT_LIBRARY.
;   AutoInitTab  -- the MakeLibrary(vectors, structure, init, dSize) args.
;   VecTable     -- Open/Close/Expunge/Reserved/UserFunc, absolute-
;                    pointer form (terminated by -1), the same encoding
;                    the real scspill.library uses.
;   InitFunc     -- D0=libBase, A0=segList (BPTR), A6=ExecBase per the
;                    AUTOINIT calling convention. Proves it really ran by
;                    writing a marker word into lib_Revision, proves A0/D0
;                    were passed correctly by storing them into two extra
;                    data cells past struct Library's own 34 bytes, and
;                    proves the trampoline supports *nested* library calls
;                    (the plan's L2 raison d'etre) by calling AllocMem via
;                    A6=ExecBase mid-init and storing its result too.
;                    Returns the original libBase in D0, as required.
;   OpenFunc     -- increments lib_OpenCnt (a real library's own job, not
;                    volamos's -- library-device-loading-plan.md §2.4) and
;                    returns A6 (the base) in D0.
;   CloseFunc/ExpungeFunc/ReservedFunc -- trivial, return 0.
;   UserFunc     -- the first user vector (LVO -30): `moveq #42,d0 ; rts`,
;                    executed *natively* by the CPU backend once called --
;                    no host dispatch at all, the whole architectural
;                    point of loading a real library (plan §1.4).
;
; Regenerating: `vasmm68k_mot -Fhunkexe -nosym -o fixtures/testlib fixtures/testlib.s`

LIB_REVISION_OFFSET    equ     22
LIB_OPENCNT_OFFSET     equ     32
SEGLIST_MARKER_OFFSET  equ     36
ALLOCMEM_MARKER_OFFSET equ     40
LIB_DATA_SIZE          equ     44      ; >= 34 (struct Library) + 2 marker longwords
INIT_MARKER            equ     $2A2A
_LVOAllocMem           equ     -198

        section code

start:
        moveq   #0,d0
        rts

Resident:
        dc.w    $4AFC                  ; RTC_MATCHWORD
        dc.l    Resident                ; RT_MATCHTAG
        dc.l    EndCode                 ; RT_ENDSKIP (unused by execlib.rs)
        dc.b    $80                     ; RT_FLAGS = RTF_AUTOINIT
        dc.b    1                       ; RT_VERSION
        dc.b    9                       ; RT_TYPE = NT_LIBRARY
        dc.b    0                       ; RT_PRI
        dc.l    LibName                 ; RT_NAME
        dc.l    LibIdString              ; RT_IDSTRING
        dc.l    AutoInitTab              ; RT_INIT

AutoInitTab:
        dc.l    LIB_DATA_SIZE            ; dSize
        dc.l    VecTable                 ; vectors (absolute-pointer form)
        dc.l    0                        ; structure (NULL -- no InitStruct)
        dc.l    InitFunc                 ; initFunc

VecTable:
        dc.l    OpenFunc
        dc.l    CloseFunc
        dc.l    ExpungeFunc
        dc.l    ReservedFunc
        dc.l    UserFunc
        dc.l    -1                       ; terminator

InitFunc:
        ; D0=libBase, A0=segList (BPTR), A6=ExecBase.
        movem.l d0/a0/a6,-(sp)
        move.l  d0,a1                    ; a1 = libBase, kept across the call below
        move.w  #INIT_MARKER,LIB_REVISION_OFFSET(a1)
        move.l  a0,SEGLIST_MARKER_OFFSET(a1)
        moveq   #4,d0                    ; AllocMem(4, MEMF_ANY) -- proves the
        moveq   #0,d1                    ; trampoline supports a nested library
        jsr     _LVOAllocMem(a6)         ; call mid-initFunc (plan's L2 point)
        move.l  d0,ALLOCMEM_MARKER_OFFSET(a1)
        movem.l (sp)+,d0/a0/a6           ; restore original D0 (libBase)/A0/A6
        rts                              ; return libBase in D0, per MakeLibrary's contract

OpenFunc:
        addq.w  #1,LIB_OPENCNT_OFFSET(a6)
        move.l  a6,d0
        rts

CloseFunc:
        moveq   #0,d0
        rts

ExpungeFunc:
        moveq   #0,d0
        rts

ReservedFunc:
        moveq   #0,d0
        rts

UserFunc:
        moveq   #42,d0
        rts

LibName:
        dc.b    "test.library",0
        even
LibIdString:
        dc.b    "test.library 1.0",0
        even
EndCode:
