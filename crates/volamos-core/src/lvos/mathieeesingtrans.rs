//! Generated `mathieeesingtrans.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `mathieeesingtrans.library` interface description
//! (`workbench/libs/mathieeesingtrans/mathieeesingtrans.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/c207aae1d67ac419553530b80eb62dcab2f923ee/workbench/libs/mathieeesingtrans/mathieeesingtrans.conf>
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

use crate::cpu::{AddressRegister, DataRegister};
use crate::lvos::{ArgReg, LvoEntry};

/// The full `mathieeesingtrans.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static MATHIEEESINGTRANS_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "IEEESPAtan",
        lvo: -30,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPSin",
        lvo: -36,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPCos",
        lvo: -42,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPTan",
        lvo: -48,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPSincos",
        lvo: -54,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPSinh",
        lvo: -60,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPCosh",
        lvo: -66,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPTanh",
        lvo: -72,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPExp",
        lvo: -78,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPLog",
        lvo: -84,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPPow",
        lvo: -90,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPSqrt",
        lvo: -96,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPTieee",
        lvo: -102,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPFieee",
        lvo: -108,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPAsin",
        lvo: -114,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPAcos",
        lvo: -120,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEESPLog10",
        lvo: -126,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvos::find_by_name;

    // Sanity-check a handful of well-known LVOs against published AmigaOS
    // mathieeesingtrans.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[
            ("IEEESPAtan", -30),
            ("IEEESPFieee", -108),
            ("IEEESPLog10", -126),
        ];
        for (name, lvo) in cases {
            let entry = find_by_name(MATHIEEESINGTRANS_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
