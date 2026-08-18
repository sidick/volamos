//! `dos.library` string<->number conversion: `StrToLong`. A home for
//! this small family of calls (`StrToDate`/`DateToStr` etc. can join
//! later) that don't fit any of the other `dos*` modules' themes.

use crate::cpu::{Cpu, DataRegister};
use crate::dispatch::{DOS_LIBRARY_BASE, DispatchError, HandlerContext, LibraryTable};
use crate::guestmem::read_c_string;
use crate::lvos::dos::DOS_LVOS;
use crate::memory::AddressSpace;

/// Converts a decimal ASCII string per `StrToLong`'s documented
/// algorithm: skip leading spaces/tabs (counted in the result), an
/// optional leading `-` (no `+`), then digits. Returns `(characters,
/// value)`: `characters` is `-1` (with `value` `0`) if no digit was
/// found at all; otherwise it's how many bytes were consumed (including
/// skipped whitespace and the sign) and `value` is the parsed number.
/// On overflow (magnitude `> 2^31`), stops *before* the digit that would
/// overflow and reports `characters` up to that point, with `value`
/// documented by the real function as meaningless in that case.
fn str_to_long(input: &[u8]) -> (i32, i32) {
    let mut i = 0usize;
    while i < input.len() && matches!(input[i], b' ' | b'\t') {
        i += 1;
    }
    let negative = if i < input.len() && input[i] == b'-' {
        i += 1;
        true
    } else {
        false
    };
    let digits_start = i;
    let limit: i64 = if negative {
        1i64 << 31
    } else {
        (1i64 << 31) - 1
    };
    let mut value: i64 = 0;
    while i < input.len() && input[i].is_ascii_digit() {
        let candidate = value * 10 + i64::from(input[i] - b'0');
        if candidate > limit {
            break;
        }
        value = candidate;
        i += 1;
    }
    if i == digits_start {
        return (-1, 0);
    }
    let value = if negative { -value } else { value };
    (i as i32, value as i32)
}

/// `StrToLong` (`D1` = string `CString*`, `D2` = `LONG*` result). `D0` =
/// characters converted, or `-1`. Does not touch `IoErr()` -- matches
/// the real function, which is documented as leaving it alone even on
/// failure.
fn str_to_long_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let string_ptr = ctx.cpu.data_register(DataRegister(1));
    let value_ptr = ctx.cpu.data_register(DataRegister(2));
    let input = read_c_string(ctx.mem, string_ptr);
    let (consumed, value) = str_to_long(&input);
    if value_ptr != 0 {
        ctx.mem.write_u32(value_ptr, value as u32);
    }
    ctx.cpu.set_data_register(DataRegister(0), consumed as u32);
    Ok(())
}

/// Registers `StrToLong` onto [`DOS_LIBRARY_BASE`], looked up by name
/// through [`DOS_LVOS`]. Called from [`crate::dispatch::Runtime::new`]
/// alongside the other `dos.library` registrations.
pub fn register_dosstr_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    table
        .register_by_name(
            mem,
            DOS_LIBRARY_BASE,
            DOS_LVOS,
            "dos.library",
            "StrToLong",
            str_to_long_handler::<C>,
        )
        .unwrap_or_else(|e| panic!("StrToLong should be in DOS_LVOS: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_positive_number() {
        assert_eq!(str_to_long(b"123"), (3, 123));
    }

    #[test]
    fn negative_number() {
        assert_eq!(str_to_long(b"-42"), (3, -42));
    }

    #[test]
    fn skips_leading_whitespace_and_counts_it() {
        assert_eq!(str_to_long(b"  \t 7"), (5, 7));
    }

    #[test]
    fn stops_at_first_non_digit() {
        assert_eq!(str_to_long(b"12abc"), (2, 12));
    }

    #[test]
    fn no_digits_is_minus_one() {
        assert_eq!(str_to_long(b"abc"), (-1, 0));
        assert_eq!(str_to_long(b""), (-1, 0));
        assert_eq!(str_to_long(b"   "), (-1, 0));
        assert_eq!(str_to_long(b"-"), (-1, 0));
    }

    #[test]
    fn plus_sign_is_not_accepted() {
        // '+' is not a recognized sign -- StrToLong stops immediately,
        // finding no digits.
        assert_eq!(str_to_long(b"+5"), (-1, 0));
    }

    #[test]
    fn positive_overflow_stops_before_the_overflowing_digit() {
        // i32::MAX = 2147483647; "214748364" (9 digits) is still safe,
        // but appending the final "8" would make 2147483648, one over
        // the limit -- so the count stops at 9, not 11.
        let (chars, _) = str_to_long(b"21474836480");
        assert_eq!(chars, 9);
    }

    #[test]
    fn negative_min_is_exactly_representable() {
        assert_eq!(str_to_long(b"-2147483648"), (11, i32::MIN));
    }

    // --- End-to-end: real A-line trap dispatch ---

    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{Runtime, StartConfig};
    use crate::guestmem::write_c_string;
    use crate::memory::{AddressSpace, FlatMemory};

    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }
    fn jsr_disp16(an: u16) -> u16 {
        0x4EA8 | an
    }
    const RTS: u16 = 0x4E75;

    fn push_move_imm_to_d(words: &mut Vec<u16>, dn: u16, imm: u32) -> usize {
        let idx = words.len();
        words.push(move_imm_to_d(dn));
        words.push((imm >> 16) as u16);
        words.push(imm as u16);
        idx
    }
    fn push_jsr(words: &mut Vec<u16>, an: u16, disp: i32) {
        words.push(jsr_disp16(an));
        words.push(disp as u16);
    }
    fn patch_imm32(words: &mut [u16], idx: usize, value: u32) {
        words[idx + 1] = (value >> 16) as u16;
        words[idx + 2] = value as u16;
    }
    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    #[test]
    fn end_to_end_str_to_long_via_trap_dispatch() {
        // D1 = string, D2 = value ptr; jsr StrToLong(a6); D0 (== the
        // exit code, since nothing after touches it) is the character
        // count, and the LONG at value_addr should hold the parsed
        // number.
        let mut words = Vec::new();
        let string_idx = push_move_imm_to_d(&mut words, 1, 0);
        let value_idx = push_move_imm_to_d(&mut words, 2, 0);
        push_jsr(&mut words, 6, -816); // StrToLong(a6)
        words.push(RTS);

        let source = b"  42abc";
        let source_addr = TRAP_TABLE_END + (words.len() as u32) * 2;
        let value_addr = source_addr + source.len() as u32 + 1;
        patch_imm32(&mut words, string_idx, source_addr);
        patch_imm32(&mut words, value_idx, value_addr);

        let mut mem = FlatMemory::new(0x2_0000);
        load_words(&mut mem, TRAP_TABLE_END, &words);
        write_c_string(&mut mem, source_addr, source);
        mem.write_u32(value_addr, 0);

        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry: TRAP_TABLE_END,
                load_end: value_addr + 8,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 4, "\"  42\" is 4 characters converted");
        assert_eq!(rt.memory().read_u32(value_addr), 42);
    }
}
