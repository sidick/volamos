//! `mathieeedoubbas.library`/`mathieeedoubtrans.library`/`mathtrans.library`:
//! real math library implementations, not just [`crate::dispatch`]'s
//! "vamos escape hatch" fake traps -- see
//! `crate::dispatch::STANDARD_WORKBENCH_LIBRARIES`'s doc for why these
//! specific libraries (unlike an arbitrary optional disk library) are
//! treated as always-present. Found missing while running the real
//! `PhxAss` assembler.
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
//! encoding, not IEEE-754. Per <https://wiki.amigaos.net/wiki/Math_Libraries>:
//! bit 0 is the sign, bits 1-7 are a 7-bit exponent in excess-64 (so a
//! stored field of `$40` means an unbiased exponent of `0`), and bits 8-31
//! are a 24-bit normalized mantissa treated as a fraction in `[0.5, 1)`
//! (or the whole value is `0` if every bit is `0`). [`ffp_to_f32`]/
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
    let sign = if bits & 1 != 0 { -1.0f32 } else { 1.0f32 };
    let exponent = ((bits >> 1) & 0x7F) as i32 - 64;
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
    if value == 0.0 || !value.is_finite() {
        // No FFP encoding for +-inf/NaN; 0 is the closest honest answer
        // this runtime can give without inventing a value real FFP
        // hardware never had to represent either.
        return 0;
    }
    let bits = value.to_bits();
    let sign = (bits >> 31) & 1;
    let raw_exp = (bits >> 23) & 0xFF;
    let mantissa24 = (1u32 << 23) | (bits & 0x7F_FFFF);

    let e_ffp_field = raw_exp as i32 - 62;
    // FFP's exponent field is 7 bits (0..=127); saturate rather than
    // wrap on overflow/underflow -- see this module's doc comment.
    let e_ffp_field = e_ffp_field.clamp(1, 127) as u32;

    (mantissa24 << 8) | (e_ffp_field << 1) | sign
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
    write_f64(ctx.cpu, 0, y.ceil());
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

/// One-`double`-argument `mathieeedoubtrans.library` function
/// (`D0/D1` in, `D0/D1` out).
fn ieeedp_unary<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    f: impl FnOnce(f64) -> f64,
) -> Result<(), DispatchError> {
    let y = read_f64(ctx.cpu, 0);
    write_f64(ctx.cpu, 0, f(y));
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

/// `mathieeedoubtrans.library`'s `IEEEDPPow` (LVO -90: `D0/D1` = `x`,
/// `D2/D3` = `y`). Result is `x` raised to the `y` power.
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
/// `fnum2`). `D0` = `fnum1` raised to the `fnum2` power, FFP-encoded.
fn sp_pow_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fnum1 = ffp_to_f32(ctx.cpu.data_register(DataRegister(1)));
    let fnum2 = ffp_to_f32(ctx.cpu.data_register(DataRegister(0)));
    ctx.cpu
        .set_data_register(DataRegister(0), f32_to_ffp(fnum1.powf(fnum2)));
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

/// Registers every implemented handler for all three math libraries.
/// Called unconditionally from [`crate::dispatch::Runtime::new`].
pub fn register_mathlibs_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    register_mathieeedoubbas_handlers(table, mem);
    register_mathieeedoubtrans_handlers(table, mem);
    register_mathtrans_handlers(table, mem);
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
    fn ffp_one_matches_known_encoding() {
        // 1.0: FFP mantissa 0x800000 (0.5 as a 24-bit fraction),
        // exponent field 65 ($41, excess-64 for unbiased exponent 1,
        // since 0.5 * 2^1 == 1.0), sign 0 -- independently derivable
        // from this module's doc comment's bit layout, not just a
        // round-trip check.
        assert_eq!(f32_to_ffp(1.0), (0x0080_0000u32 << 8) | (65 << 1));
    }
}
