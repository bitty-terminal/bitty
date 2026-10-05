//! Native component broker (DIR-030, CTX-0906).
//!
//! A native component is a separate, single-purpose executable installed on
//! demand and shared by every plugin that needs it (first: `net`, executable
//! `bitty-net`). Core never links a network or AI implementation crate: it
//! spawns the component as a stdio coprocess and speaks native-component wire
//! protocol v1 ([`bitty_network_wire`], the only crate Core links from the
//! bitty-network repository).
//!
//! Core is the policy authority and the component is mechanism only:
//!
//! - [`resolve`] finds `<root>/<name>/current` and
//!   `<root>/<name>/<version>/bitty-component.toml` under the data-directory
//!   root ([`components_root_for`], same resolver as `plugins/`, with the
//!   developer-only [`COMPONENTS_DIR_ENV`] override). `PATH` is never read.
//! - [`resolve_search`] applies the issue #1651 search priority (user
//!   `$XDG_DATA_HOME/bitty/components/` over system
//!   `/usr/lib/bitty/components/` via [`system_components_root_for`]); the
//!   user tier wins on collision and a tampered user install fails closed
//!   without system fallback. [`discover_components`] merges both tiers for
//!   `bitty component list`.
//! - [`ComponentDescriptor`] validation checks the name grammar, the semver
//!   version, the executable name (no path separators), the protocol range,
//!   and the SHA-256 digest of the executable. Every spawn re-runs the full
//!   resolution and digest check; any mismatch fails closed.
//! - [`ComponentBroker`] spawns the coprocess with a cleared environment plus
//!   [`COMPONENT_ENV_ALLOWLIST`], in the version directory, captures stderr
//!   into a [`COMPONENT_STDERR_MAX_BYTES`] ring, completes the
//!   `Hello`/`HelloAck` handshake, multiplexes requests by id (at most
//!   [`COMPONENT_MAX_IN_FLIGHT`]), stops the process after
//!   [`COMPONENT_IDLE_TIMEOUT`] idle (stdin close, [`COMPONENT_SHUTDOWN_GRACE`],
//!   then a kill of the recorded child only), and handles crashes
//!   ([`CrashTracker`]: in-flight requests fail with `component_lost`,
//!   restart backoff from [`COMPONENT_BACKOFF_INITIAL`] doubling to
//!   [`COMPONENT_BACKOFF_MAX`], and [`COMPONENT_CRASH_LIMIT`] crashes inside
//!   [`COMPONENT_CRASH_WINDOW`] make the component unavailable until restart).
//! - [`PluginGrant::compute`] derives the per-plugin network grant from the
//!   granted `network.connect:*` capabilities intersected with the manifest
//!   `[[network.egress]]` declarations; every request carries that grant and
//!   the plugin id for attribution. The component re-checks the grant and
//!   never widens it.
//!
//! The broker is driven by its owner: [`ComponentBroker::submit`] never
//! blocks on the component, and responses arrive as [`BrokerEvent`]s from
//! [`ComponentBroker::poll`] / [`ComponentBroker::wait`]. Time is injected
//! (`now: Instant`) so idle, handshake, and backoff policy are deterministic
//! under test.
//!
//! Not in this slice (DIR-030 follow-ups): the Lua-facing request surface
//! (a non-blocking request handle plus a response event, which depends on
//! the application event loop), per-platform process sandboxing, and the
//! registry install source.

mod broker;
mod descriptor;
mod env;
mod grant;
mod inventory;
mod policy;
mod stderr;

use std::time::Duration;

pub use broker::{
    BrokerConfig, BrokerError, BrokerEvent, BrokerEventKind, ComponentBroker, ComponentRequest,
    ComponentState, ComponentStatus, RequestId, StopOutcome,
};
pub use descriptor::{
    ComponentDescriptor, DescriptorError, ResolveError, ResolvedComponent, components_root_for,
    executable_file_name, resolve, validate_component_name,
};
pub use env::{ComponentEnv, is_allowlisted_env};
pub use grant::{GrantError, PluginGrant};
pub use inventory::{
    ComponentSource, ComponentSummary, InstalledVersion, SYSTEM_COMPONENTS_DIR_DEFAULT,
    SYSTEM_COMPONENTS_DIR_ENV, SearchedComponent, component_search_roots_for, discover_components,
    incompatible_component_hint, missing_component_hint, resolve_search,
    system_components_root_for, version_satisfies_caret,
};
pub use policy::{CrashTracker, SpawnGate};
pub use stderr::StderrRing;

