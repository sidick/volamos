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
//! `SocketBaseTagList`/`WaitSelect`/`Dup2Socket`/`vsyslog`/
//! `GetSocketEvents`/`SetSocketSignals`/`gethostbyaddr`/`getservbyname`/
//! `getservbyport`/`getprotobyname`/`getprotobynumber`/`gethostname`/
//! `gethostid`/`sendmsg`/`recvmsg`/`Inet_LnaOf`/`Inet_NetOf`/
//! `Inet_MakeAddr` -- real outbound and inbound TCP, plus UDP, real error
//! reporting through the documented `Errno()`/`SetErrnoPtr()` mechanism,
//! real forward *and* reverse DNS lookups, and real services/protocols
//! database lookups, all via the host's own resolver/libc (see "DNS: a
//! real, blocking host lookup" below), the
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
//! `getnetbyname`/`getnetbyaddr` (the `/etc/networks` network-name
//! database -- rarer than the services/protocols databases above, no
//! corpus binary or conformance test needs it yet).
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
//! # `SO_EVENTMASK`/`GetSocketEvents`: real per-socket event detection
//!
//! `SO_EVENTMASK` ([`setsockopt_handler`]/[`getsockopt_handler`]) arms a
//! set of `FD_*` bits (`FD_READ`/`FD_WRITE`/`FD_ACCEPT`/`FD_CLOSE`/
//! `FD_ERROR` -- real `<libraries/bsdsocket.h>` values) on one socket;
//! `SocketBaseTagList`'s `SBTC_SIGEVENTMASK` tag arms an Exec signal bit
//! to raise whenever any armed event fires on any socket;
//! `GetSocketEvents` drains one pending `(fd, bits)` pair at a time,
//! round-robin across sockets that have one. There is no dedicated
//! "check for events" call in the real API -- real programs discover
//! events by calling `WaitSelect` (often with every `fd_set` `NULL` and
//! only the signal armed, since the events themselves aren't in any
//! `fd_set`), so [`wait_select_poll`] is where detection actually
//! happens: every event-armed socket rides along in the very same real
//! `poll(2)` call as the caller's own explicit `fd_set` interests
//! (a *single* `poll(2)` covers both, so a real event unblocks a
//! `WaitSelect(0, NULL, NULL, NULL, &tv, &sigmask)` call early exactly
//! like real hardware) -- see that function's own doc comment for the
//! `poll(2)`-revents-to-`FD_*`-bits classification, including how a
//! listening socket's `POLLIN` becomes `FD_ACCEPT` instead of `FD_READ`,
//! and how `FD_CLOSE` is disambiguated from real pending data via a
//! zero-consuming `peek`. Deliberately does not attempt `FD_CONNECT`
//! detection (see that function's doc for why treating an already-
//! writable connected socket as "connect just completed" would misfire
//! on every call) -- `bsdsocktest`'s own `FD_CONNECT` test tolerates
//! this, since a synchronous/fast loopback `connect()` returning `0`
//! directly is one of its explicitly documented-acceptable outcomes.
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
const SBTC_BREAKMASK: u32 = 1;
const SBTC_SIGEVENTMASK: u32 = 4;
const SBTC_DTABLESIZE: u32 = 8;
const SBTC_ERRNOLONGPTR: u32 = 24;
const SBTC_HERRNOLONGPTR: u32 = 25;
const SBTC_RELEASESTRPTR: u32 = 29;

// --- GetSocketEvents()/SO_EVENTMASK event bits -- a real
// <libraries/bsdsocket.h> (Roadshow) value set, not invented. See
// [`wait_select_poll`]'s event-synthesis pass and
// [`get_socket_events_handler`].
const FD_ACCEPT: u32 = 0x01;
const FD_CONNECT: u32 = 0x02;
#[allow(dead_code)] // listed for completeness against the real header; no
// portable way to detect true OOB-data-pending short of a peek this
// backend doesn't yet perform for FD_OOB specifically (see the module
// docs -- FD_OOB/MSG_OOB share the same unimplemented-OOB gap as
// WaitSelect's own exceptfds).
const FD_OOB: u32 = 0x04;
const FD_READ: u32 = 0x08;
const FD_WRITE: u32 = 0x10;
const FD_ERROR: u32 = 0x20;
const FD_CLOSE: u32 = 0x40;

/// `SO_EVENTMASK` -- a real `<sys/socket.h>` (Roadshow) value, not
/// invented. Purely internal bookkeeping (which `FD_*` bits arm an event
/// on this socket) -- unlike every other `setsockopt`/`getsockopt`
/// option this module implements, it has no underlying real host socket
/// option to translate to/from, so [`setsockopt_handler`]/
/// [`getsockopt_handler`] special-case it before ever reaching their
/// normal `socket2`-backed dispatch.
const SO_EVENTMASK: i32 = 0x2001;

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
    /// `SO_EVENTMASK`'s armed `FD_*` bits (see [`wait_select_poll`]'s
    /// event-synthesis pass) -- `0` (the default) means "no events
    /// armed", matching every socket's real default.
    event_mask: u32,
    /// `FD_*` bits [`wait_select_poll`] has detected as real and not yet
    /// handed back by [`get_socket_events_handler`] -- the queue that
    /// function drains.
    pending_events: u32,
    /// Whether `listen()` was ever called on this socket -- distinguishes
    /// "readable" (`FD_READ`, real data) from "a connection is waiting"
    /// (`FD_ACCEPT`) when interpreting a `POLLIN` result, since both look
    /// identical at the `poll(2)` level.
    is_listener: bool,
}

impl SocketEntry {
    fn new(socket: Socket) -> Self {
        SocketEntry {
            socket,
            event_mask: 0,
            pending_events: 0,
            is_listener: false,
        }
    }
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
    /// Every guest-heap allocation [`build_hostent`] built for the
    /// *previous* successful [`gethostbyname_handler`] or
    /// [`gethostbyaddr_handler`] call (whichever ran last -- both share
    /// this one slot, matching real `bsdsocket.library`'s own single
    /// static result buffer) -- the `struct hostent`, the name copy, the
    /// aliases array, the address-pointer array, and each address block
    /// -- freed at the start of the next call rather than ever being
    /// freed by the guest (see the module docs' "DNS" section for why).
    /// Empty until the first successful lookup.
    hostent_allocs: Vec<u32>,
    /// Same lifetime pattern as [`Self::hostent_allocs`], but for
    /// [`getservbyname_handler`]/[`getservbyport_handler`]'s `struct
    /// servent*` result -- a separate static buffer from `hostent`,
    /// matching real `bsdsocket.library`'s own per-function-family
    /// static results.
    servent_allocs: Vec<u32>,
    /// Same lifetime pattern as [`Self::hostent_allocs`], but for
    /// [`getprotobyname_handler`]/[`getprotobynumber_handler`]'s
    /// `struct protoent*` result.
    protoent_allocs: Vec<u32>,
    /// Guest heap address of a small, lazily-allocated buffer holding
    /// this library's version/release identifier string, returned by
    /// `SocketBaseTagList`'s `SBTC_RELEASESTRPTR` query -- see
    /// [`socket_base_tag_list_handler`].
    release_str_buf: Option<u32>,
    /// The last `h_errno`-shaped code any DNS handler here recorded --
    /// mirrors [`Self::last_errno`]'s own role, but for the separate
    /// `h_errno` channel, used as the initial value a lazily-allocated
    /// [`Self::herrno_ptr`] starts with (see
    /// [`socket_base_tag_list_handler`]'s `SBTC_HERRNOLONGPTR` GET
    /// branch).
    last_herrno: i32,
    /// `SBTC_SIGEVENTMASK`'s current value: the Exec signal bit(s)
    /// [`wait_select_poll`]'s event-synthesis pass raises when it detects
    /// a real, armed `SO_EVENTMASK` event on any socket. `0` (the
    /// default) means no signal is armed for events at all.
    sigeventmask: u32,
    /// `SBTC_BREAKMASK`'s current value -- plain round-trip storage (no
    /// handler here currently *acts* on it; nothing in this backend
    /// synthesizes its own Ctrl-C delivery independent of the guest's own
    /// task signal state). Defaults to `SIGBREAKF_CTRL_C` (bit 12),
    /// matching real `bsdsocket.library`'s own default.
    breakmask: u32,
    /// `SBTC_DTABLESIZE`'s current value -- what [`getdtablesize_handler`]
    /// returns and [`socket_handler`]'s own `MAX_OPEN_SOCKETS`-shaped
    /// admission check now consults instead of the fixed constant, so a
    /// guest that raises it via `SocketBaseTagList` really can open more
    /// sockets afterward. Starts at [`MAX_OPEN_SOCKETS`]; capped growth
    /// (see [`socket_base_tag_list_handler`]'s `SBTC_DTABLESIZE` SET
    /// branch) against unbounded guest-driven allocation.
    dtablesize: u32,
    /// [`get_socket_events_handler`]'s round-robin cursor: the last fd
    /// handed back, so consecutive calls with multiple sockets pending
    /// visit them in rotation rather than always returning the
    /// lowest-numbered one (real documented `GetSocketEvents` behavior --
    /// confirmed against `bsdsocktest`'s own round-robin test). `-1`
    /// (never yet returned anything) sorts before every real fd.
    event_cursor: i32,
}

