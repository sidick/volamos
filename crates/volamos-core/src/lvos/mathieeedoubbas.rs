//! Generated `mathieeedoubbas.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `mathieeedoubbas.library` interface description
//! (`workbench/libs/mathieeedoubbas/mathieeedoubbas.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/c207aae1d67ac419553530b80eb62dcab2f923ee/workbench/libs/mathieeedoubbas/mathieeedoubbas.conf>
//! - Source commit: c207aae1d67ac419553530b80eb62dcab2f923ee
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

use crate::cpu::DataRegister;
use crate::lvos::{ArgReg, LvoEntry};

/// The full `mathieeedoubbas.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static MATHIEEEDOUBBAS_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "IEEEDPFix",
        lvo: -30,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPFlt",
        lvo: -36,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPCmp",
        lvo: -42,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPTst",
        lvo: -48,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPAbs",
        lvo: -54,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPNeg",
        lvo: -60,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPAdd",
        lvo: -66,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPSub",
        lvo: -72,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPMul",
        lvo: -78,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPDiv",
        lvo: -84,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPFloor",
        lvo: -90,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPCeil",
        lvo: -96,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvos::find_by_name;

    // Sanity-check a handful of well-known LVOs against published AmigaOS
    // mathieeedoubbas.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[
            ("IEEEDPFix", -30),
            ("IEEEDPFlt", -36),
            ("IEEEDPFloor", -90),
            ("IEEEDPCeil", -96),
        ];
        for (name, lvo) in cases {
            let entry = find_by_name(MATHIEEEDOUBBAS_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
