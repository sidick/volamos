//! A minimal, flat guest address space.
//!
//! The m68k guest CPU is big-endian, so all multi-byte accesses here use
//! big-endian byte order regardless of the host's native endianness.
//!
//! # Optional sanitizer
//!
//! [`FlatMemory`] can optionally carry a [`crate::sanitize::ShadowMap`]
//! (installed with [`FlatMemory::enable_sanitizer`], off by default --
//! see the CLI's `--sanitize` flag). When installed, every access path
//! below (`read_u8`/`write_u8` and all four overridden multi-byte fast
//! paths) checks/updates it first; when not installed, every access is
//! exactly as fast and exactly as total as before this feature existed
//! -- one `Option` check (via [`FlatMemory::shadow_mut`]/[`FlatMemory::
//! shadow`]) per access, not per byte.
//!
//! The shadow map lives *inside* `FlatMemory` rather than as a separate
//! wrapper type implementing [`AddressSpace`]: [`crate::cpu::Cpu::
//! Memory`] is a single concrete associated type (`FlatMemory`, see
//! `crate::backend`'s module doc on why), so a generic wrapper would
//! need threading a type parameter through the entire runtime for a
//! feature that's off by default. An `Option` field is far simpler and
//! costs nothing when unused.

use crate::sanitize::ShadowMap;

/// The width of one guest CPU store, as seen by a [`StorePort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreSize {
    /// A byte store.
    Byte,
    /// A word store.
    Word,
    /// A long store.
    Long,
}

/// A device-style, write-only port at a fixed guest address, fed by guest
/// CPU stores (the debug ports -- see [`crate::uaedebug`], [`crate::emu68debug`]).
///
/// Installed with [`FlatMemory::add_store_port`]. Stores are offered to
/// the ports in [`crate::backend`]'s [`m68k::AddressBus`] implementation,
/// **before** the bounds check, the sanitizer shadow map and the backing
/// bytes: the address needn't be backed by RAM (it may be far outside
/// `--ram`), a claimed store never reaches memory, and it is never a
/// sanitizer violation. Library-handler writes through [`AddressSpace`]
/// are not guest stores and are never offered. Reads are not intercepted.
pub trait StorePort {
    /// Whether a store at `addr` belongs to this port, judged on the
    /// address exactly as the CPU core presents it (68000/68010 cores
    /// present the 24-bit-masked address).
    fn claims(&self, addr: u32) -> bool;

    /// Whether a store of `size` at `addr` belongs to this port. Defaults
    /// to [`Self::claims`]; a port that must let some widths fall through
    /// to ordinary memory (the Emu68 port is byte-only) overrides it.
    /// This is what the store path actually asks.
    fn claims_store(&self, addr: u32, _size: StoreSize) -> bool {
        self.claims(addr)
    }

    /// Handles a claimed store. `mem` is the guest memory, for ports that
    /// need to read it (e.g. a string the guest points at).
    fn store(&mut self, mem: &dyn AddressSpace, addr: u32, value: u32, size: StoreSize);
}

/// A byte-addressable memory space that a [`Cpu`](crate::cpu::Cpu)
/// implementation can read from and write to.
///
/// Multi-byte reads/writes use m68k (big-endian) byte order.
///
/// Out-of-range behavior is deliberately simple and total (no `Result`,
/// no panics): reads past the end of the backing store return `0`, and
/// writes past the end of the backing store are silently ignored. This
/// keeps the trait ergonomic for a CPU core's hot path; callers that need
/// to detect bad guest addresses (e.g. to raise a bus error) should check
/// bounds themselves via [`AddressSpace::len`] before accessing.
pub trait AddressSpace {
    /// The size in bytes of this address space.
    fn len(&self) -> usize;

    /// Returns `true` if this address space has zero bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Guest-CPU bus accesses observed so far, as `(reads, writes)`.
    /// `(0, 0)` by default, for address spaces that don't count --
    /// counting is [`FlatMemory`]'s job and only meaningful under
    /// `--clock-mhz`; see [`FlatMemory::access_counts`] for why the count
    /// lives in the `m68k::AddressBus` impl rather than here.
    fn bus_access_counts(&self) -> (u64, u64) {
        (0, 0)
    }

    /// Reads a single byte. Returns `0` if `addr` is out of range.
    fn read_u8(&self, addr: u32) -> u8;

    /// Writes a single byte. Silently ignored if `addr` is out of range.
    fn write_u8(&mut self, addr: u32, value: u8);

    /// Clears any sanitizer poison over `[addr, addr + len)`, a block the
    /// guest heap has just handed out (issue #95).
    ///
    /// **Called by [`crate::guestmem::GuestHeap::alloc_with_requested`],
    /// which is the only place that should:** the heap recycles
    /// addresses, so a fresh block may still carry the
    /// `PoisonReason::Freed` marking of the previous allocation, and
    /// writing over it does not clear that (the shadow map heals
    /// `Uninit` bytes but records and keeps `Unaddressable` ones). Doing
    /// it at the one point where a block leaves the heap's control --
    /// rather than in each of the four dozen handlers that allocate --
    /// is what keeps a new allocation site from silently reintroducing
    /// the bug; see that method's doc for the full reasoning.
    ///
    /// Default: nothing to do, for address spaces with no shadow map.
    fn clear_fresh_block(&mut self, _addr: u32, _len: u32) {}