/// `SIGBREAKF_CTRL_C` (bit 12) -- `exec/exec.h`'s standard Ctrl-C signal
/// bit, and real `bsdsocket.library`'s own default `SBTC_BREAKMASK`.
const SIGBREAKF_CTRL_C: u32 = 1 << 12;

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
            servent_allocs: Vec::new(),
            protoent_allocs: Vec::new(),
            release_str_buf: None,
            last_herrno: 0,
            sigeventmask: 0,
            breakmask: SIGBREAKF_CTRL_C,
            dtablesize: MAX_OPEN_SOCKETS as u32,
            event_cursor: -1,
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
        self.last_herrno = code;
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

    if ctx.bsdsocket.sockets.len() as u32 >= ctx.bsdsocket.dtablesize {
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
    ctx.bsdsocket.sockets.insert(id, SocketEntry::new(socket));
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
            if let Some(entry) = ctx.bsdsocket.sockets.get_mut(&fd) {
                entry.is_listener = true;
            }
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

    if ctx.bsdsocket.sockets.len() as u32 >= ctx.bsdsocket.dtablesize {
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
            ctx.bsdsocket.sockets.insert(id, SocketEntry::new(accepted));
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

// --- sendmsg/recvmsg: real `struct msghdr`/`struct iovec` scatter-
// gather -- a real `<sys/socket.h>`/`<sys/uio.h>` (Roadshow) layout, not
// invented. `msg_name`/`msg_namelen` (addressed send, e.g. UDP) and
// `msg_control`/`msg_controllen` (ancillary data) are read but not
// acted on -- no real corpus binary or conformance test exercises
// either yet (`bsdsocktest`'s own sendmsg/recvmsg tests both zero the
// whole struct via `memset` and only ever set `msg_iov`/`msg_iovlen`,
// i.e. a connected-socket, no-ancillary-data scatter/gather send/recv,
// same shape `send`/`recv` already cover, just split across multiple
// buffers). `msg_flags` (recvmsg's own output field) is always left
// `0` -- no `MSG_TRUNC` detection, since that only matters for a
// datagram recv into a too-small buffer, which isn't the tested shape
// either.

/// `struct iovec` size (`iov_base`/`iov_len`, 4 bytes each -- no
/// padding on `m68k`).
const IOVEC_SIZE: u32 = 8;

/// Reads `msg_iov`/`msg_iovlen` from a `struct msghdr` at `msg_ptr`
/// (offsets `8`/`12` -- see this section's own doc for the full field
/// layout) as a list of `(base, len)` pairs.
fn read_iovecs(mem: &dyn AddressSpace, msg_ptr: u32) -> Vec<(u32, u32)> {
    let iov_ptr = mem.read_u32(msg_ptr.wrapping_add(8));
    let iov_len = mem.read_u32(msg_ptr.wrapping_add(12));
    (0..iov_len)
        .map(|i| {
            let entry = iov_ptr.wrapping_add(i * IOVEC_SIZE);
            (mem.read_u32(entry), mem.read_u32(entry.wrapping_add(4)))
        })
        .collect()
}

/// `sendmsg(sock, msg, flags)`. `D0` = number of bytes sent, or `-1`
/// with `Errno()` set. Gathers every `iovec`'s bytes (in order) into
/// one buffer and sends it as a single real `send(2)` call -- see this
/// section's own doc for why `msg_name`/`msg_control` aren't consulted.
fn sendmsg_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let msg_ptr = ctx.cpu.address_register(AddressRegister(0));

    let mut buf = Vec::new();
    for (base, len) in read_iovecs(ctx.mem, msg_ptr) {
        for i in 0..len {
            buf.push(ctx.mem.read_u8(base.wrapping_add(i)));
        }
    }

    let Some(result) = with_socket(ctx, fd, |socket| socket.send(&buf)) else {
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

/// `recvmsg(sock, msg, flags)`. `D0` = number of bytes received, or
/// `-1` with `Errno()` set. Receives into one buffer sized to the sum
/// of every `iovec`'s length, then scatters the real bytes received
/// back across the `iovec`s in order (a short read only fills as many
/// `iovec`s -- and as much of the final one -- as the byte count
/// covers, matching real scatter semantics). `msg_flags` is always
/// written `0` -- see this section's own doc.
fn recvmsg_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let fd = ctx.cpu.data_register(DataRegister(0)) as i32;
    let msg_ptr = ctx.cpu.address_register(AddressRegister(0));

    let iovecs = read_iovecs(ctx.mem, msg_ptr);
    let total_len: u32 = iovecs.iter().map(|&(_, len)| len).sum();
    let mut buf = vec![0u8; total_len as usize];

    let Some(result) = with_socket(ctx, fd, |socket| socket.recv(as_uninit_slice(&mut buf))) else {
        return Ok(());
    };
    match result {
        Ok(n) => {
            let mut remaining = n;
            let mut src = 0usize;
            for (base, len) in iovecs {
                if remaining == 0 {
                    break;
                }
                let take = remaining.min(len as usize);
                for i in 0..take {
                    ctx.mem.write_u8(base.wrapping_add(i as u32), buf[src + i]);
                }
                src += take;
                remaining -= take;
            }
            ctx.mem.write_u32(msg_ptr.wrapping_add(24), 0); // msg_flags
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

    // SO_EVENTMASK has no underlying real host socket option -- it's
    // purely this backend's own bookkeeping for which FD_* bits
    // wait_select_poll's event-synthesis pass should watch for on this
    // socket -- so it's handled directly against BsdSocketState instead
    // of falling into the socket2-backed Apply dispatch below.
    if level == SOL_SOCKET && optname == SO_EVENTMASK {
        let mask = ctx.mem.read_u32(optval_ptr);
        let Some(entry) = ctx.bsdsocket.sockets.get_mut(&fd) else {
            ctx.bsdsocket.set_errno(ctx.mem, EBADF);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
            return Ok(());
        };
        entry.event_mask = mask;
        entry.pending_events &= mask; // dropping newly-disarmed bits
        ctx.bsdsocket.set_errno(ctx.mem, 0);
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }

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

    if level == SOL_SOCKET && optname == SO_EVENTMASK {
        let Some(entry) = ctx.bsdsocket.sockets.get(&fd) else {
            ctx.bsdsocket.set_errno(ctx.mem, EBADF);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
            return Ok(());
        };
        ctx.mem.write_u32(optval_ptr, entry.event_mask);
        ctx.bsdsocket.set_errno(ctx.mem, 0);
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }

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

/// `getdtablesize()`. `D0` = [`BsdSocketState::dtablesize`] (starts at
/// [`MAX_OPEN_SOCKETS`], raisable via `SocketBaseTagList(SBTC_DTABLESIZE)`
/// -- see [`socket_base_tag_list_handler`]) -- the real cap this backend
/// enforces, matching real `getdtablesize`'s "the largest fd value plus
/// one this process could ever have" contract closely enough for a
/// caller sizing an `fd_set`-like structure.
fn getdtablesize_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu
        .set_data_register(DataRegister(0), ctx.bsdsocket.dtablesize);
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

    // Every SO_EVENTMASK-armed socket rides along in the same real
    // poll(2) call, regardless of nfds/the caller's own fd_sets -- real
    // GetSocketEvents-driven callers call WaitSelect(0, NULL, NULL, NULL,
    // &tv, &sigmask) exactly like this (see the module docs' "SO_
    // EVENTMASK/GetSocketEvents" section), so event detection can't be
    // gated on the caller having also listed the fd explicitly.
    let event_fds: Vec<i32> = ctx
        .bsdsocket
        .sockets
        .iter()
        .filter(|(_, e)| e.event_mask != 0)
        .map(|(&fd, _)| fd)
        .collect();
    let event_pollfds: Vec<libc::pollfd> = event_fds
        .iter()
        .map(|&fd| {
            let entry = &ctx.bsdsocket.sockets[&fd];
            let mut events = 0;
            if entry.event_mask & (FD_READ | FD_ACCEPT | FD_CLOSE) != 0 {
                events |= libc::POLLIN;
            }
            if entry.event_mask & (FD_WRITE | FD_CONNECT) != 0 {
                events |= libc::POLLOUT;
            }
            libc::pollfd {
                fd: entry.socket.as_raw_fd(),
                events,
                revents: 0,
            }
        })
        .collect();
    pollfds.extend(event_pollfds);

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

    // Classify each event-armed socket's real poll(2) result into FD_*
    // bits (see the module docs' "SO_EVENTMASK/GetSocketEvents" section
    // for the full rationale) and queue newly-detected ones onto
    // pending_events, for get_socket_events_handler to drain later.
    // Deliberately does NOT attempt FD_CONNECT: a connected TCP socket's
    // send buffer is essentially always POLLOUT-ready, so treating that
    // as "connect just completed" would fire on every single WaitSelect
    // call rather than once -- a real caller either sees its own
    // synchronous connect() return 0 (no event needed) or hits a
    // fast-completing loopback non-blocking connect the same way; both
    // are documented-acceptable outcomes bsdsocktest's own FD_CONNECT
    // test tolerates.
    let mut any_new_event = false;
    for (i, &fd) in event_fds.iter().enumerate() {
        let revents = pollfds[interests.len() + i].revents;
        let Some(entry) = ctx.bsdsocket.sockets.get(&fd) else {
            continue;
        };
        let armed = entry.event_mask;
        let is_listener = entry.is_listener;
        let mut new_bits = 0u32;
        if revents & libc::POLLIN != 0 {
            if is_listener {
                if armed & FD_ACCEPT != 0 {
                    new_bits |= FD_ACCEPT;
                }
            } else if armed & FD_READ != 0 {
                new_bits |= FD_READ;
            } else if armed & FD_CLOSE != 0 {
                // POLLIN alone doesn't distinguish "real data arrived"
                // from "peer closed" (EOF also reads as readable) --
                // only relevant when FD_READ isn't also armed (if it
                // were, the branch above already claims this readiness
                // as FD_READ, matching real FD_READ's own "readable,
                // including at EOF" definition). Disambiguate with a
                // real zero-consuming peek: 0 bytes back means EOF.
                let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 1];
                if matches!(entry.socket.peek(&mut buf), Ok(0)) {
                    new_bits |= FD_CLOSE;
                }
            }
        }
        if revents & libc::POLLHUP != 0 && armed & FD_CLOSE != 0 {
            new_bits |= FD_CLOSE;
        }
        if revents & libc::POLLOUT != 0 && !is_listener && armed & FD_WRITE != 0 {
            new_bits |= FD_WRITE;
        }
        if revents & libc::POLLERR != 0 && armed & FD_ERROR != 0 {
            new_bits |= FD_ERROR;
        }
        if new_bits != 0 {
            if let Some(entry) = ctx.bsdsocket.sockets.get_mut(&fd) {
                entry.pending_events |= new_bits;
            }
            any_new_event = true;
        }
    }
    if any_new_event && ctx.bsdsocket.sigeventmask != 0 {
        let recvd = ctx.mem.read_u32(ctx.current_task + exectask::TC_SIGRECVD);
        ctx.mem.write_u32(
            ctx.current_task + exectask::TC_SIGRECVD,
            recvd | ctx.bsdsocket.sigeventmask,
        );
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

// --- Inet_LnaOf/Inet_NetOf/Inet_MakeAddr: classic 4.3BSD classful
// address splitting -- real `IN_CLASSA`/`IN_CLASSB`/`IN_CLASSC` masks
// (`<netinet/in.h>`), independently confirmed against `bsdsocktest`'s
// own worked example (`10.1.2.3` -> net `0x0a`, host `0x010203`). None
// of these three need an explicit `ntohl`/`htonl` byte-swap: `m68k` is
// big-endian, so real network byte order and the guest's own native
// register value already coincide -- the `in_addr_t` these functions
// take/return is exactly the plain 32-bit value `D0`/`D1` already hold,
// no different from any other address-arithmetic LVO here.

const IN_CLASSA_NET: u32 = 0xFF00_0000;
const IN_CLASSA_NSHIFT: u32 = 24;
const IN_CLASSA_HOST: u32 = 0x00FF_FFFF;
const IN_CLASSB_NET: u32 = 0xFFFF_0000;
const IN_CLASSB_NSHIFT: u32 = 16;
const IN_CLASSB_HOST: u32 = 0x0000_FFFF;
const IN_CLASSC_NET: u32 = 0xFFFF_FF00;
const IN_CLASSC_NSHIFT: u32 = 8;
const IN_CLASSC_HOST: u32 = 0x0000_00FF;

fn is_classa(addr: u32) -> bool {
    addr & 0x8000_0000 == 0
}
fn is_classb(addr: u32) -> bool {
    addr & 0xC000_0000 == 0x8000_0000
}

/// `Inet_LnaOf(in)`. `D0` = the host (local network address) portion of
/// `in`, per whichever class (A/B/C) `in` falls into.
fn inet_lnaof_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr = ctx.cpu.data_register(DataRegister(0));
    let host = if is_classa(addr) {
        addr & IN_CLASSA_HOST
    } else if is_classb(addr) {
        addr & IN_CLASSB_HOST
    } else {
        addr & IN_CLASSC_HOST
    };
    ctx.cpu.set_data_register(DataRegister(0), host);
    Ok(())
}

/// `Inet_NetOf(in)`. `D0` = the network portion of `in`, per whichever
/// class (A/B/C) `in` falls into.
fn inet_netof_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr = ctx.cpu.data_register(DataRegister(0));
    let net = if is_classa(addr) {
        (addr & IN_CLASSA_NET) >> IN_CLASSA_NSHIFT
    } else if is_classb(addr) {
        (addr & IN_CLASSB_NET) >> IN_CLASSB_NSHIFT
    } else {
        (addr & IN_CLASSC_NET) >> IN_CLASSC_NSHIFT
    };
    ctx.cpu.set_data_register(DataRegister(0), net);
    Ok(())
}

/// `Inet_MakeAddr(net, host)`. `D0` = the combined address, choosing
/// class A/B/C shape by `net`'s own magnitude (real 4.3BSD
/// `inet_makeaddr`'s documented rule -- not `host`'s), matching
/// [`inet_netof_handler`]/[`inet_lnaof_handler`]'s own split the other
/// way, confirmed round-tripping against `bsdsocktest`'s own test.
fn inet_makeaddr_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let net = ctx.cpu.data_register(DataRegister(0));
    let host = ctx.cpu.data_register(DataRegister(1));
    let addr = if net < 128 {
        (net << IN_CLASSA_NSHIFT) | (host & IN_CLASSA_HOST)
    } else if net < 65536 {
        (net << IN_CLASSB_NSHIFT) | (host & IN_CLASSB_HOST)
    } else if net < 16_777_216 {
        (net << IN_CLASSC_NSHIFT) | (host & IN_CLASSC_HOST)
    } else {
        net
    };
    ctx.cpu.set_data_register(DataRegister(0), addr);
    Ok(())
}

/// Shared `struct hostent` builder for [`gethostbyname_handler`] and
/// [`gethostbyaddr_handler`]'s identical result shape (one name, no
/// aliases, one or more `AF_INET` addresses): frees the *previous*
/// call's blocks first (either function's -- see
/// [`BsdSocketState::hostent_allocs`]' doc, which now covers both),
/// allocates a fresh `h_name`/`h_aliases` (empty)/`h_addr_list`/
/// `hostent`, and records the new allocations for the next call (from
/// either function) to free.
fn build_hostent<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    name_bytes: &[u8],
    addrs: &[Ipv4Addr],
    lvo: i32,
    handler_name: &str,
) -> Result<u32, DispatchError> {
    for addr in std::mem::take(&mut ctx.bsdsocket.hostent_allocs) {
        let _ = ctx.heap.free(addr);
    }

    let mut allocs = Vec::new();
    let mut alloc = |ctx: &mut HandlerContext<'_, C>, size: u32| -> Result<u32, DispatchError> {
        let addr = ctx
            .heap
            .alloc(size.max(4))
            .map_err(|e| DispatchError::HandlerFailed {
                library: "bsdsocket.library".to_string(),
                lvo,
                handler_name: handler_name.to_string(),
                message: format!("{handler_name}: guest heap allocation failed: {e}"),
            })?;
        allocs.push(addr);
        Ok(addr)
    };

    let name_buf = alloc(ctx, name_bytes.len() as u32 + 1)?;
    crate::guestmem::write_c_string(ctx.mem, name_buf, name_bytes);

    let aliases_arr = alloc(ctx, 4)?; // just a NULL terminator: no alias data available
    ctx.mem.write_u32(aliases_arr, 0);

    let mut addr_block_addrs = Vec::with_capacity(addrs.len());
    for ip in addrs {
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
    Ok(hostent)
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

    let hostent = build_hostent(ctx, &name_bytes, &addrs, -210, "gethostbyname")?;
    ctx.bsdsocket.set_errno(ctx.mem, 0);
    ctx.cpu.set_data_register(DataRegister(0), hostent);
    Ok(())
}

/// `gethostbyaddr(addr, len, type)`. `D0` = a `struct hostent*` (same
/// shape as [`gethostbyname_handler`]'s, single-address, no aliases),
/// or `NULL` with `Errno()` set to a `<netdb.h>` `h_errno` code
/// (`HOST_NOT_FOUND`) -- same error-reporting channel and shared result
/// buffer as `gethostbyname` (see [`BsdSocketState::hostent_allocs`]).
/// `addr` (`A0`) is a real `struct in_addr*` (4 raw address bytes, *not*
/// a string -- unlike `inet_addr`'s `STRPTR`), `len` (`D0`) must be `4`,
/// `type` (`D1`) must be `AF_INET`; anything else fails with
/// `HOST_NOT_FOUND`, matching `gethostbyname`'s own "nothing resolved"
/// outcome for an equally nonsensical request.
///
/// A real, blocking reverse-DNS (PTR) lookup via the host's own
/// resolver -- `std::net` has no portable reverse-DNS primitive (the
/// same wall Copperline's own `hostsocket-plugin` hit for its
/// `gethostbyaddr`, see that crate's module docs), but `libc::
/// getnameinfo` (POSIX, already available via the `libc` dependency) is
/// exactly the real OS resolver call every real reverse-DNS client
/// (including AmigaOS's own bsdsocket.library, on real hardware) is
/// built on, so this is no less "real" than `gethostbyname`'s own
/// `ToSocketAddrs`-based forward lookup -- just a different libc entry
/// point for the reverse direction. `NI_NAMEREQD` is passed so a
/// address with no PTR record fails outright instead of `getnameinfo`
/// silently falling back to the numeric address string as if it were a
/// resolved name.
#[cfg(unix)]
fn gethostbyaddr_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let addr_ptr = ctx.cpu.address_register(AddressRegister(0));
    let len = ctx.cpu.data_register(DataRegister(0)) as i32;
    let type_ = ctx.cpu.data_register(DataRegister(1)) as i32;

    let fail = |ctx: &mut HandlerContext<'_, C>, code: i32| {
        ctx.bsdsocket.set_herrno(ctx.mem, code);
        ctx.cpu.set_data_register(DataRegister(0), 0);
    };

    if type_ != AF_INET || len < 4 || addr_ptr == 0 {
        fail(ctx, HOST_NOT_FOUND);
        return Ok(());
    }
    let octets = [
        ctx.mem.read_u8(addr_ptr),
        ctx.mem.read_u8(addr_ptr.wrapping_add(1)),
        ctx.mem.read_u8(addr_ptr.wrapping_add(2)),
        ctx.mem.read_u8(addr_ptr.wrapping_add(3)),
    ];
    let orig_addr = Ipv4Addr::from(octets);

    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_addr.s_addr = u32::from(orig_addr).to_be();
    let mut host_buf = [0 as libc::c_char; libc::NI_MAXHOST as usize];
    let rc = unsafe {
        libc::getnameinfo(
            (&raw const sa).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            host_buf.as_mut_ptr(),
            host_buf.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if rc != 0 {
        fail(ctx, HOST_NOT_FOUND);
        return Ok(());
    }
    let name = unsafe { std::ffi::CStr::from_ptr(host_buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    let hostent = build_hostent(ctx, name.as_bytes(), &[orig_addr], -216, "gethostbyaddr")?;
    ctx.bsdsocket.set_errno(ctx.mem, 0);
    ctx.cpu.set_data_register(DataRegister(0), hostent);
    Ok(())
}

/// See [`gethostbyaddr_handler`]'s doc for the Unix version's real
/// implementation -- this runtime's own Windows support is unverified
/// at runtime (see the module docs' "WaitSelect" section for the same
/// caveat on that LVO), so this honestly reports "not supported" rather
/// than guessing at a `GetNameInfo`-based implementation nobody has run.
#[cfg(not(unix))]
fn gethostbyaddr_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.bsdsocket.set_herrno(ctx.mem, HOST_NOT_FOUND);
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// Shared `struct servent` builder for [`getservbyname_handler`]/
/// [`getservbyport_handler`] -- same "free the previous call's blocks,
/// build a fresh result" pattern as [`build_hostent`], separate result
/// buffer (see [`BsdSocketState::servent_allocs`]). `s_aliases` is
/// always a bare `NULL` terminator: the real host `getservbyname`/
/// `getservbyport` calls this wraps do return an alias list, but no
/// real corpus binary or conformance test reads it (matching
/// `build_hostent`'s own "no alias data available" `h_aliases`), so
/// it's not copied across. `port` is the *plain* (host-endianness- and
/// libc-transform-independent) port number, e.g. `80` for HTTP -- see
/// [`getservbyname_handler`]'s doc for why callers must decode the raw
/// host `libc::servent::s_port` value before passing it here, not pass
/// it through untouched.
#[cfg(unix)]
fn build_servent<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    name_bytes: &[u8],
    port: u16,
    proto_bytes: &[u8],
    lvo: i32,
    handler_name: &str,
) -> Result<u32, DispatchError> {
    for addr in std::mem::take(&mut ctx.bsdsocket.servent_allocs) {
        let _ = ctx.heap.free(addr);
    }
    let mut allocs = Vec::new();
    let mut alloc = |ctx: &mut HandlerContext<'_, C>, size: u32| -> Result<u32, DispatchError> {
        let addr = ctx
            .heap
            .alloc(size.max(4))
            .map_err(|e| DispatchError::HandlerFailed {
                library: "bsdsocket.library".to_string(),
                lvo,
                handler_name: handler_name.to_string(),
                message: format!("{handler_name}: guest heap allocation failed: {e}"),
            })?;
        allocs.push(addr);
        Ok(addr)
    };

    let name_buf = alloc(ctx, name_bytes.len() as u32 + 1)?;
    crate::guestmem::write_c_string(ctx.mem, name_buf, name_bytes);
    let aliases_arr = alloc(ctx, 4)?;
    ctx.mem.write_u32(aliases_arr, 0);
    let proto_buf = alloc(ctx, proto_bytes.len() as u32 + 1)?;
    crate::guestmem::write_c_string(ctx.mem, proto_buf, proto_bytes);

    let servent = alloc(ctx, 16)?;
    ctx.mem.write_u32(servent, name_buf); // s_name
    ctx.mem.write_u32(servent.wrapping_add(4), aliases_arr); // s_aliases
    ctx.mem.write_u32(servent.wrapping_add(8), port as u32); // s_port (plain value)
    ctx.mem.write_u32(servent.wrapping_add(12), proto_buf); // s_proto

    ctx.bsdsocket.servent_allocs = allocs;
    Ok(servent)
}

/// `getservbyname(name, proto)`. `D0` = a `struct servent*`, or `NULL`
/// if the host's own services database has no such entry (real,
/// documented "just returns NULL" contract -- no `h_errno`/`Errno()`
/// code is defined for this family of lookups). A real, blocking
/// lookup via the host's own `libc::getservbyname` -- the exact same
/// "trust the real OS resolver" posture `gethostbyname`/`gethostbyaddr`
/// already use, just a different libc entry point (the services
/// database, not DNS).
///
/// # `s_port`'s byte order
///
/// Real `libc::servent::s_port` holds the port in network byte order,
/// packed into a native-width `int` by ordinary C assignment (i.e. its
/// *numeric value*, not its raw memory bytes, equals what `htons()`
/// produced on the host) -- so on this project's little-endian
/// development host, `getservbyname("http", ...)`'s raw `s_port` is
/// `20480` (`0x5000`), not `80`. `m68k` is big-endian, so a real m68k
/// C compiler's `ntohs()` is a no-op there -- the guest just reads
/// `s_port`'s numeric value directly, so writing the *raw host* value
/// into guest memory would hand the guest `20480` instead of `80`.
/// `u16::from_be(raw_value as u16)` decodes the raw host value back to
/// the plain port number first (found the hard way: `bsdsocktest`'s
/// `getservbyname(): "http"/"tcp" -> port 80` test failing with the
/// wrong port, not a crash -- an easy bug to miss without a real
/// conformance run to catch it, since a synthetic same-host round-trip
/// test would have the identical mismatch on both sides and still
/// "pass").
#[cfg(unix)]
fn getservbyname_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    let proto_ptr = ctx.cpu.address_register(AddressRegister(1));
    let name_bytes = crate::guestmem::read_c_string(ctx.mem, name_ptr);
    let proto_bytes = crate::guestmem::read_c_string(ctx.mem, proto_ptr);
    let name_c = std::ffi::CString::new(name_bytes.clone()).unwrap_or_default();
    let proto_c = std::ffi::CString::new(proto_bytes.clone()).unwrap_or_default();

    let raw = unsafe { libc::getservbyname(name_c.as_ptr(), proto_c.as_ptr()) };
    if raw.is_null() {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }
    let port = u16::from_be(unsafe { (*raw).s_port } as u16);
    let servent = build_servent(ctx, &name_bytes, port, &proto_bytes, -234, "getservbyname")?;
    ctx.cpu.set_data_register(DataRegister(0), servent);
    Ok(())
}

