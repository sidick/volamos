; testlib_initfail.s -- Phase L3 fixture for volamos: a second tiny
; RTF_AUTOINIT library, identical in shape to testlib.s, except its
; initFunc unconditionally refuses the open (`moveq #0,d0 ; rts` -- NULL).
; Exercises execlib.rs's `after_init` NULL-init-result path: the seglist
; must be unloaded and the make_library allocation freed, with nothing
; registered, so a later OpenLibrary of the same name gets a completely
; fresh attempt.
;
; Regenerating:
;   vasmm68k_mot -Fhunkexe -nosym -o fixtures/testlib_initfail fixtures/testlib_initfail.s

LIB_DATA_SIZE   equ     34      ; == sizeof(struct Library), no extra markers needed

        section code

start:
        moveq   #0,d0
        rts

Resident:
        dc.w    $4AFC
        dc.l    Resident
        dc.l    EndCode
        dc.b    $80                      ; RTF_AUTOINIT
        dc.b    1
        dc.b    9                        ; NT_LIBRARY
        dc.b    0
        dc.l    LibName
        dc.l    LibIdString
        dc.l    AutoInitTab

AutoInitTab:
        dc.l    LIB_DATA_SIZE
        dc.l    VecTable
        dc.l    0
        dc.l    InitFunc

VecTable:
        dc.l    OpenFunc
        dc.l    CloseFunc
        dc.l    ExpungeFunc
        dc.l    ReservedFunc
        dc.l    -1

InitFunc:
        ; Unconditionally refuses the open, per real MakeLibrary's own
        ; contract ("initFunction... returns NULL if it fails").
        moveq   #0,d0
        rts

OpenFunc:
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

LibName:
        dc.b    "initfail.library",0
        even
LibIdString:
        dc.b    "initfail.library 1.0",0
        even
EndCode:
