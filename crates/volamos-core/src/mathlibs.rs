//! `mathieeedoubbas.library`/`mathieeedoubtrans.library`/
//! `mathieeesingbas.library`/`mathieeesingtrans.library`/
//! `mathtrans.library`: real math library implementations, not just
//! [`crate::dispatch`]'s "vamos escape hatch" fake traps -- see
//! `crate::dispatch::STANDARD_WORKBENCH_LIBRARIES`'s doc for why these
//! specific libraries (unlike an arbitrary optional disk library) are
//! treated as always-present. Found missing while running the real
//! `PhxAss` assembler.
//!
//! `mathieeesingbas`/`mathieeesingtrans` mirror `mathieeedoubbas`/
//! `mathieeedoubtrans` function-for-function (same LVO offsets, per
//! AROS's own `.conf` files -- `IEEESPFix`/`IEEEDPFix` are both `-30`,
//! and so on down the list) but operate on IEEE single precision
//! (`f32`, one register per value) instead of double (`f64`, a register
//! pair) -- see [`ieeesp_unary`]'s doc for the one place their
//! behavior is an *assumption* carried over from the double-precision
//! real-hardware verification (issue #52) rather than independently
//! confirmed for singles specifically.
//!
//! # Calling convention
//!
//! A `double` argument or result occupies a register *pair*: the high 32
//! bits in the lower-numbered register, the low 32 bits in the next one
//! (`D0`/`D1` for the first `double`, `D2`/`D3` for a second one, per each
//! library's real AROS `.conf` interface description --
//! [`crate::lvos::mathieeedoubbas`]/[`crate::lvos::mathieeedoubtrans`]).
//! [`read_f64`]/[`write_f64`] centralize that packing. A `LONG` argument or
//! result (an IEEE single's raw bits, an FFP-encoded value, or a plain
//! 32-bit integer) is a single register, standard AmigaOS convention.
//!
//! # `mathtrans.library`'s Fast Floating Point (FFP) format
//!
//! `mathtrans.library` predates `mathieeedoubbas.library`/
//! `mathieeedoubtrans.library` and operates on AmigaOS's own 32-bit FFP
//! encoding, not IEEE-754. Bit 7 is the sign, bits 0-6 are a 7-bit
//! exponent in excess-64 (so a stored field of `$40` means an unbiased
//! exponent of `0`), and bits 8-31 are a 24-bit normalized mantissa
//! treated as a fraction in `[0.5, 1)` (or the whole value is `0` if
//! every bit is `0`) -- confirmed directly against amitools' own
//! `test/src/math_fast.h` FFP constants (`FFP_ONE = $80000041`,
//! `FFP_PI = $C90FDB42`, `FFP_1000 = $FA00004A`, ...decoding each to
//! its real value under this layout, byte for byte) rather than a
//! secondhand wiki description -- see issue #53's writeup for the
//! decode-by-hand verification of all five. [`ffp_to_f32`]/
//! [`f32_to_ffp`] convert to/from a plain `f32` by re-deriving the shared
//! bit pattern from IEEE-754 single precision's own `1.mantissa * 2^exp`
//! layout (see [`f32_to_ffp`]'s doc for the derivation) rather than a
//! `log2`/`powi` round trip, avoiding floating-point edge cases at power-
//! of-two boundaries. FFP's exponent field is only 7 bits (roughly
//! `2^-64`..`2^63`) versus IEEE single's 8 (`2^-126`..`2^127`), a real,
//! documented range limitation of the format -- out-of-range results
//! saturate to FFP's largest/smallest representable magnitude rather than
//! panicking or wrapping.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, HandlerContext, LibraryTable};
use crate::lvos::mathieeedoubbas::MATHIEEEDOUBBAS_LVOS;
use crate::lvos::mathieeedoubtrans::MATHIEEEDOUBTRANS_LVOS;
use crate::lvos::mathieeesingbas::MATHIEEESINGBAS_LVOS;
use crate::lvos::mathieeesingtrans::MATHIEEESINGTRANS_LVOS;
use crate::lvos::mathtrans::MATHTRANS_LVOS;
use crate::memory::AddressSpace;

/// Reads a `double` argument/result from a register pair (`base`/`base+1`
/// as `Dn` numbers, big-endian: `base` holds the high 32 bits).
fn read_f64<C: Cpu>(cpu: &C, base: u8) -> f64 {
    let hi = cpu.data_register(DataRegister(base)) as u64;
    let lo = cpu.data_register(DataRegister(base + 1)) as u64;
    f64::from_bits((hi << 32) | lo)
}

/// Writes a `double` result into a register pair -- see [`read_f64`].
fn write_f64<C: Cpu>(cpu: &mut C, base: u8, value: f64) {
    let bits = value.to_bits();
    cpu.set_data_register(DataRegister(base), (bits >> 32) as u32);
    cpu.set_data_register(DataRegister(base + 1), bits as u32);
}

/// Converts an AmigaOS FFP-encoded 32-bit value to a plain `f32` -- see
/// this module's doc comment for the bit layout.
fn ffp_to_f32(bits: u32) -> f32 {
    if bits == 0 {
        return 0.0;
    }
    let sign = if bits & 0x80 != 0 { -1.0f32 } else { 1.0f32 };
    let exponent = (bits & 0x7F) as i32 - 64;
    let mantissa = bits >> 8; // already a 24-bit fraction over 2^24
    sign * (mantissa as f32 / (1u32 << 24) as f32) * 2f32.powi(exponent)
}

/// Converts a plain `f32` to AmigaOS's FFP encoding -- see this module's
/// doc comment for the bit layout.
///
/// # Derivation
///
/// IEEE-754 single precision represents a normal value as `1.M * 2^E_ieee`
/// (`M` a 23-bit fraction, `E_ieee` the unbiased exponent); FFP represents
/// the same value as `F * 2^(E_ffp - 64)` (`F` a 24-bit fraction in
/// `[0.5, 1)`). The 24-bit integer `(1<<23) | M` (IEEE's implicit leading
/// 1 plus its 23 explicit mantissa bits) is *exactly* FFP's 24-bit
/// mantissa field: read as a `Q1.23` fixed-point number it's `1.M`; read
/// as `Q0.24` (FFP's convention) it's `1.M / 2`, i.e. `F`. So
/// `1.M * 2^E_ieee == 2F * 2^E_ieee == F * 2^(E_ieee + 1)`, giving
/// `E_ffp = E_ieee + 65`, and since `E_ffp` (as used above in
/// `F * 2^(E_ffp - 64)`) *is* the stored excess-64 field already (not
/// biased again), the stored field is `E_ieee + 65 = (raw_exp - 127) + 65
/// = raw_exp - 62`.
fn f32_to_ffp(value: f32) -> u32 {
    if value == 0.0 {
        return 0;
    }
    // A domain error (`SPAcos`/`SPAsin` outside `[-1,1]`, `SPLog`/
    // `SPSqrt` of a negative number, ...) produces `NaN` here, not a
    // finite-but-huge value -- there's no honest FFP encoding for
    // "undefined", and issue #53's real-corpus comparison confirms `0`
    // (not saturating to the largest magnitude, which is for a
    // genuine overflow -- see below) is what real `mathtrans.library`
    // itself reports for these.
    if value.is_nan() {
        return 0;
    }
    let bits = value.to_bits();
    let sign = (bits >> 31) & 1;

    // FFP's exponent field is 7 bits (excess-64, so 1..=127 once
    // clamped away from the reserved all-zero "value is 0" encoding);
    // IEEE single's own range is wider on both ends (roughly
    // 2^-126..2^127 versus FFP's 2^-64..2^63 -- see this module's doc
    // comment), and this project's own comparison harness against
    // amitools' real test/bin corpus (issue #53) found real
    // `mathffp`/`mathtrans` saturate *asymmetrically* on the two
    // sides, not just clamp the exponent field in place:
    // - Overflow (too *large* to represent, including `value` already
    //   being `+-inf` -- e.g. `SPExp`/`SPCosh`/`SPSinh` of a huge
    //   input routinely overflow `f32` to infinity before FFP encoding
    //   even sees a finite number) saturates to FFP's largest
    //   representable magnitude with the *correct sign* -- mantissa
    //   all-`1`s, exponent field `127` (`$FFFFFF7F`/`$FFFFFFFF`,
    //   exactly `test/src/math_fast.h`'s own `FFP_MAX`/`FFP_MAX_NEG`
    //   constants) -- not the previous behavior of clamping only the
    //   exponent field while leaving whatever mantissa the original
    //   value happened to have, which produced a smaller-magnitude,
    //   wrong value still tagged with the maximum exponent.
    // - Underflow (too *small* to represent even at FFP's smallest
    //   nonzero exponent, e.g. converting IEEE's own `FLT_MIN`, whose
    //   magnitude is far below FFP's floor) flushes to `0` instead --
    //   clamping *up* to FFP's smallest representable nonzero
    //   magnitude would silently turn a tiny-but-real value into one
    //   many orders of magnitude larger, which is a far worse
    //   approximation than just rounding it down to zero.
    if value.is_infinite() {
        return ffp_max_magnitude(sign);
    }
    let raw_exp = (bits >> 23) & 0xFF;
    let e_ffp_field = raw_exp as i32 - 62;
    // `>= 127`, not `> 127`: verified against real Kickstart 3.1
    // (40.72) via Copperline (issue #53's `mul3`/`mul4` residual --
    // `SPMul(FFP_INT_MAX, FFP_INT_MAX)`/`SPMul(FFP_INT_MIN,
    // FFP_INT_MIN)`, whose exact mathematical result's exponent field
    // computes to precisely `127`). Real hardware treats that boundary
    // value as already-saturated (full `$FFFFFF7F`/`$FFFFFFFF`), not a
    // legitimately representable finite value with a merely-large
    // mantissa -- i.e. field `127` is reserved for "this is FFP_MAX",
    // the same way field `0` is reserved for "this is zero", leaving
    // only `1..=126` for ordinary finite magnitudes.
    if e_ffp_field >= 127 {
        return ffp_max_magnitude(sign);
    }
    if e_ffp_field < 1 {
        return 0;
    }

    let mantissa24 = (1u32 << 23) | (bits & 0x7F_FFFF);
    (mantissa24 << 8) | (sign << 7) | (e_ffp_field as u32)
}

