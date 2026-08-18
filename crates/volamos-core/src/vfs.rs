//! Volume/assign manager and Amiga path translation.
//!
//! This module is pure host-side: it never touches [`crate::cpu::Cpu`] or
//! [`crate::memory::AddressSpace`]. It only deals in Amiga path strings
//! (`"SYS:work/foo"`), host [`PathBuf`]s, and real filesystem lookups via
//! `std::fs::read_dir`. That split is deliberate — it makes path
//! resolution fully unit-testable against real temp directories without
//! any CPU/memory scaffolding, and keeps dos.library handlers (T10/T11)
//! thin wrappers around [`Vfs::resolve`].
//!
//! # Amiga path syntax
//!
//! An Amiga path is `[Vol:]component[/component...]`, where:
//!
//! - A path containing `:` splits into a volume/assign name (before the
//!   `:`) and the rest of the path (after it). A colon with nothing
//!   before it (`":work"`) means "root of the current volume": the
//!   current directory's volume/assign name is reused.
//! - A path with no `:` at all is relative to the current directory.
//! - The rest of the path is split on `/`. A *non-empty* component
//!   descends into that subdirectory. An *empty* component — produced by
//!   a leading `/`, a doubled `//`, or (for a relative path) any `/` that
//!   appears before the first non-empty component — pops one level, i.e.
//!   it means "parent directory". This is the AmigaOS convention: `/`
//!   plays the role Unix gives to `..`, so `Vol:a/b//c` means `Vol:a/c`
//!   (from `a/b`, `//` pops back up to `a`, then descends into `c`), and
//!   `Vol:a//` / `Vol:a/..` (Amiga doesn't have `..`) means the parent of
//!   `Vol:a`.
//!
//! # Volumes, assigns, and auto-assign
//!
//! A path's leading name (before the first `:`) is looked up, in order:
//!
//! 1. **Assigns** — a name mapped (case-insensitively) to an ordered list
//!    of *target* Amiga paths (multi-assign). Each target is itself
//!    resolved recursively (a target may start with another assign or a
//!    volume name), bounded by [`MAX_ASSIGN_DEPTH`] to catch cycles.
//! 2. **Volumes** — a name mapped (case-insensitively) to a host
//!    directory; this is the base case of the recursion.
//! 3. **Auto-assign** — if neither of the above knows the name and an
//!    auto-assign root is configured, the name is treated as an assign
//!    to a single target directory `<auto_root>/<name>` on the host,
//!    mirroring vamos's auto-assign fallback (any unresolved volume/
//!    assign name becomes a subdirectory of one fallback root instead of
//!    a hard error). If no auto-assign root is configured, this is
//!    [`VfsError::UnknownVolume`].
//!
//! Multi-assign search order: for [`ResolveMode::MustExist`], each target
//! is tried in list order and the first one where the remaining path
//! actually resolves wins. For [`ResolveMode::ParentMustExist`] (file
//! creation), only the *first* target is used — this matches vamos,
//! which always creates new files/dirs in the first entry of a
//! multi-assign rather than searching.
//!
//! # Case-insensitive lookup
//!
//! AmigaOS filesystems are case-insensitive; host filesystems (at least
//! the ones this project targets in CI) are typically case-sensitive.
//! Each path component is matched against a `read_dir` listing of its
//! parent, preferring (in order): an exact-case match, else a *unique*
//! case-insensitive match, else — if there are multiple case-insensitive
//! matches — the one that sorts first byte-wise (a deterministic but
//! otherwise arbitrary tie-break; documented here since vamos's own
//! tie-break in this rare situation isn't behaviourally load-bearing for
//! any known corpus binary). For [`ResolveMode::MustExist`], no match at
//! all is [`VfsError::NotFound`]. For [`ResolveMode::ParentMustExist`],
//! the final component is matched the same way *if* it exists, but if it
//! doesn't exist at all, it's appended verbatim (preserving the caller's
//! given case) rather than erroring, so `Open(..., MODE_NEWFILE)` creates
//! a file with the name the guest asked for.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Recursion depth limit for assign expansion. Guards against assign
/// cycles (`A: -> B:foo`, `B: -> A:bar`) and pathological chains.
pub const MAX_ASSIGN_DEPTH: usize = 16;

/// Distinguishes the two path-resolution modes dos.library callers need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveMode {
    /// Every component, including the last, must already exist on the
    /// host. Used for `Open(MODE_OLDFILE)`, `Lock`, `Examine`, and
    /// similar "the object must be there" operations.
    MustExist,
    /// Every component *except* the last must already exist; the last
    /// component may or may not exist. Used for `Open(MODE_NEWFILE)`
    /// and other "create if missing" operations. If the last component
    /// exists, it's matched case-insensitively like any other
    /// component; if it doesn't exist, it's appended to the resolved
    /// host path exactly as given (preserving case for the new file).
    ParentMustExist,
}