    /// Reads a big-endian 16-bit value. Any byte that falls out of range
    /// reads as `0`.
    fn read_u16(&self, addr: u32) -> u16 {
        let hi = self.read_u8(addr) as u16;
        let lo = self.read_u8(addr.wrapping_add(1)) as u16;
        (hi << 8) | lo
    }

    /// Writes a big-endian 16-bit value. Any byte that falls out of range
    /// is silently dropped.
    fn write_u16(&mut self, addr: u32, value: u16) {
        self.write_u8(addr, (value >> 8) as u8);
        self.write_u8(addr.wrapping_add(1), value as u8);
    }

    /// Reads a big-endian 32-bit value. Any byte that falls out of range
    /// reads as `0`.
    fn read_u32(&self, addr: u32) -> u32 {
        let hi = self.read_u16(addr) as u32;
        let lo = self.read_u16(addr.wrapping_add(2)) as u32;
        (hi << 16) | lo
    }

    /// Writes a big-endian 32-bit value. Any byte that falls out of range
    /// is silently dropped.
    fn write_u32(&mut self, addr: u32, value: u32) {
        self.write_u16(addr, (value >> 16) as u16);
        self.write_u16(addr.wrapping_add(2), value as u16);
    }

    /// The sanitizer shadow map this address space carries, if any.
    ///
    /// This lives on the trait (defaulting to `None`) rather than only
    /// as an inherent method on [`FlatMemory`] because the library-call
    /// handlers that need to poison and un-poison ranges --
    /// `crate::execmem`'s `AllocMem`/`FreeMem` above all -- are generic
    /// over `C: Cpu` and so only ever see their memory as
    /// `C::Memory: AddressSpace`, never as the concrete `FlatMemory`.
    /// Without these two methods the shadow map would be installed but
    /// unreachable from precisely the code that knows where allocations
    /// begin and end.
    fn shadow(&self) -> Option<&crate::sanitize::ShadowMap> {
        None
    }

    /// Mutable counterpart to [`AddressSpace::shadow`]. Defaults to
    /// `None` so an address space with no sanitizer support needs no
    /// boilerplate.
    fn shadow_mut(&mut self) -> Option<&mut crate::sanitize::ShadowMap> {
        None
    }

    /// Invalidates the modeled instruction cache (`--cache`, issue
    /// #148), if one is installed.
    ///
    /// This lives on the trait (defaulting to a no-op) for the same
    /// reason [`AddressSpace::shadow`] does: `crate::execmem`'s
    /// `CacheClearU`/`CacheClearE` handlers -- the only callers -- are
    /// generic over `C: Cpu` and so only ever see their memory as
    /// `C::Memory: AddressSpace`, never as the concrete [`FlatMemory`].
    ///
    /// Always invalidates everything rather than modeling a
    /// partial-range clear, matching real `CacheClearE`'s own
    /// documented latitude to do exactly that ("regardless of the
    /// length given, the function will determine the most efficient
    /// way to implement the operation... the entire cache may be
    /// cleared" -- real 68030 hardware does this too) -- there is no
    /// cached *data* to lose by over-invalidating, only hit/miss
    /// timing to reset, so a full clear is simpler and just as
    /// correct as a precise one.
    fn invalidate_icache(&mut self) {}
}

/// A direct-mapped instruction-cache hit/miss model for `--cache`
/// (issue #148) -- see [`FlatMemory::enable_icache`].
///
/// Tracks only which guest longwords are resident, never their
/// contents: a guest instruction fetch always reads real, current
/// memory regardless of cache state (see `crate::backend`'s
/// `AddressBus::read_immediate_word`/`read_immediate_long`, the only
/// callers of [`Self::access_span`]) -- this model exists purely to
/// drive the `m68k` crate's cache-hit/miss *timing* hooks
/// (`AddressBus::last_fetch_was_cached`/
/// `instruction_fetches_were_cached`), and has no bearing on
/// correctness.
///
/// Modeled the way Copperline's `src/cache.rs` is (cited as a design
/// reference in issue #148, GPL-3.0 so not shared code): one tag slot
/// per [`Self::LINE_SIZE`]-byte line, direct-mapped by `(line base /
/// LINE_SIZE) % lines.len()`.
///
/// Two simplifications, both deliberate (see the CLI's `--cache` flag
/// doc): a real `CACRF_EnableI` toggle (`crate::execmem`'s
/// `CacheControl`) is not modeled -- `--cache` is an unconditional
/// CLI-level choice for the whole run, not something a guest can turn
/// on or off at runtime -- and the instruction cache never snoops
/// data writes (matching real 68020/68030/68040/68060 hardware; that
/// asymmetry is exactly why `CacheClearU`/`CacheClearE` exist at all),
/// so only [`FlatMemory::invalidate_icache`] ever clears it.
struct IcacheModel {
    lines: Vec<Option<u32>>,
}

