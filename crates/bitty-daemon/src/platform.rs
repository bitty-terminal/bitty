//! Platform cells for the `bittyd` local endpoint (`D-15`).
//!
//! The contract's `D-15` table fixes which cells may be served and with which
//! peer-identity mechanism. This module turns that table into a compiled
//! decision so no call site can fall back to a weaker transport by accident:
//!
//! | Cell                     | Endpoint transport | Peer mechanism                              |
//! | ------------------------ | ------------------ | ------------------------------------------- |
//! | Linux x86_64 / ARM64     | Unix socket        | `SO_PEERCRED`, `/proc` start time, user-ns   |
//! | macOS ARM64 / x86_64     | Unix socket        | `LOCAL_PEERCRED` + `LOCAL_PEERPID`          |
//! | FreeBSD x86_64           | none yet           | `LOCAL_PEERCRED` only; no pid in the safe API |
//! | Windows x86_64           | named pipe (plan)  | client pid + token SID (plan)               |
//! | Other cells              | none               | none                                        |
//!
//! A cell without a [`EndpointTransport::UnixSocket`] answer refuses with
//! `Denied/UnsupportedPlatform`; there is no fallback socket, no abstract
//! namespace, and no TCP listener. FreeBSD is refused rather than served with
//! a weaker check: the accepted re-check needs the peer pid and start time, and
//! the safe `nix` surface on FreeBSD exposes `xucred` (uid/gid) only. Windows
//! has a designed named-pipe transport but no implementation in this slice:
//! `\\.\pipe\bittyd-<SID>` with a current-user-only DACL, remote clients
//! rejected at the pipe level, `GetNamedPipeClientProcessId` plus token SID
//! comparison, and fail-closed SID/AppContainer/integrity-level mismatches.
//! That implementation needs the reviewed Win32 FFI seam owned by
//! `bitty-ipc-winpipe` and a native-runner conformance campaign; it is not
//! approximated here.

/// A platform cell from the accepted contract's `D-15` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlatformCell {
    /// Linux on x86_64: the first v0.2.0 delivery cell.
    LinuxX86_64,
    /// Linux on ARM64: tier 2, same surface and mechanism as x86_64.
    LinuxAarch64,
    /// Any other Linux architecture.
    LinuxOther,
    /// macOS on Apple silicon: tier 1, conformance-gated.
    MacosArm64,
    /// macOS on Intel: tier 2, conformance-gated.
    MacosX86_64,
    /// Any other macOS architecture.
    MacosOther,
    /// FreeBSD on x86_64: tier 2.
    FreeBsdX86_64,
    /// Windows on x86_64: tier 1, conformance-gated.
    WindowsX86_64,
    /// Any cell the contract does not list.
    Other,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const CURRENT_CELL: PlatformCell = PlatformCell::LinuxX86_64;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const CURRENT_CELL: PlatformCell = PlatformCell::LinuxAarch64;
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
const CURRENT_CELL: PlatformCell = PlatformCell::LinuxOther;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const CURRENT_CELL: PlatformCell = PlatformCell::MacosArm64;
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const CURRENT_CELL: PlatformCell = PlatformCell::MacosX86_64;
#[cfg(all(
    target_os = "macos",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
const CURRENT_CELL: PlatformCell = PlatformCell::MacosOther;
#[cfg(target_os = "freebsd")]
const CURRENT_CELL: PlatformCell = PlatformCell::FreeBsdX86_64;
#[cfg(target_os = "windows")]
const CURRENT_CELL: PlatformCell = PlatformCell::WindowsX86_64;
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "windows"
)))]
const CURRENT_CELL: PlatformCell = PlatformCell::Other;

impl PlatformCell {
    /// The cell this binary was compiled for.
    #[must_use]
    pub const fn current() -> Self {
        CURRENT_CELL
    }