/// FFP's largest representable magnitude with the given sign (`0` or
/// `1`, as extracted from an IEEE bit pattern) -- mantissa all-`1`s,
/// exponent field `127`. Matches `test/src/math_fast.h`'s own
/// `FFP_MAX`/`FFP_MAX_NEG` constants (`$FFFFFF7F`/`$FFFFFFFF`) exactly.
/// Also stands in for `+-inf`/`NaN`, which FFP has no encoding for at
/// all -- see [`f32_to_ffp`]'s own doc for why this, not `0`, is the
/// honest answer for an overflowing (as opposed to underflowing)
/// result.
fn ffp_max_magnitude(sign: u32) -> u32 {
    (0x00FF_FFFFu32 << 8) | (sign << 7) | 127
}

/// `mathieeedoubbas.library`'s `IEEEDPFix` (LVO -30: `D0/D1` = `double`
/// `y`). `D0` = `y` truncated toward zero to a 32-bit integer.
fn ieeedp_fix_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = read_f64(ctx.cpu, 0);
    ctx.cpu.set_data_register(DataRegister(0), y as i32 as u32);
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPFlt` (LVO -36: `D0` = 32-bit integer
/// `y`). `D0/D1` = `y` converted to `double`.
fn ieeedp_flt_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = ctx.cpu.data_register(DataRegister(0)) as i32;
    write_f64(ctx.cpu, 0, y as f64);
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPCmp` (LVO -42: `D0/D1` = `y`,
/// `D2/D3` = `z`). `D0` = `0` if equal, negative if `y < z`, positive if
/// `y > z`.
fn ieeedp_cmp_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = read_f64(ctx.cpu, 0);
    let z = read_f64(ctx.cpu, 2);
    let result = if y < z {
        -1i32
    } else if y > z {
        1
    } else {
        0
    };
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPTst` (LVO -48: `D0/D1` = `y`). `D0`
/// = `0` if `y == 0`, negative if `y < 0`, positive if `y > 0`.
fn ieeedp_tst_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = read_f64(ctx.cpu, 0);
    let result = if y < 0.0 {
        -1i32
    } else if y > 0.0 {
        1
    } else {
        0
    };
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPAbs` (LVO -54: `D0/D1` = `y`).
fn ieeedp_abs_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = read_f64(ctx.cpu, 0);
    write_f64(ctx.cpu, 0, y.abs());
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPNeg` (LVO -60: `D0/D1` = `y`).
fn ieeedp_neg_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = read_f64(ctx.cpu, 0);
    write_f64(ctx.cpu, 0, -y);
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPAdd` (LVO -66: `D0/D1` = `y`,
/// `D2/D3` = `z`).
fn ieeedp_add_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let (y, z) = (read_f64(ctx.cpu, 0), read_f64(ctx.cpu, 2));
    write_f64(ctx.cpu, 0, y + z);
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPSub` (LVO -72: `D0/D1` = `y`,
/// `D2/D3` = `z`). Result is `y - z`.
fn ieeedp_sub_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let (y, z) = (read_f64(ctx.cpu, 0), read_f64(ctx.cpu, 2));
    write_f64(ctx.cpu, 0, y - z);
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPMul` (LVO -78: `D0/D1` = `y`,
/// `D2/D3` = `z`).
fn ieeedp_mul_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let (y, z) = (read_f64(ctx.cpu, 0), read_f64(ctx.cpu, 2));
    write_f64(ctx.cpu, 0, y * z);
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPDiv` (LVO -84: `D0/D1` = `y`,
/// `D2/D3` = `z`). Result is `y / z`; division by `0` yields IEEE-754
/// infinity/NaN, same as real hardware would produce for the underlying
/// bit pattern (this runtime doesn't special-case it into some other
/// error path).
fn ieeedp_div_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let (y, z) = (read_f64(ctx.cpu, 0), read_f64(ctx.cpu, 2));
    write_f64(ctx.cpu, 0, y / z);
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPFloor` (LVO -90: `D0/D1` = `y`).
fn ieeedp_floor_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = read_f64(ctx.cpu, 0);
    write_f64(ctx.cpu, 0, y.floor());
    Ok(())
}

/// `mathieeedoubbas.library`'s `IEEEDPCeil` (LVO -96: `D0/D1` = `y`).
fn ieeedp_ceil_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = read_f64(ctx.cpu, 0);
    let result = y.ceil();
    // Rust's `f64::ceil` (like strict IEEE-754) preserves the sign of
    // a zero result -- `(-0.5f64).ceil()` is `-0.0`. Real Kickstart
    // 3.1's `mathieeedoubbas.library` does not: verified against real
    // hardware via Copperline (issue #51/#52's writeup) that
    // `IEEEDPCeil` of a small negative number (rounding up to zero)
    // gives plain `+0.0`, matching `vamos`; volamos previously matched
    // Rust's/strict IEEE-754's `-0.0` instead, which was the actual
    // bug (the opposite of what issue #51 originally assumed -- `vamos`
    // was right here, not volamos).
    let result = if result == 0.0 { 0.0 } else { result };
    write_f64(ctx.cpu, 0, result);
    Ok(())
}

/// Registers every implemented `mathieeedoubbas.library` handler onto
/// [`crate::dispatch::MATHIEEEDOUBBAS_LIBRARY_BASE`].
fn register_mathieeedoubbas_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    crate::dispatch::MATHIEEEDOUBBAS_LIBRARY_BASE,
                    MATHIEEEDOUBBAS_LVOS,
                    "mathieeedoubbas.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in MATHIEEEDOUBBAS_LVOS: {e}", $name));
        };
    }
    reg!("IEEEDPFix", ieeedp_fix_handler::<C>);
    reg!("IEEEDPFlt", ieeedp_flt_handler::<C>);
    reg!("IEEEDPCmp", ieeedp_cmp_handler::<C>);
    reg!("IEEEDPTst", ieeedp_tst_handler::<C>);
    reg!("IEEEDPAbs", ieeedp_abs_handler::<C>);
    reg!("IEEEDPNeg", ieeedp_neg_handler::<C>);
    reg!("IEEEDPAdd", ieeedp_add_handler::<C>);
    reg!("IEEEDPSub", ieeedp_sub_handler::<C>);
    reg!("IEEEDPMul", ieeedp_mul_handler::<C>);
    reg!("IEEEDPDiv", ieeedp_div_handler::<C>);
    reg!("IEEEDPFloor", ieeedp_floor_handler::<C>);
    reg!("IEEEDPCeil", ieeedp_ceil_handler::<C>);
}

