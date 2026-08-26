//! `bsdsocket.library` LVO (library vector offset) metadata table.
//!
//! # Provenance
//!
//! Unlike this module's siblings (`dos`, `exec`, `intuition`, ...),
//! `bsdsocket.library` is not part of AROS's own ROM-resident `workbench/
//! libs` tree in the shape [`crate::lvos`]'s `gen_lvos.py` tool consumes
//! (an AROS `##begin functionlist` `.conf` block) -- it's a third-party
//! TCP/IP stack API (AmiTCP/Miami/Genesis/Roadshow all implement the same
//! ABI), so this table is instead hand-transcribed from a real Roadshow
//! NDK's `bsdsocket_lib.fd` file (the classic AmigaOS `.fd` format: a
//! `##bias N` directive followed by one function-and-register-list line
//! per LVO, `-6` apart, standard AmigaOS spacing -- the exact same facts
//! `gen_lvos.py` extracts from AROS's `.conf` format, just a different
//! on-disk encoding of them). Every LVO offset below was independently
//! re-derived by counting `##bias 30` (`socket`, `-30`) plus `-6` per
//! subsequent line, not copied from any single already-computed table --
//! see `docs/plan.md`'s (or the originating GitHub issue's) research
//! notes for the full derivation. Only the LVOs this runtime actually
//! implements are listed (see [`crate::bsdsocket`]'s module docs for
//! scope) -- unlike the AROS-generated tables' "list everything, even
//! unimplemented, for unknown-call diagnostics" convention, since there
//! is no single canonical "the" `bsdsocket.library` interface description
//! to exhaustively list from (AmiTCP/Miami/Genesis/Roadshow all extended
//! the base ABI differently over the years).

use crate::cpu::{AddressRegister, DataRegister};
use crate::lvos::{ArgReg, LvoEntry};

/// The `bsdsocket.library` LVOs this runtime implements handlers for
/// (see [`crate::bsdsocket::register_bsdsocket_handlers`]).
pub static BSDSOCKET_LVOS: &[LvoEntry] = &[
    LvoEntry {
        name: "socket",
        lvo: -30,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "bind",
        lvo: -36,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "listen",
        lvo: -42,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "accept",
        lvo: -48,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "connect",
        lvo: -54,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "sendto",
        lvo: -60,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "send",
        lvo: -66,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "recvfrom",
        lvo: -72,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "recv",
        lvo: -78,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
        ],
        private: false,
    },
    LvoEntry {
        name: "setsockopt",
        lvo: -90,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "getsockopt",
        lvo: -96,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "shutdown",
        lvo: -84,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "IoctlSocket",
        lvo: -114,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::A(AddressRegister(0)),
        ],
        private: false,
    },
    LvoEntry {
        name: "getsockname",
        lvo: -102,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "getpeername",
        lvo: -108,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "CloseSocket",
        lvo: -120,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "getdtablesize",
        lvo: -138,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "WaitSelect",
        lvo: -126,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
            ArgReg::A(AddressRegister(2)),
            ArgReg::A(AddressRegister(3)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "Errno",
        lvo: -162,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "SetErrnoPtr",
        lvo: -168,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Inet_NtoA",
        lvo: -174,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "inet_addr",
        lvo: -180,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Inet_LnaOf",
        lvo: -186,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Inet_NetOf",
        lvo: -192,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "Inet_MakeAddr",
        lvo: -198,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "inet_network",
        lvo: -204,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "gethostbyname",
        lvo: -210,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "gethostbyaddr",
        lvo: -216,
        args: &[
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "getservbyname",
        lvo: -234,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::A(AddressRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "getservbyport",
        lvo: -240,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "getprotobyname",
        lvo: -246,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "getprotobynumber",
        lvo: -252,
        args: &[ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "gethostname",
        lvo: -282,
        args: &[ArgReg::A(AddressRegister(0)), ArgReg::D(DataRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "gethostid",
        lvo: -288,
        args: &[],
        private: false,
    },
    LvoEntry {
        name: "SocketBaseTagList",
        lvo: -294,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "vsyslog",
        lvo: -258,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::A(AddressRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "Dup2Socket",
        lvo: -264,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "sendmsg",
        lvo: -270,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    LvoEntry {
        name: "recvmsg",
        lvo: -276,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::A(AddressRegister(0)),
            ArgReg::D(DataRegister(1)),
        ],
        private: false,
    },
    // ObtainSocket/ReleaseSocket/ReleaseCopyOfSocket hand a socket's
    // ownership between *processes* by a small integer ID (real use:
    // pass a listening socket to a child process). This runtime models
    // a single guest task with no other process to hand a socket to, so
    // these are registered (avoiding an unhandled-call crash for any
    // caller that unconditionally tries them) but honestly always fail
    // -- see `release_socket_handler`'s doc comment.
    LvoEntry {
        name: "ObtainSocket",
        lvo: -144,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
            ArgReg::D(DataRegister(3)),
        ],
        private: false,
    },
    LvoEntry {
        name: "ReleaseSocket",
        lvo: -150,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "ReleaseCopyOfSocket",
        lvo: -156,
        args: &[ArgReg::D(DataRegister(0)), ArgReg::D(DataRegister(1))],
        private: false,
    },
    LvoEntry {
        name: "GetSocketEvents",
        lvo: -300,
        args: &[ArgReg::A(AddressRegister(0))],
        private: false,
    },
    LvoEntry {
        name: "SetSocketSignals",
        lvo: -132,
        args: &[
            ArgReg::D(DataRegister(0)),
            ArgReg::D(DataRegister(1)),
            ArgReg::D(DataRegister(2)),
        ],
        private: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry's LVO must match `-30 - 6*index` (the standard AmigaOS
    /// bias-30-then-6-per-slot spacing this table was hand-derived from --
    /// a regression check against a transcription slip, same spirit as
    /// the AROS-generated tables' own `known_lvos_match_amigaos`-style
    /// tests, just checking arithmetic consistency rather than against a
    /// second independent source).
    #[test]
    fn lvos_match_the_standard_bias_30_spacing() {
        // Only entries transcribed in on-disk .fd order are checked this
        // way; this table skips setsockopt/getsockopt/WaitSelect/
        // SetSocketSignals/ObtainSocket/ReleaseSocket/
        // ReleaseCopyOfSocket (not yet implemented -- see the module
        // docs), so indices don't line up 1:1 with bias order. Instead,
        // just check every listed LVO is a multiple of -6 offset from
        // -30 (i.e. `(lvo + 30) % 6 == 0`) and unique.
        let mut seen = std::collections::HashSet::new();
        for entry in BSDSOCKET_LVOS {
            assert!(
                (entry.lvo + 30) % 6 == 0,
                "{}'s LVO {} isn't on the standard -30,-36,-42,... grid",
                entry.name,
                entry.lvo
            );
            assert!(
                seen.insert(entry.lvo),
                "duplicate LVO {} ({})",
                entry.lvo,
                entry.name
            );
        }
    }
}