impl IcacheModel {
    /// One cache line per longword -- matches Copperline's model (see
    /// this type's doc) and is small enough that a 256-byte 68020/030
    /// cache still gets 64 independent tag slots.
    const LINE_SIZE: u32 = 4;

    /// `size_bytes` is the CLI's `--cache` sizing by `--cpu` (256 on
    /// 68020/68030, 4096 on 68040/68060); rounded down to a whole
    /// number of [`Self::LINE_SIZE`]-byte lines, at least one.
    fn new(size_bytes: u32) -> Self {
        let lines = ((size_bytes / Self::LINE_SIZE) as usize).max(1);
        Self {
            lines: vec![None; lines],
        }
    }

    /// The base address of the line containing `addr`.
    fn line_base(addr: u32) -> u32 {
        addr & !(Self::LINE_SIZE - 1)
    }

    fn index(&self, line_base: u32) -> usize {
        (line_base / Self::LINE_SIZE) as usize % self.lines.len()
    }

    /// Looks up the line covering `addr`, filling it in (recording a
    /// miss) if it wasn't already resident. Returns whether this was
    /// a hit.
    fn access(&mut self, addr: u32) -> bool {
        let base = Self::line_base(addr);
        let idx = self.index(base);
        let hit = self.lines[idx] == Some(base);
        self.lines[idx] = Some(base);
        hit
    }

    /// As [`Self::access`], but for a fetch spanning `len` bytes from
    /// `addr` (a word or long immediate can straddle two lines when
    /// `addr` isn't [`Self::LINE_SIZE`]-aligned, e.g. a 4-byte fetch
    /// at a merely 2-byte-aligned PC) -- a hit only if *every* line
    /// the fetch touches was already resident, matching how a real
    /// cache miss stalls the whole access regardless of which word
    /// within it was the one missing. Only ever called with `len` of
    /// 2 or 4 (a word or long instruction-stream read), so this always
    /// terminates in at most two [`Self::access`] calls.
    fn access_span(&mut self, addr: u32, len: u32) -> bool {
        debug_assert!(
            len == 2 || len == 4,
            "instruction fetches are word/long only"
        );
        let last = Self::line_base(addr.wrapping_add(len - 1));
        let mut hit = true;
        let mut base = Self::line_base(addr);
        loop {
            hit &= self.access(base);
            if base == last {
                break;
            }
            base = base.wrapping_add(Self::LINE_SIZE);
        }
        hit
    }

    fn invalidate_all(&mut self) {
        self.lines.fill(None);
    }
}

/// A simple flat, contiguous [`AddressSpace`] backed by a `Vec<u8>`.
///
/// This is a placeholder implementation good enough for early CPU/trap
/// plumbing work. It has no notion of memory-mapped regions, protection,
/// or sparse allocation; guest address `0` maps to byte `0` of the
/// backing `Vec`.
pub struct FlatMemory {
    bytes: Vec<u8>,
    /// Count of guest-CPU bus reads and writes, for the CLI's
    /// `--clock-mhz` report (see [`FlatMemory::access_counts`]).
    reads: u64,
    writes: u64,
    /// The optional sanitizer shadow map -- see this module's doc.
    /// Boxed so the common (disabled) case doesn't pay for
    /// [`ShadowMap`]'s own `Vec`/`HashMap` fields inline in every
    /// `FlatMemory`, and so `FlatMemory` stays cheap to move around
    /// (e.g. into a nested [`crate::dispatch::Runtime`]).
    shadow: Option<Box<ShadowMap>>,
    /// Store-intercepting ports (`--uae-debug`/`--emu68-debug`), empty by default; see
    /// [`StorePort`]. The disabled cost is one `is_empty` check per guest
    /// store.
    ports: Vec<Box<dyn StorePort>>,
    /// The modeled instruction cache (`--cache`, issue #148), if
    /// [`Self::enable_icache`] was called -- see [`IcacheModel`]'s doc.
    /// `None` (the default) preserves this runtime's pre-issue-#148
    /// behavior exactly: every instruction fetch reports as cached,
    /// matching [`m68k::AddressBus::last_fetch_was_cached`]'s own
    /// "cached by default" contract.
    icache: Option<IcacheModel>,
    /// Whether the most recent instruction-stream fetch hit
    /// [`Self::icache`] -- backs
    /// [`m68k::AddressBus::last_fetch_was_cached`] (via
    /// [`Self::icache_last_fetch_was_cached`]). `true` whenever
    /// `icache` is `None`, matching that method's documented default.
    icache_last_fetch_cached: bool,
    /// AND-accumulator over every instruction-stream fetch since the
    /// last [`Self::icache_begin_instruction_fetches`] call -- backs
    /// [`m68k::AddressBus::instruction_fetches_were_cached`] (via
    /// [`Self::icache_fetches_were_cached`]), consulted only by the
    /// 68020/68030 timing tables; see that method's doc.
    icache_fetches_cached_since_begin: bool,
}

