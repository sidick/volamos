//! Generated `mathieeedoubtrans.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `mathieeedoubtrans.library` interface description
//! (`workbench/libs/mathieeedoubtrans/mathieeedoubtrans.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/c207aae1d67ac419553530b80eb62dcab2f923ee/workbench/libs/mathieeedoubtrans/mathieeedoubtrans.conf>
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

use crate::cpu::{AddressRegister, DataRegister};
use crate::lvos::{ArgReg, LvoEntry};

/// The full `mathieeedoubtrans.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static MATHIEEEDOUBTRANS_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "IEEEDPAtan",
        lvo: -30,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPSin",
        lvo: -36,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPCos",
        lvo: -42,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPTan",
        lvo: -48,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPSincos",
        lvo: -54,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPSinh",
        lvo: -60,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPCosh",
        lvo: -66,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPTanh",
        lvo: -72,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPExp",
        lvo: -78,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPLog",
        lvo: -84,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPPow",
        lvo: -90,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPSqrt",
        lvo: -96,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPTieee",
        lvo: -102,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPFieee",
        lvo: -108,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPAsin",
        lvo: -114,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPAcos",
        lvo: -120,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IEEEDPLog10",
        lvo: -126,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvos::find_by_name;

    // Sanity-check a handful of well-known LVOs against published AmigaOS
    // mathieeedoubtrans.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[
            ("IEEEDPAtan", -30),
            ("IEEEDPFieee", -108),
            ("IEEEDPLog10", -126),
        ];
        for (name, lvo) in cases {
            let entry = find_by_name(MATHIEEEDOUBTRANS_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