/// Errors from Amiga path resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsError {
    /// A required path component (or the whole target, under
    /// [`ResolveMode::MustExist`]) doesn't exist on the host.
    NotFound {
        /// The full Amiga path that was being resolved.
        amiga_path: String,
    },
    /// A path component that should be a directory (i.e. everything but
    /// possibly the last component) exists on the host but isn't one.
    NotADirectory {
        /// The full Amiga path that was being resolved.
        amiga_path: String,
    },
    /// The leading name of a path isn't a known volume or assign, and no
    /// auto-assign root is configured to fall back to.
    UnknownVolume {
        /// The unrecognized volume/assign name.
        name: String,
    },
    /// Assign expansion recursed past [`MAX_ASSIGN_DEPTH`], almost
    /// certainly because of a cycle (e.g. `A:` resolving through `B:`
    /// back to `A:`).
    AssignLoop {
        /// The assign name whose expansion exceeded the depth limit.
        name: String,
    },
    /// The Amiga path syntax itself is malformed (e.g. more than one
    /// `:`), or an attempt was made to pop above the volume root.
    InvalidPath {
        /// A human-readable description of what was wrong.
        reason: String,
    },
    /// A host filesystem operation failed for a reason not covered
    /// above (permissions, I/O error, etc).
    Io {
        /// `Display` of the underlying `std::io::Error`.
        message: String,
    },
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsError::NotFound { amiga_path } => {
                write!(f, "object not found: {amiga_path}")
            }
            VfsError::NotADirectory { amiga_path } => {
                write!(f, "not a directory: {amiga_path}")
            }
            VfsError::UnknownVolume { name } => {
                write!(f, "unknown volume or assign: {name}:")
            }
            VfsError::AssignLoop { name } => {
                write!(f, "assign loop detected while expanding {name}:")
            }
            VfsError::InvalidPath { reason } => {
                write!(f, "invalid path: {reason}")
            }
            VfsError::Io { message } => write!(f, "I/O error: {message}"),
        }
    }
}

impl std::error::Error for VfsError {}

impl From<std::io::Error> for VfsError {
    fn from(err: std::io::Error) -> Self {
        VfsError::Io {
            message: err.to_string(),
        }
    }
}

/// One parsed Amiga path: an optional leading volume/assign name (`None`
/// for a path with no `:`, i.e. relative to the current directory; `Some("")`
/// for a bare leading `:`, i.e. "root of the current volume"), plus a
/// sequence of components where `Component::Parent` is an empty
/// (`/`-produced) component and `Component::Named` is a real path
/// segment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedPath {
    /// `None`: no `:` in the path, purely relative. `Some(name)`: text
    /// before the `:` (possibly empty for a bare leading `:`).
    volume: Option<String>,
    components: Vec<Component>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Component {
    /// Step up one directory level (produced by a leading or doubled `/`).
    Parent,
    /// Descend into a named subdirectory/file.
    Named(String),
}

/// Split the part of a path after the volume/assign prefix (if any) into
/// [`Component`]s. A leading `/` or an internal `//` produces a
/// [`Component::Parent`]; anything else between slashes is a
/// [`Component::Named`]. A trailing `/` contributes nothing extra (it
/// simply terminates the last component).
fn split_components(rest: &str) -> Vec<Component> {
    if rest.is_empty() {
        return Vec::new();
    }
    let mut components = Vec::new();
    for part in rest.split('/') {
        if part.is_empty() {
            components.push(Component::Parent);
        } else {
            components.push(Component::Named(part.to_string()));
        }
    }
    // A trailing '/' produces a spurious empty-string part from
    // `split('/')` (e.g. "a/" -> ["a", ""]), which would wrongly count
    // as a Parent step. Drop a single trailing empty component that
    // corresponds to a trailing slash rather than a genuine doubled
    // slash: `split` already turned "a//" into ["a", "", ""], where the
    // *last* empty is the trailing-slash artifact and the middle one is
    // real. So: if the input didn't end with '/', nothing to trim; if
    // it did, drop exactly the final Parent we just pushed.
    if rest.ends_with('/') && matches!(components.last(), Some(Component::Parent)) {
        components.pop();
    }
    components
}

fn parse_path(amiga_path: &str) -> Result<ParsedPath, VfsError> {
    let colon_count = amiga_path.matches(':').count();
    if colon_count > 1 {
        return Err(VfsError::InvalidPath {
            reason: format!("more than one ':' in path: {amiga_path}"),
        });
    }
    if let Some(idx) = amiga_path.find(':') {
        let (vol, rest) = amiga_path.split_at(idx);
        let rest = &rest[1..]; // skip the ':'
        Ok(ParsedPath {
            volume: Some(vol.to_string()),
            components: split_components(rest),
        })
    } else {
        Ok(ParsedPath {
            volume: None,
            components: split_components(amiga_path),
        })
    }
}

/// Configuration for a [`Vfs`]: volume map, assign map, auto-assign
/// fallback root, and current directory.
#[derive(Debug, Clone, Default)]
pub struct VfsConfig {
    /// Volume name (case-insensitive) -> host directory.
    pub volumes: Vec<(String, PathBuf)>,
    /// Assign name (case-insensitive) -> ordered list of Amiga path
    /// targets (multi-assign search order).
    pub assigns: Vec<(String, Vec<String>)>,
    /// When set, an unrecognized `name:` is treated as an assign to
    /// `<auto_root>/name` on the host, rather than an error.
    pub auto_assign_root: Option<PathBuf>,
    /// The current directory, as an Amiga path. Must resolve to a
    /// volume-rooted path (i.e. eventually bottom out in a known/
    /// auto-assignable volume) — validated when the [`Vfs`] is built
    /// and whenever [`Vfs::set_cwd`] is called.
    pub cwd: String,
}

/// The volume/assign manager. See the module docs for path semantics.
#[derive(Debug, Clone)]
pub struct Vfs {
    config: VfsConfig,
}

impl Vfs {
    /// Build a `Vfs` from a config, validating that `cwd` resolves.
    pub fn new(config: VfsConfig) -> Result<Self, VfsError> {
        let vfs = Vfs { config };
        // Validate cwd resolves to *some* volume root (doesn't need to
        // exist on the host as a directory for us to accept it as
        // configuration, but the volume/assign chain must be sound).
        vfs.expand_to_volume_root(&vfs.config.cwd)?;
        Ok(vfs)
    }

    /// The current directory, as an Amiga path string.
    pub fn cwd(&self) -> &str {
        &self.config.cwd
    }

    /// Set the current directory. The new value is validated (assign/
    /// volume chain must resolve) before being accepted; on error the
    /// old cwd is left in place.
    pub fn set_cwd(&mut self, amiga_path: &str) -> Result<(), VfsError> {
        // Resolve relative to the *current* cwd first, so callers can
        // pass relative paths (e.g. CurrentDir with a Lock-relative
        // path), then normalize back to an absolute Amiga-path string
        // for storage.
        let normalized = self.normalize_to_absolute(amiga_path)?;
        self.expand_to_volume_root(&normalized)?;
        self.config.cwd = normalized;
        Ok(())
    }

    /// Resolve an Amiga path to a host path.
    pub fn resolve(&self, amiga_path: &str, mode: ResolveMode) -> Result<PathBuf, VfsError> {
        let parsed = parse_path(amiga_path)?;
        let vol_name = self.leading_name(&parsed, amiga_path)?;
        let components = self.effective_components(&parsed)?;
        let targets = self.assign_targets(&vol_name, 0)?;
        self.resolve_against_targets(&targets, &components, mode, amiga_path)
    }

    /// Determine the leading volume/assign name for a parsed path,
    /// falling back to the cwd's volume for a bare leading `:` or a
    /// fully relative path share the same starting point: the cwd's
    /// leading name.
    fn leading_name(&self, parsed: &ParsedPath, amiga_path: &str) -> Result<String, VfsError> {
        match &parsed.volume {
            Some(name) if !name.is_empty() => Ok(name.clone()),
            _ => {
                // Either "no ':' at all" (relative) or "bare ':'"
                // (root of current volume): both start from the cwd's
                // leading name.
                let cwd_parsed = parse_path(&self.config.cwd)?;
                match cwd_parsed.volume {
                    Some(name) if !name.is_empty() => Ok(name),
                    _ => Err(VfsError::InvalidPath {
                        reason: format!(
                            "cannot resolve '{amiga_path}': current directory '{}' has no volume",
                            self.config.cwd
                        ),
                    }),
                }
            }
        }
    }

    /// Given a parsed path's leading name and its components, work out
    /// the *full* effective component list including anything
    /// contributed by the cwd (for relative paths) or by "root of
    /// current volume" (for bare leading `:`, which contributes no
    /// extra components — it explicitly means the root).
    fn effective_components(&self, parsed: &ParsedPath) -> Result<Vec<Component>, VfsError> {
        match &parsed.volume {
            Some(_) => Ok(parsed.components.clone()),
            None => {
                // Relative path: prefix with the cwd's own components
                // (not the cwd's volume name, just its path
                // components), then apply this path's components
                // (including any Parent pops) on top.
                let cwd_parsed = parse_path(&self.config.cwd)?;
                let mut combined = cwd_parsed.components;
                combined.extend(parsed.components.clone());
                Ok(combined)
            }
        }
    }

    /// Normalize an Amiga path (which may be relative, or a bare `:`)
    /// into an absolute `Vol:a/b/c` string, applying `Parent` pops.
    fn normalize_to_absolute(&self, amiga_path: &str) -> Result<String, VfsError> {
        let parsed = parse_path(amiga_path)?;
        let vol_name = self.leading_name(&parsed, amiga_path)?;
        let components = self.effective_components(&parsed)?;
        let named = apply_parent_pops(&components, amiga_path)?;
        Ok(format!("{vol_name}:{}", named.join("/")))
    }

    /// Expand an assign/volume name to its list of Amiga-path targets
    /// is not what this does; this resolves a *volume* root (i.e. the
    /// base case: `name` must eventually be an actual volume, not an
    /// assign) — used only for cwd validation, where we just need to
    /// know the chain terminates soundly.
    fn expand_to_volume_root(&self, amiga_path: &str) -> Result<PathBuf, VfsError> {
        let parsed = parse_path(amiga_path)?;
        let vol_name = self.leading_name(&parsed, amiga_path)?;
        self.host_dir_for_name(&vol_name, 0)
    }