impl std::fmt::Debug for FlatMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlatMemory")
            .field("len", &self.bytes.len())
            .field("sanitizer", &self.shadow.is_some())
            .field("store_ports", &self.ports.len())
            .field("icache_lines", &self.icache.as_ref().map(|c| c.lines.len()))
            .finish_non_exhaustive()
    }
}

impl FlatMemory {
    /// Creates a new zero-initialized [`FlatMemory`] of `size` bytes,
    /// with the sanitizer disabled (see [`Self::enable_sanitizer`]) and
    /// no instruction cache modeled (see [`Self::enable_icache`]).
    pub fn new(size: usize) -> Self {
        Self {
            bytes: vec![0u8; size],
            reads: 0,
            writes: 0,
            shadow: None,
            ports: Vec::new(),
            icache: None,
            icache_last_fetch_cached: true,
            icache_fetches_cached_since_begin: true,
        }
    }

    /// Installs a [`StorePort`]; from now on guest CPU stores it claims
    /// are diverted to it, and [`m68k::AddressBus::fast_mem`] returns
    /// `None` (the JIT's raw-pointer stores would bypass the port --
    /// the same reason `--sanitize` disables it).
    pub fn add_store_port(&mut self, port: Box<dyn StorePort>) {
        self.ports.push(port);
    }

    /// Whether any [`StorePort`] is installed.
    pub fn has_store_ports(&self) -> bool {
        !self.ports.is_empty()
    }

    /// Offers a guest CPU store to the installed ports; `true` if one
    /// claimed (and consumed) it. Called only from the
    /// [`m68k::AddressBus`] impl.
    #[inline]
    pub(crate) fn intercept_store(&mut self, addr: u32, value: u32, size: StoreSize) -> bool {
        if self.ports.is_empty() {
            return false;
        }
        self.intercept_store_slow(addr, value, size)
    }

    fn intercept_store_slow(&mut self, addr: u32, value: u32, size: StoreSize) -> bool {
        let Some(idx) = self.ports.iter().position(|p| p.claims_store(addr, size)) else {
            return false;
        };
        // Taken out so the port can read the rest of `self` (it is handed
        // `&FlatMemory`); put back before returning.
        let mut ports = std::mem::take(&mut self.ports);
        ports[idx].store(&*self, addr, value, size);
        self.ports = ports;
        true
    }

    /// Returns a read-only view of the backing bytes.
    /// Guest-CPU bus accesses so far, as `(reads, writes)`.
    ///
    /// Counted in [`m68k::AddressBus`]'s impl (`crate::backend`), *not*
    /// in the [`crate::cpu::AddressSpace`] methods, which is the
    /// distinction that makes the number meaningful: this runtime's own
    /// native library handlers reach guest memory through `AddressSpace`
    /// directly, so their traffic is excluded and what remains is what
    /// the emulated CPU itself put on the bus.
    ///
    /// Only complete under `--clock-mhz`. On the default execution path
    /// the trace JIT reads and writes through the raw `fast_mem` pointer,
    /// bypassing the `AddressBus` methods entirely, so these counts see
    /// an arbitrary fraction of the real traffic -- the same reason no
    /// cycle count exists there either. The CLI reports them only
    /// alongside a cycle count, never on their own.
    pub fn access_counts(&self) -> (u64, u64) {
        (self.reads, self.writes)
    }

    /// Records one guest-CPU bus read. Called only from the
    /// [`m68k::AddressBus`] impl -- see [`FlatMemory::access_counts`].
    #[inline]
    pub(crate) fn count_read(&mut self) {
        self.reads = self.reads.wrapping_add(1);
    }

