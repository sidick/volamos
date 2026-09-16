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

//! # Stack-pointer tracking (increment 2)
//!
//! Everything below the guest stack pointer is dead: a real Amiga has no
//! guard page there either, but AmigaOS convention (like every m68k
//! stack-based ABI) treats it as unreserved, so a read or write there
//! means a program is reusing a frame it already released, or has
//! overrun its stack downward. [`ShadowMap::begin_stack_tracking`]/
//! [`ShadowMap::update_stack_pointer`] keep `[stack_base, sp)` poisoned
//! as [`PoisonReason::BelowStackPointer`] as the guest runs.
//!
//! The update is deliberately **incremental**: it is called once per
//! instruction by the run loop, so re-poisoning the whole tracked region
//! on every call (as a naive "poison `[stack_base, sp)` from scratch"
//! implementation would) turns an O(1) per-instruction operation into an
//! O(stack size) one, which would make `--sanitize` unusably slow on any
//! program that isn't trivial. Instead only the delta between the last
//! seen SP and the new one is touched: growth (`sp` decreased) marks the
//! newly-used bytes [`ShadowState::Uninit`] rather than
//! [`ShadowState::Valid`] -- this is deliberate, not an oversight, since
//! it means a later increment gets uninitialized-stack-read detection
//! for free, and it's harmless today because [`ShadowMap::report_uninit`]
//! defaults to off. Shrinkage (`sp` increased) marks the newly-released
//! bytes [`PoisonReason::BelowStackPointer`]. An unchanged SP -- by far
//! the most common case, since most instructions don't touch A7 at all
//! -- does no work beyond the one comparison that detects this.
//!
//! The tricky part is that the SP can legitimately leave the tracked
//! region entirely: `exec.library`'s `StackSwap` (see `exectask.rs`)
//! hands a task a completely different stack with its own
//! `tc_SPLower`/`tc_SPUpper` bounds, and supervisor mode runs on a
//! separate supervisor stack pointer. Neither of those is a bug, so
//! when a new SP falls outside `[stack_base, stack_top]`,
//! [`ShadowMap::update_stack_pointer`] stops tracking -- it doesn't
//! poison some huge bogus range trying to reach the new SP, and it
//! doesn't panic.
//!
//! It used to *also* not report or clean up anything at all, on the
//! theory that "no claims" is strictly better than "wrong claims" while
//! we have no reliable way to know what happens to the abandoned
//! region. That theory was only half right: doing nothing does avoid
//! inventing new claims, but it leaves the *old* ones -- the
//! [`PoisonReason::BelowStackPointer`] poison already written over
//! `[stack_base, sp)` -- sitting in the shadow map indefinitely. That
//! memory doesn't stop existing just because tracking looked away, and
//! once some *other* stack happens to reuse the same guest addresses
//! (which a heap-allocated `StackSwap` stack routinely does, since it
//! comes from the same `AllocMem` pool as everything else), every
//! ordinary access to it reports a violation against poison that no
//! longer describes anything real. This is exactly what running the
//! real SAS/C `sc` compiler under `--sanitize` hit: `sc` calls
//! `StackSwap` onto its own larger heap-allocated stack and back, and
//! the moment it did, plain reads and writes on that heap-allocated
//! stack came back as "invalid access (below stack pointer)" -- stale
//! poison from the *original* stack, landmining an address it no longer
//! had anything to do with.
//!
//! So suspending now also **cleans up**: [`Self::update_stack_pointer`]
//! clears the stale poison back to [`ShadowState::Valid`] over the
//! *entire* previously-tracked region and empties the shadow call stack
//! (see the next section) before setting `sp` to `None`. Neither action
//! requires knowing anything about what will happen to that memory next
//! -- retracting a claim is always safe, unlike inventing one, so this
//! doesn't reintroduce the false-positive risk the original
//! do-nothing design was guarding against. Tracking resumes the next
//! time the SP is observed back inside `[stack_base, stack_top]`; unlike
//! the old behavior of silently adopting the new SP with **no shadow-map
//! mutation at all** (which left detection quietly off for the rest of
//! the run for any task that ever left its stack region even once),
//! resumption now re-poisons `[stack_base, sp)` exactly as
//! [`Self::begin_stack_tracking`] would, so a below-SP access after
//! resuming is caught again instead of staying invisible.
//!
//! `exec.library`'s `StackSwap` handler doesn't rely on this generic
//! suspend-then-resume path at all, though: `exectask.rs`'s
//! `stack_swap_handler` calls [`Self::reset_stack_tracking`] directly,
//! right after completing the swap, because a *known*, atomic stack
//! switch shouldn't have to wait for [`Self::update_stack_pointer`] to
//! eventually notice the SP is somewhere new on some later instruction
//! -- see that method's own doc for why the two paths, despite ending
//! up in a similar place, are worth keeping separate.
//!
//! Every range this machinery ever poisons is a subset of `[stack_base,
//! stack_top]` by construction (both the old and new SP are checked to
//! lie in that range before a delta is computed at all), and
//! [`ShadowMap::mark_uninit`]/[`ShadowMap::mark_unaddressable`] clamp to
//! the map's own bounds regardless (see [`ShadowMap::clamp_range`]), so
//! there's no path by which a stray delta -- however large -- can mark
//! an absurd range or panic.
//!
//! # The shadow call stack (increment 2)
//!
//! Heap redzones and stack-underflow detection catch corruption near
//! the *data* a guest program manipulates; they say nothing about a
//! guest bug that overwrites its own return address (a classic stack
//! buffer overflow). [`ShadowMap::record_call`]/[`ShadowMap::
//! check_return`] add exactly that check -- something even Valgrind's
//! memcheck doesn't offer, since it has no notion of "this stack slot
//! holds a return address" at all.
//!
//! The state (a small bounded stack of `(slot address, expected return
//! address)` pairs) lives directly on [`ShadowMap`] rather than in a
//! separate top-level type, for the same reason the shadow bytes and
//! the stack-pointer tracking do: the run loop already threads a
//! `&mut ShadowMap` through call/return handling for the stack-pointer
//! update, and a second free-standing type would just be more state for
//! `backend.rs`/`dispatch.rs` to carry around and keep in sync for no
//! benefit -- there's exactly one shadow-call-stack per guest run, same
//! as there's exactly one shadow byte map.
//!
//! **The reconciliation rule is the important part of this design, not
//! the happy path**, because a m68k program has several completely
//! legitimate ways to make a return address slot disappear without ever
//! executing a matching `RTS`:
//!
//! - `longjmp`-style non-local unwinding, which restores a saved SP and
//!   simply abandons every frame between it and the current one.
//! - A handler that manually pops its own return address off the stack
//!   (`addq.l #4,sp` before falling through, etc).
//! - volamos's own library-call mechanism: [`crate::dispatch`] resolves
//!   a `JSR`-to-a-library-vector itself and performs the `RTS` in host
//!   code -- it pops the return address the guest's `JSR` pushed and
//!   resumes there directly, without ever executing a real `RTS`
//!   instruction that could ask this module to check anything.
//!
//! So before every check, recorded frames are reconciled against the
//! *current* SP: any frame whose slot address is below the current SP
//! has, by definition, already had its stack space reclaimed (see the
//! stack-pointer tracking section above -- "below SP" is exactly the
//! region that tracking treats as dead), regardless of *how* that
//! happened, and is discarded without comment. Only after that
//! reconciliation does [`ShadowMap::check_return`] look at what's left:
//!
//! - If the top recorded frame's slot address is exactly the current
//!   SP, this `RTS` is popping that frame's return address, so the
//!   value actually found there is compared against what was recorded.
//!   A mismatch is genuine corruption -- something overwrote that stack
//!   slot after the call but before the return -- and is reported with
//!   both the expected and the actual address, deliberately the two
//!   most useful numbers such a report can contain.
//! - If no recorded frame's slot matches the current SP (including the
//!   case where there's no frame at all), **nothing is reported**. This
//!   is not a gap in coverage, it's required correctness: `move.l
//!   #target,-(sp)` followed by `rts` is a common, entirely legitimate
//!   m68k idiom for a computed jump that was never a "call" in the
//!   first place, and volamos itself arranges a return address at
//!   process startup that no `JSR` ever pushed -- so the very first
//!   `RTS` a guest program executes is *guaranteed* to have no matching
//!   frame. Reporting on a non-match would make every single guest
//!   program's first instruction sequence a false positive.
//!
//! There's one more way a recorded frame can become permanently
//! unreachable that this SP-based reconciliation can't catch on its
//! own, because it isn't a matter of the *current* SP moving past a
//! slot: `StackSwap` (see `exectask.rs`) hands the task a completely
//! different stack, so a frame recorded while running on the *old*
//! stack has a `slot_sp` that has nothing to do with the *new* stack's
//! address range at all. Reconciliation only ever compares recorded
//! slots against the current SP, so a stale cross-stack frame simply
//! sits in the log -- right up until the new stack happens to reuse the
//! very same guest address for one of its own calls (unremarkable for
//! two heap-allocated `StackSwap` stacks drawn from the same pool).
//! At that point [`Self::check_return`] finds a slot-address match and
//! compares the *new* stack's actual return address against the *old*
//! stack's recorded one -- a spurious [`ViolationKind::
//! ReturnAddressCorrupted`] for two calls that share nothing but a
//! coincidentally-reused address. This isn't hypothetical: it's exactly
//! what the real SAS/C `sc` compiler triggered under `--sanitize` (see
//! the "stack-pointer tracking" doc above for the matching below-SP
//! half of the same `StackSwap` bug). It's a different failure mode
//! from `fixtures/stacktest`'s `pushret` case above -- that one is
//! about *never* recording a frame for a computed jump in the first
//! place; this one is about a frame that *was* legitimately recorded,
//! then outlived the stack it was recorded on.
//!
//! [`Self::reset_stack_tracking`] closes this the same way it closes
//! the stale-poison half: it clears the shadow call stack entirely
//! whenever the task is known to have switched to a new stack, since no
//! frame recorded against an abandoned stack can ever be validly
//! returned to, and keeping it around risks exactly this collision for
//! no benefit.
//!
//! Recursion means the stack can grow without bound, and a host process
//! can't; [`MAX_CALL_STACK_DEPTH`] caps how many frames are retained,
//! discarding the oldest (outermost) frame once the cap is hit rather
//! than refusing the new one (the innermost, most-recently-made call is
//! the one most likely to be relevant to whatever the guest is doing
//! right now). Frames dropped this way are counted (see [`ShadowMap::
//! dropped_call_frames`]) rather than silently vanishing, so a report
//! can at least say "N frames were never checked" instead of implying a
//! false all-clear for arbitrarily deep recursion.
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
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
    /// [`ShadowMap::check_return`] found a stack slot it had previously
    /// recorded (via [`ShadowMap::record_call`]) a return address in,
    /// but the value now stored there doesn't match -- something wrote
    /// over the return address between the call and the return. See
    /// this module's "shadow call stack" doc for the reconciliation
    /// rule that keeps this from firing on legitimate non-`RTS`
    /// unwinding.
    ReturnAddressCorrupted,
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
    /// For [`ViolationKind::ReturnAddressCorrupted`] only: the return
    /// address that [`ShadowMap::record_call`] recorded for this slot.
    /// `None` for every other kind.
    pub expected_return_addr: Option<u32>,
    /// For [`ViolationKind::ReturnAddressCorrupted`] only: the value
    /// [`ShadowMap::check_return`] actually found in that slot. `None`
    /// for every other kind.
    pub actual_return_addr: Option<u32>,
    /// How many times this exact `(pc, addr, kind)` combination has
    /// been recorded.
    pub hits: u64,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.kind == ViolationKind::ReturnAddressCorrupted {
            // Deliberately not folded into the generic "N-byte
            // read/write at ADDR" phrasing below: this isn't an access
            // against a poisoned byte, it's a value mismatch, and the
            // two addresses being compared are the single most useful
            // thing such a report can print (see this module's "shadow
            // call stack" doc).
            write!(
                f,
                "return address corrupted at stack slot {addr:#010x}: expected {expected:#010x}, found {actual:#010x}",
                addr = self.addr,
                expected = self.expected_return_addr.unwrap_or(0),
                actual = self.actual_return_addr.unwrap_or(0),
            )?;
        } else {
            let access = match self.kind {
                ViolationKind::InvalidRead => "invalid",
                ViolationKind::InvalidWrite => "invalid",
                ViolationKind::UninitRead => "uninitialized",
                ViolationKind::ReturnAddressCorrupted => unreachable!(),
            };
            let verb = match self.kind {
                ViolationKind::InvalidRead | ViolationKind::UninitRead => "read",
                ViolationKind::InvalidWrite => "write",
                ViolationKind::ReturnAddressCorrupted => unreachable!(),
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

/// The tracked guest stack region and the last SP [`ShadowMap::
/// update_stack_pointer`] observed inside it -- see this module's
/// "stack-pointer tracking" doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StackRegion {
    /// Lowest tracked address -- the far end of the stack, reached only
    /// by a runaway overflow.
    base: u32,
    /// Highest tracked address (inclusive) -- where the stack started
    /// out empty, e.g. a task's `tc_SPUpper`.
    top: u32,
    /// The last SP seen while it was inside `[base, top]`, or `None` if
    /// the most recently observed SP was outside that range (tracking
    /// is suspended -- see [`ShadowMap::update_stack_pointer`]).
    sp: Option<u32>,
}

/// The maximum number of outstanding call frames [`ShadowMap`]'s shadow
/// call stack retains -- see this module's "shadow call stack" doc for
/// why the *oldest* frame is dropped once this is exceeded, and
/// [`ShadowMap::dropped_call_frames`] for how the drop is surfaced
/// rather than silently forgotten. Comfortably deeper than any
/// realistic non-pathological guest call depth, while still bounding
/// host memory against runaway/infinite guest recursion.
pub const MAX_CALL_STACK_DEPTH: usize = 4096;

/// How far below the tracked stack pointer an access is still forgiven,
/// in bytes -- see
/// [`ShadowMap::below_sp_violation_is_within_grace_band`] for the full
/// reasoning. Sized to cover the largest single-instruction stack push
/// on this CPU family (`movem.l` with all sixteen registers, 64 bytes);
/// anything deeper than one instruction's worth of push is a genuine
/// use of released or never-reserved stack.
pub const BELOW_SP_GRACE_BYTES: u32 = 64;

/// One outstanding call recorded by [`ShadowMap::record_call`] -- the
/// stack slot a return address was pushed to, and the value that was
/// pushed there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CallFrame {
    /// The guest address of the stack slot holding the return address
    /// (i.e. the SP immediately after the call's `JSR` pushed it).
    slot_sp: u32,
    /// The return address that was pushed there.
    return_addr: u32,
}

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
    /// The tracked stack region and last-seen SP, or `None` until
    /// [`Self::begin_stack_tracking`] is called -- see this module's
    /// "stack-pointer tracking" doc.
    stack: Option<StackRegion>,
    /// Outstanding call frames recorded by [`Self::record_call`],
    /// oldest first -- see this module's "shadow call stack" doc.
    /// Bounded at [`MAX_CALL_STACK_DEPTH`].
    call_stack: VecDeque<CallFrame>,
    /// How many call frames were discarded because [`Self::record_call`]
    /// was invoked while the stack was already at
    /// [`MAX_CALL_STACK_DEPTH`] -- see [`Self::dropped_call_frames`].
    dropped_call_frames: u64,
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
            stack: None,
            call_stack: VecDeque::new(),
            dropped_call_frames: 0,
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
        self.push_violation(
            key,
            Violation {
                pc,
                addr,
                size,
                kind,
                reason,
                expected_return_addr: None,
                actual_return_addr: None,
                hits: 1,
            },
        );
    }

    /// Records one [`ViolationKind::ReturnAddressCorrupted`] at stack
    /// slot `addr`, where `expected` was recorded by [`Self::
    /// record_call`] and `actual` is what [`Self::check_return`] found
    /// there instead. Shares dedup/cap bookkeeping with [`Self::record`]
    /// via [`Self::push_violation`], but builds a [`Violation`] with the
    /// `expected_return_addr`/`actual_return_addr` fields populated
    /// instead of `reason`, since this isn't an access against a
    /// poisoned byte.
    fn record_return_corruption(&self, addr: u32, expected: u32, actual: u32) {
        let pc = self.current_pc;
        let kind = ViolationKind::ReturnAddressCorrupted;
        let key: ViolationKey = (pc, addr, kind);
        self.push_violation(
            key,
            Violation {
                pc,
                addr,
                size: 4,
                kind,
                reason: None,
                expected_return_addr: Some(expected),
                actual_return_addr: Some(actual),
                hits: 1,
            },
        );
    }

    /// Shared dedup/cap logic for [`Self::record`]/[`Self::
    /// record_return_corruption`]: bumps an existing entry's `hits` on a
    /// repeat of `key`, otherwise appends `violation` unless the log is
    /// already at [`MAX_VIOLATIONS`], in which case the miss is only
    /// counted (see [`Self::suppressed_count`]).
    fn push_violation(&self, key: ViolationKey, violation: Violation) {
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
        violations.push(violation);
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
                if reason == Some(PoisonReason::BelowStackPointer)
                    && self.below_sp_violation_is_within_grace_band(addr)
                {
                    return;
                }
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
                if reason == Some(PoisonReason::BelowStackPointer)
                    && self.below_sp_violation_is_within_grace_band(addr)
                {
                    return;
                }
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

    /// Whether a below-stack-pointer poison at `addr` should be
    /// forgiven because it falls in the grace band just under the
    /// tracked stack pointer.
    ///
    /// This exists because of an ordering problem that is structural,
    /// not incidental: **every stack push writes below the current
    /// stack pointer by definition.** `move.l d0,-(sp)` decrements A7
    /// as part of the store, and `MOVEM`/`LINK`/`JSR` do the same, so
    /// the store lands at an address the shadow map still believes is
    /// below SP -- the run loop cannot republish the new SP until the
    /// instruction has finished. Poisoning strictly below the last-seen
    /// SP therefore reports a violation for every single subroutine
    /// call and register save a normal program makes; running
    /// `fixtures/hello` (which does nothing but one `PutStr`) produced
    /// exactly that before this band existed.
    ///
    /// The band is [`BELOW_SP_GRACE_BYTES`] wide, which comfortably
    /// covers the largest push any one instruction can perform
    /// (`movem.l d0-d7/a0-a7,-(sp)` moves 64 bytes). Accesses further
    /// below SP than that are still reported, which is the bug class
    /// worth catching: a released stack frame being read after the fact,
    /// or a wild pointer well past the live stack. valgrind takes the
    /// same approach for the same reason.
    ///
    /// The check is `O(1)` and only runs once a byte has *already* been
    /// found poisoned, so it costs nothing on the hot path.
    fn below_sp_violation_is_within_grace_band(&self, addr: u32) -> bool {
        let Some(region) = self.stack else {
            return false;
        };
        let Some(sp) = region.sp else {
            return false;
        };
        addr < sp && sp - addr <= BELOW_SP_GRACE_BYTES
    }

    /// Checks every byte of a `size`-byte access starting at `addr`
    /// before it happens, for a read. A 2- or 4-byte access straddling
    /// into a poisoned range is caught even if its first byte is fine
    /// -- see this module's doc on multi-byte accesses. Takes `&self`
    /// -- see this struct's "interior mutability" doc, and
    /// [`crate::memory::AddressSpace::read_u8`]'s signature this
    /// ultimately serves.
    pub(crate) fn check_read(&self, addr: u32, size: u8) {
        // Stop at the first byte that reports. A 4-byte access running
        // into a redzone would otherwise log four violations at four
        // consecutive addresses -- they all describe one access and one
        // bug, and the dedup key is (pc, addr, kind), so the per-byte
        // addresses defeat deduplication precisely when it is most
        // wanted. Reporting the first offending byte keeps the address
        // in the message the one a reader can act on.
        for i in 0..u32::from(size) {
            let before = self.violation_count();
            self.check_read_byte(addr.wrapping_add(i), size);
            if self.violation_count() != before {
                return;
            }
        }
    }

    /// Checks and updates every byte of a `size`-byte access starting
    /// at `addr`, for a write.
    pub(crate) fn check_write(&mut self, addr: u32, size: u8) {
        // As `check_read`: report at most once per access. Every byte is
        // still *visited*, because a write has to heal each `Uninit`
        // byte it covers even after one of them has reported.
        let mut reported = false;
        for i in 0..u32::from(size) {
            let before = self.violation_count();
            if reported {
                self.heal_uninit_byte(addr.wrapping_add(i));
            } else {
                self.check_write_byte(addr.wrapping_add(i), size);
                reported = self.violation_count() != before;
            }
        }
    }

    /// Promotes a single `Uninit` byte to `Valid` without any checking
    /// -- the tail of a write whose violation has already been
    /// reported. See [`Self::check_write`].
    fn heal_uninit_byte(&mut self, addr: u32) {
        if self.state(addr) == ShadowState::Uninit
            && let Some(slot) = self.bytes.get_mut(addr as usize)
        {
            *slot = VALID_BYTE;
        }
    }

    /// Begins tracking the guest stack, poisoning `[stack_base, sp)` as
    /// [`PoisonReason::BelowStackPointer`] (the region below the
    /// initial SP is exactly as dead as any subsequent access below a
    /// later SP -- see this module's "stack-pointer tracking" doc).
    /// `stack_base`/`stack_top` are remembered for every future call to
    /// [`Self::update_stack_pointer`], which needs them both to compute
    /// deltas and to detect the SP leaving the region entirely.
    ///
    /// If `sp` itself is outside `[stack_base, stack_top]`, no
    /// poisoning happens and tracking starts in the same "suspended"
    /// state [`Self::update_stack_pointer`] would put it in for any
    /// other out-of-region SP -- there's nothing unusual about a task
    /// being constructed with its stack pointer not yet inside what
    /// will become its tracked region.
    pub fn begin_stack_tracking(&mut self, stack_base: u32, stack_top: u32, sp: u32) {
        let in_region = stack_base <= sp && sp <= stack_top;
        if in_region {
            self.poison_below_sp(stack_base, sp);
        }
        self.stack = Some(StackRegion {
            base: stack_base,
            top: stack_top,
            sp: in_region.then_some(sp),
        });
    }

    /// Marks `[stack_base, sp)` [`PoisonReason::BelowStackPointer`] --
    /// the poisoning half shared by [`Self::begin_stack_tracking`] and
    /// every place that re-establishes a baseline (a resume in
    /// [`Self::update_stack_pointer`], or a fresh region in
    /// [`Self::reset_stack_tracking`]). A no-op if `sp <= stack_base`
    /// (an empty stack has nothing below its pointer yet to poison).
    fn poison_below_sp(&mut self, stack_base: u32, sp: u32) {
        if sp > stack_base {
            self.mark_unaddressable(stack_base, sp - stack_base, PoisonReason::BelowStackPointer);
        }
    }

    /// Clears every byte of `[base, top]` (inclusive, as
    /// [`StackRegion::top`] is stored) back to [`ShadowState::Valid`],
    /// wiping out any [`PoisonReason::BelowStackPointer`] poison left
    /// over from tracking that region. Used when tracking stops caring
    /// about a stack region (a `StackSwap`-driven
    /// [`Self::reset_stack_tracking`], or the generic suspend path in
    /// [`Self::update_stack_pointer`]) -- see this module's "stack-
    /// pointer tracking" doc for why leaving stale poison behind is
    /// worse than having no claim on the region at all.
    ///
    /// `top.saturating_sub(base).saturating_add(1)` rather than plain
    /// arithmetic: `top` is allowed to be `u32::MAX` (an empty map's
    /// nominal upper bound), and computing an exact byte count for
    /// `[base, u32::MAX]` would need one more value than `u32` can hold.
    /// Saturating instead of overflow-panicking loses at most the single
    /// byte at `u32::MAX` in that extreme, never-hit-in-practice corner
    /// (this crate's guest address spaces are megabytes, not 4
    /// gigabytes); [`Self::clamp_range`] (via [`Self::mark_valid`])
    /// clamps to the map's real length regardless.
    fn clear_stack_region(&mut self, base: u32, top: u32) {
        let len = top.saturating_sub(base).saturating_add(1);
        self.mark_valid(base, len);
    }

    /// Updates the tracked stack pointer to `sp`, called once per
    /// instruction by the run loop. Does nothing if [`Self::
    /// begin_stack_tracking`] hasn't been called. See this module's
    /// "stack-pointer tracking" doc for the full design rationale;
    /// summary:
    ///
    /// - Unchanged SP (the overwhelmingly common case): does nothing
    ///   beyond the one comparison that detects this.
    /// - SP decreased (stack grew): marks `[sp, old_sp)`
    ///   [`ShadowState::Uninit`].
    /// - SP increased (stack shrank): marks `[old_sp, sp)`
    ///   [`PoisonReason::BelowStackPointer`].
    /// - New SP outside `[stack_base, stack_top]`: suspends tracking --
    ///   no report -- but first clears the stale
    ///   [`PoisonReason::BelowStackPointer`] poison over the whole
    ///   previously-tracked region and empties the shadow call stack,
    ///   so neither becomes a landmine for whatever guest addresses get
    ///   reused next (see this module's "stack-pointer tracking" and
    ///   "shadow call stack" docs -- this is the generic path;
    ///   [`Self::reset_stack_tracking`] is the direct one `StackSwap`
    ///   itself uses). This is the normal case for supervisor-mode
    ///   execution (or any other unforeseen route the SP takes outside
    ///   the tracked region), not a bug.
    /// - New SP back inside `[stack_base, stack_top]` after being
    ///   suspended: resumes tracking, adopting `sp` as the new baseline
    ///   and re-poisoning `[stack_base, sp)` exactly as
    ///   [`Self::begin_stack_tracking`] would, so detection actually
    ///   comes back instead of staying silently off for the rest of the
    ///   run.
    pub fn update_stack_pointer(&mut self, sp: u32) {
        let Some(region) = self.stack else {
            return;
        };
        if region.sp == Some(sp) {
            return; // hot path: nothing moved.
        }
        let StackRegion {
            base,
            top,
            sp: last_sp,
        } = region;
        let in_region = base <= sp && sp <= top;
        match last_sp {
            Some(old_sp) => {
                if !in_region {
                    // Leaving the region we were tracking: nothing else
                    // ever lives inside a stack region (see
                    // reset_stack_tracking's doc), so retracting every
                    // claim over it -- the poison and the call frames
                    // recorded against it -- is always safe, and leaving
                    // either behind is exactly the stale-claim bug this
                    // module's docs describe.
                    self.clear_stack_region(base, top);
                    self.call_stack.clear();
                    self.stack = Some(StackRegion {
                        base,
                        top,
                        sp: None,
                    });
                    return;
                }
                // Both old_sp and sp are within [base, top] here, so
                // the ranges marked below are too -- no separate clamp
                // needed (mark_uninit/mark_unaddressable also clamp to
                // the map's own bounds regardless; see clamp_range).
                if sp < old_sp {
                    self.mark_uninit(sp, old_sp - sp);
                } else {
                    self.mark_unaddressable(old_sp, sp - old_sp, PoisonReason::BelowStackPointer);
                }
                self.stack = Some(StackRegion {
                    base,
                    top,
                    sp: Some(sp),
                });
            }
            None => {
                if in_region {
                    // Resuming: re-establish poisoning from the new
                    // baseline rather than adopting it with a silent,
                    // permanently-blind shadow map (see this module's
                    // "stack-pointer tracking" doc).
                    self.poison_below_sp(base, sp);
                    self.stack = Some(StackRegion {
                        base,
                        top,
                        sp: Some(sp),
                    });
                }
                // else: still outside the tracked region, remain
                // suspended.
            }
        }
    }

    /// Tears down tracking of whatever stack region was previously
    /// tracked (if any) and begins tracking a brand new one from `sp` --
    /// the operation `exec.library`'s `StackSwap` handler
    /// ([`crate::exectask`]'s `stack_swap_handler`) needs right after it
    /// finishes swapping a task onto a different stack.
    ///
    /// This exists as a distinct entry point rather than relying on
    /// [`Self::update_stack_pointer`]'s generic "SP left the region"
    /// path because a `StackSwap` is a *known*, atomic switch, not an
    /// SP that gradually wanders off: the handler teleports `A7`
    /// straight from the old stack to the new one in a single step (see
    /// `exectask.rs`'s `StackSwap` doc), so there is no sequence of
    /// per-instruction [`Self::update_stack_pointer`] calls that would
    /// ever ask this module to notice the old region being abandoned --
    /// only calls describing the *new* stack, which
    /// [`Self::update_stack_pointer`] would otherwise happily (and
    /// wrongly) treat as ordinary growth/shrinkage of whatever region it
    /// still thinks is live, or -- if the new stack happens to fall
    /// entirely outside the old tracked bounds -- as an ordinary
    /// suspend, leaving the *old* stack's poison and call frames
    /// dangling until the generic path (never called again for that
    /// region) would have cleaned them up. Calling this directly the
    /// moment the swap completes cleans up eagerly instead of relying on
    /// that coincidence.
    ///
    /// Concretely:
    ///
    /// 1. Clears the stale [`PoisonReason::BelowStackPointer`] poison
    ///    over the *entire* previously-tracked `[base, top]`, not just
    ///    the sub-range that happened to be poisoned at the moment of
    ///    the switch. A narrower "precise" clear (say, just
    ///    `[base, last_seen_sp)`) would be just as correct -- this
    ///    module's own stack-tracking machinery never poisons anything
    ///    else inside a stack region -- but would need to thread the
    ///    last-seen SP through here for no real benefit: **nothing else
    ///    ever lives inside `[base, top]`**. A stack region is
    ///    exclusively reserved for one task's frames; it is never shared
    ///    with the heap allocator's redzones or any other bookkeeping
    ///    this module tracks, so a blanket clear over the whole region
    ///    can never accidentally un-poison something unrelated that
    ///    happens to overlap it. The extra byte-writes this costs are
    ///    `O(stack region)`, but only on a stack switch -- a handful of
    ///    times per run (`sc`, the motivating case below, does it four
    ///    times for a whole compile), nothing like the per-instruction
    ///    frequency [`Self::update_stack_pointer`] itself has to stay
    ///    `O(1)` for.
    /// 2. Clears the shadow call stack entirely -- see this module's
    ///    "shadow call stack" doc for why a frame recorded against an
    ///    abandoned stack is a landmine (a future slot-address
    ///    collision with the new stack), not a merely-stale-but-harmless
    ///    entry.
    /// 3. Begins tracking `[stack_base, stack_top]` from `sp`, via
    ///    [`Self::begin_stack_tracking`] (shared, not duplicated: both
    ///    ultimately just need to poison `[stack_base, sp)` and record
    ///    the new region as current).
    ///
    /// Two concrete bugs motivated this, both filed against the same
    /// `StackSwap` gap: `fixtures/stacktest`'s `pushret` case already
    /// established that a *never-recorded* frame (a computed jump) must
    /// not be flagged; this closes the sibling failure mode, a frame
    /// that *was* legitimately recorded and then outlived the stack it
    /// was recorded on. And running the real SAS/C `sc` compiler under
    /// `--sanitize` hit both halves of this at once -- `sc` calls
    /// `StackSwap` onto its own larger heap-allocated stack and back,
    /// which produced both a `ReturnAddressCorrupted` false positive
    /// (the call-stack half) and stray below-SP read/write reports
    /// against the abandoned original stack (the poison half) -- see
    /// this module's top-of-file "stack-pointer tracking" doc for that
    /// half's own detailed writeup.
    pub fn reset_stack_tracking(&mut self, stack_base: u32, stack_top: u32, sp: u32) {
        if let Some(old) = self.stack {
            self.clear_stack_region(old.base, old.top);
        }
        self.call_stack.clear();
        self.begin_stack_tracking(stack_base, stack_top, sp);
    }

    /// Records a subroutine call whose `JSR` just pushed `return_addr`
    /// at guest address `slot_sp` (i.e. `slot_sp` is the SP immediately
    /// after the push). See this module's "shadow call stack" doc.
    ///
    /// Before pushing the new frame, discards every previously recorded
    /// frame whose slot lies below `slot_sp` -- that space has since
    /// been reclaimed (by any means; see the module doc's
    /// reconciliation rule), so those frames could never be validly
    /// returned to and would otherwise sit in the log forever.
    ///
    /// If this would exceed [`MAX_CALL_STACK_DEPTH`], the oldest
    /// (outermost) frame is dropped to make room, and [`Self::
    /// dropped_call_frames`]'s count is incremented -- see
    /// [`MAX_CALL_STACK_DEPTH`]'s doc for why the oldest, not the
    /// newest, is the one sacrificed.
    pub fn record_call(&mut self, slot_sp: u32, return_addr: u32) {
        self.call_stack.retain(|f| f.slot_sp >= slot_sp);
        self.call_stack.push_back(CallFrame {
            slot_sp,
            return_addr,
        });
        if self.call_stack.len() > MAX_CALL_STACK_DEPTH {
            self.call_stack.pop_front();
            self.dropped_call_frames += 1;
        }
    }

    /// Validates a return: `sp` is the stack pointer the `RTS` (or
    /// equivalent) is returning with, `actual_return_addr` is the
    /// address it actually jumped to (read from the stack slot at
    /// `sp`... or wherever it isn't). See this module's "shadow call
    /// stack" doc for the full reconciliation rule this implements;
    /// summary:
    ///
    /// 1. Discards every recorded frame whose slot lies below `sp` --
    ///    already unwound, by any means, since that's exactly the
    ///    region [`Self::update_stack_pointer`] would also now consider
    ///    dead.
    /// 2. If what's left has a top frame whose slot is *exactly* `sp`,
    ///    this return is popping that frame: compares the recorded
    ///    return address against `actual_return_addr` and reports
    ///    [`ViolationKind::ReturnAddressCorrupted`] on a mismatch, then
    ///    removes the frame either way (it's been consumed).
    /// 3. Otherwise -- no frame recorded for this exact slot -- reports
    ///    nothing. This is not a gap: it's what makes computed jumps
    ///    (`move.l #target,-(sp) / rts`) and volamos's own
    ///    process-startup return address (which no `JSR` ever pushed)
    ///    non-findings instead of false positives.
    pub fn check_return(&mut self, sp: u32, actual_return_addr: u32) {
        self.call_stack.retain(|f| f.slot_sp >= sp);
        let Some(&top) = self.call_stack.back() else {
            return;
        };
        if top.slot_sp != sp {
            return;
        }
        self.call_stack.pop_back();
        if top.return_addr != actual_return_addr {
            self.record_return_corruption(sp, top.return_addr, actual_return_addr);
        }
    }

    /// How many call frames [`Self::record_call`] has discarded because
    /// the shadow call stack was already at [`MAX_CALL_STACK_DEPTH`] --
    /// surfaced so a report can say "N frames were never checked"
    /// instead of implying a false all-clear for very deep recursion.
    pub fn dropped_call_frames(&self) -> u64 {
        self.dropped_call_frames
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

        // One access, one violation: `check_read` stops at the first
        // offending byte rather than logging byte 8 and byte 9
        // separately, since both describe the same access and the same
        // bug (and the per-byte addresses would defeat dedup, whose key
        // includes the address). The reported address is the first byte
        // that actually offended, not the access's base.
        assert_eq!(shadow.violation_count(), 1, "one access reports once");
        let v = &shadow.violations()[0];
        assert_eq!(v.addr, 8, "reports the first offending byte");
        assert_eq!(v.size, 4, "and remembers the whole access's size");
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

    // -- Stack-pointer tracking -------------------------------------

    #[test]
    fn begin_stack_tracking_poisons_below_the_initial_sp() {
        let mut shadow = ShadowMap::new(0x2000);
        shadow.begin_stack_tracking(0x1000, 0x2000, 0x1800);

        assert_eq!(shadow.state(0x0fff), ShadowState::Valid, "outside stack");
        assert_eq!(shadow.state(0x1000), ShadowState::Unaddressable);
        assert_eq!(
            shadow.poison_reason(0x1000),
            Some(PoisonReason::BelowStackPointer)
        );
        assert_eq!(shadow.state(0x17ff), ShadowState::Unaddressable);
        assert_eq!(shadow.state(0x1800), ShadowState::Valid, "at/above sp");
        assert_eq!(shadow.state(0x1fff), ShadowState::Valid);
    }

    #[test]
    fn stack_growth_marks_the_new_bytes_uninit_not_valid() {
        let mut shadow = ShadowMap::new(0x2000);
        shadow.begin_stack_tracking(0x1000, 0x2000, 0x1800);

        // sp decreases: the stack grew by pushing 0x100 bytes.
        shadow.update_stack_pointer(0x1700);

        assert_eq!(
            shadow.state(0x1700),
            ShadowState::Uninit,
            "newly-used stack is Uninit, not Valid -- see module doc"
        );
        assert_eq!(shadow.state(0x17ff), ShadowState::Uninit);
        // Below the new sp is still poisoned.
        assert_eq!(shadow.state(0x1000), ShadowState::Unaddressable);
        assert_eq!(shadow.state(0x16ff), ShadowState::Unaddressable);
        // Reading Uninit isn't reported unless report_uninit is set.
        assert_eq!(shadow.violation_count(), 0);
    }

    #[test]
    fn stack_shrink_marks_the_released_bytes_below_stack_pointer() {
        let mut shadow = ShadowMap::new(0x2000);
        shadow.begin_stack_tracking(0x1000, 0x2000, 0x1700);

        // sp increases: the stack shrank, releasing [0x1700, 0x1900).
        shadow.update_stack_pointer(0x1900);

        assert_eq!(shadow.state(0x1700), ShadowState::Unaddressable);
        assert_eq!(
            shadow.poison_reason(0x1700),
            Some(PoisonReason::BelowStackPointer)
        );
        assert_eq!(shadow.state(0x18ff), ShadowState::Unaddressable);
        assert_eq!(shadow.state(0x1900), ShadowState::Valid, "still in use");

        shadow.set_current_pc(0x99);
        shadow.check_read(0x1800, 1);
        assert_eq!(shadow.violation_count(), 1);
        assert_eq!(
            shadow.violations()[0].reason,
            Some(PoisonReason::BelowStackPointer)
        );
    }

    #[test]
    fn unchanged_stack_pointer_does_nothing() {
        let mut shadow = ShadowMap::new(0x2000);
        shadow.begin_stack_tracking(0x1000, 0x2000, 0x1800);
        let before = shadow.bytes.clone();

        shadow.update_stack_pointer(0x1800);
        shadow.update_stack_pointer(0x1800);
        shadow.update_stack_pointer(0x1800);

        assert_eq!(shadow.bytes, before, "no shadow byte should have moved");
    }

    #[test]
    fn sp_leaving_the_tracked_region_clears_stale_poison_and_call_frames() {
        let mut shadow = ShadowMap::new(0x2000);
        shadow.begin_stack_tracking(0x1000, 0x2000, 0x1800);
        assert_eq!(shadow.state(0x1000), ShadowState::Unaddressable);
        // A call frame recorded on the stack we're about to abandon.
        shadow.record_call(0x17fc, 0x1234);

        // StackSwap (or supervisor mode) hands the CPU a completely
        // different stack, far outside [0x1000, 0x2000).
        shadow.update_stack_pointer(0x8000);

        // The abandoned region's stale BelowStackPointer poison must be
        // retracted -- left in place, it would become a landmine for
        // whatever later reuses those guest addresses (the real
        // false-positive class this fixes; see this module's doc).
        assert_eq!(shadow.state(0x1000), ShadowState::Valid);
        assert_eq!(shadow.state(0x17ff), ShadowState::Valid);
        shadow.check_read(0x1000, 4);
        shadow.check_write(0x17ff, 1);
        assert_eq!(shadow.violation_count(), 0);

        // The abandoned call frame must not be checked either.
        shadow.check_return(0x17fc, 0x1234);
        assert_eq!(shadow.violation_count(), 0);

        // While suspended, further moves (even ones that look like a
        // huge "delta" against the old sp) must still be no-ops.
        let after_leaving = shadow.bytes.clone();
        shadow.update_stack_pointer(0x8100);
        shadow.update_stack_pointer(0x7000);
        assert_eq!(shadow.bytes, after_leaving);
    }

    #[test]
    fn sp_re_entering_the_tracked_region_resumes_and_re_poisons_from_the_new_baseline() {
        let mut shadow = ShadowMap::new(0x2000);
        shadow.begin_stack_tracking(0x1000, 0x2000, 0x1800);

        shadow.update_stack_pointer(0x8000); // leave (e.g. supervisor mode)
        shadow.update_stack_pointer(0x1750); // return to the original stack

        // Detection must actually resume: the resumed baseline
        // re-poisons [base, sp) exactly as begin_stack_tracking would,
        // rather than leaving the whole region silently (and therefore
        // undetectably) Valid for the rest of the run.
        assert_eq!(shadow.state(0x1000), ShadowState::Unaddressable);
        assert_eq!(shadow.state(0x174f), ShadowState::Unaddressable);
        assert_eq!(shadow.state(0x1750), ShadowState::Valid);

        // Tracking is active again: a subsequent move produces a normal
        // incremental delta relative to the resumed baseline (0x1750),
        // not relative to whatever sp was before we left (0x1800).
        shadow.update_stack_pointer(0x1740);
        assert_eq!(shadow.state(0x1740), ShadowState::Uninit);
        assert_eq!(shadow.state(0x174f), ShadowState::Uninit);
    }

    #[test]
    fn absurd_delta_is_clamped_to_the_tracked_region_not_the_whole_map() {
        let mut shadow = ShadowMap::new(0x10000);
        shadow.begin_stack_tracking(0x1000, 0x2000, 0x1800);

        // A jump far outside the tracked region must be treated as
        // "left the region" (see the leaving-tracking test), never as
        // a same-region delta that would try to poison from 0x1800 up
        // to/through u32::MAX.
        shadow.update_stack_pointer(u32::MAX);

        assert_eq!(
            shadow.state(0x1800),
            ShadowState::Valid,
            "old sp position must be untouched, not swept into a bogus poison"
        );
        assert_eq!(shadow.state(0xffff), ShadowState::Valid);

        // A legitimate huge-but-in-region delta (grow all the way to
        // stack_base from the freshly-resumed baseline) is still fine
        // and stays inside [base, top].
        shadow.update_stack_pointer(0x1900); // re-enter, new baseline
        shadow.update_stack_pointer(0x1000); // grow to the very base
        assert_eq!(shadow.state(0x1000), ShadowState::Uninit);
        assert_eq!(shadow.state(0x18ff), ShadowState::Uninit);
        // Above the resumed baseline was never part of this delta.
        assert_eq!(shadow.state(0x1900), ShadowState::Valid);
        // Never touched outside the map/region regardless.
        assert_eq!(shadow.state(0x2000), ShadowState::Valid);
    }

    #[test]
    fn reset_stack_tracking_clears_the_previous_region_stale_poison() {
        // The below-SP half of the sc/StackSwap bug: a shadow map that
        // still says an address is BelowStackPointer, purely because
        // the task that used to own that stack region swapped away
        // from it, with no other claim on the memory ever established.
        let mut shadow = ShadowMap::new(0x4000);
        shadow.begin_stack_tracking(0x1000, 0x2000, 0x1800);
        assert_eq!(shadow.state(0x1000), ShadowState::Unaddressable);

        // StackSwap onto an entirely different (e.g. heap-allocated)
        // stack region.
        shadow.reset_stack_tracking(0x3000, 0x3800, 0x3800);

        // The old region's poison must be gone -- it no longer
        // describes anything live once the task has swapped off that
        // stack.
        assert_eq!(shadow.state(0x1000), ShadowState::Valid);
        assert_eq!(shadow.state(0x17ff), ShadowState::Valid);
        shadow.check_read(0x1500, 4);
        assert_eq!(shadow.violation_count(), 0);

        // The new region is tracked and poisoned exactly as
        // begin_stack_tracking would (sp == top here, so the whole
        // region below it is poisoned).
        assert_eq!(shadow.state(0x3000), ShadowState::Unaddressable);
        assert_eq!(shadow.state(0x37ff), ShadowState::Unaddressable);
    }

    #[test]
    fn reset_stack_tracking_clears_pending_call_frames() {
        // The return-address half of the sc/StackSwap bug: a frame
        // recorded on the old stack must not survive to collide with an
        // unrelated frame the new stack later records at the same
        // guest address.
        let mut shadow = ShadowMap::new(0x4000);
        shadow.begin_stack_tracking(0x1000, 0x2000, 0x1800);
        // A call recorded on the old stack, never returned through --
        // StackSwap abandons it, same as a longjmp would.
        shadow.record_call(0x17fc, 0x1234);

        // The new stack happens to reuse the exact same guest address
        // for its own, entirely unrelated frame.
        shadow.reset_stack_tracking(0x3000, 0x3800, 0x3800);
        shadow.record_call(0x17fc, 0x5678);

        // A return through 0x17fc now belongs to the new frame; it must
        // be judged against 0x5678 on its own merits, not flagged as
        // corruption against the abandoned old-stack frame's 0x1234.
        shadow.check_return(0x17fc, 0x5678);
        assert_eq!(
            shadow.violation_count(),
            0,
            "the reused slot's new frame must be judged on its own merits"
        );
    }

    // -- Shadow call stack --------------------------------------------

    #[test]
    fn clean_call_and_return_reports_nothing() {
        let mut shadow = ShadowMap::new(0x2000);
        shadow.record_call(0x1000, 0x4000);

        shadow.check_return(0x1000, 0x4000);

        assert_eq!(shadow.violation_count(), 0);
    }

    #[test]
    fn overwritten_return_address_is_reported_with_expected_and_actual() {
        let mut shadow = ShadowMap::new(0x2000);
        shadow.record_call(0x1000, 0x4000);
        shadow.set_current_pc(0x3a1c);

        // Something smashed the stack: the slot now holds 0x41414141
        // instead of the recorded return address.
        shadow.check_return(0x1000, 0x4141_4141);

        assert_eq!(shadow.violation_count(), 1);
        let v = &shadow.violations()[0];
        assert_eq!(v.kind, ViolationKind::ReturnAddressCorrupted);
        assert_eq!(v.addr, 0x1000);
        assert_eq!(v.expected_return_addr, Some(0x4000));
        assert_eq!(v.actual_return_addr, Some(0x4141_4141));
        assert_eq!(v.pc, 0x3a1c);

        let text = v.to_string();
        assert!(text.contains("0x00004000"), "{text}");
        assert!(text.contains("0x41414141"), "{text}");
    }

    #[test]
    fn unwinding_a_frame_below_sp_reports_nothing() {
        let mut shadow = ShadowMap::new(0x2000);
        // A deep call whose frame will be abandoned by a longjmp-style
        // unwind rather than a matching RTS.
        shadow.record_call(0x0f00, 0x4000);
        // An outer call that will actually return normally.
        shadow.record_call(0x1000, 0x5000);

        // Unwind straight past the inner frame (0x0f00 < 0x1000) without
        // ever "returning" through it.
        shadow.check_return(0x1000, 0x5000);

        assert_eq!(
            shadow.violation_count(),
            0,
            "the abandoned inner frame must not be flagged"
        );
    }

    #[test]
    fn rts_with_no_matching_frame_reports_nothing() {
        let mut shadow = ShadowMap::new(0x2000);
        // No record_call at all -- e.g. volamos's own process-startup
        // return address, which no JSR ever pushed.
        shadow.check_return(0x1000, 0x1234_5678);

        assert_eq!(shadow.violation_count(), 0);
    }

    #[test]
    fn computed_jump_push_then_rts_idiom_reports_nothing() {
        let mut shadow = ShadowMap::new(0x2000);
        shadow.record_call(0x2000, 0x4000);

        // `move.l #target,-(sp)` followed by `rts`: a slot gets a value
        // pushed and immediately "returned" through, but record_call
        // was never told about it, so there's no frame at this slot.
        shadow.check_return(0x1ffc, 0xdead_beef);

        assert_eq!(shadow.violation_count(), 0);
        // The real recorded frame at 0x2000 is untouched by this.
        shadow.check_return(0x2000, 0x4000);
        assert_eq!(shadow.violation_count(), 0);
    }

    #[test]
    fn library_dispatch_style_pop_without_rts_does_not_get_flagged_later() {
        // volamos's own dispatch.rs pops a JSR's return address and
        // resumes there itself, without ever executing a real RTS. The
        // frame must simply become stale once sp has moved back past
        // it, not linger and misfire against some unrelated later
        // return that happens to reuse the same slot.
        let mut shadow = ShadowMap::new(0x2000);
        shadow.record_call(0x1000, 0x4000);

        // Stack pointer moves back up past 0x1000 without a check_return
        // ever happening for that frame (dispatch.rs's own doing).
        shadow.record_call(0x1004, 0x9999); // a later, unrelated call reusing/near that area

        // Reconciliation on this new call already dropped the stale
        // frame at 0x1000 (0x1000 < 0x1004), so a later return matching
        // 0x1000 again must not somehow resurrect it.
        shadow.check_return(0x1000, 0x4000);
        assert_eq!(shadow.violation_count(), 0);
    }

    #[test]
    fn call_stack_depth_is_bounded_and_drops_are_counted() {
        let mut shadow = ShadowMap::new(0x10_0000);
        // Push far more frames than MAX_CALL_STACK_DEPTH, each at a
        // strictly *decreasing* slot address -- mimicking a real
        // downward-growing stack, where each nested call's return
        // address lands below the previous one's, so record_call's own
        // "below this slot" pruning doesn't reconcile any of them away.
        let extra = 10;
        let base_slot = 0x0080_0000u32;
        for i in 0..(MAX_CALL_STACK_DEPTH + extra) as u32 {
            shadow.record_call(base_slot - i * 8, 0x9000_0000 + i);
        }

        assert_eq!(shadow.dropped_call_frames(), extra as u64);

        // The most recent (innermost) frame is still there and still
        // gets checked. Checked first, since its slot is the smallest
        // address of all recorded frames -- reconciling against it
        // can't discard any other still-live frame.
        let last = (MAX_CALL_STACK_DEPTH + extra - 1) as u32;
        let last_slot = base_slot - last * 8;
        let last_expected = 0x9000_0000 + last;
        shadow.check_return(last_slot, last_expected.wrapping_add(1));
        assert_eq!(shadow.violation_count(), 1);

        // The oldest frame (at base_slot) was dropped for exceeding the
        // depth cap, so a return matching its slot must not be found --
        // even though, being the highest address of all, reconciling
        // against it also legitimately discards every remaining frame
        // (they're all "below" it), which is why this comes last.
        shadow.check_return(base_slot, 0x9000_0000);
        assert_eq!(
            shadow.violation_count(),
            1,
            "dropped frame must not be checked against"
        );
    }
}
