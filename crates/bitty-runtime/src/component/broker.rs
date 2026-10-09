//! Coprocess lifecycle and request multiplexing for native components.
//!
//! Threads per running component: one stdout reader (decodes frames into
//! the broker's shared bounded inbound queue), one stderr reader (fills the
//! bounded ring), and one stdin writer (drains a bounded outbound queue).
//! All policy state lives on the owner thread; the helper threads hold no
//! policy and exit when their pipe closes.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use bitty_network_wire::{
    CONNECTION_ID, DEFAULT_MAX_BODY_BYTES, ErrorKind, FrameReader, MAX_BODY_CHUNK_BYTES,
    MAX_REQUEST_BODY_BYTES, Message, Method, PROTOCOL_VERSION, WireError, encode_frame,
};

use super::descriptor::{ResolveError, validate_component_name};
use super::env::ComponentEnv;
use super::grant::PluginGrant;
use super::inventory::resolve_search;
use super::policy::{CrashTracker, DeadlineStrikes, SpawnGate};
use super::stderr::{StderrRing, stderr_log_tail};
use super::{
    COMPONENT_EXIT_RECHECK, COMPONENT_HANDSHAKE_TIMEOUT, COMPONENT_IDLE_TIMEOUT,
    COMPONENT_INBOUND_QUEUE_FRAMES, COMPONENT_MAX_BODY_BYTES_CEILING, COMPONENT_MAX_IN_FLIGHT,
    COMPONENT_OUTBOUND_QUEUE_BATCHES, COMPONENT_POLL_MAX_FRAMES, COMPONENT_REQUEST_DEADLINE_GRACE,
    COMPONENT_REQUEST_DEFAULT_TIMEOUT, COMPONENT_REQUEST_MAX_TIMEOUT, COMPONENT_SHUTDOWN_GRACE,
    COMPONENT_STDERR_MAX_BYTES, COMPONENT_STDERR_READ_CHUNK,
};

/// Broker-assigned request id (non-zero, unique for the broker lifetime).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(u64);

impl RequestId {
    /// Raw wire id.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Receives warn-level component diagnostics (DIR-030 D4).
///
/// Core links no logging crate, so the broker owner supplies the sink: on a
/// crash, a handshake failure, or an idle stop the broker hands the newest
/// bounded stderr tail (already escaped by [`stderr_log_tail`]) to
/// [`ComponentWarnSink::warn`]. Without a sink the tails are dropped. The
/// tail may be empty when the component wrote nothing to stderr.
pub trait ComponentWarnSink {
    /// Log one warn-level diagnostic for `component` (`reason` is a short
    /// static cause; `stderr_tail` is the escaped tail).
    fn warn(&mut self, component: &str, reason: &str, stderr_tail: &str);
}

/// Any `FnMut(&str, &str, &str)` works as a sink (tests capture into a
/// shared log this way).
impl<F> ComponentWarnSink for F
where
    F: FnMut(&str, &str, &str),
{
    fn warn(&mut self, component: &str, reason: &str, stderr_tail: &str) {
        self(component, reason, stderr_tail);
    }
}

/// Core timeout for one request (DIR-030 D2): the requested `timeout_ms`,
/// or [`COMPONENT_REQUEST_DEFAULT_TIMEOUT`] when none was requested,
/// clamped to [`COMPONENT_REQUEST_MAX_TIMEOUT`]. The result is forwarded
/// as the wire `timeout_ms`, so the component's own deadline fires first
/// and Core's grace-covered deadline only catches a silent component.
#[must_use]
pub fn effective_timeout(timeout_ms: u32) -> Duration {
    let base = if timeout_ms == 0 {
        COMPONENT_REQUEST_DEFAULT_TIMEOUT
    } else {
        Duration::from_millis(u64::from(timeout_ms))
    };
    base.min(COMPONENT_REQUEST_MAX_TIMEOUT)
}

/// Core response body budget for one request (DIR-030 D3): the requested
/// `max_body_bytes`, or the wire default (8 MiB) when none was requested,
/// clamped to [`COMPONENT_MAX_BODY_BYTES_CEILING`]. Core enforces the
/// result too; the component never widens it.
#[must_use]
pub fn effective_max_body_bytes(max_body_bytes: u64) -> u64 {
    if max_body_bytes == 0 {
        DEFAULT_MAX_BODY_BYTES
    } else {
        max_body_bytes.min(COMPONENT_MAX_BODY_BYTES_CEILING)
    }
}

/// Broker construction parameters.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// User component root (`components_root_for`); `None` means the user
    /// tier is absent (no-root execution still consults `system_root`).
    pub root: Option<PathBuf>,
    /// System component root (`system_components_root_for`); `None` disables
    /// the system tier. The user tier wins on collision.
    pub system_root: Option<PathBuf>,
    /// Allowlisted environment forwarded to every component.
    pub env: ComponentEnv,
    /// Core version announced in `Hello`.
    pub core_version: String,
    /// Idle time before stdin is closed.
    pub idle_timeout: Duration,
    /// Grace between stdin close and the kill.
    pub shutdown_grace: Duration,
    /// `HelloAck` deadline.
    pub handshake_timeout: Duration,
}

impl BrokerConfig {
    /// DIR-030 defaults for `root` and `env` (system tier disabled).
    #[must_use]
    pub fn new(root: Option<PathBuf>, env: ComponentEnv) -> Self {
        Self {
            root,
            system_root: None,
            env,
            core_version: env!("CARGO_PKG_VERSION").to_owned(),
            idle_timeout: COMPONENT_IDLE_TIMEOUT,
            shutdown_grace: COMPONENT_SHUTDOWN_GRACE,
            handshake_timeout: COMPONENT_HANDSHAKE_TIMEOUT,
        }
    }

    /// Enable the system tier for two-tier (issue #1651) resolution.
    #[must_use]
    pub fn with_system_root(mut self, system_root: Option<PathBuf>) -> Self {
        self.system_root = system_root;
        self
    }
}