    /// Records one guest-CPU bus write. See [`FlatMemory::count_read`].
    #[inline]
    pub(crate) fn count_write(&mut self) {
        self.writes = self.writes.wrapping_add(1);
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns a mutable view of the backing bytes.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Reads a big-endian 16-bit value **without** consulting the
    /// sanitizer shadow map, returning `0` for an out-of-range address
    /// exactly as [`AddressSpace::read_u16`] does.
    ///
    /// This exists for the sanitizer's own use, and using it anywhere
    /// else would silently create a hole in the checking. The run loop
    /// has to look at the opcode word at `PC` before every instruction
    /// (to classify calls and returns for the shadow call stack, see
    /// `crate::m68kops`) and at the return-address slot the stack
    /// pointer points to. Those are the sanitizer *inspecting* guest
    /// memory on its own behalf, not the guest accessing it: routing
    /// them through the checked path would attribute the sanitizer's own
    /// reads to the guest -- inventing violations that never happened
    /// and, worse, healing `Uninit` bytes the guest has not actually
    /// written yet, which would mask real bugs.
    pub fn peek_u16(&self, addr: u32) -> u16 {
        let addr = addr as usize;
        match self.bytes.get(addr..addr + 2) {
            Some(slice) => u16::from_be_bytes(slice.try_into().unwrap()),
            None => 0,
        }
    }

    /// Reads a big-endian 32-bit value without consulting the shadow
    /// map -- see [`Self::peek_u16`] for why this exists and why it must
    /// not be used outside the sanitizer.
    pub fn peek_u32(&self, addr: u32) -> u32 {
        let addr = addr as usize;
        match self.bytes.get(addr..addr + 4) {
            Some(slice) => u32::from_be_bytes(slice.try_into().unwrap()),
            None => 0,
        }
    }

    /// Installs a fresh [`ShadowMap`] sized to this memory's length, so
    /// every subsequent access is checked against it -- see this
    /// module's doc and the CLI's `--sanitize` flag.
    ///
    /// It's harmless to call this before or after
    /// [`crate::loader::load`] populates guest memory: the shadow map's
    /// default state is [`crate::sanitize::ShadowState::Valid`] (see
    /// that type's doc for why), so the program image being written
    /// through [`AddressSpace`] either before or after this call is
    /// checked against `Valid` bytes either way and never flags a
    /// violation. Enabling it right after construction (as the CLI
    /// does) is simply the more obvious place to put the call, not a
    /// correctness requirement.
    ///
    /// Also see [`crate::backend`]'s `AddressBus::fast_mem` impl: once
    /// this is called, `fast_mem` starts returning `None`, which is
    /// what forces the m68k crate's trace JIT off the raw-pointer fast
    /// path that would otherwise bypass every check installed here.
    pub fn enable_sanitizer(&mut self) {
        self.shadow = Some(Box::new(ShadowMap::new(self.bytes.len())));
    }

    /// The installed [`ShadowMap`], if [`Self::enable_sanitizer`] was
    /// called.
    pub fn shadow(&self) -> Option<&ShadowMap> {
        self.shadow.as_deref()
    }

    /// Mutable access to the installed [`ShadowMap`], if any -- for
    /// callers that mark ranges valid/uninit/unaddressable (heap
    /// allocators, stack-pointer tracking, ...) or that publish the
    /// current PC (the CPU run loop, see [`ShadowMap::set_current_pc`]'s
    /// doc).
    pub fn shadow_mut(&mut self) -> Option<&mut ShadowMap> {
        self.shadow.as_deref_mut()
    }

    /// Installs a modeled instruction cache (`--cache`, issue #148) of
    /// `size_bytes` -- see [`IcacheModel`]'s doc for the model itself
    /// and the CLI's `--cache` flag for how `size_bytes` is chosen per
    /// `--cpu`. Harmless to call before or after
    /// [`crate::loader::load`] populates memory, same reasoning as
    /// [`Self::enable_sanitizer`]'s doc: the model tracks addresses
    /// only, never cached *contents*, so there's nothing for load
    /// order to get wrong.
    pub fn enable_icache(&mut self, size_bytes: u32) {
        self.icache = Some(IcacheModel::new(size_bytes));
    }

    /// Records one instruction-stream fetch of `len` bytes at `addr`,
    /// updating [`Self::icache_last_fetch_was_cached`]'s state and
    /// ANDing into [`Self::icache_fetches_were_cached`]'s
    /// accumulator. Called only from `crate::backend`'s `AddressBus::
    /// read_immediate_word`/`read_immediate_long`. Always a hit when
    /// [`Self::icache`] is `None`, so `--cache`'s absence costs
    /// nothing and changes nothing.
    pub(crate) fn icache_note_instruction_fetch(&mut self, addr: u32, len: u32) {
        let hit = match &mut self.icache {
            Some(icache) => icache.access_span(addr, len),
            None => true,
        };
        self.icache_last_fetch_cached = hit;
        self.icache_fetches_cached_since_begin &= hit;
    }

    /// See [`m68k::AddressBus::begin_instruction_fetches`].
    pub(crate) fn icache_begin_instruction_fetches(&mut self) {
        self.icache_fetches_cached_since_begin = true;
    }

    /// See [`m68k::AddressBus::last_fetch_was_cached`].
    pub(crate) fn icache_last_fetch_was_cached(&self) -> bool {
        self.icache_last_fetch_cached
    }

    /// See [`m68k::AddressBus::instruction_fetches_were_cached`].
    pub(crate) fn icache_fetches_were_cached(&self) -> bool {
        self.icache_fetches_cached_since_begin
    }
}

impl AddressSpace for FlatMemory {
    fn bus_access_counts(&self) -> (u64, u64) {
        self.access_counts()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn invalidate_icache(&mut self) {
        if let Some(icache) = &mut self.icache {
            icache.invalidate_all();
        }
    }

    fn read_u8(&self, addr: u32) -> u8 {
        if let Some(shadow) = &self.shadow {
            shadow.check_read(addr, 1);
        }
        self.bytes.get(addr as usize).copied().unwrap_or(0)
    }

    fn clear_fresh_block(&mut self, addr: u32, len: u32) {
        if let Some(shadow) = &mut self.shadow {
            shadow.mark_valid(addr, len);
        }
    }

    fn write_u8(&mut self, addr: u32, value: u8) {
        if let Some(shadow) = &mut self.shadow {
            shadow.check_write(addr, 1);
        }
        if let Some(slot) = self.bytes.get_mut(addr as usize) {
            *slot = value;
        }
    }

    // Overridden (rather than relying on the default trait methods'
    // byte-by-byte composition) so the common in-range case is a single
    // bounds check plus a native big-endian load/store, instead of two or
    // four separate bounds-checked `read_u8`/`write_u8` calls. This is a
    // hot path: every instruction fetch and most operand reads/writes go
    // through here.
    //
    // Each override below still checks/updates the sanitizer shadow map
    // (when installed) for *every* byte of the access, not just the
    // first -- a 2- or 4-byte access whose last byte lands in a
    // redzone is just as much a violation as one whose first byte
    // does, and delegating to `read_u8`/`write_u8` here (which would
    // get that for free) is exactly the fast path these overrides
    // exist to avoid. The shadow check itself is one `Option` check
    // per *access* (not per byte) before the byte-granular loop inside
    // `ShadowMap::check_read`/`check_write` -- see `memory.rs`'s module
    // doc.

    fn read_u16(&self, addr: u32) -> u16 {
        if let Some(shadow) = &self.shadow {
            shadow.check_read(addr, 2);
        }
        let addr = addr as usize;
        match self.bytes.get(addr..addr + 2) {
            Some(slice) => u16::from_be_bytes(slice.try_into().unwrap()),
            None => {
                let hi = self.bytes.get(addr).copied().unwrap_or(0) as u16;
                let lo = self.bytes.get(addr + 1).copied().unwrap_or(0) as u16;
                (hi << 8) | lo
            }
        }
    }

    fn write_u16(&mut self, addr: u32, value: u16) {
        if let Some(shadow) = &mut self.shadow {
            shadow.check_write(addr, 2);
        }
        let addr = addr as usize;
        match self.bytes.get_mut(addr..addr + 2) {
            Some(slice) => slice.copy_from_slice(&value.to_be_bytes()),
            None => {
                if let Some(slot) = self.bytes.get_mut(addr) {
                    *slot = (value >> 8) as u8;
                }
                if let Some(slot) = self.bytes.get_mut(addr + 1) {
                    *slot = value as u8;
                }
            }
        }
    }

    fn read_u32(&self, addr: u32) -> u32 {
        if let Some(shadow) = &self.shadow {
            shadow.check_read(addr, 4);
        }
        let addr = addr as usize;
        match self.bytes.get(addr..addr + 4) {
            Some(slice) => u32::from_be_bytes(slice.try_into().unwrap()),
            None => {
                let mut value = 0u32;
                for i in 0..4 {
                    let byte = self.bytes.get(addr + i).copied().unwrap_or(0);
                    value = (value << 8) | u32::from(byte);
                }
                value
            }
        }
    }

    fn write_u32(&mut self, addr: u32, value: u32) {
        if let Some(shadow) = &mut self.shadow {
            shadow.check_write(addr, 4);
        }
        let addr = addr as usize;
        match self.bytes.get_mut(addr..addr + 4) {
            Some(slice) => slice.copy_from_slice(&value.to_be_bytes()),
            None => {
                for (i, byte) in value.to_be_bytes().into_iter().enumerate() {
                    if let Some(slot) = self.bytes.get_mut(addr + i) {
                        *slot = byte;
                    }
                }
            }
        }
    }

    // These two forward to the inherent methods of the same name above.
    // Both spellings exist deliberately: the inherent ones keep working
    // for code holding a concrete `FlatMemory` without importing the
    // trait, while these make the shadow map reachable from the handler
    // code that is generic over `C::Memory: AddressSpace` (see the trait
    // methods' own docs). Inherent methods win name resolution on a
    // concrete `FlatMemory`, so the two can never disagree.

    fn shadow(&self) -> Option<&ShadowMap> {
        self.shadow.as_deref()
    }

    fn shadow_mut(&mut self) -> Option<&mut ShadowMap> {
        self.shadow.as_deref_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitize::PoisonReason;

    #[test]
    fn peek_bypasses_the_shadow_map_entirely() {
        // The sanitizer's own inspection of guest memory (opcode words,
        // return-address slots) must not be attributed to the guest --
        // see peek_u16's doc. Poison a range, peek it, and confirm
        // nothing was recorded and no Uninit byte was healed.
        let mut mem = FlatMemory::new(64);
        mem.enable_sanitizer();
        let shadow = mem.shadow_mut().unwrap();
        shadow.mark_unaddressable(0, 8, PoisonReason::Redzone);
        shadow.mark_uninit(8, 8);

        assert_eq!(mem.peek_u16(0), 0);
        assert_eq!(mem.peek_u32(8), 0);

        let shadow = mem.shadow().unwrap();
        assert_eq!(
            shadow.violation_count(),
            0,
            "peeking must never record a violation"
        );
        assert_eq!(
            shadow.state(8),
            crate::sanitize::ShadowState::Uninit,
            "peeking must not heal Uninit bytes the guest hasn't written"
        );
    }

    #[test]
    fn without_enable_sanitizer_shadow_is_none_and_fast_mem_stays_available() {
        let mem = FlatMemory::new(16);
        assert!(mem.shadow().is_none());
    }

    #[test]
    fn enable_sanitizer_installs_a_shadow_map_sized_to_memory_len() {
        let mut mem = FlatMemory::new(16);
        mem.enable_sanitizer();
        assert_eq!(mem.shadow().unwrap().len(), 16);
    }

    #[test]
    fn one_byte_redzone_read_through_flatmemory_is_a_violation() {
        let mut mem = FlatMemory::new(16);
        mem.enable_sanitizer();
        mem.shadow_mut()
            .unwrap()
            .mark_unaddressable(4, 1, PoisonReason::Redzone);

        let _ = mem.read_u8(4);

        assert_eq!(mem.shadow().unwrap().violation_count(), 1);
    }

    #[test]
    fn four_byte_write_straddling_into_a_redzone_is_a_violation_through_flatmemory() {
        let mut mem = FlatMemory::new(16);
        mem.enable_sanitizer();
        // Bytes 6,7 valid; 8,9 poisoned. A 4-byte write at addr 6 covers
        // both -- must be caught even though the access started clean.
        mem.shadow_mut()
            .unwrap()
            .mark_unaddressable(8, 2, PoisonReason::Redzone);

        mem.write_u32(6, 0xAABB_CCDD);

        // One violation, not one per straddled byte -- see
        // ShadowMap::check_write's doc on reporting once per access.
        assert_eq!(mem.shadow().unwrap().violation_count(), 1);
        // The write still proceeds (detector, not enforcer) -- see
        // crate::sanitize's module doc.
        assert_eq!(mem.read_u8(6), 0xAA);
        assert_eq!(mem.read_u8(7), 0xBB);
    }

    #[test]
    fn write_promotes_uninit_to_valid_through_flatmemory() {
        let mut mem = FlatMemory::new(16);
        mem.enable_sanitizer();
        mem.shadow_mut().unwrap().mark_uninit(0, 4);

        mem.write_u16(0, 0x1234);

        assert_eq!(
            mem.shadow().unwrap().state(0),
            crate::sanitize::ShadowState::Valid
        );
        assert_eq!(
            mem.shadow().unwrap().state(1),
            crate::sanitize::ShadowState::Valid
        );
        // Bytes 2,3 weren't part of the write, still Uninit.
        assert_eq!(
            mem.shadow().unwrap().state(2),
            crate::sanitize::ShadowState::Uninit
        );
    }

    #[test]
    fn u16_roundtrip_is_big_endian() {
        let mut mem = FlatMemory::new(16);
        mem.write_u16(0, 0x1234);
        // Big-endian: high byte first.
        assert_eq!(mem.read_u8(0), 0x12);
        assert_eq!(mem.read_u8(1), 0x34);
        assert_eq!(mem.read_u16(0), 0x1234);
    }

    #[test]
    fn u32_roundtrip_is_big_endian() {
        let mut mem = FlatMemory::new(16);
        mem.write_u32(4, 0xDEAD_BEEF);
        assert_eq!(mem.read_u8(4), 0xDE);
        assert_eq!(mem.read_u8(5), 0xAD);
        assert_eq!(mem.read_u8(6), 0xBE);
        assert_eq!(mem.read_u8(7), 0xEF);
        assert_eq!(mem.read_u32(4), 0xDEAD_BEEF);
    }

    #[test]
    fn out_of_range_read_returns_zero() {
        let mem = FlatMemory::new(4);
        assert_eq!(mem.read_u8(100), 0);
        assert_eq!(mem.read_u16(100), 0);
        assert_eq!(mem.read_u32(100), 0);
    }

    #[test]
    fn out_of_range_write_is_ignored() {
        let mut mem = FlatMemory::new(4);
        mem.write_u8(100, 0xFF);
        mem.write_u32(1000, 0xFFFF_FFFF);
        assert_eq!(mem.as_slice(), &[0, 0, 0, 0]);
    }

    #[test]
    fn straddling_out_of_range_write_partially_applies() {
        // A multi-byte access that starts in range but crosses the end
        // writes the in-range bytes and drops the rest.
        let mut mem = FlatMemory::new(4);
        mem.write_u32(2, 0xAABB_CCDD);
        assert_eq!(mem.read_u8(2), 0xAA);
        assert_eq!(mem.read_u8(3), 0xBB);
        // Bytes 4 and 5 don't exist; reading them back gives 0.
        assert_eq!(mem.read_u8(4), 0);
    }

    // --- IcacheModel / --cache (issue #148) ---

    #[test]
    fn without_enable_icache_every_fetch_is_a_free_hit() {
        // The pre-issue-#148 default, preserved: no model installed at
        // all means `icache_note_instruction_fetch` always reports a
        // hit, matching `m68k::AddressBus::last_fetch_was_cached`'s own
        // documented "cached by default" contract.
        let mut mem = FlatMemory::new(64);
        mem.icache_note_instruction_fetch(0, 2);
        assert!(mem.icache_last_fetch_was_cached());
        mem.icache_note_instruction_fetch(1000, 2);
        assert!(mem.icache_last_fetch_was_cached());
    }

    #[test]
    fn enable_icache_first_fetch_of_a_line_is_a_miss_second_is_a_hit() {
        let mut mem = FlatMemory::new(64);
        mem.enable_icache(16); // 4 lines of 4 bytes each

        mem.icache_note_instruction_fetch(0, 2);
        assert!(!mem.icache_last_fetch_was_cached(), "cold line");

        mem.icache_note_instruction_fetch(0, 2);
        assert!(
            mem.icache_last_fetch_was_cached(),
            "same line, now resident"
        );
    }

    #[test]
    fn enable_icache_direct_mapped_aliasing_evicts_the_other_line() {
        // A 16-byte cache (4 lines) direct-mapped by (line base / 4) %
        // 4: addresses 0 and 16 both land in index 0, so touching 16
        // evicts 0's line, and touching 0 again is a fresh miss.
        let mut mem = FlatMemory::new(64);
        mem.enable_icache(16);

        mem.icache_note_instruction_fetch(0, 2);
        mem.icache_note_instruction_fetch(0, 2);
        assert!(mem.icache_last_fetch_was_cached(), "warmed up");

        mem.icache_note_instruction_fetch(16, 2);
        assert!(!mem.icache_last_fetch_was_cached(), "aliases index 0, cold");

        mem.icache_note_instruction_fetch(0, 2);
        assert!(
            !mem.icache_last_fetch_was_cached(),
            "evicted by the access to 16"
        );
    }

    #[test]
    fn enable_icache_a_fetch_spanning_two_lines_hits_only_if_both_are_resident() {
        // A 4-byte fetch at a merely 2-byte-aligned address straddles
        // two 4-byte lines (here: lines at 0 and 4). `access` fills a
        // line as a side effect of even a missed access (real cache
        // behavior: a miss still ends with the line resident), so this
        // warms exactly one line by hand before ever touching the
        // straddling range, to keep the two lines' residency apart.
        let mut mem = FlatMemory::new(64);
        mem.enable_icache(4096); // plenty of lines, no aliasing in play

        // Warm only the first line (address 0..4), leaving the second
        // (4..8) untouched.
        mem.icache_note_instruction_fetch(0, 2);
        assert!(!mem.icache_last_fetch_was_cached(), "cold start");

        mem.icache_note_instruction_fetch(2, 4);
        assert!(
            !mem.icache_last_fetch_was_cached(),
            "first line resident, second still cold -- a miss overall"
        );
        // That access also filled the second line as a side effect of
        // the miss (same as any real cache), so a repeat is now a hit.
        mem.icache_note_instruction_fetch(2, 4);
        assert!(
            mem.icache_last_fetch_was_cached(),
            "both lines now resident"
        );
    }

    #[test]
    fn icache_begin_instruction_fetches_resets_the_and_accumulator() {
        let mut mem = FlatMemory::new(64);
        mem.enable_icache(16);

        mem.icache_begin_instruction_fetches();
        mem.icache_note_instruction_fetch(0, 2); // cold -> miss
        assert!(
            !mem.icache_fetches_were_cached(),
            "one miss in the sequence taints the whole instruction"
        );

        // A later instruction, now that address 0 is resident, should
        // start a fresh (true) accumulator rather than carrying the
        // earlier miss forward.
        mem.icache_begin_instruction_fetches();
        mem.icache_note_instruction_fetch(0, 2); // now a hit
        assert!(mem.icache_fetches_were_cached());
    }

    #[test]
    fn icache_fetches_were_cached_defaults_to_last_fetch_was_cached_without_a_begin_call() {
        // Matches `m68k::AddressBus::instruction_fetches_were_cached`'s
        // own documented default -- callers that never call
        // `begin_instruction_fetches` still get a sensible answer.
        let mut mem = FlatMemory::new(64);
        mem.enable_icache(16);
        mem.icache_note_instruction_fetch(0, 2);
        assert_eq!(
            mem.icache_fetches_were_cached(),
            mem.icache_last_fetch_was_cached()
        );
    }

    #[test]
    fn invalidate_icache_undoes_every_prior_fetch() {
        let mut mem = FlatMemory::new(64);
        mem.enable_icache(16);
        mem.icache_note_instruction_fetch(0, 2);
        mem.icache_note_instruction_fetch(0, 2);
        assert!(mem.icache_last_fetch_was_cached(), "warmed up");

        AddressSpace::invalidate_icache(&mut mem);

        mem.icache_note_instruction_fetch(0, 2);
        assert!(
            !mem.icache_last_fetch_was_cached(),
            "invalidate_icache must evict everything"
        );
    }

    #[test]
    fn invalidate_icache_without_enable_icache_is_a_harmless_no_op() {
        let mut mem = FlatMemory::new(64);
        AddressSpace::invalidate_icache(&mut mem); // must not panic
        mem.icache_note_instruction_fetch(0, 2);
        assert!(
            mem.icache_last_fetch_was_cached(),
            "still the free-hit default"
        );
    }
}
