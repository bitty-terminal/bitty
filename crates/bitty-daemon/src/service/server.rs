//! The `bittyd` admission loop: endpoint, connections, frames, and shutdown
//! (`D-05`, `D-07`, `D-15`, `D-16`).
//!
//! The loop is single-threaded and poll-driven. Startup order is:
//!
//! 1. apply the `D-16` core-dump controls (`RLIMIT_CORE=0` and
//!    `PR_SET_DUMPABLE=0` on Linux), before any privileged state exists;
//! 2. bind and attest the per-user endpoint (`0700` directory, `0600` socket,
//!    single-instance `flock`);
//! 3. mint and persist the per-boot operator capability (`0600` file);
//! 4. open the owner-private audit sink;
//! 5. block `SIGTERM`/`SIGINT` and create a signal fd, so the same loop that
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

#[cfg(all(unix, target_os = "linux"))]
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

    /// How often the loop sweeps retired grant records.
    ///
    /// Grants expire on the `Deadline` stage, so the sweep is a memory bound,
    /// not an authorization input: a lagging record still fails the TTL check in
    /// `host_grant_bound`/`validate`. One second bounds the retained set to one
    /// second of retired records past the TTL without paying an O(store) sweep
    /// on every request wakeup.
    pub const GRANT_SWEEP_INTERVAL_MS: u64 = 1_000;

    /// Poll tick; bounds the idle, negotiate, and tamper timers.
    pub const TICK_MS: u16 = 250;

    /// The window of the per-connection wire-refusal audit budget.
    pub const WIRE_AUDIT_WINDOW_MS: u64 = 1_000;

    /// How long a retired connection may take to drain its refusal frame.
    ///
    /// A closing refusal is a single small frame, so one second is generous;
    /// the deadline keeps a peer that stops reading from holding a connection
    /// slot (and from being reclassified as an unnegotiated handshake).
    pub const CLOSE_DRAIN_TIMEOUT_MS: u64 = 1_000;

    /// Per-connection budget for wire-level refusal audit records.
    ///
    /// A rate-limited or malformed frame that never resolves to a method would
    /// otherwise write one record per frame; load shedding at the accepted
    /// request rate could rotate both `audit_file_bytes` segments in seconds.
    /// The budget is `refusal_audit_budget_per_sec` records per window;
    /// records beyond it are dropped until the next window. Method refusals
    /// still write one individual record through the dispatch path.
    #[derive(Debug)]
    struct WireAuditBudget {
        window_start_ms: u64,
        recorded: u32,
    }

    impl WireAuditBudget {
        fn new(now_ms: u64) -> Self {
            Self {
                window_start_ms: now_ms,
                recorded: 0,
            }
        }

        /// Whether one more wire refusal may be recorded in this window.
        fn admit(&mut self, now_ms: u64, budget: u32) -> bool {
            if now_ms.saturating_sub(self.window_start_ms) >= WIRE_AUDIT_WINDOW_MS {
                self.window_start_ms = now_ms;
                self.recorded = 0;
            }
            if self.recorded >= budget.max(1) {
                return false;
            }
            self.recorded += 1;
            true
        }
    }

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
        /// Only `process.spawn` is granted: it is the one transport scope the
        /// daemon catalog evaluates (`session.create`'s two-authority rule).
        /// The daemon grants no `terminal.*`, `view.*`, `config.*`, `plugin.*`,
        /// or `debug.*` scope, so the handshake never advertises an ambient
        /// content scope. `session.host` consent remains the effective create
        /// authority; when the spawn path lands, `process.spawn` must come from
        /// the executable policy instead of this transport default.
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
            let mut transport_scopes = bitty_ipc_api::scope::ScopeSet::new();
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
        /// The Linux core-dump controls (`RLIMIT_CORE`, `PR_SET_DUMPABLE`)
        /// could not be applied; the daemon refuses to serve dumpable.
        Hardening,
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
                Self::Hardening => {
                    formatter.write_str("core-dump hardening failed (RLIMIT_CORE/PR_SET_DUMPABLE)")
                }
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
        /// The per-window budget for wire-level refusal records.
        wire_audit: WireAuditBudget,
        /// When a retired connection must be closed even if its refusal frame
        /// has not drained.
        drain_deadline_ms: Option<u64>,
        /// The last reply was a refusal that closes the connection
        /// (`Refusal::closes_connection`). The queued refusal bytes are flushed
        /// first; reading stops and the stream is shut down only once `output`
        /// drains.
        close_after_write: bool,
        closed: bool,
    }

    impl PeerIo {
        fn wants_write(&self) -> bool {
            !self.closed && !self.output.is_empty()
        }

        /// Stop reading and close after the queued bytes are written.
        fn retire(&mut self) {
            self.close_after_write = true;
        }

        /// Whether a retired connection has nothing left to write.
        fn drained(&self) -> bool {
            self.close_after_write && self.output.is_empty()
        }

        fn close(&mut self) {
            self.closed = true;
            self.output.clear();
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Apply the `D-16` core-dump controls before any privileged startup step.
    ///
    /// `RLIMIT_CORE` is lowered to zero and the Linux dumpable flag is cleared
    /// before the endpoint is bound and before the operator capability is
    /// minted, so neither the socket capability file nor a crash artifact can
    /// capture the in-memory control token or operator capability. Both calls
    /// are process-wide and are not undone later. A failure refuses startup
    /// rather than serving dumpable.
    fn harden_core_dumps() -> Result<(), ServerError> {
        use nix::sys::prctl;
        use nix::sys::resource::{Resource, setrlimit};

        setrlimit(Resource::RLIMIT_CORE, 0, 0).map_err(|_| ServerError::Hardening)?;
        prctl::set_dumpable(false).map_err(|_| ServerError::Hardening)?;
        Ok(())
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

        harden_core_dumps()?;
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
        let mut next_sweep = GRANT_SWEEP_INTERVAL_MS;
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
                // A retired connection has already refused; no further frame is
                // read, only the queued refusal bytes are flushed.
                if !peer.close_after_write && ready.contains(nix::poll::PollFlags::POLLIN) {
                    read_peer(peer, &mut service, &config, current, &mut shutting_down);
                }
                if !peer.closed
                    && (ready.contains(nix::poll::PollFlags::POLLOUT) || peer.wants_write())
                {
                    flush_peer(peer);
                }
                if peer.drained() {
                    peer.close();
                } else if peer.close_after_write && peer.drain_deadline_ms.is_none() {
                    peer.drain_deadline_ms =
                        Some(current.as_millis().saturating_add(CLOSE_DRAIN_TIMEOUT_MS));
                }
            }

            // Retired grant records are swept on the loop's own timer, so the
            // store is bounded by its caps, not by how long the daemon runs.
            if current.as_millis() >= next_sweep {
                let _ = service.sweep_grants(current);
                next_sweep = current.as_millis().saturating_add(GRANT_SWEEP_INTERVAL_MS);
            }

            let idle = config.limits.effective(LimitId::TransportIdleMs);
            let negotiate = config.limits.effective(LimitId::NegotiateTimeoutMs);
            for peer in &mut peers {
                match peer_timer(
                    peer.client.is_negotiated(),
                    peer.close_after_write,
                    peer.drain_deadline_ms,
                    peer.accepted_at_ms,
                    peer.last_activity_ms,
                    current.as_millis(),
                    negotiate,
                    idle,
                    peer.wants_write(),
                ) {
                    PeerTimer::Keep => {}
                    PeerTimer::Close => peer.close(),
                    PeerTimer::Timeout => {
                        // The handshake deadline closes with one typed timeout
                        // record (the contract's aggregated audit record).
                        // There is no correlation id for a request that never
                        // completed, so no response frame can be rendered.
                        service.record_refusal(
                            &peer.client,
                            crate::audit::AuditAction::Wire,
                            Refusal::timeout(0, negotiate),
                            current,
                        );
                        peer.close();
                    }
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

    /// The per-peer timer decision for one loop iteration.
    ///
    /// A retired connection is governed only by its drain deadline: it never
    /// collects a second handshake-timeout record while its refusal frame is
    /// still queued.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum PeerTimer {
        Keep,
        Close,
        Timeout,
    }

    #[allow(clippy::too_many_arguments)]
    fn peer_timer(
        negotiated: bool,
        close_after_write: bool,
        drain_deadline_ms: Option<u64>,
        accepted_at_ms: u64,
        last_activity_ms: u64,
        now_ms: u64,
        negotiate_ms: u64,
        idle_ms: u64,
        wants_write: bool,
    ) -> PeerTimer {
        if close_after_write {
            return match drain_deadline_ms {
                Some(deadline) if now_ms >= deadline => PeerTimer::Close,
                _ => PeerTimer::Keep,
            };
        }
        if !negotiated && now_ms >= accepted_at_ms.saturating_add(negotiate_ms) {
            return PeerTimer::Timeout;
        }
        if !wants_write && now_ms.saturating_sub(last_activity_ms) >= idle_ms {
            return PeerTimer::Close;
        }
        PeerTimer::Keep
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
            wire_audit: WireAuditBudget::new(now.as_millis()),
            drain_deadline_ms: None,
            close_after_write: false,
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
        if peer.close_after_write {
            return;
        }
        let expected = FramePeer {
            uid: peer.client.peer_uid(),
            pid: peer.client.peer().identity().pid(),
        };
        loop {
            // Pre-authentication: the byte budget covers completed frames *and*
            // the frame being assembled, so a client cannot dribble a full
            // `max_frame_bytes` body before negotiating. After negotiation the
            // frame bound alone applies.
            let cap = if peer.client.is_negotiated() {
                None
            } else {
                let budget = usize::try_from(config.limits.effective(LimitId::PreAuthMaxBytes))
                    .unwrap_or(usize::MAX);
                Some(budget.saturating_sub(peer.pre_auth_bytes))
            };
            let payload = match peer.reader.read_capped(&peer.stream, expected, cap) {
                Ok(Some(payload)) => payload,
                Ok(None) => return,
                Err(error) => {
                    refuse_frame_closure(peer, service, error, now);
                    return;
                }
            };
            peer.last_activity_ms = now.as_millis();
            peer.pre_auth_bytes = peer.pre_auth_bytes.saturating_add(4 + payload.len());
            if !peer.client.is_negotiated() {
                let budget = usize::try_from(config.limits.effective(LimitId::PreAuthMaxBytes))
                    .unwrap_or(usize::MAX);
                if peer.pre_auth_bytes > budget {
                    let refusal = Refusal::limit_exceeded(
                        u64::try_from(budget).unwrap_or(u64::MAX),
                        u64::try_from(peer.pre_auth_bytes).unwrap_or(u64::MAX),
                    );
                    service.record_refusal(
                        &peer.client,
                        crate::audit::AuditAction::Wire,
                        refusal,
                        now,
                    );
                    // A complete frame has a correlation id, so the refusal is
                    // a typed frame; a partial one was refused mid-assembly
                    // above and can only be audited, never answered. Either
                    // way, let already-queued bytes drain before the close.
                    if let Some((id, _, _)) = envelope_correlation(&payload) {
                        if !reply(peer, &id, Err(refusal)) {
                            peer.close();
                        }
                    } else {
                        peer.retire();
                    }
                    return;
                }
            }
            if !handle_payload(peer, service, &payload, now, shutting_down) {
                peer.close();
                return;
            }
            if peer.close_after_write {
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

    /// Attribute and close a frame-level failure.
    ///
    /// A frame that never completed carries no correlation id, so no
    /// conforming response envelope can be rendered; the refusal is still
    /// recorded once (D-16) and the connection closes. A failure after a
    /// complete frame is answered by `handle_payload` instead.
    fn refuse_frame_closure(
        peer: &mut PeerIo,
        service: &mut Service,
        error: crate::service::frame::FrameError,
        now: MonotonicMs,
    ) {
        use crate::service::frame::FrameError;

        let refusal = match error {
            FrameError::Closed | FrameError::Io => {
                peer.close();
                return;
            }
            FrameError::Unauthenticated => Refusal::unauthenticated(),
            FrameError::FrameTooLarge { limit, actual } => Refusal::limit_exceeded(
                u64::try_from(limit).unwrap_or(u64::MAX),
                u64::try_from(actual).unwrap_or(u64::MAX),
            ),
            FrameError::BudgetExceeded { limit } => Refusal::limit_exceeded(
                u64::try_from(limit).unwrap_or(u64::MAX),
                u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1),
            ),
        };
        service.record_refusal(&peer.client, crate::audit::AuditAction::Wire, refusal, now);
        // Stop reading but let any already-queued response drain before the
        // stream closes; `retire` closes immediately when nothing is queued.
        peer.retire();
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
        use crate::audit::AuditAction;

        let Some((id, method_name, wire_version)) = envelope_correlation(payload) else {
            // No correlation id can be echoed, so there is no conforming
            // response; the refusal is still attributed exactly once. Any
            // earlier response already queued is flushed before the close.
            service.record_refusal(
                &peer.client,
                AuditAction::Wire,
                Refusal::invalid_request(),
                now,
            );
            peer.retire();
            return true;
        };
        if !peer.client.is_negotiated() {
            // Stage 1 stays first: an unsupported per-frame `v` is
            // `VersionMismatch` before the handshake gate.
            match wire_version {
                None => {
                    let refusal = Refusal::invalid_request();
                    record_floodable_wire(peer, service, refusal, now);
                    return reply(peer, &id, Err(refusal));
                }
                Some(version) if bitty_ipc_api::wire::validate_wire_version(version).is_err() => {
                    let refusal = Refusal::version_mismatch();
                    service.record_refusal(&peer.client, AuditAction::Wire, refusal, now);
                    return reply(peer, &id, Err(refusal));
                }
                Some(_) => {}
            }
            // Stage 2, ahead of the rate limiter, the grammar, and the
            // catalog: an unnegotiated connection may only send
            // `daemon.negotiate`. It cannot probe the method catalog
            // (`NotFound`/`InvalidMethod`) and cannot consume a rate budget
            // before the handshake.
            if method_name != crate::contract::NEGOTIATE_METHOD {
                let refusal = Refusal::denied(crate::codes::RefusalCode::AlreadyNegotiated);
                service.record_refusal(&peer.client, AuditAction::Wire, refusal, now);
                return reply(peer, &id, Err(refusal));
            }
        }
        if peer.limiter.check(now.as_millis()).is_err() {
            let refusal = Refusal::denied(crate::codes::RefusalCode::RateLimited);
            record_floodable_wire(peer, service, refusal, now);
            return reply(peer, &id, Err(refusal));
        }
        let envelope = match RequestEnvelope::parse(payload) {
            Ok(envelope) => envelope,
            Err(refusal) => {
                if refusal.closes_connection() {
                    service.record_refusal(&peer.client, AuditAction::Wire, refusal, now);
                } else {
                    record_floodable_wire(peer, service, refusal, now);
                }
                return reply(peer, &id, Err(refusal));
            }
        };
        let method = match resolve(&envelope.method) {
            MethodResolution::Known(method) => method,
            MethodResolution::Unknown => {
                let refusal = Refusal::not_found();
                record_floodable_wire(peer, service, refusal, now);
                return reply(peer, &id, Err(refusal));
            }
            MethodResolution::Malformed => {
                let refusal = Refusal::invalid_method();
                record_floodable_wire(peer, service, refusal, now);
                return reply(peer, &id, Err(refusal));
            }
        };
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
    ///
    /// A refusal that [`Refusal::closes_connection`] sets the retire flag: the
    /// refusal bytes stay queued, reading stops, and the server loop shuts the
    /// stream down only after `output` drains. The bytes are never dropped by
    /// closing first.
    fn reply(peer: &mut PeerIo, id: &str, result: Result<Value, Refusal>) -> bool {
        let closes = result
            .as_ref()
            .err()
            .is_some_and(|refusal| refusal.closes_connection());
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
                if closes {
                    peer.retire();
                }
                true
            }
            Err(_) => false,
        }
    }

    /// Record a non-closing wire refusal within the per-connection budget.
    fn record_floodable_wire(
        peer: &mut PeerIo,
        service: &mut Service,
        refusal: Refusal,
        now: MonotonicMs,
    ) {
        let budget = u32::try_from(
            service
                .limits()
                .effective(LimitId::RefusalAuditBudgetPerSec),
        )
        .unwrap_or(u32::MAX);
        if peer.wire_audit.admit(now.as_millis(), budget) {
            service.record_refusal(&peer.client, crate::audit::AuditAction::Wire, refusal, now);
        }
    }

    /// The envelope's correlation id and method name, when they are safe to
    /// echo and compare.
    ///
    /// This is the bounded pre-parse read the stage-2 negotiate gate needs: it
    /// extracts only the two strings the server must know before it validates
    /// the grammar or resolves the catalog.
    fn envelope_correlation(payload: &[u8]) -> Option<(String, String, Option<u16>)> {
        let value: Value = serde_json::from_slice(payload).ok()?;
        let id = value.get("id")?.as_str()?;
        if id.is_empty() || id.len() > bitty_ipc_api::wire::MAX_ID_BYTES {
            return None;
        }
        let method = value.get("method")?.as_str()?;
        let version = value
            .get("v")
            .and_then(Value::as_u64)
            .and_then(|version| u16::try_from(version).ok());
        Some((id.to_owned(), method.to_owned(), version))
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

    #[cfg(test)]
    mod tests {
        use super::{PeerTimer, peer_timer};

        #[test]
        fn a_retired_peer_has_a_bounded_drain_deadline_and_no_second_timeout() {
            // Retired, drain deadline not reached: keep waiting.
            assert_eq!(
                peer_timer(false, true, Some(1_000), 0, 0, 999, 5_000, 60_000, true),
                PeerTimer::Keep
            );
            // Retired past the deadline: close; never the handshake timeout.
            assert_eq!(
                peer_timer(false, true, Some(1_000), 0, 0, 1_000, 5_000, 60_000, true),
                PeerTimer::Close
            );
            // Unnegotiated before and past the deadline.
            assert_eq!(
                peer_timer(false, false, None, 0, 0, 4_999, 5_000, 60_000, false),
                PeerTimer::Keep
            );
            assert_eq!(
                peer_timer(false, false, None, 0, 0, 5_000, 5_000, 60_000, false),
                PeerTimer::Timeout
            );
            // Negotiated: idle only when the output queue is drained.
            assert_eq!(
                peer_timer(true, false, None, 0, 0, 60_000, 5_000, 60_000, true),
                PeerTimer::Keep
            );
            assert_eq!(
                peer_timer(true, false, None, 0, 0, 60_000, 5_000, 60_000, false),
                PeerTimer::Close
            );
        }
    }
}

#[cfg(all(unix, target_os = "linux"))]
pub use unix::{ServerConfig, ServerError, ServerOutcome, TAMPER_INTERVAL_MS, TICK_MS, run};

#[cfg(not(all(unix, target_os = "linux")))]
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

#[cfg(not(all(unix, target_os = "linux")))]
pub use unsupported::{ServerConfig, ServerError, ServerOutcome, TAMPER_INTERVAL_MS, TICK_MS, run};
