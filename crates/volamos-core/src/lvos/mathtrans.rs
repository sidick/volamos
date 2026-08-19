//! Generated `mathtrans.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Derived from AROS's `mathtrans.library` interface description
//! (`workbench/libs/mathtrans/mathtrans.conf`, the `##begin functionlist` block AROS's own build
//! generates the `.sfd`/`.fd` files from -- see `tools/gen_lvos.py` for
//! why this repo reads the `.conf` directly rather than a generated `.sfd`).
//!
//! - Source URL: <https://raw.githubusercontent.com/aros-development-team/AROS/c207aae1d67ac419553530b80eb62dcab2f923ee/workbench/libs/mathtrans/mathtrans.conf>
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

/// The full `mathtrans.library` LVO table (all known functions, not just the
/// ones this runtime currently implements handlers for -- this way
/// unknown-call diagnostics can print a real function name for any of
/// them, not just the handful we emulate).
pub static MATHTRANS_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "SPAtan",
        lvo: -30,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPSin",
        lvo: -36,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPCos",
        lvo: -42,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPTan",
        lvo: -48,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPSincos",
        lvo: -54,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPSinh",
        lvo: -60,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPCosh",
        lvo: -66,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPTanh",
        lvo: -72,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPExp",
        lvo: -78,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPLog",
        lvo: -84,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPPow",
        lvo: -90,
        args: &[ArgReg::D(DataRegister(1)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPSqrt",
        lvo: -96,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPTieee",
        lvo: -102,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPFieee",
        lvo: -108,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPAsin",
        lvo: -114,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPAcos",
        lvo: -120,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SPLog10",
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
    // mathtrans.library values (see docs/plan.md's T7/T12 entries).
    #[test]
    fn known_lvos_match_amigaos() {
        let cases: &[(&str, i32)] = &[("SPAtan", -30), ("SPFieee", -108), ("SPLog10", -126)];
        for (name, lvo) in cases {
            let entry = find_by_name(MATHTRANS_LVOS, name)
                .unwrap_or_else(|| panic!("missing LVO entry for {name}"));
            assert_eq!(entry.lvo, *lvo, "{name} LVO mismatch");
        }
    }
}
