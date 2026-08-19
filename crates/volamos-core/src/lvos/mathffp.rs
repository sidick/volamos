//! Generated `mathffp.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `mathffp.library` interface description
//! (`workbench/libs/mathffp/mathffp.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/bfc27c04c63b89288a1ef066cbdd370dc4fc7130/workbench/libs/mathffp/mathffp.conf>
//! - Source commit: bfc27c04c63b89288a1ef066cbdd370dc4fc7130
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

/// The full `mathffp.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static MATHFFP_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "SPFix",
        lvo: -30,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPFlt",
        lvo: -36,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPCmp",
        lvo: -42,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPTst",
        lvo: -48,
        args: &[ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "SPAbs",
        lvo: -54,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPNeg",
        lvo: -60,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPAdd",
        lvo: -66,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPSub",
        lvo: -72,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPMul",
        lvo: -78,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPDiv",
        lvo: -84,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPFloor",
        lvo: -90,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPCeil",
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
    // mathffp.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[("SPAdd", -66), ("SPCeil", -96)];
        for (name, lvo) in cases {
            let entry = find_by_name(MATHFFP_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
