//! Generated `locale.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `locale.library` interface description
//! (`workbench/libs/locale/locale.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/d57636f86361b34ba7652dd1d9699b4349788844/workbench/libs/locale/locale.conf>
//! - Source commit: d57636f86361b34ba7652dd1d9699b4349788844
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

/// The full `locale.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static LOCALE_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "CloseCatalog",
        lvo: -36,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "CloseLocale",
        lvo: -42,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ConvToLower",
        lvo: -48,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ConvToUpper",
        lvo: -54,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "FormatDate",
        lvo: -60,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "FormatString",
        lvo: -66,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "GetCatalogStr",
        lvo: -72,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "GetLocaleStr",
        lvo: -78,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsAlNum",
        lvo: -84,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsAlpha",
        lvo: -90,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsCntrl",
        lvo: -96,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsDigit",
        lvo: -102,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsGraph",
        lvo: -108,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsLower",
        lvo: -114,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsPrint",
        lvo: -120,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsPunct",
        lvo: -126,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsSpace",
        lvo: -132,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsUpper",
        lvo: -138,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IsXDigit",
        lvo: -144,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "OpenCatalogA",
        lvo: -150,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "OpenLocale",
        lvo: -156,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "ParseDate",
        lvo: -162,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "LocalePrefsUpdate",
        lvo: -168,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "StrConvert",
        lvo: -174,
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
        name: "StrnCmp",
        lvo: -180,
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
        name: "LocRawDoFmt",
        lvo: -186,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
        ],
        private: true,
    },
    LvoEntry {
        name: "LocStrnicmp",
        lvo: -192,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(0)),
        ],
        private: true,
    },
    LvoEntry {
        name: "LocStricmp",
        lvo: -198,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: true,
    },
    LvoEntry {
        name: "LocToLower",
        lvo: -204,
        args: &[ArgReg::D(DataRegister(0))],
        private: true,
    },
    LvoEntry {
        name: "LocToUpper",
        lvo: -210,
        args: &[ArgReg::D(DataRegister(0))],
        private: true,
    },
    LvoEntry {
        name: "LocDateToStr",
        lvo: -216,
        args: &[ArgReg::D(DataRegister(1))],
        private: true,
    },
    LvoEntry {
        name: "LocStrToDate",
        lvo: -222,
        args: &[ArgReg::D(DataRegister(1))],
        private: true,
    },
    LvoEntry {
        name: "LocDosGetLocalizedString",
        lvo: -228,
        args: &[ArgReg::D(DataRegister(1))],
        private: true,
    },
    LvoEntry {
        name: "LocVNewRawDoFmt",
        lvo: -234,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: true,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvos::find_by_name;

    // Sanity-check a handful of well-known LVOs against published AmigaOS
    // locale.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[
            ("CloseCatalog", -36),
            ("IsUpper", -138),
            ("OpenLocale", -156),
            ("StrnCmp", -180),
        ];
        for (name, lvo) in cases {
            let entry = find_by_name(LOCALE_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
