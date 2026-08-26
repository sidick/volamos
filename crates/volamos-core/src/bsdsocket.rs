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
//! `inet_addr`/`gethostbyname` -- real outbound and inbound TCP, plus
//! UDP, real error reporting through the documented `Errno()`/
//! `SetErrnoPtr()` mechanism, and real forward DNS lookups via the
//! host's own resolver (see "DNS: a real, blocking host lookup" below).
//! Deliberately **not yet implemented** (calling these traps as an
//! ordinary unknown-call, same as any other library's unimplemented LVO
//! in this codebase -- see [`crate::lvos::bsdsocket`]'s module docs for
//! why this table only lists what's implemented, not the full ABI):
//! `setsockopt`/`getsockopt` (real option roundtrip storage, no
//! consumer needing it yet), `WaitSelect`/`SetSocketSignals` (real
//! `select()`-shaped multiplexing needs deciding whether to integrate
//! with `crate::exectask`'s signal model or just poll -- a real design
//! choice, not a quick add), `gethostbyaddr` (reverse/PTR lookup --
//! `std::net` has no portable reverse-DNS primitive; Copperline's own
//! `hostsocket-plugin` hit the identical wall and stayed a stub for the
//! same reason, see that crate's module docs), `Dup2Socket`/
//! `ObtainSocket`/`ReleaseSocket` (fd-sharing across processes -- this
//! runtime doesn't have that among its own processes to begin with),
//! `sendmsg`/`recvmsg`/`vsyslog`/`SocketBaseTagList`/`GetSocketEvents`.
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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, ToSocketAddrs};

use socket2::{Domain, SockAddr, Socket, Type};

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, HandlerContext, LibraryTable};
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

/// Maps a [`std::io::Error`] from a `socket2` call to the fixed BSD
/// `errno` numbering `bsdsocket.library` documents -- see the module
/// docs' "Errno" section for why this doesn't inspect the host's raw OS
/// errno.
fn translate_errno(e: &std::io::Error) -> i32 {
    use std::io::ErrorKind::*;
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
}

impl BsdSocketState {
    pub fn new() -> Self {
        Self {
            sockets: HashMap::new(),
            next_id: 1,
            last_errno: 0,
            errno_ptr: None,
            ntoa_buf: None,
            hostent_allocs: Vec::new(),
        }
    }

    /// Records `code` as the current `Errno()` value, and mirrors it into
    /// guest memory at [`Self::errno_ptr`] if one was set via
    /// `SetErrnoPtr`. Called by every fallible handler on both success
    /// (`code = 0`) and failure, matching real `bsdsocket.library`'s own
    /// "every call sets errno, not just failing ones" behavior.
    fn set_errno(&mut self, mem: &mut dyn AddressSpace, code: i32) {
        self.last_errno = code;
        if let Some(ptr) = self.errno_ptr {
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
    if let Err(e) = socket.set_nonblocking(true) {
        let code = translate_errno(&e);
        fail(ctx, code);
        return Ok(());
    }

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
/// waiting -- this backend is always non-blocking; see the module docs'
/// note on `WaitSelect` for how a guest would wait for readiness).
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
            if let Err(e) = accepted.set_nonblocking(true) {
                let code = translate_errno(&e);
                ctx.bsdsocket.set_errno(ctx.mem, code);
                ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
                return Ok(());
            }
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
        }
        Err(e) => {
            let code = translate_errno(&e);
            ctx.bsdsocket.set_errno(ctx.mem, code);
            ctx.cpu.set_data_register(DataRegister(0), 0xFFFF_FFFF);
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
        }
        Err(e) if is_connect_in_progress(&e) => {
            ctx.bsdsocket.set_errno(ctx.mem, EINPROGRESS);
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

/// Whether a non-blocking `connect()`'s error just means "still in
/// progress" (`EINPROGRESS`/`EALREADY` on Unix, `WSAEWOULDBLOCK` on
/// Windows -- `std::io::Error::kind()` maps all of these to
/// [`std::io::ErrorKind::WouldBlock`]).
fn is_connect_in_progress(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock
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

    let fail = |ctx: &mut HandlerContext<'_, C>, code: i32| {
        ctx.bsdsocket.set_errno(ctx.mem, code);
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

/// Registers this module's `bsdsocket.library` handlers, looked up by
/// name through [`BSDSOCKET_LVOS`]. **Not** called from
/// [`crate::dispatch::Runtime::new`] -- see the module docs' "Opt-in,
/// not always-on" section; call sites go through [`crate::dispatch::
/// Runtime::enable_bsdsocket`] instead.
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
    reg!("getsockname", getsockname_handler::<C>);
    reg!("getpeername", getpeername_handler::<C>);
    reg!("CloseSocket", close_socket_handler::<C>);
    reg!("getdtablesize", getdtablesize_handler::<C>);
    reg!("Errno", errno_handler::<C>);
    reg!("SetErrnoPtr", set_errno_ptr_handler::<C>);
    reg!("Inet_NtoA", inet_ntoa_handler::<C>);
    reg!("inet_addr", inet_addr_handler::<C>);
    reg!("gethostbyname", gethostbyname_handler::<C>);
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

        let mut words = movea_bsdsocket_base_to_a6().to_vec();
        push_move_imm_a(&mut words, 0, name_addr);
        words.extend_from_slice(&jsr_disp16_a6(-210)); // gethostbyname -> D0
        words.extend_from_slice(&jsr_disp16_a6(-162)); // Errno() -> D0
        words.push(RTS);

        let mut mem = FlatMemory::new(0x2_0000);
        let entry = TRAP_TABLE_END;
        load_words(&mut mem, entry, &words);
        // .invalid is a reserved TLD (RFC 2606) guaranteed to never
        // resolve, avoiding real-network flakiness in this test.
        crate::guestmem::write_c_string(&mut mem, name_addr, b"this-name-does-not-exist.invalid");
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
        assert_eq!(code, HOST_NOT_FOUND);
    }
}