/// `getservbyport(port, proto)`. `D0` = a `struct servent*`, or `NULL`.
/// `port` (`D0`) arrives from the guest as a *plain* port number (see
/// [`getservbyname_handler`]'s doc: the guest's own `htons()` is a
/// no-op on big-endian `m68k`, so `bsdsocktest`'s `getservbyport(htons
/// (21), ...)` call hands this backend `21` directly, unchanged) --
/// `.to_be()` re-encodes it into the network-order-as-native-int
/// representation the *host's* `libc::getservbyport` itself expects
/// (the mirror-image conversion of `getservbyname_handler`'s own
/// `u16::from_be` on the way out). Same real, blocking host lookup
/// otherwise.
#[cfg(unix)]
fn getservbyport_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let port = ctx.cpu.data_register(DataRegister(0)) as u16;
    let host_port = port.to_be() as libc::c_int;
    let proto_ptr = ctx.cpu.address_register(AddressRegister(0));
    let proto_bytes = crate::guestmem::read_c_string(ctx.mem, proto_ptr);
    let proto_c = std::ffi::CString::new(proto_bytes.clone()).unwrap_or_default();

    let raw = unsafe { libc::getservbyport(host_port, proto_c.as_ptr()) };
    if raw.is_null() {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }
    let (name_bytes, real_port) = unsafe {
        (
            std::ffi::CStr::from_ptr((*raw).s_name).to_bytes().to_vec(),
            u16::from_be((*raw).s_port as u16),
        )
    };
    let servent = build_servent(
        ctx,
        &name_bytes,
        real_port,
        &proto_bytes,
        -240,
        "getservbyport",
    )?;
    ctx.cpu.set_data_register(DataRegister(0), servent);
    Ok(())
}