/// One HTTP request for a component, attributed to a plugin.
#[derive(Debug, Clone)]
pub struct ComponentRequest {
    /// Request method.
    pub method: Method,
    /// Absolute URL.
    pub url: String,
    /// Request headers.
    pub headers: Vec<(String, String)>,
    /// Requested deadline in milliseconds (`0` = Core default). The broker
    /// forwards [`effective_timeout`] as the wire value and expires the
    /// request past it plus the deadline grace.
    pub timeout_ms: u32,
    /// Requested response body cap (`0` = wire default, 8 MiB). The broker
    /// forwards [`effective_max_body_bytes`] as the wire value and Core
    /// enforces it too.
    pub max_body_bytes: u64,
    /// Optional request body (at most 8 MiB), chunked by the broker.
    pub body: Option<Vec<u8>>,
}

impl ComponentRequest {
    /// `method url` with no headers, defaults, and no body.
    #[must_use]
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            timeout_ms: 0,
            max_body_bytes: 0,
            body: None,
        }
    }
}

/// Synchronous submit failure (nothing was sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerError {
    /// Resolution or verification failed (fail closed, nothing spawned).
    Resolve(ResolveError),
    /// The component is in crash backoff.
    Backoff {
        /// Remaining delay.
        retry_after: Duration,
    },
    /// The component crashed too often and is unavailable until restart.
    Unavailable,
    /// [`COMPONENT_MAX_IN_FLIGHT`] requests are already pending, or the
    /// outbound queue is full.
    Busy,
    /// The request does not encode within the wire bounds.
    InvalidRequest(WireError),
    /// The request body exceeds the wire bound.
    BodyTooLarge,
    /// The executable could not be started.
    Spawn(std::io::ErrorKind),
}

impl fmt::Display for BrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrokerError::Resolve(error) => write!(f, "component unavailable: {error}"),
            BrokerError::Backoff { retry_after } => {
                write!(
                    f,
                    "component restarting; retry after {} ms",
                    retry_after.as_millis()
                )
            }
            BrokerError::Unavailable => f.write_str("component unavailable after repeated crashes"),
            BrokerError::Busy => write!(
                f,
                "component busy ({COMPONENT_MAX_IN_FLIGHT} requests in flight)"
            ),
            BrokerError::InvalidRequest(error) => write!(f, "invalid request: {error}"),
            BrokerError::BodyTooLarge => {
                write!(f, "request body exceeds {MAX_REQUEST_BODY_BYTES} bytes")
            }
            BrokerError::Spawn(kind) => write!(f, "component spawn failed: {kind}"),
        }
    }
}

impl std::error::Error for BrokerError {}

impl From<ResolveError> for BrokerError {
    fn from(error: ResolveError) -> Self {
        BrokerError::Resolve(error)
    }
}

/// Response progress for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerEventKind {
    /// Status and headers.
    Head {
        /// HTTP status.
        status: u16,
        /// Response headers.
        headers: Vec<(String, String)>,
    },
    /// One body chunk; `last` ends the request.
    Body {
        /// Chunk bytes.
        data: Vec<u8>,
        /// Final chunk.
        last: bool,
    },
    /// Terminal failure (from the component, or `component_lost` /
    /// `budget` / `protocol` / `timeout` produced by Core).
    Failed {
        /// Error category.
        kind: ErrorKind,
        /// Redacted detail.
        message: String,
    },
}

/// One response event, attributed to the requesting plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerEvent {
    /// Request id from [`ComponentBroker::submit`].
    pub id: RequestId,
    /// Requesting plugin.
    pub plugin_id: String,
    /// Component that served it.
    pub component: String,
    /// What happened.
    pub kind: BrokerEventKind,
}

impl BrokerEvent {
    /// Whether this event ends its request.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            BrokerEventKind::Body { last: true, .. } | BrokerEventKind::Failed { .. }
        )
    }
}

/// Coarse component state for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentState {
    /// Not running (never started, idle-stopped, or waiting out a backoff).
    Stopped,
    /// Spawned; waiting for `HelloAck`.
    Handshaking,
    /// Handshake complete.
    Running,
    /// Stdin closed after idle; waiting for exit within the grace.
    Stopping,
    /// Latched unavailable after repeated crashes.
    Unavailable,
}

/// How the last process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Exited on its own after stdin close.
    Exited,
    /// Killed (recorded PID only) after the grace expired.
    Killed,
    /// Counted as a crash.
    Crashed,
}

/// Diagnostic snapshot of one component slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentStatus {
    /// Current state.
    pub state: ComponentState,
    /// PID of the running child, if any.
    pub pid: Option<u32>,
    /// Requests in flight (including those queued behind the handshake).
    pub in_flight: usize,
    /// Processes spawned so far.
    pub spawns: u64,
    /// How the last process ended.
    pub last_stop: Option<StopOutcome>,
    /// Negotiated protocol and component version once running.
    pub negotiated: Option<(u16, String)>,
}

struct InFlight {
    plugin_id: String,
    max_body_bytes: u64,
    received_body: u64,
    head_seen: bool,
    /// Core deadline: submit time plus [`effective_timeout`] plus
    /// [`COMPONENT_REQUEST_DEADLINE_GRACE`].
    deadline: Instant,
}

enum Handshake {
    Pending {
        deadline: Instant,
        queued: Vec<(RequestId, Vec<Vec<u8>>)>,
    },
    Ready {
        protocol: u16,
        version: String,
    },
}

struct Process {
    generation: u64,
    child: Child,
    stdin: Option<SyncSender<Vec<Vec<u8>>>>,
    handshake: Handshake,
    in_flight: BTreeMap<RequestId, InFlight>,
    last_activity: Instant,
    stop_deadline: Option<Instant>,
}

