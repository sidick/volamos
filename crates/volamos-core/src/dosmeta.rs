//! `.uaem` metadata sidecars: a host-filesystem-side, one-line text file
//! (`<name>.uaem`, a sibling of the object it describes) storing
//! Amiga-specific metadata a plain host file/directory can't represent
//! on its own: protection bits, the AmigaDOS modification date, and a
//! comment.
//!
//! This is the FS-UAE/Amiberry/Copperline-shared sidecar convention,
//! not invented by this runtime -- the format below is taken from an
//! independently-built reference implementation
//! (`~/src/amisnap/src/amiga/applyuaem.c`/`tools/amisnap_reader.py`),
//! itself checked against real captured Copperline `.uaem` output. This
//! runtime's own [`read_sidecar`]/[`write_sidecar`] are therefore
//! interoperable with those tools' sidecars, not a private format only
//! this runtime understands (unlike, say, `crate::dospattern`'s
//! tokenized encoding, which nothing outside this runtime ever reads).
//!
//! # Format
//!
//! One line:
//!
//! ```text
//! HSPARWED YYYY-MM-DD HH:MM:SS.CC optional comment to end of line
//! ```
//!
//! - The 8-character protection-flag string: `h`/`s`/`p`/`a` (`FIBB_HOLD`/
//!   `SCRIPT`/`PURE`/`ARCHIVE`, bits 7-4 of `fib_Protection`) show their
//!   letter when *set* (active-high); `r`/`w`/`e`/`d` (`FIBB_READ`/
//!   `WRITE`/`EXECUTE`/`DELETE`, bits 3-0) show theirs when *clear*
//!   (active-low -- the classic "shown inverted" AmigaDOS convention
//!   `List`/`Dir` already display). `-` where a letter isn't shown.
//! - A space, then the `DateStamp` as `YYYY-MM-DD HH:MM:SS.CC` (`CC` is
//!   centiseconds, i.e. `ds_Tick / 2` -- ticks are 1/50s units,
//!   centiseconds are 1/100s).
//! - An optional space-then-comment, to end of line (absent = no
//!   comment). `\n`-terminated.
//!
//! # Scope
//!
//! [`read_sidecar`]/[`write_sidecar`] are the only two entry points;
//! callers decide *when* to read/write one. `crate::doslock`'s
//! `fill_fib` reads one on every `Examine`/`ExNext`/`MatchFirst`,
//! falling back to this runtime's previous all-default behavior
//! (`fib_Protection == 0`, the AmigaOS epoch, no comment) when none
//! exists -- a missing sidecar is not an error, just "no metadata
//! recorded yet". `crate::dosprotect`'s `SetProtection` and this
//! module's own `SetComment` handler write one, merging onto whatever
//! was already there (so setting the comment doesn't clobber
//! previously-set protection bits, and vice versa).
//!
//! Reading a fixed `DateStamp` (from a sidecar someone else wrote, not
//! this runtime deriving it from host mtime) doesn't reintroduce the
//! non-determinism `crate::doslock`'s module docs deliberately avoid --
//! a sidecar's own date field is explicit, checked-in data, not a
//! live filesystem timestamp.

use crate::utility::{days_to_ymd, ymd_to_days};
use std::path::{Path, PathBuf};

/// `(bitmask, lowercase display letter, active_high)` in left-to-right
/// display order -- taken from `~/src/amisnap`'s independently
/// reverse-engineered table (itself checked against real captured
/// Copperline `.uaem` output), not re-derived from the bit names alone.
const PROT_ORDER: [(u32, u8, bool); 8] = [
    (0x80, b'h', true),
    (0x40, b's', true),
    (0x20, b'p', true),
    (0x10, b'a', true),
    (0x08, b'r', false),
    (0x04, b'w', false),
    (0x02, b'e', false),
    (0x01, b'd', false),
];

/// One `.uaem` sidecar's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Meta {
    /// `fib_Protection`'s low 8 bits (`FIBB_HOLD`..`FIBB_DELETE`).
    pub prot: u32,
    /// `(ds_Days, ds_Minute, ds_Tick)`.
    pub date: (i32, i32, i32),
    /// `fib_Comment`, if the sidecar had one.
    pub comment: Option<Vec<u8>>,
}

