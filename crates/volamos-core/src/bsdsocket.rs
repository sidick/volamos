//! `bsdsocket.library`: real TCP/UDP networking via a **host-socket
//! passthrough** -- guest `socket()`/`connect()`/`send()`/`recv()`/...
//! calls translate directly onto real host OS sockets (the `socket2`
//! crate), not an embedded guest-visible TCP/IP stack. See
//! `crates/dispatch.rs`'s [`crate::dispatch::Runtime::enable_bsdsocket`]
//! and the originating design research (GitHub issue) for the full
//! rationale; the short version: Copperline (the cycle-exact Amiga
//! emulator this project uses for ground-truth testing) has a real,
//! conformance-tested `bsdsocket.library` implementation with exactly
//! two transports -- an embedded `smoltcp` stack (needed only because
//! *it* requires cycle-exact determinism) and a host-socket passthrough
//! backend (`transport = "host"`, its "Amiberry-style" option). volamos
//! has no determinism constraint, so the host-socket shape -- not the
//! embedded-stack one -- is the directly applicable model, and this
//! module follows Copperline's own host-side primitives closely (real
//! non-blocking `socket2::Socket`s, a plain handle table, the same
//! `sock_open`/`sock_connect`/`sock_send`/`sock_recv`/... shape) minus
//! the WASM linear-memory-copy boundary Copperline's plugin architecture
//! needs and this runtime doesn't (LVO handlers already have direct
//! `&mut dyn AddressSpace` access).
//!
//! # Opt-in, not always-on
//!
//! Unlike `locale.library`/`intuition.library`/`mathffp.library` (real,
//! always-registered libraries this runtime treats as ROM-resident,
//! matching real KS/WB 3.1), `bsdsocket.library` is **not** part of this
//! project's KS/WB 3.1 baseline -- on real hardware it only exists if a
//! third-party TCP/IP stack (AmiTCP/Miami/Genesis/Roadshow) is installed,
//! and giving guest code real host network access is a meaningfully
//! different trust boundary than local filesystem access (which this
//! runtime already gates behind explicit `-V`/`-a` opt-in). So this
//! library is only registered when [`crate::dispatch::Runtime::
//! enable_bsdsocket`] is called explicitly (wired to a CLI flag), never
//! unconditionally from [`crate::dispatch::Runtime::new`] -- without
//! that opt-in, `OpenLibrary("bsdsocket.library", 0)` behaves exactly
//! like it does today (not found, matching a real bare KS/WB 3.1
//! system with no TCP stack installed).
//!
//! # Scope: a first, real slice, not the whole LVO surface
//!
//! Implements `socket`/`bind`/`listen`/`accept`/`connect`/`send`/`recv`/
//! `sendto`/`recvfrom`/`shutdown`/`getsockname`/`getpeername`/
//! `CloseSocket`/`getdtablesize`/`Errno`/`SetErrnoPtr`/`Inet_NtoA`/
//! `inet_addr`/`gethostbyname`/`IoctlSocket`/`setsockopt`/`getsockopt`/
//! `SocketBaseTagList`/`WaitSelect`/`Dup2Socket`/`vsyslog` -- real outbound and inbound TCP,
//! plus UDP, real error reporting through the documented `Errno()`/
//! `SetErrnoPtr()` mechanism, real forward DNS lookups via the host's
//! own resolver (see "DNS: a real, blocking host lookup" below), the
//! socket option set a real conformance suite (`bsdsocktest`'s own
//! `sockopt` test category) actually exercises: `SO_REUSEADDR`/
//! `SO_KEEPALIVE`/`SO_LINGER`/`SO_SNDBUF`/`SO_RCVBUF`/`SO_SNDTIMEO`/
//! `SO_RCVTIMEO`/`SO_ERROR`/`SO_TYPE`/`TCP_NODELAY` -- all real
//! `socket2` calls against the host socket, not roundtrip-only storage
//! (`SO_ERROR` in particular has real BSD "read consumes the pending
//! error" semantics, via `socket2`'s own `take_error`), and real
//! `select()`-shaped multiplexing via `WaitSelect` (Unix-only -- see
//! "WaitSelect: real poll(2), not a busy-loop" below). Deliberately
//! **not yet implemented** (calling these traps as an ordinary
//! unknown-call, same as any other library's unimplemented LVO in this
//! codebase -- see [`crate::lvos::bsdsocket`]'s module docs for why
//! this table only lists what's implemented, not the full ABI):
//! `SetSocketSignals` (a simpler, older sibling of `WaitSelect`'s own
//! signal-mask parameter -- no corpus binary or conformance test needs
//! it yet), `gethostbyaddr` (reverse/PTR lookup -- `std::net` has no
//! portable reverse-DNS primitive; Copperline's own `hostsocket-plugin`
//! hit the identical wall and stayed a stub for the same reason, see that
//! crate's module docs), `sendmsg`/`recvmsg`/`GetSocketEvents`.
//! `ObtainSocket`/`ReleaseSocket`/`ReleaseCopyOfSocket` *are* registered
//! (so a caller that unconditionally tries them doesn't crash the whole
//! run) but always honestly fail with `EOPNOTSUPP` -- see
//! [`release_socket_handler`]'s doc comment for why a real handoff isn't
//! possible here. `IoctlSocket` itself only implements `FIONBIO`
//! (see "Blocking by default" below) -- `FIONREAD`/`FIOASYNC` are
//! unimplemented too (no portable `socket2` equivalent for either).
//!
//! # Blocking by default
//!
//! Every socket [`socket_handler`]/[`accept_handler`] hand back starts
//! in the OS default **blocking** mode -- the real BSD/AmigaOS default
//! -- not forced non-blocking. This module's first slice got this
//! backwards (every socket was unconditionally
//! `set_nonblocking(true)`'d, with no way to opt out at all): found
//! running a real, unmodified consumer (MicroPython's Amiga port,
//! `ports/amiga/modsocket.c`), whose own `connect()` wrapper has no
//! `EINPROGRESS` handling whatsoever -- it assumes a synchronous
//! `connect()`, and only ever asks for non-blocking mode explicitly via
//! `IoctlSocket(fd, FIONBIO, ...)` (gated behind Python's
//! `settimeout()`/`setblocking()`, which a caller that never touches
//! either never triggers). Forcing every socket non-blocking regardless
//! made every simple, default-mode socket program's very first
//! `connect()` fail with a spurious `EINPROGRESS` it had no way to
//! recover from. [`ioctl_socket_handler`] implements the one `IoctlSocket`
//! command (`FIONBIO`) real socket code actually needs to opt into
//! non-blocking mode explicitly; a blocking `connect`/`accept`/`send`/
//! `recv` on a socket that hasn't asked for that just blocks the host
//! thread until the real I/O completes -- the same "trust the host,
//! real blocking calls are fine" posture `gethostbyname` already uses,
//! not a shortcut.
//!
//! # `WaitSelect`: real `poll(2)`, not a busy-loop
//!
//! [`wait_select_handler`] translates a real AmigaOS `WaitSelect` call
//! (BSD `select()` plus an Exec signal mask -- a real Roadshow Autodoc,
//! see that function's own doc for the exact contract) directly onto a
//! real `libc::poll(2)` call against the underlying host file
//! descriptors (`socket2::Socket::as_raw_fd`) -- Unix only (`#[cfg(unix)]`;
//! this runtime's own Windows support is unverified at runtime already,
//! see `docs/plan.md`'s notes, so `WaitSelect` on Windows returns
//! `EOPNOTSUPP` rather than a guess). This is the same "trust the host"
//! posture every other blocking call in this module already takes
//! (`connect`, `gethostbyname`, ...): the host kernel's own `poll(2)`
//! already solves "is this fd ready" correctly for every socket state
//! this backend can produce (a listening socket's `POLLIN` meaning "a
//! connection is pending `accept()`", a connecting socket's `POLLOUT`
//! meaning "the handshake finished", ...), so there's no reason to
//! reimplement any of that by hand.
//!
//! **The `signals` parameter, and why this runtime doesn't need to poll
//! for it mid-wait**: real `WaitSelect` also races the file-descriptor
//! wait against an Exec signal mask, waking on whichever happens first.
//! Because this runtime is single-tasking and non-preemptive -- no other
//! guest code can run concurrently to `Signal()` the waiting task while
//! `WaitSelect` is blocked inside a single host `poll(2)` call -- a
//! signal can only ever already be pending *before* the call starts,
//! never arrive *during* it. So [`wait_select_handler`] checks
//! `tc_SigRecvd` against the requested mask exactly once, up front:
//! matching real semantics either way ("no socket ready" + "a signal was
//! already pending" `=>` return `0` without ever calling `poll(2)` at
//! all) but skipping the "wake immediately if a signal arrives mid-wait"
//! case, since that case is structurally unreachable in this runtime's
//! own task model -- not a missing feature, just a state this
//! architecture can't produce. `crate::exectask::TC_SIGRECVD` is the
//! same field `SetSignal`/`Wait`/`Signal` already maintain.
//!
//! # Errno: real BSD numbering, not the host's own
//!
//! `Errno()`/`SetErrnoPtr()` report the fixed BSD `errno` numbering
//! `bsdsocket.library` itself documents (`<sys/errno.h>` from a real
//! Roadshow NDK) -- not whatever the *host* OS's own errno numbering
//! happens to be (which differs across macOS/Linux/Windows). [`translate_errno`]
//! maps a [`std::io::Error`] to this fixed numbering via
//! [`std::io::ErrorKind`] (the common socket-relevant cases only, same
//! set Copperline's own translator falls back to when a raw host errno
//! doesn't have a direct mapping) -- not by inspecting the host's raw OS
//! errno at all, a deliberate simplification versus Copperline's more
//! thorough platform-specific table; expand this if a real corpus binary
//! needs a specific errno this doesn't yet produce.
//!
//! # DNS: a real, blocking host lookup
//!
//! `gethostbyname` resolves via `(name, 0u16).to_socket_addrs()` --
//! `std::net`'s standard, portable way to invoke the host OS's own
//! resolver (`getaddrinfo` under the hood), the same "trust the host" spirit
//! as everything else in this module. Unlike Copperline's own
//! `hostsocket-plugin` (which needs a `resolve_start`/`resolve_poll`
//! background-thread-plus-non-blocking-poll dance specifically because a
//! WASM plugin can't block its host's emulation thread), this runtime has
//! no such constraint -- a library call handler already runs synchronously
//! on the guest's own call boundary, so a plain blocking call here is
//! consistent with how every other host I/O in this codebase already
//! works (`Open`, `Seek`, ...), not a shortcut.
//!
//! Only `AF_INET` (IPv4) results are surfaced, matching this module's
//! `AF_INET`-only scope elsewhere; a name that resolves to IPv6-only
//! addresses reports as not found. Real `<netdb.h>` documents lookup
//! failures via a separate `extern int h_errno` global "left" by
//! `gethostbyname`/`gethostbyaddr` -- but no LVO in the real
//! `bsdsocket_lib.fd` ever sets one via a registered pointer the way
//! `SetErrnoPtr` does for plain `errno`, meaning on real AmigaOS this
//! value is read back through the *same* `Errno()` channel this module
//! already implements (a real, if slightly surprising, historical
//! AmigaOS quirk, not a simplification here) -- so a failed
//! `gethostbyname` reports one of `<netdb.h>`'s own `HOST_NOT_FOUND`/
//! `TRY_AGAIN`/`NO_DATA` codes through [`BsdSocketState::set_errno`],
//! exactly like every other failing call in this module.
//!
//! The returned `struct hostent` (real Roadshow `<netdb.h>` layout: five
//! 4-byte fields, `h_name`/`h_aliases`/`h_addrtype`/`h_length`/
//! `h_addr_list`, 20 bytes) and everything it points to live on the
//! guest heap and are rebuilt fresh on every call -- real
//! `gethostbyname`'s own documented contract is a reused, overwritten-
//! by-the-next-call static buffer, not a caller-freed allocation (there
//! is no `FreeHostent`-style LVO in the real ABI), so
//! [`BsdSocketState::hostent_allocs`] frees the *previous* call's
//! blocks at the start of the next one rather than ever handing
//! ownership to the guest.
//!
//! # SnoopDos-style logging: `connect`/`accept` set `call_detail`
//!
//! [`connect_handler`]/[`accept_handler`] set [`HandlerContext::
//! call_detail`] on every *meaningful* outcome (a completed or
//! in-progress outbound connection, an accepted inbound one, or a real
//! failure), printed by the CLI's `-s`/`--snoop` flag exactly like every
//! other resource-opening call already does (`OpenLibrary`/`Open`, see
//! [`crate::dispatch::open_library_common`]'s doc) -- no separate
//! logging mechanism, just this module joining the existing convention.
//! `accept`'s `EAGAIN` ("nothing waiting yet") is deliberately *not*
//! logged: it's the overwhelmingly common outcome of a guest's own
//! accept-loop polling, and logging it would be noise, not a real event
//! -- the same "meaningful events only" posture `--snoop`'s own module
//! doc already promises.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, ToSocketAddrs};

use socket2::{Domain, SockAddr, Socket, Type};

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, HandlerContext, LibraryTable};
use crate::exectask;
use crate::lvos::bsdsocket::BSDSOCKET_LVOS;
use crate::memory::AddressSpace;

// --- Fixed BSD errno numbering (bsdsocket.library's own, not the host's) ---