/// Shared `struct protoent` builder for [`getprotobyname_handler`]/
/// [`getprotobynumber_handler`] -- same pattern as [`build_servent`],
/// separate result buffer (see [`BsdSocketState::protoent_allocs`]).
#[cfg(unix)]
fn build_protoent<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
    name_bytes: &[u8],
    proto: i32,
    lvo: i32,
    handler_name: &str,
) -> Result<u32, DispatchError> {
    for addr in std::mem::take(&mut ctx.bsdsocket.protoent_allocs) {
        let _ = ctx.heap.free(addr);
    }
    let mut allocs = Vec::new();
    let mut alloc = |ctx: &mut HandlerContext<'_, C>, size: u32| -> Result<u32, DispatchError> {
        let addr = ctx
            .heap
            .alloc(size.max(4))
            .map_err(|e| DispatchError::HandlerFailed {
                library: "bsdsocket.library".to_string(),
                lvo,
                handler_name: handler_name.to_string(),
                message: format!("{handler_name}: guest heap allocation failed: {e}"),
            })?;
        allocs.push(addr);
        Ok(addr)
    };

    let name_buf = alloc(ctx, name_bytes.len() as u32 + 1)?;
    crate::guestmem::write_c_string(ctx.mem, name_buf, name_bytes);
    let aliases_arr = alloc(ctx, 4)?;
    ctx.mem.write_u32(aliases_arr, 0);

    let protoent = alloc(ctx, 12)?;
    ctx.mem.write_u32(protoent, name_buf); // p_name
    ctx.mem.write_u32(protoent.wrapping_add(4), aliases_arr); // p_aliases
    ctx.mem.write_u32(protoent.wrapping_add(8), proto as u32); // p_proto

    ctx.bsdsocket.protoent_allocs = allocs;
    Ok(protoent)
}

/// `getprotobyname(name)`. `D0` = a `struct protoent*`, or `NULL`. Same
/// real, blocking host lookup posture as [`getservbyname_handler`], via
/// `libc::getprotobyname`.
#[cfg(unix)]
fn getprotobyname_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    let name_bytes = crate::guestmem::read_c_string(ctx.mem, name_ptr);
    let name_c = std::ffi::CString::new(name_bytes.clone()).unwrap_or_default();

    let raw = unsafe { libc::getprotobyname(name_c.as_ptr()) };
    if raw.is_null() {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }
    let proto = unsafe { (*raw).p_proto };
    let protoent = build_protoent(ctx, &name_bytes, proto, -246, "getprotobyname")?;
    ctx.cpu.set_data_register(DataRegister(0), protoent);
    Ok(())
}

/// `getprotobynumber(proto)`. `D0` = a `struct protoent*`, or `NULL`.
/// Same real, blocking host lookup posture, via
/// `libc::getprotobynumber`.
#[cfg(unix)]
fn getprotobynumber_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let proto = ctx.cpu.data_register(DataRegister(0)) as libc::c_int;
    let raw = unsafe { libc::getprotobynumber(proto) };
    if raw.is_null() {
        ctx.cpu.set_data_register(DataRegister(0), 0);
        return Ok(());
    }
    let name_bytes = unsafe { std::ffi::CStr::from_ptr((*raw).p_name).to_bytes().to_vec() };
    let protoent = build_protoent(ctx, &name_bytes, proto, -252, "getprotobynumber")?;
    ctx.cpu.set_data_register(DataRegister(0), protoent);
    Ok(())
}

/// `gethostname(name, namelen)`. `D0` = `0` on success, or `-1` with
/// `Errno()` set. A real host `libc::gethostname` call, truncated to
/// fit `namelen` the same way a real kernel truncates it (real BSD
/// `gethostname` silently truncates rather than failing when the
/// buffer is too small -- `bsdsocktest`'s own small-buffer test
/// tolerates either outcome, but truncating is the more real one).
#[cfg(unix)]
fn gethostname_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let name_ptr = ctx.cpu.address_register(AddressRegister(0));
    let namelen = ctx.cpu.data_register(DataRegister(0)) as i32;

    if namelen <= 0 {
        ctx.bsdsocket.set_errno(ctx.mem, EINVAL);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        return Ok(());
    }
    let mut buf = vec![0u8; namelen as usize];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        let code = translate_errno(&std::io::Error::last_os_error());
        ctx.bsdsocket.set_errno(ctx.mem, code);
        ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        return Ok(());
    }
    // Real gethostname NUL-terminates within namelen; a real host libc
    // call already did that, but ensure it here too in case the host's
    // name is exactly namelen bytes with no room left for the NUL (some
    // platforms don't guarantee truncation leaves room for it).
    buf[namelen as usize - 1] = 0;
    for (i, &b) in buf.iter().enumerate() {
        ctx.mem.write_u8(name_ptr.wrapping_add(i as u32), b);
    }
    ctx.bsdsocket.set_errno(ctx.mem, 0);
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}

