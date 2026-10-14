; testlib_noauto.s -- fixture for volamos's non-RTF_AUTOINIT library
; loading (issue #133): an NT_LIBRARY `struct Resident` whose rt_Init is a
; code pointer called directly by InitResident (D0=0, A0=segList BPTR,
; A6=ExecBase), not an AUTOINIT table. The init routine builds the library
; itself with MakeLibrary (with an InitStruct table for the Node/version
; fields) and AddLibrary, then returns the base in D0 -- the shape of
; real Kickstart modules such as graphics.library.
;
; One source, three binaries, selected by the VARIANT symbol:
;
;   VARIANT=0  testlib_noauto           MakeLibrary + AddLibrary, returns base
;   VARIANT=1  testlib_noauto_nullret   MakeLibrary + AddLibrary, but returns
;                                       NULL (volamos must find the added
;                                       library by its Resident name)
;   VARIANT=2  testlib_noauto_refuse    MakeLibrary, then frees it and returns
;                                       NULL without AddLibrary (init refusal)
;   VARIANT=3  testlib_noauto_keep      like 0, but the last Close leaves the
;                                       library resident (only RemLibrary,
;                                       via the Expunge vector, removes it)
;
; The library's Close vector (on the last close) and its Expunge vector (when
; not open; otherwise it returns 0) implement the real expunge idiom for a
; library that owns its base: Remove() its own node, FreeMem() base-NegSize..
; itself, and return the segList that init stored at SEGLIST_MARKER_OFFSET.
;
; Built with PhxAss run under volamos itself (no vasm needed):
;
;   volamos --cwd work: -V work:fixtures ~/amiga/PhxAss/PhxAss \
;       work:testlib_noauto.s SET VARIANT=0 TO work:testlib_noauto
;   ... SET VARIANT=1 TO work:testlib_noauto_nullret
;   ... SET VARIANT=2 TO work:testlib_noauto_refuse
;   ... SET VARIANT=3 TO work:testlib_noauto_keep

LIB_NEGSIZE_OFFSET     equ     16
LIB_POSSIZE_OFFSET     equ     18
LIB_OPENCNT_OFFSET     equ     32
SEGLIST_MARKER_OFFSET  equ     34
LIB_DATA_SIZE          equ     38      ; struct Library (34) + segList longword
_LVORemove             equ     -252
_LVOFreeMem            equ     -210
_LVOMakeLibrary        equ     -84
_LVOAddLibrary         equ     -396

        section code,code

start:
        moveq   #0,d0
        rts

Resident:
        dc.w    $4AFC                  ; RTC_MATCHWORD
        dc.l    Resident                ; RT_MATCHTAG
        dc.l    EndCode                 ; RT_ENDSKIP
        dc.b    0                       ; RT_FLAGS = 0: NOT RTF_AUTOINIT
        dc.b    1                       ; RT_VERSION
        dc.b    9                       ; RT_TYPE = NT_LIBRARY
        dc.b    0                       ; RT_PRI
        dc.l    LibName                 ; RT_NAME
        dc.l    LibIdString             ; RT_IDSTRING
        dc.l    Init                    ; RT_INIT = code, called directly

Init:
        ; D0=0, A0=segList (BPTR), A6=ExecBase.
        movem.l d2/a2/a6,-(sp)
        move.l  a0,d2                   ; d2 = segList
        lea     VecTable(pc),a0         ; vectors (absolute-pointer form)
        lea     InitTab(pc),a1          ; InitStruct table
        sub.l   a2,a2                   ; init routine = NULL
        move.l  #LIB_DATA_SIZE,d0
        move.l  d2,d1                   ; segList
        jsr     _LVOMakeLibrary(a6)
        tst.l   d0
        beq.s   InitDone
        move.l  d0,a2                   ; a2 = base
        move.l  d2,SEGLIST_MARKER_OFFSET(a2)
        IFEQ    VARIANT-2
        bsr.s   FreeBase
        moveq   #0,d0
        ELSE
        move.l  a2,a1
        jsr     _LVOAddLibrary(a6)
        IFEQ    VARIANT-1
        moveq   #0,d0
        ELSE
        move.l  a2,d0
        ENDC
        ENDC
InitDone:
        movem.l (sp)+,d2/a2/a6
        rts

; Frees the library at a2 (NegSize+PosSize bytes from base-NegSize). a6=ExecBase.
FreeBase:
        move.l  a2,a1
        moveq   #0,d0
        move.w  LIB_NEGSIZE_OFFSET(a2),d0
        sub.l   d0,a1
        moveq   #0,d1
        move.w  LIB_POSSIZE_OFFSET(a2),d1
        add.l   d1,d0
        jsr     _LVOFreeMem(a6)
        rts

OpenFunc:
        addq.w  #1,LIB_OPENCNT_OFFSET(a6)
        move.l  a6,d0
        rts

CloseFunc:
        subq.w  #1,LIB_OPENCNT_OFFSET(a6)
        bne.s   CloseStillOpen
        IFEQ    VARIANT-3
        bra.s   CloseStillOpen          ; keep variant: stay resident, expunge only on RemLibrary
        ELSE
        ; Last close: expunge ourselves, as a real library's Expunge does.
        bra.s   DoExpunge
        ENDC
CloseStillOpen:
        moveq   #0,d0
        rts

; Expunge: refuse (D0=0) while open, otherwise remove ourselves.
ExpungeFunc:
        tst.w   LIB_OPENCNT_OFFSET(a6)
        bne.s   CloseStillOpen
DoExpunge:
        ; a6 = our base. Removes the node, frees the base, returns the segList.
        movem.l d2/a2/a6,-(sp)
        move.l  a6,a2                   ; a2 = our base
        move.l  SEGLIST_MARKER_OFFSET(a2),d2
        move.l  4.w,a6                  ; ExecBase
        move.l  a2,a1
        jsr     _LVORemove(a6)
        bsr.s   FreeBase
        move.l  d2,d0                   ; return the segList
        movem.l (sp)+,d2/a2/a6
        rts

ReservedFunc:
        moveq   #0,d0
        rts

UserFunc:
        moveq   #42,d0
        rts

        even
VecTable:
        dc.l    OpenFunc
        dc.l    CloseFunc
        dc.l    ExpungeFunc
        dc.l    ReservedFunc
        dc.l    UserFunc
        dc.l    -1

        ; InitStruct table (exec/initializers.i encodings).
        even
InitTab:
        dc.b    $a0,8,9,0               ; INITBYTE LN_TYPE,NT_LIBRARY
        dc.b    $80,10                  ; INITLONG LN_NAME,LibName
        dc.l    LibName
        dc.b    $90,20                  ; INITWORD LIB_VERSION,1
        dc.w    1
        dc.b    $90,22                  ; INITWORD LIB_REVISION,7
        dc.w    7
        dc.b    $80,24                  ; INITLONG LIB_IDSTRING,LibIdString
        dc.l    LibIdString
        dc.w    0

LibName:
        dc.b    "noauto.library",0
        even
LibIdString:
        dc.b    "noauto.library 1.7",0
        even
EndCode:
