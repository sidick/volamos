//! A shadow-memory sanitizer for the guest address space (issue #65).
//!
//! # What this is for
//!
//! `volamos` today faithfully emulates AmigaOS's own memory model: any
//! guest address is readable and writable, out-of-range accesses read
//! as `0`/are silently dropped (see
//! [`crate::memory::AddressSpace`]'s doc), and nothing stops a guest
//! program from reading memory it never allocated, writing past the end
//! of a block it did allocate, or reading a buffer before anything
//! initialized it. That total, never-fails behavior is exactly right
//! for faithfully *running* real AmigaOS binaries (a real Amiga has no
//! MMU-backed guard pages either, for the CPU models this runtime
//! targets), but it means a guest heap-corruption bug that a real
//! ASan/Valgrind-style tool would catch immediately instead corrupts
//! some other unrelated structure silently, and only crashes (if at
//! all) far away from the real bug.
//!
//! [`ShadowMap`] adds an opt-in (`--sanitize`, off by default) detector
//! bolted onto the existing, unconditionally-total access path: one
//! shadow byte per guest byte records whether that byte is currently
//! [`Valid`](ShadowState::Valid), [`Uninit`](ShadowState::Uninit)
//! (allocated but never written), or
//! [`Unaddressable`](ShadowState::Unaddressable) (a heap redzone, a
//! freed block, or space below the current stack pointer). Whoever owns
//! allocation bookkeeping (the guest heap allocator, stack-pointer
//! tracking, etc.) calls [`ShadowMap::mark_valid`]/
//! [`ShadowMap::mark_uninit`]/[`ShadowMap::mark_unaddressable`] to keep
//! the shadow map in sync with what's actually live; [`crate::memory`]'s
//! access path (`read_u8`/`write_u8`/the multi-byte fast paths) consults
//! it on every access and calls [`ShadowMap::record`] when it finds
//! something wrong.
//!
//! # Why the default state is `Valid`, not `Unaddressable`
//!
//! It would seem safer for a byte nobody has explicitly claimed to be
//! flagged as suspect by default. In practice that's backwards for this
//! runtime: the loaded program image, the trap table, the fake
//! `ExecBase`/library bases, and every other host-written guest
//! structure are written directly through [`crate::memory::AddressSpace`]
//! *before* (and often entirely outside of) any allocation-tracking
//! codepath -- there is no "malloc call" moment for any of that. If the
//! default were `Unaddressable` or `Uninit`, every single one of those
//! writes and the guest's very first instruction fetch would report a
//! violation, drowning the one real bug a run might contain in
//! thousands of false positives. So the whole address space starts
//! `Valid`, and only regions someone *deliberately* poisons (heap
//! redzones around a real allocation, a block that's been freed, or the
//! guard region below the guest stack pointer) are ever anything else.
//! This means [`ShadowMap`] can only ever catch violations against
//! *tracked* allocations -- it's a detector for the bugs the tracking
//! layer knows to poison around, not a general "was this byte ever
//! written" prover.
//!
//! # Why violations, not `Result`s or panics
//!
//! [`crate::memory::AddressSpace::read_u8`]/`write_u8` have no error
//! channel (see that trait's own "total, never-fails" doc) and no idea
//! what guest instruction is making the access -- threading a
//! `Result` through every access on the hottest path in the whole
//! emulator, or panicking the whole process the first time a guest
//! bug trips a redzone, would both make the detector far less useful
//! than a single run that finishes and reports *everything* it saw
//! wrong. So violations are appended to a capped, deduplicated log
//! instead (see [`Violation`]/[`ShadowMap::record`]/[`ShadowMap::report`]).
//!
//! # PC attribution
//!
//! The access path itself has no way to know which guest instruction
//! (or host-side library call) is responsible for the access it's
//! checking -- that context lives in the CPU run loop and the library
//! dispatcher, several calls up the stack. Rather than thread a PC
//! parameter through every `AddressSpace` method (which would break the
//! trait for every other implementor and caller), [`ShadowMap::
//! set_current_pc`] lets the run loop publish "this is the PC I'm about
//! to execute" once per step/batch, and [`ShadowMap::record`] just
//! stamps whatever was last published. This means a violation caused by
//! a *host-side* library handler (e.g. a syscall implementation writing
//! a guest output buffer) is attributed to the PC of the `TRAP`/A-line
//! instruction that invoked the library call -- which is exactly the
//! useful thing to report, since that's the guest instruction a
//! developer would actually want to look at.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;

/// The maximum number of distinct violations [`ShadowMap`] will retain.
/// Once the log reaches this size, further distinct violations are
/// merely counted (see [`ShadowMap::suppressed_count`]) rather than
/// stored, so a guest bug that trips a fresh, never-before-seen
/// violation on every iteration of a tight loop can't exhaust host
/// memory building an unbounded log.
pub const MAX_VIOLATIONS: usize = 1000;

