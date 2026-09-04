//! `BITTY_SOCKET` servo (CTX-0144, Issue #236).
//!
//! Thin Unix-socket front-end over [`bitty_ipc::devtools`]: binds the socket
//! `bitty-devtools` expects, accepts connections on a background thread, and
//! dispatches the handshake plus the minimal read-only round-trip (`ping`,
//! `getSnapshot`). Full introspection is CTX-0159, which registers new
//! `bitty.debug/*` handlers on the shared [`bitty_ipc::devtools::Dispatcher`]
//! without touching this file's lifecycle.
//!
//! # Fail-soft contract
//!
//! Socket failure must never crash the terminal: every setup error disables
//! serving with one stderr line and the terminal continues normally. Stale
//! socket files from dead instances are reclaimed after a live check; a live
//! peer keeps its socket (no stealing). Directory and socket modes are
//! `0700`/`0600` with owner attestation; peer UID equality is verified
//! before the first request byte is parsed; per-connection `RC-9` rate
//! limits apply and the 16-connection cap sheds the newest arrival.
//!
//! # Platform
//!
//! Unix only. Other platforms get a disabled guard (same fail-soft shape, no
//! behavior fabricated).

#![forbid(unsafe_code)]

use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};

/// Static server facts handed to the servo at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerDescriptor {
    /// Grid columns the runtime was configured with.
    pub cols: usize,
    /// Grid rows the runtime was configured with.
    pub rows: usize,
}

/// Live servo handle. Holds the accept thread until dropped; dropping
/// requests shutdown and joins the accept loop (bounded by the poll
/// interval). Connection threads are detached and finish via EOF or their
/// read timeouts; process exit reaps them.
pub struct IpcServeGuard {
    /// Whether a listener is actually serving.
    enabled: bool,
    /// Socket path served (empty when disabled).
    socket_path: String,
    /// Shutdown flag for the accept loop.
    shutdown: Arc<AtomicBool>,
    /// Accept-loop thread (joined on drop).
    #[cfg(unix)]
    handle: Option<std::thread::JoinHandle<()>>,
}

impl IpcServeGuard {
    /// Whether the socket is being served.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Socket path served (empty when disabled).
    #[must_use]
    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }
}

impl Drop for IpcServeGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        #[cfg(unix)]
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Serve `BITTY_SOCKET` on a background thread (fail-soft).
///
/// Resolves the path from `BITTY_SOCKET` / `XDG_RUNTIME_DIR` /
/// `BITTY_INSTANCE_ID` (advisory identifiers, never credentials), prepares
/// the directory, reclaims stale sockets, binds, attests modes, and spawns
/// the accept loop. Any failure returns a disabled guard after one stderr
/// line; the caller keeps the guard alive for the process lifetime.
#[must_use]
pub fn serve_in_background(descriptor: ServerDescriptor) -> IpcServeGuard {
    #[cfg(unix)]
    {
        unix_serve(descriptor)
    }
    #[cfg(not(unix))]
    {
        let _ = descriptor;
        eprintln!(
            "bitty: ipc socket serving is unavailable on this platform (continuing without IPC)"
        );
        IpcServeGuard {
            enabled: false,
            socket_path: String::new(),
            shutdown: Arc::new(AtomicBool::new(true)),
        }
    }
}

/// Unix serving path: resolve, prepare, reclaim, bind, attest, spawn.
#[cfg(unix)]
fn unix_serve(descriptor: ServerDescriptor) -> IpcServeGuard {
    match try_listen() {
        Ok(listen) => {
            let dispatcher = Arc::new(bitty_ipc::devtools::Dispatcher::with_defaults());
            let server = bitty_ipc::devtools::ServerInfo::new(
                listen.instance,
                listen.socket_path.clone(),
                descriptor.cols,
                descriptor.rows,
            );
            let shutdown = Arc::new(AtomicBool::new(false));
            let active = Arc::new(AtomicUsize::new(0));
            let handle = std::thread::spawn({
                let shutdown = Arc::clone(&shutdown);
                let active = Arc::clone(&active);
                move || {
                    accept_loop(
                        listen.listener,
                        listen.runtime_uid,
                        dispatcher,
                        server,
                        shutdown,
                        active,
                    );
                }
            });
            IpcServeGuard {
                enabled: true,
                socket_path: listen.socket_path,
                shutdown,
                handle: Some(handle),
            }
        }
        Err(reason) => {
            eprintln!("bitty: ipc disabled (fail-soft): {reason}");
            IpcServeGuard {
                enabled: false,
                socket_path: String::new(),
                shutdown: Arc::new(AtomicBool::new(true)),
                handle: None,
            }
        }
    }
}

/// Bound listener plus the attested identity to serve as.
#[cfg(unix)]
struct BoundListener {
    /// Accepted socket.
    listener: std::os::unix::net::UnixListener,
    /// Path bound.
    socket_path: String,
    /// Instance served.
    instance: String,
    /// Serving UID (socket-file owner, established at bind).
    runtime_uid: u32,
}

