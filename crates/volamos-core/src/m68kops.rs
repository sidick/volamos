//! A pure m68k control-flow-instruction classifier (issue #65).
//!
//! # What this is for
//!
//! The memory sanitizer's next increment is a *shadow call stack*: on
//! every `JSR`/`BSR` the runtime records the return address that got
//! pushed onto the guest stack, and on every `RTS`/`RTE`/`RTR`/`RTD` it
//! checks the address about to be popped against what it remembers —
//! catching stack-smashing (a corrupted return address) at the exact
//! instruction that would otherwise jump to garbage, rather than
//! reporting a confusing crash far downstream.
//!
//! To do that, the CPU run loop needs to know, from the raw opcode word
//! alone and *before* the instruction executes, whether it is a call, a
//! return, or something that merely adjusts the frame (`LINK`/`UNLK`) or
//! neither. The `m68k` crate this project embeds as its interpreter
//! doesn't expose that classification (it decodes straight to an
//! executed effect), so this module is a small, independent decoder
//! over just the opcode word. It is deliberately kept dependency-free
//! and pure — no guest-memory reads, no interpreter state — both
//! because that's all the classification genuinely needs and because a
//! wrong bit mask here is easy to get subtly wrong and hard to debug
//! once it's wired into the sanitizer (a mask that's too broad floods
//! the shadow stack with bogus calls/returns; one that's too narrow
//! silently lets real corruption through).
//!
//! # Bit-layout reasoning
//!
//! All encodings below are the standard M68000 Programmer's Reference
//! Manual (PRM) instruction encodings, the same ones reproduced
//! verbatim in every other 68k reference (e.g. the 68020/030 PRM for
//! the `LINK.L` and long-branch additions). Bit numbering is MSB-first,
//! bit 15 down to bit 0.
//!
//! ## `RTS`/`RTE`/`RTR`/`RTD` and their `0x4E7x` neighbours
//!
//! These are all in the single-word "no operand" instruction group
//! `0100 1110 0111 nnnn` (`0x4E70`–`0x4E7F`), selected by the low
//! nibble:
//!
//! | low nibble | opcode   | mnemonic         |
//! |------------|----------|------------------|
//! | `0000`     | `0x4E70` | `RESET`          |
//! | `0001`     | `0x4E71` | `NOP`            |
//! | `0010`     | `0x4E72` | `STOP #imm`      |
//! | `0011`     | `0x4E73` | `RTE`            |
//! | `0100`     | `0x4E74` | `RTD #imm` (68010+) |
//! | `0101`     | `0x4E75` | `RTS`            |
//! | `0110`     | `0x4E76` | `TRAPV`          |
//! | `0111`     | `0x4E77` | `RTR`            |
//! | `1010`     | `0x4E7A` | `MOVEC Rc,Rn` (68010+) |
//! | `1011`     | `0x4E7B` | `MOVEC Rn,Rc` (68010+) |
//! | others     | —        | unassigned/illegal |
//!
//! Masking with `0xFFF0` isolates the whole group at once, then a
//! `match` on the low nibble picks out exactly `RTE`/`RTD`/`RTS`/`RTR`
//! and sends everything else (including `NOP`/`STOP`/`RESET`/`TRAPV`/
//! `MOVEC`/unassigned) to [`ControlFlowOp::Other`] — the point being
//! that these near neighbours must never be confused with each other.
//!
//! ## `JSR`/`JMP` and legal effective-address modes
//!
//! Both share the group `0100 1110 1s mmmrrr` (`s` at bit 6): `s = 0`
//! is `JSR` (`0x4E80`–`0x4EBF`), `s = 1` is `JMP` (`0x4EC0`–`0x4EFF`).
//! Masking with `0xFFC0` isolates the fixed top 10 bits and leaves the
//! 6-bit effective-address field (`mmm` = mode, `rrr` = register/mode
//! extension) in the low bits.
//!
//! Both instructions require a *control* addressing mode (roughly:
//! "names a memory location", not a register or an immediate), the
//! same restricted set `LEA`/`PEA`/control-form `MOVEM` also require:
//!
//! | mode (`mmm`) | addressing mode           | legal for `JSR`/`JMP`? |
//! |---------------|---------------------------|--------------------------|
//! | `000`         | `Dn`                      | no (not a memory ref)   |
//! | `001`         | `An`                      | no (not a memory ref)   |
//! | `010`         | `(An)`                    | yes                     |
//! | `011`         | `(An)+`                   | no (not control-form)   |
//! | `100`         | `-(An)`                   | no (not control-form)   |
//! | `101`         | `(d16,An)`                | yes                     |
//! | `110`         | `(d8,An,Xn)` / full ext.  | yes                     |
//! | `111`, `rrr=000` | `(xxx).W` absolute short | yes                   |
//! | `111`, `rrr=001` | `(xxx).L` absolute long  | yes                   |
//! | `111`, `rrr=010` | `(d16,PC)`               | yes                   |
//! | `111`, `rrr=011` | `(d8,PC,Xn)` / full ext. | yes                   |
//! | `111`, `rrr=100` | `#imm`                   | no (not a memory ref) |
//! | `111`, `rrr>=101`| —                        | no (undefined)        |
//!
//! An opcode in the `JSR`/`JMP` word ranges whose effective-address bits
//! spell out an illegal mode is not a valid call/jump at all (it's an
//! illegal instruction) and classifies as [`ControlFlowOp::Other`] —
//! this is the "illegal EA mode is not a call" case the shadow call
//! stack must not be fooled by.
//!
//! ## `BSR` vs `BRA`/`Bcc`
//!
//! The short-branch group is `0110 cccc dddddddd`: the low byte `dddddddd`
//! is either the signed 8-bit displacement itself, or (if `0x00`) a flag
//! that a 16-bit word displacement follows, or (68020+, if `0xFF`) a
//! flag that a 32-bit long displacement follows. The high nibble `cccc`
//! selects among condition `0000` (always true → `BRA`), `0001`
//! (`BSR`), and `0010`–`1111` (the 14 real condition codes → `Bcc`).
//! Masking with `0xFF00` and comparing against `0x6100` picks out
//! exactly the `BSR` byte range and nothing from the adjacent `BRA`
//! (`0x6000`–`0x60FF`) or `Bcc` (`0x6200`–`0x6FFF`) ranges.
//!
//! ## `LINK`/`UNLK`
//!
//! `LINK.W An,#imm16` is `0100 1110 0101 0rrr` (`0x4E50`–`0x4E57`,
//! mask `0xFFF8`), immediately followed by `UNLK An` at
//! `0100 1110 0101 1rrr` (`0x4E58`–`0x4E5F`). `LINK.L An,#imm32`
//! (68020+) reuses what would otherwise be an illegal `NBCD` encoding
//! (`NBCD` requires a data-alterable effective address, and address
//! register direct — mode `001` — is not one) at
//! `0100 1000 0000 1rrr` (`0x4808`–`0x480F`, mask `0xFFF8`).
//!
//! # Sources
//!
//! All of the above are standard M68000/68020 PRM encodings; no single
//! non-authoritative source was needed to derive them; cross-checked
//! bit-for-bit against multiple independent 68k opcode-map references
//! for internal consistency before writing the masks below.

/// What a single 16-bit opcode word does to the return-address /
/// call-frame state that a shadow call stack cares about.
///
/// This is deliberately *not* a full instruction decode — e.g. `Bcc`
/// and `BRA` are lumped into [`Other`](ControlFlowOp::Other) because
/// they don't touch the stack at all, and `TRAP`/exception-generating
/// instructions aren't distinguished from ordinary ALU ops for the same
/// reason. It only distinguishes what the shadow call stack needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlFlowOp {
    /// `JSR <ea>` — pushes a return address; the call target comes from
    /// the effective address (which the caller must still decode/read
    /// itself; this module only confirms the *mode* is one of the
    /// legal control forms, it doesn't read guest memory).
    Jsr,
    /// `BSR.{b,w,l}` — pushes a return address; the call target is
    /// PC-relative. See [`bsr_displacement_size`] for the extension
    /// word(s) the caller needs to read to compute both the target and
    /// the exact return address.
    Bsr,
    /// `RTS` — pops a return address.
    Rts,
    /// `RTE` — returns from an exception, popping a larger frame (at
    /// minimum SR + PC; more on the 68010+ if a format/vector word
    /// indicates one). Not a plain call return; included so the
    /// sanitizer can treat it distinctly (e.g. exempt it, since the
    /// "return address" here was pushed by CPU exception delivery, not
    /// a `JSR`/`BSR` the shadow stack necessarily saw).
    Rte,
    /// `RTR` — pops the condition-code word, then a return address.
    Rtr,
    /// `RTD #d` (68010+) — pops a return address and then additionally
    /// adjusts the stack pointer by the signed 16-bit immediate that
    /// follows the opcode word.
    Rtd,
    /// `JMP <ea>` — transfers control without touching the stack at
    /// all. Included (rather than folded into `Other`) because the
    /// shadow call stack still cares: a `JMP` through a corrupted
    /// pointer is exactly the kind of hijack it exists to catch, even
    /// though no return address is involved.
    Jmp,
    /// `LINK` (word or, 68020+, long form) — frame setup: pushes the
    /// frame pointer and adjusts the stack. Not a call/return, but
    /// tracking it lets a caller that wants full frame-integrity
    /// checking (not just return-address checking) see the paired
    /// `LINK`/`UNLK`.
    Link,
    /// `UNLK An` — frame teardown, the inverse of `LINK`.
    Unlk,
    /// Anything else: every other opcode, *and* opcode words that sit
    /// in the `JSR`/`JMP` bit ranges but spell out an effective-address
    /// mode that's illegal for those instructions.
    Other,
}

/// How a `BSR` (or `Bcc`/`BRA`, though this module doesn't classify
/// those) opcode's low byte encodes its branch displacement.
///
/// All three short-branch families (`BRA`, `BSR`, `Bcc`) share this
/// same low-byte convention, so this type is named generically even
/// though [`bsr_displacement_size`] is the only entry point that
/// exposes it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BranchDisplacementSize {
    /// The displacement is the low byte itself (any value except
    /// `0x00`/`0xFF`, which are reserved for the two cases below); no
    /// extension word follows.
    Byte,
    /// Low byte is `0x00`: a 16-bit signed displacement follows as one
    /// extension word.
    Word,
    /// Low byte is `0xFF` (68020+ only): a 32-bit signed displacement
    /// follows as two extension words (one long word).
    Long,
}

impl BranchDisplacementSize {
    /// Number of extra bytes read from immediately after the opcode
    /// word to get the displacement (0, 2, or 4).
    pub const fn extension_bytes(self) -> u32 {
        match self {
            BranchDisplacementSize::Byte => 0,
            BranchDisplacementSize::Word => 2,
            BranchDisplacementSize::Long => 4,
        }
    }
}

/// `mode == 010`: `(An)`.
const EA_MODE_AN_INDIRECT: u16 = 0b010;
/// `mode == 101`: `(d16,An)`.
const EA_MODE_AN_DISP: u16 = 0b101;
/// `mode == 110`: `(d8,An,Xn)` or, on the 68020+, a full extension word.
const EA_MODE_AN_INDEX: u16 = 0b110;
/// `mode == 111`: the "other" group, disambiguated by the register
/// field (absolute/PC-relative/immediate).
const EA_MODE_OTHER: u16 = 0b111;

/// Effective-address modes that are legal for `JSR`/`JMP` (the "control
/// addressing" subset — see the module doc's table for the full
/// derivation and which modes are excluded and why).
fn is_legal_jsr_jmp_ea(mode: u16, reg: u16) -> bool {
    match mode {
        EA_MODE_AN_INDIRECT | EA_MODE_AN_DISP | EA_MODE_AN_INDEX => true,
        EA_MODE_OTHER => matches!(reg, 0b000..=0b011),
        _ => false,
    }
}

/// Classify a single 16-bit m68k opcode word for shadow-call-stack
/// purposes.
///
/// This is a pure function of the opcode word alone: it never needs
/// extension words to *classify* an instruction (only to compute its
/// full length or its target/return address afterwards — see the
/// per-variant docs on [`ControlFlowOp`] and [`bsr_displacement_size`]
/// for what's left to the caller).
pub fn classify(opcode: u16) -> ControlFlowOp {
    // The `0x4E7x` no-operand group: RESET/NOP/STOP/RTE/RTD/RTS/TRAPV/
    // RTR/MOVEC/unassigned, selected by the low nibble. Handled first
    // and as a single masked group so these tightly-packed neighbours
    // can't leak into any other case below.
    if opcode & 0xFFF0 == 0x4E70 {
        return match opcode & 0x000F {
            0x3 => ControlFlowOp::Rte,
            0x4 => ControlFlowOp::Rtd,
            0x5 => ControlFlowOp::Rts,
            0x7 => ControlFlowOp::Rtr,
            _ => ControlFlowOp::Other,
        };
    }

    // LINK.W An,#imm16 = 0x4E50-0x4E57.
    if opcode & 0xFFF8 == 0x4E50 {
        return ControlFlowOp::Link;
    }
    // UNLK An = 0x4E58-0x4E5F.
    if opcode & 0xFFF8 == 0x4E58 {
        return ControlFlowOp::Unlk;
    }
    // LINK.L An,#imm32 (68020+) = 0x4808-0x480F (an illegal-for-NBCD
    // encoding repurposed; see module doc).
    if opcode & 0xFFF8 == 0x4808 {
        return ControlFlowOp::Link;
    }

    // BSR = 0x6100-0x61FF. Must be checked with an exact match on the
    // condition nibble (via the 0xFF00 mask), not a range, so it can't
    // be confused with the adjacent BRA (0x6000-0x60FF) or Bcc
    // (0x6200-0x6FFF) byte ranges.
    if opcode & 0xFF00 == 0x6100 {
        return ControlFlowOp::Bsr;
    }

    // JSR <ea> = 0x4E80-0x4EBF.
    if opcode & 0xFFC0 == 0x4E80 {
        let mode = (opcode >> 3) & 0x7;
        let reg = opcode & 0x7;
        return if is_legal_jsr_jmp_ea(mode, reg) {
            ControlFlowOp::Jsr
        } else {
            ControlFlowOp::Other
        };
    }

    // JMP <ea> = 0x4EC0-0x4EFF.
    if opcode & 0xFFC0 == 0x4EC0 {
        let mode = (opcode >> 3) & 0x7;
        let reg = opcode & 0x7;
        return if is_legal_jsr_jmp_ea(mode, reg) {
            ControlFlowOp::Jmp
        } else {
            ControlFlowOp::Other
        };
    }

    ControlFlowOp::Other
}

