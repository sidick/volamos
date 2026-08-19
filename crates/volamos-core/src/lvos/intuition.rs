//! Generated `intuition.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `intuition.library` interface description
//! (`rom/intuition/intuition.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/20059e387a243743def47d12d3c4156031deae2f/rom/intuition/intuition.conf>
//! - Source commit: 20059e387a243743def47d12d3c4156031deae2f
//! - Generated: 2026-08-19
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

/// The full `intuition.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static INTUITION_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "AddGadget",
        lvo: -42,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "ClearDMRequest",
        lvo: -48,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ClearMenuStrip",
        lvo: -54,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ClearPointer",
        lvo: -60,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "CloseScreen",
        lvo: -66,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "CloseWindow",
        lvo: -72,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "CloseWorkBench",
        lvo: -78,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "CurrentTime",
        lvo: -84,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "DisplayAlert",
        lvo: -90,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "DisplayBeep",
        lvo: -96,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "DoubleClick",
        lvo: -102,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "DrawBorder",
        lvo: -108,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "DrawImage",
        lvo: -114,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "EndRequest",
        lvo: -120,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetDefPrefs",
        lvo: -126,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "GetPrefs",
        lvo: -132,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "InitRequester",
        lvo: -138,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ItemAddress",
        lvo: -144,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ModifyIDCMP",
        lvo: -150,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ModifyProp",
        lvo: -156,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "MoveScreen",
        lvo: -162,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "MoveWindow",
        lvo: -168,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "OffGadget",
        lvo: -174,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "OffMenu",
        lvo: -180,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "OnGadget",
        lvo: -186,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "OnMenu",
        lvo: -192,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "OpenScreen",
        lvo: -198,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "OpenWindow",
        lvo: -204,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "OpenWorkBench",
        lvo: -210,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "PrintIText",
        lvo: -216,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "RefreshGadgets",
        lvo: -222,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "RemoveGadget",
        lvo: -228,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "ReportMouse",
        lvo: -234,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Request",
        lvo: -240,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "ScreenToBack",
        lvo: -246,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ScreenToFront",
        lvo: -252,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SetDMRequest",
        lvo: -258,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SetMenuStrip",
        lvo: -264,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SetPointer",
        lvo: -270,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SetWindowTitles",
        lvo: -276,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "ShowTitle",
        lvo: -282,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SizeWindow",
        lvo: -288,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "ViewAddress",
        lvo: -294,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "ViewPortAddress",
        lvo: -300,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "WindowToBack",
        lvo: -306,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "WindowToFront",
        lvo: -312,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "WindowLimits",
        lvo: -318,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SetPrefs",
        lvo: -324,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "IntuiTextLength",
        lvo: -330,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "WBenchToBack",
        lvo: -336,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "WBenchToFront",
        lvo: -342,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "AutoRequest",
        lvo: -348,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "BeginRefresh",
        lvo: -354,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "BuildSysRequest",
        lvo: -360,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "EndRefresh",
        lvo: -366,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FreeSysRequest",
        lvo: -372,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "MakeScreen",
        lvo: -378,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "RemakeDisplay",
        lvo: -384,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "RethinkDisplay",
        lvo: -390,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "AllocRemember",
        lvo: -396,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AlohaWorkbench",
        lvo: -402,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FreeRemember",
        lvo: -408,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "LockIBase",
        lvo: -414,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "UnlockIBase",
        lvo: -420,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "GetScreenData",
        lvo: -426,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "RefreshGList",
        lvo: -432,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AddGList",
        lvo: -438,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "RemoveGList",
        lvo: -444,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "ActivateWindow",
        lvo: -450,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "RefreshWindowFrame",
        lvo: -456,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ActivateGadget",
        lvo: -462,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "NewModifyProp",
        lvo: -468,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
            ArgReg::D(DataRegister(4)),
            ArgReg::D(DataRegister(5)),
        ],
        private: false,
    },
    LvoEntry {
        name: "QueryOverscan",
        lvo: -474,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "MoveWindowInFrontOf",
        lvo: -480,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "ChangeWindowBox",
        lvo: -486,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SetEditHook",
        lvo: -492,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SetMouseQueue",
        lvo: -498,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ZipWindow",
        lvo: -504,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "LockPubScreen",
        lvo: -510,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "UnlockPubScreen",
        lvo: -516,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "LockPubScreenList",
        lvo: -522,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "UnlockPubScreenList",
        lvo: -528,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "NextPubScreen",
        lvo: -534,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SetDefaultPubScreen",
        lvo: -540,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SetPubScreenModes",
        lvo: -546,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "PubScreenStatus",
        lvo: -552,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ObtainGIRPort",
        lvo: -558,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ReleaseGIRPort",
        lvo: -564,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "GadgetMouse",
        lvo: -570,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SetIPrefs",
        lvo: -576,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "GetDefaultPubScreen",
        lvo: -582,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "EasyRequestArgs",
        lvo: -588,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "BuildEasyRequestArgs",
        lvo: -594,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SysReqHandler",
        lvo: -600,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvos::find_by_name;

    // Sanity-check a handful of well-known LVOs against published AmigaOS
    // intuition.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[
            ("CurrentTime", -84),
            ("DisplayAlert", -90),
            ("AutoRequest", -348),
            ("EasyRequestArgs", -588),
        ];
        for (name, lvo) in cases {
            let entry = find_by_name(INTUITION_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