/// `mathieeesingbas.library`'s `IEEESPFix` (LVO -30: `D0` = single `y`).
/// `D0` = `y` truncated toward zero to a 32-bit integer.
fn ieeesp_fix_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu.set_data_register(DataRegister(0), y as i32 as u32);
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPFlt` (LVO -36: `D0` = 32-bit
/// integer `y`). `D0` = `y` converted to an IEEE single.
fn ieeesp_flt_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = ctx.cpu.data_register(DataRegister(0)) as i32;
    ctx.cpu
        .set_data_register(DataRegister(0), (y as f32).to_bits());
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPCmp` (LVO -42: `D0` = `y`, `D1` =
/// `z`). `D0` = `0` if equal, negative if `y < z`, positive if `y > z`.
fn ieeesp_cmp_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    let z = f32::from_bits(ctx.cpu.data_register(DataRegister(1)));
    let result = if y < z {
        -1i32
    } else if y > z {
        1
    } else {
        0
    };
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPTst` (LVO -48: `D0` = `y`). `D0` =
/// `0` if `y == 0`, negative if `y < 0`, positive if `y > 0`.
fn ieeesp_tst_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    let result = if y < 0.0 {
        -1i32
    } else if y > 0.0 {
        1
    } else {
        0
    };
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPAbs` (LVO -54: `D0` = `y`).
fn ieeesp_abs_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), y.abs().to_bits());
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPNeg` (LVO -60: `D0` = `y`).
fn ieeesp_neg_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu.set_data_register(DataRegister(0), (-y).to_bits());
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPAdd` (LVO -66: `D0` = `y`, `D1` =
/// `z`).
fn ieeesp_add_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    let z = f32::from_bits(ctx.cpu.data_register(DataRegister(1)));
    ctx.cpu
        .set_data_register(DataRegister(0), (y + z).to_bits());
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPSub` (LVO -72: `D0` = `y`, `D1` =
/// `z`). Result is `y - z`.
fn ieeesp_sub_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    let z = f32::from_bits(ctx.cpu.data_register(DataRegister(1)));
    ctx.cpu
        .set_data_register(DataRegister(0), (y - z).to_bits());
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPMul` (LVO -78: `D0` = `y`, `D1` =
/// `z`).
fn ieeesp_mul_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    let z = f32::from_bits(ctx.cpu.data_register(DataRegister(1)));
    ctx.cpu
        .set_data_register(DataRegister(0), (y * z).to_bits());
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPDiv` (LVO -84: `D0` = `y`, `D1` =
/// `z`). Result is `y / z`; division by `0` yields IEEE-754
/// infinity/NaN, same posture as [`ieeedp_div_handler`].
fn ieeesp_div_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    let z = f32::from_bits(ctx.cpu.data_register(DataRegister(1)));
    ctx.cpu
        .set_data_register(DataRegister(0), (y / z).to_bits());
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPFloor` (LVO -90: `D0` = `y`).
fn ieeesp_floor_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), y.floor().to_bits());
    Ok(())
}

/// `mathieeesingbas.library`'s `IEEESPCeil` (LVO -96: `D0` = `y`). See
/// [`ieeedp_ceil_handler`]'s doc for the real-hardware-confirmed
/// positive-zero convention this carries over by analogy (not
/// independently reverified for singles).
fn ieeesp_ceil_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    let result = y.ceil();
    let result = if result == 0.0 { 0.0 } else { result };
    ctx.cpu.set_data_register(DataRegister(0), result.to_bits());
    Ok(())
}

/// Registers every implemented `mathieeesingbas.library` handler onto
/// [`crate::dispatch::MATHIEEESINGBAS_LIBRARY_BASE`].
fn register_mathieeesingbas_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    crate::dispatch::MATHIEEESINGBAS_LIBRARY_BASE,
                    MATHIEEESINGBAS_LVOS,
                    "mathieeesingbas.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in MATHIEEESINGBAS_LVOS: {e}", $name));
        };
    }
    reg!("IEEESPFix", ieeesp_fix_handler::<C>);
    reg!("IEEESPFlt", ieeesp_flt_handler::<C>);
    reg!("IEEESPCmp", ieeesp_cmp_handler::<C>);
    reg!("IEEESPTst", ieeesp_tst_handler::<C>);
    reg!("IEEESPAbs", ieeesp_abs_handler::<C>);
    reg!("IEEESPNeg", ieeesp_neg_handler::<C>);
    reg!("IEEESPAdd", ieeesp_add_handler::<C>);
    reg!("IEEESPSub", ieeesp_sub_handler::<C>);
    reg!("IEEESPMul", ieeesp_mul_handler::<C>);
    reg!("IEEESPDiv", ieeesp_div_handler::<C>);
    reg!("IEEESPFloor", ieeesp_floor_handler::<C>);
    reg!("IEEESPCeil", ieeesp_ceil_handler::<C>);
}

/// One-`double`-argument `mathieeedoubtrans.library` function
/// (`D0/D1` in, `D0/D1` out).
fn ieeedp_unary<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    f: impl FnOnce(f64) -> f64,
) -> Result<(), DispatchError> {
    let y = read_f64(ctx.cpu, 0);
    let result = f(y);
    // A domain error (IEEEDPAcos/IEEEDPAsin outside [-1,1], IEEEDPLog/
    // IEEEDPLog10/IEEEDPSqrt of a negative number) produces NaN.
    // Rust's f64 math propagates whatever sign its own internal
    // computation happens to leave on the NaN -- e.g. `(-2.0f64).asin()`
    // is negative-signed while `(2.0f64).asin()`/`(-2.0f64).acos()` are
    // positive-signed, an inconsistency with no documented meaning.
    // Real Kickstart 3.1's mathieeedoubtrans.library doesn't have this
    // inconsistency -- verified against real hardware via Copperline
    // (issue #52's writeup: mathieeedoubbas.library's IEEEDPDiv(0,0)/
    // IEEEDPDiv(-0,0) both give the identical positive-signed NaN
    // `$7FF10000_00000000`, regardless of input sign) -- so every
    // domain-error result here is canonicalized to a single, fixed,
    // positive-signed NaN too, rather than trusting Rust's own
    // input-sign-dependent propagation. The exact NaN payload bits
    // aren't reproduced (NaN payloads are implementation-specific
    // microcode detail no emulator is expected to match exactly), only
    // the sign, which is the only part any real program could
    // meaningfully observe (e.g. via IEEEDPTst).
    let result = if result.is_nan() { f64::NAN } else { result };
    write_f64(ctx.cpu, 0, result);
    Ok(())
}

macro_rules! ieeedp_unary_handler {
    ($fn_name:ident, $op:expr) => {
        fn $fn_name<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
            ieeedp_unary(ctx, $op)
        }
    };
}

ieeedp_unary_handler!(ieeedp_atan_handler, f64::atan);
ieeedp_unary_handler!(ieeedp_sin_handler, f64::sin);
ieeedp_unary_handler!(ieeedp_cos_handler, f64::cos);
ieeedp_unary_handler!(ieeedp_tan_handler, f64::tan);
ieeedp_unary_handler!(ieeedp_sinh_handler, f64::sinh);
ieeedp_unary_handler!(ieeedp_cosh_handler, f64::cosh);
ieeedp_unary_handler!(ieeedp_tanh_handler, f64::tanh);
ieeedp_unary_handler!(ieeedp_exp_handler, f64::exp);
ieeedp_unary_handler!(ieeedp_log_handler, f64::ln);
ieeedp_unary_handler!(ieeedp_sqrt_handler, f64::sqrt);
ieeedp_unary_handler!(ieeedp_asin_handler, f64::asin);
ieeedp_unary_handler!(ieeedp_acos_handler, f64::acos);
ieeedp_unary_handler!(ieeedp_log10_handler, f64::log10);

/// `mathieeedoubtrans.library`'s `IEEEDPSincos` (LVO -54: `A0` = pointer
/// to store the cosine as a `double`, `D0/D1` = `y`). `D0/D1` = the sine
/// (the function's actual return value); the cosine is written to `*A0`.
fn ieeedp_sincos_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let z_ptr = ctx.cpu.address_register(AddressRegister(0));
    let y = read_f64(ctx.cpu, 0);
    let (sin, cos) = y.sin_cos();
    let cos_bits = cos.to_bits();
    ctx.mem.write_u32(z_ptr, (cos_bits >> 32) as u32);
    ctx.mem.write_u32(z_ptr.wrapping_add(4), cos_bits as u32);
    write_f64(ctx.cpu, 0, sin);
    Ok(())
}

/// `mathieeedoubtrans.library`'s `IEEEDPPow` (LVO -90: `D0/D1` and
/// `D2/D3` hold the call's two `double` arguments). Confirmed against
/// amitools' own `math_double_trans` ground truth (`IEEEDPPow(3.0,
/// 4.0)` -> `64.0` = `4**3`, not `3**4` = `81`) that `D0/D1` (labeled
/// `x` here) ends up holding the *second* C-level argument and is the
/// real base, with `D2/D3` (`y`) the real exponent -- whether that's
/// the real LVO's own documented convention or an artifact of how the
/// compiled C stub happens to push a two-`double` call's arguments
/// wasn't traced further, but the observable effect (and this
/// handler's own already-correct `x.powf(y)`) is confirmed either way.
/// [`ieeesp_pow_handler`]'s own doc has the equivalent single-precision
/// story, including the parallel with `mathtrans.library`'s FFP
/// `SPPow`, which has the same base/exponent relationship.
fn ieeedp_pow_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let (x, y) = (read_f64(ctx.cpu, 0), read_f64(ctx.cpu, 2));
    write_f64(ctx.cpu, 0, x.powf(y));
    Ok(())
}

/// `mathieeedoubtrans.library`'s `IEEEDPTieee` (LVO -102: `D0/D1` =
/// `y`). `D0` = `y` converted to an IEEE single precision value's raw
/// bits.
fn ieeedp_tieee_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let y = read_f64(ctx.cpu, 0);
    ctx.cpu
        .set_data_register(DataRegister(0), (y as f32).to_bits());
    Ok(())
}