/// The state of one guest byte in a [`ShadowMap`].
///
/// Explicit `u8` reprs so the shadow map's backing storage is a plain,
/// cheap `Vec<u8>` (one byte of overhead per guest byte) rather than a
/// larger enum representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ShadowState {
    /// Addressable and holds a meaningful value -- either explicitly
    /// marked so, or (the default for the whole map -- see this
    /// module's doc) never touched by allocation tracking at all.
    Valid = 0,
    /// Addressable (part of a live allocation) but never written since
    /// it became addressable -- reading it is suspect (see
    /// [`ViolationKind::UninitRead`]), writing it is fine and makes it
    /// [`Valid`](Self::Valid).
    Uninit = 1,
    /// Not currently addressable: a heap redzone, a freed block, or
    /// below the guest stack pointer. Any access is a violation; see
    /// [`PoisonReason`] for which of those it was.
    Unaddressable = 2,
}

/// Why a range of bytes was marked [`ShadowState::Unaddressable`],
/// carried purely for diagnostics -- [`ShadowMap`]'s own logic never
/// branches on this, it's just reported in [`Violation`]/[`ShadowMap::
/// report`] so a developer can tell a redzone overrun apart from a
/// use-after-free at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoisonReason {
    /// Guard bytes placed around a live heap allocation, to catch a
    /// buffer overrun/underrun.
    Redzone,
    /// A block that was allocated and has since been freed (a
    /// use-after-free access lands here).
    Freed,
    /// Padding inserted purely for alignment, never meant to be
    /// accessed by guest code.
    AlignmentSlack,
    /// Guest-stack-relative space below the current stack pointer --
    /// AmigaOS convention (like most stack-based ABIs) is that this
    /// region is unreserved and any access to it is a bug (a "stack
    /// underflow"/reading below A7).
    BelowStackPointer,
}

impl fmt::Display for PoisonReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PoisonReason::Redzone => "heap redzone",
            PoisonReason::Freed => "freed block",
            PoisonReason::AlignmentSlack => "alignment slack",
            PoisonReason::BelowStackPointer => "below stack pointer",
        })
    }
}

impl PoisonReason {
    /// All four variants -- used only by this module's own tests, to
    /// exercise the [`Self::ordinal`]/[`Self::from_ordinal`] round trip
    /// for every reason without hand-listing them a second time.
    #[cfg(test)]
    const ALL: [PoisonReason; 4] = [
        PoisonReason::Redzone,
        PoisonReason::Freed,
        PoisonReason::AlignmentSlack,
        PoisonReason::BelowStackPointer,
    ];

    /// This variant's ordinal, `0..4` -- the low bits of an
    /// [`ShadowState::Unaddressable`] shadow byte (see the module-level
    /// "shadow byte encoding" doc above [`ShadowMap`]).
    const fn ordinal(self) -> u8 {
        match self {
            PoisonReason::Redzone => 0,
            PoisonReason::Freed => 1,
            PoisonReason::AlignmentSlack => 2,
            PoisonReason::BelowStackPointer => 3,
        }
    }

    /// The variant for a given ordinal, or `None` if it's out of range.
    /// Only ordinals `0..4` are ever produced by [`Self::ordinal`], but a
    /// stray/corrupted shadow byte should decode to *something* sane
    /// rather than panic -- see [`ShadowMap::decode_byte`].
    const fn from_ordinal(ordinal: u8) -> Option<PoisonReason> {
        match ordinal {
            0 => Some(PoisonReason::Redzone),
            1 => Some(PoisonReason::Freed),
            2 => Some(PoisonReason::AlignmentSlack),
            3 => Some(PoisonReason::BelowStackPointer),
            _ => None,
        }
    }
}

/// What kind of bad access a [`Violation`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViolationKind {
    /// A read touched at least one [`ShadowState::Unaddressable`] byte.
    InvalidRead,
    /// A write touched at least one [`ShadowState::Unaddressable`] byte.
    /// The write still happens (see this module's "why violations, not
    /// panics" doc) -- this is a detector, not an enforcer.
    InvalidWrite,
    /// A read touched at least one [`ShadowState::Uninit`] byte, and
    /// [`ShadowMap::report_uninit`] is enabled. Off by default -- issue
    /// #65 scopes uninitialized-read detection as an optional later
    /// increment, since it needs every allocator/heap codepath to
    /// reliably mark freshly-allocated memory `Uninit` first, which
    /// this change doesn't attempt to audit for.
    UninitRead,
}

/// One distinct kind of bad access, deduplicated by `(pc, addr, kind)`
/// -- see [`ShadowMap::record`]. `hits` counts how many times this
/// exact combination was seen, so a violation inside a tight guest loop
/// is reported once with an honest count instead of flooding the log
/// with copies of the same finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The guest program counter of the instruction (or, for a
    /// host-side library handler's access, the library call site) that
    /// caused this access -- see this module's "PC attribution" doc.
    pub pc: u32,
    /// The guest address of the first byte of the access that
    /// triggered this violation.
    pub addr: u32,
    /// The size in bytes of that access (1, 2, or 4).
    pub size: u8,
    /// What kind of bad access this was.
    pub kind: ViolationKind,
    /// Why the byte was poisoned, if this was an
    /// [`ViolationKind::InvalidRead`]/[`ViolationKind::InvalidWrite`]
    /// against an [`ShadowState::Unaddressable`] byte. `None` for
    /// [`ViolationKind::UninitRead`] (there's no "poison reason" for
    /// merely-uninitialized memory).
    pub reason: Option<PoisonReason>,
    /// How many times this exact `(pc, addr, kind)` combination has
    /// been recorded.
    pub hits: u64,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let access = match self.kind {
            ViolationKind::InvalidRead => "invalid",
            ViolationKind::InvalidWrite => "invalid",
            ViolationKind::UninitRead => "uninitialized",
        };
        let verb = match self.kind {
            ViolationKind::InvalidRead | ViolationKind::UninitRead => "read",
            ViolationKind::InvalidWrite => "write",
        };
        write!(
            f,
            "{access} {size}-byte {verb} at {addr:#010x}",
            size = self.size,
            addr = self.addr,
        )?;
        if let Some(reason) = self.reason {
            write!(f, " ({reason})")?;
        }
        write!(f, " from PC {pc:#010x}", pc = self.pc)?;
        if self.hits > 1 {
            write!(f, " ({} hits)", self.hits)?;
        }
        Ok(())
    }
}

/// Key used to deduplicate violations -- see [`ShadowMap::record`].
type ViolationKey = (u32, u32, ViolationKind);

/// Shadow-byte encoding used by [`ShadowMap::bytes`].
///
/// This module originally stored `states: Vec<ShadowState>` alongside a
/// `reasons: HashMap<u32, PoisonReason>` recording *why* each
/// currently-`Unaddressable` byte was poisoned. That was fine for the
/// 32-byte heap redzones this detector shipped with -- a handful of
/// hash-map entries per allocation is noise -- but it doesn't scale to
/// the feature this module was always heading towards: poisoning the
/// entire below-stack-pointer region of guest memory (see
/// [`PoisonReason::BelowStackPointer`]) as
/// [`ShadowMap::mark_unaddressable`] is called over larger and larger
/// ranges. A `HashMap<u32, PoisonReason>` entry costs on the order of
/// 48 bytes of host memory (the u32 key, the enum value, and the open-
/// addressing/bucket overhead) *per poisoned guest byte* -- for a
/// 16 MiB guest with even a modest stack-guard region, that's millions
/// of entries and tens of megabytes of host RAM to store two bits of
/// real information, plus a hash of the address on every poison call
/// *and* every violation lookup on the hot access path.
///
/// So the reason is folded directly into the same `Vec<u8>` that already
/// stores one entry per guest byte (`states`, now `bytes`), at zero
/// extra memory cost and with plain array indexing instead of a hash
/// lookup:
///
/// - `0` ([`VALID_BYTE`]) -- [`ShadowState::Valid`].
/// - `1` ([`UNINIT_BYTE`]) -- [`ShadowState::Uninit`].
/// - `0x80 | ordinal` ([`UNADDRESSABLE_TAG`] bit set) --
///   [`ShadowState::Unaddressable`], with the low bits holding the
///   poisoned [`PoisonReason`]'s ordinal (see
///   [`PoisonReason::ordinal`]/[`PoisonReason::from_ordinal`]). The high
///   bit is what [`ShadowMap::decode_byte`] tests first; there are only
///   four reasons today, so the low bits never come close to colliding
///   with it, but tagging in the high bit rather than just using values
///   `2..6` makes that invariant self-evident at every call site instead
///   of relying on `PoisonReason` never growing past four variants.
///
/// [`ShadowMap::encode_unaddressable`]/[`ShadowMap::decode_byte`] are the
/// only places that know this layout; everything else in this module
/// (and all of its public API) talks in [`ShadowState`]/[`PoisonReason`]
/// as before.
const VALID_BYTE: u8 = 0;
const UNINIT_BYTE: u8 = 1;
const UNADDRESSABLE_TAG: u8 = 0x80;

/// A per-guest-byte shadow memory used to detect invalid/uninitialized
/// accesses. See this module's doc for the full design rationale.
///
/// # Interior mutability of the violation log
///
/// [`crate::memory::AddressSpace::read_u8`] (and this crate's
/// [`crate::memory::FlatMemory`] read fast paths) take `&self`, not
/// `&mut self` -- reading guest memory has never needed mutable access,
/// and changing that trait would ripple `&mut` requirements out into
/// every one of this crate's many call sites that currently only
/// borrow memory immutably to read it. So the violation log
/// (`violations`/`index`/`suppressed`) uses [`RefCell`]/[`Cell`] to let
/// [`Self::check_read`] record a violation through a shared reference;
/// `bytes` (the actual poison bookkeeping -- see the "shadow byte
/// encoding" doc above) doesn't need this, since every method that
/// mutates it ([`Self::mark_valid`] and friends, and the write path's
/// `Uninit` -> `Valid` promotion) already has `&mut self` (guest writes
/// go through `FlatMemory::write_u8`, which does take `&mut self`).
#[derive(Debug, Clone)]
pub struct ShadowMap {
    /// One encoded byte per guest byte, indexed by guest address -- see
    /// the "shadow byte encoding" doc above. Replaces the old
    /// `states: Vec<ShadowState>` + `reasons: HashMap<u32, PoisonReason>`
    /// pair; decode with [`Self::decode_byte`], encode with
    /// [`Self::encode_unaddressable`]/[`VALID_BYTE`]/[`UNINIT_BYTE`].
    bytes: Vec<u8>,
    /// Deduplicated violations seen so far, in first-seen order. Capped
    /// at [`MAX_VIOLATIONS`] distinct entries. `RefCell`-wrapped -- see
    /// this struct's doc.
    violations: RefCell<Vec<Violation>>,
    /// Maps `(pc, addr, kind)` to that violation's index in
    /// `violations`, so a repeat access bumps the existing entry's
    /// `hits` instead of appending a duplicate.
    index: RefCell<HashMap<ViolationKey, usize>>,
    /// How many violations were seen but not recorded because the log
    /// was already full (see [`Self::suppressed_count`]).
    suppressed: Cell<u64>,
    /// The most recent guest PC published via [`Self::set_current_pc`].
    /// Starts at `0` (a plausible-looking but essentially meaningless
    /// value) until the run loop publishes a real one -- see this
    /// module's "PC attribution" doc.
    current_pc: u32,
    /// Whether [`ViolationKind::UninitRead`] is reported at all. Off by
    /// default -- see [`ViolationKind::UninitRead`]'s doc.
    pub report_uninit: bool,
}

impl ShadowMap {
    /// Creates a new shadow map covering `len` guest bytes, all
    /// initially [`ShadowState::Valid`] (see this module's doc for why).
    pub fn new(len: usize) -> Self {
        Self {
            bytes: vec![VALID_BYTE; len],
            violations: RefCell::new(Vec::new()),
            index: RefCell::new(HashMap::new()),
            suppressed: Cell::new(0),
            current_pc: 0,
            report_uninit: false,
        }
    }

    /// The number of guest bytes this map covers.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns `true` if this map covers zero bytes.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Encodes `reason` as an [`ShadowState::Unaddressable`] shadow byte
    /// -- see the "shadow byte encoding" doc above [`ShadowMap`].
    fn encode_unaddressable(reason: PoisonReason) -> u8 {
        UNADDRESSABLE_TAG | reason.ordinal()
    }

    /// Decodes one raw shadow byte into its [`ShadowState`] and (for
    /// [`ShadowState::Unaddressable`]) [`PoisonReason`] -- the inverse of
    /// [`VALID_BYTE`]/[`UNINIT_BYTE`]/[`Self::encode_unaddressable`].
    ///
    /// An [`UNADDRESSABLE_TAG`]-tagged byte whose low bits don't decode
    /// to a known [`PoisonReason`] can't actually occur (every write to
    /// `bytes` goes through [`Self::encode_unaddressable`] with a real
    /// `PoisonReason`), but falling back to `Redzone` rather than
    /// panicking or `unwrap`-ing keeps this decode total, matching this
    /// crate's general "out-of-range/unexpected is handled, not a panic"
    /// convention (see [`crate::memory::AddressSpace`]'s doc).
    fn decode_byte(byte: u8) -> (ShadowState, Option<PoisonReason>) {
        if byte & UNADDRESSABLE_TAG != 0 {
            let reason = PoisonReason::from_ordinal(byte & !UNADDRESSABLE_TAG)
                .unwrap_or(PoisonReason::Redzone);
            (ShadowState::Unaddressable, Some(reason))
        } else if byte == UNINIT_BYTE {
            (ShadowState::Uninit, None)
        } else {
            (ShadowState::Valid, None)
        }
    }

    /// Clamps `[addr, addr + len)` to `[0, self.len())`, returning
    /// `None` if the range is entirely out of bounds (or `len == 0`).
    /// Shared by every `mark_*` method so none of them can panic on an
    /// out-of-range or overflowing request -- see this crate's general
    /// "out-of-range is total, not an error" convention (`crate::
    /// memory::AddressSpace`'s doc).
    fn clamp_range(&self, addr: u32, len: u32) -> Option<std::ops::Range<usize>> {
        if len == 0 {
            return None;
        }
        let start = addr as usize;
        if start >= self.bytes.len() {
            return None;
        }
        let end = addr
            .checked_add(len)
            .map_or(self.bytes.len(), |e| e as usize);
        let end = end.min(self.bytes.len());
        if end <= start { None } else { Some(start..end) }
    }

    /// Marks `[addr, addr + len)` [`ShadowState::Valid`] (addressable,
    /// initialized), clearing any [`PoisonReason`] over that range.
    /// Used both to bring a freshly-allocated block into service and
    /// (by the access path itself) to clear [`ShadowState::Uninit`] the
    /// moment guest or host code writes to it -- see
    /// [`crate::memory`]'s write path.
    pub fn mark_valid(&mut self, addr: u32, len: u32) {
        let Some(range) = self.clamp_range(addr, len) else {
            return;
        };
        self.bytes[range].fill(VALID_BYTE);
    }

    /// Marks `[addr, addr + len)` [`ShadowState::Uninit`] (addressable,
    /// but not yet written) -- typically called right after a heap
    /// allocator hands out a fresh block, before the guest has had a
    /// chance to initialize it.
    pub fn mark_uninit(&mut self, addr: u32, len: u32) {
        let Some(range) = self.clamp_range(addr, len) else {
            return;
        };
        self.bytes[range].fill(UNINIT_BYTE);
    }

    /// Marks `[addr, addr + len)` [`ShadowState::Unaddressable`] for
    /// `reason` -- a redzone, a freed block, alignment slack, or space
    /// below the stack pointer. Any subsequent access to these bytes
    /// (until the range is next `mark_valid`/`mark_uninit`'d) is a
    /// violation.
    pub fn mark_unaddressable(&mut self, addr: u32, len: u32, reason: PoisonReason) {
        let Some(range) = self.clamp_range(addr, len) else {
            return;
        };
        self.bytes[range].fill(Self::encode_unaddressable(reason));
    }

    /// The current [`ShadowState`] of `addr`, or [`ShadowState::Valid`]
    /// if `addr` is out of range -- out-of-range guest addresses are
    /// already handled (as reading `0`/dropping the write) by
    /// [`crate::memory::AddressSpace`] itself, so the shadow map has
    /// nothing useful to add there.
    pub fn state(&self, addr: u32) -> ShadowState {
        self.bytes
            .get(addr as usize)
            .map_or(ShadowState::Valid, |&b| Self::decode_byte(b).0)
    }

    /// The [`PoisonReason`] `addr` was poisoned for, or `None` if `addr`
    /// is out of range or not currently [`ShadowState::Unaddressable`].
    /// Exposed publicly (not just used internally by
    /// [`Self::check_read_byte`]/[`Self::check_write_byte`]) since a
    /// caller writing its own diagnostics -- e.g. dumping shadow-map
    /// state around a crash -- wants the same "why is this poisoned"
    /// answer the violation log already reports.
    pub fn poison_reason(&self, addr: u32) -> Option<PoisonReason> {
        self.bytes
            .get(addr as usize)
            .and_then(|&b| Self::decode_byte(b).1)
    }

    /// Publishes `pc` as the guest program counter responsible for
    /// accesses from now until the next call -- see this module's "PC
    /// attribution" doc. Called once per instruction/batch by the CPU
    /// run loop.
    pub fn set_current_pc(&mut self, pc: u32) {
        self.current_pc = pc;
    }

    /// The most recently published PC -- see [`Self::set_current_pc`].
    pub fn current_pc(&self) -> u32 {
        self.current_pc
    }

    /// How many violations were seen after the log reached
    /// [`MAX_VIOLATIONS`] distinct entries, and so were counted but not
    /// stored.
    pub fn suppressed_count(&self) -> u64 {
        self.suppressed.get()
    }

    /// The distinct (deduplicated) violations recorded so far, in
    /// first-seen order. Returns a snapshot (`Vec` clone) rather than a
    /// borrowed slice since the log lives behind a [`RefCell`] (see this
    /// struct's doc) -- fine for a report printed once, after a run
    /// finishes, not a hot-path call.
    pub fn violations(&self) -> Vec<Violation> {
        self.violations.borrow().clone()
    }

    /// Total number of violation events recorded, counting repeats
    /// (i.e. summing every [`Violation::hits`]) but not suppressed
    /// ones. `0` means a clean run.
    pub fn violation_count(&self) -> u64 {
        self.violations.borrow().iter().map(|v| v.hits).sum()
    }

    /// Records one bad access at `addr` of `size` bytes, kind `kind`,
    /// poisoned for `reason` (`None` for [`ViolationKind::UninitRead`]).
    /// Deduplicates by `(pc, addr, kind)` -- using [`Self::current_pc`]
    /// as the PC -- bumping [`Violation::hits`] on a repeat rather than
    /// appending a new entry, and does nothing once the log already
    /// holds [`MAX_VIOLATIONS`] distinct entries and this would be a
    /// brand new one (repeats of an already-logged violation still bump
    /// their existing `hits` even after the cap is hit, since that's
    /// O(1) and doesn't grow the log). Takes `&self` -- see this
    /// struct's "interior mutability" doc.
    fn record(&self, addr: u32, size: u8, kind: ViolationKind, reason: Option<PoisonReason>) {
        let pc = self.current_pc;
        let key: ViolationKey = (pc, addr, kind);
        let mut index = self.index.borrow_mut();
        let mut violations = self.violations.borrow_mut();
        if let Some(&i) = index.get(&key) {
            violations[i].hits += 1;
            return;
        }
        if violations.len() >= MAX_VIOLATIONS {
            self.suppressed.set(self.suppressed.get() + 1);
            return;
        }
        let i = violations.len();
        violations.push(Violation {
            pc,
            addr,
            size,
            kind,
            reason,
            hits: 1,
        });
        index.insert(key, i);
    }

    /// Checks one byte about to be read at `addr`, recording an
    /// [`ViolationKind::InvalidRead`] if it's [`ShadowState::
    /// Unaddressable`], or an [`ViolationKind::UninitRead`] if it's
    /// [`ShadowState::Uninit`] and [`Self::report_uninit`] is on.
    /// `size` is the size in bytes of the whole access this byte is
    /// part of (carried through purely for the [`Violation`]'s own
    /// `size` field/diagnostics, not used to decide anything here).
    fn check_read_byte(&self, addr: u32, size: u8) {
        match self.state(addr) {
            ShadowState::Unaddressable => {
                let reason = self.poison_reason(addr);
                self.record(addr, size, ViolationKind::InvalidRead, reason);
            }
            ShadowState::Uninit if self.report_uninit => {
                self.record(addr, size, ViolationKind::UninitRead, None);
            }
            ShadowState::Uninit | ShadowState::Valid => {}
        }
    }

    /// Checks and updates one byte about to be written at `addr`,
    /// recording an [`ViolationKind::InvalidWrite`] if it's
    /// [`ShadowState::Unaddressable`] (the write still proceeds -- see
    /// this module's "why violations, not panics" doc), or promoting it
    /// to [`ShadowState::Valid`] if it was [`ShadowState::Uninit`] --
    /// this is what gives free syscall-buffer checking, since host-side
    /// library handlers write guest output buffers through this same
    /// path. `size` is carried through the same way as
    /// [`Self::check_read_byte`]'s.
    fn check_write_byte(&mut self, addr: u32, size: u8) {
        match self.state(addr) {
            ShadowState::Unaddressable => {
                let reason = self.poison_reason(addr);
                self.record(addr, size, ViolationKind::InvalidWrite, reason);
            }
            ShadowState::Uninit => {
                if let Some(slot) = self.bytes.get_mut(addr as usize) {
                    *slot = VALID_BYTE;
                }
            }
            ShadowState::Valid => {}
        }
    }

    /// Checks every byte of a `size`-byte access starting at `addr`
    /// before it happens, for a read. A 2- or 4-byte access straddling
    /// into a poisoned range is caught even if its first byte is fine
    /// -- see this module's doc on multi-byte accesses. Takes `&self`
    /// -- see this struct's "interior mutability" doc, and
    /// [`crate::memory::AddressSpace::read_u8`]'s signature this
    /// ultimately serves.
    pub(crate) fn check_read(&self, addr: u32, size: u8) {
        for i in 0..u32::from(size) {
            self.check_read_byte(addr.wrapping_add(i), size);
        }
    }

    /// Checks and updates every byte of a `size`-byte access starting
    /// at `addr`, for a write.
    pub(crate) fn check_write(&mut self, addr: u32, size: u8) {
        for i in 0..u32::from(size) {
            self.check_write_byte(addr.wrapping_add(i), size);
        }
    }

    /// A human-readable multi-line report of every recorded violation,
    /// suitable for printing to stderr after a `--sanitize` run. Empty
    /// when [`Self::violation_count`] is `0`.
    pub fn report(&self) -> String {
        use std::fmt::Write as _;

        let violations = self.violations.borrow();
        let mut out = String::new();
        if violations.is_empty() {
            return out;
        }
        let _ = writeln!(
            out,
            "sanitizer: {} distinct violation(s):",
            violations.len()
        );
        for v in violations.iter() {
            let _ = writeln!(out, "  {v}");
        }
        let suppressed = self.suppressed.get();
        if suppressed > 0 {
            let _ = writeln!(
                out,
                "  ... and {suppressed} further violation(s) suppressed (log cap {MAX_VIOLATIONS})"
            );
        }
        out
    }
}

impl fmt::Display for ShadowMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.report())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_valid_and_a_clean_run_reports_nothing() {
        let mut shadow = ShadowMap::new(16);
        shadow.check_read(0, 1);
        shadow.check_write(4, 4);
        shadow.check_read(15, 1);
        assert_eq!(shadow.violation_count(), 0);
        assert!(shadow.violations().is_empty());
        assert_eq!(shadow.report(), "");
    }

    #[test]
    fn redzone_read_is_a_violation_with_the_right_reason() {
        let mut shadow = ShadowMap::new(16);
        shadow.mark_unaddressable(8, 4, PoisonReason::Redzone);

        shadow.set_current_pc(0x1000);
        shadow.check_read(8, 1);

        assert_eq!(shadow.violation_count(), 1);
        let v = &shadow.violations()[0];
        assert_eq!(v.kind, ViolationKind::InvalidRead);
        assert_eq!(v.addr, 8);
        assert_eq!(v.pc, 0x1000);
        assert_eq!(v.reason, Some(PoisonReason::Redzone));
    }

    #[test]
    fn redzone_write_is_a_violation_and_still_a_write_would_proceed() {
        // check_write itself doesn't perform the underlying write (that's
        // memory.rs's job) -- this just verifies it flags the violation
        // and leaves the poisoned state alone (a write to unaddressable
        // memory doesn't heal it).
        let mut shadow = ShadowMap::new(16);
        shadow.mark_unaddressable(8, 4, PoisonReason::Freed);

        shadow.check_write(8, 1);

        assert_eq!(shadow.violation_count(), 1);
        assert_eq!(shadow.violations()[0].kind, ViolationKind::InvalidWrite);
        assert_eq!(shadow.state(8), ShadowState::Unaddressable);
    }

    #[test]
    fn four_byte_read_straddling_a_redzone_is_a_violation() {
        let mut shadow = ShadowMap::new(16);
        // Bytes 0..8 valid, 8..16 a redzone.
        shadow.mark_unaddressable(8, 8, PoisonReason::Redzone);

        // A 4-byte read at address 6 covers bytes 6,7 (valid) and 8,9
        // (redzone) -- straddling in.
        shadow.check_read(6, 4);

        assert_eq!(shadow.violation_count(), 2, "bytes 8 and 9 each flag once");
    }

    #[test]
    fn dedup_by_pc_addr_kind_counts_hits_instead_of_growing_the_log() {
        let mut shadow = ShadowMap::new(16);
        shadow.mark_unaddressable(4, 1, PoisonReason::Redzone);
        shadow.set_current_pc(0x2000);

        for _ in 0..5 {
            shadow.check_read(4, 1);
        }

        assert_eq!(shadow.violations().len(), 1);
        assert_eq!(shadow.violations()[0].hits, 5);
        assert_eq!(shadow.violation_count(), 5);
    }

    #[test]
    fn different_pc_for_the_same_address_is_a_separate_violation() {
        let mut shadow = ShadowMap::new(16);
        shadow.mark_unaddressable(4, 1, PoisonReason::Redzone);

        shadow.set_current_pc(0x2000);
        shadow.check_read(4, 1);
        shadow.set_current_pc(0x3000);
        shadow.check_read(4, 1);

        assert_eq!(shadow.violations().len(), 2);
    }

    #[test]
    fn log_cap_suppresses_further_distinct_violations_but_keeps_counting() {
        let mut shadow = ShadowMap::new(MAX_VIOLATIONS + 10);
        for i in 0..(MAX_VIOLATIONS + 10) as u32 {
            shadow.mark_unaddressable(i, 1, PoisonReason::Redzone);
        }

        for i in 0..(MAX_VIOLATIONS + 10) as u32 {
            shadow.set_current_pc(i); // distinct pc -> distinct violation each time
            shadow.check_read(i, 1);
        }

        assert_eq!(shadow.violations().len(), MAX_VIOLATIONS);
        assert_eq!(shadow.suppressed_count(), 10);
    }

    #[test]
    fn write_to_uninit_marks_it_valid() {
        let mut shadow = ShadowMap::new(16);
        shadow.mark_uninit(0, 4);
        assert_eq!(shadow.state(0), ShadowState::Uninit);

        shadow.check_write(0, 4);

        assert_eq!(shadow.state(0), ShadowState::Valid);
        assert_eq!(shadow.state(3), ShadowState::Valid);
        // Writing to Uninit is not itself a violation.
        assert_eq!(shadow.violation_count(), 0);
    }

    #[test]
    fn uninit_read_is_silent_by_default_but_reported_when_enabled() {
        let mut shadow = ShadowMap::new(16);
        shadow.mark_uninit(0, 4);

        shadow.check_read(0, 1);
        assert_eq!(shadow.violation_count(), 0, "off by default");

        shadow.report_uninit = true;
        shadow.check_read(0, 1);
        assert_eq!(shadow.violation_count(), 1);
        assert_eq!(shadow.violations()[0].kind, ViolationKind::UninitRead);
    }

    #[test]
    fn mark_valid_clears_a_previous_poison_reason() {
        let mut shadow = ShadowMap::new(16);
        shadow.mark_unaddressable(0, 4, PoisonReason::Freed);
        shadow.mark_valid(0, 4);

        shadow.check_read(0, 1);

        assert_eq!(shadow.violation_count(), 0);
        assert_eq!(shadow.state(0), ShadowState::Valid);
    }

    #[test]
    fn mark_ranges_clamp_to_the_map_length_without_panicking() {
        let mut shadow = ShadowMap::new(8);
        shadow.mark_unaddressable(4, 100, PoisonReason::Redzone);
        assert_eq!(shadow.state(4), ShadowState::Unaddressable);
        assert_eq!(shadow.state(7), ShadowState::Unaddressable);

        // Entirely out of range: no panic, no effect.
        shadow.mark_unaddressable(1000, 4, PoisonReason::Redzone);
        shadow.mark_uninit(1000, 4);
        shadow.mark_valid(1000, 4);

        // addr overflowing u32 when added to len: no panic.
        shadow.mark_unaddressable(u32::MAX - 2, 10, PoisonReason::Redzone);
    }

    #[test]
    fn poison_reason_round_trips_every_variant() {
        for reason in PoisonReason::ALL {
            let mut shadow = ShadowMap::new(16);
            shadow.mark_unaddressable(4, 1, reason);
            assert_eq!(shadow.state(4), ShadowState::Unaddressable);
            assert_eq!(shadow.poison_reason(4), Some(reason));
        }
    }

    #[test]
    fn poison_reason_is_none_for_valid_uninit_and_out_of_range() {
        let mut shadow = ShadowMap::new(16);
        assert_eq!(shadow.poison_reason(0), None, "default Valid");

        shadow.mark_uninit(0, 4);
        assert_eq!(shadow.poison_reason(0), None, "Uninit has no reason");

        assert_eq!(shadow.poison_reason(1000), None, "out of range");
    }

    #[test]
    fn mark_valid_and_mark_uninit_clear_a_previously_set_reason() {
        let mut shadow = ShadowMap::new(16);
        shadow.mark_unaddressable(0, 4, PoisonReason::Freed);
        assert_eq!(shadow.poison_reason(0), Some(PoisonReason::Freed));

        shadow.mark_valid(0, 4);
        assert_eq!(
            shadow.poison_reason(0),
            None,
            "mark_valid must clear the old reason, not leave it stale \
             behind a Valid state"
        );

        shadow.mark_unaddressable(0, 4, PoisonReason::Redzone);
        assert_eq!(shadow.poison_reason(0), Some(PoisonReason::Redzone));

        shadow.mark_uninit(0, 4);
        assert_eq!(
            shadow.poison_reason(0),
            None,
            "mark_uninit must clear the old reason, not leave it stale \
             behind an Uninit state"
        );
    }

    #[test]
    fn a_large_poisoned_range_is_marked_and_queried_without_issue() {
        // This is the case that motivated collapsing `states` +
        // `reasons: HashMap<u32, PoisonReason>` into one Vec<u8>: a
        // below-stack-pointer guard region can be megabytes, and a
        // per-byte HashMap entry for each one would be a scalability
        // landmine.
        const ONE_MIB: u32 = 1024 * 1024;
        let mut shadow = ShadowMap::new((4 * ONE_MIB) as usize);

        shadow.mark_unaddressable(ONE_MIB, ONE_MIB, PoisonReason::BelowStackPointer);

        assert_eq!(shadow.state(ONE_MIB - 1), ShadowState::Valid);
        assert_eq!(shadow.state(ONE_MIB), ShadowState::Unaddressable);
        assert_eq!(
            shadow.poison_reason(ONE_MIB),
            Some(PoisonReason::BelowStackPointer)
        );
        assert_eq!(
            shadow.state(ONE_MIB + ONE_MIB / 2),
            ShadowState::Unaddressable
        );
        assert_eq!(shadow.state(2 * ONE_MIB - 1), ShadowState::Unaddressable);
        assert_eq!(shadow.state(2 * ONE_MIB), ShadowState::Valid);

        shadow.set_current_pc(0x4000);
        shadow.check_read(ONE_MIB + 42, 1);
        assert_eq!(shadow.violation_count(), 1);
        assert_eq!(
            shadow.violations()[0].reason,
            Some(PoisonReason::BelowStackPointer)
        );
    }

    #[test]
    fn report_renders_pc_kind_size_address_and_reason() {
        let mut shadow = ShadowMap::new(0x20);
        shadow.mark_unaddressable(0x10, 2, PoisonReason::Redzone);
        shadow.set_current_pc(0x3a1c);
        shadow.check_read(0x10, 2);

        let report = shadow.report();
        assert!(report.contains("invalid 2-byte read"));
        assert!(report.contains("0x00000010"));
        assert!(report.contains("heap redzone"));
        assert!(report.contains("0x00003a1c"));
    }
}
