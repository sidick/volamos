//! `locale.library`: character classification (`IsAlpha`/`IsDigit`/etc.),
//! case conversion (`ConvToUpper`/`ConvToLower`), locale-aware string
//! compare (`StrnCmp`), and a minimal `OpenLocale`/`CloseLocale`. Added
//! closing a gap found comparing volamos's coverage against vamos's own
//! (`docs/plan.md`'s dated entry) -- vamos itself only covers "the
//! ctype-style character classification and string case/compare calls
//! ... plus basic OpenLocale/date formatting -- not a full locale/catalog
//! system", and this module matches that same scope, not a real
//! multi-locale/catalog implementation.
//!
//! # No real locale/catalog system: every function ignores its `locale` argument
//!
//! Every real `locale.library` classification/conversion/compare
//! function takes a `struct Locale*` and is documented to fall back to
//! "the built-in default" behavior when it's `NULL`. This runtime never
//! implements a real locale-preferences/catalog system (no
//! `LC:`-directory scanning, no translated catalogs -- matching this
//! project's KS/WB 3.1 console-tool scope, not a GUI/Locale-prefs
//! target), so every handler here *always* uses that same built-in
//! default, whether or not a real (or fake) `Locale*` was passed in --
//! there is only ever one behavior to have. That default is the classic
//! Amiga "international mode" charset: the NDK's own `libraries/
//! locale.h` documents `loc_CodeSet` as "always 0 for now" (i.e. no
//! numbered codeset registry existed yet in the V40-era library this
//! runtime targets), and codeset 0 conventionally means exactly the
//! built-in charset `crate::utility`'s `ToUpper`/`ToLower`/`Stricmp`
//! already implement (ASCII + the Latin-1/ISO-8859-1 `0xC0`-`0xFE`
//! accented range, excluding the multiplication/division signs) -- so
//! this module reuses [`crate::utility::amiga_tolower`]/
//! [`crate::utility::amiga_toupper`] directly rather than re-deriving
//! the same case-mapping data a second time.
//!
//! # `OpenLocale`/`CloseLocale`: a real, but content-free, allocation
//!
//! `OpenLocale` allocates and zeroes a real, correctly-sized (168 bytes,
//! `<libraries/locale.h>`'s `struct Locale`, field-by-field) block on
//! the guest heap and returns it -- a genuine, non-`NULL` `Locale*` a
//! caller can pass back into every other function here, and (since it's
//! zeroed, not garbage) `loc_CodeSet` reads back as `0`, matching the
//! real "always 0 for now" default even for guest code that reads the
//! field directly instead of going through a library call. No field is
//! otherwise populated (no locale name, no date/currency formatting
//! strings) -- consistent with this module's "not a full locale/catalog
//! system" scope; a caller that actually reads
//! `loc_LocaleName`/`loc_DateFormat`/etc. rather than just passing the
//! pointer through to `IsUpper`-family calls will see empty/`NULL`
//! fields, not real values. `CloseLocale` frees it via the same
//! `GuestHeap` live-allocation check `crate::execmem`'s `FreeVec` uses
//! (loud failure on an unknown/already-freed pointer, matching this
//! runtime's established bug-catching posture); `CloseLocale(NULL)` is
//! a documented-legal no-op, same convention as every other free-half-
//! of-a-pair call in this runtime.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, HandlerContext, LibraryTable};
use crate::lvos::locale::LOCALE_LVOS;
use crate::memory::AddressSpace;
use crate::utility::{amiga_tolower, amiga_toupper};