/// `gethostid()`. `D0` = a real, non-zero host-identifying value, via
/// `libc::gethostid` -- the same real machine identifier real
/// `bsdsocket.library` also just forwards from the host TCP/IP stack's
/// own idea of it.
#[cfg(unix)]
fn gethostid_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let id = unsafe { libc::gethostid() };
    ctx.cpu.set_data_register(DataRegister(0), id as u32);
    Ok(())
}

/// See the Unix versions' own docs -- this runtime's own Windows
/// support is unverified at runtime, so these honestly report "not
/// found"/"not supported" rather than guessing at Winsock equivalents
/// nobody has run.
#[cfg(not(unix))]
fn getservbyname_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}
#[cfg(not(unix))]
fn getservbyport_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}
#[cfg(not(unix))]
fn getprotobyname_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}
#[cfg(not(unix))]
fn getprotobynumber_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 0);
    Ok(())
}
#[cfg(not(unix))]
fn gethostname_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.bsdsocket.set_errno(ctx.mem, EOPNOTSUPP);
    ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
    Ok(())
}
#[cfg(not(unix))]
fn gethostid_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    ctx.cpu.set_data_register(DataRegister(0), 0);
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
/// Tag codes implemented, matching what a real conformance suite
/// (`bsdsocktest`'s `testutil.c` and its `signals`/`misc` categories)
/// actually calls:
///
/// - `SBTC_ERRNOLONGPTR`/`SBTC_HERRNOLONGPTR` (`SET` and `GET`, by value
///   or by reference -- see [`BsdSocketState::errno_ptr`]/[`herrno_ptr`]):
///   the modern, tag-based equivalent of `SetErrnoPtr`, but for `errno`
///   and the *separate* `h_errno` channel respectively. `GET` lazily
///   allocates a real, live-updating storage location on first use (see
///   the match arms below) -- deliberately *not* replicating real
///   Roadshow's own documented bug here (`bsdsocktest`'s bundled
///   `docs/COMPATIBILITY.md`: "Roadshow supports SET but not readback of
///   the registered errno pointer"): that's an incidental defect in one
///   vendor's implementation, not part of `SocketBaseTagList`'s own
///   documented contract, so there's no reason to deliberately reproduce
///   it here the way this codebase reproduces genuine AmigaOS API
///   behavior elsewhere.
/// - `SBTC_BREAKMASK`/`SBTC_SIGEVENTMASK` (`SET`/`GET`): plain
///   round-trip storage, shared with [`set_socket_signals_handler`]'s
///   older `int_mask`/`io_mask` parameters.
/// - `SBTC_DTABLESIZE` (`SET`/`GET`): see [`BsdSocketState::dtablesize`].
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
                (SBTC_ERRNOLONGPTR, false) if by_ref => {
                    // Real bsdsocket.library always has a valid internal
                    // errno storage location, even before a caller ever
                    // calls SetErrnoPtr/sets this tag explicitly -- a
                    // caller is entitled to fetch it and read/write it
                    // directly from then on. Lazily allocate one on first
                    // GET and adopt it as errno_ptr, so it starts
                    // reflecting live updates from the very next
                    // set_errno call.
                    let ptr = match ctx.bsdsocket.errno_ptr {
                        Some(ptr) => ptr,
                        None => {
                            let addr =
                                ctx.heap
                                    .alloc(4)
                                    .map_err(|e| DispatchError::HandlerFailed {
                                        library: "bsdsocket.library".to_string(),
                                        lvo: -294,
                                        handler_name: "SocketBaseTagList".to_string(),
                                        message: format!(
                                            "SocketBaseTagList: guest heap allocation failed: {e}"
                                        ),
                                    })?;
                            ctx.mem.write_u32(addr, ctx.bsdsocket.last_errno as u32);
                            ctx.bsdsocket.errno_ptr = Some(addr);
                            addr
                        }
                    };
                    ctx.mem.write_u32(ti_data, ptr);
                }
                (SBTC_HERRNOLONGPTR, true) => {
                    let ptr = if by_ref {
                        ctx.mem.read_u32(ti_data)
                    } else {
                        ti_data
                    };
                    ctx.bsdsocket.herrno_ptr = if ptr == 0 { None } else { Some(ptr) };
                }
                (SBTC_HERRNOLONGPTR, false) if by_ref => {
                    // Same lazy-adoption pattern as SBTC_ERRNOLONGPTR's
                    // GET above, for the separate h_errno channel.
                    let ptr = match ctx.bsdsocket.herrno_ptr {
                        Some(ptr) => ptr,
                        None => {
                            let addr =
                                ctx.heap
                                    .alloc(4)
                                    .map_err(|e| DispatchError::HandlerFailed {
                                        library: "bsdsocket.library".to_string(),
                                        lvo: -294,
                                        handler_name: "SocketBaseTagList".to_string(),
                                        message: format!(
                                            "SocketBaseTagList: guest heap allocation failed: {e}"
                                        ),
                                    })?;
                            ctx.mem.write_u32(addr, ctx.bsdsocket.last_herrno as u32);
                            ctx.bsdsocket.herrno_ptr = Some(addr);
                            addr
                        }
                    };
                    ctx.mem.write_u32(ti_data, ptr);
                }
                (SBTC_BREAKMASK, true) => {
                    ctx.bsdsocket.breakmask = if by_ref {
                        ctx.mem.read_u32(ti_data)
                    } else {
                        ti_data
                    };
                }
                (SBTC_BREAKMASK, false) if by_ref => {
                    ctx.mem.write_u32(ti_data, ctx.bsdsocket.breakmask);
                }
                (SBTC_SIGEVENTMASK, true) => {
                    ctx.bsdsocket.sigeventmask = if by_ref {
                        ctx.mem.read_u32(ti_data)
                    } else {
                        ti_data
                    };
                }
                (SBTC_SIGEVENTMASK, false) if by_ref => {
                    ctx.mem.write_u32(ti_data, ctx.bsdsocket.sigeventmask);
                }
                (SBTC_DTABLESIZE, true) => {
                    let requested = if by_ref {
                        ctx.mem.read_u32(ti_data)
                    } else {
                        ti_data
                    };
                    // A real caller can only ever grow this (see
                    // bsdsocktest's own "may not reduce" comment); clamp
                    // against unbounded guest-driven HashMap growth the
                    // same way MAX_OPEN_SOCKETS already bounds it, one
                    // sane ceiling for a backend with no real OS-level fd
                    // table to actually expand.
                    ctx.bsdsocket.dtablesize = requested
                        .max(MAX_OPEN_SOCKETS as u32)
                        .min(4096)
                        .max(ctx.bsdsocket.dtablesize);
                }
                (SBTC_DTABLESIZE, false) if by_ref => {
                    ctx.mem.write_u32(ti_data, ctx.bsdsocket.dtablesize);
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
    ctx.bsdsocket.sockets.insert(id, SocketEntry::new(dup));
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

/// `GetSocketEvents(ULONG *event_ptr)`. `D0` = the fd of one socket with
/// a real, armed `SO_EVENTMASK` event pending (see [`wait_select_poll`]'s
/// event-synthesis pass, which is what actually detects and queues these
/// onto [`SocketEntry::pending_events`]), with `*event_ptr` set to that
/// socket's consumed `FD_*` bits -- or `-1` (leaving `*event_ptr`
/// untouched) if nothing is pending. Retrieval consumes the event (real
/// documented behavior: a second immediate call sees nothing for that
/// fd until it becomes ready again). When more than one socket has a
/// pending event, scans in fd order starting just after
/// [`BsdSocketState::event_cursor`] (wrapping back to the lowest fd) --
/// real `GetSocketEvents` is documented to round-robin across sockets
/// with pending events rather than always favoring the same one,
/// confirmed against `bsdsocktest`'s own round-robin test.
fn get_socket_events_handler<C: Cpu>(ctx: &mut HandlerContext<'_, C>) -> Result<(), DispatchError> {
    let event_ptr = ctx.cpu.address_register(AddressRegister(0));

    let mut candidates: Vec<i32> = ctx
        .bsdsocket
        .sockets
        .iter()
        .filter(|(_, e)| e.pending_events != 0)
        .map(|(&fd, _)| fd)
        .collect();
    candidates.sort_unstable();
    let cursor = ctx.bsdsocket.event_cursor;
    let next = candidates
        .iter()
        .find(|&&fd| fd > cursor)
        .or_else(|| candidates.first())
        .copied();

    match next {
        Some(fd) => {
            let entry = ctx
                .bsdsocket
                .sockets
                .get_mut(&fd)
                .expect("fd came from this same sockets map");
            let events = entry.pending_events;
            entry.pending_events = 0;
            ctx.bsdsocket.event_cursor = fd;
            ctx.mem.write_u32(event_ptr, events);
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), fd as u32);
        }
        None => {
            ctx.bsdsocket.set_errno(ctx.mem, 0);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
        }
    }
    Ok(())
}