    /// Resolve a volume/assign name down to a single host directory by
    /// following assign chains to their *first* target only (used for
    /// cwd validation; a real resolve uses [`Self::assign_targets`] to
    /// get the full multi-assign target list instead).
    fn host_dir_for_name(&self, name: &str, depth: usize) -> Result<PathBuf, VfsError> {
        if depth > MAX_ASSIGN_DEPTH {
            return Err(VfsError::AssignLoop {
                name: name.to_string(),
            });
        }
        if let Some(dir) = self.lookup_volume(name) {
            return Ok(dir);
        }
        if let Some(targets) = self.lookup_assign(name) {
            let first = targets.first().ok_or_else(|| VfsError::InvalidPath {
                reason: format!("assign '{name}:' has no targets"),
            })?;
            let target_parsed = parse_path(first)?;
            let target_vol = self.leading_name(&target_parsed, first)?;
            let base = self.host_dir_for_name(&target_vol, depth + 1)?;
            let named = apply_parent_pops(&target_parsed.components, first)?;
            return Ok(join_components(&base, &named));
        }
        if let Some(root) = &self.config.auto_assign_root {
            return Ok(root.join(name));
        }
        Err(VfsError::UnknownVolume {
            name: name.to_string(),
        })
    }

    fn lookup_volume(&self, name: &str) -> Option<PathBuf> {
        self.config
            .volumes
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, dir)| dir.clone())
    }

    fn lookup_assign(&self, name: &str) -> Option<Vec<String>> {
        self.config
            .assigns
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, targets)| targets.clone())
    }

    /// Expand `name:` into the full ordered list of host directories a
    /// multi-assign search should try (for a plain volume, this is a
    /// single-element list). Each assign target is itself resolved
    /// recursively (it may reference another assign or a volume),
    /// bounded by [`MAX_ASSIGN_DEPTH`].
    fn assign_targets(&self, name: &str, depth: usize) -> Result<Vec<PathBuf>, VfsError> {
        if depth > MAX_ASSIGN_DEPTH {
            return Err(VfsError::AssignLoop {
                name: name.to_string(),
            });
        }
        if let Some(dir) = self.lookup_volume(name) {
            return Ok(vec![dir]);
        }
        if let Some(targets) = self.lookup_assign(name) {
            if targets.is_empty() {
                return Err(VfsError::InvalidPath {
                    reason: format!("assign '{name}:' has no targets"),
                });
            }
            let mut result = Vec::new();
            for target in &targets {
                let target_parsed = parse_path(target)?;
                let target_vol = self.leading_name(&target_parsed, target)?;
                let bases = self.assign_targets(&target_vol, depth + 1)?;
                let named = apply_parent_pops(&target_parsed.components, target)?;
                for base in bases {
                    result.push(join_components(&base, &named));
                }
            }
            return Ok(result);
        }
        if let Some(root) = &self.config.auto_assign_root {
            return Ok(vec![root.join(name)]);
        }
        Err(VfsError::UnknownVolume {
            name: name.to_string(),
        })
    }

    /// Try each candidate base directory in order, resolving
    /// `components` (which may include `Parent` pops applying at the
    /// *volume-root* level too, though popping above a target's root
    /// is treated as clamping at that root rather than an error —
    /// vamos-style leniency, since AmigaOS itself just no-ops a
    /// too-many-parents case at the volume boundary) against the real
    /// host filesystem.
    fn resolve_against_targets(
        &self,
        targets: &[PathBuf],
        components: &[Component],
        mode: ResolveMode,
        original_path: &str,
    ) -> Result<PathBuf, VfsError> {
        let named = apply_parent_pops(components, original_path)?;
        match mode {
            ResolveMode::MustExist => {
                let mut last_err = None;
                for base in targets {
                    match resolve_named_components(base, &named, true, original_path) {
                        Ok(path) => return Ok(path),
                        Err(e) => last_err = Some(e),
                    }
                }
                Err(last_err.unwrap_or(VfsError::NotFound {
                    amiga_path: original_path.to_string(),
                }))
            }
            ResolveMode::ParentMustExist => {
                let base = targets.first().ok_or_else(|| VfsError::InvalidPath {
                    reason: format!("no resolution targets for '{original_path}'"),
                })?;
                resolve_named_components(base, &named, false, original_path)
            }
        }
    }
}

/// Apply `Parent` (pop-one-level) components against a flat list of
/// preceding `Named` components, producing the final list of named
/// segments to walk from the volume/assign root. Popping past the root
/// (more `Parent`s than preceding `Named`s) is clamped at the root
/// rather than treated as an error, matching AmigaOS/vamos leniency.
fn apply_parent_pops(
    components: &[Component],
    _original_path: &str,
) -> Result<Vec<String>, VfsError> {
    let mut stack: Vec<String> = Vec::new();
    for c in components {
        match c {
            Component::Named(name) => stack.push(name.clone()),
            Component::Parent => {
                stack.pop();
            }
        }
    }
    Ok(stack)
}

fn join_components(base: &Path, named: &[String]) -> PathBuf {
    let mut path = base.to_path_buf();
    for n in named {
        path.push(n);
    }
    path
}

/// Walk `named` components from `base` on the real host filesystem,
/// matching each one case-insensitively against `read_dir` output. If
/// `last_must_exist` is true (MustExist mode), every component
/// including the last must be found. Otherwise (ParentMustExist mode),
/// every component but the last must be found and be a directory; the
/// last component is matched case-insensitively if present, else
/// appended verbatim (preserving the caller's case) so newly created
/// files/dirs get the name the guest asked for.
fn resolve_named_components(
    base: &Path,
    named: &[String],
    last_must_exist: bool,
    original_path: &str,
) -> Result<PathBuf, VfsError> {
    let mut current = base.to_path_buf();
    if named.is_empty() {
        return Ok(current);
    }
    let last_index = named.len() - 1;
    for (i, name) in named.iter().enumerate() {
        let is_last = i == last_index;
        let must_exist = !is_last || last_must_exist;
        match find_case_insensitive(&current, name)? {
            Some(matched_name) => {
                current.push(matched_name);
            }
            None => {
                if must_exist {
                    return Err(VfsError::NotFound {
                        amiga_path: original_path.to_string(),
                    });
                } else {
                    // Last component of a ParentMustExist resolution,
                    // doesn't exist yet: append verbatim, preserving
                    // the caller's given case.
                    current.push(name);
                    return Ok(current);
                }
            }
        }
        if !is_last {
            let meta = fs::metadata(&current)?;
            if !meta.is_dir() {
                return Err(VfsError::NotADirectory {
                    amiga_path: original_path.to_string(),
                });
            }
        }
    }
    Ok(current)
}