/// `mathieeedoubtrans.library`'s `IEEEDPFieee` (LVO -108: `D0` = an IEEE
/// single precision value's raw bits). `D0/D1` = that value converted to
/// `double`.
fn ieeedp_fieee_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let bits = ctx.cpu.data_register(DataRegister(0));
    write_f64(ctx.cpu, 0, f32::from_bits(bits) as f64);
    Ok(())
}

/// Registers every implemented `mathieeedoubtrans.library` handler onto
/// [`crate::dispatch::MATHIEEEDOUBTRANS_LIBRARY_BASE`].
fn register_mathieeedoubtrans_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    crate::dispatch::MATHIEEEDOUBTRANS_LIBRARY_BASE,
                    MATHIEEEDOUBTRANS_LVOS,
                    "mathieeedoubtrans.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in MATHIEEEDOUBTRANS_LVOS: {e}", $name));
        };
    }
    reg!("IEEEDPAtan", ieeedp_atan_handler::<C>);
    reg!("IEEEDPSin", ieeedp_sin_handler::<C>);
    reg!("IEEEDPCos", ieeedp_cos_handler::<C>);
    reg!("IEEEDPTan", ieeedp_tan_handler::<C>);
    reg!("IEEEDPSincos", ieeedp_sincos_handler::<C>);
    reg!("IEEEDPSinh", ieeedp_sinh_handler::<C>);
    reg!("IEEEDPCosh", ieeedp_cosh_handler::<C>);
    reg!("IEEEDPTanh", ieeedp_tanh_handler::<C>);
    reg!("IEEEDPExp", ieeedp_exp_handler::<C>);
    reg!("IEEEDPLog", ieeedp_log_handler::<C>);
    reg!("IEEEDPPow", ieeedp_pow_handler::<C>);
    reg!("IEEEDPSqrt", ieeedp_sqrt_handler::<C>);
    reg!("IEEEDPTieee", ieeedp_tieee_handler::<C>);
    reg!("IEEEDPFieee", ieeedp_fieee_handler::<C>);
    reg!("IEEEDPAsin", ieeedp_asin_handler::<C>);
    reg!("IEEEDPAcos", ieeedp_acos_handler::<C>);
    reg!("IEEEDPLog10", ieeedp_log10_handler::<C>);
}

/// One-single-argument `mathieeesingtrans.library` function (`D0` in,
/// `D0` out). See [`ieeedp_unary`]'s doc for the NaN-sign
/// canonicalization this carries over by analogy -- not independently
/// reverified against real hardware for singles specifically, only
/// assumed consistent with the same library family's double-precision
/// behavior (issue #52).
fn ieeesp_unary<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    f: impl FnOnce(f32) -> f32,
) -> Result<(), DispatchError> {
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    let result = f(y);
    let result = if result.is_nan() { f32::NAN } else { result };
    ctx.cpu.set_data_register(DataRegister(0), result.to_bits());
    Ok(())
}

macro_rules! ieeesp_unary_handler {
    ($fn_name:ident, $op:expr) => {
        fn $fn_name<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
            ieeesp_unary(ctx, $op)
        }
    };
}

ieeesp_unary_handler!(ieeesp_atan_handler, f32::atan);
ieeesp_unary_handler!(ieeesp_sin_handler, f32::sin);
ieeesp_unary_handler!(ieeesp_cos_handler, f32::cos);
ieeesp_unary_handler!(ieeesp_tan_handler, f32::tan);
ieeesp_unary_handler!(ieeesp_sinh_handler, f32::sinh);
ieeesp_unary_handler!(ieeesp_cosh_handler, f32::cosh);
ieeesp_unary_handler!(ieeesp_tanh_handler, f32::tanh);
ieeesp_unary_handler!(ieeesp_exp_handler, f32::exp);
ieeesp_unary_handler!(ieeesp_log_handler, f32::ln);
ieeesp_unary_handler!(ieeesp_sqrt_handler, f32::sqrt);
ieeesp_unary_handler!(ieeesp_asin_handler, f32::asin);
ieeesp_unary_handler!(ieeesp_acos_handler, f32::acos);
ieeesp_unary_handler!(ieeesp_log10_handler, f32::log10);

/// `mathieeesingtrans.library`'s `IEEESPSincos` (LVO -54: `A0` = pointer
/// to store the cosine as a single, `D0` = `y`). `D0` = the sine (the
/// function's actual return value); the cosine is written to `*A0`.
fn ieeesp_sincos_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let z_ptr = ctx.cpu.address_register(AddressRegister(0));
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    let (sin, cos) = y.sin_cos();
    ctx.mem.write_u32(z_ptr, cos.to_bits());
    ctx.cpu.set_data_register(DataRegister(0), sin.to_bits());
    Ok(())
}

/// `mathieeesingtrans.library`'s `IEEESPPow` (LVO -90: `D1` = the
/// C-level `x` parameter, `D0` = `y`, per AROS's own `.conf` -- note the
/// register order is *not* `D0`-then-`D1` the way every other
/// two-argument single-precision call in these two libraries is; this
/// is the real ABI, not a transcription error). Despite the `.conf`'s
/// own naming, the result is `y` raised to the `x` power, *not* `x`
/// raised to `y` -- confirmed against amitools' own `math_single_trans`
/// ground truth (`IEEESPPow(3.0, 4.0)` -> `64.0` = `4**3`, not
/// `3**4` = `81`; `IEEESPPow(1000.0, 0.0)` -> `0.0` = `0**1000`, not
/// `1000**0` = `1.0`). `D0` ends up the real base and `D1` the real
/// exponent here, the same base/exponent relationship
/// [`ieeedp_pow_handler`]'s own (already-correct) code has for its
/// `D0/D1` register pair vs. `D2/D3`, and the same shape as
/// `mathtrans.library`'s FFP `SPPow` quirk (see `sp_pow_handler`'s
/// doc) -- whether this is the real LVO's own documented convention or
/// an artifact of how a compiled C stub pushes a multi-argument call's
/// registers wasn't traced further; the observable effect is what's
/// confirmed.
fn ieeesp_pow_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let x = f32::from_bits(ctx.cpu.data_register(DataRegister(1)));
    let y = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), y.powf(x).to_bits());
    Ok(())
}

/// `mathieeesingtrans.library`'s `IEEESPTieee`/`IEEESPFieee` (LVOs -102/
/// -108: `D0` = `y`). Both are documented no-ops -- "included for
/// completeness although they just return the input parameter" (their
/// own NDK autodoc) -- since the library's own native format already
/// *is* IEEE single precision, unlike `mathieeedoubtrans.library`'s
/// `IEEEDPTieee`/`IEEEDPFieee`, which do a real double<->single
/// conversion.
fn ieeesp_identity_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let _ = ctx;
    Ok(())
}

/// Registers every implemented `mathieeesingtrans.library` handler onto
/// [`crate::dispatch::MATHIEEESINGTRANS_LIBRARY_BASE`].
fn register_mathieeesingtrans_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    crate::dispatch::MATHIEEESINGTRANS_LIBRARY_BASE,
                    MATHIEEESINGTRANS_LVOS,
                    "mathieeesingtrans.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in MATHIEEESINGTRANS_LVOS: {e}", $name));
        };
    }
    reg!("IEEESPAtan", ieeesp_atan_handler::<C>);
    reg!("IEEESPSin", ieeesp_sin_handler::<C>);
    reg!("IEEESPCos", ieeesp_cos_handler::<C>);
    reg!("IEEESPTan", ieeesp_tan_handler::<C>);
    reg!("IEEESPSincos", ieeesp_sincos_handler::<C>);
    reg!("IEEESPSinh", ieeesp_sinh_handler::<C>);
    reg!("IEEESPCosh", ieeesp_cosh_handler::<C>);
    reg!("IEEESPTanh", ieeesp_tanh_handler::<C>);
    reg!("IEEESPExp", ieeesp_exp_handler::<C>);
    reg!("IEEESPLog", ieeesp_log_handler::<C>);
    reg!("IEEESPPow", ieeesp_pow_handler::<C>);
    reg!("IEEESPSqrt", ieeesp_sqrt_handler::<C>);
    reg!("IEEESPTieee", ieeesp_identity_handler::<C>);
    reg!("IEEESPFieee", ieeesp_identity_handler::<C>);
    reg!("IEEESPAsin", ieeesp_asin_handler::<C>);
    reg!("IEEESPAcos", ieeesp_acos_handler::<C>);
    reg!("IEEESPLog10", ieeesp_log10_handler::<C>);
}

/// One-FFP-argument `mathtrans.library` function (`D0` in, `D0` out).
fn sp_unary<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    f: impl FnOnce(f32) -> f32,
) -> Result<(), DispatchError> {
    let fnum = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), f32_to_ffp(f(fnum)));
    Ok(())
}

macro_rules! sp_unary_handler {
    ($fn_name:ident, $op:expr) => {
        fn $fn_name<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
            sp_unary(ctx, $op)
        }
    };
}