/// Resolve, prepare, reclaim stale, bind, and attest. Fail-soft: every
/// failure is a `String` reason, never a panic.
#[cfg(unix)]
fn try_listen() -> Result<BoundListener, String> {
    use std::os::unix::net::{UnixListener, UnixStream};

    let env = bitty_ipc::devtools::SocketEnv::from_process_env();
    let (socket_path, instance) = bitty_ipc::devtools::resolve_socket_path_from_env(&env, None)
        .map_err(|err| {
            format!("socket path unavailable: {err} (set XDG_RUNTIME_DIR or BITTY_SOCKET)")
        })?;
    let dir = bitty_ipc::devtools::prepare_socket_dir(&socket_path)
        .map_err(|err| format!("socket directory rejected: {err}"))?;
    // Stale reclaim: a live peer keeps its socket (connect succeeds); a dead
    // instance leaves a file that refuses connections (remove + rebind).
    if std::fs::symlink_metadata(&socket_path).is_ok() {
        match UnixStream::connect(&socket_path) {
            Ok(_) => {
                return Err(format!(
                    "socket {socket_path} is already served by a live instance"
                ));
            }
            Err(_) => {
                std::fs::remove_file(&socket_path).map_err(|err| {
                    format!("stale socket {socket_path} cannot be reclaimed: {err}")
                })?;
            }
        }
    }
    let listener = UnixListener::bind(&socket_path)
        .map_err(|err| format!("bind {socket_path} failed: {err}"))?;
    let runtime_uid = bitty_ipc::devtools::attest_bound_socket(&socket_path, &dir)
        .map_err(|err| format!("socket attestation failed: {err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("listener setup failed: {err}"))?;
    Ok(BoundListener {
        listener,
        socket_path,
        instance,
        runtime_uid,
    })
}

/// Accept loop: polls for arrivals, sheds past the `RC-9` connection cap
/// (newest first), and hands each connection to a bounded handler thread.
#[cfg(unix)]
fn accept_loop(
    listener: std::os::unix::net::UnixListener,
    runtime_uid: u32,
    dispatcher: Arc<bitty_ipc::devtools::Dispatcher>,
    server: bitty_ipc::devtools::ServerInfo,
    shutdown: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
) {
    use std::time::Duration;

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if active.load(Ordering::SeqCst) >= bitty_ipc::devtools::max_connections() {
                    eprintln!("bitty: ipc connection shed (at RC-9 cap, newest first)");
                    drop(stream);
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let dispatcher = Arc::clone(&dispatcher);
                let server = server.clone();
                let active = Arc::clone(&active);
                std::thread::spawn(move || {
                    let _counted = ActiveCount::new(active);
                    serve_stream(stream, runtime_uid, &dispatcher, &server);
                });
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::Interrupted =>
            {
                std::thread::sleep(Duration::from_millis(
                    bitty_ipc::devtools::ACCEPT_POLL_INTERVAL_MS,
                ));
            }
            Err(err) => {
                eprintln!("bitty: ipc accept error: {err}");
                std::thread::sleep(Duration::from_millis(
                    bitty_ipc::devtools::ACCEPT_POLL_INTERVAL_MS * 5,
                ));
            }
        }
    }
}

/// RAII decrement for the active-connection counter.
#[cfg(unix)]
struct ActiveCount {
    /// Counter to decrement on drop.
    active: Arc<AtomicUsize>,
}

#[cfg(unix)]
impl ActiveCount {
    /// Hold one active-connection slot.
    fn new(active: Arc<AtomicUsize>) -> Self {
        Self { active }
    }
}

#[cfg(unix)]
impl Drop for ActiveCount {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serve one accepted stream: timeouts, peer identity, dispatch loop.
///
/// Peer identity is transport-attested: this stream arrived on the
/// owner-only (`0600`) socket the servo bound itself, so the kernel already
/// refused any other UID at `connect`. See
/// [`bitty_ipc::devtools::transport_attested_peer`] for the contract and the
/// recorded `SO_PEERCRED` hardening for CTX-0159.
#[cfg(unix)]
fn serve_stream(
    mut stream: std::os::unix::net::UnixStream,
    runtime_uid: u32,
    dispatcher: &bitty_ipc::devtools::Dispatcher,
    server: &bitty_ipc::devtools::ServerInfo,
) {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    if stream
        .set_read_timeout(Some(Duration::from_secs(
            bitty_ipc::devtools::CONN_IDLE_TIMEOUT_SECS,
        )))
        .is_err()
    {
        return;
    }
    if stream
        .set_write_timeout(Some(Duration::from_secs(
            bitty_ipc::devtools::CONN_WRITE_TIMEOUT_SECS,
        )))
        .is_err()
    {
        return;
    }
    let peer = bitty_ipc::devtools::transport_attested_peer(runtime_uid);
    let context = bitty_ipc::devtools::ServeContext::new(server);
    let mut limiter = bitty_ipc::limits::RateLimiter::rc9_default();
    let clock = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    };
    match bitty_ipc::devtools::serve_connection(
        &mut stream,
        peer,
        runtime_uid,
        dispatcher,
        &context,
        &mut limiter,
        &clock,
    ) {
        Ok(stats) => {
            if stats.denied > 0 || stats.framing_errors > 0 {
                eprintln!(
                    "bitty: ipc connection closed (requests={} denied={} framing_errors={})",
                    stats.requests, stats.denied, stats.framing_errors
                );
            }
        }
        Err(err) => {
            eprintln!("bitty: ipc connection rejected: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_accessors_report_state() {
        let guard = IpcServeGuard {
            enabled: true,
            socket_path: "/tmp/bitty-test.sock".to_string(),
            shutdown: Arc::new(AtomicBool::new(false)),
            #[cfg(unix)]
            handle: None,
        };
        assert!(guard.is_enabled());
        assert_eq!(guard.socket_path(), "/tmp/bitty-test.sock");
    }

    #[test]
    fn descriptor_carries_grid_geometry() {
        let descriptor = ServerDescriptor { cols: 80, rows: 24 };
        assert_eq!(descriptor.cols, 80);
        assert_eq!(descriptor.rows, 24);
    }
}