pub const EBADF: i32 = 9;
pub const EACCES: i32 = 13;
pub const EFAULT: i32 = 14;
pub const EINVAL: i32 = 22;
pub const EMFILE: i32 = 24;
pub const EAGAIN: i32 = 35;
pub const EINPROGRESS: i32 = 36;
pub const EALREADY: i32 = 37;
pub const ENOTSOCK: i32 = 38;
pub const EOPNOTSUPP: i32 = 45;
pub const EADDRINUSE: i32 = 48;
pub const ENETUNREACH: i32 = 51;
pub const ECONNABORTED: i32 = 53;
pub const ECONNRESET: i32 = 54;
pub const ENOTCONN: i32 = 57;
pub const ETIMEDOUT: i32 = 60;
pub const ECONNREFUSED: i32 = 61;
pub const EIO_: i32 = 5;

// --- <netdb.h>'s h_errno codes -- see the module docs' "DNS" section
// for why these travel through the same Errno()/SetErrnoPtr() channel
// as ordinary errno values instead of a separate mechanism.
pub const HOST_NOT_FOUND: i32 = 1;
pub const TRY_AGAIN: i32 = 2;

/// `FIONBIO` -- `_IOW('f', 126, LONG)` per a real Roadshow
/// `<sys/filio.h>`/`<sys/ioccom.h>` (re-derived: `IOC_IN (0x80000000) |
/// (sizeof(LONG)=4 << 16) | ('f'=0x66 << 8) | 126`), the one
/// `IoctlSocket` command this backend implements -- see
/// [`ioctl_socket_handler`].
const FIONBIO: u32 = 0x8004_667E;

// --- SocketBaseTagList tag encoding -- a real `<libraries/bsdsocket.h>`
// (Roadshow) / `<utility/tagitem.h>` (`TAG_USER`), not invented. See
// [`socket_base_tag_list_handler`]'s doc for the bit layout these
// combine into.
const TAG_DONE: u32 = 0;
const TAG_IGNORE: u32 = 1;
const TAG_MORE: u32 = 2;
const TAG_SKIP: u32 = 3;
const TAG_USER: u32 = 0x8000_0000;
const SBTF_REF: u32 = 0x8000;
const SBTF_SET: u32 = 1;
const SBTC_ERRNOLONGPTR: u32 = 24;
const SBTC_HERRNOLONGPTR: u32 = 25;
const SBTC_RELEASESTRPTR: u32 = 29;

/// Maps a [`std::io::Error`] from a `socket2` call to the fixed BSD
/// `errno` numbering `bsdsocket.library` documents -- see the module
/// docs' "Errno" section for why this doesn't inspect the host's raw OS
/// errno.
fn translate_errno(e: &std::io::Error) -> i32 {
    use std::io::ErrorKind::*;
    // NOT is_connect_in_progress(e) here: that check treats any
    // WouldBlock-kind error as "connect still in progress" (a heuristic
    // only valid for connect() itself, whose non-blocking "not done yet"
    // signal can surface as either ErrorKind -- see that function's own
    // doc), but accept()/send()/recv()'s own genuine EAGAIN/EWOULDBLOCK
    // is *also* WouldBlock-kind. Calling it here made every one of
    // those wrongly report EINPROGRESS instead of EAGAIN -- a real
    // regression this module briefly shipped, caught by a real
    // conformance suite (bsdsocktest's "accept(): EWOULDBLOCK when
    // non-blocking, no pending" test) rather than by this module's own
    // (all-synthetic, until then) test coverage. connect_handler already
    // checks is_connect_in_progress itself, in its own dedicated match
    // arm, before ever falling through to this function -- it doesn't
    // need this function to also know about it.
    match e.kind() {
        WouldBlock => EAGAIN,
        ConnectionRefused => ECONNREFUSED,
        ConnectionReset | ConnectionAborted => ECONNRESET,
        NotConnected => ENOTCONN,
        TimedOut => ETIMEDOUT,
        AddrInUse => EADDRINUSE,
        PermissionDenied => EACCES,
        InvalidInput => EINVAL,
        _ => EIO_,
    }
}

/// Domain/type constants `socket()` accepts -- `<sys/socket.h>`'s real
/// values, and the only ones this backend supports (AF_INET/SOCK_STREAM/
/// SOCK_DGRAM), matching Copperline's own host-socket backend's Phase 1
/// scope.
const AF_INET: i32 = 2;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;

// --- setsockopt/getsockopt levels and option names -- real
// <sys/socket.h>/<netinet/tcp.h> values from a real Roadshow NDK, not
// invented. See [`setsockopt_handler`]/[`getsockopt_handler`].
const SOL_SOCKET: i32 = 0xFFFF;
const IPPROTO_TCP: i32 = 6;
const SO_REUSEADDR: i32 = 0x0004;
const SO_KEEPALIVE: i32 = 0x0008;
const SO_LINGER: i32 = 0x0080;
const SO_SNDBUF: i32 = 0x1001;
const SO_RCVBUF: i32 = 0x1002;
const SO_SNDTIMEO: i32 = 0x1005;
const SO_RCVTIMEO: i32 = 0x1006;
const SO_ERROR: i32 = 0x1007;
const SO_TYPE: i32 = 0x1008;
const TCP_NODELAY: i32 = 0x01;

/// A cap on concurrently open sockets -- a plain sanity bound against
/// runaway guest socket creation, same reasoning (and the same value) as
/// Copperline's own `MAX_OPEN_HOST_SOCKETS`.
const MAX_OPEN_SOCKETS: usize = 64;

/// One open socket: the real host `Socket` plus enough bookkeeping to
/// answer `getsockname`/`getpeername`/`accept` without extra host calls
/// where `socket2` doesn't already provide them cheaply.
struct SocketEntry {
    socket: Socket,
}

/// Host-side `bsdsocket.library` state: the open-socket handle table and
/// the `Errno()`/`SetErrnoPtr()` bookkeeping every call updates. Lives on
/// [`crate::dispatch::Runtime`] as its own top-level field (like
/// [`crate::dosfile::DosState`]), not nested under `DosState` -- no
/// natural coupling to `dos.library`'s own state.
pub struct BsdSocketState {
    sockets: HashMap<i32, SocketEntry>,
    next_id: i32,
    /// The last error code any handler here set -- what `Errno()`
    /// returns.
    last_errno: i32,
    /// Guest address `SetErrnoPtr` asked to also mirror `last_errno`
    /// into on every update (real `bsdsocket.library`'s way of keeping a
    /// C runtime's global `errno` variable in sync without a per-call
    /// round trip) -- `None` until a guest calls `SetErrnoPtr`. Only the
    /// 4-byte (`LONG`) size is supported; see [`Self::set_errno`].
    errno_ptr: Option<u32>,
    /// Guest address [`SocketBaseTagList`]'s `SBTC_HERRNOLONGPTR` tag (or
    /// [`set_herrno_ptr_handler`]-equivalent) asked to mirror `h_errno`
    /// into -- the *separate* variable real `gethostbyname`/
    /// `gethostbyaddr` failures report through (see the module docs'
    /// "DNS" section: this runtime originally, wrongly, folded DNS
    /// failures into the same channel as [`Self::errno_ptr`]; real
    /// `bsdsocket.library` keeps them distinct, confirmed against a real
    /// conformance suite -- `bsdsocktest`'s own `testutil.c` registers
    /// both pointers separately and reads them back independently).
    /// `None` until a guest registers one.
    ///
    /// [`SocketBaseTagList`]: socket_base_tag_list_handler
    herrno_ptr: Option<u32>,
    /// Guest heap address of a small scratch buffer [`inet_ntoa_handler`]
    /// reuses across calls for its dotted-decimal string result
    /// (matching real `Inet_NtoA`'s own "static buffer, valid until the
    /// next call" contract) -- allocated lazily on first use.
    ntoa_buf: Option<u32>,
    /// Every guest-heap allocation [`gethostbyname_handler`] built for
    /// the *previous* successful call -- the `struct hostent`, the name
    /// copy, the aliases array, the address-pointer array, and each
    /// address block -- freed at the start of the next call rather than
    /// ever being freed by the guest (see the module docs' "DNS"
    /// section for why). Empty until the first successful lookup.
    hostent_allocs: Vec<u32>,
    /// Guest heap address of a small, lazily-allocated buffer holding
    /// this library's version/release identifier string, returned by
    /// `SocketBaseTagList`'s `SBTC_RELEASESTRPTR` query -- see
    /// [`socket_base_tag_list_handler`].
    release_str_buf: Option<u32>,
}

impl BsdSocketState {
    pub fn new() -> Self {
        Self {
            sockets: HashMap::new(),
            next_id: 1,
            last_errno: 0,
            errno_ptr: None,
            herrno_ptr: None,
            ntoa_buf: None,
            hostent_allocs: Vec::new(),
            release_str_buf: None,
        }
    }

    /// Records `code` as the current `Errno()` value, and mirrors it into
    /// guest memory at [`Self::errno_ptr`] if one was set via
    /// `SetErrnoPtr`/`SocketBaseTagList(SBTC_ERRNOLONGPTR)`. Called by
    /// every fallible handler on both success (`code = 0`) and failure,
    /// matching real `bsdsocket.library`'s own "every call sets errno,
    /// not just failing ones" behavior. Does *not* touch
    /// [`Self::herrno_ptr`] -- see [`Self::set_herrno`].
    fn set_errno(&mut self, mem: &mut dyn AddressSpace, code: i32) {
        self.last_errno = code;
        if let Some(ptr) = self.errno_ptr {
            mem.write_u32(ptr, code as u32);
        }
    }

    /// Mirrors `code` into guest memory at [`Self::herrno_ptr`] if one
    /// was registered via `SocketBaseTagList(SBTC_HERRNOLONGPTR)` -- the
    /// separate `h_errno` channel real `gethostbyname`/`gethostbyaddr`
    /// failures report through, distinct from [`Self::set_errno`] (see
    /// the module docs' "DNS" section). A no-op if no pointer was ever
    /// registered -- unlike `errno`, real `h_errno` has no host-visible
    /// "current value" this runtime tracks independently of a caller's
    /// own registered mirror, since nothing here reads it back the way
    /// `Errno()` does for plain errno.
    fn set_herrno(&mut self, mem: &mut dyn AddressSpace, code: i32) {
        if let Some(ptr) = self.herrno_ptr {
            mem.write_u32(ptr, code as u32);
        }
    }
}

impl Default for BsdSocketState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BsdSocketState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BsdSocketState")
            .field("open_sockets", &self.sockets.len())
            .field("last_errno", &self.last_errno)
            .finish()
    }
}

// --- struct sockaddr_in <-> guest memory ---
//
// Real layout (a real Roadshow NDK's <sys/socket.h>/<netinet/in.h>,
// 4.3BSD-style *with* sin_len, 16 bytes total): sin_len(1) +
// sin_family(1) + sin_port(2, network/big-endian) + sin_addr(4,
// network/big-endian) + sin_zero[8] (padding).

const SOCKADDR_IN_SIZE: u32 = 16;

fn read_sockaddr_in(mem: &dyn AddressSpace, addr: u32) -> SocketAddrV4 {
    let port = mem.read_u16(addr.wrapping_add(2));
    let ip = mem.read_u32(addr.wrapping_add(4));
    SocketAddrV4::new(Ipv4Addr::from(ip), port)
}

fn write_sockaddr_in(mem: &mut dyn AddressSpace, addr: u32, sa: SocketAddrV4) {
    mem.write_u8(addr, SOCKADDR_IN_SIZE as u8);
    mem.write_u8(addr.wrapping_add(1), AF_INET as u8);
    mem.write_u16(addr.wrapping_add(2), sa.port());
    mem.write_u32(addr.wrapping_add(4), u32::from(*sa.ip()));
    for i in 0..8 {
        mem.write_u8(addr.wrapping_add(8 + i), 0);
    }
}

/// Casts a `&mut [u8]` to `&mut [MaybeUninit<u8>]` for `socket2`'s
/// `recv_from`, which (like the rest of the `std`/`socket2` ecosystem's
/// newer, uninitialized-memory-aware I/O APIs) wants a buffer it hasn't
/// been proven safe to assume is initialized. Safe here specifically
/// because `u8` has no invalid bit patterns -- any byte value is a valid
/// `u8`, so treating an already-initialized `&mut [u8]` as `&mut
/// [MaybeUninit<u8>]` can't observe uninitialized memory through it (the
/// bytes already ARE initialized; this cast just relaxes the type-level
/// guarantee `socket2`'s signature asks for, it doesn't weaken what's
/// actually in memory) -- the same reasoning `socket2`'s own
/// documentation gives for this exact pattern.
fn as_uninit_slice(buf: &mut [u8]) -> &mut [std::mem::MaybeUninit<u8>] {
    unsafe { &mut *(buf as *mut [u8] as *mut [std::mem::MaybeUninit<u8>]) }
}

// --- LVO handlers ---

/// `socket(domain, type, protocol)`. `D0` = a new socket fd, or `-1`
/// with `Errno()` set. Only `AF_INET`/`SOCK_STREAM`/`SOCK_DGRAM` are
/// supported (see the module docs); anything else is `EOPNOTSUPP`.
fn socket_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let domain = ctx.cpu.data_register(DataRegister(0)) as i32;
    let type_ = ctx.cpu.data_register(DataRegister(1)) as i32;

    let fail = |ctx: &mut HandlerContext<'_, C>, code: i32| {
        ctx.bsdsocket.set_errno(ctx.mem, code);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
    };

    if ctx.bsdsocket.sockets.len() >= MAX_OPEN_SOCKETS {
        fail(ctx, EMFILE);
        return Ok(());
    }
    if domain != AF_INET {
        fail(ctx, EOPNOTSUPP);
        return Ok(());
    }
    let sock_type = match type_ {
        SOCK_STREAM => Type::STREAM,
        SOCK_DGRAM => Type::DGRAM,
        _ => {
            fail(ctx, EOPNOTSUPP);
            return Ok(());
        }
    };
    let socket = match Socket::new(Domain::IPV4, sock_type, None) {
        Ok(s) => s,
        Err(e) => {
            let code = translate_errno(&e);
            fail(ctx, code);
            return Ok(());
        }
    };
    // Blocking by default -- the real BSD/AmigaOS default (see the
    // module docs' "Blocking by default" section). Non-blocking mode is
    // only entered via an explicit IoctlSocket(fd, FIONBIO, ...) call.

    let id = ctx.bsdsocket.next_id;
    ctx.bsdsocket.next_id = ctx.bsdsocket.next_id.wrapping_add(1).max(1);
    ctx.bsdsocket.sockets.insert(id, SocketEntry { socket });
    ctx.bsdsocket.set_errno(ctx.mem, 0);
    ctx.cpu.set_data_register(DataRegister(0), id as u32);
    Ok(())
}

/// Shared "look up `fd` or fail with `EBADF`" used by every handler
/// below that needs an existing socket. `f` only takes the `Socket`
/// itself (every caller already reads whatever it needs from `ctx`
/// beforehand into plain local values -- a guest-memory buffer, a
/// parsed `sockaddr_in` -- so `f`'s body never needs `ctx` at all,
/// keeping this a plain, unremarkable disjoint-borrow: this call
/// borrows only `ctx.bsdsocket.sockets`, which ends before the caller
/// touches `ctx.mem`/`ctx.cpu` again for the result).
fn with_socket<R>(
    ctx: &mut HandlerContext<'_, impl Cpu>,
    fd: i32,
    f: impl FnOnce(&mut Socket) -> R,
) -> Option<R> {
    match ctx.bsdsocket.sockets.get_mut(&fd) {
        Some(entry) => Some(f(&mut entry.socket)),
        None => {
            ctx.bsdsocket.set_errno(ctx.mem, EBADF);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
            None
        }
    }
}

/// `bind(sock, name, namelen)`. `D0` = `0`, or `-1` with `Errno()` set.
fn bind_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    let sa = read_sockaddr_in(ctx.mem, name_ptr);

    let Some(result) = with_socket(ctx, fd, |socket| {
        socket.bind(&SockAddr::from(std::net::SocketAddr::V4(sa)))
    }) else {
        return Ok(());
    };
    match result {
        Ok(()) => {
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `listen(sock, backlog)`. `D0` = `0`, or `-1` with `Errno()` set.
fn listen_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let backlog = ctx.cpu.data_register(DataRegister(1)) as i32;

    let Some(result) = with_socket(ctx, fd, |socket| socket.listen(backlog.max(1))) else {
        return Ok(());
    };
    match result {
        Ok(()) => {
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `accept(sock, addr, addrlen)`. `D0` = a new fd for the accepted
/// connection, or `-1` with `Errno()` set (`EAGAIN` when nothing is
/// waiting, on a listening socket the guest explicitly put into
/// non-blocking mode via `IoctlSocket(fd, FIONBIO, ...)` -- see the
/// module docs' "Blocking by default" section; a plain blocking `accept`
/// just blocks the host thread until a connection arrives, matching real
/// semantics). The accepted socket itself is left in the OS default
/// blocking mode too (real BSD `accept()` never forces non-blocking on
/// the accepted socket regardless of the listener's own mode).
/// `addr`/`addrlen`: if `addr` is non-`NULL`, the peer's address is
/// written there.
fn accept_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let addr_ptr = ctx.cpu.address_register(AddressRegister(0));

    if ctx.bsdsocket.sockets.len() >= MAX_OPEN_SOCKETS {
        ctx.bsdsocket.set_errno(ctx.mem, EMFILE);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        return Ok(());
    }

    let Some(result) = with_socket(ctx, fd, |socket| socket.accept()) else {
        return Ok(());
    };
    match result {
        Ok((accepted, peer)) => {
            if addr_ptr != 0
                && let Some(peer) = peer.as_socket_ipv4()
            {
                write_sockaddr_in(ctx.mem, addr_ptr, peer);
            }
            let id = ctx.bsdsocket.next_id;
            ctx.bsdsocket.next_id = ctx.bsdsocket.next_id.wrapping_add(1).max(1);
            ctx.bsdsocket
                .sockets
                .insert(id, SocketEntry { socket: accepted });
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), id as u32);
            if let Some(peer) = peer.as_socket_ipv4() {
                *ctx.call_detail = Some(format!("accept <- {peer} -> ok (fd {id}, was fd {fd})"));
            }
        }
        // EAGAIN ("nothing waiting yet") is the overwhelmingly common
        // outcome of a guest's own accept-loop polling -- not logged,
        // same "meaningful events only" posture the module docs'
        // SnoopDos-style logging already has for everything else (see
        // crate::dispatch's OpenLibrary/Open call_detail precedent).
        Err(e) if translate_errno(&e) == EAGAIN => {
            ctx.bsdsocket.set_errno(ctx.mem, EAGAIN);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
            *ctx.call_detail = Some(format!("accept (fd {fd}) -> failed (errno {code})"));
        }
    }
    Ok(())
}

/// `connect(sock, name, namelen)`. `D0` = `0` on an immediate connect
/// (rare for TCP), or `-1` with `Errno()` set to `EINPROGRESS` (the
/// non-blocking connect is under way -- a real caller retries `connect()`
/// to observe completion, exactly like a real non-blocking BSD socket;
/// see [`crate::bsdsocket`]'s module docs on why there's no separate
/// blocking mode) or a real failure code.
fn connect_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    let sa = read_sockaddr_in(ctx.mem, name_ptr);

    let Some(result) = with_socket(ctx, fd, |socket| {
        socket.connect(&SockAddr::from(std::net::SocketAddr::V4(sa)))
    }) else {
        return Ok(());
    };
    match result {
        Ok(()) => {
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), 0);
            *ctx.call_detail = Some(format!("connect -> {sa} (fd {fd}) -> ok"));
        }
        Err(e) if is_connect_in_progress(&e) => {
            ctx.bsdsocket.set_errno(ctx.mem, EINPROGRESS);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
            *ctx.call_detail = Some(format!("connect -> {sa} (fd {fd}) -> in progress"));
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
            *ctx.call_detail = Some(format!(
                "connect -> {sa} (fd {fd}) -> failed (errno {code})"
            ));
        }
    }
    Ok(())
}

/// Whether a non-blocking `connect()`'s error just means "still in
/// progress" (`EINPROGRESS` on Unix, `WSAEWOULDBLOCK`/`WSAEINPROGRESS`
/// on Windows) -- checked against the *raw* OS error code, not
/// `std::io::Error::kind()`: this runtime once wrongly assumed
/// `ErrorKind::WouldBlock` covered it, but empirically (a real
/// non-blocking loopback `connect()` during development)
/// `EINPROGRESS`'s kind is `ErrorKind::InProgress` -- a *different*,
/// still-unstable-to-match `std` variant on this toolchain (rustc
/// 1.97), not `WouldBlock` at all. `WouldBlock` is still checked too, in
/// case some platform reports it that way instead.
fn is_connect_in_progress(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    match e.raw_os_error() {
        #[cfg(unix)]
        Some(code) if code == libc::EINPROGRESS => true,
        // WSAEWOULDBLOCK / WSAEINPROGRESS -- stable, well-known Windows
        // Sockets error codes; hardcoded rather than pulled from a
        // crate since `libc` doesn't cover WinSock-specific numbering.
        #[cfg(windows)]
        Some(10035) | Some(10036) => true,
        _ => false,
    }
}

/// `send(sock, buf, len, flags)`. `D0` = bytes sent, or `-1` with
/// `Errno()` set (`EAGAIN` if it would block).
fn send_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let buf_ptr = ctx.cpu.address_register(AddressRegister(0));
    let len = ctx.cpu.data_register(DataRegister(1));

    let mut buf = vec![0u8; len as usize];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ctx.mem.read_u8(buf_ptr.wrapping_add(i as u32));
    }

    let Some(result) = with_socket(ctx, fd, |socket| socket.write(&buf)) else {
        return Ok(());
    };
    match result {
        Ok(n) => {
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), n as u32);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `recv(sock, buf, len, flags)`. `D0` = bytes received (`0` at EOF), or
/// `-1` with `Errno()` set (`EAGAIN` if it would block).
fn recv_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let buf_ptr = ctx.cpu.address_register(AddressRegister(0));
    let len = ctx.cpu.data_register(DataRegister(1));

    let mut buf = vec![0u8; len as usize];
    let Some(result) = with_socket(ctx, fd, |socket| socket.read(&mut buf)) else {
        return Ok(());
    };
    match result {
        Ok(n) => {
            for (i, &b) in buf[..n].iter().enumerate() {
                ctx.mem.write_u8(buf_ptr.wrapping_add(i as u32), b);
            }
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), n as u32);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `sendto(sock, buf, len, flags, to, tolen)`. `D0` = bytes sent, or
/// `-1` with `Errno()` set.
fn sendto_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let buf_ptr = ctx.cpu.address_register(AddressRegister(0));
    let len = ctx.cpu.data_register(DataRegister(1));
    let to_ptr = ctx.cpu.address_register(AddressRegister(1));
    let to = read_sockaddr_in(ctx.mem, to_ptr);

    let mut buf = vec![0u8; len as usize];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ctx.mem.read_u8(buf_ptr.wrapping_add(i as u32));
    }

    let Some(result) = with_socket(ctx, fd, |socket| {
        socket.send_to(&buf, &SockAddr::from(std::net::SocketAddr::V4(to)))
    }) else {
        return Ok(());
    };
    match result {
        Ok(n) => {
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), n as u32);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `recvfrom(sock, buf, len, flags, addr, addrlen)`. `D0` = bytes
/// received, or `-1` with `Errno()` set. `addr`: if non-`NULL`, the
/// sender's address is written there.
fn recvfrom_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let buf_ptr = ctx.cpu.address_register(AddressRegister(0));
    let len = ctx.cpu.data_register(DataRegister(1));
    let addr_ptr = ctx.cpu.address_register(AddressRegister(1));

    let mut buf = vec![0u8; len as usize];
    let Some(result) = with_socket(ctx, fd, |socket| {
        socket.recv_from(as_uninit_slice(&mut buf))
    }) else {
        return Ok(());
    };
    match result {
        Ok((n, from)) => {
            for (i, &b) in buf[..n].iter().enumerate() {
                ctx.mem.write_u8(buf_ptr.wrapping_add(i as u32), b);
            }
            if addr_ptr != 0
                && let Some(from) = from.as_socket_ipv4()
            {
                write_sockaddr_in(ctx.mem, addr_ptr, from);
            }
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), n as u32);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// Reads a real AmigaOS `struct timeval` (`<devices/timer.h>`'s
/// `tv_secs`/`tv_micro` naming, 4 bytes each -- see the module docs'
/// "Blocking by default" section's sibling note on this same AmigaOS-
/// vs-POSIX field-naming gotcha for `WaitSelect`) at `addr` as a
/// [`std::time::Duration`].
fn read_timeval(mem: &dyn AddressSpace, addr: u32) -> std::time::Duration {
    let secs = mem.read_u32(addr);
    let micros = mem.read_u32(addr.wrapping_add(4));
    std::time::Duration::new(secs as u64, micros.wrapping_mul(1000))
}

/// Writes a [`std::time::Duration`] (or `None`, meaning "no timeout",
/// written as all-zero) as a real AmigaOS `struct timeval` at `addr`.
fn write_timeval(mem: &mut dyn AddressSpace, addr: u32, d: Option<std::time::Duration>) {
    let d = d.unwrap_or_default();
    mem.write_u32(addr, d.as_secs() as u32);
    mem.write_u32(addr.wrapping_add(4), d.subsec_micros());
}

