//! Config-file support (GitHub issues #5 and #16): `~/.volamos`
//! supplies user-global defaults for the CLI's own "scaffolding" flags
//! (`-V`/`-a`/`--cwd`/`--auto-assign`/`--stack`/`--ram`/`--cpu`/`--fpu`/`--jit`/
//! `-v`/`-s`), so a repeated-use project doesn't need to retype them on
//! every invocation; a `.volamos` next to the launched binary (in
//! `<program>`'s own containing directory) overrides it, so a
//! toolchain installation (e.g. SAS/C with its multi-volume layout)
//! can be fully self-contained and invocable from anywhere; a
//! `.volamos` in the current directory overrides both for per-project
//! settings; an explicit CLI flag always wins over all three. No file
//! can set `<program>`/`[args...]` -- config files are scaffolding
//! only, never "what to run".
//!
//! **Grammar**: `KEY=VALUE` per line, blank lines and `#`-comment lines
//! ignored, whitespace around `=` trimmed. Keys mirror the CLI flags:
//! `VOLUME`/`ASSIGN` (repeatable, same `NAME:value` grammar as `-V`/
//! `-a`), `CWD`, `AUTO_ASSIGN`, `STACK`/`RAM` (same `K`/`M`-suffixed
//! syntax as the flags), `CPU`, `FPU`/`JIT`/`VERBOSE`/`SNOOP` (`true`/
//! `false`). A repeated *singular* key within one file follows the same
//! "last one wins" rule repeating a CLI flag already has. Relative
//! `VOLUME`/`AUTO_ASSIGN` host directories resolve against **the
//! config file's own directory** (uniformly, for all three sources --
//! decided in issue #16), so a self-contained toolchain's `.volamos`
//! can say `VOLUME=LIB:lib` and mean "the `lib` subdirectory next to
//! me" regardless of invocation cwd. (A CLI-supplied relative path
//! still resolves against the process working directory, as before.
//! For the cwd `.volamos` the two rules coincide; for `~/.volamos`
//! this was a behavior change, flagged in the changelog.)
//!
//! **Precedence**: for a "repeatable" setting (`-V`/`-a`), entries from
//! every source all apply (nothing is dropped) -- see [`merge`]'s doc
//! for why concatenating `higher ++ lower` is exactly right here,
//! reusing [`volamos_core::vfs`]'s existing first-match-wins name
//! lookup rather than needing any change there. For every other
//! setting: CLI > cwd file > program-dir file > global file > built-in
//! default. (The program-dir file sits *below* the cwd file so a
//! project-local override still beats a toolchain's own defaults, and
//! *above* `~/.volamos` so a blanket home default can't silently
//! override what a toolchain declares it needs -- issue #16's
//! precedence rationale.) The same physical file is never loaded
//! twice: if two sources name the same file (cwd == program dir, or
//! either == `$HOME`), only the highest-precedence occurrence is
//! consulted -- see [`load_all`].

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

/// `<program-dir>/.volamos`: a `.volamos` next to the binary being
/// launched, i.e. in `program`'s own containing directory (issue #16).
/// `None` for a bare program name with no directory part at all
/// (`volamos sc ...`) -- its containing directory is the process cwd,
/// which the cwd `.volamos` source already covers at higher
/// precedence.
fn program_dir_path(program: &str) -> Option<PathBuf> {
    let parent = Path::new(program).parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.join(".volamos"))
}

/// Re-anchors the relative host paths in one file's [`Overrides`]
/// (`VOLUME` host directories and `AUTO_ASSIGN`) onto `base`, the
/// config file's own directory -- the issue #16 resolution rule, applied
/// uniformly to every config source. Absolute paths pass through
/// untouched. Amiga-side paths (`ASSIGN` targets, `CWD`) aren't host
/// paths and are never anchored.
fn anchor_relative_host_paths(overrides: &mut Overrides, base: &Path) {
    for (_, hostdir) in &mut overrides.volumes {
        if hostdir.is_relative() {
            *hostdir = base.join(&*hostdir);
        }
    }
    if let Some(root) = &mut overrides.auto_assign_root
        && root.is_relative()
    {
        *root = base.join(&*root);
    }
}

/// Loads and parses one config file, re-anchoring its relative host
/// paths onto the file's own directory (see
/// [`anchor_relative_host_paths`]). `Ok(None)` means the file simply
/// doesn't exist -- the common case, since most users won't have one --
/// not an error; any other read failure, or a parse error, is reported
/// with `path` prefixed onto the underlying message.
fn load(path: &Path) -> Result<Option<Overrides>, String> {
    match std::fs::read_to_string(path) {
        Ok(source) => {
            let mut overrides = parse(&source).map_err(|e| format!("{}: {e}", path.display()))?;
            if let Some(dir) = path.parent() {
                anchor_relative_host_paths(&mut overrides, dir);
            }
            Ok(Some(overrides))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("couldn't read {}: {e}", path.display())),
    }
}

/// Loads `./.volamos`, `<program-dir>/.volamos`, and `~/.volamos` (any
/// or all may be absent) and merges them in that precedence order --
/// cwd over program dir over home, per this module's own doc. The same
/// physical file is loaded at most once even when two sources name it
/// (cwd equals program dir, or either equals `$HOME`) -- candidates
/// are compared by canonicalized path, and only the highest-precedence
/// occurrence is kept, so its repeatable `VOLUME`/`ASSIGN` entries
/// aren't duplicated. The result still needs merging against the CLI's
/// own [`Overrides`] (CLI wins over every file) -- see [`merge`] and
/// `crate::main`.
pub(crate) fn load_all(program: &str) -> Result<Overrides, String> {
    // Highest precedence first.
    load_candidates([local_path(), program_dir_path(program), global_path()])
}

