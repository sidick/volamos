//! Generated `mathieeesingbas.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `mathieeesingbas.library` interface description
//! (`workbench/libs/mathieeesingbas/mathieeesingbas.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/c207aae1d67ac419553530b80eb62dcab2f923ee/workbench/libs/mathieeesingbas/mathieeesingbas.conf>
//! - Source commit: c207aae1d67ac419553530b80eb62dcab2f923ee
//! - Generated: 2026-09-12
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

/// The full `mathieeesingbas.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static MATHIEEESINGBAS_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "IEEESPFix",
        lvo: -30,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPFlt",
        lvo: -36,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPCmp",
        lvo: -42,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPTst",
        lvo: -48,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPAbs",
        lvo: -54,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPNeg",
        lvo: -60,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPAdd",
        lvo: -66,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPSub",
        lvo: -72,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPMul",
        lvo: -78,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPDiv",
        lvo: -84,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPFloor",
        lvo: -90,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPCeil",
        lvo: -96,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvos::find_by_name;

    // Sanity-check a handful of well-known LVOs against published AmigaOS
    // mathieeesingbas.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[
            ("IEEESPFix", -30),
            ("IEEESPFlt", -36),
            ("IEEESPFloor", -90),
            ("IEEESPCeil", -96),
        ];
        for (name, lvo) in cases {
            let entry = find_by_name(MATHIEEESINGBAS_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