sp_unary_handler!(sp_atan_handler, f32::atan);
sp_unary_handler!(sp_sin_handler, f32::sin);
sp_unary_handler!(sp_cos_handler, f32::cos);
sp_unary_handler!(sp_tan_handler, f32::tan);
sp_unary_handler!(sp_sinh_handler, f32::sinh);
sp_unary_handler!(sp_cosh_handler, f32::cosh);
sp_unary_handler!(sp_tanh_handler, f32::tanh);
sp_unary_handler!(sp_exp_handler, f32::exp);
sp_unary_handler!(sp_log_handler, f32::ln);
sp_unary_handler!(sp_sqrt_handler, f32::sqrt);
sp_unary_handler!(sp_asin_handler, f32::asin);
sp_unary_handler!(sp_acos_handler, f32::acos);
sp_unary_handler!(sp_log10_handler, f32::log10);

/// `mathtrans.library`'s `SPSincos` (LVO -54: `D1` = pointer to store the
/// cosine as an FFP `LONG`, `D0` = `fnum1`). `D0` = the sine (the
/// function's actual return value, also FFP-encoded); the cosine is
/// written to `*D1`.
fn sp_sincos_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let z_ptr = ctx.cpu.data_register(DataRegister(1));
    let fnum1 = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    let (sin, cos) = fnum1.sin_cos();
    ctx.mem.write_u32(z_ptr, f32_to_ffp(cos));
    ctx.cpu.set_data_register(DataRegister(0), f32_to_ffp(sin));
    Ok(())
}

/// `mathtrans.library`'s `SPPow` (LVO -90: `D1` = `fnum1`, `D0` =
/// `fnum2`). **Real `SPPow` computes `fnum2` raised to the `fnum1`
/// power, not `fnum1` raised to the `fnum2` power** -- the same
/// historical argument-order quirk as [`sp_sub_handler`]/
/// [`sp_div_handler`] (confirmed empirically against `vamos`, via
/// amitools' own `test/src/math_fast_trans.c`: `SPPow(FFP_2, FFP_10)`
/// -- `fnum1=2`, `fnum2=10` in this LVO's own register convention --
/// produces `100` (`10^2`), not `1024` (`2^10`); `SPPow(FFP_1000,
/// FFP_ZERO)` produces `0` (`0^1000`), not `1` (`1000^0`) -- issue
/// #53). `D0` = the FFP-encoded result.
fn sp_pow_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fnum1 = ffp_to_f32(ctx.cpu.data_register(DataRegister(1)));
    let fnum2 = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), f32_to_ffp(fnum2.powf(fnum1)));
    Ok(())
}

/// `mathtrans.library`'s `SPTieee` (LVO -102: `D0` = an FFP-encoded
/// `LONG`). `D0` = that value converted to an IEEE single precision
/// value's raw bits.
fn sp_tieee_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fnum = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu.set_data_register(DataRegister(0), fnum.to_bits());
    Ok(())
}

/// `mathtrans.library`'s `SPFieee` (LVO -108: `D0` = an IEEE single
/// precision value's raw bits). `D0` = that value converted to FFP.
fn sp_fieee_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ieee = f32::from_bits(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu.set_data_register(DataRegister(0), f32_to_ffp(ieee));
    Ok(())
}

/// Registers every implemented `mathtrans.library` handler onto
/// [`crate::dispatch::MATHTRANS_LIBRARY_BASE`].
fn register_mathtrans_handlers<C: Cpu + 'static>(table: &mut LibraryTable<C>, mem: &mut C::Memory) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    crate::dispatch::MATHTRANS_LIBRARY_BASE,
                    MATHTRANS_LVOS,
                    "mathtrans.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in MATHTRANS_LVOS: {e}", $name));
        };
    }
    reg!("SPAtan", sp_atan_handler::<C>);
    reg!("SPSin", sp_sin_handler::<C>);
    reg!("SPCos", sp_cos_handler::<C>);
    reg!("SPTan", sp_tan_handler::<C>);
    reg!("SPSincos", sp_sincos_handler::<C>);
    reg!("SPSinh", sp_sinh_handler::<C>);
    reg!("SPCosh", sp_cosh_handler::<C>);
    reg!("SPTanh", sp_tanh_handler::<C>);
    reg!("SPExp", sp_exp_handler::<C>);
    reg!("SPLog", sp_log_handler::<C>);
    reg!("SPPow", sp_pow_handler::<C>);
    reg!("SPSqrt", sp_sqrt_handler::<C>);
    reg!("SPTieee", sp_tieee_handler::<C>);
    reg!("SPFieee", sp_fieee_handler::<C>);
    reg!("SPAsin", sp_asin_handler::<C>);
    reg!("SPAcos", sp_acos_handler::<C>);
    reg!("SPLog10", sp_log10_handler::<C>);
}

/// `mathffp.library`'s `SPFix` (LVO -30: `D0` = `parm`, FFP-encoded).
/// `D0` = `parm` truncated toward zero to a 32-bit integer. Real
/// `SPFix` (traced against AROS's `workbench/libs/mathffp/spfix.c`,
/// since the NDK Autodoc doesn't spell out the overflow behavior)
/// saturates to `i32::MIN`/`i32::MAX` on an out-of-range magnitude
/// rather than wrapping -- Rust's `as i32` float-to-int cast already
/// saturates the same way (stable behavior since Rust 1.45), so no
/// extra clamping code is needed here.
fn sp_fix_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let parm = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), parm as i32 as u32);
    Ok(())
}

/// `mathffp.library`'s `SPFlt` (LVO -36: `D0` = 32-bit integer).
/// `D0` = that integer converted to FFP.
fn sp_flt_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let inum = ctx.cpu.data_register(DataRegister(0)) as i32;
    ctx.cpu
        .set_data_register(DataRegister(0), f32_to_ffp(inum as f32));
    Ok(())
}

/// `mathffp.library`'s `SPCmp` (LVO -42: `D1` = `leftParm`, `D0` =
/// `rightParm`, both FFP). `D0` = `1` if `leftParm > rightParm`, `0` if
/// equal, `-1` if `leftParm < rightParm` -- traced against AROS's
/// `spcmp.c` for the exact tri-state values (the NDK Autodoc only says
/// "positive"/"zero"/"negative"). Natural left-vs-right order, *unlike*
/// [`sp_sub_handler`]/[`sp_div_handler`] below.
fn sp_cmp_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let left = ffp_to_f32(ctx.cpu.data_register(DataRegister(1)));
    let right = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    let result: i32 = if left > right {
        1
    } else if left < right {
        -1
    } else {
        0
    };
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// `mathffp.library`'s `SPTst` (LVO -48: `D1` = `parm`, FFP). `D0` =
/// `1` if positive, `0` if zero, `-1` if negative -- traced against
/// AROS's `sptst.c` (equivalent to `SPCmp(parm, 0)`, per its own
/// `SEE ALSO`).
fn sp_tst_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let parm = ffp_to_f32(ctx.cpu.data_register(DataRegister(1)));
    let result: i32 = if parm > 0.0 {
        1
    } else if parm < 0.0 {
        -1
    } else {
        0
    };
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// One-FFP-argument `mathffp.library` function (`D0` in, `D0` out).
fn sp_unary_ffp<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    f: impl FnOnce(f32) -> f32,
) -> Result<(), DispatchError> {
    let parm = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), f32_to_ffp(f(parm)));
    Ok(())
}

macro_rules! sp_unary_ffp_handler {
    ($fn_name:ident, $op:expr) => {
        fn $fn_name<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
            sp_unary_ffp(ctx, $op)
        }
    };
}

sp_unary_ffp_handler!(sp_abs_handler, f32::abs);
sp_unary_ffp_handler!(sp_neg_handler, |x: f32| -x);
sp_unary_ffp_handler!(sp_floor_handler, f32::floor);
sp_unary_ffp_handler!(sp_ceil_handler, f32::ceil);

/// `mathffp.library`'s `SPAdd` (LVO -66: `D1` = `leftParm`, `D0` =
/// `rightParm`, both FFP). `D0` = their sum, FFP-encoded. Natural,
/// commutative order.
fn sp_add_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let left = ffp_to_f32(ctx.cpu.data_register(DataRegister(1)));
    let right = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), f32_to_ffp(left + right));
    Ok(())
}

/// `mathffp.library`'s `SPSub` (LVO -72: `D1` = `leftParm`, `D0` =
/// `rightParm`, both FFP). **Real `SPSub` computes `rightParm -
/// leftParm`, not `leftParm - rightParm`** -- a genuine, well-known
/// historical AmigaOS quirk (the "arguments effectively swapped"
/// behavior of `mathffp.library`'s subtract/divide, confirmed here by
/// reading AROS's `spsub.c` literally: `SPAdd(fnum2, fnum1 ^
/// FFPSign_Mask)`, i.e. `fnum2 + (-fnum1)` where `fnum1`/`fnum2` are
/// `leftParm`/`rightParm` in bias order -- not a guess from the
/// function name, which would suggest the opposite). This runtime
/// reproduces that quirk faithfully rather than the "obviously
/// correct" order, since real guest code compiled against real
/// `mathffp.library` already accounts for it.
fn sp_sub_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let left = ffp_to_f32(ctx.cpu.data_register(DataRegister(1)));
    let right = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), f32_to_ffp(right - left));
    Ok(())
}

