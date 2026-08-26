//! Config-file support (GitHub issue #5): `~/.volamos` supplies
//! user-global defaults for the CLI's own "scaffolding" flags
//! (`-V`/`-a`/`--cwd`/`--auto-assign`/`--stack`/`--ram`/`--cpu`/`--fpu`/`--jit`/
//! `-v`/`-s`), so a repeated-use project doesn't need to retype them on
//! every invocation; a `.volamos` in the current directory overrides it
//! for per-project settings; an explicit CLI flag always wins over
//! both. Neither file can set `<program>`/`[args...]` -- config files
//! are scaffolding only, never "what to run".
//!
//! **Grammar**: `KEY=VALUE` per line, blank lines and `#`-comment lines
//! ignored, whitespace around `=` trimmed. Keys mirror the CLI flags:
//! `VOLUME`/`ASSIGN` (repeatable, same `NAME:value` grammar as `-V`/
//! `-a`), `CWD`, `AUTO_ASSIGN`, `STACK`/`RAM` (same `K`/`M`-suffixed
//! syntax as the flags), `CPU`, `FPU`/`JIT`/`VERBOSE`/`SNOOP` (`true`/
//! `false`). A repeated *singular* key within one file follows the same
//! "last one wins" rule repeating a CLI flag already has. Relative
//! `VOLUME`/`AUTO_ASSIGN` host directories resolve against volamos's
//! own process working directory, same as a CLI-supplied relative path
//! -- not against the config file's own location.
//!
//! **Precedence**: for a "repeatable" setting (`-V`/`-a`), entries from
//! every source all apply (nothing is dropped) -- see [`merge`]'s doc
//! for why concatenating `higher ++ lower` is exactly right here,
//! reusing [`volamos_core::vfs`]'s existing first-match-wins name
//! lookup rather than needing any change there. For every other
//! setting, CLI > local file > global file > built-in default.

use std::path::{Path, PathBuf};

use volamos_core::backend::CpuType;

use crate::{parse_byte_size, parse_cpu_type, split_name_value};

/// A partial set of CLI-equivalent settings from one source (a config
/// file, or the CLI itself via [`crate::parse_args_raw`]) -- singular
/// fields are `None` when that source didn't set them; the two
/// repeatable fields are simply empty. See this module's own doc for
/// the full precedence/merge story.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Overrides {
    pub(crate) verbose: Option<bool>,
    pub(crate) snoop: Option<bool>,
    pub(crate) volumes: Vec<(String, PathBuf)>,
    pub(crate) assigns: Vec<(String, Vec<String>)>,
    pub(crate) cwd: Option<String>,
    pub(crate) auto_assign_root: Option<PathBuf>,
    pub(crate) stack_size: Option<u32>,
    pub(crate) ram_size: Option<u32>,
    pub(crate) cpu_type: Option<CpuType>,
    pub(crate) fpu: Option<bool>,
    pub(crate) jit: Option<bool>,
    /// `--net`: enables `bsdsocket.library` (real host network access for
    /// the guest). Deliberately **not** a recognized `~/.volamos`/
    /// `.volamos` config key (see `crate::config`'s module doc and
    /// `volamos_core::bsdsocket`'s "Opt-in, not always-on" section) --
    /// granting real network access is a different trust boundary than
    /// every other config-file-controllable setting, so it must be typed
    /// explicitly on the command line every time, not silently inherited
    /// from a config file the invoker may not even remember exists.
    pub(crate) net: Option<bool>,
}

/// Parses a `true`/`false` value (case-insensitive), for the `FPU`/
/// `VERBOSE`/`SNOOP` config keys.
fn parse_bool(key: &str, s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{key} expects true or false, got {s:?}")),
    }
}

/// Parses one config file's contents (pure -- no I/O) into an
/// [`Overrides`]. Errors name the exact line number and problem, but
/// don't include the file path itself -- callers ([`load`]) prefix
/// that, since this function doesn't know it.
pub(crate) fn parse(source: &str) -> Result<Overrides, String> {
    let mut overrides = Overrides::default();

    for (index, raw_line) in source.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let lineno = index + 1;
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {lineno}: expected KEY=VALUE, got {raw_line:?}"))?;
        let key = key.trim();
        let value = value.trim();
        let with_line = |e: String| format!("line {lineno}: {e}");

        match key.to_ascii_uppercase().as_str() {
            "VOLUME" => {
                let (name, hostdir) = split_name_value("VOLUME", value).map_err(with_line)?;
                overrides
                    .volumes
                    .push((name.to_string(), PathBuf::from(hostdir)));
            }
            "ASSIGN" => {
                let (name, targets) = split_name_value("ASSIGN", value).map_err(with_line)?;
                let targets: Vec<String> = targets.split('+').map(str::to_string).collect();
                overrides.assigns.push((name.to_string(), targets));
            }
            "CWD" => overrides.cwd = Some(value.to_string()),
            "AUTO_ASSIGN" => overrides.auto_assign_root = Some(PathBuf::from(value)),
            "STACK" => {
                overrides.stack_size = Some(parse_byte_size("STACK", value).map_err(with_line)?);
            }
            "RAM" => {
                overrides.ram_size = Some(parse_byte_size("RAM", value).map_err(with_line)?);
            }
            "CPU" => overrides.cpu_type = Some(parse_cpu_type(value).map_err(with_line)?),
            "FPU" => overrides.fpu = Some(parse_bool("FPU", value).map_err(with_line)?),
            "JIT" => overrides.jit = Some(parse_bool("JIT", value).map_err(with_line)?),
            "VERBOSE" => overrides.verbose = Some(parse_bool("VERBOSE", value).map_err(with_line)?),
            "SNOOP" => overrides.snoop = Some(parse_bool("SNOOP", value).map_err(with_line)?),
            other => return Err(with_line(format!("unknown key {other:?}"))),
        }
    }

    Ok(overrides)
}