struct Slot {
    name: String,
    tracker: CrashTracker,
    deadline_strikes: DeadlineStrikes,
    process: Option<Process>,
    stderr: Arc<Mutex<StderrRing>>,
    spawns: u64,
    last_stop: Option<StopOutcome>,
}

enum InboundEvent {
    Frame(Message),
    Failed,
    Closed,
}

struct Inbound {
    component: String,
    generation: u64,
    event: InboundEvent,
}

/// The native component broker (one per Bitty instance).
pub struct ComponentBroker {
    config: BrokerConfig,
    slots: BTreeMap<String, Slot>,
    inbound_tx: SyncSender<Inbound>,
    inbound_rx: Receiver<Inbound>,
    warn_sink: Option<Box<dyn ComponentWarnSink>>,
    next_request: u64,
    next_generation: u64,
}

impl fmt::Debug for ComponentBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComponentBroker")
            .field("root", &self.config.root)
            .field("components", &self.slots.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ComponentBroker {
    /// Broker with no running component.
    #[must_use]
    pub fn new(config: BrokerConfig) -> Self {
        let (inbound_tx, inbound_rx) = sync_channel(COMPONENT_INBOUND_QUEUE_FRAMES);
        Self {
            config,
            slots: BTreeMap::new(),
            inbound_tx,
            inbound_rx,
            warn_sink: None,
            next_request: 0,
            next_generation: 0,
        }
    }

    /// Install the warn sink for D4 stderr tails (none by default; without
    /// one the tails are dropped and only [`Self::stderr_tail`] exposes
    /// them).
    #[must_use]
    pub fn with_warn_sink(mut self, sink: impl ComponentWarnSink + 'static) -> Self {
        self.warn_sink = Some(Box::new(sink));
        self
    }

    /// Submit `request` to `component` on behalf of `grant`'s plugin.
    ///
    /// Spawns the component on first use (after full resolution and digest
    /// verification). Never blocks on the component: before the handshake
    /// completes, the request is queued behind it. Responses arrive as
    /// [`BrokerEvent`]s.
    pub fn submit(
        &mut self,
        now: Instant,
        component: &str,
        grant: &PluginGrant,
        request: ComponentRequest,
    ) -> Result<RequestId, BrokerError> {
        validate_component_name(component)
            .map_err(|_| BrokerError::Resolve(ResolveError::InvalidName))?;
        let id = self.peek_request_id();
        // DIR-030 D2/D3: every request carries the effective timeout and
        // budget as its wire values; the Core deadline adds the grace.
        let timeout = effective_timeout(request.timeout_ms);
        let max_body_bytes = effective_max_body_bytes(request.max_body_bytes);
        // The effective timeout always fits `u32` (clamped to 300 s);
        // saturate defensively so the wire value can never wrap.
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let mut request = request;
        request.timeout_ms = timeout_ms;
        request.max_body_bytes = max_body_bytes;
        let frames = encode_request(id, grant, request)?;

        self.ensure_running(now, component)?;
        let Some(slot) = self.slots.get_mut(component) else {
            return Err(BrokerError::Unavailable);
        };
        let Some(process) = slot.process.as_mut() else {
            return Err(BrokerError::Unavailable);
        };
        let pending = process.in_flight.len();
        if pending >= COMPONENT_MAX_IN_FLIGHT {
            return Err(BrokerError::Busy);
        }
        match &mut process.handshake {
            Handshake::Pending { queued, .. } => queued.push((id, frames)),
            Handshake::Ready { .. } => {
                let Some(stdin) = process.stdin.as_ref() else {
                    return Err(BrokerError::Unavailable);
                };
                match stdin.try_send(frames) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => return Err(BrokerError::Busy),
                    Err(TrySendError::Disconnected(_)) => return Err(BrokerError::Unavailable),
                }
            }
        }
        process.in_flight.insert(
            id,
            InFlight {
                plugin_id: grant.plugin_id().to_owned(),
                max_body_bytes,
                received_body: 0,
                head_seen: false,
                deadline: now + timeout + COMPONENT_REQUEST_DEADLINE_GRACE,
            },
        );
        process.last_activity = now;
        self.next_request = id.0;
        Ok(id)
    }

    /// Abandon request `id`. No further event is reported for it; a late
    /// frame from the component is dropped. Unknown ids are ignored.
    pub fn cancel(&mut self, id: RequestId) {
        for slot in self.slots.values_mut() {
            let Some(process) = slot.process.as_mut() else {
                continue;
            };
            if process.in_flight.remove(&id).is_none() {
                continue;
            }
            match &mut process.handshake {
                Handshake::Pending { queued, .. } => {
                    queued.retain(|(queued_id, _)| *queued_id != id)
                }
                Handshake::Ready { .. } => {
                    if let (Some(stdin), Ok(frame)) = (
                        process.stdin.as_ref(),
                        encode_frame(&Message::Cancel { id: id.0 }),
                    ) {
                        // A full queue drops the cancel; the broker already
                        // forgot the id, so late frames are discarded.
                        let _ = stdin.try_send(vec![frame]);
                    }
                }
            }
            return;
        }
    }

    /// Process pending component output and run lifecycle policy at `now`
    /// (handshake deadline, idle stop, stop grace). Never blocks; handles at
    /// most [`COMPONENT_POLL_MAX_FRAMES`] frames per call.
    pub fn poll(&mut self, now: Instant) -> Vec<BrokerEvent> {
        let mut events = Vec::new();
        for _ in 0..COMPONENT_POLL_MAX_FRAMES {
            match self.inbound_rx.try_recv() {
                Ok(inbound) => self.handle_inbound(now, inbound, &mut events),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        self.maintain(now, &mut events);
        events
    }

    /// Like [`Self::poll`], but first blocks up to `timeout` for component
    /// output. `now` drives policy; `timeout` is real time.
    pub fn wait(&mut self, now: Instant, timeout: Duration) -> Vec<BrokerEvent> {
        let mut events = Vec::new();
        match self.inbound_rx.recv_timeout(timeout) {
            Ok(inbound) => self.handle_inbound(now, inbound, &mut events),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
        events.extend(self.poll(now));
        events
    }

    /// Graceful stop of every component: close stdin, wait up to the grace
    /// for exit, then kill the recorded child. In-flight requests fail with
    /// `component_lost`.
    pub fn shutdown(&mut self) -> Vec<BrokerEvent> {
        let mut events = Vec::new();
        let names: Vec<String> = self.slots.keys().cloned().collect();
        for name in &names {
            if let Some(slot) = self.slots.get_mut(name) {
                if let Some(process) = slot.process.as_mut() {
                    fail_all(
                        &slot.name,
                        process,
                        ErrorKind::ComponentLost,
                        "core shutdown",
                        &mut events,
                    );
                    process.stdin = None;
                }
            }
        }
        let deadline = Instant::now() + self.config.shutdown_grace;
        // Phase 1 (event-driven): a child exit closes its stdout, which the
        // reader thread reports as `Closed`.
        let mut open: Vec<(String, u64)> = self
            .slots
            .iter()
            .filter_map(|(name, slot)| Some((name.clone(), slot.process.as_ref()?.generation)))
            .collect();
        while !open.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.inbound_rx.recv_timeout(remaining) {
                Ok(Inbound {
                    component,
                    generation,
                    event: InboundEvent::Closed | InboundEvent::Failed,
                }) => open.retain(|entry| *entry != (component.clone(), generation)),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        // Phase 2: reap exited children; stdout may close a moment before
        // the exit status is observable, so re-check in short slices until
        // the same deadline, then kill whatever is left (recorded PIDs only).
        loop {
            let mut running = 0usize;
            for slot in self.slots.values_mut() {
                let Some(process) = slot.process.as_mut() else {
                    continue;
                };
                match process.child.try_wait() {
                    Ok(Some(_)) | Err(_) => reap(slot, StopOutcome::Exited),
                    Ok(None) => running += 1,
                }
            }
            if running == 0 || Instant::now() >= deadline {
                break;
            }
            thread::sleep(
                COMPONENT_EXIT_RECHECK.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        for slot in self.slots.values_mut() {
            if slot.process.is_some() {
                kill_and_reap(slot, StopOutcome::Killed);
            }
        }
        events
    }

    /// Diagnostic snapshot for `component`, if it was ever used.
    #[must_use]
    pub fn status(&self, component: &str) -> Option<ComponentStatus> {
        let slot = self.slots.get(component)?;
        let (state, pid, in_flight, negotiated) = match &slot.process {
            Some(process) => {
                let state = if process.stop_deadline.is_some() {
                    ComponentState::Stopping
                } else if matches!(process.handshake, Handshake::Pending { .. }) {
                    ComponentState::Handshaking
                } else {
                    ComponentState::Running
                };
                let negotiated = match &process.handshake {
                    Handshake::Ready { protocol, version } => Some((*protocol, version.clone())),
                    Handshake::Pending { .. } => None,
                };
                (
                    state,
                    Some(process.child.id()),
                    process.in_flight.len(),
                    negotiated,
                )
            }
            None if slot.tracker.is_unavailable() => (ComponentState::Unavailable, None, 0, None),
            None => (ComponentState::Stopped, None, 0, None),
        };
        Some(ComponentStatus {
            state,
            pid,
            in_flight,
            spawns: slot.spawns,
            last_stop: slot.last_stop,
            negotiated,
        })
    }

    /// Newest bytes the component wrote to stderr (bounded ring).
    #[must_use]
    pub fn stderr_tail(&self, component: &str) -> Option<Vec<u8>> {
        let slot = self.slots.get(component)?;
        let ring = slot.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        Some(ring.snapshot())
    }

    fn peek_request_id(&self) -> RequestId {
        // Wrap-around is unreachable in practice (u64), and 0 is reserved.
        RequestId(self.next_request.checked_add(1).unwrap_or(1))
    }

    fn ensure_running(&mut self, now: Instant, component: &str) -> Result<(), BrokerError> {
        let slot = self
            .slots
            .entry(component.to_owned())
            .or_insert_with(|| Slot {
                name: component.to_owned(),
                tracker: CrashTracker::new(),
                deadline_strikes: DeadlineStrikes::new(),
                process: None,
                stderr: Arc::new(Mutex::new(StderrRing::new(COMPONENT_STDERR_MAX_BYTES))),
                spawns: 0,
                last_stop: None,
            });
        if let Some(process) = slot.process.as_ref() {
            if process.stop_deadline.is_none() {
                return Ok(());
            }
            // Stopping after idle with nothing in flight: finish it now
            // rather than run two processes for one component.
            kill_and_reap(slot, StopOutcome::Killed);
        }
        match slot.tracker.gate(now) {
            SpawnGate::Ready => {}
            SpawnGate::Backoff { retry_after } => return Err(BrokerError::Backoff { retry_after }),
            SpawnGate::Unavailable => return Err(BrokerError::Unavailable),
        }
        // Two-tier resolution (issue #1651): user wins on collision; a
        // tampered user install fails closed without system fallback.
        // Full resolution and digest verification before every spawn.
        let searched = resolve_search(
            self.config.root.as_deref(),
            self.config.system_root.as_deref(),
            component,
        )?;
        let resolved = searched.resolved;
        self.next_generation += 1;
        let generation = self.next_generation;
        let process = spawn(
            &resolved.executable_path,
            &resolved.version_dir,
            &self.config,
            component,
            generation,
            now,
            &self.inbound_tx,
            &slot.stderr,
        )?;
        slot.process = Some(process);
        slot.spawns += 1;
        Ok(())
    }

    fn handle_inbound(&mut self, now: Instant, inbound: Inbound, events: &mut Vec<BrokerEvent>) {
        let Some(slot) = self.slots.get_mut(&inbound.component) else {
            return;
        };
        let current = slot
            .process
            .as_ref()
            .is_some_and(|process| process.generation == inbound.generation);
        if !current {
            return; // a previous process of this component; ignore
        }
        match inbound.event {
            InboundEvent::Frame(message) => {
                if let Err(reason) = handle_frame(slot, now, message, events) {
                    crash(slot, now, reason, events, &mut self.warn_sink);
                }
            }
            InboundEvent::Failed => {
                crash(slot, now, "undecodable frame", events, &mut self.warn_sink)
            }
            InboundEvent::Closed => {
                let stopping = slot
                    .process
                    .as_ref()
                    .is_some_and(|process| process.stop_deadline.is_some());
                if stopping {
                    let exited = slot
                        .process
                        .as_mut()
                        .is_some_and(|process| matches!(process.child.try_wait(), Ok(Some(_))));
                    if exited {
                        reap(slot, StopOutcome::Exited);
                    }
                    // Otherwise the grace deadline in `maintain` decides.
                } else {
                    crash(
                        slot,
                        now,
                        "component closed its output",
                        events,
                        &mut self.warn_sink,
                    );
                }
            }
        }
    }

    fn maintain(&mut self, now: Instant, events: &mut Vec<BrokerEvent>) {
        let idle_timeout = self.config.idle_timeout;
        let grace = self.config.shutdown_grace;
        for slot in self.slots.values_mut() {
            let stopping = slot
                .process
                .as_ref()
                .is_some_and(|process| process.stop_deadline.is_some());
            if stopping {
                let Some(process) = slot.process.as_mut() else {
                    continue;
                };
                if let Some(deadline) = process.stop_deadline {
                    match process.child.try_wait() {
                        Ok(Some(_)) => reap(slot, StopOutcome::Exited),
                        _ if now >= deadline => kill_and_reap(slot, StopOutcome::Killed),
                        _ => {}
                    }
                }
                continue;
            }
            // Core request deadlines (DIR-030 D2): expiries fail with
            // `timeout`; enough consecutive ones take the crash path.
            if sweep_deadlines(slot, now, events) {
                crash(
                    slot,
                    now,
                    "core request deadlines expiring repeatedly",
                    events,
                    &mut self.warn_sink,
                );
                continue;
            }
            let Some(process) = slot.process.as_mut() else {
                continue;
            };
            if let Handshake::Pending { deadline, .. } = process.handshake {
                if now >= deadline {
                    crash(slot, now, "handshake timeout", events, &mut self.warn_sink);
                }
                continue;
            }
            if matches!(process.child.try_wait(), Ok(Some(_))) {
                crash(slot, now, "component exited", events, &mut self.warn_sink);
                continue;
            }
            if process.in_flight.is_empty()
                && now.saturating_duration_since(process.last_activity) >= idle_timeout
            {
                // Idle stop: close stdin; the component exits on EOF.
                process.stdin = None;
                process.stop_deadline = Some(now + grace);
                // DIR-030 D4: idle stops log the stderr tail too.
                emit_warn(&mut self.warn_sink, slot, "idle stop");
            }
        }
    }
}

impl Drop for ComponentBroker {
    /// Abort path: kill every recorded child without a grace. Call
    /// [`ComponentBroker::shutdown`] first for a graceful stop.
    fn drop(&mut self) {
        for slot in self.slots.values_mut() {
            if slot.process.is_some() {
                kill_and_reap(slot, StopOutcome::Killed);
            }
        }
    }
}

/// Encode the request frames up front so nothing partial is ever sent.
fn encode_request(
    id: RequestId,
    grant: &PluginGrant,
    request: ComponentRequest,
) -> Result<Vec<Vec<u8>>, BrokerError> {
    let body = request.body.unwrap_or_default();
    if body.len() as u64 > MAX_REQUEST_BODY_BYTES {
        return Err(BrokerError::BodyTooLarge);
    }
    let mut frames = Vec::with_capacity(1 + body.len().div_ceil(MAX_BODY_CHUNK_BYTES));
    frames.push(
        encode_frame(&Message::HttpRequest {
            id: id.0,
            plugin_id: grant.plugin_id().to_owned(),
            grant: grant.grant().clone(),
            method: request.method,
            url: request.url,
            headers: request.headers,
            timeout_ms: request.timeout_ms,
            max_body_bytes: request.max_body_bytes,
            body_follows: !body.is_empty(),
        })
        .map_err(BrokerError::InvalidRequest)?,
    );
    let chunks: Vec<&[u8]> = body.chunks(MAX_BODY_CHUNK_BYTES).collect();
    let count = chunks.len();
    for (index, chunk) in chunks.into_iter().enumerate() {
        frames.push(
            encode_frame(&Message::RequestBody {
                id: id.0,
                data: chunk.to_vec(),
                last: index + 1 == count,
            })
            .map_err(BrokerError::InvalidRequest)?,
        );
    }
    Ok(frames)
}

#[allow(clippy::too_many_arguments)]
fn spawn(
    executable: &Path,
    version_dir: &Path,
    config: &BrokerConfig,
    component: &str,
    generation: u64,
    now: Instant,
    inbound: &SyncSender<Inbound>,
    stderr_ring: &Arc<Mutex<StderrRing>>,
) -> Result<Process, BrokerError> {
    let hello = encode_frame(&Message::Hello {
        min: PROTOCOL_VERSION,
        max: PROTOCOL_VERSION,
        component: component.to_owned(),
        version: config.core_version.clone(),
    })
    .map_err(BrokerError::InvalidRequest)?;

    let mut command = Command::new(executable);
    command
        .env_clear()
        .current_dir(version_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in config.env.vars() {
        command.env(name, value);
    }
    let mut child = command
        .spawn()
        .map_err(|error| BrokerError::Spawn(error.kind()))?;

    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(BrokerError::Spawn(std::io::ErrorKind::BrokenPipe));
    };

    let (stdin_tx, stdin_rx) = sync_channel::<Vec<Vec<u8>>>(COMPONENT_OUTBOUND_QUEUE_BATCHES);
    let started = spawn_writer(stdin, stdin_rx)
        .and_then(|()| spawn_reader(stdout, component.to_owned(), generation, inbound.clone()))
        .and_then(|()| spawn_stderr(stderr, Arc::clone(stderr_ring)));
    if let Err(error) = started {
        let _ = child.kill();
        let _ = child.wait();
        return Err(BrokerError::Spawn(error.kind()));
    }
    if stdin_tx.try_send(vec![hello]).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(BrokerError::Spawn(std::io::ErrorKind::BrokenPipe));
    }
    Ok(Process {
        generation,
        child,
        stdin: Some(stdin_tx),
        handshake: Handshake::Pending {
            deadline: now + config.handshake_timeout,
            queued: Vec::new(),
        },
        in_flight: BTreeMap::new(),
        last_activity: now,
        stop_deadline: None,
    })
}

fn spawn_writer(
    mut stdin: std::process::ChildStdin,
    rx: Receiver<Vec<Vec<u8>>>,
) -> std::io::Result<()> {
    thread::Builder::new()
        .name("bitty-component-stdin".into())
        .spawn(move || {
            // Ends when the broker drops the sender (stdin close) or the
            // pipe breaks; dropping `stdin` delivers EOF to the component.
            while let Ok(batch) = rx.recv() {
                for frame in &batch {
                    if stdin.write_all(frame).is_err() {
                        return;
                    }
                }
                if stdin.flush().is_err() {
                    return;
                }
            }
        })
        .map(|_| ())
}

fn spawn_reader(
    stdout: ChildStdout,
    component: String,
    generation: u64,
    tx: SyncSender<Inbound>,
) -> std::io::Result<()> {
    thread::Builder::new()
        .name("bitty-component-stdout".into())
        .spawn(move || {
            let mut reader = FrameReader::new(stdout);
            loop {
                let event = match reader.read_message() {
                    Ok(Some(message)) => InboundEvent::Frame(message),
                    Ok(None) => InboundEvent::Closed,
                    Err(_) => InboundEvent::Failed,
                };
                let terminal = !matches!(event, InboundEvent::Frame(_));
                let sent = tx.send(Inbound {
                    component: component.clone(),
                    generation,
                    event,
                });
                if sent.is_err() || terminal {
                    return;
                }
            }
        })
        .map(|_| ())
}

fn spawn_stderr(mut stderr: ChildStderr, ring: Arc<Mutex<StderrRing>>) -> std::io::Result<()> {
    thread::Builder::new()
        .name("bitty-component-stderr".into())
        .spawn(move || {
            let mut buf = vec![0u8; COMPONENT_STDERR_READ_CHUNK];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => ring
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(&buf[..n]),
                }
            }
        })
        .map(|_| ())
}

/// Apply one component frame. `Err` is a protocol violation (crash).
fn handle_frame(
    slot: &mut Slot,
    now: Instant,
    message: Message,
    events: &mut Vec<BrokerEvent>,
) -> Result<(), &'static str> {
    let component = slot.name.clone();
    let Some(process) = slot.process.as_mut() else {
        return Ok(());
    };
    process.last_activity = now;
    if let Handshake::Pending { queued, .. } = &mut process.handshake {
        let Message::HelloAck {
            protocol,
            component: acked,
            version,
        } = message
        else {
            return Err("first frame was not HelloAck");
        };
        if protocol != PROTOCOL_VERSION || acked != component {
            return Err("handshake mismatch");
        }
        let queued = std::mem::take(queued);
        process.handshake = Handshake::Ready { protocol, version };
        let Some(stdin) = process.stdin.as_ref() else {
            return Err("stdin closed during handshake");
        };
        for (_, frames) in queued {
            if stdin.try_send(frames).is_err() {
                return Err("outbound queue full after handshake");
            }
        }
        return Ok(());
    }
    match message {
        Message::ResponseHead {
            id,
            status,
            headers,
        } => {
            let id = RequestId(id);
            let Some(entry) = process.in_flight.get_mut(&id) else {
                return Ok(()); // cancelled or finished
            };
            if entry.head_seen {
                return Err("duplicate ResponseHead");
            }
            entry.head_seen = true;
            events.push(BrokerEvent {
                id,
                plugin_id: entry.plugin_id.clone(),
                component,
                kind: BrokerEventKind::Head { status, headers },
            });
        }
        Message::ResponseBody { id, data, last } => {
            let id = RequestId(id);
            let Some(entry) = process.in_flight.get_mut(&id) else {
                return Ok(());
            };
            if !entry.head_seen {
                return Err("ResponseBody before ResponseHead");
            }
            entry.received_body = entry.received_body.saturating_add(data.len() as u64);
            if entry.received_body > entry.max_body_bytes {
                // Core enforces the budget too; the component never widens it.
                let plugin_id = entry.plugin_id.clone();
                process.in_flight.remove(&id);
                if let (Some(stdin), Ok(frame)) = (
                    process.stdin.as_ref(),
                    encode_frame(&Message::Cancel { id: id.0 }),
                ) {
                    let _ = stdin.try_send(vec![frame]);
                }
                events.push(BrokerEvent {
                    id,
                    plugin_id,
                    component,
                    kind: BrokerEventKind::Failed {
                        kind: ErrorKind::Budget,
                        message: "response body exceeds the request budget".into(),
                    },
                });
                return Ok(());
            }
            let plugin_id = entry.plugin_id.clone();
            if last {
                process.in_flight.remove(&id);
                // A component-produced terminal frame clears the deadline
                // strike count (DIR-030 D2).
                slot.deadline_strikes.reset();
            }
            events.push(BrokerEvent {
                id,
                plugin_id,
                component,
                kind: BrokerEventKind::Body { data, last },
            });
        }
        Message::Error { id, kind, message } => {
            if id == CONNECTION_ID {
                return Err("connection-level component error");
            }
            let id = RequestId(id);
            let Some(entry) = process.in_flight.remove(&id) else {
                return Ok(());
            };
            // A component-produced terminal frame clears the deadline
            // strike count (DIR-030 D2). Core-produced terminal failures
            // (budget, timeout) do not: only the component clearing a
            // request proves it responsive.
            slot.deadline_strikes.reset();
            events.push(BrokerEvent {
                id,
                plugin_id: entry.plugin_id,
                component,
                kind: BrokerEventKind::Failed { kind, message },
            });
        }
        // Core-to-component messages, or a second HelloAck.
        _ => return Err("unexpected message direction"),
    }
    Ok(())
}

/// Every pending request of `process` fails with `kind`.
fn fail_all(
    component: &str,
    process: &mut Process,
    kind: ErrorKind,
    message: &str,
    events: &mut Vec<BrokerEvent>,
) {
    if let Handshake::Pending { queued, .. } = &mut process.handshake {
        queued.clear();
    }
    for (id, entry) in std::mem::take(&mut process.in_flight) {
        events.push(BrokerEvent {
            id,
            plugin_id: entry.plugin_id,
            component: component.to_owned(),
            kind: BrokerEventKind::Failed {
                kind,
                message: message.to_owned(),
            },
        });
    }
}

/// Expire requests past their Core deadline (DIR-030 D2).
///
/// Each expired request fails with `timeout`; `Cancel` is sent only when
/// the request had reached the component (a handshake-queued request is
/// just dropped from the queue). A late component frame for an expired id
/// finds no booking and is discarded, never a protocol error. Returns
/// whether [`super::COMPONENT_DEADLINE_CRASH_THRESHOLD`] consecutive
/// expiries tripped: the caller then takes the crash path, which fails
/// whatever is left with `component_lost`.
fn sweep_deadlines(slot: &mut Slot, now: Instant, events: &mut Vec<BrokerEvent>) -> bool {
    let Some(process) = slot.process.as_mut() else {
        return false;
    };
    let expired: Vec<RequestId> = process
        .in_flight
        .iter()
        .filter(|(_, entry)| now >= entry.deadline)
        .map(|(id, _)| *id)
        .collect();
    if expired.is_empty() {
        return false;
    }
    let ready = matches!(process.handshake, Handshake::Ready { .. });
    let mut tripped = false;
    for id in expired {
        let Some(entry) = process.in_flight.remove(&id) else {
            continue;
        };
        if ready {
            if let (Some(stdin), Ok(frame)) = (
                process.stdin.as_ref(),
                encode_frame(&Message::Cancel { id: id.0 }),
            ) {
                // A full queue drops the cancel; the id is already
                // forgotten, so late frames are discarded.
                let _ = stdin.try_send(vec![frame]);
            }
        } else if let Handshake::Pending { queued, .. } = &mut process.handshake {
            queued.retain(|(queued_id, _)| *queued_id != id);
        }
        events.push(BrokerEvent {
            id,
            plugin_id: entry.plugin_id,
            component: slot.name.clone(),
            kind: BrokerEventKind::Failed {
                kind: ErrorKind::Timeout,
                message: "core request deadline expired".into(),
            },
        });
        if slot.deadline_strikes.expire() {
            tripped = true;
        }
    }
    tripped
}

/// Hand the newest bounded stderr tail to the warn sink, if any (DIR-030
/// D4). The tail is escaped by [`stderr_log_tail`]; it may be empty when
/// the component wrote nothing.
fn emit_warn(sink: &mut Option<Box<dyn ComponentWarnSink>>, slot: &Slot, reason: &str) {
    let Some(sink) = sink.as_mut() else {
        return;
    };
    let ring = slot.stderr.lock().unwrap_or_else(PoisonError::into_inner);
    sink.warn(&slot.name, reason, &stderr_log_tail(&ring.snapshot()));
}

/// Crash: fail in-flight with `component_lost`, kill the recorded child,
/// and start the backoff (or latch unavailable). The stderr tail goes to
/// the warn sink (DIR-030 D4).
fn crash(
    slot: &mut Slot,
    now: Instant,
    reason: &str,
    events: &mut Vec<BrokerEvent>,
    warn_sink: &mut Option<Box<dyn ComponentWarnSink>>,
) {
    let name = slot.name.clone();
    if let Some(process) = slot.process.as_mut() {
        fail_all(
            &name,
            process,
            ErrorKind::ComponentLost,
            &format!("component lost: {reason}"),
            events,
        );
    }
    kill_and_reap(slot, StopOutcome::Crashed);
    let _ = slot.tracker.record_crash(now);
    emit_warn(warn_sink, slot, reason);
}

/// Forget an already-exited child (reaps it).
fn reap(slot: &mut Slot, outcome: StopOutcome) {
    if let Some(mut process) = slot.process.take() {
        process.stdin = None;
        let _ = process.child.wait();
        slot.last_stop = Some(outcome);
    }
}

/// Kill only the child recorded at spawn, then reap it.
fn kill_and_reap(slot: &mut Slot, outcome: StopOutcome) {
    if let Some(mut process) = slot.process.take() {
        process.stdin = None;
        if matches!(process.child.try_wait(), Ok(None)) {
            let _ = process.child.kill();
        }
        let _ = process.child.wait();
        slot.last_stop = Some(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_network_wire::{FRAME_HEADER_BYTES, decode};

    fn offline_grant() -> PluginGrant {
        PluginGrant::compute(
            "acme.test",
            std::iter::empty::<&bitty_plugin_host::capability::CapabilityId>(),
            &[],
        )
        .expect("grant")
    }

    #[test]
    fn request_body_is_chunked_with_last_flag() {
        let mut request = ComponentRequest::new(Method::Post, "https://api.example.com/");
        request.body = Some(vec![7u8; MAX_BODY_CHUNK_BYTES * 2 + 1]);
        let frames = encode_request(RequestId(9), &offline_grant(), request).expect("encode");
        assert_eq!(frames.len(), 4);
        let messages: Vec<Message> = frames
            .iter()
            .map(|frame| decode(&frame[FRAME_HEADER_BYTES..]).expect("decode"))
            .collect();
        match &messages[0] {
            Message::HttpRequest {
                id,
                plugin_id,
                body_follows,
                ..
            } => {
                assert_eq!(*id, 9);
                assert_eq!(plugin_id, "acme.test");
                assert!(*body_follows);
            }
            other => panic!("unexpected {other:?}"),
        }
        let lasts: Vec<bool> = messages[1..]
            .iter()
            .map(|message| match message {
                Message::RequestBody { last, .. } => *last,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(lasts, [false, false, true]);
    }

    #[test]
    fn request_without_body_is_one_frame() {
        let frames = encode_request(
            RequestId(1),
            &offline_grant(),
            ComponentRequest::new(Method::Get, "https://api.example.com/"),
        )
        .expect("encode");
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn oversize_body_and_invalid_url_fail_before_sending() {
        let mut request = ComponentRequest::new(Method::Post, "https://api.example.com/");
        request.body = Some(vec![
            0u8;
            usize::try_from(MAX_REQUEST_BODY_BYTES).expect("fits")
                + 1
        ]);
        assert_eq!(
            encode_request(RequestId(1), &offline_grant(), request).err(),
            Some(BrokerError::BodyTooLarge)
        );
        let long = ComponentRequest::new(
            Method::Get,
            "x".repeat(bitty_network_wire::MAX_URL_BYTES + 1),
        );
        assert!(matches!(
            encode_request(RequestId(1), &offline_grant(), long),
            Err(BrokerError::InvalidRequest(_))
        ));
    }
    #[test]
    fn invalid_component_name_is_rejected_without_spawning() {
        let mut broker = ComponentBroker::new(BrokerConfig::new(None, ComponentEnv::empty()));
        let result = broker.submit(
            Instant::now(),
            "../net",
            &offline_grant(),
            ComponentRequest::new(Method::Get, "https://api.example.com/"),
        );
        assert_eq!(result, Err(BrokerError::Resolve(ResolveError::InvalidName)));
        assert!(broker.status("../net").is_none());
    }

    #[test]
    fn effective_timeout_defaults_and_clamps_to_300s() {
        assert_eq!(effective_timeout(0), COMPONENT_REQUEST_DEFAULT_TIMEOUT);
        assert_eq!(effective_timeout(0), Duration::from_secs(30));
        assert_eq!(effective_timeout(1), Duration::from_millis(1));
        assert_eq!(effective_timeout(30_000), Duration::from_secs(30));
        assert_eq!(effective_timeout(300_000), COMPONENT_REQUEST_MAX_TIMEOUT);
        assert_eq!(
            effective_timeout(999_000_000),
            COMPONENT_REQUEST_MAX_TIMEOUT
        );
        assert_eq!(effective_timeout(u32::MAX), COMPONENT_REQUEST_MAX_TIMEOUT);
    }

    #[test]
    fn effective_body_budget_defaults_and_clamps_to_64mib() {
        assert_eq!(effective_max_body_bytes(0), DEFAULT_MAX_BODY_BYTES);
        assert_eq!(effective_max_body_bytes(0), 8 * 1024 * 1024);
        assert_eq!(effective_max_body_bytes(16), 16);
        assert_eq!(
            effective_max_body_bytes(COMPONENT_MAX_BODY_BYTES_CEILING),
            COMPONENT_MAX_BODY_BYTES_CEILING
        );
        assert_eq!(
            effective_max_body_bytes(u64::MAX),
            COMPONENT_MAX_BODY_BYTES_CEILING
        );
    }

    #[test]
    fn encode_carries_effective_timeout_and_budget_on_the_wire() {
        // `submit` forwards the effective values as the wire `timeout_ms` /
        // `max_body_bytes`; this pins the encode layer of that contract.
        let mut request = ComponentRequest::new(Method::Get, "https://api.example.com/");
        request.timeout_ms =
            u32::try_from(effective_timeout(0).as_millis()).expect("300 s fits u32");
        request.max_body_bytes = effective_max_body_bytes(0);
        let frames = encode_request(RequestId(3), &offline_grant(), request).expect("encode");
        assert_eq!(frames.len(), 1);
        match decode(&frames[0][FRAME_HEADER_BYTES..]).expect("decode") {
            Message::HttpRequest {
                timeout_ms,
                max_body_bytes,
                ..
            } => {
                assert_eq!(timeout_ms, 30_000);
                assert_eq!(max_body_bytes, 8 * 1024 * 1024);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn warn_sink_receives_the_escaped_stderr_tail() {
        let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
        let capture = Arc::clone(&seen);
        let mut sink: Option<Box<dyn ComponentWarnSink>> = Some(Box::new(
            move |component: &str, reason: &str, tail: &str| {
                capture
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((component.to_owned(), reason.to_owned(), tail.to_owned()));
            },
        ));
        let slot = Slot {
            name: "net".to_owned(),
            tracker: CrashTracker::new(),
            deadline_strikes: DeadlineStrikes::new(),
            process: None,
            stderr: Arc::new(Mutex::new(StderrRing::new(64))),
            spawns: 0,
            last_stop: None,
        };
        slot.stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(b"boom \x1b[0m\\\\");
        emit_warn(&mut sink, &slot, "test crash");
        assert_eq!(
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_slice(),
            [(
                "net".to_owned(),
                "test crash".to_owned(),
                "boom \\u{1b}[0m\\\\\\\\".to_owned()
            )]
            .as_slice()
        );
        // No sink: nothing happens, no panic.
        let mut none: Option<Box<dyn ComponentWarnSink>> = None;
        emit_warn(&mut none, &slot, "test crash");
    }
}