/// `setsockopt(sock, level, optname, optval, optlen)`. `D0` = `0`, or
/// `-1` with `Errno()` set (`ENOPROTOOPT`-shaped as `EOPNOTSUPP` for an
/// option this backend doesn't implement -- see the module docs'
/// "setsockopt/getsockopt" section for the exact set covered, chosen to
/// match what a real conformance suite's own `sockopt` test category
/// actually exercises). `optval` is read from guest memory *before* the
/// underlying `socket2` call, matching every other handler's "read
/// whatever's needed from `ctx.mem` first, since `with_socket`'s
/// closure only gets the `Socket` itself" convention.
fn setsockopt_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let level = ctx.cpu.data_register(DataRegister(1)) as i32;
    let optname = ctx.cpu.data_register(DataRegister(2)) as i32;
    let optval_ptr = ctx.cpu.address_register(AddressRegister(0));

    enum Apply {
        Bool(bool),
        Size(usize),
        Linger(Option<std::time::Duration>),
        RecvTimeout(Option<std::time::Duration>),
        SendTimeout(Option<std::time::Duration>),
        Nodelay(bool),
        Unsupported,
    }

    let apply = match (level, optname) {
        (SOL_SOCKET, SO_REUSEADDR) => Apply::Bool(ctx.mem.read_u32(optval_ptr) != 0),
        (SOL_SOCKET, SO_KEEPALIVE) => Apply::Bool(ctx.mem.read_u32(optval_ptr) != 0),
        (SOL_SOCKET, SO_SNDBUF) => Apply::Size(ctx.mem.read_u32(optval_ptr) as usize),
        (SOL_SOCKET, SO_RCVBUF) => Apply::Size(ctx.mem.read_u32(optval_ptr) as usize),
        (SOL_SOCKET, SO_LINGER) => {
            let onoff = ctx.mem.read_u32(optval_ptr) != 0;
            let secs = ctx.mem.read_u32(optval_ptr.wrapping_add(4));
            Apply::Linger(onoff.then(|| std::time::Duration::from_secs(secs as u64)))
        }
        (SOL_SOCKET, SO_RCVTIMEO) => {
            let d = read_timeval(ctx.mem, optval_ptr);
            Apply::RecvTimeout((!d.is_zero()).then_some(d))
        }
        (SOL_SOCKET, SO_SNDTIMEO) => {
            let d = read_timeval(ctx.mem, optval_ptr);
            Apply::SendTimeout((!d.is_zero()).then_some(d))
        }
        (IPPROTO_TCP, TCP_NODELAY) => Apply::Nodelay(ctx.mem.read_u32(optval_ptr) != 0),
        _ => Apply::Unsupported,
    };
    if matches!(apply, Apply::Unsupported) {
        ctx.bsdsocket.set_errno(ctx.mem, EOPNOTSUPP);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        return Ok(());
    }

    let Some(result) = with_socket(ctx, fd, |socket| match apply {
        Apply::Bool(v) if optname == SO_REUSEADDR => socket.set_reuse_address(v),
        Apply::Bool(v) => socket.set_keepalive(v),
        Apply::Size(v) if optname == SO_SNDBUF => socket.set_send_buffer_size(v),
        Apply::Size(v) => socket.set_recv_buffer_size(v),
        Apply::Linger(v) => socket.set_linger(v),
        Apply::RecvTimeout(v) => socket.set_read_timeout(v),
        Apply::SendTimeout(v) => socket.set_write_timeout(v),
        Apply::Nodelay(v) => socket.set_nodelay(v),
        Apply::Unsupported => unreachable!("filtered out above"),
    }) else {
        return Ok(());
    };
    match result {
        Ok(()) => {
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `getsockopt(sock, level, optname, optval, optlen)`. `D0` = `0`, or
/// `-1` with `Errno()` set. `optlen` (`A1`) is read but never updated
/// (every option this backend implements has a fixed, known size the
/// caller already knows) -- see [`setsockopt_handler`]'s doc for the
/// exact option set and its provenance. `SO_ERROR` is real BSD
/// "read-and-clear" semantics: querying it consumes the socket's
/// pending error (via `socket2`'s own `take_error`), matching a real
/// kernel's own behavior, not just a value roundtrip.
fn getsockopt_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let level = ctx.cpu.data_register(DataRegister(1)) as i32;
    let optname = ctx.cpu.data_register(DataRegister(2)) as i32;
    let optval_ptr = ctx.cpu.address_register(AddressRegister(0));

    if !matches!(
        (level, optname),
        (SOL_SOCKET, SO_REUSEADDR)
            | (SOL_SOCKET, SO_KEEPALIVE)
            | (SOL_SOCKET, SO_SNDBUF)
            | (SOL_SOCKET, SO_RCVBUF)
            | (SOL_SOCKET, SO_LINGER)
            | (SOL_SOCKET, SO_RCVTIMEO)
            | (SOL_SOCKET, SO_SNDTIMEO)
            | (SOL_SOCKET, SO_ERROR)
            | (SOL_SOCKET, SO_TYPE)
            | (IPPROTO_TCP, TCP_NODELAY)
    ) {
        ctx.bsdsocket.set_errno(ctx.mem, EOPNOTSUPP);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        return Ok(());
    }

    let Some(entry) = ctx.bsdsocket.sockets.get(&fd) else {
        ctx.bsdsocket.set_errno(ctx.mem, EBADF);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        return Ok(());
    };
    let socket = &entry.socket;

    let result: std::io::Result<()> = (|| {
        match (level, optname) {
            (SOL_SOCKET, SO_REUSEADDR) => ctx
                .mem
                .write_u32(optval_ptr, socket.reuse_address()? as u32),
            (SOL_SOCKET, SO_KEEPALIVE) => ctx.mem.write_u32(optval_ptr, socket.keepalive()? as u32),
            (SOL_SOCKET, SO_SNDBUF) => ctx
                .mem
                .write_u32(optval_ptr, socket.send_buffer_size()? as u32),
            (SOL_SOCKET, SO_RCVBUF) => ctx
                .mem
                .write_u32(optval_ptr, socket.recv_buffer_size()? as u32),
            (SOL_SOCKET, SO_LINGER) => {
                let linger = socket.linger()?;
                ctx.mem.write_u32(optval_ptr, linger.is_some() as u32);
                ctx.mem.write_u32(
                    optval_ptr.wrapping_add(4),
                    linger.unwrap_or_default().as_secs() as u32,
                );
            }
            (SOL_SOCKET, SO_RCVTIMEO) => write_timeval(ctx.mem, optval_ptr, socket.read_timeout()?),
            (SOL_SOCKET, SO_SNDTIMEO) => {
                write_timeval(ctx.mem, optval_ptr, socket.write_timeout()?)
            }
            (SOL_SOCKET, SO_ERROR) => {
                let code = match socket.take_error()? {
                    Some(e) => translate_errno(&e),
                    None => 0,
                };
                ctx.mem.write_u32(optval_ptr, code as u32);
            }
            (SOL_SOCKET, SO_TYPE) => {
                let t = if socket.r#type()? == Type::DGRAM {
                    SOCK_DGRAM
                } else {
                    SOCK_STREAM
                };
                ctx.mem.write_u32(optval_ptr, t as u32);
            }
            (IPPROTO_TCP, TCP_NODELAY) => ctx.mem.write_u32(optval_ptr, socket.nodelay()? as u32),
            _ => unreachable!("filtered out above"),
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `shutdown(sock, how)`. `D0` = `0`, or `-1` with `Errno()` set.
fn shutdown_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    use std::net::Shutdown;
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let how = ctx.cpu.data_register(DataRegister(1)) as i32;
    let direction = match how {
        0 => Shutdown::Read,
        1 => Shutdown::Write,
        2 => Shutdown::Both,
        _ => {
            ctx.bsdsocket.set_errno(ctx.mem, EINVAL);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
            return Ok(());
        }
    };

    let Some(result) = with_socket(ctx, fd, |socket| socket.shutdown(direction)) else {
        return Ok(());
    };
    match result {
        Ok(()) => {
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `IoctlSocket(sock, req, argp)`. `D0` = `0`, or `-1` with `Errno()`
/// set. Only `FIONBIO` (set/clear non-blocking mode) is implemented --
/// see the module docs' "Blocking by default" section; `argp` points to
/// a `LONG`, `0` = blocking, nonzero = non-blocking. Any other `req` is
/// `EOPNOTSUPP`.
fn ioctl_socket_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let req = ctx.cpu.data_register(DataRegister(1));
    let argp = ctx.cpu.address_register(AddressRegister(0));

    if req != FIONBIO {
        ctx.bsdsocket.set_errno(ctx.mem, EOPNOTSUPP);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        return Ok(());
    }
    let nonblocking = ctx.mem.read_u32(argp) != 0;

    let Some(result) = with_socket(ctx, fd, |socket| socket.set_nonblocking(nonblocking)) else {
        return Ok(());
    };
    match result {
        Ok(()) => {
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `getsockname(sock, name, namelen)`. `D0` = `0`, or `-1` with
/// `Errno()` set.
fn getsockname_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));

    let Some(result) = with_socket(ctx, fd, |socket| socket.local_addr()) else {
        return Ok(());
    };
    match result {
        Ok(addr) if addr.as_socket_ipv4().is_some() => {
            write_sockaddr_in(ctx.mem, name_ptr, addr.as_socket_ipv4().unwrap());
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
        Ok(_) => {
            ctx.bsdsocket.set_errno(ctx.mem, EOPNOTSUPP);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `getpeername(sock, name, namelen)`. `D0` = `0`, or `-1` with
/// `Errno()` set (`ENOTCONN` if the socket isn't connected).
fn getpeername_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));

    let Some(result) = with_socket(ctx, fd, |socket| socket.peer_addr()) else {
        return Ok(());
    };
    match result {
        Ok(addr) if addr.as_socket_ipv4().is_some() => {
            write_sockaddr_in(ctx.mem, name_ptr, addr.as_socket_ipv4().unwrap());
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), 0);
        }
        Ok(_) => {
            ctx.bsdsocket.set_errno(ctx.mem, EOPNOTSUPP);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `CloseSocket(sock)`. `D0` = `0`, or `-1` with `Errno()` = `EBADF` for
/// an unknown fd.
fn close_socket_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    if ctx.bsdsocket.sockets.remove(&fd).is_some() {
        ctx.bsdsocket.set_errno(ctx.mem, 0);
        ctx.cpu.set_data_register(DataRegister(0), 0);
    } else {
        ctx.bsdsocket.set_errno(ctx.mem, EBADF);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
    }
    Ok(())
}

/// `getdtablesize()`. `D0` = [`MAX_OPEN_SOCKETS`] -- the real cap this
/// backend enforces, matching real `getdtablesize`'s "the largest fd
/// value plus one this process could ever have" contract closely enough
/// for a caller sizing an `fd_set`-like structure.
fn getdtablesize_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu
        .set_data_register(DataRegister(0), MAX_OPEN_SOCKETS as u32);
    Ok(())
}

// --- WaitSelect: real fd_set <-> guest memory, real poll(2) ---

/// `sizeof(fd_set)`: `FD_SETSIZE` (256) bits, packed into 32-bit words
/// (`fd_mask`) -- a real `<sys/socket.h>` layout (confirmed against a
/// real Roadshow NDK), 8 longwords = 32 bytes.
const FD_SET_BYTES: u32 = 32;

/// Whether `fd` is a member of the `fd_set` at `addr` (`FD_ISSET`'s own
/// semantics, translated to guest memory).
fn fd_set_test(mem: &dyn AddressSpace, addr: u32, fd: i32) -> bool {
    if !(0..256).contains(&fd) {
        return false;
    }
    let word = mem.read_u32(addr.wrapping_add((fd as u32 / 32) * 4));
    word & (1 << (fd as u32 % 32)) != 0
}

/// Zeroes an `fd_set` at `addr` (`FD_ZERO`).
fn fd_set_zero(mem: &mut dyn AddressSpace, addr: u32) {
    for i in 0..FD_SET_BYTES {
        mem.write_u8(addr.wrapping_add(i), 0);
    }
}

/// Adds `fd` to the `fd_set` at `addr` (`FD_SET`).
fn fd_set_add(mem: &mut dyn AddressSpace, addr: u32, fd: i32) {
    let word_addr = addr.wrapping_add((fd as u32 / 32) * 4);
    let word = mem.read_u32(word_addr);
    mem.write_u32(word_addr, word | (1 << (fd as u32 % 32)));
}

/// `WaitSelect(nfds, readfds, writefds, exceptfds, timeout, signals)`.
/// `D0` = the number of ready descriptors (summed across all three
/// sets -- a descriptor ready for both reading and writing counts
/// twice, matching real `select()`'s own return-value convention), `0`
/// if the timeout elapsed with nothing ready, or `-1` with `Errno()`
/// set (`EINTR` if a requested signal was already pending -- see the
/// module docs' "WaitSelect" section). On error, the descriptor sets
/// are left unmodified, matching the real documented contract; on
/// success, each non-`NULL` set is replaced with the subset of ready
/// descriptors (real `FD_ZERO`-then-`FD_SET` semantics), even if that
/// subset is empty.
///
/// `nfds` (`D0`) is the "highest descriptor plus one" bound real
/// `select()` uses -- descriptors `0..nfds` are examined. `timeout`
/// (`A3`) is a real AmigaOS `struct timeval*` (`NULL` = block
/// indefinitely, `{0,0}` = a zero-wait poll). `signals` (`D1`... wait,
/// `A3`... see the LVO table -- `signals` is actually the register-list
/// oddity here: `D1`, per the real Autodoc's own SYNOPSIS, since Exec
/// signal masks are conventionally `D`-register arguments even this
/// deep into an otherwise `A`-register-heavy call) is a `ULONG*`
/// user-signal mask, in/out: on input, which signals to also wait on;
/// on output, which of those were actually pending (see the module
/// docs for why this runtime only ever checks that once, up front).
fn wait_select_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let nfds = ctx.cpu.data_register(DataRegister(0)) as i32;
    let readfds_ptr = ctx.cpu.address_register(AddressRegister(0));
    let writefds_ptr = ctx.cpu.address_register(AddressRegister(1));
    let exceptfds_ptr = ctx.cpu.address_register(AddressRegister(2));
    let timeout_ptr = ctx.cpu.address_register(AddressRegister(3));
    let signals_ptr = ctx.cpu.data_register(DataRegister(1));

    // The signal-mask check -- see the module docs for why this only
    // ever needs to happen once, before any polling.
    if signals_ptr != 0 {
        let requested = ctx.mem.read_u32(signals_ptr);
        if requested != 0 {
            let recvd = ctx.mem.read_u32(ctx.current_task + exectask::TC_SIGRECVD);
            let matched = recvd & requested;
            if matched != 0 {
                ctx.mem
                    .write_u32(ctx.current_task + exectask::TC_SIGRECVD, recvd & !matched);
                ctx.mem.write_u32(signals_ptr, matched);
                ctx.bsdsocket.set_errno(ctx.mem, 0);
                ctx.cpu.set_data_register(DataRegister(0), 0);
                return Ok(());
            }
        }
    }

    wait_select_poll(
        ctx,
        nfds,
        readfds_ptr,
        writefds_ptr,
        exceptfds_ptr,
        timeout_ptr,
        signals_ptr,
    )
}

#[cfg(unix)]
fn wait_select_poll<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    nfds: i32,
    readfds_ptr: u32,
    writefds_ptr: u32,
    exceptfds_ptr: u32,
    timeout_ptr: u32,
    signals_ptr: u32,
) -> Result<(), DispatchError> {
    use std::os::unix::io::AsRawFd;

    // Which of our own fds are of interest, and in which set(s).
    struct Interest {
        fd: i32,
        read: bool,
        write: bool,
        except: bool,
    }
    let mut interests: Vec<Interest> = Vec::new();
    for fd in 0..nfds {
        if !ctx.bsdsocket.sockets.contains_key(&fd) {
            continue;
        }
        let read = readfds_ptr != 0 && fd_set_test(ctx.mem, readfds_ptr, fd);
        let write = writefds_ptr != 0 && fd_set_test(ctx.mem, writefds_ptr, fd);
        let except = exceptfds_ptr != 0 && fd_set_test(ctx.mem, exceptfds_ptr, fd);
        if read || write || except {
            interests.push(Interest {
                fd,
                read,
                write,
                except,
            });
        }
    }

    let mut pollfds: Vec<libc::pollfd> = interests
        .iter()
        .map(|i| {
            let raw_fd = ctx.bsdsocket.sockets[&i.fd].socket.as_raw_fd();
            let mut events = 0;
            if i.read {
                events |= libc::POLLIN;
            }
            if i.write {
                events |= libc::POLLOUT;
            }
            if i.except {
                events |= libc::POLLPRI;
            }
            libc::pollfd {
                fd: raw_fd,
                events,
                revents: 0,
            }
        })
        .collect();

    let timeout_ms: i32 = if timeout_ptr == 0 {
        -1
    } else {
        let d = read_timeval(ctx.mem, timeout_ptr);
        d.as_millis().min(i32::MAX as u128) as i32
    };

    let ready = unsafe {
        libc::poll(
            pollfds.as_mut_ptr(),
            pollfds.len() as libc::nfds_t,
            timeout_ms,
        )
    };

    if signals_ptr != 0 {
        ctx.mem.write_u32(signals_ptr, 0);
    }

    if ready < 0 {
        let code = translate_errno(&std::io::Error::last_os_error());
        ctx.bsdsocket.set_errno(ctx.mem, code);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        return Ok(());
    }

    if readfds_ptr != 0 {
        fd_set_zero(ctx.mem, readfds_ptr);
    }
    if writefds_ptr != 0 {
        fd_set_zero(ctx.mem, writefds_ptr);
    }
    if exceptfds_ptr != 0 {
        fd_set_zero(ctx.mem, exceptfds_ptr);
    }

    let mut count = 0u32;
    for (interest, pfd) in interests.iter().zip(pollfds.iter()) {
        let revents = pfd.revents;
        if interest.read && revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            fd_set_add(ctx.mem, readfds_ptr, interest.fd);
            count += 1;
        }
        if interest.write && revents & (libc::POLLOUT | libc::POLLERR) != 0 {
            fd_set_add(ctx.mem, writefds_ptr, interest.fd);
            count += 1;
        }
        if interest.except && revents & libc::POLLPRI != 0 {
            fd_set_add(ctx.mem, exceptfds_ptr, interest.fd);
            count += 1;
        }
    }

    ctx.bsdsocket.set_errno(ctx.mem, 0);
    ctx.cpu.set_data_register(DataRegister(0), count);
    Ok(())
}

#[cfg(not(unix))]
fn wait_select_poll<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    _nfds: i32,
    _readfds_ptr: u32,
    _writefds_ptr: u32,
    _exceptfds_ptr: u32,
    _timeout_ptr: u32,
    _signals_ptr: u32,
) -> Result<(), DispatchError> {
    // See the module docs' "WaitSelect" section: this runtime's own
    // Windows support is unverified at runtime, so this honestly
    // reports "not supported" rather than guessing at a WSAPoll-based
    // implementation nobody has run.
    ctx.bsdsocket.set_errno(ctx.mem, EOPNOTSUPP);
    ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
    Ok(())
}

/// `Errno()`. `D0` = the last error code any handler here set (`0` if
/// the last call succeeded, or if no call has been made yet).
fn errno_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu
        .set_data_register(DataRegister(0), ctx.bsdsocket.last_errno as u32);
    Ok(())
}