/// [`load_all`]'s environment-free core: loads and merges candidate
/// config-file paths given highest-precedence first (a `None` slot is a
/// source that couldn't even name a path, e.g. no `$HOME`), skipping
/// any candidate that names the same physical file as an
/// already-loaded, higher-precedence one.
fn load_candidates(candidates: [Option<PathBuf>; 3]) -> Result<Overrides, String> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut merged = Overrides::default();
    for path in candidates.into_iter().flatten() {
        // Identity for dedup: the canonicalized path where the file
        // exists (canonicalize fails for a nonexistent file, which can
        // never collide with anything real anyway -- fall back to the
        // path as spelled).
        let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.contains(&identity) {
            continue;
        }
        seen.push(identity);
        let layer = load(&path)?.unwrap_or_default();
        // Earlier candidates are higher precedence, so the running
        // merge is always `higher` over each new, lower layer.
        merged = merge(merged, layer);
    }
    Ok(merged)
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

    /// A unique temp directory, cleaned up on drop (same pattern as
    /// volamos-core's own tests).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-config-test-{tag}-{pid}-{n}"));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    // --- program_dir_path (issue #16) ---

    #[test]
    fn program_dir_path_uses_the_containing_directory() {
        assert_eq!(
            program_dir_path("/opt/sasc/c/sc"),
            Some(PathBuf::from("/opt/sasc/c/.volamos"))
        );
        assert_eq!(
            program_dir_path("tools/sc"),
            Some(PathBuf::from("tools/.volamos"))
        );
    }

    #[test]
    fn program_dir_path_is_none_for_a_bare_program_name() {
        // A bare name's directory is the process cwd, which the cwd
        // .volamos source already covers at higher precedence.
        assert_eq!(program_dir_path("sc"), None);
    }

    // --- relative host paths anchor to the config file's directory ---

    #[test]
    fn load_anchors_relative_volume_and_auto_assign_to_the_file_dir() {
        let tmp = TempDir::new("anchor");
        let path = tmp.path().join(".volamos");
        std::fs::write(
            &path,
            "VOLUME=LIB:lib\nVOLUME=SC:/abs/sc\nAUTO_ASSIGN=volumes\n",
        )
        .unwrap();
        let overrides = load(&path).unwrap().expect("file exists");
        assert_eq!(
            overrides.volumes,
            vec![
                ("LIB".to_string(), tmp.path().join("lib")),
                ("SC".to_string(), PathBuf::from("/abs/sc")),
            ],
            "relative host paths anchor to the file's own directory; absolute ones pass through"
        );
        assert_eq!(overrides.auto_assign_root, Some(tmp.path().join("volumes")));
    }

    // --- load_candidates: three-layer precedence + dedup ---

    #[test]
    fn program_dir_layer_sits_between_cwd_and_global() {
        let cwd_dir = TempDir::new("layer-cwd");
        let prog_dir = TempDir::new("layer-prog");
        let home_dir = TempDir::new("layer-home");
        std::fs::write(cwd_dir.path().join(".volamos"), "STACK=1K\n").unwrap();
        std::fs::write(
            prog_dir.path().join(".volamos"),
            "STACK=2K\nRAM=2M\nVOLUME=SC:/prog/sc\n",
        )
        .unwrap();
        std::fs::write(
            home_dir.path().join(".volamos"),
            "STACK=4K\nRAM=4M\nCPU=68020\nVOLUME=SC:/home/sc\n",
        )
        .unwrap();

        let merged = load_candidates([
            Some(cwd_dir.path().join(".volamos")),
            Some(prog_dir.path().join(".volamos")),
            Some(home_dir.path().join(".volamos")),
        ])
        .unwrap();

        assert_eq!(merged.stack_size, Some(1024), "cwd wins over both");
        assert_eq!(
            merged.ram_size,
            Some(2 * 1024 * 1024),
            "program dir wins over global"
        );
        assert_eq!(
            merged.cpu_type,
            Some(volamos_core::backend::CpuType::M68020),
            "global still fills what nothing higher set"
        );
        assert_eq!(
            merged.volumes.first(),
            Some(&("SC".to_string(), PathBuf::from("/prog/sc"))),
            "for a NAME: in both layers, the program dir's mapping is found first"
        );
    }

    #[test]
    fn same_file_named_by_two_sources_is_loaded_once() {
        // cwd == program dir (launching a binary out of the current
        // directory): the shared .volamos must not contribute its
        // VOLUME entries twice.
        let dir = TempDir::new("dedup");
        let path = dir.path().join(".volamos");
        std::fs::write(&path, "VOLUME=SYS:/host/sys\n").unwrap();
        let merged = load_candidates([Some(path.clone()), Some(path), None]).unwrap();
        assert_eq!(
            merged.volumes,
            vec![("SYS".to_string(), PathBuf::from("/host/sys"))]
        );
    }

    #[test]
    fn missing_candidate_files_are_fine() {
        let dir = TempDir::new("all-missing");
        let merged = load_candidates([
            Some(dir.path().join(".volamos")),
            None,
            Some(dir.path().join("also-missing/.volamos")),
        ])
        .unwrap();
        assert_eq!(merged, Overrides::default());
    }
}