/// `SetSocketSignals(int_mask, io_mask, urgent_mask)`. No return value.
/// A simpler, older predecessor of `SocketBaseTagList`'s
/// `SBTC_BREAKMASK`/`SBTC_SIGEVENTMASK` tags (same real historical
/// relationship as `SetErrnoPtr` to `SBTC_ERRNOLONGPTR`) -- `int_mask`
/// and `io_mask` map onto exactly those same two pieces of state
/// ([`BsdSocketState::breakmask`]/[`BsdSocketState::sigeventmask`]), so
/// setting either one here is immediately visible through the newer
/// API too, and vice versa. `urgent_mask` (a signal for OOB data across
/// any socket) is accepted but not wired to real delivery -- this
/// backend doesn't detect OOB data at all yet (see the module docs'
/// `FD_OOB` note in the `SO_EVENTMASK`/`GetSocketEvents` section, the
/// same gap `WaitSelect`'s own `exceptfds` has).
fn set_socket_signals_handler<C: Cpu>(
    ctx: &mut HandlerContext<'_, C>,
) -> Result<(), DispatchError> {
    let int_mask = ctx.cpu.data_register(DataRegister(0));
    let io_mask = ctx.cpu.data_register(DataRegister(1));
    ctx.bsdsocket.breakmask = int_mask;
    ctx.bsdsocket.sigeventmask = io_mask;
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
    reg!("Inet_LnaOf", inet_lnaof_handler::<C>);
    reg!("Inet_NetOf", inet_netof_handler::<C>);
    reg!("Inet_MakeAddr", inet_makeaddr_handler::<C>);
    reg!("gethostbyname", gethostbyname_handler::<C>);
    reg!("gethostbyaddr", gethostbyaddr_handler::<C>);
    reg!("getservbyname", getservbyname_handler::<C>);
    reg!("getservbyport", getservbyport_handler::<C>);
    reg!("getprotobyname", getprotobyname_handler::<C>);
    reg!("getprotobynumber", getprotobynumber_handler::<C>);
    reg!("gethostname", gethostname_handler::<C>);
    reg!("gethostid", gethostid_handler::<C>);
    reg!("SocketBaseTagList", socket_base_tag_list_handler::<C>);
    reg!("Dup2Socket", dup2_socket_handler::<C>);
    reg!("sendmsg", sendmsg_handler::<C>);
    reg!("recvmsg", recvmsg_handler::<C>);
    reg!("vsyslog", vsyslog_handler::<C>);
    reg!("ObtainSocket", release_socket_handler::<C>);
    reg!("ReleaseSocket", release_socket_handler::<C>);
    reg!("ReleaseCopyOfSocket", release_socket_handler::<C>);
    reg!("GetSocketEvents", get_socket_events_handler::<C>);
    reg!("SetSocketSignals", set_socket_signals_handler::<C>);
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
    fn end_to_end_gethostbyaddr_resolves_loopback_via_the_real_host_resolver() {
        let addr_buf: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, addr_buf);
        push_move_imm_d(&mut words, 0, 4); // len
        push_move_imm_d(&mut words, 1, AF_INET as u32); // type
        words.extend_from_slice(&jsr_disp16_a6(-216)); // gethostbyaddr -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        mem.write_u32(addr_buf, u32::from(Ipv4Addr::LOCALHOST));
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
        assert_ne!(
            hostent, 0,
            "127.0.0.1 should always have some real PTR record on a real host"
        );

        let mem = rt.memory();
        assert_eq!(mem.read_u32(hostent + 8), AF_INET as u32, "h_addrtype");
        assert_eq!(mem.read_u32(hostent + 12), 4, "h_length");
        let addr_list = mem.read_u32(hostent + 16);
        let first_addr_ptr = mem.read_u32(addr_list);
        assert_eq!(
            mem.read_u32(first_addr_ptr),
            u32::from(Ipv4Addr::LOCALHOST),
            "h_addr_list[0] should echo back the original queried address"
        );
        let name_ptr = mem.read_u32(hostent);
        let name = crate::guestmem::read_c_string(mem, name_ptr);
        assert!(
            !name.is_empty(),
            "a real resolved PTR name should be non-empty"
        );
    }

    #[test]
    fn end_to_end_gethostbyaddr_rejects_a_non_af_inet_type_with_host_not_found() {
        let addr_buf: u32 = 0x1_8000;
        let herrno_addr: u32 = 0x1_8100;
        let tags_addr: u32 = 0x1_8200;

        const SBTM_SETVAL_HERRNOLONGPTR: u32 = TAG_USER | (SBTC_HERRNOLONGPTR << 1) | SBTF_SET;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, tags_addr);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // register h_errno mirror

        push_move_imm_a(&mut words, 0, addr_buf);
        push_move_imm_d(&mut words, 0, 4);
        push_move_imm_d(&mut words, 1, 99); // an unsupported address family
        words.extend_from_slice(&jsr_disp16_a6(-216)); // gethostbyaddr -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        mem.write_u32(addr_buf, u32::from(Ipv4Addr::LOCALHOST));
        mem.write_u32(tags_addr, SBTM_SETVAL_HERRNOLONGPTR);
        mem.write_u32(tags_addr.wrapping_add(4), herrno_addr);
        mem.write_u32(tags_addr.wrapping_add(8), TAG_DONE);
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
        assert_eq!(code, 0, "gethostbyaddr should return NULL on failure");
        assert_eq!(
            rt.memory().read_u32(herrno_addr),
            HOST_NOT_FOUND as u32,
            "h_errno should report HOST_NOT_FOUND for an unsupported address family"
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

    #[test]
    fn end_to_end_socket_base_tags_get_lazily_allocates_errno_and_herrno_pointers() {
        let errno_out: u32 = 0x1_8000;
        let herrno_out: u32 = 0x1_8100;
        let tags_addr: u32 = 0x1_8200;

        const SBTM_GETREF_ERRNOLONGPTR: u32 = TAG_USER | SBTF_REF | (SBTC_ERRNOLONGPTR << 1);
        const SBTM_GETREF_HERRNOLONGPTR: u32 = TAG_USER | SBTF_REF | (SBTC_HERRNOLONGPTR << 1);

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, tags_addr);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // SocketBaseTagList() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        mem.write_u32(tags_addr, SBTM_GETREF_ERRNOLONGPTR);
        mem.write_u32(tags_addr.wrapping_add(4), errno_out);
        mem.write_u32(tags_addr.wrapping_add(8), SBTM_GETREF_HERRNOLONGPTR);
        mem.write_u32(tags_addr.wrapping_add(12), herrno_out);
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
        rt.run(&mut out, None).expect("run should succeed");

        assert_ne!(
            rt.memory().read_u32(errno_out),
            0,
            "SBTC_ERRNOLONGPTR GET should hand back a real, valid pointer"
        );
        assert_ne!(
            rt.memory().read_u32(herrno_out),
            0,
            "SBTC_HERRNOLONGPTR GET should hand back a real, valid pointer"
        );
    }

    #[test]
    fn end_to_end_socket_base_tags_dtablesize_round_trips_and_raises_getdtablesize() {
        let before_out: u32 = 0x1_8000;
        let after_out: u32 = 0x1_8100;
        let tags1: u32 = 0x1_8200;
        let tags2: u32 = 0x1_8300;

        const SBTM_GETREF_DTABLESIZE: u32 = TAG_USER | SBTF_REF | (SBTC_DTABLESIZE << 1);
        const SBTM_SETVAL_DTABLESIZE: u32 = TAG_USER | (SBTC_DTABLESIZE << 1) | SBTF_SET;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, tags1);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // GET before
        push_move_imm_a(&mut words, 0, tags2);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // SET 128, then GET after
        words.extend_from_slice(&jsr_disp16_a6(-138)); // getdtablesize() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        mem.write_u32(tags1, SBTM_GETREF_DTABLESIZE);
        mem.write_u32(tags1.wrapping_add(4), before_out);
        mem.write_u32(tags1.wrapping_add(8), TAG_DONE);
        mem.write_u32(tags2, SBTM_SETVAL_DTABLESIZE);
        mem.write_u32(tags2.wrapping_add(4), 128);
        mem.write_u32(tags2.wrapping_add(8), SBTM_GETREF_DTABLESIZE);
        mem.write_u32(tags2.wrapping_add(12), after_out);
        mem.write_u32(tags2.wrapping_add(16), TAG_DONE);
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

        assert!(rt.memory().read_u32(before_out) >= MAX_OPEN_SOCKETS as u32);
        assert!(rt.memory().read_u32(after_out) >= 128);
        assert!(
            code >= 128,
            "getdtablesize() should reflect the SBTC_DTABLESIZE change, got {code}"
        );
    }

    #[test]
    fn end_to_end_socket_base_tags_sigeventmask_and_breakmask_round_trip() {
        let out1: u32 = 0x1_8000;
        let out2: u32 = 0x1_8100;
        let tags: u32 = 0x1_8200;

        const SBTM_SETVAL_SIGEVENTMASK: u32 = TAG_USER | (SBTC_SIGEVENTMASK << 1) | SBTF_SET;
        const SBTM_GETREF_SIGEVENTMASK: u32 = TAG_USER | SBTF_REF | (SBTC_SIGEVENTMASK << 1);
        const SBTM_SETVAL_BREAKMASK: u32 = TAG_USER | (SBTC_BREAKMASK << 1) | SBTF_SET;
        const SBTM_GETREF_BREAKMASK: u32 = TAG_USER | SBTF_REF | (SBTC_BREAKMASK << 1);

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, tags);
        words.extend_from_slice(&jsr_disp16_a6(-294));
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        mem.write_u32(tags, SBTM_SETVAL_SIGEVENTMASK);
        mem.write_u32(tags.wrapping_add(4), 0x10000);
        mem.write_u32(tags.wrapping_add(8), SBTM_GETREF_SIGEVENTMASK);
        mem.write_u32(tags.wrapping_add(12), out1);
        mem.write_u32(tags.wrapping_add(16), SBTM_SETVAL_BREAKMASK);
        mem.write_u32(tags.wrapping_add(20), 0x20000);
        mem.write_u32(tags.wrapping_add(24), SBTM_GETREF_BREAKMASK);
        mem.write_u32(tags.wrapping_add(28), out2);
        mem.write_u32(tags.wrapping_add(32), TAG_DONE);
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
        rt.run(&mut out, None).expect("run should succeed");

        assert_eq!(rt.memory().read_u32(out1), 0x10000);
        assert_eq!(rt.memory().read_u32(out2), 0x20000);
    }

    #[test]
    fn end_to_end_so_eventmask_fd_read_fires_and_is_consumed_by_getsocketevents() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a real listener");
        let port = listener.local_addr().unwrap().port();
        let accept_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            std::thread::sleep(std::time::Duration::from_millis(150));
            stream.write_all(b"hi").expect("write_all");
        });

        let sockaddr_buf: u32 = 0x1_8000;
        let eventmask_buf: u32 = 0x1_8100;
        let sigmask_buf: u32 = 0x1_8200;
        let timeout_buf: u32 = 0x1_8300;
        let tags_buf: u32 = 0x1_8400;
        let evmask_out: u32 = 0x1_8500;
        const USER_SIGNAL_BIT: u32 = 0x0001_0000;
        const SBTM_SETVAL_SIGEVENTMASK: u32 = TAG_USER | (SBTC_SIGEVENTMASK << 1) | SBTF_SET;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, tags_buf);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // SocketBaseTagList: arm SIGEVENTMASK

        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd (1)
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_d(&mut words, 1, 16);
        words.extend_from_slice(&jsr_disp16_a6(-54)); // connect() -> D0

        push_move_imm_d(&mut words, 0, 1); // fd
        push_move_imm_d(&mut words, 1, SOL_SOCKET as u32);
        push_move_imm_d(&mut words, 2, SO_EVENTMASK as u32);
        push_move_imm_a(&mut words, 0, eventmask_buf);
        push_move_imm_d(&mut words, 3, 4);
        words.extend_from_slice(&jsr_disp16_a6(-90)); // setsockopt(fd, SO_EVENTMASK, FD_READ)

        push_move_imm_d(&mut words, 0, 0); // nfds = 0
        push_move_imm_a(&mut words, 0, 0);
        push_move_imm_a(&mut words, 1, 0);
        push_move_imm_a(&mut words, 2, 0);
        push_move_imm_a(&mut words, 3, timeout_buf);
        push_move_imm_d(&mut words, 1, sigmask_buf);
        words.extend_from_slice(&jsr_disp16_a6(-126)); // WaitSelect()

        push_move_imm_a(&mut words, 0, evmask_out);
        words.extend_from_slice(&jsr_disp16_a6(-300)); // GetSocketEvents() -> D0 (fd), *evmask_out = bits

        push_move_imm_a(&mut words, 0, evmask_out.wrapping_add(4));
        words.extend_from_slice(&jsr_disp16_a6(-300)); // GetSocketEvents() again -> D0 (should be -1)
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        write_sockaddr_in(
            &mut mem,
            sockaddr_buf,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        );
        mem.write_u32(eventmask_buf, FD_READ);
        mem.write_u32(sigmask_buf, USER_SIGNAL_BIT);
        write_timeval(
            &mut mem,
            timeout_buf,
            Some(std::time::Duration::from_secs(2)),
        );
        mem.write_u32(tags_buf, SBTM_SETVAL_SIGEVENTMASK);
        mem.write_u32(tags_buf.wrapping_add(4), USER_SIGNAL_BIT);
        mem.write_u32(tags_buf.wrapping_add(8), TAG_DONE);
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

        assert_ne!(
            rt.memory().read_u32(evmask_out) & FD_READ,
            0,
            "the first GetSocketEvents() call should report FD_READ"
        );
        assert_eq!(
            code, -1,
            "a second GetSocketEvents() call should see the event already consumed"
        );
        assert_eq!(
            rt.memory().read_u32(evmask_out.wrapping_add(4)),
            0,
            "no data was written for the second (nothing pending) call"
        );
    }

    #[test]
    fn end_to_end_so_eventmask_fd_accept_fires_on_listener() {
        let sockaddr_buf: u32 = 0x1_8000;
        let eventmask_buf: u32 = 0x1_8100;
        let sigmask_buf: u32 = 0x1_8200;
        let timeout_buf: u32 = 0x1_8300;
        let tags_buf: u32 = 0x1_8400;
        let evmask_out: u32 = 0x1_8500;
        const USER_SIGNAL_BIT: u32 = 0x0001_0000;
        const SBTM_SETVAL_SIGEVENTMASK: u32 = TAG_USER | (SBTC_SIGEVENTMASK << 1) | SBTF_SET;
        let port = 58245u16;

        let connect_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect")
        });

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, tags_buf);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // arm SIGEVENTMASK

        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd (1)
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_d(&mut words, 1, 16);
        words.extend_from_slice(&jsr_disp16_a6(-36)); // bind() -- overwrites D0 with 0
        push_move_imm_d(&mut words, 0, 1); // D0 = fd (1, the only socket) -- reload after bind()
        push_move_imm_d(&mut words, 1, 1);
        words.extend_from_slice(&jsr_disp16_a6(-42)); // listen()

        push_move_imm_d(&mut words, 0, 1);
        push_move_imm_d(&mut words, 1, SOL_SOCKET as u32);
        push_move_imm_d(&mut words, 2, SO_EVENTMASK as u32);
        push_move_imm_a(&mut words, 0, eventmask_buf);
        push_move_imm_d(&mut words, 3, 4);
        words.extend_from_slice(&jsr_disp16_a6(-90)); // setsockopt(fd, SO_EVENTMASK, FD_ACCEPT)

        push_move_imm_d(&mut words, 0, 0);
        push_move_imm_a(&mut words, 0, 0);
        push_move_imm_a(&mut words, 1, 0);
        push_move_imm_a(&mut words, 2, 0);
        push_move_imm_a(&mut words, 3, timeout_buf);
        push_move_imm_d(&mut words, 1, sigmask_buf);
        words.extend_from_slice(&jsr_disp16_a6(-126)); // WaitSelect()

        push_move_imm_a(&mut words, 0, evmask_out);
        words.extend_from_slice(&jsr_disp16_a6(-300)); // GetSocketEvents() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        write_sockaddr_in(
            &mut mem,
            sockaddr_buf,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        );
        mem.write_u32(eventmask_buf, FD_ACCEPT);
        mem.write_u32(sigmask_buf, USER_SIGNAL_BIT);
        write_timeval(
            &mut mem,
            timeout_buf,
            Some(std::time::Duration::from_secs(2)),
        );
        mem.write_u32(tags_buf, SBTM_SETVAL_SIGEVENTMASK);
        mem.write_u32(tags_buf.wrapping_add(4), USER_SIGNAL_BIT);
        mem.write_u32(tags_buf.wrapping_add(8), TAG_DONE);
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
        let _ = connect_thread.join().unwrap();

        assert_eq!(
            code, 1,
            "GetSocketEvents should report the listener's own fd"
        );
        assert_ne!(
            rt.memory().read_u32(evmask_out) & FD_ACCEPT,
            0,
            "FD_ACCEPT should be among the reported bits"
        );
    }

    #[test]
    fn end_to_end_so_eventmask_no_spurious_events_on_idle_socket() {
        let timeout_buf: u32 = 0x1_8100;
        let eventmask_buf: u32 = 0x1_8200;
        let tags_buf: u32 = 0x1_8300;
        let evmask_out: u32 = 0x1_8400;
        const USER_SIGNAL_BIT: u32 = 0x0001_0000;
        const SBTM_SETVAL_SIGEVENTMASK: u32 = TAG_USER | (SBTC_SIGEVENTMASK << 1) | SBTF_SET;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, tags_buf);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // arm SIGEVENTMASK

        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd (1), never connected

        push_move_imm_d(&mut words, 0, 1);
        push_move_imm_d(&mut words, 1, SOL_SOCKET as u32);
        push_move_imm_d(&mut words, 2, SO_EVENTMASK as u32);
        push_move_imm_a(&mut words, 0, eventmask_buf);
        push_move_imm_d(&mut words, 3, 4);
        words.extend_from_slice(&jsr_disp16_a6(-90)); // setsockopt: arm FD_READ|FD_WRITE|FD_CONNECT

        push_move_imm_d(&mut words, 0, 0);
        push_move_imm_a(&mut words, 0, 0);
        push_move_imm_a(&mut words, 1, 0);
        push_move_imm_a(&mut words, 2, 0);
        push_move_imm_a(&mut words, 3, timeout_buf);
        push_move_imm_d(&mut words, 1, 0); // no signal mask -- checking pure poll behavior
        words.extend_from_slice(&jsr_disp16_a6(-126)); // WaitSelect(), 100ms

        push_move_imm_a(&mut words, 0, evmask_out);
        words.extend_from_slice(&jsr_disp16_a6(-300)); // GetSocketEvents() -> D0 (expect -1)
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        mem.write_u32(eventmask_buf, FD_READ | FD_WRITE | FD_CONNECT);
        write_timeval(
            &mut mem,
            timeout_buf,
            Some(std::time::Duration::from_millis(100)),
        );
        mem.write_u32(tags_buf, SBTM_SETVAL_SIGEVENTMASK);
        mem.write_u32(tags_buf.wrapping_add(4), USER_SIGNAL_BIT);
        mem.write_u32(tags_buf.wrapping_add(8), TAG_DONE);
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
            code, -1,
            "no event should ever fire on a never-connected socket"
        );
        assert_eq!(
            rt.memory().read_u32(task + exectask::TC_SIGRECVD) & USER_SIGNAL_BIT,
            0,
            "no spurious signal should be delivered either"
        );
    }

    #[test]
    fn end_to_end_set_socket_signals_shares_state_with_socket_base_tags() {
        let out1: u32 = 0x1_8000;
        let out2: u32 = 0x1_8100;
        let tags: u32 = 0x1_8200;

        const SBTM_GETREF_BREAKMASK: u32 = TAG_USER | SBTF_REF | (SBTC_BREAKMASK << 1);
        const SBTM_GETREF_SIGEVENTMASK: u32 = TAG_USER | SBTF_REF | (SBTC_SIGEVENTMASK << 1);

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, 0x1000); // int_mask
        push_move_imm_d(&mut words, 1, 0x2000); // io_mask
        push_move_imm_d(&mut words, 2, 0); // urgent_mask
        words.extend_from_slice(&jsr_disp16_a6(-132)); // SetSocketSignals()

        push_move_imm_a(&mut words, 0, tags);
        words.extend_from_slice(&jsr_disp16_a6(-294)); // SocketBaseTagList: read both back
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        mem.write_u32(tags, SBTM_GETREF_BREAKMASK);
        mem.write_u32(tags.wrapping_add(4), out1);
        mem.write_u32(tags.wrapping_add(8), SBTM_GETREF_SIGEVENTMASK);
        mem.write_u32(tags.wrapping_add(12), out2);
        mem.write_u32(tags.wrapping_add(16), TAG_DONE);
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
        rt.run(&mut out, None).expect("run should succeed");

        assert_eq!(rt.memory().read_u32(out1), 0x1000);
        assert_eq!(rt.memory().read_u32(out2), 0x2000);
    }

    #[test]
    fn end_to_end_getservbyname_http_tcp_resolves_to_port_80() {
        let name_addr: u32 = 0x1_8000;
        let proto_addr: u32 = 0x1_8100;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, name_addr);
        push_move_imm_a(&mut words, 1, proto_addr);
        words.extend_from_slice(&jsr_disp16_a6(-234)); // getservbyname -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, name_addr, b"http");
        crate::guestmem::write_c_string(&mut mem, proto_addr, b"tcp");
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
        let servent = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_ne!(
            servent, 0,
            "\"http\"/\"tcp\" should be a real, well-known service on any real host"
        );
        // s_port holds the plain port number (m68k's own ntohs() is a
        // no-op there -- see getservbyname_handler's doc for the real
        // byte-order bug this guards against), so no decode needed here.
        assert_eq!(rt.memory().read_u32(servent + 8), 80);
    }

    #[test]
    fn end_to_end_getservbyport_21_tcp_resolves_to_ftp() {
        let proto_addr: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        // A real m68k caller's own htons(21) is a no-op (m68k is
        // big-endian) -- this backend receives the plain port number 21
        // directly, matching what getservbyport_handler's own doc says
        // to expect.
        push_move_imm_d(&mut words, 0, 21);
        push_move_imm_a(&mut words, 0, proto_addr);
        words.extend_from_slice(&jsr_disp16_a6(-240)); // getservbyport -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, proto_addr, b"tcp");
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
        let servent = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_ne!(
            servent, 0,
            "port 21/\"tcp\" should be a real, well-known service on any real host"
        );
        let name_ptr = rt.memory().read_u32(servent);
        let name = crate::guestmem::read_c_string(rt.memory(), name_ptr);
        assert_eq!(name.to_ascii_lowercase(), b"ftp");
    }

    #[test]
    fn end_to_end_getprotobyname_tcp_resolves_to_protocol_6() {
        let name_addr: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, name_addr);
        words.extend_from_slice(&jsr_disp16_a6(-246)); // getprotobyname -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        crate::guestmem::write_c_string(&mut mem, name_addr, b"tcp");
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
        let protoent = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_ne!(protoent, 0, "\"tcp\" should be a real, well-known protocol");
        assert_eq!(rt.memory().read_u32(protoent + 8), 6, "p_proto");
    }

    #[test]
    fn end_to_end_gethostname_retrieves_a_real_non_empty_name() {
        let buf: u32 = 0x1_8000;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, buf);
        push_move_imm_d(&mut words, 0, 256);
        words.extend_from_slice(&jsr_disp16_a6(-282)); // gethostname -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        assert_eq!(code, 0, "gethostname should succeed on any real host");
        let name = crate::guestmem::read_c_string(rt.memory(), buf);
        assert!(
            !name.is_empty(),
            "a real host always has a non-empty hostname"
        );
    }

    #[test]
    fn end_to_end_gethostid_runs_and_returns_the_real_host_value() {
        // Not asserted non-zero: a real host's own gethostid() (this
        // backend's own `libc::gethostid` passthrough) legitimately
        // returns 0 on a machine that never had one configured (true on
        // this project's own macOS dev host) -- this test only confirms
        // the real host call round-trips into D0 without crashing.
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        words.extend_from_slice(&jsr_disp16_a6(-288)); // gethostid -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed");
        let host_id = unsafe { libc::gethostid() } as i32;
        assert_eq!(
            code, host_id,
            "should mirror the real host's own gethostid()"
        );
    }

    #[test]
    fn end_to_end_inet_lnaof_netof_makeaddr_round_trip_class_a() {
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, 0x0a01_0203); // 10.1.2.3
        words.extend_from_slice(&jsr_disp16_a6(-186)); // Inet_LnaOf -> D0
        words.push(move_d0_to_d(3)); // D3 = host part

        push_move_imm_d(&mut words, 0, 0x0a01_0203);
        words.extend_from_slice(&jsr_disp16_a6(-192)); // Inet_NetOf -> D0
        words.push(move_d0_to_d(4)); // D4 = net part

        const MOVE_D4_TO_D0: u16 = 0x2004;
        const MOVE_D3_TO_D1: u16 = 0x2203;
        words.push(MOVE_D4_TO_D0);
        words.push(MOVE_D3_TO_D1);
        words.extend_from_slice(&jsr_disp16_a6(-198)); // Inet_MakeAddr(net, host) -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_eq!(
            code, 0x0a01_0203,
            "Inet_MakeAddr(NetOf(x), LnaOf(x)) should round-trip to x"
        );
    }

    #[test]
    fn end_to_end_inet_lnaof_extracts_class_a_host_part() {
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, 0x0a01_0203); // 10.1.2.3
        words.extend_from_slice(&jsr_disp16_a6(-186)); // Inet_LnaOf -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_eq!(code, 0x01_0203);
    }

    #[test]
    fn end_to_end_inet_netof_extracts_class_a_net_part() {
        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, 0x0a01_0203); // 10.1.2.3
        words.extend_from_slice(&jsr_disp16_a6(-192)); // Inet_NetOf -> D0
        words.push(RTS);

        let mut rt = runtime_with_program(&words);
        let mut out = Vec::new();
        let code = rt.run(&mut out, None).expect("run should succeed") as u32;
        assert_eq!(code, 0x0a);
    }

    #[test]
    fn end_to_end_sendmsg_recvmsg_scatter_gather_round_trip() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a real listener");
        let port = listener.local_addr().unwrap().port();
        let accept_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 100];
            stream.read_exact(&mut buf).expect("read_exact");
            buf
        });

        let sockaddr_buf: u32 = 0x1_8000;
        let send_data: u32 = 0x1_8100; // 100 bytes
        let send_msg: u32 = 0x1_8200; // struct msghdr (28 bytes)
        let send_iov: u32 = 0x1_8300; // 3 iovec entries (50+30+20 = 100)

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd (1)
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_d(&mut words, 1, 16);
        words.extend_from_slice(&jsr_disp16_a6(-54)); // connect() -> D0

        push_move_imm_d(&mut words, 0, 1); // fd
        push_move_imm_a(&mut words, 0, send_msg);
        push_move_imm_d(&mut words, 1, 0); // flags
        words.extend_from_slice(&jsr_disp16_a6(-270)); // sendmsg() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        write_sockaddr_in(
            &mut mem,
            sockaddr_buf,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        );
        let pattern: Vec<u8> = (0..100).map(|i| (i * 3 + 7) as u8).collect();
        for (i, &b) in pattern.iter().enumerate() {
            mem.write_u8(send_data + i as u32, b);
        }
        // struct msghdr: msg_name=0, msg_namelen=0, msg_iov, msg_iovlen=3,
        // msg_control=0, msg_controllen=0, msg_flags=0
        mem.write_u32(send_msg, 0);
        mem.write_u32(send_msg + 4, 0);
        mem.write_u32(send_msg + 8, send_iov);
        mem.write_u32(send_msg + 12, 3);
        mem.write_u32(send_msg + 16, 0);
        mem.write_u32(send_msg + 20, 0);
        mem.write_u32(send_msg + 24, 0);
        // 3 iovecs: 50 + 30 + 20 = 100 bytes, into the same send_data buffer
        mem.write_u32(send_iov, send_data);
        mem.write_u32(send_iov + 4, 50);
        mem.write_u32(send_iov + 8, send_data + 50);
        mem.write_u32(send_iov + 12, 30);
        mem.write_u32(send_iov + 16, send_data + 80);
        mem.write_u32(send_iov + 20, 20);

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
        let received = accept_thread.join().unwrap();

        assert_eq!(code, 100, "sendmsg() should report all 100 bytes sent");
        assert_eq!(
            received.to_vec(),
            pattern,
            "the real host peer should see the gathered bytes, in order"
        );
    }

    #[test]
    fn end_to_end_recvmsg_scatters_across_multiple_iovecs() {
        let sender = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a real listener");
        let port = sender.local_addr().unwrap().port();
        let pattern: Vec<u8> = (0..100).map(|i| (i * 5 + 3) as u8).collect();
        let pattern_for_thread = pattern.clone();
        let send_thread = std::thread::spawn(move || {
            let (mut stream, _) = sender.accept().expect("accept");
            stream.write_all(&pattern_for_thread).expect("write_all");
        });

        let sockaddr_buf: u32 = 0x1_8000;
        let recv_data: u32 = 0x1_8100; // 100 bytes, scattered across 3 iovecs
        let recv_msg: u32 = 0x1_8200;
        let recv_iov: u32 = 0x1_8300;

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_d(&mut words, 0, AF_INET as u32);
        push_move_imm_d(&mut words, 1, SOCK_STREAM as u32);
        push_move_imm_d(&mut words, 2, 0);
        words.extend_from_slice(&jsr_disp16_a6(-30)); // socket() -> D0 = fd (1)
        push_move_imm_a(&mut words, 0, sockaddr_buf);
        push_move_imm_d(&mut words, 1, 16);
        words.extend_from_slice(&jsr_disp16_a6(-54)); // connect() -> D0

        push_move_imm_d(&mut words, 0, 1); // fd
        push_move_imm_a(&mut words, 0, recv_msg);
        push_move_imm_d(&mut words, 1, 0); // flags
        words.extend_from_slice(&jsr_disp16_a6(-276)); // recvmsg() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        write_sockaddr_in(
            &mut mem,
            sockaddr_buf,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        );
        mem.write_u32(recv_msg, 0);
        mem.write_u32(recv_msg + 4, 0);
        mem.write_u32(recv_msg + 8, recv_iov);
        mem.write_u32(recv_msg + 12, 3);
        mem.write_u32(recv_msg + 16, 0);
        mem.write_u32(recv_msg + 20, 0);
        mem.write_u32(recv_msg + 24, 0xDEAD_BEEF); // should be overwritten with 0
        mem.write_u32(recv_iov, recv_data);
        mem.write_u32(recv_iov + 4, 50);
        mem.write_u32(recv_iov + 8, recv_data + 50);
        mem.write_u32(recv_iov + 12, 30);
        mem.write_u32(recv_iov + 16, recv_data + 80);
        mem.write_u32(recv_iov + 20, 20);

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
        send_thread.join().unwrap();

        assert_eq!(code, 100, "recvmsg() should report all 100 bytes received");
        let mem = rt.memory();
        let scattered: Vec<u8> = (0..100).map(|i| mem.read_u8(recv_data + i)).collect();
        assert_eq!(
            scattered, pattern,
            "bytes should land in guest memory in the same order, across iovec boundaries"
        );
        assert_eq!(
            mem.read_u32(recv_msg + 24),
            0,
            "msg_flags should be cleared"
        );
    }
}
