//! Generated `icon.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `icon.library` interface description
//! (`workbench/libs/icon/icon.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/d57636f86361b34ba7652dd1d9699b4349788844/workbench/libs/icon/icon.conf>
//! - Source commit: d57636f86361b34ba7652dd1d9699b4349788844
//! - Generated: 2026-11-15
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

/// The full `icon.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static ICON_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "FreeFreeList",
        lvo: -54,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "AddFreeList",
        lvo: -72,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "GetDiskObject",
        lvo: -78,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "PutDiskObject",
        lvo: -84,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "FreeDiskObject",
        lvo: -90,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FindToolType",
        lvo: -96,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "MatchToolValue",
        lvo: -102,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "BumpRevision",
        lvo: -108,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetDefDiskObject",
        lvo: -120,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "PutDefDiskObject",
        lvo: -126,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "GetDiskObjectNew",
        lvo: -132,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "DeleteDiskObject",
        lvo: -138,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "DupDiskObjectA",
        lvo: -150,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IconControlA",
        lvo: -156,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "DrawIconStateA",
        lvo: -162,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::A(AddressRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "GetIconRectangleA",
        lvo: -168,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
            ArgReg::A(AddressRegister(4)),
        ],
        private: false,
    },
    LvoEntry {
        name: "NewDiskObject",
        lvo: -174,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "GetIconTagList",
        lvo: -180,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "PutIconTagList",
        lvo: -186,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "LayoutIconA",
        lvo: -192,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "ChangeToSelectedIconColor",
        lvo: -198,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvos::find_by_name;

    // Sanity-check a handful of well-known LVOs against published AmigaOS
    // icon.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[
            ("GetDiskObject", -78),
            ("GetDefDiskObject", -120),
            ("DeleteDiskObject", -138),
            ("ChangeToSelectedIconColor", -198),
        ];
        for (name, lvo) in cases {
            let entry = find_by_name(ICON_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