/// Merges two layers, `higher` taking precedence over `lower`. For a
/// singular field, `higher`'s value wins if set, else `lower`'s. For
/// the repeatable `volumes`/`assigns` fields, both sources' entries are
/// concatenated `higher ++ lower` -- [`volamos_core::vfs::Vfs`]'s
/// `lookup_volume`/`lookup_assign` resolve a name via the *first*
/// matching entry in the list, so putting `higher`'s entries first
/// means a `NAME:` present in both layers resolves to `higher`'s
/// mapping, while a `NAME:` present in only one layer is unaffected --
/// exactly "override on conflict, otherwise both apply" without any
/// special-casing here or in `volamos-core`.
pub(crate) fn merge(higher: Overrides, lower: Overrides) -> Overrides {
    Overrides {
        verbose: higher.verbose.or(lower.verbose),
        snoop: higher.snoop.or(lower.snoop),
        volumes: higher.volumes.into_iter().chain(lower.volumes).collect(),
        assigns: higher.assigns.into_iter().chain(lower.assigns).collect(),
        cwd: higher.cwd.or(lower.cwd),
        auto_assign_root: higher.auto_assign_root.or(lower.auto_assign_root),
        stack_size: higher.stack_size.or(lower.stack_size),
        ram_size: higher.ram_size.or(lower.ram_size),
        cpu_type: higher.cpu_type.or(lower.cpu_type),
        fpu: higher.fpu.or(lower.fpu),
        jit: higher.jit.or(lower.jit),
        // net is deliberately CLI-only (see Overrides::net's doc) -- a
        // config file layer's `net` is always None, so this is really
        // just "the CLI's own value passes through unchanged", not a
        // real merge.
        net: higher.net.or(lower.net),
    }
}

/// `~/.volamos`, if `$HOME` is set.
fn global_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".volamos"))
}

/// `./.volamos`, relative to volamos's own process working directory.
fn local_path() -> Option<PathBuf> {
    std::env::current_dir().ok().map(|dir| dir.join(".volamos"))
}

/// Loads and parses one config file. `Ok(None)` means the file simply
/// doesn't exist -- the common case, since most users won't have one --
/// not an error; any other read failure, or a parse error, is reported
/// with `path` prefixed onto the underlying message.
fn load(path: &Path) -> Result<Option<Overrides>, String> {
    match std::fs::read_to_string(path) {
        Ok(source) => parse(&source)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("couldn't read {}: {e}", path.display())),
    }
}

