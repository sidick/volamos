//! Generated `exec.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `exec.library` interface description
//! (`rom/exec/exec.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/d649ad4cd366bdcfe226ad70d5720c192cfe4653/rom/exec/exec.conf>
//! - Source commit: d649ad4cd366bdcfe226ad70d5720c192cfe4653
//! - Generated: 2026-08-18
//! - Generator: `tools/gen_lvos.py`
//!
//! Only uncopyrightable interface facts were extracted from the source --
//! function names, LVO offsets, and argument-register assignments -- as
//! bare data; no descriptive text, comments, or file structure from the
//! source was copied. This file is licensed under the same terms as the
//! rest of this repository: MIT OR Apache-2.0.
//!
//! DO NOT EDIT BY HAND. Regenerate with `tools/gen_lvos.py`.

use crate::cpu::{AddressRegister, DataRegister};
use crate::lvos::{ArgReg, LvoEntry};

/// The full `exec.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static EXEC_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "open",
        lvo: -6,
        args: &[ArgReg::D(DataRegister(0))],
        private: true,
    },
    LvoEntry {
        name: "close",
        lvo: -12,
        args: &[],
        private: true,
    },
    LvoEntry {
        name: "Supervisor",
        lvo: -30,
        args: &[ArgReg::A(AddressRegister(5))],
        private: false,
    },
    LvoEntry {
        name: "ExitIntr",
        lvo: -36,
        args: &[],
        private: true,
    },
    LvoEntry {
        name: "Schedule",
        lvo: -42,
        args: &[],
        private: true,
    },
    LvoEntry {
        name: "Reschedule",
        lvo: -48,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "Switch",
        lvo: -54,
        args: &[],
        private: true,
    },
    LvoEntry {
        name: "Dispatch",
        lvo: -60,
        args: &[],
        private: true,
    },
    LvoEntry {
        name: "Exception",
        lvo: -66,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "InitCode",
        lvo: -72,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "InitStruct",
        lvo: -78,
        args: &[
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "MakeLibrary",
        lvo: -84,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "MakeFunctions",
        lvo: -90,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "FindResident",
        lvo: -96,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "InitResident",
        lvo: -102,
        args: &[ArgReg::A(AddressRegister(1)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Alert",
        lvo: -108,
        args: &[ArgReg::D(DataRegister(7))],
        private: false,
    },
    LvoEntry {
        name: "Debug",
        lvo: -114,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Disable",
        lvo: -120,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "Enable",
        lvo: -126,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "Forbid",
        lvo: -132,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "Permit",
        lvo: -138,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "SetSR",
        lvo: -144,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SuperState",
        lvo: -150,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "UserState",
        lvo: -156,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SetIntVector",
        lvo: -162,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AddIntServer",
        lvo: -168,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RemIntServer",
        lvo: -174,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Cause",
        lvo: -180,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Allocate",
        lvo: -186,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Deallocate",
        lvo: -192,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AllocMem",
        lvo: -198,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AllocAbs",
        lvo: -204,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "FreeMem",
        lvo: -210,
        args: &[ArgReg::A(AddressRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AvailMem",
        lvo: -216,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AllocEntry",
        lvo: -222,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FreeEntry",
        lvo: -228,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Insert",
        lvo: -234,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AddHead",
        lvo: -240,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AddTail",
        lvo: -246,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Remove",
        lvo: -252,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RemHead",
        lvo: -258,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "RemTail",
        lvo: -264,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Enqueue",
        lvo: -270,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "FindName",
        lvo: -276,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AddTask",
        lvo: -282,
        args: &[
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "RemTask",
        lvo: -288,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "FindTask",
        lvo: -294,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SetTaskPri",
        lvo: -300,
        args: &[ArgReg::A(AddressRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SetSignal",
        lvo: -306,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SetExcept",
        lvo: -312,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Wait",
        lvo: -318,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Signal",
        lvo: -324,
        args: &[ArgReg::A(AddressRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AllocSignal",
        lvo: -330,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FreeSignal",
        lvo: -336,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AllocTrap",
        lvo: -342,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FreeTrap",
        lvo: -348,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AddPort",
        lvo: -354,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RemPort",
        lvo: -360,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "PutMsg",
        lvo: -366,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetMsg",
        lvo: -372,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ReplyMsg",
        lvo: -378,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "WaitPort",
        lvo: -384,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FindPort",
        lvo: -390,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AddLibrary",
        lvo: -396,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RemLibrary",
        lvo: -402,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "OldOpenLibrary",
        lvo: -408,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "CloseLibrary",
        lvo: -414,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SetFunction",
        lvo: -420,
        args: &[
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SumLibrary",
        lvo: -426,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AddDevice",
        lvo: -432,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RemDevice",
        lvo: -438,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "OpenDevice",
        lvo: -444,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "CloseDevice",
        lvo: -450,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "DoIO",
        lvo: -456,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SendIO",
        lvo: -462,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "CheckIO",
        lvo: -468,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "WaitIO",
        lvo: -474,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AbortIO",
        lvo: -480,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AddResource",
        lvo: -486,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RemResource",
        lvo: -492,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "OpenResource",
        lvo: -498,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RawIOInit",
        lvo: -504,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "RawMayGetChar",
        lvo: -510,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "RawPutChar",
        lvo: -516,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "RawDoFmt",
        lvo: -522,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "GetCC",
        lvo: -528,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "TypeOfMem",
        lvo: -534,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Procure",
        lvo: -540,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Vacate",
        lvo: -546,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "OpenLibrary",
        lvo: -552,
        args: &[ArgReg::A(AddressRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "InitSemaphore",
        lvo: -558,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ObtainSemaphore",
        lvo: -564,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ReleaseSemaphore",
        lvo: -570,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AttemptSemaphore",
        lvo: -576,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ObtainSemaphoreList",
        lvo: -582,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ReleaseSemaphoreList",
        lvo: -588,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FindSemaphore",
        lvo: -594,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AddSemaphore",
        lvo: -600,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RemSemaphore",
        lvo: -606,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SumKickData",
        lvo: -612,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "AddMemList",
        lvo: -618,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "CopyMem",
        lvo: -624,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "CopyMemQuick",
        lvo: -630,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "CacheClearU",
        lvo: -636,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "CacheClearE",
        lvo: -642,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "CacheControl",
        lvo: -648,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "CreateIORequest",
        lvo: -654,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "DeleteIORequest",
        lvo: -660,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "CreateMsgPort",
        lvo: -666,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "DeleteMsgPort",
        lvo: -672,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ObtainSemaphoreShared",
        lvo: -678,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AllocVec",
        lvo: -684,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "FreeVec",
        lvo: -690,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "CreatePool",
        lvo: -696,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "DeletePool",
        lvo: -702,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AllocPooled",
        lvo: -708,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FreePooled",
        lvo: -714,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AttemptSemaphoreShared",
        lvo: -720,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ColdReboot",
        lvo: -726,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "StackSwap",
        lvo: -732,
        args: &[ArgReg::A(AddressRegister(0))],
        private: true,
    },
    LvoEntry {
        name: "ChildFree",
        lvo: -738,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ChildOrphan",
        lvo: -744,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ChildStatus",
        lvo: -750,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ChildWait",
        lvo: -756,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "CachePreDMA",
        lvo: -762,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "CachePostDMA",
        lvo: -768,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AddMemHandler",
        lvo: -774,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RemMemHandler",
        lvo: -780,
        args: &[ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "ObtainQuickVector",
        lvo: -786,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "NewStackSwap",
        lvo: -804,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "TaggedOpenLibrary",
        lvo: -810,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ReadGayle",
        lvo: -816,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "VNewRawDoFmt",
        lvo: -822,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "NewMinList",
        lvo: -828,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AVL_AddNode",
        lvo: -852,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AVL_RemNodeByAddress",
        lvo: -858,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AVL_RemNodeByKey",
        lvo: -864,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AVL_FindNode",
        lvo: -870,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AVL_FindPrevNodeByAddress",
        lvo: -876,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AVL_FindPrevNodeByKey",
        lvo: -882,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AVL_FindNextNodeByAddress",
        lvo: -888,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AVL_FindNextNodeByKey",
        lvo: -894,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AVL_FindFirstNode",
        lvo: -900,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AVL_FindLastNode",
        lvo: -906,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "NewCreateTaskA",
        lvo: -918,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FindTaskByPID",
        lvo: -996,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AddResetCallback",
        lvo: -1002,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "RemResetCallback",
        lvo: -1008,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AllocVecPooled",
        lvo: -1014,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FreeVecPooled",
        lvo: -1020,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "ShutdownA",
        lvo: -1038,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "NewAllocEntry",
        lvo: -1044,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "NewAddTask",
        lvo: -1056,
        args: &[
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
            ArgReg::A(AddressRegister(4)),
        ],
        private: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvos::find_by_name;

    // Sanity-check a handful of well-known LVOs against published AmigaOS
    // exec.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[
            ("OpenLibrary", -552),
            ("OldOpenLibrary", -408),
            ("CloseLibrary", -414),
            ("AllocMem", -198),
            ("FreeMem", -210),
            ("FindTask", -294),
        ];
        for (name, lvo) in cases {
            let entry = find_by_name(EXEC_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