impl Default for Meta {
    /// This runtime's previous all-default behavior, for a target with
    /// no sidecar yet: no protection bits set, the AmigaOS epoch, no
    /// comment.
    fn default() -> Self {
        Meta {
            prot: 0,
            date: (0, 0, 0),
            comment: None,
        }
    }
}

/// The sidecar path for `target` (a sibling file, `<target>.uaem`).
pub(crate) fn sidecar_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(".uaem");
    PathBuf::from(s)
}

/// Whether `file_name` (a single path component, not a full path) is
/// itself a `.uaem` sidecar. Real FS-UAE/Amiberry/Copperline hide these
/// from the emulated Amiga's own directory listings entirely -- they're
/// a host-side implementation detail of the mount, not a real Amiga
/// file -- so every directory-listing site in this runtime
/// (`crate::doslock`'s `Examine`/`ExNext`, `crate::dosanchor`'s
/// `MatchFirst`/`MatchNext`) filters entries with this before exposing
/// them to the guest.
pub(crate) fn is_sidecar_name(file_name: &str) -> bool {
    file_name.ends_with(".uaem")
}

fn prot_to_flags(prot: u32) -> [u8; 8] {
    let mut out = [b'-'; 8];
    for (i, &(mask, letter, active_high)) in PROT_ORDER.iter().enumerate() {
        let bit_set = prot & mask != 0;
        let show = if active_high { bit_set } else { !bit_set };
        if show {
            out[i] = letter;
        }
    }
    out
}

fn flags_to_prot(flags: &[u8]) -> u32 {
    let mut prot = 0u32;
    for (i, &(mask, letter, active_high)) in PROT_ORDER.iter().enumerate() {
        let present = flags.get(i) == Some(&letter);
        let bit_set = if active_high { present } else { !present };
        if bit_set {
            prot |= mask;
        }
    }
    prot
}

fn format_timestamp(days: i32, minute: i32, tick: i32) -> String {
    let (year, month, mday) = days_to_ymd(days.max(0) as u32);
    let hour = minute / 60;
    let min = minute % 60;
    let sec = tick / 50;
    let cc = (tick % 50) * 2;
    format!("{year:04}-{month:02}-{mday:02} {hour:02}:{min:02}:{sec:02}.{cc:02}")
}

/// Parses `"YYYY-MM-DD HH:MM:SS.CC"` (exactly 22 bytes) into
/// `(ds_Days, ds_Minute, ds_Tick)`. `None` if it doesn't match.
fn parse_timestamp(s: &[u8]) -> Option<(i32, i32, i32)> {
    if s.len() != 22 {
        return None;
    }
    let digit = |i: usize| -> Option<u32> {
        let c = *s.get(i)?;
        if c.is_ascii_digit() {
            Some(u32::from(c - b'0'))
        } else {
            None
        }
    };
    if s[4] != b'-'
        || s[7] != b'-'
        || s[10] != b' '
        || s[13] != b':'
        || s[16] != b':'
        || s[19] != b'.'
    {
        return None;
    }
    let year = digit(0)? * 1000 + digit(1)? * 100 + digit(2)? * 10 + digit(3)?;
    let month = digit(5)? * 10 + digit(6)?;
    let mday = digit(8)? * 10 + digit(9)?;
    let hour = digit(11)? * 10 + digit(12)?;
    let min = digit(14)? * 10 + digit(15)?;
    let sec = digit(17)? * 10 + digit(18)?;
    let cc = digit(20)? * 10 + digit(21)?;
    let days = ymd_to_days(year, month, mday) as i32;
    let minute = (hour * 60 + min) as i32;
    let tick = (sec * 50 + cc / 2) as i32;
    Some((days, minute, tick))
}

/// Reads `target`'s `.uaem` sidecar, if any. `None` (not an error) if
/// no sidecar exists, or it can't be parsed (corrupt/foreign file --
/// fails closed rather than propagating a host I/O error into an
/// AmigaDOS `IoErr()` for what's an optional enhancement).
pub(crate) fn read_sidecar(target: &Path) -> Option<Meta> {
    let raw = std::fs::read(sidecar_path(target)).ok()?;
    let line_end = raw.iter().position(|&b| b == b'\n').unwrap_or(raw.len());
    let mut line = &raw[..line_end];
    if line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    if line.len() < 31 {
        return None;
    }
    let flags = &line[0..8];
    if line[8] != b' ' {
        return None;
    }
    let (days, minute, tick) = parse_timestamp(&line[9..31])?;
    let comment = if line.len() > 31 && line[31] == b' ' {
        Some(line[32..].to_vec())
    } else {
        None
    };
    Some(Meta {
        prot: flags_to_prot(flags),
        date: (days, minute, tick),
        comment,
    })
}

