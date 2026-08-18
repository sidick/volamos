//! Generated `dos.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `dos.library` interface description
//! (`rom/dos/dos.conf`, the `##begin functionlist` block AROS's own build
//! generates `dos_lib.sfd`/`dos_lib.fd` from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/d649ad4cd366bdcfe226ad70d5720c192cfe4653/rom/dos/dos.conf>
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

/// The full `dos.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static DOS_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "OpenLib",
        lvo: -6,
        args: &[ArgReg::D(DataRegister(0))],
        private: true,
    },
    LvoEntry {
        name: "CloseLib",
        lvo: -12,
        args: &[],
        private: true,
    },
    LvoEntry {
        name: "Open",
        lvo: -30,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "Close",
        lvo: -36,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Read",
        lvo: -42,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "Write",
        lvo: -48,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "Input",
        lvo: -54,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "Output",
        lvo: -60,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "Seek",
        lvo: -66,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "DeleteFile",
        lvo: -72,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Rename",
        lvo: -78,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "Lock",
        lvo: -84,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "UnLock",
        lvo: -90,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "DupLock",
        lvo: -96,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Examine",
        lvo: -102,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "ExNext",
        lvo: -108,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "Info",
        lvo: -114,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "CreateDir",
        lvo: -120,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "CurrentDir",
        lvo: -126,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IoErr",
        lvo: -132,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "CreateProc",
        lvo: -138,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "Exit",
        lvo: -144,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "LoadSeg",
        lvo: -150,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "UnLoadSeg",
        lvo: -156,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "DeviceProc",
        lvo: -174,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SetComment",
        lvo: -180,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "SetProtection",
        lvo: -186,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "DateStamp",
        lvo: -192,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Delay",
        lvo: -198,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "WaitForChar",
        lvo: -204,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "ParentDir",
        lvo: -210,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IsInteractive",
        lvo: -216,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Execute",
        lvo: -222,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AllocDosObject",
        lvo: -228,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "FreeDosObject",
        lvo: -234,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "DoPkt",
        lvo: -240,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
            ArgReg::D(DataRegister(5)),
            ArgReg::D(DataRegister(6)),
            ArgReg::D(DataRegister(7)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SendPkt",
        lvo: -246,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "WaitPkt",
        lvo: -252,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "ReplyPkt",
        lvo: -258,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AbortPkt",
        lvo: -264,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "LockRecord",
        lvo: -270,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
            ArgReg::D(DataRegister(5)),
        ],
        private: false,
    },
    LvoEntry {
        name: "LockRecords",
        lvo: -276,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "UnLockRecord",
        lvo: -282,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "UnLockRecords",
        lvo: -288,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SelectInput",
        lvo: -294,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SelectOutput",
        lvo: -300,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "FGetC",
        lvo: -306,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "FPutC",
        lvo: -312,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "UnGetC",
        lvo: -318,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "FRead",
        lvo: -324,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "FWrite",
        lvo: -330,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "FGets",
        lvo: -336,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "FPuts",
        lvo: -342,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "VFWritef",
        lvo: -348,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "VFPrintf",
        lvo: -354,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "Flush",
        lvo: -360,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SetVBuf",
        lvo: -366,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "DupLockFromFH",
        lvo: -372,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "OpenFromLock",
        lvo: -378,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "ParentOfFH",
        lvo: -384,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "ExamineFH",
        lvo: -390,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "SetFileDate",
        lvo: -396,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "NameFromLock",
        lvo: -402,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "NameFromFH",
        lvo: -408,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SplitName",
        lvo: -414,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
            ArgReg::D(DataRegister(5)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SameLock",
        lvo: -420,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "SetMode",
        lvo: -426,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "ExAll",
        lvo: -432,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
            ArgReg::D(DataRegister(5)),
        ],
        private: false,
    },
    LvoEntry {
        name: "ReadLink",
        lvo: -438,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
            ArgReg::D(DataRegister(5)),
        ],
        private: false,
    },
    LvoEntry {
        name: "MakeLink",
        lvo: -444,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "ChangeMode",
        lvo: -450,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SetFileSize",
        lvo: -456,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SetIoErr",
        lvo: -462,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Fault",
        lvo: -468,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "PrintFault",
        lvo: -474,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "ErrorReport",
        lvo: -480,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "DisplayError",
        lvo: -486,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "Cli",
        lvo: -492,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "CreateNewProc",
        lvo: -498,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RunCommand",
        lvo: -504,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "GetConsoleTask",
        lvo: -510,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "SetConsoleTask",
        lvo: -516,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetFileSysTask",
        lvo: -522,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "SetFileSysTask",
        lvo: -528,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetArgStr",
        lvo: -534,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "SetArgStr",
        lvo: -540,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "FindCliProc",
        lvo: -546,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "MaxCli",
        lvo: -552,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "SetCurrentDirName",
        lvo: -558,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetCurrentDirName",
        lvo: -564,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "SetProgramName",
        lvo: -570,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetProgramName",
        lvo: -576,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "SetPrompt",
        lvo: -582,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetPrompt",
        lvo: -588,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "SetProgramDir",
        lvo: -594,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetProgramDir",
        lvo: -600,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "SystemTagList",
        lvo: -606,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "AssignLock",
        lvo: -612,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "AssignLate",
        lvo: -618,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "AssignPath",
        lvo: -624,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "AssignAdd",
        lvo: -630,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "RemAssignList",
        lvo: -636,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "GetDeviceProc",
        lvo: -642,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "FreeDeviceProc",
        lvo: -648,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "LockDosList",
        lvo: -654,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "UnLockDosList",
        lvo: -660,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AttemptLockDosList",
        lvo: -666,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "RemDosEntry",
        lvo: -672,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AddDosEntry",
        lvo: -678,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "FindDosEntry",
        lvo: -684,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "NextDosEntry",
        lvo: -690,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "MakeDosEntry",
        lvo: -696,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "FreeDosEntry",
        lvo: -702,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IsFileSystem",
        lvo: -708,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Format",
        lvo: -714,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "Relabel",
        lvo: -720,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "Inhibit",
        lvo: -726,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "AddBuffers",
        lvo: -732,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "CompareDates",
        lvo: -738,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "DateToStr",
        lvo: -744,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "StrToDate",
        lvo: -750,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "InternalLoadSeg",
        lvo: -756,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "InternalUnLoadSeg",
        lvo: -762,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "NewLoadSeg",
        lvo: -768,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "AddSegment",
        lvo: -774,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "FindSegment",
        lvo: -780,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "RemSegment",
        lvo: -786,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "CheckSignal",
        lvo: -792,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "ReadArgs",
        lvo: -798,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "FindArg",
        lvo: -804,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "ReadItem",
        lvo: -810,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "StrToLong",
        lvo: -816,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "MatchFirst",
        lvo: -822,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "MatchNext",
        lvo: -828,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "MatchEnd",
        lvo: -834,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "ParsePattern",
        lvo: -840,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "MatchPattern",
        lvo: -846,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "FreeArgs",
        lvo: -858,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "FilePart",
        lvo: -870,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "PathPart",
        lvo: -876,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AddPart",
        lvo: -882,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "StartNotify",
        lvo: -888,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "EndNotify",
        lvo: -894,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SetVar",
        lvo: -900,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "GetVar",
        lvo: -906,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "DeleteVar",
        lvo: -912,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "FindVar",
        lvo: -918,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "CliInit",
        lvo: -924,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "CliInitNewcli",
        lvo: -930,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "CliInitRun",
        lvo: -936,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "WriteChars",
        lvo: -942,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "PutStr",
        lvo: -948,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "VPrintf",
        lvo: -954,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "ParsePatternNoCase",
        lvo: -966,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "MatchPatternNoCase",
        lvo: -972,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "DosGetString",
        lvo: -978,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SameDevice",
        lvo: -984,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "ExAllEnd",
        lvo: -990,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
            ArgReg::D(DataRegister(5)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SetOwner",
        lvo: -996,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))],
        private: false,
    },
    LvoEntry {
        name: "ScanVars",
        lvo: -1014,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "GetSegListInfo",
        lvo: -1176,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AssignAddToList",
        lvo: -1356,
        args: &[
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvos::find_by_name;

    // Sanity-check a handful of well-known LVOs against published AmigaOS
    // dos.library values (see docs/plan.md's T7 entry).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[
            ("Open", -30),
            ("Close", -36),
            ("Read", -42),
            ("Write", -48),
            ("Input", -54),
            ("Output", -60),
            ("Seek", -66),
            ("Lock", -84),
            ("Examine", -102),
            ("ExNext", -108),
            ("CurrentDir", -126),
            ("IoErr", -132),
            ("ParentDir", -210),
            ("PutStr", -948),
        ];
        for (name, lvo) in cases {
            let entry = find_by_name(DOS_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }

    #[test]
    fn open_and_lock_take_d1_d2() {
        let open = find_by_name(DOS_LVOS, "Open").unwrap();
        assert_eq!(
            open.args,
            &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))]
        );
        let lock = find_by_name(DOS_LVOS, "Lock").unwrap();
        assert_eq!(
            lock.args,
            &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(2))]
        );
        let putstr = find_by_name(DOS_LVOS, "PutStr").unwrap();
        assert_eq!(putstr.args, &[ArgReg::D(DataRegister(1))]);
    }
}
