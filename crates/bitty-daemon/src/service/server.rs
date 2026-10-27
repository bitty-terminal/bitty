//! The `bittyd` admission loop: endpoint, connections, frames, and shutdown
//! (`D-05`, `D-07`, `D-15`, `D-16`).
//!
//! The loop is single-threaded and poll-driven. Startup order is:
//!
//! 1. bind and attest the per-user endpoint (`0700` directory, `0600` socket,
//!    single-instance `flock`);
//! 2. mint and persist the per-boot operator capability (`0600` file);
//! 3. open the owner-private audit sink;
//! 4. block `SIGTERM`/`SIGINT` and create a signal fd, so the same loop that
//!    serves requests also runs the signal drain without a signal-handler
//!    hack.
//!
//! Each accepted connection captures the peer before parsing a frame
//! ([`crate::peer::PeerGuard::capture`]), gets one server-assigned [`crate::handshake::ClientId`], and starts
//! with the baseline consumer scopes. The connection cap
//! (`max_connections_per_endpoint`), the request rate and burst
//! (`max_request_rate_per_sec`, `max_request_burst`), the per-frame bound
//! (`max_frame_bytes`), the pre-authentication byte budget
//! (`pre_auth_max_bytes`), the negotiate deadline (`negotiate_timeout_ms`), and
//! the idle bound (`transport_idle_ms`) are the compiled `D-14` values; none is
//! a hardcoded second copy.
//!
//! `daemon.stop` (or a signal) sets the stop request; the loop finishes the
//! current responses, closes every connection, removes the socket only while
//! the path still names this process's object, and returns the outcome the CLI
//! maps to an exit code. An endpoint tamper stops acceptance immediately and is
//! the contract's exit-code `6` path; it is never retried through a weaker
//! transport, and no TCP listener is ever opened.

#[cfg(unix)]
mod unix {
    use std::fmt;
    use std::io::{ErrorKind, Write};
    use std::os::fd::AsFd;
    use std::path::PathBuf;
    use std::time::Instant;

    use bitty_ipc_core::RateLimiter;
    use serde_json::Value;

    use crate::audit_sink::{AuditFileWriter, AuditSinkError, default_audit_dir};
    use crate::endpoint::{BoundEndpoint, EndpointEnv, EndpointError, open_endpoint};
    use crate::handshake::{ClientId, EffectiveScopes, EndpointKind, ServerIdentity};
    use crate::limits::{LimitId, Limits};
    use crate::operator::{OperatorError, OperatorStore};
    use crate::registry::MonotonicMs;
    use crate::service::dispatch::{Refusal, ServerFacts, Service, ServiceError, StopMode};
    use crate::service::frame::{FramePeer, PeerFrameReader, enable_credentials};
    use crate::service::protocol::Connection;
    use crate::service::scopes::{MethodResolution, ServiceMethod, ServiceScopeSet, resolve};
    use crate::service::wire::{self, RequestEnvelope};

    /// How often the loop re-attests the endpoint.
    pub const TAMPER_INTERVAL_MS: u64 = 5_000;

    /// Poll tick; bounds the idle, negotiate, and tamper timers.
    pub const TICK_MS: u16 = 250;

    /// The loop configuration.
    #[derive(Debug, Clone)]
    pub struct ServerConfig {
        /// Effective limits for this run.
        pub limits: Limits,
        /// Endpoint resolution inputs (never read from the environment directly by
        /// the loop, so tests can pass an explicit runtime root).
        pub endpoint_env: EndpointEnv,
        /// Owner-private audit directory.
        pub audit_dir: PathBuf,
        /// Transport scopes granted to every same-user connection.
        ///
        /// In this slice it is the `bitty-ipc` CLI default plus `process.spawn`:
        /// the daemon spawns no worker yet (bitty#2604 owns the spawn path and its
        /// executable allowlist), so `process.spawn` gates nothing here and
        /// `session.host` consent remains the effective create authority. When the
        /// spawn path lands, the grant must come from the executable policy
        /// instead of this transport default.
        pub transport_scopes: bitty_ipc_api::scope::ScopeSet,
    }

    impl ServerConfig {
        /// Read the endpoint and audit roots from the process environment.
        ///
        /// # Errors
        ///
        /// [`ServerError::NoAuditDirectory`] when neither `XDG_STATE_HOME` nor
        /// `HOME` is usable; the daemon refuses to run with an unwritten sink.
        pub fn from_process() -> Result<Self, ServerError> {
            let audit_dir = default_audit_dir().ok_or(ServerError::NoAuditDirectory)?;
            let mut transport_scopes = bitty_ipc_api::scope::ScopeSet::cli_default();
            transport_scopes.insert(bitty_ipc_api::scope::Scope::ProcessSpawn);
            Ok(Self {
                limits: Limits::default(),
                endpoint_env: EndpointEnv::from_process(),
                audit_dir,
                transport_scopes,
            })
        }
    }