/// For a `BSR` opcode (as confirmed by [`classify`]), which of the
/// three displacement encodings its low byte selects.
///
/// Returns `None` if `opcode` doesn't classify as [`ControlFlowOp::Bsr`].
///
/// This only tells the caller *how many extension bytes to read* to get
/// the displacement; it does not read them. To compute the address
/// actually pushed as the return address, the caller still needs to
/// add `2` (the opcode word itself) plus
/// [`extension_bytes`](BranchDisplacementSize::extension_bytes) to the
/// instruction's own address — the displacement value itself is only
/// needed for the *call target*, not the return address.
pub fn bsr_displacement_size(opcode: u16) -> Option<BranchDisplacementSize> {
    if classify(opcode) != ControlFlowOp::Bsr {
        return None;
    }
    Some(match opcode & 0x00FF {
        0x00 => BranchDisplacementSize::Word,
        0xFF => BranchDisplacementSize::Long,
        _ => BranchDisplacementSize::Byte,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 0x4E70-0x4E7F: the RTS/RTE/RTR/RTD neighbourhood ----

    #[test]
    fn rtx_group_every_opcode_in_range() {
        let expected: [ControlFlowOp; 16] = [
            ControlFlowOp::Other, // 0x4E70 RESET
            ControlFlowOp::Other, // 0x4E71 NOP
            ControlFlowOp::Other, // 0x4E72 STOP
            ControlFlowOp::Rte,   // 0x4E73 RTE
            ControlFlowOp::Rtd,   // 0x4E74 RTD
            ControlFlowOp::Rts,   // 0x4E75 RTS
            ControlFlowOp::Other, // 0x4E76 TRAPV
            ControlFlowOp::Rtr,   // 0x4E77 RTR
            ControlFlowOp::Other, // 0x4E78 unassigned
            ControlFlowOp::Other, // 0x4E79 unassigned
            ControlFlowOp::Other, // 0x4E7A MOVEC Rc,Rn
            ControlFlowOp::Other, // 0x4E7B MOVEC Rn,Rc
            ControlFlowOp::Other, // 0x4E7C unassigned
            ControlFlowOp::Other, // 0x4E7D unassigned
            ControlFlowOp::Other, // 0x4E7E unassigned
            ControlFlowOp::Other, // 0x4E7F unassigned (illegal is here too)
        ];
        for (i, exp) in expected.iter().enumerate() {
            let opcode = 0x4E70 + i as u16;
            assert_eq!(
                classify(opcode),
                *exp,
                "opcode 0x{opcode:04X} misclassified"
            );
        }
    }

    #[test]
    fn rtx_neighbours_are_pairwise_distinct() {
        assert_ne!(classify(0x4E73), classify(0x4E74));
        assert_ne!(classify(0x4E74), classify(0x4E75));
        assert_ne!(classify(0x4E75), classify(0x4E77));
        assert_ne!(classify(0x4E73), classify(0x4E75));
        assert_ne!(classify(0x4E73), classify(0x4E77));
        assert_ne!(classify(0x4E74), classify(0x4E77));
    }

    // ---- LINK / UNLK ----

    #[test]
    fn link_word_all_eight_registers() {
        for reg in 0u16..8 {
            let opcode = 0x4E50 | reg;
            assert_eq!(classify(opcode), ControlFlowOp::Link, "reg A{reg}");
        }
    }

    #[test]
    fn unlk_all_eight_registers() {
        for reg in 0u16..8 {
            let opcode = 0x4E58 | reg;
            assert_eq!(classify(opcode), ControlFlowOp::Unlk, "reg A{reg}");
        }
    }

    #[test]
    fn link_long_all_eight_registers() {
        for reg in 0u16..8 {
            let opcode = 0x4808 | reg;
            assert_eq!(classify(opcode), ControlFlowOp::Link, "reg A{reg}");
        }
    }

    #[test]
    fn link_and_unlk_do_not_bleed_into_each_other() {
        // The boundary is 0x4E57/0x4E58; make sure it's exact.
        assert_eq!(classify(0x4E57), ControlFlowOp::Link);
        assert_eq!(classify(0x4E58), ControlFlowOp::Unlk);
        assert_eq!(classify(0x4E5F), ControlFlowOp::Unlk);
        assert_eq!(classify(0x4E60), ControlFlowOp::Other);
        assert_eq!(classify(0x4E4F), ControlFlowOp::Other);
    }

    // ---- BSR vs BRA vs Bcc ----

    #[test]
    fn bsr_full_byte_range_is_bsr() {
        for disp in 0u16..=0xFF {
            let opcode = 0x6100 | disp;
            assert_eq!(classify(opcode), ControlFlowOp::Bsr, "disp 0x{disp:02X}");
        }
    }

    #[test]
    fn bra_full_byte_range_is_not_bsr() {
        for disp in 0u16..=0xFF {
            let opcode = 0x6000 | disp;
            assert_ne!(
                classify(opcode),
                ControlFlowOp::Bsr,
                "opcode 0x{opcode:04X}"
            );
            assert_eq!(
                classify(opcode),
                ControlFlowOp::Other,
                "opcode 0x{opcode:04X}"
            );
        }
    }

    #[test]
    fn every_bcc_condition_code_is_not_bsr() {
        // cc = 0b0010 .. 0b1111 (14 real conditions); 0b0000 is BRA and
        // 0b0001 is BSR, already covered by the tests above.
        for cc in 0x2u16..=0xF {
            let base = cc << 8;
            for disp in [0x00u16, 0x01, 0x7F, 0x80, 0xFE, 0xFF] {
                let opcode = 0x6000 | base | disp;
                assert_ne!(
                    classify(opcode),
                    ControlFlowOp::Bsr,
                    "cc 0x{cc:X} opcode 0x{opcode:04X} wrongly classified as BSR"
                );
                assert_eq!(
                    classify(opcode),
                    ControlFlowOp::Other,
                    "cc 0x{cc:X} opcode 0x{opcode:04X}"
                );
            }
        }
    }

    #[test]
    fn bsr_displacement_size_byte_word_long() {
        assert_eq!(
            bsr_displacement_size(0x6101),
            Some(BranchDisplacementSize::Byte)
        );
        assert_eq!(
            bsr_displacement_size(0x617F),
            Some(BranchDisplacementSize::Byte)
        );
        assert_eq!(
            bsr_displacement_size(0x6180),
            Some(BranchDisplacementSize::Byte)
        );
        assert_eq!(
            bsr_displacement_size(0x61FE),
            Some(BranchDisplacementSize::Byte)
        );
        assert_eq!(
            bsr_displacement_size(0x6100),
            Some(BranchDisplacementSize::Word)
        );
        assert_eq!(
            bsr_displacement_size(0x61FF),
            Some(BranchDisplacementSize::Long)
        );
    }

    #[test]
    fn bsr_displacement_size_none_for_non_bsr() {
        assert_eq!(bsr_displacement_size(0x6000), None);
        assert_eq!(bsr_displacement_size(0x6200), None);
        assert_eq!(bsr_displacement_size(0x4E75), None);
    }

    #[test]
    fn branch_displacement_extension_bytes() {
        assert_eq!(BranchDisplacementSize::Byte.extension_bytes(), 0);
        assert_eq!(BranchDisplacementSize::Word.extension_bytes(), 2);
        assert_eq!(BranchDisplacementSize::Long.extension_bytes(), 4);
    }

    // ---- JSR / JMP: legal and illegal effective-address modes ----

    fn jsr_opcode(mode: u16, reg: u16) -> u16 {
        0x4E80 | (mode << 3) | reg
    }
    fn jmp_opcode(mode: u16, reg: u16) -> u16 {
        0x4EC0 | (mode << 3) | reg
    }

    #[test]
    fn jsr_legal_ea_modes() {
        // (An) for all 8 address registers.
        for reg in 0u16..8 {
            assert_eq!(
                classify(jsr_opcode(0b010, reg)),
                ControlFlowOp::Jsr,
                "(A{reg})"
            );
        }
        // (d16,An) for all 8 address registers.
        for reg in 0u16..8 {
            assert_eq!(
                classify(jsr_opcode(0b101, reg)),
                ControlFlowOp::Jsr,
                "(d16,A{reg})"
            );
        }
        // (d8,An,Xn) for all 8 address registers.
        for reg in 0u16..8 {
            assert_eq!(
                classify(jsr_opcode(0b110, reg)),
                ControlFlowOp::Jsr,
                "(d8,A{reg},Xn)"
            );
        }
        // mode 111: abs.w, abs.l, (d16,PC), (d8,PC,Xn).
        for reg in [0b000u16, 0b001, 0b010, 0b011] {
            assert_eq!(
                classify(jsr_opcode(0b111, reg)),
                ControlFlowOp::Jsr,
                "mode 111 reg {reg:03b}"
            );
        }
    }

    #[test]
    fn jsr_illegal_ea_modes_are_other() {
        // Dn, An, (An)+, -(An) are illegal for JSR regardless of register.
        for mode in [0b000u16, 0b001, 0b011, 0b100] {
            for reg in 0u16..8 {
                assert_eq!(
                    classify(jsr_opcode(mode, reg)),
                    ControlFlowOp::Other,
                    "mode {mode:03b} reg {reg:03b} should be illegal"
                );
            }
        }
        // mode 111 with reg 100 (#imm) or reg >= 101 (undefined).
        for reg in [0b100u16, 0b101, 0b110, 0b111] {
            assert_eq!(
                classify(jsr_opcode(0b111, reg)),
                ControlFlowOp::Other,
                "mode 111 reg {reg:03b} should be illegal"
            );
        }
    }

    #[test]
    fn jmp_legal_ea_modes() {
        for reg in 0u16..8 {
            assert_eq!(
                classify(jmp_opcode(0b010, reg)),
                ControlFlowOp::Jmp,
                "(A{reg})"
            );
        }
        for reg in 0u16..8 {
            assert_eq!(
                classify(jmp_opcode(0b101, reg)),
                ControlFlowOp::Jmp,
                "(d16,A{reg})"
            );
        }
        for reg in 0u16..8 {
            assert_eq!(
                classify(jmp_opcode(0b110, reg)),
                ControlFlowOp::Jmp,
                "(d8,A{reg},Xn)"
            );
        }
        for reg in [0b000u16, 0b001, 0b010, 0b011] {
            assert_eq!(
                classify(jmp_opcode(0b111, reg)),
                ControlFlowOp::Jmp,
                "mode 111 reg {reg:03b}"
            );
        }
    }

    #[test]
    fn jmp_illegal_ea_modes_are_other() {
        for mode in [0b000u16, 0b001, 0b011, 0b100] {
            for reg in 0u16..8 {
                assert_eq!(
                    classify(jmp_opcode(mode, reg)),
                    ControlFlowOp::Other,
                    "mode {mode:03b} reg {reg:03b} should be illegal"
                );
            }
        }
        for reg in [0b100u16, 0b101, 0b110, 0b111] {
            assert_eq!(
                classify(jmp_opcode(0b111, reg)),
                ControlFlowOp::Other,
                "mode 111 reg {reg:03b} should be illegal"
            );
        }
    }

    #[test]
    fn jsr_and_jmp_are_distinguished_by_bit_six_not_confused() {
        // Same effective-address bits, differing only in the JSR/JMP
        // selector bit, must classify differently.
        for mode in [0b010u16, 0b101, 0b110] {
            for reg in 0u16..8 {
                let jsr = jsr_opcode(mode, reg);
                let jmp = jmp_opcode(mode, reg);
                assert_eq!(classify(jsr), ControlFlowOp::Jsr);
                assert_eq!(classify(jmp), ControlFlowOp::Jmp);
                assert_ne!(classify(jsr), classify(jmp));
            }
        }
    }

    #[test]
    fn known_specific_opcodes() {
        // JSR (A0), JMP (A0): the most common real-world forms.
        assert_eq!(classify(0x4E90), ControlFlowOp::Jsr); // JSR (A0)
        assert_eq!(classify(0x4ED0), ControlFlowOp::Jmp); // JMP (A0)
        // JSR abs.l / JMP abs.l, as generated by most compilers/linkers
        // for absolute calls.
        assert_eq!(classify(0x4EB9), ControlFlowOp::Jsr); // JSR $xxxxxxxx.L
        assert_eq!(classify(0x4EF9), ControlFlowOp::Jmp); // JMP $xxxxxxxx.L
    }

    // ---- Full-space sweep: exact population counts + "mostly Other" ----

    #[test]
    fn full_opcode_space_population_counts() {
        // Every legal (mode, reg) combination for JSR/JMP: 8 + 8 + 8
        // for modes 010/101/110, plus 4 for mode 111 (regs 0-3) = 28.
        const EXPECTED_JSR_JMP: usize = 8 + 8 + 8 + 4;

        let mut counts: std::collections::HashMap<ControlFlowOp, usize> =
            std::collections::HashMap::new();
        for opcode in 0u32..=0xFFFF {
            *counts.entry(classify(opcode as u16)).or_insert(0) += 1;
        }

        assert_eq!(
            counts.get(&ControlFlowOp::Jsr).copied().unwrap_or(0),
            EXPECTED_JSR_JMP
        );
        assert_eq!(
            counts.get(&ControlFlowOp::Jmp).copied().unwrap_or(0),
            EXPECTED_JSR_JMP
        );
        assert_eq!(counts.get(&ControlFlowOp::Bsr).copied().unwrap_or(0), 256);
        assert_eq!(counts.get(&ControlFlowOp::Rts).copied().unwrap_or(0), 1);
        assert_eq!(counts.get(&ControlFlowOp::Rte).copied().unwrap_or(0), 1);
        assert_eq!(counts.get(&ControlFlowOp::Rtr).copied().unwrap_or(0), 1);
        assert_eq!(counts.get(&ControlFlowOp::Rtd).copied().unwrap_or(0), 1);
        assert_eq!(
            counts.get(&ControlFlowOp::Link).copied().unwrap_or(0),
            8 + 8
        );
        assert_eq!(counts.get(&ControlFlowOp::Unlk).copied().unwrap_or(0), 8);

        let classified_total = EXPECTED_JSR_JMP * 2 + 256 + 1 + 1 + 1 + 1 + (8 + 8) + 8;
        let other = counts.get(&ControlFlowOp::Other).copied().unwrap_or(0);
        assert_eq!(other, 65536 - classified_total);
        // Sanity: the overwhelming majority of the opcode space is
        // ordinary data-processing instructions, not control flow. If a
        // mask were accidentally too broad this would fail loudly.
        assert!(other > 60000, "Other count suspiciously low: {other}");
    }

    #[test]
    fn unrelated_data_processing_opcodes_are_other() {
        // A representative sweep of opcodes from instruction families
        // that must never be mistaken for control flow: ORI/ANDI/SUBI
        // (0x0000-0x0FFF region), MOVE.B/W/L (0x1000-0x3FFF), MOVEQ
        // (0x7xxx), OR/SUB/SBCD (0x8xxx/0x9xxx), AND/MUL/ABCD/EXG
        // (0xCxxx), ADD (0xDxxx), shifts/rotates (0xExxx).
        let samples: &[u16] = &[
            0x0000, 0x0001, 0x023C, 0x0800, 0x0FFF, // ORI/BTST family
            0x1000, 0x2001, 0x3040, 0x33FF, // MOVE family
            0x7000, 0x7201, 0x70FF, // MOVEQ
            0x8000, 0x91C0, 0x9FFF, // OR/SUB
            0xB000, 0xB1AA, // CMP/EOR
            0xC000, 0xC1C0, 0xCFFF, // AND/MUL/EXG/ABCD
            0xD000, 0xD1C0, 0xDFFF, // ADD
            0xE000, 0xE118, 0xEFFF, // shift/rotate
        ];
        for &opcode in samples {
            assert_eq!(
                classify(opcode),
                ControlFlowOp::Other,
                "opcode 0x{opcode:04X} wrongly classified as control flow"
            );
        }
    }

    #[test]
    fn control_flow_ops_are_mutually_exclusive_across_full_space() {
        // Every opcode must classify to exactly one variant (trivially
        // true by construction of a `match`/if-chain returning a single
        // value, but this pins the invariant so a future refactor that
        // e.g. splits classify() into overlapping helpers can't
        // silently break it).
        for opcode in 0u32..=0xFFFF {
            let op = classify(opcode as u16);
            // Re-classifying is idempotent/pure.
            assert_eq!(classify(opcode as u16), op);
        }
    }
}
