//! `BITTY_SOCKET` servo (CTX-0144, Issue #236).
//!
//! Thin Unix-socket front-end over [`bitty_ipc::devtools`]: binds the socket
//! the devtools protocol clients (the `devtools` Lua plugin) expect, accepts connections on a background thread, and
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
//! `0700`/`0600` with owner and endpoint checks kept separate from peer proof.
//! Normal Unix serving is disabled unless the accepted-stream platform seam
//! can attest identity; per-connection `RC-9` rate limits and the 16-connection
//! cap remain bounded.
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
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};

/// Static server facts handed to the servo at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerDescriptor {
    /// Grid columns the runtime was configured with.
    pub cols: usize,
    /// Grid rows the runtime was configured with.
    pub rows: usize,
    /// Whether this process serves the `--test-mode` E2E surface (CTX-0506).
    ///
    /// Gates registration of `bitty.debug/testInfo` / `bitty.debug/testExit`
    /// only; it changes no scope, bearer, rate, or redaction rule.
    pub test_mode: bool,
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
    /// Why serving was attempted and rejected (CTX-0481): `Some` only for a
    /// genuine failure on a platform that serves, so `--fail-loud` can turn
    /// it into a non-zero exit. Platform-unsupported stays `None` (the
    /// fail-soft warning path is the documented behavior there).
    failure_reason: Option<String>,
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

    /// Why serving was attempted and rejected (CTX-0481); `None` when the
    /// servo is healthy or the platform does not serve at all.
    #[must_use]
    pub fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
    }

    /// Disabled guard without a failure reason (tests: platform-unsupported
    /// shape).
    #[cfg(test)]
    pub(crate) fn disabled_for_tests() -> Self {
        Self {
            enabled: false,
            socket_path: String::new(),
            failure_reason: None,
            shutdown: Arc::new(AtomicBool::new(true)),
            #[cfg(unix)]
            handle: None,
        }
    }

    /// Disabled guard carrying a failure reason (tests).
    #[cfg(test)]
    pub(crate) fn failed_for_tests(reason: &str) -> Self {
        let mut guard = Self::disabled_for_tests();
        guard.failure_reason = Some(reason.to_string());
        guard
    }

    #[cfg(test)]
    pub(crate) fn unsupported_for_tests() -> Self {
        Self::disabled_for_tests()
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
        unix_serve(
            descriptor,
            &bitty_ipc::devtools::SocketEnv::from_process_env(),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = descriptor;
        let reason = "ipc socket serving is unavailable on this platform";
        crate::logging::warn(|| format!("bitty: {reason} (continuing without IPC)"));
        IpcServeGuard {
            enabled: false,
            socket_path: String::new(),
            failure_reason: None,
            shutdown: Arc::new(AtomicBool::new(true)),
        }
    }
}