    /// The loop outcome, mapped by the CLI to an exit code.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ServerOutcome {
        /// `daemon.stop` requested a clean stop.
        Stopped {
            /// The accepted mode.
            mode: StopMode,
        },
        /// A `SIGTERM`/`SIGINT` signal ran the drain policy.
        Signalled,
        /// The endpoint failed an attestation; the contract's exit code `6`.
        Tampered,
    }

    /// A startup or loop failure.
    #[derive(Debug)]
    pub enum ServerError {
        /// Another instance already holds the endpoint lock.
        AlreadyRunning {
            /// The endpoint socket of the running instance.
            socket: PathBuf,
        },
        /// The endpoint could not be opened or attested.
        Endpoint(EndpointError),
        /// The operator capability could not be minted.
        Operator(OperatorError),
        /// The audit sink could not be opened.
        Audit(AuditSinkError),
        /// The service state could not be constructed.
        Service(ServiceError),
        /// No audit directory can be derived.
        NoAuditDirectory,
        /// A signal mask or signal fd operation failed.
        Signals,
        /// A filesystem or socket operation failed.
        Io,
    }

    impl fmt::Display for ServerError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::AlreadyRunning { socket } => write!(
                    formatter,
                    "bittyd is already running for this user (socket {})",
                    socket.display()
                ),
                Self::Endpoint(error) => write!(formatter, "{error}"),
                Self::Operator(error) => write!(formatter, "operator capability: {error}"),
                Self::Audit(error) => write!(formatter, "audit sink: {error}"),
                Self::Service(error) => write!(formatter, "service: {error}"),
                Self::NoAuditDirectory => {
                    formatter.write_str("no XDG_STATE_HOME or HOME for the audit sink")
                }
                Self::Signals => formatter.write_str("signal setup failed"),
                Self::Io => formatter.write_str("socket or file operation failed"),
            }
        }
    }

    impl std::error::Error for ServerError {}

    /// One accepted peer and its buffered frame state.
    struct PeerIo {
        stream: std::os::unix::net::UnixStream,
        client: Connection,
        facts: ServerFacts,
        reader: PeerFrameReader,
        output: Vec<u8>,
        limiter: RateLimiter,
        accepted_at_ms: u64,
        last_activity_ms: u64,
        pre_auth_bytes: usize,
        closed: bool,
    }

    impl PeerIo {
        fn wants_write(&self) -> bool {
            !self.closed && !self.output.is_empty()
        }

        fn close(&mut self) {
            self.closed = true;
            self.output.clear();
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Run the service loop until `daemon.stop` or a signal.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::AlreadyRunning`] for a second instance (typed, no
    /// state touched), and the startup failures that must prevent the daemon from
    /// accepting a connection.
    pub fn run(config: ServerConfig) -> Result<ServerOutcome, ServerError> {
        use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
        use nix::sys::signalfd::{SfdFlags, SignalFd};

        let endpoint = open_endpoint(&config.endpoint_env).map_err(|error| match error {
            EndpointError::AlreadyRunning { socket, .. } => ServerError::AlreadyRunning { socket },
            other => ServerError::Endpoint(other),
        })?;
        let operator =
            OperatorStore::create_in(endpoint.runtime_dir()).map_err(ServerError::Operator)?;
        let writer =
            AuditFileWriter::open(&config.audit_dir, &config.limits).map_err(ServerError::Audit)?;
        let mut service = Service::new(config.limits.clone(), operator, Box::new(writer))
            .map_err(ServerError::Service)?;
        let facts = server_facts(&endpoint)?;

        let mut mask = SigSet::empty();
        mask.add(Signal::SIGTERM);
        mask.add(Signal::SIGINT);
        pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)
            .map_err(|_| ServerError::Signals)?;
        let signal_fd =
            SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC).map_err(|_| ServerError::Signals)?;

        endpoint
            .listener()
            .set_nonblocking(true)
            .map_err(|_| ServerError::Io)?;

        let start = Instant::now();
        let now = || MonotonicMs::from_millis(start.elapsed().as_millis() as u64);
        let mut peers: Vec<PeerIo> = Vec::new();
        let mut next_tamper = TAMPER_INTERVAL_MS;
        let mut shutting_down = false;

        loop {
            let current = now();
            if shutting_down && peers.iter().all(|peer| !peer.wants_write()) {
                break;
            }
            let mut poll_fds = Vec::with_capacity(peers.len() + 2);
            poll_fds.push(nix::poll::PollFd::new(
                endpoint.listener().as_fd(),
                nix::poll::PollFlags::POLLIN,
            ));
            poll_fds.push(nix::poll::PollFd::new(
                signal_fd.as_fd(),
                nix::poll::PollFlags::POLLIN,
            ));
            for peer in &peers {
                let mut flags = nix::poll::PollFlags::POLLIN;
                if peer.wants_write() {
                    flags |= nix::poll::PollFlags::POLLOUT;
                }
                poll_fds.push(nix::poll::PollFd::new(peer.stream.as_fd(), flags));
            }
            nix::poll::poll(&mut poll_fds, TICK_MS).map_err(|_| ServerError::Io)?;
            let listener_ready = poll_fds[0]
                .revents()
                .is_some_and(|flags| flags.contains(nix::poll::PollFlags::POLLIN));
            let signal_ready = poll_fds[1]
                .revents()
                .is_some_and(|flags| flags.contains(nix::poll::PollFlags::POLLIN));
            let peer_ready: Vec<nix::poll::PollFlags> = poll_fds
                .iter()
                .skip(2)
                .map(|fd| fd.revents().unwrap_or_else(nix::poll::PollFlags::empty))
                .collect();
            drop(poll_fds);

            if signal_ready
                && signal_fd
                    .read_signal()
                    .map_err(|_| ServerError::Signals)?
                    .is_some()
            {
                let _ = service.signal_stop(current);
                shutting_down = true;
            }

            if listener_ready && !shutting_down {
                let cap =
                    usize::try_from(config.limits.effective(LimitId::MaxConnectionsPerEndpoint))
                        .unwrap_or(usize::MAX);
                loop {
                    match endpoint.listener().accept() {
                        Ok((stream, _)) => {
                            if peers.iter().filter(|peer| !peer.closed).count() >= cap {
                                drop(stream);
                                continue;
                            }
                            if let Some(peer) = accept_peer(stream, &config, &facts, current) {
                                peers.push(peer);
                            }
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
            }

            for (index, peer) in peers.iter_mut().enumerate() {
                if peer.closed {
                    continue;
                }
                let ready = peer_ready
                    .get(index)
                    .copied()
                    .unwrap_or_else(nix::poll::PollFlags::empty);
                if ready.contains(nix::poll::PollFlags::POLLIN) {
                    read_peer(peer, &mut service, &config, current, &mut shutting_down);
                }
                if !peer.closed
                    && (ready.contains(nix::poll::PollFlags::POLLOUT) || peer.wants_write())
                {
                    flush_peer(peer);
                }
            }

            let idle = config.limits.effective(LimitId::TransportIdleMs);
            let negotiate = config.limits.effective(LimitId::NegotiateTimeoutMs);
            for peer in &mut peers {
                let age = current.as_millis().saturating_sub(peer.last_activity_ms);
                let negotiate_deadline = peer.accepted_at_ms.saturating_add(negotiate);
                let stale_handshake =
                    !peer.client.is_negotiated() && current.as_millis() >= negotiate_deadline;
                let idle_expired = age >= idle && !peer.wants_write();
                if stale_handshake || idle_expired {
                    peer.close();
                }
            }
            peers.retain(|peer| !peer.closed);

            if current.as_millis() >= next_tamper {
                if endpoint.reattest().is_err() {
                    for peer in &mut peers {
                        peer.close();
                    }
                    drop(peers);
                    let _ = endpoint.close();
                    return Ok(ServerOutcome::Tampered);
                }
                next_tamper = current.as_millis().saturating_add(TAMPER_INTERVAL_MS);
            }

            if service.stop_requested().is_some() {
                shutting_down = true;
            }
        }

        for peer in &mut peers {
            peer.close();
        }
        drop(peers);
        let _ = endpoint.close();
        Ok(match service.stop_requested() {
            Some(mode) => ServerOutcome::Stopped { mode },
            None => ServerOutcome::Signalled,
        })
    }

    fn accept_peer(
        stream: std::os::unix::net::UnixStream,
        config: &ServerConfig,
        facts: &ServerFacts,
        now: MonotonicMs,
    ) -> Option<PeerIo> {
        // Enable credential receiving before any frame can be read: the kernel
        // attaches SCM_CREDENTIALS based on the flag at send time.
        enable_credentials(&stream).ok()?;
        let peer = crate::peer::PeerGuard::capture(&stream).ok()?;
        let client_id = ClientId::generate().ok()?;
        let scopes = EffectiveScopes {
            service: ServiceScopeSet::baseline(),
            transport: config.transport_scopes.clone(),
        };
        let client = Connection::new(client_id, peer, scopes);
        stream.set_nonblocking(true).ok()?;
        Some(PeerIo {
            stream,
            client,
            facts: *facts,
            reader: PeerFrameReader::new(),
            output: Vec::new(),
            limiter: RateLimiter::new(
                u32::try_from(config.limits.effective(LimitId::MaxRequestRatePerSec))
                    .unwrap_or(u32::MAX),
                u32::try_from(config.limits.effective(LimitId::MaxRequestBurst))
                    .unwrap_or(u32::MAX),
            ),
            accepted_at_ms: now.as_millis(),
            last_activity_ms: now.as_millis(),
            pre_auth_bytes: 0,
            closed: false,
        })
    }

    fn read_peer(
        peer: &mut PeerIo,
        service: &mut Service,
        config: &ServerConfig,
        now: MonotonicMs,
        shutting_down: &mut bool,
    ) {
        let expected = FramePeer {
            uid: peer.client.peer_uid(),
            pid: peer.client.peer().identity().pid(),
        };
        loop {
            let payload = match peer.reader.read(&peer.stream, expected) {
                Ok(Some(payload)) => payload,
                Ok(None) => return,
                Err(_) => {
                    peer.close();
                    return;
                }
            };
            peer.last_activity_ms = now.as_millis();
            peer.pre_auth_bytes = peer.pre_auth_bytes.saturating_add(4 + payload.len());
            if !peer.client.is_negotiated()
                && peer.pre_auth_bytes
                    > usize::try_from(config.limits.effective(LimitId::PreAuthMaxBytes))
                        .unwrap_or(usize::MAX)
            {
                peer.close();
                return;
            }
            if !handle_payload(peer, service, &payload, now, shutting_down) {
                peer.close();
                return;
            }
            if peer.output.len()
                > usize::try_from(config.limits.effective(LimitId::MaxConnectionOutboundBytes))
                    .unwrap_or(usize::MAX)
            {
                peer.close();
                return;
            }
        }
    }

    fn flush_peer(peer: &mut PeerIo) {
        while !peer.output.is_empty() {
            match peer.stream.write(&peer.output) {
                Ok(0) => {
                    peer.close();
                    return;
                }
                Ok(written) => {
                    peer.output.drain(..written);
                    peer.last_activity_ms = peer.last_activity_ms.max(peer.accepted_at_ms);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(_) => {
                    peer.close();
                    return;
                }
            }
        }
    }

    /// Handle one frame payload. Returns `false` when the connection must close.
    fn handle_payload(
        peer: &mut PeerIo,
        service: &mut Service,
        payload: &[u8],
        now: MonotonicMs,
        shutting_down: &mut bool,
    ) -> bool {
        let Some(id) = envelope_id(payload) else {
            return false;
        };
        if peer.limiter.check(now.as_millis()).is_err() {
            return reply(
                peer,
                &id,
                Err(Refusal::denied(crate::codes::RefusalCode::RateLimited)),
            );
        }
        let envelope = match RequestEnvelope::parse(payload) {
            Ok(envelope) => envelope,
            Err(refusal) => return reply(peer, &id, Err(refusal)),
        };
        let method = match resolve(&envelope.method) {
            MethodResolution::Known(method) => method,
            MethodResolution::Unknown => return reply(peer, &id, Err(Refusal::not_found())),
            MethodResolution::Malformed => return reply(peer, &id, Err(Refusal::invalid_method())),
        };
        if !peer.client.is_negotiated() && method != ServiceMethod::DaemonNegotiate {
            return reply(
                peer,
                &id,
                Err(Refusal::denied(
                    crate::codes::RefusalCode::AlreadyNegotiated,
                )),
            );
        }
        let result = if method == ServiceMethod::DaemonNegotiate {
            negotiate(peer, service, envelope.params, now)
        } else {
            service.dispatch(&mut peer.client, method, &envelope.params, now)
        };
        if result.is_ok() {
            service.route_deliveries(&mut peer.client);
        }
        let keeps_open = reply(peer, &id, result);
        if service.stop_requested().is_some() {
            *shutting_down = true;
        }
        keeps_open
    }

    fn negotiate(
        peer: &mut PeerIo,
        service: &mut Service,
        params: Value,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let request = wire::parse_negotiate(&params)?;
        let facts = peer.facts;
        let response = service.negotiate(&mut peer.client, &request, facts, now)?;
        wire::handshake_value(&response)
    }

    /// Append a rendered response. Returns `false` when the connection must close.
    fn reply(peer: &mut PeerIo, id: &str, result: Result<Value, Refusal>) -> bool {
        let rendered = match result {
            Ok(value) => wire::render_success(id, &value),
            Err(refusal) => wire::render_error(id, refusal),
        };
        let Ok(payload) = rendered else {
            return false;
        };
        match wire::frame(&payload) {
            Ok(bytes) => {
                peer.output.extend_from_slice(&bytes);
                true
            }
            Err(_) => false,
        }
    }

    /// The envelope's correlation id, when it is safe to echo.
    fn envelope_id(payload: &[u8]) -> Option<String> {
        let value: Value = serde_json::from_slice(payload).ok()?;
        let id = value.get("id")?.as_str()?;
        if id.is_empty() || id.len() > bitty_ipc_api::wire::MAX_ID_BYTES {
            return None;
        }
        Some(id.to_owned())
    }

    fn server_facts(endpoint: &BoundEndpoint) -> Result<ServerFacts, ServerError> {
        use std::os::unix::fs::MetadataExt;

        let metadata =
            std::fs::symlink_metadata(endpoint.socket_path()).map_err(|_| ServerError::Io)?;
        let uid = nix::unistd::geteuid().as_raw();
        Ok(ServerFacts {
            server_identity: ServerIdentity {
                uid,
                endpoint_kind: EndpointKind::UnixSocket,
                endpoint_owner: metadata.uid(),
                endpoint_mode: metadata.mode() & 0o777,
            },
            daemon_pid: std::process::id(),
        })
    }
}

#[cfg(unix)]
pub use unix::{ServerConfig, ServerError, ServerOutcome, TAMPER_INTERVAL_MS, TICK_MS, run};

#[cfg(not(unix))]
mod unsupported {
    use std::fmt;
    use std::path::PathBuf;

    use crate::endpoint::EndpointEnv;
    use crate::limits::Limits;

    /// How often a served loop would re-attest the endpoint.
    pub const TAMPER_INTERVAL_MS: u64 = 5_000;

    /// Poll tick for a served loop.
    pub const TICK_MS: u16 = 250;

    /// The loop configuration (unused on an unsupported cell).
    #[derive(Debug, Clone)]
    pub struct ServerConfig {
        /// Effective limits.
        pub limits: Limits,
        /// Endpoint inputs.
        pub endpoint_env: EndpointEnv,
        /// Audit directory.
        pub audit_dir: PathBuf,
        /// Transport scopes.
        pub transport_scopes: bitty_ipc_api::scope::ScopeSet,
    }

    impl ServerConfig {
        /// Refuse to derive a configuration on an unsupported cell.
        ///
        /// # Errors
        ///
        /// Always [`ServerError::UnsupportedPlatform`].
        pub fn from_process() -> Result<Self, ServerError> {
            Err(ServerError::UnsupportedPlatform)
        }
    }

    /// The loop outcome.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ServerOutcome {
        /// A clean stop.
        Stopped {
            /// The accepted mode.
            mode: crate::service::dispatch::StopMode,
        },
        /// A signal drain.
        Signalled,
        /// Endpoint tamper.
        Tampered,
    }

    /// A startup failure.
    #[derive(Debug)]
    pub enum ServerError {
        /// This platform cell has no accepted endpoint transport.
        UnsupportedPlatform,
        /// Another instance already holds the endpoint.
        AlreadyRunning {
            /// The endpoint socket.
            socket: PathBuf,
        },
    }

    impl fmt::Display for ServerError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::UnsupportedPlatform => formatter
                    .write_str("this platform cell has no accepted bittyd endpoint transport"),
                Self::AlreadyRunning { socket } => write!(
                    formatter,
                    "bittyd is already running (socket {})",
                    socket.display()
                ),
            }
        }
    }

    impl std::error::Error for ServerError {}

    /// Refuse to run on an unsupported cell; no weaker transport exists.
    ///
    /// # Errors
    ///
    /// Always [`ServerError::UnsupportedPlatform`].
    pub fn run(_config: ServerConfig) -> Result<ServerOutcome, ServerError> {
        Err(ServerError::UnsupportedPlatform)
    }
}

#[cfg(not(unix))]
pub use unsupported::{ServerConfig, ServerError, ServerOutcome, TAMPER_INTERVAL_MS, TICK_MS, run};