/// `SetErrnoPtr(errno_ptr, size)`. No return value. Only a 4-byte
/// (`LONG`) mirror is supported -- real `bsdsocket.library` also allows
/// 1/2-byte sizes for very old C runtimes, not needed by anything this
/// runtime targets (see the module docs' KS/WB 3.1 scope).
fn set_errno_ptr_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ptr = ctx.cpu.address_register(AddressRegister(0));
    ctx.bsdsocket.errno_ptr = if ptr == 0 { None } else { Some(ptr) };
    Ok(())
}

/// `Inet_NtoA(ip)`. `D0` = a pointer to a NUL-terminated dotted-decimal
/// string (e.g. `"127.0.0.1"`), valid until the next `Inet_NtoA` call --
/// real `Inet_NtoA`'s own documented contract (a reused static buffer,
/// not a fresh allocation per call).
fn inet_ntoa_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let ip = ctx.cpu.data_register(DataRegister(0));
    let addr = Ipv4Addr::from(ip);
    let text = addr.to_string();

    let buf_addr = match ctx.bsdsocket.ntoa_buf {
        Some(addr) => addr,
        None => {
            // "###.###.###.###\0" -- 16 bytes is enough for any IPv4
            // dotted-decimal string plus its NUL terminator.
            let addr = ctx
                .heap
                .alloc(16)
                .map_err(|e| DispatchError::HandlerFailed {
                    library: "bsdsocket.library".to_string(),
                    lvo: -174,
                    handler_name: "Inet_NtoA".to_string(),
                    message: format!("Inet_NtoA: guest heap allocation failed: {e}"),
                })?;
            ctx.bsdsocket.ntoa_buf = Some(addr);
            addr
        }
    };
    crate::guestmem::write_c_string(ctx.mem, buf_addr, text.as_bytes());
    ctx.cpu.set_data_register(DataRegister(0), buf_addr);
    Ok(())
}

/// `inet_addr(cp)`. `D0` = the parsed IPv4 address as a big-endian
/// `ULONG` (network byte order, matching `struct in_addr`'s own
/// `s_addr`), or `0xFFFFFFFF` (`INADDR_NONE`) if `cp` doesn't parse as a
/// dotted-decimal address.
fn inet_addr_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let cp_ptr = ctx.cpu.address_register(AddressRegister(0));
    let bytes = crate::guestmem::read_c_string(ctx.mem, cp_ptr);
    let text = String::from_utf8_lossy(&bytes);
    let result = match text.parse::<Ipv4Addr>() {
        Ok(addr) => u32::from(addr),
        Err(_) => 0xFFFF_FFFF,
    };
    ctx.cpu.set_data_register(DataRegister(0), result);
    Ok(())
}

/// `gethostbyname(name)`. `D0` = a `struct hostent*` (`AF_INET` results
/// only), or `NULL` with `Errno()` set to a `<netdb.h>` `h_errno` code
/// (`HOST_NOT_FOUND`/`TRY_AGAIN`) -- see the module docs' "DNS" section
/// for the resolution mechanism, the error-reporting quirk, and the
/// reused-buffer lifetime.
fn gethostbyname_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    let name_bytes = crate::guestmem::read_c_string(ctx.mem, name_ptr);
    let name = String::from_utf8_lossy(&name_bytes).into_owned();

    // Free the previous call's blocks before doing anything else -- see
    // BsdSocketState::hostent_allocs' doc.
    for addr in std::mem::take(&mut ctx.bsdsocket.hostent_allocs) {
        let _ = ctx.heap.free(addr);
    }

    // A real gethostbyname failure reports through h_errno, not the
    // ordinary errno channel -- see BsdSocketState::set_herrno's doc.
    let fail = |ctx: &mut HandlerContext<'_, C>, code: i32| {
        ctx.bsdsocket.set_herrno(ctx.mem, code);
        ctx.cpu.set_data_register(DataRegister(0), 0);
    };

    // A real, blocking host resolver call -- see the module docs.
    let addrs: Vec<Ipv4Addr> = match (name.as_str(), 0u16).to_socket_addrs() {
        Ok(iter) => iter
            .filter_map(|sa| match sa {
                std::net::SocketAddr::V4(v4) => Some(*v4.ip()),
                std::net::SocketAddr::V6(_) => None,
            })
            .take(8)
            .collect(),
        Err(_) => Vec::new(),
    };
    if addrs.is_empty() {
        fail(ctx, HOST_NOT_FOUND);
        return Ok(());
    }

    let mut allocs = Vec::new();
    let mut alloc = |ctx: &mut HandlerContext<'_, C>, size: u32| -> Result<u32, DispatchError> {
        let addr = ctx
            .heap
            .alloc(size.max(4))
            .map_err(|e| DispatchError::HandlerFailed {
                library: "bsdsocket.library".to_string(),
                lvo: -210,
                handler_name: "gethostbyname".to_string(),
                message: format!("gethostbyname: guest heap allocation failed: {e}"),
            })?;
        allocs.push(addr);
        Ok(addr)
    };

    let name_buf = alloc(ctx, name_bytes.len() as u32 + 1)?;
    crate::guestmem::write_c_string(ctx.mem, name_buf, &name_bytes);

    let aliases_arr = alloc(ctx, 4)?; // just a NULL terminator: no alias data available
    ctx.mem.write_u32(aliases_arr, 0);

    let mut addr_block_addrs = Vec::with_capacity(addrs.len());
    for ip in &addrs {
        let block = alloc(ctx, 4)?;
        ctx.mem.write_u32(block, u32::from(*ip));
        addr_block_addrs.push(block);
    }

    let addr_list_arr = alloc(ctx, (addrs.len() as u32 + 1) * 4)?;
    for (i, &block) in addr_block_addrs.iter().enumerate() {
        ctx.mem
            .write_u32(addr_list_arr.wrapping_add(i as u32 * 4), block);
    }
    ctx.mem.write_u32(
        addr_list_arr.wrapping_add(addr_block_addrs.len() as u32 * 4),
        0,
    );

    let hostent = alloc(ctx, 20)?;
    ctx.mem.write_u32(hostent, name_buf); // h_name
    ctx.mem.write_u32(hostent.wrapping_add(4), aliases_arr); // h_aliases
    ctx.mem.write_u32(hostent.wrapping_add(8), AF_INET as u32); // h_addrtype
    ctx.mem.write_u32(hostent.wrapping_add(12), 4); // h_length
    ctx.mem.write_u32(hostent.wrapping_add(16), addr_list_arr); // h_addr_list

    ctx.bsdsocket.hostent_allocs = allocs;
    ctx.bsdsocket.set_errno(ctx.mem, 0);
    ctx.cpu.set_data_register(DataRegister(0), hostent);
    Ok(())
}