/// Loads `~/.volamos` and `./.volamos` (either or both may be absent)
/// and merges them, local overriding global per this module's own doc.
/// The result still needs merging against the CLI's own [`Overrides`]
/// (CLI wins over both) -- see [`merge`] and `crate::main`.
pub(crate) fn load_all() -> Result<Overrides, String> {
    let global = match global_path() {
        Some(path) => load(&path)?.unwrap_or_default(),
        None => Overrides::default(),
    };
    let local = match local_path() {
        Some(path) => load(&path)?.unwrap_or_default(),
        None => Overrides::default(),
    };
    Ok(merge(local, global))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source_yields_no_overrides() {
        assert_eq!(parse("").unwrap(), Overrides::default());
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let overrides = parse("\n# a comment\n   \n# STACK=999\n").unwrap();
        assert_eq!(overrides, Overrides::default());
    }

    #[test]
    fn whitespace_around_key_and_value_is_trimmed() {
        let overrides = parse("  STACK = 256K  \n").unwrap();
        assert_eq!(overrides.stack_size, Some(256 * 1024));
    }

    #[test]
    fn every_key_parses() {
        let source = "VOLUME=SYS:/host/sys\n\
                       ASSIGN=LIBS:SYS:libsA+SYS:libsB\n\
                       CWD=SYS:\n\
                       AUTO_ASSIGN=/host/auto\n\
                       STACK=256K\n\
                       RAM=32M\n\
                       CPU=68020\n\
                       FPU=true\n\
                       VERBOSE=false\n\
                       SNOOP=true\n";
        let overrides = parse(source).unwrap();
        assert_eq!(
            overrides.volumes,
            vec![("SYS".to_string(), PathBuf::from("/host/sys"))]
        );
        assert_eq!(
            overrides.assigns,
            vec![(
                "LIBS".to_string(),
                vec!["SYS:libsA".to_string(), "SYS:libsB".to_string()]
            )]
        );
        assert_eq!(overrides.cwd, Some("SYS:".to_string()));
        assert_eq!(
            overrides.auto_assign_root,
            Some(PathBuf::from("/host/auto"))
        );
        assert_eq!(overrides.stack_size, Some(256 * 1024));
        assert_eq!(overrides.ram_size, Some(32 * 1024 * 1024));
        assert_eq!(overrides.cpu_type, Some(CpuType::M68020));
        assert_eq!(overrides.fpu, Some(true));
        assert_eq!(overrides.verbose, Some(false));
        assert_eq!(overrides.snoop, Some(true));
    }

    #[test]
    fn key_is_case_insensitive() {
        let overrides = parse("stack=1K\n").unwrap();
        assert_eq!(overrides.stack_size, Some(1024));
    }

    #[test]
    fn volume_is_repeatable() {
        let overrides = parse("VOLUME=SYS:/host/sys\nVOLUME=WORK:/host/work\n").unwrap();
        assert_eq!(
            overrides.volumes,
            vec![
                ("SYS".to_string(), PathBuf::from("/host/sys")),
                ("WORK".to_string(), PathBuf::from("/host/work")),
            ]
        );
    }

    #[test]
    fn repeated_singular_key_last_line_wins() {
        let overrides = parse("STACK=1K\nSTACK=2K\n").unwrap();
        assert_eq!(overrides.stack_size, Some(2048));
    }

    #[test]
    fn missing_equals_is_an_error() {
        let err = parse("STACK 1K\n").unwrap_err();
        assert!(err.contains("line 1"), "unexpected message: {err}");
    }

    #[test]
    fn unknown_key_is_an_error() {
        let err = parse("NOPE=1\n").unwrap_err();
        assert!(err.contains("unknown key"), "unexpected message: {err}");
        assert!(err.contains("NOPE"), "unexpected message: {err}");
    }

    #[test]
    fn malformed_volume_is_an_error() {
        let err = parse("VOLUME=notanassign\n").unwrap_err();
        assert!(err.contains("line 1"), "unexpected message: {err}");
    }

    #[test]
    fn malformed_stack_size_is_an_error() {
        let err = parse("STACK=notanumber\n").unwrap_err();
        assert!(err.contains("line 1"), "unexpected message: {err}");
    }

    #[test]
    fn malformed_cpu_is_an_error() {
        let err = parse("CPU=68999\n").unwrap_err();
        assert!(err.contains("line 1"), "unexpected message: {err}");
    }

    #[test]
    fn malformed_bool_is_an_error() {
        let err = parse("FPU=yes\n").unwrap_err();
        assert!(err.contains("line 1"), "unexpected message: {err}");
    }

    #[test]
    fn jit_key_parses() {
        let overrides = parse("JIT=true\n").unwrap();
        assert_eq!(overrides.jit, Some(true));
    }

    #[test]
    fn error_reports_correct_line_number_past_comments() {
        let err = parse("# comment\nVOLUME=bad\n").unwrap_err();
        assert!(err.contains("line 2"), "unexpected message: {err}");
    }

    #[test]
    fn merge_prefers_higher_for_singular_fields() {
        let higher = Overrides {
            stack_size: Some(1),
            ..Overrides::default()
        };
        let lower = Overrides {
            stack_size: Some(2),
            ram_size: Some(3),
            ..Overrides::default()
        };
        let merged = merge(higher, lower);
        assert_eq!(merged.stack_size, Some(1));
        assert_eq!(merged.ram_size, Some(3));
    }

    #[test]
    fn merge_concatenates_repeatable_fields_higher_first() {
        let higher = Overrides {
            volumes: vec![("SYS".to_string(), PathBuf::from("/higher/sys"))],
            ..Overrides::default()
        };
        let lower = Overrides {
            volumes: vec![
                ("SYS".to_string(), PathBuf::from("/lower/sys")),
                ("WORK".to_string(), PathBuf::from("/lower/work")),
            ],
            ..Overrides::default()
        };
        let merged = merge(higher, lower);
        // higher's SYS: entry comes first, so it's the one a
        // first-match-wins lookup finds; lower's distinct WORK: entry
        // still comes through unchanged.
        assert_eq!(
            merged.volumes,
            vec![
                ("SYS".to_string(), PathBuf::from("/higher/sys")),
                ("SYS".to_string(), PathBuf::from("/lower/sys")),
                ("WORK".to_string(), PathBuf::from("/lower/work")),
            ]
        );
    }

    #[test]
    fn load_missing_file_is_ok_none() {
        let path = std::env::temp_dir().join("volamos-config-test-definitely-missing-file");
        assert_eq!(load(&path).unwrap(), None);
    }
}