/// `sizeof(struct Locale)` per `<libraries/locale.h>`, field-by-field:
/// `loc_LocaleName`/`loc_LanguageName` (4 each, 8) + `loc_PrefLanguages[10]`
/// (4 each, 40) + `loc_Flags` (4) + `loc_CodeSet`/`loc_CountryCode`/
/// `loc_TelephoneCode`/`loc_GMTOffset` (4 each, 16) +
/// `loc_MeasuringSystem`/`loc_CalendarType`/`loc_Reserved0[2]` (1 each,
/// 4) + `loc_DateTimeFormat`/`loc_DateFormat`/`loc_TimeFormat` (4 each,
/// 12) + `loc_ShortDateTimeFormat`/`loc_ShortDateFormat`/
/// `loc_ShortTimeFormat` (4 each, 12) + `loc_DecimalPoint`/
/// `loc_GroupSeparator`/`loc_FracGroupSeparator`/`loc_Grouping`/
/// `loc_FracGrouping` (4 each, 20) + `loc_MonDecimalPoint`/
/// `loc_MonGroupSeparator`/`loc_MonFracGroupSeparator`/
/// `loc_MonGrouping`/`loc_MonFracGrouping` (4 each, 20) +
/// `loc_MonFracDigits`/`loc_MonIntFracDigits`/`loc_Reserved1[2]` (1
/// each, 4) + `loc_MonCS`/`loc_MonSmallCS`/`loc_MonIntCS` (4 each, 12) +
/// `loc_MonPositiveSign` (4) + `loc_MonPositiveSpaceSep`/
/// `loc_MonPositiveSignPos`/`loc_MonPositiveCSPos`/`loc_Reserved2` (1
/// each, 4) + `loc_MonNegativeSign` (4) +
/// `loc_MonNegativeSpaceSep`/`loc_MonNegativeSignPos`/
/// `loc_MonNegativeCSPos`/`loc_Reserved3` (1 each, 4) = 168.
const LOCALE_STRUCT_SIZE: u32 = 168;

/// `OpenLocale` (`A0` = name `CString*`, ignored -- see the module docs;
/// real `OpenLocale(NULL)` opens "the current default locale", and
/// since every locale this runtime can ever produce *is* that default,
/// a named lookup couldn't behave any differently). `D0` = a real,
/// zeroed `struct Locale*`, never `NULL` (unlike real `OpenLocale`,
/// which can fail -- this runtime has no preferences file to fail to
/// read).
fn open_locale_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr = ctx
        .heap
        .alloc(LOCALE_STRUCT_SIZE)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "locale.library".to_string(),
            lvo: -156,
            handler_name: "OpenLocale".to_string(),
            message: format!("OpenLocale: guest heap allocation failed: {e}"),
        })?;
    for i in 0..LOCALE_STRUCT_SIZE {
        ctx.mem.write_u8(addr.wrapping_add(i), 0);
    }
    ctx.cpu.set_data_register(DataRegister(0), addr);
    Ok(())
}

/// `CloseLocale` (`A0` = `struct Locale*`). No return value. `NULL` is a
/// documented-legal no-op; any other address must be a live
/// [`open_locale_handler`] allocation or this fails loudly (see the
/// module docs).
fn close_locale_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr = ctx.cpu.address_register(AddressRegister(0));
    if addr == 0 {
        return Ok(());
    }
    ctx.heap
        .free(addr)
        .map_err(|e| DispatchError::HandlerFailed {
            library: "locale.library".to_string(),
            lvo: -42,
            handler_name: "CloseLocale".to_string(),
            message: format!(
                "CloseLocale called on {addr:#010x}, which isn't a currently-live OpenLocale \
             allocation (never allocated, already freed, or not an OpenLocale pointer at \
             all): {e}"
            ),
        })
}

/// `ConvToUpper`/`ConvToLower` (`A0` = `struct Locale*`, ignored, `D0` =
/// `character` as a `ULONG`). `D0` = the converted character. Real
/// classic-charset conversion only ever maps a single byte's worth of
/// value (`0`-`255`); anything outside that range (not a real character
/// this charset can represent at all) passes through unchanged, same
/// as `crate::utility`'s `ToUpper`/`ToLower` implicitly do by only
/// operating on a `UBYTE` in the first place.
fn conv_case<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    f: impl FnOnce(u8) -> u8,
) -> Result<(), DispatchError> {
    let character = ctx.cpu.data_register(DataRegister(0));
    let result = if character <= 0xFF {
        f(character as u8) as u32
    } else {
        character
    };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

fn conv_to_upper_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    conv_case(ctx, amiga_toupper)
}

fn conv_to_lower_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    conv_case(ctx, amiga_tolower)
}

/// Amiga "international mode" `IsUpper`: exactly the condition
/// [`amiga_tolower`] changes (i.e. `c` really is an uppercase letter in
/// this charset).
fn is_upper(c: u8) -> bool {
    amiga_tolower(c) != c
}

/// Amiga "international mode" `IsLower`: the inverse of [`is_upper`],
/// via [`amiga_toupper`].
fn is_lower(c: u8) -> bool {
    amiga_toupper(c) != c
}

/// `IsAlpha`: a letter, upper or lower, in this charset.
fn is_alpha(c: u8) -> bool {
    is_upper(c) || is_lower(c)
}