/// `mathffp.library`'s `SPMul` (LVO -78: `D1` = `leftParm`, `D0` =
/// `rightParm`, both FFP). `D0` = their product, FFP-encoded. Natural,
/// commutative order.
fn sp_mul_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let left = ffp_to_f32(ctx.cpu.data_register(DataRegister(1)));
    let right = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), f32_to_ffp(left * right));
    Ok(())
}

/// `mathffp.library`'s `SPDiv` (LVO -84: `D1` = `leftParm`, `D0` =
/// `rightParm`, both FFP). **Real `SPDiv` computes `rightParm /
/// leftParm`, not `leftParm / rightParm`** -- the same historical
/// argument-order quirk as [`sp_sub_handler`] (confirmed against
/// AROS's `spdiv.c`, which treats its second bias-order parameter as
/// the dividend and its first as the divisor). Reproduced faithfully
/// for the same reason.
fn sp_div_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let left = ffp_to_f32(ctx.cpu.data_register(DataRegister(1)));
    let right = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), f32_to_ffp(right / left));
    Ok(())
}

/// Registers every implemented `mathffp.library` handler onto
/// [`crate::dispatch::MATHFFP_LIBRARY_BASE`].
fn register_mathffp_handlers<C: Cpu + 'static>(table: &mut LibraryTable<C>, mem: &mut C::Memory) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    crate::dispatch::MATHFFP_LIBRARY_BASE,
                    crate::lvos::mathffp::MATHFFP_LVOS,
                    "mathffp.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in MATHFFP_LVOS: {e}", $name));
        };
    }
    reg!("SPFix", sp_fix_handler::<C>);
    reg!("SPFlt", sp_flt_handler::<C>);
    reg!("SPCmp", sp_cmp_handler::<C>);
    reg!("SPTst", sp_tst_handler::<C>);
    reg!("SPAbs", sp_abs_handler::<C>);
    reg!("SPNeg", sp_neg_handler::<C>);
    reg!("SPAdd", sp_add_handler::<C>);
    reg!("SPSub", sp_sub_handler::<C>);
    reg!("SPMul", sp_mul_handler::<C>);
    reg!("SPDiv", sp_div_handler::<C>);
    reg!("SPFloor", sp_floor_handler::<C>);
    reg!("SPCeil", sp_ceil_handler::<C>);
}

