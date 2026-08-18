//! Generated `utility.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `utility.library` interface description
//! (`rom/utility/utility.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/d649ad4cd366bdcfe226ad70d5720c192cfe4653/rom/utility/utility.conf>
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

/// The full `utility.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static UTILITY_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "FindTagItem",
        lvo: -30,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "GetTagData",
        lvo: -36,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::A(AddressRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "PackBoolTags",
        lvo: -42,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "NextTagItem",
        lvo: -48,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FilterTagChanges",
        lvo: -54,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "MapTags",
        lvo: -60,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AllocateTagItems",
        lvo: -66,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "CloneTagItems",
        lvo: -72,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FreeTagItems",
        lvo: -78,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "RefreshTagItemClones",
        lvo: -84,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "TagInArray",
        lvo: -90,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FilterTagItems",
        lvo: -96,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "CallHookPkt",
        lvo: -102,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "Amiga2Date",
        lvo: -120,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Date2Amiga",
        lvo: -126,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "CheckDate",
        lvo: -132,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SMult32",
        lvo: -138,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "UMult32",
        lvo: -144,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SDivMod32",
        lvo: -150,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "UDivMod32",
        lvo: -156,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Stricmp",
        lvo: -162,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "Strnicmp",
        lvo: -168,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "ToUpper",
        lvo: -174,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ToLower",
        lvo: -180,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ApplyTagChanges",
        lvo: -186,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SMult64",
        lvo: -198,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "UMult64",
        lvo: -204,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "PackStructureTags",
        lvo: -210,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "UnpackStructureTags",
        lvo: -216,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "AddNamedObject",
        lvo: -222,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AllocNamedObjectA",
        lvo: -228,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "AttemptRemNamedObject",
        lvo: -234,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FindNamedObject",
        lvo: -240,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "FreeNamedObject",
        lvo: -246,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "NamedObjectName",
        lvo: -252,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ReleaseNamedObject",
        lvo: -258,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "RemNamedObject",
        lvo: -264,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetUniqueID",
        lvo: -270,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "Strlcpy",
        lvo: -300,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "Strlcat",
        lvo: -306,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "VSNPrintf",
        lvo: -312,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "SetMem",
        lvo: -396,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvos::find_by_name;

    // Sanity-check a handful of well-known LVOs against published AmigaOS
    // utility.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[
            ("FindTagItem", -30),
            ("GetTagData", -36),
            ("NextTagItem", -48),
            ("Stricmp", -162),
            ("Strnicmp", -168),
            ("Amiga2Date", -120),
            ("Date2Amiga", -126),
            ("CheckDate", -132),
        ];
        for (name, lvo) in cases {
            let entry = find_by_name(UTILITY_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