/// Writes/overwrites `target`'s `.uaem` sidecar.
pub(crate) fn write_sidecar(target: &Path, meta: &Meta) -> std::io::Result<()> {
    let flags = prot_to_flags(meta.prot);
    let mut line = String::from_utf8_lossy(&flags).into_owned();
    line.push(' ');
    line.push_str(&format_timestamp(meta.date.0, meta.date.1, meta.date.2));
    if let Some(comment) = &meta.comment {
        line.push(' ');
        line.push_str(&String::from_utf8_lossy(comment));
    }
    line.push('\n');
    std::fs::write(sidecar_path(target), line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf as StdPathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: StdPathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-dosmeta-test-{tag}-{pid}-{n}"));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn prot_to_flags_matches_the_real_captured_example() {
        // "prot=0x11 -- ARCHIVE|DELETE -- produced '---arwe-'" per
        // amisnap_reader.py's own comment, confirmed against real
        // captured Copperline output.
        assert_eq!(&prot_to_flags(0x11), b"---arwe-");
    }

    #[test]
    fn flags_to_prot_round_trips_through_prot_to_flags() {
        for prot in [0u32, 0x11, 0xFF, 0x80, 0x08, 0xAB] {
            let flags = prot_to_flags(prot);
            assert_eq!(
                flags_to_prot(&flags),
                prot,
                "round trip failed for {prot:#x}"
            );
        }
    }

    #[test]
    fn timestamp_round_trips() {
        // mins=123, ticks=7 -> "02:03:00.14": 123 = 2*60+3, 7*2=14 --
        // the same worked example amisnap_reader.py's own docstring
        // verifies "the hard way" against a real captured sample.
        let s = format_timestamp(0, 123, 7);
        assert_eq!(s, "1978-01-01 02:03:00.14");
        assert_eq!(parse_timestamp(s.as_bytes()), Some((0, 123, 7)));
    }

    #[test]
    fn read_sidecar_parses_the_real_amisnap_fixture_line() {
        let tmp = TempDir::new("read-fixture");
        let target = tmp.path().join("root.txt");
        fs::write(
            sidecar_path(&target),
            b"---arwe- 2024-07-18 10:00:00.20 root level file\n",
        )
        .unwrap();

        let meta = read_sidecar(&target).expect("should parse");
        assert_eq!(meta.prot, 0x11);
        assert_eq!(meta.comment.as_deref(), Some(&b"root level file"[..]));
    }

    #[test]
    fn read_sidecar_missing_file_is_none() {
        let tmp = TempDir::new("read-missing");
        assert!(read_sidecar(&tmp.path().join("nope.txt")).is_none());
    }

    #[test]
    fn read_sidecar_corrupt_file_is_none() {
        let tmp = TempDir::new("read-corrupt");
        let target = tmp.path().join("f.txt");
        fs::write(sidecar_path(&target), b"not a valid uaem line\n").unwrap();
        assert!(read_sidecar(&target).is_none());
    }

    #[test]
    fn write_then_read_round_trips_with_a_comment() {
        let tmp = TempDir::new("write-read");
        let target = tmp.path().join("f.txt");
        let meta = Meta {
            prot: 0x05,
            date: (100, 90, 7),
            comment: Some(b"hello world".to_vec()),
        };
        write_sidecar(&target, &meta).unwrap();
        let read_back = read_sidecar(&target).expect("should parse what we just wrote");
        assert_eq!(read_back, meta);
    }

    #[test]
    fn write_then_read_round_trips_without_a_comment() {
        let tmp = TempDir::new("write-read-nocomment");
        let target = tmp.path().join("f.txt");
        let meta = Meta {
            prot: 0,
            date: (0, 0, 0),
            comment: None,
        };
        write_sidecar(&target, &meta).unwrap();
        let read_back = read_sidecar(&target).expect("should parse what we just wrote");
        assert_eq!(read_back, meta);
    }

    #[test]
    fn default_meta_matches_the_pre_existing_hardcoded_defaults() {
        let d = Meta::default();
        assert_eq!(d.prot, 0);
        assert_eq!(d.date, (0, 0, 0));
        assert_eq!(d.comment, None);
    }
}