/// Unix serving path: resolve, prepare, reclaim, bind, attest, spawn.
/// `env` is the advisory resolution input, passed explicitly so tests can
/// cover the enabled path without mutating process-global state.
#[cfg(unix)]
fn unix_serve(descriptor: ServerDescriptor, env: &bitty_ipc::devtools::SocketEnv) -> IpcServeGuard {
    if !bitty_ipc::accepted_stream_peer_attestation_available() {
        let reason =
            "accepted-stream peer attestation is unavailable; IPC surface disabled".to_string();
        crate::logging::warn(|| format!("bitty: ipc unavailable (fail-soft): {reason}"));
        return IpcServeGuard {
            enabled: false,
            socket_path: String::new(),
            failure_reason: None,
            shutdown: Arc::new(AtomicBool::new(true)),
            handle: None,
        };
    }
    match try_listen(env) {
        Ok(listen) => {
            // CTX-0506: test mode registers the E2E surface (`testInfo`,
            // `testExit`); normal instances keep the default table.
            let dispatcher = Arc::new(if descriptor.test_mode {
                bitty_ipc::devtools::Dispatcher::with_test_mode()
            } else {
                bitty_ipc::devtools::Dispatcher::with_defaults()
            });
            let server = bitty_ipc::devtools::ServerInfo::new(
                listen.instance,
                listen.socket_path.clone(),
                descriptor.cols,
                descriptor.rows,
            );
            let shutdown = Arc::new(AtomicBool::new(false));
            let active = Arc::new(AtomicUsize::new(0));
            let services = ConnectionServices::from_process_env();
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
                        services,
                    );
                }
            });
            IpcServeGuard {
                enabled: true,
                socket_path: listen.socket_path,
                failure_reason: None,
                shutdown,
                handle: Some(handle),
            }
        }
        Err(reason) => {
            crate::logging::warn(|| format!("bitty: ipc disabled (fail-soft): {reason}"));
            IpcServeGuard {
                enabled: false,
                socket_path: String::new(),
                failure_reason: Some(reason),
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

/// Server-owned state shared by every accepted connection (CTX-0792).
///
/// Built once per listener, never per connection: the authority mints one
/// principal/session per accepted stream (#1403), and the endpoint limiter is
/// the aggregate RC-9 budget every connection draws from (#1404).
#[cfg(unix)]
#[derive(Clone)]
struct ConnectionServices {
    /// Mints and revokes one principal/session per accepted connection.
    authority: bitty_ipc::ctl::ControlAuthority,
    /// Endpoint-wide RC-9 request budget (100 req/s, 200 burst), shared by
    /// every connection: opening more connections never buys more requests.
    endpoint_limiter: Arc<Mutex<bitty_ipc::limits::RateLimiter>>,
    /// Operator-consented scope ceiling (`BITTY_CTL_ELEVATE` on top of the CLI
    /// default), read once at listener start. It seeds each connection's
    /// session; it is never consulted directly at drain.
    granted: bitty_ipc::ScopeSet,
}

#[cfg(unix)]
impl ConnectionServices {
    fn from_process_env() -> Self {
        Self {
            authority: bitty_ipc::ctl::ControlAuthority::new(),
            endpoint_limiter: Arc::new(Mutex::new(bitty_ipc::limits::RateLimiter::rc9_default())),
            granted: bitty_ipc::ctl::elevation_from_env(
                std::env::var("BITTY_CTL_ELEVATE").ok().as_deref(),
            ),
        }
    }
}

/// Liveness verdict for an existing socket path (CTX-0792, #1404).
///
/// Only a refused connection proves the previous instance is gone. Every other
/// probe error (permission, resource exhaustion, backlog, timeout) is
/// ambiguous and must never unlink a path that may still be served.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SocketProbe {
    /// `ECONNREFUSED`: nothing listens on the path; reclaim is allowed.
    Dead,
    /// Any other failure: refuse reclaim (fail closed).
    Ambiguous,
}

#[cfg(unix)]
fn classify_socket_probe(error: &std::io::Error) -> SocketProbe {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused => SocketProbe::Dead,
        _ => SocketProbe::Ambiguous,
    }
}