    /// Stable name used in diagnostics, audit, and refusal messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxAarch64 => "linux-aarch64",
            Self::LinuxOther => "linux-other",
            Self::MacosArm64 => "macos-arm64",
            Self::MacosX86_64 => "macos-x86_64",
            Self::MacosOther => "macos-other",
            Self::FreeBsdX86_64 => "freebsd-x86_64",
            Self::WindowsX86_64 => "windows-x86_64",
            Self::Other => "other",
        }
    }

    /// The endpoint transport this cell accepts (`D-15`).
    ///
    /// [`EndpointTransport::WindowsNamedPipe`] is the designed Windows
    /// transport; this slice refuses it until the reviewed pipe implementation
    /// and native-runner conformance exist.
    #[must_use]
    pub const fn transport(self) -> EndpointTransport {
        match self {
            Self::LinuxX86_64 | Self::LinuxAarch64 | Self::LinuxOther => {
                EndpointTransport::UnixSocket
            }
            Self::MacosArm64 | Self::MacosX86_64 | Self::MacosOther => {
                EndpointTransport::UnixSocket
            }
            Self::WindowsX86_64 => EndpointTransport::WindowsNamedPipe,
            Self::FreeBsdX86_64 | Self::Other => EndpointTransport::Unsupported,
        }
    }

    /// The peer-identity mechanism this cell can provide (`D-08`, `D-15`).
    ///
    /// A cell whose mechanism is [`PeerMechanism::Unsupported`] must not bind
    /// an endpoint at all: the daemon fails closed before creating a transport
    /// it could not authenticate.
    #[must_use]
    pub const fn peer_mechanism(self) -> PeerMechanism {
        match self {
            Self::LinuxX86_64 | Self::LinuxAarch64 | Self::LinuxOther => {
                PeerMechanism::LinuxPeerCred
            }
            Self::MacosArm64 | Self::MacosX86_64 | Self::MacosOther => {
                PeerMechanism::AppleLocalPeer
            }
            Self::FreeBsdX86_64 | Self::WindowsX86_64 | Self::Other => PeerMechanism::Unsupported,
        }
    }
}

impl std::fmt::Display for PlatformCell {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// The endpoint transport a cell accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointTransport {
    /// A filesystem Unix-domain socket inside an owner-private directory.
    UnixSocket,
    /// A current-user-only local Windows named pipe (designed, not implemented
    /// in this slice).
    WindowsNamedPipe,
    /// No accepted transport; the daemon must not start an endpoint.
    Unsupported,
}

/// The kernel peer-identity mechanism a cell provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerMechanism {
    /// `SO_PEERCRED` plus the peer's `/proc` start time and user namespace.
    LinuxPeerCred,
    /// `LOCAL_PEERCRED` uid/gid plus `LOCAL_PEERPID`.
    AppleLocalPeer,
    /// No accepted mechanism; peer operations fail closed.
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_one_linux_supports_the_unix_transport() {
        assert_eq!(
            PlatformCell::LinuxX86_64.transport(),
            EndpointTransport::UnixSocket
        );
        assert_eq!(
            PlatformCell::LinuxAarch64.transport(),
            EndpointTransport::UnixSocket
        );
        assert_eq!(
            PlatformCell::LinuxX86_64.peer_mechanism(),
            PeerMechanism::LinuxPeerCred
        );
    }

    #[test]
    fn apple_cells_use_the_local_peer_socket_options() {
        assert_eq!(
            PlatformCell::MacosArm64.transport(),
            EndpointTransport::UnixSocket
        );
        assert_eq!(
            PlatformCell::MacosX86_64.peer_mechanism(),
            PeerMechanism::AppleLocalPeer
        );
    }

    #[test]
    fn unsupported_cells_never_gain_a_fallback_transport() {
        assert_eq!(
            PlatformCell::FreeBsdX86_64.transport(),
            EndpointTransport::Unsupported
        );
        assert_eq!(
            PlatformCell::FreeBsdX86_64.peer_mechanism(),
            PeerMechanism::Unsupported
        );
        assert_eq!(
            PlatformCell::Other.transport(),
            EndpointTransport::Unsupported
        );
        assert_eq!(
            PlatformCell::WindowsX86_64.transport(),
            EndpointTransport::WindowsNamedPipe
        );
        assert_eq!(
            PlatformCell::WindowsX86_64.peer_mechanism(),
            PeerMechanism::Unsupported
        );
    }

    #[test]
    fn cell_names_are_stable_diagnostics() {
        assert_eq!(PlatformCell::LinuxX86_64.name(), "linux-x86_64");
        assert_eq!(PlatformCell::WindowsX86_64.to_string(), "windows-x86_64");
        assert_eq!(PlatformCell::Other.name(), "other");
        assert!(!PlatformCell::current().name().is_empty());
    }
}