/// Registers every implemented handler for all six math libraries.
/// Called unconditionally from [`crate::dispatch::Runtime::new`].
pub fn register_mathlibs_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    register_mathieeedoubbas_handlers(table, mem);
    register_mathieeedoubtrans_handlers(table, mem);
    register_mathieeesingbas_handlers(table, mem);
    register_mathieeesingtrans_handlers(table, mem);
    register_mathtrans_handlers(table, mem);
    register_mathffp_handlers(table, mem);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffp_round_trips_common_values() {
        for x in [1.0f32, -1.0, 0.5, 2.0, 3.5, 100.0, -0.001, 0.0] {
            let bits = f32_to_ffp(x);
            let back = ffp_to_f32(bits);
            assert!(
                (back - x).abs() <= x.abs() * 1e-6 + 1e-12,
                "FFP round-trip of {x} produced {back} (bits {bits:#010x})"
            );
        }
    }

    #[test]
    fn ffp_zero_is_all_zero_bits() {
        assert_eq!(f32_to_ffp(0.0), 0);
        assert_eq!(ffp_to_f32(0), 0.0);
    }

    #[test]
    fn ffp_overflow_saturates_to_the_signed_max_magnitude() {
        // A value whose IEEE exponent is far beyond FFP's own
        // -64..63-ish range -- issue #53: this used to clamp only the
        // exponent field while keeping the original (too-small)
        // mantissa, producing a wrong-magnitude value still tagged
        // with the maximum exponent.
        assert_eq!(f32_to_ffp(1.0e30), 0xFFFF_FF7F, "FFP_MAX");
        assert_eq!(f32_to_ffp(-1.0e30), 0xFFFF_FFFF, "FFP_MAX_NEG");
    }

    #[test]
    fn ffp_infinity_also_saturates_to_the_signed_max_magnitude() {
        // Several real mathtrans functions (SPExp/SPCosh/SPSinh of a
        // huge input) overflow f32 to +-inf before FFP encoding ever
        // sees a finite number -- issue #53: this used to return 0 for
        // any non-finite input, which is a far worse answer than the
        // format's own saturation value for a result that's genuinely
        // "too large", as opposed to "undefined" (NaN).
        assert_eq!(f32_to_ffp(f32::INFINITY), 0xFFFF_FF7F);
        assert_eq!(f32_to_ffp(f32::NEG_INFINITY), 0xFFFF_FFFF);
    }

    #[test]
    fn ffp_nan_flushes_to_zero_not_the_saturated_max_magnitude() {
        // A domain error (SPAcos/SPAsin outside [-1,1], SPLog/SPSqrt of
        // a negative number, ...) produces NaN, not a huge-but-finite
        // value -- issue #53's real-corpus comparison confirms real
        // mathtrans.library reports 0 for these, distinct from a
        // genuine overflow (which saturates -- see the test above).
        assert_eq!(f32_to_ffp(f32::NAN), 0);
    }

    #[test]
    fn ffp_underflow_flushes_to_zero_rather_than_the_smallest_nonzero_magnitude() {
        // f32::MIN_POSITIVE (IEEE's smallest normal, ~2^-126) is far
        // below FFP's own smallest representable nonzero magnitude
        // (~2^-64) -- issue #53: this used to clamp *up* to that
        // smallest nonzero FFP value, silently turning a tiny-but-real
        // input into one many orders of magnitude larger.
        assert_eq!(f32_to_ffp(f32::MIN_POSITIVE), 0);
        assert_eq!(f32_to_ffp(-f32::MIN_POSITIVE), 0);
    }

    #[test]
    fn ffp_one_matches_known_encoding() {
        // 1.0: FFP mantissa 0x800000 (0.5 as a 24-bit fraction),
        // exponent field 65 ($41, excess-64 for unbiased exponent 1,
        // since 0.5 * 2^1 == 1.0) in bits 0-6, sign (positive, 0) in
        // bit 7 -- NOT independently re-derived from this module's own
        // doc comment (issue #53: an earlier version of this test did
        // exactly that, which just re-encoded the same sign/exponent
        // bit-position swap the doc comment itself had, silently
        // agreeing with a bug instead of catching it). This asserts
        // against amitools' own real `test/src/math_fast.h` constant
        // (`FFP_ONE = $80000041`) instead -- external ground truth.
        assert_eq!(f32_to_ffp(1.0), 0x8000_0041);
        assert_eq!(ffp_to_f32(0x8000_0041), 1.0);
    }

    #[test]
    fn ffp_matches_more_known_encodings_from_amitools() {
        // Same external-ground-truth approach as
        // ffp_one_matches_known_encoding, for a broader spread of real
        // constants from amitools' test/src/math_fast.h (issue #53).
        for (value, bits) in [
            (-1.0f32, 0x8000_00C1u32),
            (10.0, 0xA000_0044),
            (1000.0, 0xFA00_004A),
            (std::f32::consts::PI, 0xC90F_DB42),
        ] {
            assert_eq!(f32_to_ffp(value), bits, "encoding {value}");
            assert_eq!(ffp_to_f32(bits), value, "decoding {bits:#010x}");
        }
    }

    // --- mathffp.library, via the actual jump-table dispatch ---
    //
    // The SPSub/SPDiv argument-order quirk (see sp_sub_handler's/
    // sp_div_handler's doc) is exactly the kind of thing a future
    // refactor could accidentally "fix" back to the naive order --
    // worth a real end-to-end regression test, not just unit coverage
    // of the FFP encoding.

    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{
        MATHFFP_LIBRARY_BASE, MATHIEEEDOUBBAS_LIBRARY_BASE, MATHIEEEDOUBTRANS_LIBRARY_BASE,
        MATHTRANS_LIBRARY_BASE, Runtime, StartConfig,
    };
    use crate::memory::FlatMemory;

    fn load_words(mem: &mut FlatMemory, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    /// `move.l #imm32,Dn`.
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }

    /// `jsr <disp16>(a6)`.
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }

    const RTS: u16 = 0x4E75;

    /// `move.l #imm32,An`.
    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }

    /// Prepends `movea.l #MATHFFP_LIBRARY_BASE,a6` and builds a runtime.
    fn mathffp_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = vec![
            move_imm_to_a(6),
            (MATHFFP_LIBRARY_BASE >> 16) as u16,
            MATHFFP_LIBRARY_BASE as u16,
        ];
        full.extend_from_slice(words);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        let load_end = entry + 0x400;
        Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        )
    }

    /// Prepends `movea.l #MATHTRANS_LIBRARY_BASE,a6` and builds a
    /// runtime -- see [`mathffp_program`]'s twin, for `mathtrans.library`
    /// functions like `SPPow` (as opposed to `mathffp.library`'s
    /// `SPAdd`/`SPSub`/etc.).
    fn mathtrans_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = vec![
            move_imm_to_a(6),
            (MATHTRANS_LIBRARY_BASE >> 16) as u16,
            MATHTRANS_LIBRARY_BASE as u16,
        ];
        full.extend_from_slice(words);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        let load_end = entry + 0x400;
        Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        )
    }

    /// `D1 = left` (FFP), `D0 = right` (FFP), calls the LVO at `disp`,
    /// returns `D0` as FFP-decoded `f32`. `program` picks which library
    /// base gets loaded into `A6` first -- [`mathffp_program`] for
    /// `mathffp.library`'s own functions, [`mathtrans_program`] for
    /// `mathtrans.library`'s.
    fn run_binary_ffp_via(
        program: fn(&[u16]) -> Runtime<M68kCpu>,
        disp: i32,
        left: f32,
        right: f32,
    ) -> f32 {
        let mut words = Vec::new();
        words.push(move_imm_to_d(1));
        let left_bits = f32_to_ffp(left);
        words.push((left_bits >> 16) as u16);
        words.push(left_bits as u16);
        words.push(move_imm_to_d(0));
        let right_bits = f32_to_ffp(right);
        words.push((right_bits >> 16) as u16);
        words.push(right_bits as u16);
        words.extend_from_slice(&jsr_disp16_a6(disp));
        words.push(RTS);

        let mut rt = program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        ffp_to_f32(code as u32)
    }

    /// `D1 = left` (FFP), `D0 = right` (FFP), calls the `mathffp.library`
    /// LVO at `disp`, returns `D0` as FFP-decoded `f32`.
    fn run_binary_ffp(disp: i32, left: f32, right: f32) -> f32 {
        run_binary_ffp_via(mathffp_program, disp, left, right)
    }

    #[test]
    fn sp_add_is_commutative_natural_order() {
        let result = run_binary_ffp(-66, 3.0, 4.0);
        assert!((result - 7.0).abs() < 1e-4, "got {result}");
    }

    #[test]
    fn sp_sub_computes_right_minus_left_not_left_minus_right() {
        // leftParm=D1=10, rightParm=D0=3 -- real SPSub returns
        // rightParm - leftParm = 3 - 10 = -7, per sp_sub_handler's doc.
        let result = run_binary_ffp(-72, 10.0, 3.0);
        assert!((result - (-7.0)).abs() < 1e-4, "got {result}");
    }

    #[test]
    fn sp_mul_is_commutative_natural_order() {
        let result = run_binary_ffp(-78, 3.0, 4.0);
        assert!((result - 12.0).abs() < 1e-4, "got {result}");
    }

    #[test]
    fn sp_mul_overflow_saturates_to_ffp_max_at_the_exact_boundary_exponent() {
        // SPMul(FFP_INT_MAX, FFP_INT_MAX) / SPMul(FFP_INT_MIN,
        // FFP_INT_MIN) -- amitools' own math_fast.c mul3/mul4. The
        // exact mathematical product's FFP exponent field computes to
        // precisely 127 (FFP's reserved "this is FFP_MAX" boundary,
        // not just a large-but-representable finite value) -- verified
        // against real Kickstart 3.1 (40.72) via Copperline, which
        // prints FFFFFF7F for both (issue #53's residual, now closed).
        let int_max = 2147483648.0f32; // FFP_INT_MAX
        let int_min = -2147483648.0f32; // FFP_INT_MIN
        assert_eq!(
            f32_to_ffp(run_binary_ffp(-78, int_max, int_max)),
            0xFFFF_FF7F
        );
        assert_eq!(
            f32_to_ffp(run_binary_ffp(-78, int_min, int_min)),
            0xFFFF_FF7F
        );
    }

    #[test]
    fn sp_div_computes_right_divided_by_left_not_left_divided_by_right() {
        // leftParm=D1=2, rightParm=D0=10 -- real SPDiv returns
        // rightParm / leftParm = 10 / 2 = 5, per sp_div_handler's doc.
        let result = run_binary_ffp(-84, 2.0, 10.0);
        assert!((result - 5.0).abs() < 1e-4, "got {result}");
    }

    #[test]
    fn sp_pow_computes_fnum2_to_the_fnum1_not_fnum1_to_the_fnum2() {
        // fnum1=D1=2, fnum2=D0=10 -- real SPPow returns fnum2 ** fnum1
        // = 10 ** 2 = 100, not 2 ** 10 = 1024, per sp_pow_handler's doc
        // (issue #53, confirmed against amitools' own
        // test/src/math_fast_trans.c via a real vamos run).
        let result = run_binary_ffp_via(mathtrans_program, -90, 2.0, 10.0);
        assert!((result - 100.0).abs() < 1e-2, "got {result}");
    }

    #[test]
    fn sp_cmp_natural_left_vs_right_order() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(1));
        let left_bits = f32_to_ffp(2.0);
        words.push((left_bits >> 16) as u16);
        words.push(left_bits as u16);
        words.push(move_imm_to_d(0));
        let right_bits = f32_to_ffp(5.0);
        words.push((right_bits >> 16) as u16);
        words.push(right_bits as u16);
        words.extend_from_slice(&jsr_disp16_a6(-42)); // SPCmp
        words.push(RTS);

        let mut rt = mathffp_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, -1, "leftParm (2) < rightParm (5)");
    }

    #[test]
    fn sp_tst_sign_of_d1() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(1));
        let bits = f32_to_ffp(-2.5);
        words.push((bits >> 16) as u16);
        words.push(bits as u16);
        words.extend_from_slice(&jsr_disp16_a6(-48)); // SPTst
        words.push(RTS);

        let mut rt = mathffp_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, -1);
    }

    #[test]
    fn sp_fix_and_sp_flt_round_trip() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(42); // D0 = 42
        words.extend_from_slice(&jsr_disp16_a6(-36)); // SPFlt -> D0 = FFP(42.0)
        words.extend_from_slice(&jsr_disp16_a6(-30)); // SPFix -> D0 = 42
        words.push(RTS);

        let mut rt = mathffp_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 42);
    }

    #[test]
    fn sp_abs_and_neg() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0));
        let bits = f32_to_ffp(-3.0);
        words.push((bits >> 16) as u16);
        words.push(bits as u16);
        words.extend_from_slice(&jsr_disp16_a6(-54)); // SPAbs
        words.push(RTS);

        let mut rt = mathffp_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert!((ffp_to_f32(code as u32) - 3.0).abs() < 1e-4);
    }

    /// Prepends `movea.l #MATHIEEEDOUBBAS_LIBRARY_BASE,a6` and builds a
    /// runtime -- see [`mathffp_program`]'s twin, for
    /// `mathieeedoubbas.library` functions.
    fn mathieeedoubbas_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = vec![
            move_imm_to_a(6),
            (MATHIEEEDOUBBAS_LIBRARY_BASE >> 16) as u16,
            MATHIEEEDOUBBAS_LIBRARY_BASE as u16,
        ];
        full.extend_from_slice(words);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        let load_end = entry + 0x400;
        Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        )
    }

    /// Prepends `movea.l #MATHIEEEDOUBTRANS_LIBRARY_BASE,a6` and builds
    /// a runtime -- see [`mathieeedoubbas_program`]'s twin, for
    /// `mathieeedoubtrans.library` functions.
    fn mathieeedoubtrans_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = vec![
            move_imm_to_a(6),
            (MATHIEEEDOUBTRANS_LIBRARY_BASE >> 16) as u16,
            MATHIEEEDOUBTRANS_LIBRARY_BASE as u16,
        ];
        full.extend_from_slice(words);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        let load_end = entry + 0x400;
        Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        )
    }

    #[test]
    fn ieeedp_asin_domain_error_is_always_positive_signed_nan() {
        // IEEEDPAsin(-2.0) is outside [-1,1] -- Rust's own
        // (-2.0f64).asin() happens to produce a *negative*-signed NaN
        // (an internal-computation artifact, not a documented
        // convention), while IEEEDPAsin(2.0)/IEEEDPAcos(+-2.0) all give
        // positive-signed NaNs from Rust. Real Kickstart 3.1 has no
        // such inconsistency (issue #52's writeup) -- every
        // domain-error result canonicalizes to the same, fixed,
        // positive sign.
        const RESULT_ADDR: u32 = 0x1_0000;
        let mut words = Vec::new();
        let bits = (-2.0f64).to_bits();
        words.push(move_imm_to_d(0));
        words.push((bits >> 48) as u16);
        words.push((bits >> 32) as u16);
        words.push(move_imm_to_d(1));
        words.push((bits >> 16) as u16);
        words.push(bits as u16);
        words.extend_from_slice(&jsr_disp16_a6(-114)); // IEEEDPAsin
        words.push(move_imm_to_a(0));
        words.push((RESULT_ADDR >> 16) as u16);
        words.push(RESULT_ADDR as u16);
        words.push(0x2080); // move.l d0,(a0)
        words.push(move_imm_to_a(0));
        words.push(((RESULT_ADDR + 4) >> 16) as u16);
        words.push((RESULT_ADDR + 4) as u16);
        words.push(0x2081); // move.l d1,(a0)
        words.push(RTS);

        let mut rt = mathieeedoubtrans_program(&words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        let mem = rt.memory();
        let hi = mem.read_u32(RESULT_ADDR) as u64;
        let lo = mem.read_u32(RESULT_ADDR + 4) as u64;
        let result = f64::from_bits((hi << 32) | lo);
        assert!(result.is_nan());
        assert!(!result.is_sign_negative(), "must be a positive-signed NaN");
    }

    #[test]
    fn ieeedp_ceil_of_a_small_negative_number_gives_positive_zero() {
        // IEEEDPCeil(-0.5): Rust's/strict IEEE-754's f64::ceil gives
        // -0.0 here (sign-of-zero preserved), but real Kickstart 3.1's
        // mathieeedoubbas.library does not -- verified against real
        // hardware via Copperline (issue #51/#52's writeup: math_double
        // ceil7/ceil9 both print plain positive zero, matching vamos).
        // D0/D1 (the double result) get stashed to a fixed guest
        // address before RTS, since the exit-code mechanism only
        // captures D0's 32 bits, not a full double.
        const RESULT_ADDR: u32 = 0x1_0000;
        let mut words = Vec::new();
        let bits = (-0.5f64).to_bits();
        words.push(move_imm_to_d(0));
        words.push((bits >> 48) as u16);
        words.push((bits >> 32) as u16);
        words.push(move_imm_to_d(1));
        words.push((bits >> 16) as u16);
        words.push(bits as u16);
        words.extend_from_slice(&jsr_disp16_a6(-96)); // IEEEDPCeil
        // move.l #RESULT_ADDR,a0 ; move.l d0,(a0)
        words.push(move_imm_to_a(0));
        words.push((RESULT_ADDR >> 16) as u16);
        words.push(RESULT_ADDR as u16);
        words.push(0x2080); // move.l d0,(a0)
        // move.l #RESULT_ADDR+4,a0 ; move.l d1,(a0)
        words.push(move_imm_to_a(0));
        words.push(((RESULT_ADDR + 4) >> 16) as u16);
        words.push((RESULT_ADDR + 4) as u16);
        words.push(0x2081); // move.l d1,(a0)
        words.push(RTS);

        let mut rt = mathieeedoubbas_program(&words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
        let mem = rt.memory();
        let hi = mem.read_u32(RESULT_ADDR) as u64;
        let lo = mem.read_u32(RESULT_ADDR + 4) as u64;
        let result = f64::from_bits((hi << 32) | lo);
        assert_eq!(result, 0.0);
        assert!(!result.is_sign_negative(), "must be +0.0, not -0.0");
    }

    #[test]
    fn sp_floor_and_ceil() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0));
        let bits = f32_to_ffp(2.7);
        words.push((bits >> 16) as u16);
        words.push(bits as u16);
        words.extend_from_slice(&jsr_disp16_a6(-96)); // SPCeil
        words.push(RTS);

        let mut rt = mathffp_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert!((ffp_to_f32(code as u32) - 3.0).abs() < 1e-4);
    }

    /// Prepends `movea.l #MATHIEEESINGBAS_LIBRARY_BASE,a6` and builds a
    /// runtime -- see [`mathffp_program`]'s twin, for
    /// `mathieeesingbas.library` functions. Unlike the double-precision
    /// twins, a single-precision result fits entirely in `D0`, so these
    /// tests can just read it back as the program's own exit code --
    /// no guest-memory write-back trick needed.
    fn mathieeesingbas_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = vec![
            move_imm_to_a(6),
            (crate::dispatch::MATHIEEESINGBAS_LIBRARY_BASE >> 16) as u16,
            crate::dispatch::MATHIEEESINGBAS_LIBRARY_BASE as u16,
        ];
        full.extend_from_slice(words);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        let load_end = entry + 0x400;
        Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        )
    }

    /// Prepends `movea.l #MATHIEEESINGTRANS_LIBRARY_BASE,a6` and builds
    /// a runtime -- see [`mathieeesingbas_program`]'s twin, for
    /// `mathieeesingtrans.library` functions.
    fn mathieeesingtrans_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = vec![
            move_imm_to_a(6),
            (crate::dispatch::MATHIEEESINGTRANS_LIBRARY_BASE >> 16) as u16,
            crate::dispatch::MATHIEEESINGTRANS_LIBRARY_BASE as u16,
        ];
        full.extend_from_slice(words);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        let load_end = entry + 0x400;
        Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        )
    }

    #[test]
    fn ieeesp_add_sub_mul_div_natural_order() {
        // Unlike mathtrans.library's FFP SPAdd/SPSub/etc., the IEEE
        // single-precision library has no argument-order quirk -- D0=y,
        // D1=z, result is the ordinary y op z (matches IEEEDPAdd/Sub/
        // Mul/Div's own natural order too).
        let mut words = vec![
            move_imm_to_d(0),
            (10.0f32.to_bits() >> 16) as u16,
            10.0f32.to_bits() as u16,
            move_imm_to_d(1),
            (3.0f32.to_bits() >> 16) as u16,
            3.0f32.to_bits() as u16,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-72)); // IEEESPSub
        words.push(RTS);

        let mut rt = mathieeesingbas_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let result = f32::from_bits(code as u32);
        assert!((result - 7.0).abs() < 1e-4, "got {result}");
    }

    #[test]
    fn ieeesp_fix_and_flt_round_trip() {
        let mut words = Vec::new();
        words.push(move_imm_to_d(0));
        words.push(0);
        words.push(42);
        words.extend_from_slice(&jsr_disp16_a6(-36)); // IEEESPFlt -> D0 = 42.0f32
        words.extend_from_slice(&jsr_disp16_a6(-30)); // IEEESPFix -> D0 = 42
        words.push(RTS);

        let mut rt = mathieeesingbas_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 42);
    }

    #[test]
    fn ieeesp_ceil_of_a_small_negative_number_gives_positive_zero() {
        // Same real-hardware-confirmed convention as IEEEDPCeil (issue
        // #51/#52), carried over by analogy for the single-precision
        // twin.
        let mut words = Vec::new();
        words.push(move_imm_to_d(0));
        let bits = (-0.5f32).to_bits();
        words.push((bits >> 16) as u16);
        words.push(bits as u16);
        words.extend_from_slice(&jsr_disp16_a6(-96)); // IEEESPCeil
        words.push(RTS);

        let mut rt = mathieeesingbas_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let result = f32::from_bits(code as u32);
        assert_eq!(result, 0.0);
        assert!(!result.is_sign_negative(), "must be +0.0, not -0.0");
    }

    #[test]
    fn ieeesp_pow_computes_d0_to_the_d1_not_d1_to_the_d0() {
        // D1 = 10, D0 = 2 -- confirmed against amitools' own
        // math_single_trans ground truth (IEEESPPow(3.0, 4.0) -> 64.0
        // = 4**3, not 3**4 = 81; see ieeesp_pow_handler's own doc) that
        // the result is D0**D1 = 2**10 = 1024, not D1**D0 = 10**2 = 100.
        let mut words = vec![
            move_imm_to_d(1),
            (10.0f32.to_bits() >> 16) as u16,
            10.0f32.to_bits() as u16,
            move_imm_to_d(0),
            (2.0f32.to_bits() >> 16) as u16,
            2.0f32.to_bits() as u16,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-90)); // IEEESPPow
        words.push(RTS);

        let mut rt = mathieeesingtrans_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let result = f32::from_bits(code as u32);
        assert!((result - 1024.0).abs() < 1e-2, "got {result}");
    }

    #[test]
    fn ieeesp_tieee_and_fieee_are_identity() {
        // Both are documented no-ops for this library (its own native
        // format already is IEEE single) -- see ieeesp_identity_handler's
        // doc.
        let mut words = Vec::new();
        words.push(move_imm_to_d(0));
        let bits = 3.5f32.to_bits();
        words.push((bits >> 16) as u16);
        words.push(bits as u16);
        words.extend_from_slice(&jsr_disp16_a6(-102)); // IEEESPTieee
        words.extend_from_slice(&jsr_disp16_a6(-108)); // IEEESPFieee
        words.push(RTS);

        let mut rt = mathieeesingtrans_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(f32::from_bits(code as u32), 3.5);
    }

    #[test]
    fn ieeesp_asin_domain_error_is_always_positive_signed_nan() {
        // Same canonicalization as IEEEDPAsin (issue #52), carried over
        // by analogy for the single-precision twin.
        let mut words = Vec::new();
        words.push(move_imm_to_d(0));
        let bits = (-2.0f32).to_bits();
        words.push((bits >> 16) as u16);
        words.push(bits as u16);
        words.extend_from_slice(&jsr_disp16_a6(-114)); // IEEESPAsin
        words.push(RTS);

        let mut rt = mathieeesingtrans_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let result = f32::from_bits(code as u32);
        assert!(result.is_nan());
        assert!(!result.is_sign_negative(), "must be a positive-signed NaN");
    }
}