/// `IsCntrl`: the C0 control range (`0x00`-`0x1F`), `DEL` (`0x7F`), and
/// Latin-1's C1 control range (`0x80`-`0x9F`) -- the standard ISO-8859-1
/// definition of "not a real character," extending the classic 7-bit C
/// `iscntrl` set to cover this charset's full 8 bits.
fn is_cntrl(c: u8) -> bool {
    c < 0x20 || c == 0x7F || (0x80..=0x9F).contains(&c)
}

/// `IsPrint`: everything that isn't a control character -- the standard
/// C `isprint`/`iscntrl` complementary relationship, extended to 8 bits.
fn is_print(c: u8) -> bool {
    !is_cntrl(c)
}

/// `IsSpace`: the standard C whitespace set (space, tab, newline,
/// vertical tab, form feed, carriage return) -- this charset has no
/// additional whitespace character beyond the classic C set (in
/// particular, `0xA0`/NBSP is deliberately *not* included, matching
/// plain `isspace`'s traditional definition).
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | 0x09 | 0x0A | 0x0B | 0x0C | 0x0D)
}

/// `IsGraph`: printable and not a space -- standard C `isgraph`.
fn is_graph(c: u8) -> bool {
    is_print(c) && c != b' '
}

/// `IsAlNum`: a letter or digit.
fn is_alnum(c: u8) -> bool {
    is_alpha(c) || c.is_ascii_digit()
}

/// `IsPunct`: printable, not whitespace, not alphanumeric -- standard C
/// `ispunct`.
fn is_punct(c: u8) -> bool {
    is_graph(c) && !is_alnum(c)
}

/// One-character-argument `locale.library` classification function
/// (`A0` = `struct Locale*`, ignored, `D0` = `character` as a `ULONG`).
/// `D0` = `1`/`0` (`BOOL`) -- real `IsXxx` functions only ever return
/// `TRUE` for values that are real, representable characters in this
/// charset (`0`-`255`); anything outside that range is never any of
/// these properties.
fn is_class<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    f: impl FnOnce(u8) -> bool,
) -> Result<(), DispatchError> {
    let character = ctx.cpu.data_register(DataRegister(0));
    let result = character <= 0xFF && f(character as u8);
    ctx.cpu
        .set_data_register(DataRegister(0), if result { 1 } else { 0 });
    Ok(())
}

macro_rules! is_class_handler {
    ($fn_name:ident, $pred:expr) => {
        fn $fn_name<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
            is_class(ctx, $pred)
        }
    };
}

is_class_handler!(is_alpha_handler, is_alpha);
is_class_handler!(is_digit_handler, |c: u8| c.is_ascii_digit());
is_class_handler!(is_alnum_handler, is_alnum);
is_class_handler!(is_cntrl_handler, is_cntrl);
is_class_handler!(is_graph_handler, is_graph);
is_class_handler!(is_lower_handler, is_lower);
is_class_handler!(is_print_handler, is_print);
is_class_handler!(is_punct_handler, is_punct);
is_class_handler!(is_space_handler, is_space);
is_class_handler!(is_upper_handler, is_upper);
is_class_handler!(is_xdigit_handler, |c: u8| c.is_ascii_hexdigit());

/// `StrnCmp` (`A0` = `struct Locale*`, ignored, `A1`/`A2` = two
/// `CString*`s, `D0` = length, `D1` = comparison type). `D0` = `0` if
/// equal, negative if `string1 < string2`, positive if `string1 >
/// string2`. Real `StrnCmp`'s `D1` selects `SC_ASCII` (plain byte
/// comparison) vs. `SC_COLLATE1`/`SC_COLLATE2` (locale-aware
/// collation-order comparison) -- since this runtime has no real
/// collation table (no locale/catalog system, see the module docs),
/// every mode compares case-insensitively via
/// [`crate::utility::amiga_tolower`] (matching `utility.library`'s own
/// `Stricmp`/`Strnicmp`, the closest real behavior available without a
/// genuine collation table): a plain `SC_ASCII` byte comparison would
/// be case-*sensitive*, which is a worse approximation of real
/// locale-aware collation than case-insensitive comparison is, so this
/// deliberately doesn't distinguish `D1`'s modes.
fn strn_cmp_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let s1 = ctx.cpu.address_register(AddressRegister(1));
    let s2 = ctx.cpu.address_register(AddressRegister(2));
    let length = ctx.cpu.data_register(DataRegister(0)) as i32;
    let max_len = if length < 0 { u32::MAX } else { length as u32 };

    let mut a = s1;
    let mut b = s2;
    let mut compared = 0u32;
    let result = loop {
        if compared >= max_len {
            break 0;
        }
        let ca = ctx.mem.read_u8(a);
        let cb = ctx.mem.read_u8(b);
        let la = amiga_tolower(ca);
        let lb = amiga_tolower(cb);
        if la != lb {
            break i32::from(la) - i32::from(lb);
        }
        if ca == 0 {
            break 0;
        }
        a = a.wrapping_add(1);
        b = b.wrapping_add(1);
        compared += 1;
    };
    ctx.cpu.set_data_register(DataRegister(0), result as u32);
    Ok(())
}