/// `SocketBaseTagList(tags)`. `D0` = `0` on success, or (real
/// `SocketBaseTagList`'s own documented `RESULT` contract, a real
/// Roadshow Autodoc, not invented) the positive 1-based index of the
/// first tag that genuinely failed -- always `0` here today, since
/// nothing this handler recognizes has a failing case yet (see below).
/// `tags` is a standard AmigaOS `TagItem` array: `{ti_Tag, ti_Data}`
/// pairs, `TAG_DONE`-terminated, with `TAG_IGNORE`/`TAG_MORE`/
/// `TAG_SKIP` control tags honored.
///
/// Only three base tag codes are implemented, matching what a real
/// conformance suite (`bsdsocktest`'s `testutil.c`) actually calls at
/// startup, before running a single test:
///
/// - `SBTC_ERRNOLONGPTR`/`SBTC_HERRNOLONGPTR` (`SET` only, by value or
///   by reference -- see [`BsdSocketState::errno_ptr`]/[`herrno_ptr`]):
///   the modern, tag-based equivalent of `SetErrnoPtr`, but for `errno`
///   and the *separate* `h_errno` channel respectively. `GET` on either
///   is a documented no-op (`ti_Data` left untouched) -- matching real
///   Roadshow's own behavior, which `bsdsocktest`'s bundled
///   `docs/COMPATIBILITY.md` records as a known, harmless deviation
///   ("Roadshow supports SET but not readback of the registered errno
///   pointer"), not a bug to fix.
/// - `SBTC_RELEASESTRPTR` (`GET` by reference only): writes the address
///   of this library's own version-identifier string into `*ti_Data`.
///
/// Every other tag code is silently accepted as a no-op (not a failure)
/// -- the whole point of AmigaOS tag lists is forward/backward
/// tolerance: a caller passing a tag this backend doesn't recognize
/// (an extension a newer/different real stack supports) shouldn't fail
/// the entire call over it, only over a tag that's recognized but
/// genuinely invalid (this backend has none of those yet).
///
/// [`herrno_ptr`]: BsdSocketState::herrno_ptr
fn socket_base_tag_list_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let mut tags_ptr = ctx.cpu.address_register(AddressRegister(0));

    loop {
        let ti_tag = ctx.mem.read_u32(tags_ptr);
        let ti_data = ctx.mem.read_u32(tags_ptr.wrapping_add(4));

        match ti_tag {
            TAG_DONE => break,
            TAG_IGNORE => {
                tags_ptr = tags_ptr.wrapping_add(8);
                continue;
            }
            TAG_MORE => {
                tags_ptr = ti_data;
                continue;
            }
            TAG_SKIP => {
                tags_ptr = tags_ptr.wrapping_add(8).wrapping_add(ti_data * 8);
                continue;
            }
            _ => {}
        }

        if ti_tag & TAG_USER != 0 {
            let code = (ti_tag >> 1) & 0x3FFF;
            let by_ref = ti_tag & SBTF_REF != 0;
            let is_set = ti_tag & SBTF_SET != 0;

            match (code, is_set) {
                (SBTC_ERRNOLONGPTR, true) => {
                    let ptr = if by_ref {
                        ctx.mem.read_u32(ti_data)
                    } else {
                        ti_data
                    };
                    ctx.bsdsocket.errno_ptr = if ptr == 0 { None } else { Some(ptr) };
                }
                (SBTC_HERRNOLONGPTR, true) => {
                    let ptr = if by_ref {
                        ctx.mem.read_u32(ti_data)
                    } else {
                        ti_data
                    };
                    ctx.bsdsocket.herrno_ptr = if ptr == 0 { None } else { Some(ptr) };
                }
                (SBTC_RELEASESTRPTR, false) if by_ref => {
                    let buf = match ctx.bsdsocket.release_str_buf {
                        Some(addr) => addr,
                        None => {
                            let addr =
                                ctx.heap
                                    .alloc(32)
                                    .map_err(|e| DispatchError::HandlerFailed {
                                        library: "bsdsocket.library".to_string(),
                                        lvo: -294,
                                        handler_name: "SocketBaseTagList".to_string(),
                                        message: format!(
                                            "SocketBaseTagList: guest heap allocation failed: {e}"
                                        ),
                                    })?;
                            crate::guestmem::write_c_string(
                                ctx.mem,
                                addr,
                                b"volamos bsdsocket.library (host passthrough)",
                            );
                            ctx.bsdsocket.release_str_buf = Some(addr);
                            addr
                        }
                    };
                    ctx.mem.write_u32(ti_data, buf);
                }
                // Every other recognized-vs-unrecognized combination
                // (a GET on a SET-only tag, or a tag code this backend
                // doesn't implement at all) is a documented or
                // deliberate no-op -- see this function's own doc.
                _ => {}
            }
        }

        tags_ptr = tags_ptr.wrapping_add(8);
    }

    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// Registers this module's `bsdsocket.library` handlers, looked up by
/// name through [`BSDSOCKET_LVOS`]. **Not** called from
/// [`crate::dispatch::Runtime::new`] -- see the module docs' "Opt-in,
/// not always-on" section; call sites go through [`crate::dispatch::
/// Runtime::enable_bsdsocket`] instead.
/// `Dup2Socket(old_socket, new_socket)`. `D0` = the new descriptor
/// number, or `-1` with `Errno()` set (`EBADF`, `old_socket` not open).
/// `new_socket == -1` means "pick any free descriptor" (real `dup()`
/// semantics); a non-negative `new_socket` means "duplicate onto
/// exactly this descriptor number" (real `dup2()` semantics, closing
/// whatever was already open there first). Since this backend's fd
/// numbers are its own namespace (not real host fds -- see the module
/// docs), honoring an arbitrary caller-requested target is always
/// possible, unlike a real `dup2()`'s host-fd-table constraints.
/// Duplicated via [`Socket::try_clone`], a real `dup()` of the
/// underlying host socket -- both descriptors refer to the same
/// underlying connection/buffer afterward, exactly like real
/// `Dup2Socket`.
fn dup2_socket_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let old_fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let new_fd = ctx.cpu.data_register(DataRegister(1)) as i32;

    let Some(entry) = ctx.bsdsocket.sockets.get(&old_fd) else {
        ctx.bsdsocket.set_errno(ctx.mem, EBADF);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        return Ok(());
    };
    let dup = match entry.socket.try_clone() {
        Ok(s) => s,
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
            return Ok(());
        }
    };

    let id = if new_fd < 0 {
        let id = ctx.bsdsocket.next_id;
        ctx.bsdsocket.next_id = ctx.bsdsocket.next_id.wrapping_add(1).max(1);
        id
    } else {
        if new_fd >= ctx.bsdsocket.next_id {
            ctx.bsdsocket.next_id = new_fd.wrapping_add(1).max(1);
        }
        new_fd
    };
    ctx.bsdsocket
        .sockets
        .insert(id, SocketEntry { socket: dup });
    ctx.bsdsocket.set_errno(ctx.mem, 0);
    ctx.cpu.set_data_register(DataRegister(0), id as u32);
    Ok(())
}

/// `ObtainSocket`/`ReleaseSocket`/`ReleaseCopyOfSocket`: real
/// AmigaOS hands a socket between *processes* by a small integer ID
/// (the classic use: a listening daemon passes an accepted connection
/// to a freshly-launched child process, which calls `ObtainSocket` with
/// the ID its parent got back from `ReleaseSocket`/
/// `ReleaseCopyOfSocket`). This runtime models exactly one guest task
/// with no second process to ever hand a socket to, so there is no
/// honest way to implement the handoff itself -- but real callers
/// (confirmed by `bsdsocktest`'s own `transfer` category) probe for
/// support by calling `ReleaseSocket` and gracefully skipping if it
/// fails, so registering these as a clean, immediate `EOPNOTSUPP`
/// (rather than leaving the LVO unregistered, which crashes the whole
/// run on the "unhandled library call" trap) is the correct behavior:
/// a real caller sees "not supported here" and moves on.
fn release_socket_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.bsdsocket.set_errno(ctx.mem, EOPNOTSUPP);
    ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
    Ok(())
}

/// `vsyslog(pri, msg, args)`. No return value. Formats `msg` against
/// the `args` array using the same real `RawDoFmt`-style C-`printf`
/// directive syntax (`%s`/`%d`/`%ld`/`%u`/`%x`/`%c`/`%%`, one 4-byte
/// array slot consumed per directive) that [`crate::execfmt::
/// render_format`] already implements for `dos.library`'s `VPrintf`/
/// `VFPrintf` -- the same "manually-built argument array standing in
/// for varargs" convention `bsdsocktest`'s own `vsyslog` test uses
/// (`ULONG args[1]; args[0] = (ULONG)"test"; vsyslog(pri, "%s", args)`).
/// There is no real host syslog daemon this runtime could forward to,
/// so the rendered message is surfaced the same honest way this
/// module's other "meaningful event" logging already is -- via
/// `call_detail` (visible under `--snoop`/`--verbose`) -- rather than
/// silently discarded.
fn vsyslog_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let pri = ctx.cpu.data_register(DataRegister(0)) as i32;
    let msg_ptr = ctx.cpu.address_register(AddressRegister(0));
    let args_ptr = ctx.cpu.address_register(AddressRegister(1));

    let fmt = crate::guestmem::read_c_string(ctx.mem, msg_ptr);
    let (rendered, _) = crate::execfmt::render_format(ctx.mem, &fmt, args_ptr);
    let text = String::from_utf8_lossy(&rendered);
    *ctx.call_detail = Some(format!("vsyslog(pri={pri}): {text}"));
    Ok(())
}