/// Quarantine-then-verify reclaim of a dead socket path (CTX-0792, #1404).
///
/// The path is first renamed to a private quarantine name, so the object that
/// is inspected and removed is exactly the object that was moved: a
/// replacement that lands at `socket_path` after the probe is never unlinked.
/// The quarantined object must still be the endpoint identity captured before
/// the probe (dev/ino/uid/mode); a symlink or any identity change puts the
/// quarantined object back without replacing anything bound meanwhile
/// ([`restore_quarantined`]) and refuses the takeover.
#[cfg(unix)]
fn reclaim_socket_path(
    socket_path: &str,
    expected: bitty_ipc::devtools::SocketEndpointIdentity,
    runtime_uid: u32,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static RECLAIM_ID: AtomicU64 = AtomicU64::new(0);
    let path = std::path::Path::new(socket_path);
    let parent = path
        .parent()
        .ok_or_else(|| "socket path has no parent".to_string())?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("socket");
    let quarantine = parent.join(format!(
        ".{name}.reclaim-{}-{}",
        std::process::id(),
        RECLAIM_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::rename(socket_path, &quarantine)
        .map_err(|err| format!("socket replacement could not be quarantined: {err}"))?;
    let moved = std::fs::symlink_metadata(&quarantine)
        .map_err(|err| format!("quarantined socket cannot be inspected: {err}"))?;
    if moved.file_type().is_symlink() {
        let restore = restore_quarantined(&quarantine, socket_path);
        return Err(format!(
            "quarantined socket is a symlink; refusing takeover ({restore})"
        ));
    }
    let actual = bitty_ipc::devtools::SocketEndpointIdentity {
        dev: moved.dev(),
        ino: moved.ino(),
        uid: moved.uid(),
        mode: moved.mode() & 0o777,
    };
    if actual != expected || actual.uid != runtime_uid || actual.mode != bitty_ipc::SOCKET_MODE {
        let restore = restore_quarantined(&quarantine, socket_path);
        return Err(format!(
            "socket endpoint changed during reclaim; refusing takeover ({restore})"
        ));
    }
    std::fs::remove_file(&quarantine)
        .map_err(|err| format!("dead socket quarantine could not be removed: {err}"))
}

/// Put a quarantined object back at `socket_path` without replacing anything
/// that appeared there meanwhile (a hard link never overwrites its
/// destination), and describe the outcome for the refusal message. When the
/// path is occupied, the object stays at its quarantine name and the message
/// names it, so nothing is silently stranded or clobbered.
#[cfg(unix)]
fn restore_quarantined(quarantine: &std::path::Path, socket_path: &str) -> String {
    match std::fs::hard_link(quarantine, socket_path) {
        Ok(()) => match std::fs::remove_file(quarantine) {
            Ok(()) => String::from("original restored"),
            Err(err) => format!(
                "original restored; quarantine link {} not removed: {err}",
                quarantine.display()
            ),
        },
        Err(err) => format!(
            "original left at {} because the path could not be restored: {err}",
            quarantine.display()
        ),
    }
}

/// Serializes the probe-quarantine-unlink reclaim sequence in this process.
#[cfg(unix)]
fn reclaim_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Resolve, prepare, reclaim stale, bind, and attest. Fail-soft: every
/// failure is a `String` reason, never a panic. `env` is the explicit
/// resolution input (never re-read from the process environment here).
#[cfg(unix)]
fn try_listen(env: &bitty_ipc::devtools::SocketEnv) -> Result<BoundListener, String> {
    use std::os::unix::net::{UnixListener, UnixStream};

    let _reclaim_guard = reclaim_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (socket_path, instance) = bitty_ipc::devtools::resolve_socket_path_from_env(env, None)
        .map_err(|err| {
            format!("socket path unavailable: {err} (set XDG_RUNTIME_DIR or BITTY_SOCKET)")
        })?;
    let dir = bitty_ipc::devtools::prepare_socket_dir(&socket_path)
        .map_err(|err| format!("socket directory rejected: {err}"))?;
    if std::fs::symlink_metadata(&socket_path).is_ok() {
        // Stale reclaim: a live peer keeps its socket (connect succeeds); only
        // a refused connect proves the previous instance is dead, and the
        // endpoint identity captured here is re-verified on the quarantined
        // object before it is removed.
        let expected =
            bitty_ipc::devtools::verify_socket_endpoint_for_connect(&socket_path, dir.dir_uid)
                .map_err(|err| format!("existing socket rejected: {err}"))?;
        match UnixStream::connect(&socket_path) {
            Ok(_) => {
                return Err(format!(
                    "socket {socket_path} is already served by a live instance"
                ));
            }
            Err(err) => match classify_socket_probe(&err) {
                SocketProbe::Dead => {
                    reclaim_socket_path(&socket_path, expected, dir.dir_uid)?;
                }
                SocketProbe::Ambiguous => {
                    return Err(format!(
                        "socket probe for {socket_path} was ambiguous; refusing reclaim"
                    ));
                }
            },
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

/// Run `spawn` for an already-reserved connection slot, releasing the slot
/// when the worker thread cannot be created (CTX-0792, #1404): a spawn failure
/// must never leak the active count or leave the accept loop in a false
/// serving state.
#[cfg(unix)]
fn spawn_connection_worker<F>(
    active: &AtomicUsize,
    spawn: F,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() -> std::io::Result<std::thread::JoinHandle<()>>,
{
    match spawn() {
        Ok(handle) => Ok(handle),
        Err(error) => {
            active.fetch_sub(1, Ordering::SeqCst);
            Err(error)
        }
    }
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
    services: ConnectionServices,
) {
    use std::time::Duration;

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if !reserve_connection_slot(&active) {
                    crate::logging::warn(|| {
                        String::from("bitty: ipc connection shed (at RC-9 cap, newest first)")
                    });
                    drop(stream);
                    continue;
                }
                // CTX-0506: BSD/macOS `accept()` inherits `O_NONBLOCK` from
                // the non-blocking listener (Linux clears it), so the served
                // stream would return `WouldBlock` on its first read and be
                // closed as an idle connection before the peer's first frame
                // arrived. Restore blocking semantics explicitly; the
                // per-connection read/write timeouts in `serve_stream` remain
                // the idle bound.
                if let Err(err) = normalize_accepted_stream(&stream) {
                    crate::logging::warn(|| format!("bitty: ipc connection setup failed: {err}"));
                    active.fetch_sub(1, Ordering::SeqCst);
                    drop(stream);
                    continue;
                }
                let dispatcher = Arc::clone(&dispatcher);
                let server = server.clone();
                let worker_active = Arc::clone(&active);
                let services = services.clone();
                let spawned = spawn_connection_worker(&active, || {
                    std::thread::Builder::new()
                        .name("bitty-ipc-connection".into())
                        .spawn(move || {
                            let _counted = ActiveCount::new(worker_active);
                            serve_stream(stream, runtime_uid, &dispatcher, &server, &services);
                        })
                });
                if let Err(err) = spawned {
                    crate::logging::warn(|| format!("bitty: ipc worker unavailable: {err}"));
                }
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
                crate::logging::warn(|| format!("bitty: ipc accept error: {err}"));
                std::thread::sleep(Duration::from_millis(
                    bitty_ipc::devtools::ACCEPT_POLL_INTERVAL_MS * 5,
                ));
            }
        }
    }
}

#[cfg(unix)]
fn reserve_connection_slot(active: &AtomicUsize) -> bool {
    let mut current = active.load(Ordering::SeqCst);
    loop {
        if current >= bitty_ipc::devtools::max_connections() {
            return false;
        }
        match active.compare_exchange_weak(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => return true,
            Err(next) => current = next,
        }
    }
}

/// Restore blocking reads on a freshly accepted stream (CTX-0506).
///
/// BSD/macOS `accept()` inherits `O_NONBLOCK` from the non-blocking listener
/// (Linux clears it), so a served stream can return `WouldBlock` on its first
/// read and be closed as an idle connection before the peer's first frame
/// arrives. The per-connection read/write timeouts set in `serve_stream`
/// remain the idle bound after normalization.
#[cfg(unix)]
fn normalize_accepted_stream(stream: &std::os::unix::net::UnixStream) -> std::io::Result<()> {
    stream.set_nonblocking(false)
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

#[cfg(unix)]
fn serve_stream(
    mut stream: std::os::unix::net::UnixStream,
    runtime_uid: u32,
    dispatcher: &bitty_ipc::devtools::Dispatcher,
    server: &bitty_ipc::devtools::ServerInfo,
    services: &ConnectionServices,
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
    // ── accept boundary (CTX-0768 peer proof, then CTX-0792 authority) ──
    //
    // Order matters and is security-significant:
    //
    // 1. the socket *endpoint* is verified (path owner/mode) — a cheap check
    //    that the path we are about to use is still ours;
    // 2. a server-owned connection *authority* is opened for this connection
    //    (#1403): a fresh principal/session whose scopes and per-terminal
    //    capabilities are the only authority any request on it can use;
    // 3. the *accepted stream* is attested against the runtime uid and the
    //    resulting proof is bound to the context. This is the P0-AC-021
    //    marker CTX-0768 established: the peer is proved from kernel-reported
    //    credentials of the accepted descriptor, not from a path lookup.
    // 4. the local mark is taken from the bound proof identity only.
    //
    // Step 3 is NOT replaced by `transport_attested_peer`: that call verifies
    // the socket *endpoint* (the path's owner), which the base implementation
    // used, whereas the bound proof covers the accepted stream's own peer
    // credentials and keeps a recheck closure for the life of the connection.
    if let Err(err) =
        bitty_ipc::devtools::verify_socket_endpoint_for_connect(&server.socket_path, runtime_uid)
    {
        crate::logging::warn(|| {
            format!("bitty: ipc connection rejected (endpoint verification): {err}")
        });
        return;
    }
    let capabilities = bitty_ipc::ctl::TerminalCapabilities::from_scopes(&services.granted);
    let grant = match services
        .authority
        .open_connection(services.granted.clone(), capabilities)
    {
        Ok(grant) => grant,
        Err(err) => {
            crate::logging::warn(|| format!("bitty: ipc connection authority rejected: {err}"));
            return;
        }
    };
    let mut context = bitty_ipc::devtools::ServeContext::with_connection_grant(server, grant);
    let proof = match context.bind_connected_stream(&stream, runtime_uid) {
        Ok(proof) => proof,
        Err(err) => {
            crate::logging::warn(|| {
                format!("bitty: ipc connection rejected (peer attestation): {err}")
            });
            return;
        }
    };
    context.attest_local_peer(&proof.identity());
    let clock = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    };
    // Proof-first, then budget: `serve_bound_connection_with_endpoint_budget`
    // re-matches the bound proof to the accepted stream and rechecks the live
    // peer before serving a byte, then admits every request through the
    // shared endpoint limiter.
    let result = bitty_ipc::devtools::serve_bound_connection_with_endpoint_budget(
        &mut stream,
        &proof,
        dispatcher,
        &context,
        &services.endpoint_limiter,
        &clock,
    );
    // Session end: revoke the connection's authority and every automation
    // bearer bound to it before the worker exits (the context's `Drop` does
    // the same on an unwinding path).
    context.close_connection();
    match result {
        Ok(stats) => {
            if stats.denied > 0 || stats.framing_errors > 0 {
                crate::logging::warn(|| {
                    format!(
                        "bitty: ipc connection closed (requests={} denied={} framing_errors={})",
                        stats.requests, stats.denied, stats.framing_errors
                    )
                });
            }
        }
        Err(err) => {
            crate::logging::warn(|| format!("bitty: ipc connection rejected: {err}"));
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
            failure_reason: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            #[cfg(unix)]
            handle: None,
        };
        assert!(guard.is_enabled());
        assert_eq!(guard.socket_path(), "/tmp/bitty-test.sock");
        assert_eq!(guard.failure_reason(), None);
    }

    #[test]
    fn failure_reason_is_absent_unless_serving_failed() {
        // CTX-0481: only a genuine servable-platform failure carries a
        // reason; the platform-unsupported shape stays `None` so
        // `--fail-loud` never aborts a platform that cannot serve.
        assert_eq!(IpcServeGuard::disabled_for_tests().failure_reason(), None);
        assert_eq!(
            IpcServeGuard::failed_for_tests("bind failed").failure_reason(),
            Some("bind failed")
        );
        assert!(!IpcServeGuard::failed_for_tests("bind failed").is_enabled());
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    ))]
    #[test]
    fn supported_unix_mode_uses_real_stream_attestation() {
        assert!(bitty_ipc::accepted_stream_peer_attestation_available());
        // Provide an explicit socket base: some environments (macOS CI
        // runners set neither variable) carry no runtime dir, and the
        // servo deliberately fails closed without a derivable base. The
        // env is passed explicitly so the process environment is never
        // mutated (this module forbids unsafe code).
        let socket_path = std::env::temp_dir()
            .join(format!("bitty-serve-{}", std::process::id()))
            .join("s.sock")
            .to_string_lossy()
            .into_owned();
        let env = bitty_ipc::devtools::SocketEnv {
            bitty_socket: Some(socket_path.clone()),
            ..Default::default()
        };
        let guard = unix_serve(
            ServerDescriptor {
                cols: 80,
                rows: 24,
                test_mode: false,
            },
            &env,
        );
        assert!(guard.is_enabled());
        assert_eq!(guard.socket_path(), socket_path.as_str());
    }

    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
        )),
    ))]
    #[test]
    fn unsupported_unix_mode_fails_closed() {
        let guard = serve_in_background(ServerDescriptor {
            cols: 80,
            rows: 24,
            test_mode: false,
        });
        assert!(!guard.is_enabled());
        assert!(guard.failure_reason().is_none());
        assert!(guard.socket_path().is_empty());
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    ))]
    #[test]
    fn accepted_stream_proof_uses_platform_credentials() {
        let (_client, stream) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut context = bitty_ipc::devtools::ServeContext::new(
            &bitty_ipc::devtools::ServerInfo::new("proof".into(), "proof.sock".into(), 80, 24),
        );
        let proof = context.bind_connected_stream_current(&stream).unwrap();
        assert_eq!(
            proof.identity().peer_uid(),
            bitty_ipc::current_unix_uid().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_probe_rejects_ambiguous_errors() {
        assert_eq!(
            classify_socket_probe(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            SocketProbe::Ambiguous
        );
        assert_eq!(
            classify_socket_probe(&std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
            SocketProbe::Dead
        );
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_connection_cap_reserves_newest_last() {
        let active = AtomicUsize::new(0);
        for _ in 0..bitty_ipc::devtools::max_connections() {
            assert!(reserve_connection_slot(&active));
        }
        assert!(!reserve_connection_slot(&active));
        assert_eq!(
            active.load(Ordering::SeqCst),
            bitty_ipc::devtools::max_connections()
        );
    }

    #[cfg(unix)]
    #[test]
    fn worker_spawn_failure_rolls_back_endpoint_slot() {
        let active = AtomicUsize::new(1);
        let result = spawn_connection_worker(&active, || {
            Err::<std::thread::JoinHandle<()>, std::io::Error>(std::io::Error::other("injected"))
        });
        assert!(result.is_err());
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    /// A dead socket at `name` inside a fresh private scratch dir, with the
    /// endpoint identity `try_listen` would capture before its probe.
    #[cfg(unix)]
    fn dead_socket_fixture(
        name: &str,
    ) -> (
        std::path::PathBuf,
        String,
        bitty_ipc::devtools::SocketEndpointIdentity,
        u32,
    ) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("bitty-reclaim-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("dir mode");
        let path = dir.join("s.sock");
        // Binding then dropping the listener leaves a socket inode nobody
        // serves: a connect is refused, exactly the dead-instance shape.
        drop(std::os::unix::net::UnixListener::bind(&path).expect("bind"));
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(bitty_ipc::SOCKET_MODE),
        )
        .expect("socket mode");
        let meta = std::fs::symlink_metadata(&path).expect("socket metadata");
        let identity = bitty_ipc::devtools::SocketEndpointIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
            uid: meta.uid(),
            mode: meta.mode() & 0o777,
        };
        let path_str = path.to_str().expect("utf-8 path").to_string();
        (dir, path_str, identity, meta.uid())
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_removes_a_verified_dead_socket() {
        let (dir, path, identity, uid) = dead_socket_fixture("dead");
        let refused = std::os::unix::net::UnixStream::connect(&path).expect_err("dead socket");
        assert_eq!(classify_socket_probe(&refused), SocketProbe::Dead);
        reclaim_socket_path(&path, identity, uid).expect("verified dead socket reclaims");
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "dead socket removed"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_refuses_a_replaced_endpoint_and_restores_it() {
        use std::os::unix::fs::MetadataExt;
        let (dir, path, identity, uid) = dead_socket_fixture("swap");
        // The probe saw the dead socket; a different object replaces the path
        // before the destructive step (inode/descriptor replacement).
        std::fs::remove_file(&path).expect("unlink original");
        std::fs::write(&path, b"replacement").expect("replacement");
        let replacement_ino = std::fs::symlink_metadata(&path).expect("stat").ino();
        let err = reclaim_socket_path(&path, identity, uid).expect_err("replacement refused");
        assert!(err.contains("changed during reclaim"), "{err}");
        let restored = std::fs::symlink_metadata(&path).expect("replacement restored");
        assert_eq!(
            restored.ino(),
            replacement_ino,
            "the replacement is never unlinked"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_restore_never_clobbers_an_occupied_path() {
        let (dir, path, _identity, _uid) = dead_socket_fixture("occupied");
        // The quarantined object, plus a new endpoint bound at the original
        // path between the quarantine and the restore.
        let quarantine = dir.join(".s.sock.reclaim-test");
        std::fs::rename(&path, &quarantine).expect("quarantine");
        std::fs::write(&path, b"new instance").expect("occupant");
        let note = restore_quarantined(&quarantine, &path);
        assert!(note.contains("original left at"), "{note}");
        assert!(
            note.contains(".s.sock.reclaim-test"),
            "the stranded object is named: {note}"
        );
        assert_eq!(
            std::fs::read(&path).expect("occupant intact"),
            b"new instance",
            "the new endpoint is never replaced"
        );
        assert!(
            std::fs::symlink_metadata(&quarantine).is_ok(),
            "quarantine kept"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_probe_is_ambiguous_for_every_error_but_refusal() {
        use std::io::ErrorKind;
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::NotFound,
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
            ErrorKind::AddrInUse,
            ErrorKind::AddrNotAvailable,
            ErrorKind::ConnectionReset,
            ErrorKind::Interrupted,
            ErrorKind::OutOfMemory,
            ErrorKind::Other,
        ] {
            assert_eq!(
                classify_socket_probe(&std::io::Error::from(kind)),
                SocketProbe::Ambiguous,
                "{kind:?} must never authorize an unlink"
            );
        }
        assert_eq!(
            classify_socket_probe(&std::io::Error::from(ErrorKind::ConnectionRefused)),
            SocketProbe::Dead
        );
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_refuses_a_symlinked_endpoint() {
        let (dir, path, identity, uid) = dead_socket_fixture("link");
        let target = dir.join("target.sock");
        std::fs::rename(&path, &target).expect("move socket");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
        let err = reclaim_socket_path(&path, identity, uid).expect_err("symlink refused");
        assert!(err.contains("symlink"), "{err}");
        assert!(
            std::fs::symlink_metadata(&path)
                .expect("link restored")
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::symlink_metadata(&target).is_ok(),
            "target untouched"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn descriptor_carries_grid_geometry() {
        let descriptor = ServerDescriptor {
            cols: 80,
            rows: 24,
            test_mode: false,
        };
        assert_eq!(descriptor.cols, 80);
        assert_eq!(descriptor.rows, 24);
    }

    #[cfg(unix)]
    #[test]
    fn accepted_stream_is_normalized_to_blocking() {
        use std::io::{Read, Write};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::time::{Duration, Instant};

        // Regression (CTX-0506): BSD/macOS `accept()` inherits `O_NONBLOCK`
        // from a non-blocking listener, so an un-normalized accepted stream
        // returns `WouldBlock` on its first read and the connection is closed
        // as idle before the first frame arrives. A slow writer proves the
        // normalized stream waits for the byte instead of returning early.
        let dir = std::env::temp_dir().join(format!("bitty-acc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("s.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let mut client = UnixStream::connect(&path).expect("connect");
        let (mut stream, _) = listener.accept().expect("accept");
        normalize_accepted_stream(&stream).expect("normalize");
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("read timeout");

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _ = client.write_all(b"x");
        });
        let start = Instant::now();
        let mut byte = [0u8; 1];
        stream
            .read_exact(&mut byte)
            .expect("normalized stream must wait for the peer byte");
        assert_eq!(byte, [b'x']);
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "read returned before the peer wrote; stream stayed non-blocking"
        );
        drop(stream);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn serve_path_uses_real_bound_stream_proof() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let dispatcher = bitty_ipc::devtools::Dispatcher::with_defaults();
        let info = bitty_ipc::devtools::ServerInfo::new(
            "test".to_string(),
            "/tmp/verified-marker.sock".to_string(),
            80,
            24,
        );
        let mut context = bitty_ipc::devtools::ServeContext::new(&info);
        let proof = context.bind_connected_stream_current(&server).unwrap();
        context.attest_local_peer(&proof.identity());
        let mut limiter = bitty_ipc::limits::RateLimiter::rc9_default();
        let clock = || 0u64;
        let handle = std::thread::spawn(move || {
            bitty_ipc::devtools::serve_bound_connection(
                &mut server,
                &proof,
                &dispatcher,
                &context,
                &mut limiter,
                &clock,
            )
        });
        let payload = br#"{"id":1,"method":"bitty.debug/ping","version":"1.0"}"#;
        let wire = bitty_ipc::frame::encode_frame(payload).unwrap();
        client.write_all(&wire).unwrap();
        let mut header = [0u8; 4];
        client.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header) as usize;
        let mut body = vec![0u8; len];
        client.read_exact(&mut body).unwrap();
        assert!(String::from_utf8(body).unwrap().contains("\"ok\":true"));
        drop(client);
        let stats = handle.join().unwrap().unwrap();
        assert_eq!(stats.requests, 1);
    }
}