/// Wire codec re-exports the broker API is expressed in.
pub use bitty_network_wire::{ErrorKind, Grant, GrantHost, Method, PROTOCOL_VERSION};

/// `$XDG_DATA_HOME` or `$HOME/.local/share` from explicit environment
/// values (empty or whitespace-only values count as unset).
///
/// The single data-directory resolver shared by the plugin store
/// (`<data_home>/bitty/plugins`) and the component root
/// (`<data_home>/bitty/components`), so both always agree.
#[must_use]
pub fn data_home_for(
    xdg_data_home: Option<&str>,
    home: Option<&str>,
) -> Option<std::path::PathBuf> {
    if let Some(xdg) = xdg_data_home {
        if !xdg.trim().is_empty() {
            return Some(std::path::PathBuf::from(xdg));
        }
    }
    home.filter(|home| !home.trim().is_empty())
        .map(|home| std::path::PathBuf::from(home).join(".local").join("share"))
}

/// Developer-only override of the component root (tests; not a user
/// configuration surface).
pub const COMPONENTS_DIR_ENV: &str = "BITTY_COMPONENTS_DIR";

/// Directory under the Bitty data directory holding installed components.
pub const COMPONENTS_DIR_NAME: &str = "components";

/// Plain-text file under `<root>/<name>/` naming the active version.
pub const COMPONENT_CURRENT_FILE: &str = "current";

/// Descriptor file name inside each `<root>/<name>/<version>/` directory.
pub const COMPONENT_DESCRIPTOR_FILE: &str = "bitty-component.toml";

/// Executable name prefix (`bitty-<name>`).
pub const COMPONENT_EXECUTABLE_PREFIX: &str = "bitty-";

/// Maximum byte length of a component name (`[a-z][a-z0-9-]{0,31}`).
pub const COMPONENT_NAME_MAX_BYTES: usize = bitty_network_wire::MAX_COMPONENT_NAME_BYTES;

/// Maximum byte length of the `current` file (a version plus a newline).
pub const COMPONENT_CURRENT_MAX_BYTES: usize = 128;

/// Maximum byte length of `bitty-component.toml`.
pub const COMPONENT_DESCRIPTOR_MAX_BYTES: usize = 4 * 1024;

/// Maximum size of a component executable read for the digest check.
pub const COMPONENT_EXECUTABLE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Environment variables forwarded to a component; everything else is
/// cleared (`PATH` is never forwarded).
pub const COMPONENT_ENV_ALLOWLIST: [&str; 10] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "LANG",
    "LC_ALL",
];

/// Bounded stderr ring per component (64 KiB, newest bytes kept).
pub const COMPONENT_STDERR_MAX_BYTES: usize = 64 * 1024;

/// Idle time with nothing in flight before Core closes the component stdin.
pub const COMPONENT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Grace between closing stdin and killing the recorded child.
pub const COMPONENT_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Deadline for the `HelloAck` after spawn; a miss counts as a crash.
pub const COMPONENT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// First restart delay after a crash.
pub const COMPONENT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);

/// Restart delay ceiling.
pub const COMPONENT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Crashes inside [`COMPONENT_CRASH_WINDOW`] that make a component
/// unavailable until the next Bitty start.
pub const COMPONENT_CRASH_LIMIT: usize = 5;

/// Sliding window for [`COMPONENT_CRASH_LIMIT`].
pub const COMPONENT_CRASH_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Requests in flight per component connection (wire protocol limit).
pub const COMPONENT_MAX_IN_FLIGHT: usize = bitty_network_wire::MAX_IN_FLIGHT;

/// Inbound frames buffered between reader threads and the broker owner.
///
/// Bounded: a full queue blocks the reader thread, which applies
/// backpressure to the component through its stdout pipe.
pub const COMPONENT_INBOUND_QUEUE_FRAMES: usize = 128;

/// Outbound batches (one request, cancel, or shutdown each) queued for one
/// component's stdin writer thread.
pub const COMPONENT_OUTBOUND_QUEUE_BATCHES: usize = COMPONENT_MAX_IN_FLIGHT * 2;

/// Inbound frames processed per [`ComponentBroker::poll`] call, so one call
/// stays bounded even when a component floods its stdout.
pub const COMPONENT_POLL_MAX_FRAMES: usize = 256;

/// Re-check slice while reaping children during [`ComponentBroker::shutdown`]
/// (stdout can close a moment before the exit status is observable).
pub(crate) const COMPONENT_EXIT_RECHECK: Duration = Duration::from_millis(10);

/// Stderr read chunk size for the capture thread.
pub(crate) const COMPONENT_STDERR_READ_CHUNK: usize = 4 * 1024;