/// Look up `name` within `dir` on the host, case-insensitively.
/// Preference order: exact-case match; else the unique case-insensitive
/// match; else, if there are multiple case-insensitive matches, the one
/// that sorts first (byte-wise) among them — a deterministic tie-break
/// (see module docs). Returns `Ok(None)` if there's no match at all
/// (including if `dir` doesn't exist or isn't a directory: that's
/// reported by the caller via a subsequent `NotADirectory`/`NotFound`,
/// not here).
fn find_case_insensitive(dir: &Path, name: &str) -> Result<Option<String>, VfsError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut ci_matches: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let entry_name = entry.file_name();
        let entry_name = entry_name.to_string_lossy().to_string();
        if entry_name == name {
            return Ok(Some(entry_name));
        }
        if entry_name.eq_ignore_ascii_case(name) {
            ci_matches.push(entry_name);
        }
    }
    if ci_matches.is_empty() {
        return Ok(None);
    }
    ci_matches.sort();
    Ok(Some(ci_matches.remove(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp directory, cleaned up on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("volamos-vfs-test-{tag}-{pid}-{n}"));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn mkdir(&self, rel: &str) -> PathBuf {
            let p = self.path.join(rel);
            fs::create_dir_all(&p).expect("mkdir");
            p
        }

        fn touch(&self, rel: &str) -> PathBuf {
            let p = self.path.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("mkdir parent");
            }
            fs::write(&p, b"test").expect("write file");
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Whether `dir`'s host filesystem is case-sensitive. Tests that
    /// need two on-disk entries differing only by case (to exercise
    /// the "multiple case-insensitive matches" and "exact match
    /// preferred" paths for real) can't run meaningfully on a
    /// case-insensitive host filesystem (the default on macOS/APFS,
    /// where creating "file.txt" after "File.txt" just overwrites the
    /// same directory entry) — such tests call this and skip
    /// themselves early rather than asserting on a filesystem that
    /// can't represent the scenario.
    fn is_case_sensitive_fs(dir: &Path) -> bool {
        let lower = dir.join(".vfs_case_probe_x");
        let upper = dir.join(".vfs_case_probe_X");
        let _ = fs::remove_file(&lower);
        let _ = fs::remove_file(&upper);
        fs::write(&lower, b"x").expect("write probe file");
        let sensitive = !upper.exists();
        let _ = fs::remove_file(&lower);
        let _ = fs::remove_file(&upper);
        sensitive
    }

    fn simple_vfs(root: &Path) -> Vfs {
        Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), root.to_path_buf())],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .expect("build vfs")
    }

    // --- component splitting ---

    #[test]
    fn split_components_basic() {
        assert_eq!(
            split_components("a/b/c"),
            vec![
                Component::Named("a".into()),
                Component::Named("b".into()),
                Component::Named("c".into()),
            ]
        );
    }

    #[test]
    fn split_components_leading_slash_is_parent() {
        assert_eq!(
            split_components("/foo"),
            vec![Component::Parent, Component::Named("foo".into())]
        );
    }

    #[test]
    fn split_components_doubled_slash_is_parent() {
        assert_eq!(
            split_components("a//b"),
            vec![
                Component::Named("a".into()),
                Component::Parent,
                Component::Named("b".into()),
            ]
        );
    }

    #[test]
    fn split_components_trailing_slash_is_not_parent() {
        assert_eq!(
            split_components("a/b/"),
            vec![Component::Named("a".into()), Component::Named("b".into())]
        );
    }

    #[test]
    fn split_components_empty() {
        assert_eq!(split_components(""), Vec::<Component>::new());
    }

    // --- volume mapping and basic resolution ---

    #[test]
    fn resolves_volume_root() {
        let tmp = TempDir::new("volroot");
        let vfs = simple_vfs(tmp.path());
        let resolved = vfs.resolve("SYS:", ResolveMode::MustExist).unwrap();
        assert_eq!(resolved, tmp.path());
    }

    #[test]
    fn resolves_simple_nested_path() {
        let tmp = TempDir::new("nested");
        tmp.mkdir("work");
        tmp.touch("work/foo.txt");
        let vfs = simple_vfs(tmp.path());
        let resolved = vfs
            .resolve("SYS:work/foo.txt", ResolveMode::MustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("work/foo.txt"));
    }

    #[test]
    fn volume_name_is_case_insensitive() {
        let tmp = TempDir::new("volci");
        tmp.touch("foo.txt");
        let vfs = simple_vfs(tmp.path());
        let resolved = vfs.resolve("sys:foo.txt", ResolveMode::MustExist).unwrap();
        assert_eq!(resolved, tmp.path().join("foo.txt"));
    }

    #[test]
    fn unknown_volume_without_auto_assign_errors() {
        let tmp = TempDir::new("unknownvol");
        let vfs = simple_vfs(tmp.path());
        let err = vfs.resolve("NOPE:foo", ResolveMode::MustExist).unwrap_err();
        assert_eq!(
            err,
            VfsError::UnknownVolume {
                name: "NOPE".to_string()
            }
        );
    }

    #[test]
    fn missing_file_is_not_found() {
        let tmp = TempDir::new("missing");
        let vfs = simple_vfs(tmp.path());
        let err = vfs
            .resolve("SYS:nope.txt", ResolveMode::MustExist)
            .unwrap_err();
        assert_eq!(
            err,
            VfsError::NotFound {
                amiga_path: "SYS:nope.txt".to_string()
            }
        );
    }

    #[test]
    fn component_through_a_file_is_not_a_directory() {
        let tmp = TempDir::new("notadir");
        tmp.touch("plainfile");
        let vfs = simple_vfs(tmp.path());
        let err = vfs
            .resolve("SYS:plainfile/sub", ResolveMode::MustExist)
            .unwrap_err();
        assert_eq!(
            err,
            VfsError::NotADirectory {
                amiga_path: "SYS:plainfile/sub".to_string()
            }
        );
    }

    // --- relative-to-cwd resolution ---

    #[test]
    fn relative_path_resolves_against_cwd() {
        let tmp = TempDir::new("relcwd");
        tmp.mkdir("work");
        tmp.touch("work/foo.txt");
        let mut config = VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        };
        config.cwd = "SYS:work".to_string();
        let vfs = Vfs::new(config).unwrap();
        let resolved = vfs.resolve("foo.txt", ResolveMode::MustExist).unwrap();
        assert_eq!(resolved, tmp.path().join("work/foo.txt"));
    }

    #[test]
    fn relative_path_descends_below_cwd() {
        let tmp = TempDir::new("reldescend");
        tmp.mkdir("work/sub");
        tmp.touch("work/sub/bar.txt");
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:work".to_string(),
        })
        .unwrap();
        let resolved = vfs.resolve("sub/bar.txt", ResolveMode::MustExist).unwrap();
        assert_eq!(resolved, tmp.path().join("work/sub/bar.txt"));
    }

    // --- leading ':' semantics ---

    #[test]
    fn leading_colon_means_root_of_current_volume() {
        let tmp = TempDir::new("leadcolon");
        tmp.mkdir("work");
        tmp.touch("root.txt");
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:work".to_string(),
        })
        .unwrap();
        let resolved = vfs.resolve(":root.txt", ResolveMode::MustExist).unwrap();
        assert_eq!(resolved, tmp.path().join("root.txt"));
    }

    // --- '/' parent semantics ---

    #[test]
    fn slash_pops_one_level_absolute() {
        let tmp = TempDir::new("slashpop");
        tmp.mkdir("a/b");
        tmp.mkdir("a/c");
        let vfs = simple_vfs(tmp.path());
        // SYS:a/b//c -> from a/b, // pops to a, then descend into c.
        let resolved = vfs.resolve("SYS:a/b//c", ResolveMode::MustExist).unwrap();
        assert_eq!(resolved, tmp.path().join("a/c"));
    }

    #[test]
    fn leading_slash_after_colon_pops_from_root_clamped() {
        let tmp = TempDir::new("clamp");
        tmp.mkdir("a");
        let vfs = simple_vfs(tmp.path());
        // Popping above the volume root clamps at the root rather than
        // erroring (AmigaOS/vamos leniency).
        let resolved = vfs.resolve("SYS:/a", ResolveMode::MustExist).unwrap();
        assert_eq!(resolved, tmp.path().join("a"));
    }

    #[test]
    fn relative_path_pops_above_cwd() {
        let tmp = TempDir::new("relpop");
        tmp.mkdir("work/sub");
        tmp.touch("work/sibling.txt");
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![],
            auto_assign_root: None,
            cwd: "SYS:work/sub".to_string(),
        })
        .unwrap();
        // "/sibling.txt" from cwd work/sub: leading '/' pops once (to
        // "work"), then descends into sibling.txt.
        let resolved = vfs.resolve("/sibling.txt", ResolveMode::MustExist).unwrap();
        assert_eq!(resolved, tmp.path().join("work/sibling.txt"));
    }

    // --- case-insensitive matching ---

    #[test]
    fn case_insensitive_unique_match() {
        let tmp = TempDir::new("ciunique");
        tmp.mkdir("Work");
        tmp.touch("Work/FooBar.TXT");
        let vfs = simple_vfs(tmp.path());
        let resolved = vfs
            .resolve("SYS:work/foobar.txt", ResolveMode::MustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("Work/FooBar.TXT"));
    }

    #[test]
    fn exact_case_preferred_over_other_case_variants() {
        let tmp = TempDir::new("ciexact");
        if !is_case_sensitive_fs(tmp.path()) {
            eprintln!("skipping: host filesystem is case-insensitive");
            return;
        }
        tmp.mkdir("work");
        tmp.touch("work/File.txt");
        tmp.touch("work/file.txt");
        let vfs = simple_vfs(tmp.path());
        // Exact-case request should hit the exact-case file even though
        // a case-insensitive sibling also exists.
        let resolved = vfs
            .resolve("SYS:work/File.txt", ResolveMode::MustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("work/File.txt"));
        let resolved2 = vfs
            .resolve("SYS:work/file.txt", ResolveMode::MustExist)
            .unwrap();
        assert_eq!(resolved2, tmp.path().join("work/file.txt"));
    }

    #[test]
    fn ambiguous_case_insensitive_match_picks_sorted_first() {
        let tmp = TempDir::new("ciambig");
        if !is_case_sensitive_fs(tmp.path()) {
            eprintln!("skipping: host filesystem is case-insensitive");
            return;
        }
        tmp.mkdir("work");
        tmp.touch("work/Foo.txt");
        tmp.touch("work/foo.TXT");
        let vfs = simple_vfs(tmp.path());
        // Neither is an exact match for "FOO.txt"; both are
        // case-insensitive matches. Deterministic tie-break: sorts
        // first byte-wise. "Foo.txt" < "foo.TXT" (uppercase 'F' < 'f').
        let resolved = vfs
            .resolve("SYS:work/FOO.txt", ResolveMode::MustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("work/Foo.txt"));
    }

    // --- assign expansion ---

    #[test]
    fn simple_assign_resolves_to_volume_subpath() {
        let tmp = TempDir::new("assignsimple");
        tmp.mkdir("libs");
        tmp.touch("libs/foo.library");
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![("LIBS".to_string(), vec!["SYS:libs".to_string()])],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .unwrap();
        let resolved = vfs
            .resolve("LIBS:foo.library", ResolveMode::MustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("libs/foo.library"));
    }

    #[test]
    fn multi_assign_tries_targets_in_order_for_must_exist() {
        let tmp = TempDir::new("multiassign");
        tmp.mkdir("libsA");
        tmp.mkdir("libsB");
        tmp.touch("libsB/only_in_b.library");
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![(
                "LIBS".to_string(),
                vec!["SYS:libsA".to_string(), "SYS:libsB".to_string()],
            )],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .unwrap();
        // Not in libsA, found in libsB: search order matters.
        let resolved = vfs
            .resolve("LIBS:only_in_b.library", ResolveMode::MustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("libsB/only_in_b.library"));
    }

    #[test]
    fn multi_assign_first_hit_wins() {
        let tmp = TempDir::new("multiwins");
        tmp.mkdir("libsA");
        tmp.mkdir("libsB");
        tmp.touch("libsA/dup.library");
        tmp.touch("libsB/dup.library");
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![(
                "LIBS".to_string(),
                vec!["SYS:libsA".to_string(), "SYS:libsB".to_string()],
            )],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .unwrap();
        let resolved = vfs
            .resolve("LIBS:dup.library", ResolveMode::MustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("libsA/dup.library"));
    }

    #[test]
    fn multi_assign_parent_must_exist_uses_first_target_only() {
        let tmp = TempDir::new("multinew");
        tmp.mkdir("libsA");
        tmp.mkdir("libsB");
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![(
                "LIBS".to_string(),
                vec!["SYS:libsA".to_string(), "SYS:libsB".to_string()],
            )],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .unwrap();
        let resolved = vfs
            .resolve("LIBS:new.library", ResolveMode::ParentMustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("libsA/new.library"));
    }

    #[test]
    fn assign_can_reference_another_assign_recursively() {
        let tmp = TempDir::new("assignrecurse");
        tmp.mkdir("libs/sub");
        tmp.touch("libs/sub/thing");
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![
                ("LIBS".to_string(), vec!["SYS:libs".to_string()]),
                ("SUBLIBS".to_string(), vec!["LIBS:sub".to_string()]),
            ],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .unwrap();
        let resolved = vfs
            .resolve("SUBLIBS:thing", ResolveMode::MustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("libs/sub/thing"));
    }

    #[test]
    fn assign_cycle_is_detected() {
        let tmp = TempDir::new("assigncycle");
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![
                ("A".to_string(), vec!["B:foo".to_string()]),
                ("B".to_string(), vec!["A:bar".to_string()]),
            ],
            auto_assign_root: None,
            cwd: "SYS:".to_string(),
        })
        .unwrap();
        let err = vfs.resolve("A:x", ResolveMode::MustExist).unwrap_err();
        assert!(matches!(err, VfsError::AssignLoop { .. }));
    }

    // --- auto-assign fallback ---

    #[test]
    fn auto_assign_fallback_maps_unknown_name_under_root() {
        let tmp = TempDir::new("autoassign");
        let auto_root = tmp.mkdir("auto");
        fs::create_dir_all(auto_root.join("T")).unwrap();
        fs::write(auto_root.join("T/scratch"), b"x").unwrap();
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![],
            auto_assign_root: Some(auto_root.clone()),
            cwd: "SYS:".to_string(),
        })
        .unwrap();
        let resolved = vfs.resolve("T:scratch", ResolveMode::MustExist).unwrap();
        assert_eq!(resolved, auto_root.join("T/scratch"));
    }

    #[test]
    fn auto_assign_does_not_shadow_known_volume() {
        let tmp = TempDir::new("autoshadow");
        tmp.touch("real.txt");
        let auto_root = tmp.mkdir("auto");
        let vfs = Vfs::new(VfsConfig {
            volumes: vec![("SYS".to_string(), tmp.path().to_path_buf())],
            assigns: vec![],
            auto_assign_root: Some(auto_root),
            cwd: "SYS:".to_string(),
        })
        .unwrap();
        let resolved = vfs.resolve("SYS:real.txt", ResolveMode::MustExist).unwrap();
        assert_eq!(resolved, tmp.path().join("real.txt"));
    }

    // --- MODE_NEWFILE / ParentMustExist ---

    #[test]
    fn parent_must_exist_appends_new_name_verbatim_preserving_case() {
        let tmp = TempDir::new("newfilecase");
        tmp.mkdir("work");
        let vfs = simple_vfs(tmp.path());
        let resolved = vfs
            .resolve("SYS:work/NewFile.TXT", ResolveMode::ParentMustExist)
            .unwrap();
        assert_eq!(resolved, tmp.path().join("work/NewFile.TXT"));
    }

    #[test]
    fn parent_must_exist_matches_existing_last_component_case_insensitively() {
        let tmp = TempDir::new("newfileexisting");
        tmp.mkdir("work");
        tmp.touch("work/Existing.txt");
        let vfs = simple_vfs(tmp.path());
        let resolved = vfs
            .resolve("SYS:work/existing.txt", ResolveMode::ParentMustExist)
            .unwrap();
        // Should match the existing on-disk case, not the caller's case.
        assert_eq!(resolved, tmp.path().join("work/Existing.txt"));
    }

    #[test]
    fn parent_must_exist_errors_if_parent_missing() {
        let tmp = TempDir::new("newfilenoparent");
        let vfs = simple_vfs(tmp.path());
        let err = vfs
            .resolve("SYS:nosuchdir/new.txt", ResolveMode::ParentMustExist)
            .unwrap_err();
        assert_eq!(
            err,
            VfsError::NotFound {
                amiga_path: "SYS:nosuchdir/new.txt".to_string()
            }
        );
    }

    // --- cwd getter/setter ---

    #[test]
    fn set_cwd_validates_and_updates() {
        let tmp = TempDir::new("setcwd");
        let mut vfs = simple_vfs(tmp.path());
        vfs.set_cwd("SYS:work").unwrap();
        assert_eq!(vfs.cwd(), "SYS:work");
    }

    #[test]
    fn set_cwd_rejects_unknown_volume() {
        let tmp = TempDir::new("setcwdbad");
        let mut vfs = simple_vfs(tmp.path());
        let err = vfs.set_cwd("NOPE:work").unwrap_err();
        assert!(matches!(err, VfsError::UnknownVolume { .. }));
        // Old cwd preserved.
        assert_eq!(vfs.cwd(), "SYS:");
    }

    #[test]
    fn set_cwd_normalizes_relative_input() {
        let tmp = TempDir::new("setcwdrel");
        let mut vfs = simple_vfs(tmp.path());
        vfs.set_cwd("SYS:a/b").unwrap();
        // '/' pops one level (Amiga semantics), so "/c" from "SYS:a/b"
        // normalizes to "SYS:a/c".
        vfs.set_cwd("/c").unwrap();
        assert_eq!(vfs.cwd(), "SYS:a/c");
    }

    #[test]
    fn invalid_path_multiple_colons() {
        let tmp = TempDir::new("multicolon");
        let vfs = simple_vfs(tmp.path());
        let err = vfs
            .resolve("SYS:foo:bar", ResolveMode::MustExist)
            .unwrap_err();
        assert!(matches!(err, VfsError::InvalidPath { .. }));
    }
}