pub fn register_bsdsocket_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
    base: u32,
) {
    macro_rules! reg {
        ($name:literal, $handler:expr) => {
            table
                .register_by_name(
                    mem,
                    base,
                    BSDSOCKET_LVOS,
                    "bsdsocket.library",
                    $name,
                    $handler,
                )
                .unwrap_or_else(|e| panic!("{} should be in BSDSOCKET_LVOS: {e}", $name));
        };
    }
    reg!("socket", socket_handler::<C>);
    reg!("bind", bind_handler::<C>);
    reg!("listen", listen_handler::<C>);
    reg!("accept", accept_handler::<C>);
    reg!("connect", connect_handler::<C>);
    reg!("send", send_handler::<C>);
    reg!("recv", recv_handler::<C>);
    reg!("sendto", sendto_handler::<C>);
    reg!("recvfrom", recvfrom_handler::<C>);
    reg!("shutdown", shutdown_handler::<C>);
    reg!("setsockopt", setsockopt_handler::<C>);
    reg!("getsockopt", getsockopt_handler::<C>);
    reg!("IoctlSocket", ioctl_socket_handler::<C>);
    reg!("getsockname", getsockname_handler::<C>);
    reg!("getpeername", getpeername_handler::<C>);
    reg!("CloseSocket", close_socket_handler::<C>);
    reg!("getdtablesize", getdtablesize_handler::<C>);
    reg!("WaitSelect", wait_select_handler::<C>);
    reg!("Errno", errno_handler::<C>);
    reg!("SetErrnoPtr", set_errno_ptr_handler::<C>);
    reg!("Inet_NtoA", inet_ntoa_handler::<C>);
    reg!("inet_addr", inet_addr_handler::<C>);
    reg!("gethostbyname", gethostbyname_handler::<C>);
    reg!("SocketBaseTagList", socket_base_tag_list_handler::<C>);
    reg!("Dup2Socket", dup2_socket_handler::<C>);
    reg!("vsyslog", vsyslog_handler::<C>);
    reg!("ObtainSocket", release_socket_handler::<C>);
    reg!("ReleaseSocket", release_socket_handler::<C>);
    reg!("ReleaseCopyOfSocket", release_socket_handler::<C>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{M68kCpu, TRAP_TABLE_END};
    use crate::dispatch::{BSDSOCKET_LIBRARY_BASE, Runtime, StartConfig};
    use crate::memory::FlatMemory;

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
    /// `move.l D0,Dn`.
    fn move_d0_to_d(n: u16) -> u16 {
        0x2000 | (n << 9)
    }
    fn jsr_disp16_a6(disp: i32) -> [u16; 2] {
        [0x4EAE, disp as u16]
    }
    const RTS: u16 = 0x4E75;

    fn movea_bsdsocket_base_to_a6() -> [u16; 3] {
        [
            move_imm_to_a(6),
            (BSDSOCKET_LIBRARY_BASE >> 16) as u16,
            BSDSOCKET_LIBRARY_BASE as u16,
        ]
    }

    /// `move.l #value,D<n>` as three words (opcode + two 32-bit-immediate
    /// extension words).
    fn push_move_imm_d(words: &mut Vec<u16>, n: u16, value: u32) {
        words.push(move_imm_to_d(n));
        words.push((value >> 16) as u16);
        words.push(value as u16);
    }

    /// `movea.l #value,A<n>` as three words.
    fn push_move_imm_a(words: &mut Vec<u16>, n: u16, value: u32) {
        words.push(move_imm_to_a(n));
        words.push((value >> 16) as u16);
        words.push(value as u16);
    }

    fn runtime_with_program(words: &[u16]) -> Runtime<M68kCpu> {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
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
        rt.enable_bsdsocket();
        rt
    }

    #[test]
    fn end_to_end_socket_and_close_round_trip() {
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0); // protocol
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd
        words.push(move_d0_to_d(3)); // D3 = fd (survives the next call)
        words.push(move_d0_to_d(0)); // CloseSocket wants the fd in D0 too
        words.extend_from_slice(&jsr_disp16_a6(-120)); // CloseSocket(fd) -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0,
            "CloseSocket on a freshly-opened socket should succeed"
        );
    }

    #[test]
    fn end_to_end_socket_rejects_unsupported_domain() {
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, 99); // an unsupported domain
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = -1
        words.extend_from_slice(&jsr_disp16_a6(-162)); // Errno() -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, EOPNOTSUPP);
    }

    #[test]
    fn end_to_end_inet_addr_and_inet_ntoa_round_trip() {
        let cp_addr: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, cp_addr); // A0 = "127.0.0.1"
        words.extend_from_slice(&jsr_disp16_a6(-180)); // inet_addr -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, cp_addr, b"127.0.0.1");
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code as u32, 0x7F00_0001, "127.0.0.1 packed big-endian");

        // Round-trip it back through Inet_NtoA in a second run, reusing
        // the same packed value as D0's input.
        let mut words2 = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words2, 0, 0x7F00_0001);
        words2.extend_from_slice(&jsr_disp16_a6(-174)); // Inet_NtoA -> D0 = ptr
        words2.push(RTS);
        let mut rt2 = runtime_with_program(&words2);
        let mut out2 = Vec::new();
        let ptr = rt2.run(&mut out2, None).expect("run should succeed") as u32;
        let text = crate::guestmem::read_c_string(rt2.memory(), ptr);
        assert_eq!(text, b"127.0.0.1");
    }

    #[test]
    fn end_to_end_errno_and_set_errno_ptr_mirror_on_failure() {
        let errno_addr: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, errno_addr); // A0 = &errno mirror
        words.push(move_imm_to_d(0)); // D0 = 4 (LONG size)
        words.push(0);
        words.push(4);
        words.extend_from_slice(&jsr_disp16_a6(-168)); // SetErrnoPtr(a0,d0)
        push_move_imm_d(&mut words, 0, 999); // an fd that was never opened
        words.extend_from_slice(&jsr_disp16_a6(-120)); // CloseSocket(999) -> D0 = -1
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, -1, "CloseSocket on an unknown fd should fail");
        assert_eq!(
            rt.memory().read_u32(errno_addr),
            EBADF as u32,
            "SetErrnoPtr's mirror should reflect the failing call's errno"
        );
    }

    #[test]
    fn end_to_end_udp_bind_and_getsockname_reports_real_local_address() {
        let sockaddr_buf: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_DGRAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd
        words.push(move_d0_to_d(3)); // D3 = fd

        // bind(fd, {127.0.0.1:0}, 16) -- the sockaddr_in bytes are
        // written directly into guest memory below (before Runtime::new),
        // since the CPU program only needs the buffer's address.
        const MOVE_D3_TO_D0: u16 = 0x2003; // move.l D3,D0
        words.push(MOVE_D3_TO_D0); // D0 = fd (bind's D0 argument)
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_d(&mut words, 1, 16);
        words.extend_from_slice(&jsr_disp16_a6(-36)); // bind() -> D0

        // getsockname(fd, sockaddr_buf, ...) -- fd is still in D3.
        words.push(MOVE_D3_TO_D0);
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_a(&mut words, 1, sockaddr_buf); // namelen ptr, unused
        words.extend_from_slice(&jsr_disp16_a6(-102)); // getsockname() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        // Pre-fill the sockaddr_in buffer bind() will read: 127.0.0.1:0.
        write_sockaddr_in(
            &mut mem,
            sockaddr_buf,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        );
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "getsockname should succeed");

        let reported = read_sockaddr_in(rt.memory(), sockaddr_buf);
        assert_eq!(*reported.ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(
            reported.port(),
            0,
            "the OS should have assigned a real ephemeral port"
        );
    }

    #[test]
    fn end_to_end_gethostbyname_resolves_localhost_via_the_real_host_resolver() {
        let name_addr: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, name_addr);
        words.extend_from_slice(&jsr_disp16_a6(-210)); // gethostbyname -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, name_addr, b"localhost");
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let hostent = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_ne!(hostent, 0, "localhost should always resolve");

        let mem = rt.memory();
        assert_eq!(mem.read_u32(hostent + 8), AF_INET as u32, "h_addrtype");
        assert_eq!(mem.read_u32(hostent + 12), 4, "h_length");
        let addr_list = mem.read_u32(hostent + 16);
        let first_addr_ptr = mem.read_u32(addr_list);
        assert_ne!(first_addr_ptr, 0, "h_addr_list[0] should be non-NULL");
        let ip = Ipv4Addr::from(mem.read_u32(first_addr_ptr));
        assert!(
            ip.is_loopback(),
            "localhost should resolve to a loopback address, got {ip}"
        );
        assert_eq!(
            mem.read_u32(addr_list.wrapping_add(4)),
            0,
            "h_addr_list should be NULL-terminated"
        );

        let name_ptr = mem.read_u32(hostent);
        assert_eq!(crate::guestmem::read_c_string(mem, name_ptr), b"localhost");
    }

    #[test]
    fn end_to_end_gethostbyname_unresolvable_name_fails_with_host_not_found() {
        let name_addr: u32 = 0x1_8000;
        let herrno_addr: u32 = 0x1_8100;
        let tags_addr: u32 = 0x1_8200;

        // SocketBaseTagList(SBTM_SETVAL(SBTC_HERRNOLONGPTR), &herrno, TAG_DONE)
        // -- register a real h_errno mirror first, since a failed
        // gethostbyname reports through *that* channel, not Errno()
        // (see BsdSocketState::set_herrno's doc; confirmed against a
        // real bsdsocktest conformance run, which registers exactly
        // this tag before running a single test).
        const SBTM_SETVAL_HERRNOLONGPTR: u32 = TAG_USER | (SBTC_HERRNOLONGPTR << 1) | SBTF_SET;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, tags_addr);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // SocketBaseTagList() -> D0
        push_move_imm_a(&mut words, 0, name_addr);
        words.extend_from_slice(&jsr_disp16_a6(-210)); // gethostbyname -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        // .invalid is a reserved TLD (RFC 2606) guaranteed to never
        // resolve, avoiding real-network flakiness in this test.
        crate::guestmem::write_c_string(&mut mem, name_addr, b"this-name-does-not-exist.invalid");
        mem.write_u32(tags_addr, SBTM_SETVAL_HERRNOLONGPTR);
        mem.write_u32(tags_addr.wrapping_add(4), herrno_addr);
        mem.write_u32(tags_addr.wrapping_add(8), TAG_DONE);
        mem.write_u32(herrno_addr, 0xDEAD_BEEF); // sentinel: must be overwritten
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "gethostbyname should return NULL on failure");
        assert_eq!(
            rt.memory().read_u32(herrno_addr),
            HOST_NOT_FOUND as u32,
            "h_errno should report HOST_NOT_FOUND through its registered pointer"
        );
    }

    #[test]
    fn snoop_detail_reports_a_real_outbound_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a real listener");
        let port = listener.local_addr().unwrap().port();
        // Accept in the background so the connect completes rather than
        // just sitting in the OS's SYN queue -- either way connect()
        // itself would report the same detail, but a real accepted peer
        // makes this test's outcome unambiguous.
        let accept_thread = std::thread::spawn(move || listener.accept());

        let sockaddr_buf: u32 = 0x1_8000;
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd (connect's D0 argument, already in place)
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_d(&mut words, 1, 16);
        words.extend_from_slice(&jsr_disp16_a6(-54)); // connect() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        write_sockaddr_in(
            &mut mem,
            sockaddr_buf,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        );
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
        rt.enable_bsdsocket();

        let mut details = Vec::new();
        let mut trace = |event: &crate::dispatch::TraceEvent| {
            if let Some(detail) = &event.detail {
                details.push(detail.clone());
            }
        };
        let mut out = Vec::new();
        rt.run(&mut out, Some(&mut trace))
            .expect("run should succeed");
        let _ = accept_thread.join();

        let connect_detail = details
            .iter()
            .find(|d| d.starts_with("connect ->"))
            .unwrap_or_else(|| panic!("no connect detail among {details:?}"));
        assert!(
            connect_detail.contains(&format!("127.0.0.1:{port}")),
            "{connect_detail:?}"
        );
        assert!(
            connect_detail.ends_with("-> ok") || connect_detail.ends_with("-> in progress"),
            "{connect_detail:?}"
        );
    }

    #[test]
    fn snoop_detail_omits_accept_eagain_but_reports_a_real_failure() {
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd
        words.push(move_d0_to_d(0)); // D0 = fd (accept's D0 argument)
        push_move_imm_a(&mut words, 0, 0); // addr = NULL, don't care
        words.extend_from_slice(&jsr_disp16_a6(-48)); // accept() on an
        // unbound, unconnected socket -- a real, immediate host error
        // (not EAGAIN), exercising the "real failure" detail branch.
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut details = Vec::new();
        let mut trace = |event: &crate::dispatch::TraceEvent| {
            if let Some(detail) = &event.detail {
                details.push(detail.clone());
            }
        };
        let mut out = Vec::new();
        let code = rt
            .run(&mut out, Some(&mut trace))
            .expect("run should succeed");
        assert_eq!(code, -1);
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("accept") && d.contains("failed")),
            "expected an accept-failed detail among {details:?}"
        );
    }

    #[test]
    fn end_to_end_accept_on_a_nonblocking_listener_with_nothing_pending_is_eagain_not_einprogress()
    {
        // A real regression test: translate_errno once wrongly routed
        // every WouldBlock-kind error (not just connect()'s own
        // "still connecting" signal) through is_connect_in_progress,
        // making this exact scenario report EINPROGRESS(36) instead of
        // the correct EAGAIN(35) -- caught by a real conformance suite
        // (bsdsocktest's "accept(): EWOULDBLOCK when non-blocking, no
        // pending" test), not by this module's own prior (synthetic
        // failure-only) accept coverage. See translate_errno's own doc.
        let sockaddr_buf: u32 = 0x1_8000;
        let nonblock_flag: u32 = 0x1_8100;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd
        words.push(move_d0_to_d(3)); // D3 = fd

        const MOVE_D3_TO_D0: u16 = 0x2003;
        words.push(MOVE_D3_TO_D0);
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_d(&mut words, 1, 16);
        words.extend_from_slice(&jsr_disp16_a6(-36)); // bind(fd, 127.0.0.1:0, 16)

        words.push(MOVE_D3_TO_D0);
        push_move_imm_d(&mut words, 1, 5);
        words.extend_from_slice(&jsr_disp16_a6(-42)); // listen(fd, 5)

        words.push(MOVE_D3_TO_D0);
        push_move_imm_d(&mut words, 1, FIONBIO);
        push_move_imm_a(&mut words, 0, nonblock_flag);
        words.extend_from_slice(&jsr_disp16_a6(-114)); // IoctlSocket(fd, FIONBIO, &1)

        words.push(MOVE_D3_TO_D0);
        push_move_imm_a(&mut words, 0, 0); // addr = NULL
        words.extend_from_slice(&jsr_disp16_a6(-48)); // accept(fd, NULL, NULL) -> D0
        words.extend_from_slice(&jsr_disp16_a6(-162)); // Errno() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        write_sockaddr_in(
            &mut mem,
            sockaddr_buf,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        );
        mem.write_u32(nonblock_flag, 1);
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, EAGAIN, "must be EAGAIN/EWOULDBLOCK, not EINPROGRESS");
    }

    #[test]
    fn end_to_end_ioctlsocket_fionbio_succeeds() {
        let nonblock_flag: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd (IoctlSocket's D0 argument)
        push_move_imm_d(&mut words, 1, FIONBIO);
        push_move_imm_a(&mut words, 0, nonblock_flag);
        words.extend_from_slice(&jsr_disp16_a6(-114)); // IoctlSocket() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        mem.write_u32(nonblock_flag, 1);
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "IoctlSocket(FIONBIO) should succeed");
    }

    #[test]
    fn end_to_end_ioctlsocket_unsupported_command_fails_with_eopnotsupp() {
        let arg: u32 = 0x1_9000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd
        push_move_imm_d(&mut words, 1, 0xDEAD_BEEF); // a bogus ioctl command
        push_move_imm_a(&mut words, 0, arg);
        words.extend_from_slice(&jsr_disp16_a6(-114)); // IoctlSocket() -> D0 = -1
        words.extend_from_slice(&jsr_disp16_a6(-162)); // Errno() -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, EOPNOTSUPP);
    }

    #[test]
    fn end_to_end_socket_base_tag_list_registers_errno_ptr_and_returns_release_string() {
        let errno_addr: u32 = 0x1_8000;
        let release_ptr_out: u32 = 0x1_8100;
        let tags_addr: u32 = 0x1_8200;

        const SBTM_SETVAL_ERRNOLONGPTR: u32 = TAG_USER | (SBTC_ERRNOLONGPTR << 1) | SBTF_SET;
        const SBTM_GETREF_RELEASESTRPTR: u32 = TAG_USER | SBTF_REF | (SBTC_RELEASESTRPTR << 1);

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, tags_addr);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // SocketBaseTagList() -> D0

        // A subsequent failing call (CloseSocket on an unknown fd)
        // should now mirror its errno through the registered pointer.
        push_move_imm_d(&mut words, 0, 999);
        words.extend_from_slice(&jsr_disp16_a6(-120)); // CloseSocket(999) -> D0 = -1
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        // tags[0] = SETVAL(ERRNOLONGPTR), &errno_addr
        mem.write_u32(tags_addr, SBTM_SETVAL_ERRNOLONGPTR);
        mem.write_u32(tags_addr.wrapping_add(4), errno_addr);
        // tags[1] = GETREF(RELEASESTRPTR), &release_ptr_out
        mem.write_u32(tags_addr.wrapping_add(8), SBTM_GETREF_RELEASESTRPTR);
        mem.write_u32(tags_addr.wrapping_add(12), release_ptr_out);
        // tags[2] = TAG_DONE
        mem.write_u32(tags_addr.wrapping_add(16), TAG_DONE);
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, -1, "CloseSocket on an unknown fd should still fail");
        assert_eq!(
            rt.memory().read_u32(errno_addr),
            EBADF as u32,
            "SocketBaseTagList's SBTC_ERRNOLONGPTR registration should mirror errno"
        );

        let release_ptr = rt.memory().read_u32(release_ptr_out);
        assert_ne!(
            release_ptr, 0,
            "SBTC_RELEASESTRPTR should report a real string"
        );
        let release_str = crate::guestmem::read_c_string(rt.memory(), release_ptr);
        assert!(!release_str.is_empty());
    }

    #[test]
    fn end_to_end_setsockopt_so_reuseaddr_round_trips_through_getsockopt() {
        let optval: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd
        words.push(move_d0_to_d(4)); // D4 = fd (D3 is setsockopt's own optlen argument)

        const MOVE_D4_TO_D0: u16 = 0x2004; // move.l D4,D0

        // setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &optval(=1), 4)
        words.push(MOVE_D4_TO_D0);
        push_move_imm_d(&mut words, 1, SOL_SOCKET as u32);
        push_move_imm_d(&mut words, 2, SO_REUSEADDR as u32);
        push_move_imm_a(&mut words, 0, optval);
        push_move_imm_d(&mut words, 3, 4);
        words.extend_from_slice(&jsr_disp16_a6(-90)); // setsockopt() -> D0

        // getsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &optval, &optlen)
        words.push(MOVE_D4_TO_D0);
        push_move_imm_d(&mut words, 1, SOL_SOCKET as u32);
        push_move_imm_d(&mut words, 2, SO_REUSEADDR as u32);
        push_move_imm_a(&mut words, 0, optval);
        push_move_imm_a(&mut words, 1, optval); // optlen ptr, unused by this backend
        words.extend_from_slice(&jsr_disp16_a6(-96)); // getsockopt() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        mem.write_u32(optval, 1); // enable SO_REUSEADDR
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "getsockopt should succeed");
        assert_eq!(
            rt.memory().read_u32(optval),
            1,
            "SO_REUSEADDR should read back as enabled"
        );
    }

    #[test]
    fn end_to_end_getsockopt_so_error_reports_no_error_on_a_healthy_socket() {
        // A blocking connect() (this backend's default -- see the
        // module docs' "Blocking by default" section) reports a failed
        // connection attempt directly through its own return value, not
        // through a separately-pending SO_ERROR (that distinction only
        // applies to a non-blocking connect's later-discovered failure,
        // real BSD semantics this test doesn't need real network
        // failure timing to exercise): a fresh, never-connected socket
        // should simply report "no pending error" (0), deterministically.
        let optval: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd (getsockopt's D0 argument)
        push_move_imm_d(&mut words, 1, SOL_SOCKET as u32);
        push_move_imm_d(&mut words, 2, SO_ERROR as u32);
        push_move_imm_a(&mut words, 0, optval);
        push_move_imm_a(&mut words, 1, optval);
        words.extend_from_slice(&jsr_disp16_a6(-96)); // getsockopt(SO_ERROR) -> D0
        words.push(RTS);

        mem_write_optval_and_run(&mut words, optval, 0xDEAD_BEEF, |mem| {
            assert_eq!(
                mem.read_u32(optval),
                0,
                "a fresh socket should report no pending error"
            );
        });
    }

    /// Shared tail for the two option-round-trip tests above: loads
    /// `words`, seeds `optval` with a sentinel, runs to completion, and
    /// hands the caller the guest memory to inspect.
    fn mem_write_optval_and_run(
        words: &mut [u16],
        optval: u32,
        sentinel: u32,
        check: impl FnOnce(&FlatMemory),
    ) {
        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, words);
        mem.write_u32(optval, sentinel);
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "getsockopt should succeed");
        check(rt.memory());
    }

    #[test]
    fn end_to_end_waitselect_detects_real_udp_readiness() {
        // A fixed port (not ephemeral) so a background host thread can
        // be started *before* the guest program runs -- the whole
        // socket()/bind()/WaitSelect() sequence executes as a single,
        // uninterruptible rt.run() call, so there's no host-side point
        // between "the port is known" and "WaitSelect blocks" to send
        // from. The sender sleeps briefly, then sends, landing the real
        // datagram while WaitSelect's real (2s) poll(2) timeout is
        // still open.
        let port = 58234;
        let sockaddr_buf: u32 = 0x1_8000;
        let readfds: u32 = 0x1_8100;
        let timeout_buf: u32 = 0x1_8200;

        let sender_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind sender");
            sender.send_to(b"hi", ("127.0.0.1", port)).expect("send_to");
        });

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_DGRAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd (always 1, first socket)
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_d(&mut words, 1, 16);
        words.extend_from_slice(&jsr_disp16_a6(-36)); // bind(fd, 127.0.0.1:port, 16)

        push_move_imm_d(&mut words, 0, 2); // nfds = fd(1) + 1
        push_move_imm_a(&mut words, 0, readfds);
        push_move_imm_a(&mut words, 1, 0); // writefds = NULL
        push_move_imm_a(&mut words, 2, 0); // exceptfds = NULL
        push_move_imm_a(&mut words, 3, timeout_buf);
        push_move_imm_d(&mut words, 1, 0); // signals = NULL
        words.extend_from_slice(&jsr_disp16_a6(-126)); // WaitSelect() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        write_sockaddr_in(
            &mut mem,
            sockaddr_buf,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        );
        mem.write_u32(readfds, 0b10); // FD_SET(1, &readfds)
        for i in 4..FD_SET_BYTES {
            mem.write_u8(readfds.wrapping_add(i), 0);
        }
        write_timeval(
            &mut mem,
            timeout_buf,
            Some(std::time::Duration::from_secs(2)),
        );
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        sender_thread.join().unwrap();

        assert!(
            code >= 1,
            "expected at least one ready descriptor, got {code}"
        );
        assert!(
            fd_set_test(rt.memory(), readfds, 1),
            "fd 1 should be marked ready in readfds"
        );
    }

    #[test]
    fn end_to_end_waitselect_timeout_returns_zero_when_idle() {
        let sockaddr_buf: u32 = 0x1_8000;
        let readfds: u32 = 0x1_8100;
        let timeout_buf: u32 = 0x1_8200;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_d(&mut words, 1, 16);
        words.extend_from_slice(&jsr_disp16_a6(-36)); // bind(fd, 127.0.0.1:0, 16)
        push_move_imm_d(&mut words, 1, 1);
        words.extend_from_slice(&jsr_disp16_a6(-42)); // listen(fd, 1)

        push_move_imm_d(&mut words, 0, 2);
        push_move_imm_a(&mut words, 0, readfds);
        push_move_imm_a(&mut words, 1, 0);
        push_move_imm_a(&mut words, 2, 0);
        push_move_imm_a(&mut words, 3, timeout_buf);
        push_move_imm_d(&mut words, 1, 0);
        words.extend_from_slice(&jsr_disp16_a6(-126)); // WaitSelect() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        write_sockaddr_in(
            &mut mem,
            sockaddr_buf,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        );
        mem.write_u32(readfds, 0b10);
        for i in 4..FD_SET_BYTES {
            mem.write_u8(readfds.wrapping_add(i), 0);
        }
        // A short, real timeout -- nothing will ever connect, so this
        // should genuinely elapse and return 0 (not hang the test).
        write_timeval(
            &mut mem,
            timeout_buf,
            Some(std::time::Duration::from_millis(100)),
        );
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "idle listener should time out with 0 ready");
    }

    #[test]
    fn end_to_end_waitselect_returns_immediately_for_an_already_pending_signal() {
        use crate::dispatch::EXEC_LIBRARY_BASE;

        let signals_buf: u32 = 0x1_8000;
        const USER_SIGNAL_BIT: u32 = 0x0001_0000; // bit 16, first non-system-reserved bit

        let mut words: Vec<u16> = Vec::new();
        // SetSignal(USER_SIGNAL_BIT, USER_SIGNAL_BIT) via exec.library --
        // marks the signal as already pending before WaitSelect runs.
        words.push(move_imm_to_a(6));
        words.push((EXEC_LIBRARY_BASE >> 16) as u16);
        words.push(EXEC_LIBRARY_BASE as u16);
        push_move_imm_d(&mut words, 0, USER_SIGNAL_BIT);
        push_move_imm_d(&mut words, 1, USER_SIGNAL_BIT);
        words.extend_from_slice(&jsr_disp16_a6(-306)); // SetSignal()

        words.extend_from_slice(&movea_bsdsocket_base_to_a6());
        push_move_imm_d(&mut words, 0, 0); // nfds = 0 (no sockets to check)
        push_move_imm_a(&mut words, 0, 0);
        push_move_imm_a(&mut words, 1, 0);
        push_move_imm_a(&mut words, 2, 0);
        push_move_imm_a(&mut words, 3, 0); // timeout = NULL (would block forever if reached)
        push_move_imm_d(&mut words, 1, signals_buf); // D1 = &signals
        words.extend_from_slice(&jsr_disp16_a6(-126)); // WaitSelect() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        mem.write_u32(signals_buf, USER_SIGNAL_BIT); // requested mask
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
        rt.enable_bsdsocket();
        let task = rt.current_task();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(
            code, 0,
            "an already-pending requested signal returns 0 immediately"
        );
        assert_eq!(
            rt.memory().read_u32(signals_buf),
            USER_SIGNAL_BIT,
            "the matched signal bits should be reported back"
        );
        assert_eq!(
            rt.memory().read_u32(task + exectask::TC_SIGRECVD) & USER_SIGNAL_BIT,
            0,
            "the matched signal should be consumed (cleared) from tc_SigRecvd"
        );
    }

    #[test]
    fn end_to_end_dup2socket_new_descriptor_can_send_and_recv() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a real listener");
        let port = listener.local_addr().unwrap().port();
        let accept_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream.write_all(b"hi").expect("write_all");
        });

        let sockaddr_buf: u32 = 0x1_8000;
        let recv_buf: u32 = 0x1_8100;
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_d(&mut words, 1, 16);
        words.extend_from_slice(&jsr_disp16_a6(-54)); // connect() -> D0 (== 0, ok)

        push_move_imm_d(&mut words, 0, 1); // old_socket = fd(1), the only socket
        push_move_imm_d(&mut words, 1, 0xFFFF_FFFF); // new_socket = -1 (pick any free fd)
        words.extend_from_slice(&jsr_disp16_a6(-264)); // Dup2Socket() -> D0 = dup fd
        words.push(move_d0_to_d(3)); // D3 = dup fd

        const MOVE_D3_TO_D0: u16 = 0x2003;
        words.push(MOVE_D3_TO_D0);
        push_move_imm_a(&mut words, 0, recv_buf);
        push_move_imm_d(&mut words, 1, 2);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-78)); // recv(dup fd, buf, 2, 0) -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        write_sockaddr_in(
            &mut mem,
            sockaddr_buf,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        );
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
        rt.enable_bsdsocket();
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        accept_thread.join().unwrap();

        assert_eq!(
            code, 2,
            "recv() on the duplicated descriptor should see the real 2 bytes"
        );
        assert_eq!(rt.memory().read_u8(recv_buf), b'h');
        assert_eq!(rt.memory().read_u8(recv_buf + 1), b'i');
    }

    #[test]
    fn end_to_end_dup2socket_specific_target_is_honored() {
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd (1)

        push_move_imm_d(&mut words, 0, 1); // old_socket = fd(1)
        push_move_imm_d(&mut words, 1, 50); // new_socket = 50 (explicit target)
        words.extend_from_slice(&jsr_disp16_a6(-264)); // Dup2Socket() -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 50, "an explicit target fd should be honored exactly");
    }

    #[test]
    fn end_to_end_dup2socket_unknown_descriptor_fails_with_ebadf() {
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, 99); // old_socket = never opened
        push_move_imm_d(&mut words, 1, 0xFFFF_FFFF);
        words.extend_from_slice(&jsr_disp16_a6(-264)); // Dup2Socket() -> D0 (== -1)
        words.extend_from_slice(&jsr_disp16_a6(-162)); // Errno() -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, EBADF);
    }

    #[test]
    fn end_to_end_vsyslog_formats_message_and_reports_via_call_detail() {
        let msg_buf: u32 = 0x1_8000;
        let str_buf: u32 = 0x1_8100;
        let args_buf: u32 = 0x1_8200;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, 6); // pri = LOG_INFO
        push_move_imm_a(&mut words, 0, msg_buf); // msg = "%s"
        push_move_imm_a(&mut words, 1, args_buf); // args
        words.extend_from_slice(&jsr_disp16_a6(-258)); // vsyslog()
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        for (i, b) in b"%s\0".iter().enumerate() {
            mem.write_u8(msg_buf + i as u32, *b);
        }
        for (i, b) in b"canary\0".iter().enumerate() {
            mem.write_u8(str_buf + i as u32, *b);
        }
        mem.write_u32(args_buf, str_buf);
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
        rt.enable_bsdsocket();

        let mut details = Vec::new();
        let mut trace = |event: &crate::dispatch::TraceEvent| {
            if let Some(detail) = &event.detail {
                details.push(detail.clone());
            }
        };
        let mut out = Vec::new();
        rt.run(&mut out, Some(&mut trace))
            .expect("run should succeed");

        assert!(
            details.iter().any(|d| d == "vsyslog(pri=6): canary"),
            "expected a vsyslog detail among {details:?}"
        );
    }

    #[test]
    fn end_to_end_release_socket_family_honestly_fails_without_crashing() {
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, 1); // sock
        push_move_imm_d(&mut words, 1, 42); // id
        words.extend_from_slice(&jsr_disp16_a6(-150)); // ReleaseSocket() -> D0 (== -1)
        words.extend_from_slice(&jsr_disp16_a6(-162)); // Errno() -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, EOPNOTSUPP);
    }
}