/// Registers this module's `locale.library` handlers, looked up by name
/// through [`LOCALE_LVOS`], following
/// [`crate::execmem::register_execmem_handlers`]'s registration
/// pattern. Called unconditionally from
/// [`crate::dispatch::Runtime::new`] -- `locale.library` is a
/// [`crate::dispatch::STANDARD_WORKBENCH_LIBRARIES`] member (always
/// present, matching real KS/WB 3.1 ROM-resident behavior).
pub fn register_locale_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    crate::dispatch::LOCALE_LIBRARY_BASE,
                    LOCALE_LVOS,
                    "locale.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in LOCALE_LVOS: {e}", $name));
        };
    }
    reg!("OpenLocale", open_locale_handler::<C>);
    reg!("CloseLocale", close_locale_handler::<C>);
    reg!("ConvToUpper", conv_to_upper_handler::<C>);
    reg!("ConvToLower", conv_to_lower_handler::<C>);
    reg!("IsAlpha", is_alpha_handler::<C>);
    reg!("IsDigit", is_digit_handler::<C>);
    reg!("IsAlNum", is_alnum_handler::<C>);
    reg!("IsCntrl", is_cntrl_handler::<C>);
    reg!("IsGraph", is_graph_handler::<C>);
    reg!("IsLower", is_lower_handler::<C>);
    reg!("IsPrint", is_print_handler::<C>);
    reg!("IsPunct", is_punct_handler::<C>);
    reg!("IsSpace", is_space_handler::<C>);
    reg!("IsUpper", is_upper_handler::<C>);
    reg!("IsXDigit", is_xdigit_handler::<C>);
    reg!("StrnCmp", strn_cmp_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{LOCALE_LIBRARY_BASE, Runtime, StartConfig};
    use crate::memory::FlatMemory;

    #[test]
    fn is_upper_and_is_lower_cover_ascii_and_latin1_and_exclude_multiplication_sign() {
        assert!(is_upper(b'A'));
        assert!(!is_upper(b'a'));
        assert!(is_lower(b'a'));
        assert!(!is_lower(b'A'));
        assert!(is_upper(0xC0)); // Latin-1 A-grave
        assert!(is_lower(0xE0)); // Latin-1 a-grave
        assert!(!is_upper(0xD7)); // multiplication sign, not a letter
        assert!(!is_lower(0xF7)); // division sign, not a letter
    }

    #[test]
    fn classification_predicates_agree_with_plain_ascii_expectations() {
        assert!(is_alpha(b'x'));
        assert!(!is_alpha(b'5'));
        assert!(is_alnum(b'5'));
        assert!(is_alnum(b'x'));
        assert!(!is_alnum(b' '));
        assert!(is_cntrl(0x01));
        assert!(is_cntrl(0x7F));
        assert!(!is_cntrl(b'x'));
        assert!(is_space(b' '));
        assert!(is_space(b'\t'));
        assert!(!is_space(b'x'));
        assert!(is_print(b'x'));
        assert!(!is_print(0x01));
        assert!(is_graph(b'x'));
        assert!(!is_graph(b' '));
        assert!(is_punct(b'!'));
        assert!(!is_punct(b'x'));
        assert!(!is_punct(b' '));
    }

    fn load_words<M: AddressSpace>(mem: &mut M, addr: u32, words: &[u16]) {
        let mut offset = addr;
        for &w in words {
            mem.write_u16(offset, w);
            offset += 2;
        }
    }

    fn move_imm_to_a(n: u16) -> u16 {
        0x207C | (n << 9)
    }
    fn move_imm_to_d(n: u16) -> u16 {
        0x203C | (n << 9)
    }
    fn move_d0_to_a(n: u16) -> u16 {
        0x2040 | (n << 9)
    }
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }
    const RTS: u16 = 0x4E75;

    fn movea_locale_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (LOCALE_LIBRARY_BASE >> 16) as u16,
            LOCALE_LIBRARY_BASE as u16,
        ]
    }

    fn runtime_with_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
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

    fn locale_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut full = movea_locale_base_to_a6().to_vec();
        full.extend_from_slice(words);
        runtime_with_program(&full)
    }

    #[test]
    fn end_to_end_open_then_close_locale_round_trip() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = NULL (name, ignored)
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-156)); // OpenLocale -> D0
        words.push(move_d0_to_a(0)); // A0 = the Locale*
        words.extend_from_slice(&jsr_disp16_a6(-42)); // CloseLocale
        words.push(RTS);

        let mut rt = locale_program(&words);
        let mut out = Vec::new();
        rt.run(&mut out, None).expect("run should succeed");
    }

    #[test]
    fn end_to_end_open_locale_returns_a_real_zeroed_block() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = NULL
        words.push(0);
        words.push(0);
        words.extend_from_slice(&jsr_disp16_a6(-156)); // OpenLocale
        words.push(RTS);

        let mut rt = locale_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let addr = code as u32;
        assert_ne!(addr, 0);
        for i in 0..LOCALE_STRUCT_SIZE {
            assert_eq!(rt.memory().read_u8(addr + i), 0);
        }
    }

    #[test]
    fn close_locale_on_a_never_allocated_address_fails_loudly() {
        let mut words = Vec::new();
        words.push(move_imm_to_a(0)); // A0 = some address OpenLocale never returned
        words.push((TRAP_TABLE_END >> 16) as u16);
        words.push(TRAP_TABLE_END as u16);
        words.extend_from_slice(&jsr_disp16_a6(-42)); // CloseLocale
        words.push(RTS);

        let mut rt = locale_program(&words);
        let mut out = Vec::new();
        let err = rt
            .run(&mut out, None)
            .expect_err("closing a never-allocated address should fail loudly");
        match err {
            crate::dispatch::RuntimeError::Dispatch(DispatchError::HandlerFailed {
                library,
                lvo,
                handler_name,
                ..
            }) => {
                assert_eq!(library, "locale.library");
                assert_eq!(lvo, -42);
                assert_eq!(handler_name, "CloseLocale");
            }
            other => panic!("expected HandlerFailed, got {other:?}"),
        }
    }

    #[test]
    fn end_to_end_is_upper_and_conv_to_upper_via_dispatch() {
        let mut words = vec![
            move_imm_to_a(0), // A0 = NULL locale
            0,
            0,
            move_imm_to_d(0), // D0 = 'a'
            0,
            b'a' as u16,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-138)); // IsUpper('a') -> D0 = 0
        words.push(move_imm_to_d(0)); // D0 = 'a' again
        words.push(0);
        words.push(b'a' as u16);
        words.extend_from_slice(&jsr_disp16_a6(-54)); // ConvToUpper('a') -> D0 = 'A'
        words.push(RTS);

        let mut rt = locale_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, b'A' as u32);
    }

    #[test]
    fn end_to_end_strncmp_is_case_insensitive() {
        let str1_addr: u32 = 0x1_8000;
        let str2_addr: u32 = 0x1_8100;

        let mut words = vec![
            move_imm_to_a(0), // A0 = NULL locale
            0,
            0,
            move_imm_to_a(1), // A1 = "Hello"
            (str1_addr >> 16) as u16,
            str1_addr as u16,
            move_imm_to_a(2), // A2 = "hello"
            (str2_addr >> 16) as u16,
            str2_addr as u16,
            move_imm_to_d(0), // D0 = length (-1: unbounded)
            0xFFFF,
            0xFFFF,
            move_imm_to_d(1), // D1 = comparison type (ignored)
            0,
            0,
        ];
        words.extend_from_slice(&jsr_disp16_a6(-180)); // StrnCmp
        words.push(RTS);

        let mut full = movea_locale_base_to_a6().to_vec();
        full.extend_from_slice(&words);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &full);
        crate::guestmem::write_c_string(&mut mem, str1_addr, b"Hello");
        crate::guestmem::write_c_string(&mut mem, str2_addr, b"hello");
        let load_end = entry + 0x400;
        let mut rt = Runtime::new(
            M68kCpu::new(),
            mem,
            StartConfig {
                entry,
                load_end,
                args: Vec::new(),
                ..StartConfig::default()
            },
        );
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0,
            "case-insensitive compare should treat these as equal"
        );
    }
}
