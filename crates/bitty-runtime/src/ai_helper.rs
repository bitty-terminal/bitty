//! Resident-singleton helper supervisor (S-1, DIR-030, bitty #2123).
//!
//! Core-side spawn path for the native `ai` component (executable
//! `bitty-ai`), the long-lived helper behind the in-helper `ai.*` handler
//! endpoint (program AI-0302, helper side). This module delivers the
//! terminal-side half of the S-1 contract the ruling adopted: **resolution**,
//! **digest verification**, **stdio-coprocess spawn** over the accepted
//! bitty-ipc framing, and a **bounded supervision/restart policy**.
//!
//! ## Relationship to [`crate::component`]
//!
//! [`crate::component`] is the DIR-030 native-component broker for
//! request/response components (first: `net` / `bitty-net`). It multiplexes
//! HTTP-shaped requests over the `bitty-network-wire` codec and **idle-stops**
//! a component after [`crate::component::COMPONENT_IDLE_TIMEOUT`]. The `ai`
//! helper is a different shape: a **resident singleton** that must stay alive
//! for the whole Core session because the S-1 read-only service-metadata
//! snapshot is held against a versioned helper-side snapshot, and post-dispatch
//! `Unknown` reconciliation needs a stable snapshot-version + envelope-id
//! correlator on both sides (see `bitty-ai-docs/architecture/
//! execution-ownership-r1.md`, Ruling 2026-10-15).
//!
//! Both paths share Core's DIR-030 machinery verbatim:
//!
//! - resolution and digest verification: [`crate::component::resolve_search`]
//!   (user tier over system tier, `PATH` never read; a tampered user install
//!   fails closed without system fallback). The executable's SHA-256 is
//!   re-verified against the pinned `bitty-component.toml` digest before
//!   every spawn.
//! - restart bound: [`crate::component::CrashTracker`] and
//!   [`crate::component::SupervisionPolicy`] (backoff
//!   [`crate::component::COMPONENT_BACKOFF_INITIAL`] doubling to
//!   [`crate::component::COMPONENT_BACKOFF_MAX`], latched
//!   [`crate::component::COMPONENT_CRASH_LIMIT`] inside
//!   [`crate::component::COMPONENT_CRASH_WINDOW`]).
//! - cleared environment plus [`crate::component::COMPONENT_ENV_ALLOWLIST`]
//!   ([`crate::component::ComponentEnv`]).
//! - bounded stderr ring ([`crate::component::StderrRing`]).
//!
//! What differs, and is deliberately Core-owned here:
//!
//! - **Framing.** The helper speaks the accepted bitty-ipc length-prefixed
//!   framing (`bitty-ipc-api` `frame` module: 4-byte big-endian length prefix,
//!   256 KiB payload bound), not native-component wire protocol v1. Core only
//!   frames and transports bytes; envelope validation, scopes, consent, and
//!   the six-gate dispatcher live helper-side (out of scope for #2123).
//! - **Residency.** There is no idle stop. The process is spawned once and
//!   kept alive across requests; only an exit (crash) or [`Self::shutdown`]
//!   ends it.
//! - **Single fenced writer.** Exactly one writer thread owns the child's
//!   stdin for one process generation; a generation fence drops stale reader
//!   frames, so a superseded process can never feed the live session.
//!
//! Nothing here binds, listens, or accepts a socket: DIR-030 places the
//! helper on a stdio coprocess, and Core keeps the duplex pipe pair. The
//! accepted IPC 13-method surface, the snapshot-version correlator, and the
//! per-request gates stay helper-side.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use std::{fmt, io};

use bitty_ipc::frame::{Framer, MAX_FRAME_BYTES, encode_frame};

use super::component::{
    COMPONENT_BACKOFF_INITIAL, COMPONENT_BACKOFF_MAX, COMPONENT_CRASH_LIMIT,
    COMPONENT_CRASH_WINDOW, COMPONENT_ENV_ALLOWLIST, COMPONENT_EXIT_RECHECK,
    COMPONENT_SHUTDOWN_GRACE, COMPONENT_STDERR_MAX_BYTES, ComponentEnv, ComponentSource,
    CrashTracker, ResolveError, ResolvedComponent, SpawnGate, StderrRing, SupervisionPolicy,
    resolve_search, stderr_log_tail,
};

/// Native-component name of the AI helper (DIR-030; executable `bitty-ai`).
///
/// The name is contract data, not a host path: DIR-030 fixes the component
/// name `ai`, from which [`crate::component::executable_file_name`] derives the
/// executable (`bitty-ai`) the same way it does for `net` / `bitty-net`.
pub const HELPER_COMPONENT_NAME: &str = "ai";

/// Inbound frames buffered between the reader thread and [`ResidentHelper`].
///
/// Bounded: a full queue blocks the reader thread, which applies backpressure
/// to the helper through its stdout pipe.
pub const HELPER_INBOUND_QUEUE_FRAMES: usize = 128;

/// Outbound batches (one framed message each) queued for the single writer
/// thread.
pub const HELPER_OUTBOUND_QUEUE_BATCHES: usize = 128;

/// Inbound frames processed per [`ResidentHelper::poll`] call, so one call
/// stays bounded even when the helper floods its stdout.
pub const HELPER_POLL_MAX_FRAMES: usize = 256;

/// Read chunk size of the reader and stderr threads.
pub const HELPER_READ_CHUNK: usize = 64 * 1024;

/// Grace between dropping stdin and killing the recorded child on
/// [`ResidentHelper::shutdown`] (mirrors the DIR-030 shutdown grace).
pub const HELPER_SHUTDOWN_GRACE: Duration = COMPONENT_SHUTDOWN_GRACE;

/// How the helper process ended.
///
/// A resident helper is expected to live for the whole session, so any exit
/// while it is not being shut down is a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Exited on its own after stdin close during a graceful shutdown.
    Exited,
    /// Killed (recorded PID only) after the shutdown grace expired.
    Killed,
    /// Exited unexpectedly: counted against the restart policy.
    Crashed,
}

/// Coarse lifecycle state for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperState {
    /// Not running (never started, or waiting out a crash backoff).
    Stopped,
    /// Spawned and resident.
    Running,
    /// Graceful stop in progress (stdin closed, waiting for exit).
    Stopping,
    /// Latched unavailable after repeated crashes; needs a new session.
    Unavailable,
}

/// One event produced by [`ResidentHelper::poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperEvent {
    /// One framed message the helper wrote to stdout (raw payload bytes).
    ///
    /// Core frames and transports only: the payload is the helper's own
    /// envelope, validated helper-side.
    Frame(Vec<u8>),
    /// The helper process ended; the restart policy was applied.
    Exited(StopOutcome),
}

/// Diagnostic snapshot of the helper slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperStatus {
    /// Current lifecycle state.
    pub state: HelperState,
    /// PID of the resident child, if running.
    pub pid: Option<u32>,
    /// Generation of the resident (or most recent) process.
    pub generation: u64,
    /// Processes spawned so far this session.
    pub spawns: u64,
    /// How the last process ended, if it has ended.
    pub last_stop: Option<StopOutcome>,
    /// Descriptor version of the resolved helper, once resolved.
    pub version: Option<String>,
    /// Which resolution tier the helper came from.
    pub source: Option<ComponentSource>,
}

/// Typed spawn-path outcome (never a panic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperError {
    /// Resolution or digest verification failed (fail closed, nothing spawned).
    Resolve(ResolveError),
    /// The helper is in crash backoff; retry after the given delay.
    Backoff {
        /// Remaining delay before the next spawn attempt is allowed.
        retry_after: Duration,
    },
    /// The helper crashed too often and is unavailable until a new session.
    Unavailable,
    /// The executable could not be started.
    Spawn(io::ErrorKind),
    /// A payload exceeds the accepted framing bound.
    FrameTooLarge {
        /// Offending payload length.
        actual: usize,
        /// Framing bound ([`MAX_FRAME_BYTES`]).
        limit: usize,
    },
    /// Too many outbound batches are queued (the helper is not draining).
    Busy,
    /// No resident process (not started, stopping, or gone).
    Closed,
}

impl fmt::Display for HelperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HelperError::Resolve(error) => write!(f, "ai helper unavailable: {error}"),
            HelperError::Backoff { retry_after } => write!(
                f,
                "ai helper restarting; retry after {} ms",
                retry_after.as_millis()
            ),
            HelperError::Unavailable => f.write_str("ai helper unavailable after repeated crashes"),
            HelperError::Spawn(kind) => write!(f, "ai helper spawn failed: {kind}"),
            HelperError::FrameTooLarge { actual, limit } => {
                write!(f, "frame of {actual} bytes exceeds the {limit}-byte bound")
            }
            HelperError::Busy => f.write_str("ai helper outbound queue is full"),
            HelperError::Closed => f.write_str("no resident ai helper process"),
        }
    }
}

impl std::error::Error for HelperError {}

impl From<ResolveError> for HelperError {
    fn from(error: ResolveError) -> Self {
        HelperError::Resolve(error)
    }
}

/// Spawn-path configuration (no host values baked in; every root is passed in).
#[derive(Debug, Clone)]
pub struct HelperConfig {
    /// Native-component name to resolve (default [`HELPER_COMPONENT_NAME`]).
    pub component: String,
    /// User component root (`<data_home>/bitty/extensions`); `None` disables
    /// the user tier.
    pub user_root: Option<PathBuf>,
    /// System component root; `None` disables the system tier.
    pub system_root: Option<PathBuf>,
    /// Allowlisted environment forwarded to the helper.
    pub env: ComponentEnv,
    /// Core version, for diagnostics.
    pub core_version: String,
    /// Restart/supervision numbers (DIR-030 defaults).
    pub policy: SupervisionPolicy,
}

impl HelperConfig {
    /// DIR-030 defaults: the `ai` component, no system tier, DIR-030 crash
    /// policy.
    #[must_use]
    pub fn new(user_root: Option<PathBuf>, env: ComponentEnv) -> Self {
        Self {
            component: HELPER_COMPONENT_NAME.to_owned(),
            user_root,
            system_root: None,
            env,
            core_version: env!("CARGO_PKG_VERSION").to_owned(),
            policy: SupervisionPolicy::core_defaults(),
        }
    }

    /// Override the resolved component name (tests; DIR-030 fixes `ai`).
    #[must_use]
    pub fn with_component(mut self, component: impl Into<String>) -> Self {
        self.component = component.into();
        self
    }

    /// Enable the read-only system tier for two-tier resolution.
    #[must_use]
    pub fn with_system_root(mut self, system_root: Option<PathBuf>) -> Self {
        self.system_root = system_root;
        self
    }

    /// Override the supervision policy (tests).
    #[must_use]
    pub fn with_policy(mut self, policy: SupervisionPolicy) -> Self {
        self.policy = policy;
        self
    }
}

struct Process {
    generation: u64,
    child: Child,
    stdin: Option<SyncSender<Vec<Vec<u8>>>>,
    stop_deadline: Option<Instant>,
}

enum Inbound {
    Frame { generation: u64, payload: Vec<u8> },
    Closed { generation: u64 },
}

/// Resident-singleton supervisor for the native `ai` helper.
///
/// Owns at most one helper process. Call [`Self::ensure_running`] before use
/// (it resolves, digest-verifies, and spawns when needed), [`Self::submit_frame`]
/// to write a framed message, and [`Self::poll`] to drain helper output and
/// apply the restart policy. Time is injected so backoff is deterministic
/// under test.
pub struct ResidentHelper {
    config: HelperConfig,
    tracker: CrashTracker,
    process: Option<Process>,
    stderr: Arc<Mutex<StderrRing>>,
    inbound_tx: SyncSender<Inbound>,
    inbound_rx: Receiver<Inbound>,
    spawns: u64,
    last_stop: Option<StopOutcome>,
    next_generation: u64,
    version: Option<String>,
    source: Option<ComponentSource>,
}

impl fmt::Debug for ResidentHelper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResidentHelper")
            .field("component", &self.config.component)
            .field("user_root", &self.config.user_root)
            .field("running", &self.process.is_some())
            .finish_non_exhaustive()
    }
}

impl ResidentHelper {
    /// Supervisor with no running process.
    #[must_use]
    pub fn new(config: HelperConfig) -> Self {
        let (inbound_tx, inbound_rx) = sync_channel(HELPER_INBOUND_QUEUE_FRAMES);
        let tracker = CrashTracker::with_supervision_policy(config.policy);
        Self {
            config,
            tracker,
            process: None,
            stderr: Arc::new(Mutex::new(StderrRing::new(COMPONENT_STDERR_MAX_BYTES))),
            inbound_tx,
            inbound_rx,
            spawns: 0,
            last_stop: None,
            next_generation: 0,
            version: None,
            source: None,
        }
    }

    /// The resolved component name.
    #[must_use]
    pub fn component(&self) -> &str {
        &self.config.component
    }

    /// Resolve and digest-verify the helper without spawning it.
    ///
    /// Every spawn re-runs this internally; exposing it lets Core fail closed
    /// on a missing or tampered install before allocating anything.
    pub fn resolve(&self) -> Result<ResolvedComponent, ResolveError> {
        resolve_search(
            self.config.user_root.as_deref(),
            self.config.system_root.as_deref(),
            &self.config.component,
        )
        .map(|searched| searched.resolved)
    }

    /// Ensure a resident helper is running at `now`, spawning one if needed.
    ///
    /// Fails closed on resolution/digest mismatch, reports the remaining
    /// backoff while a crash delay is active, and reports [`HelperError::Unavailable`]
    /// once the crash limit latched.
    pub fn ensure_running(&mut self, now: Instant) -> Result<(), HelperError> {
        if self
            .process
            .as_ref()
            .is_some_and(|process| process.stop_deadline.is_none())
        {
            return Ok(());
        }
        // A stopping process is finished immediately: a resident singleton
        // never overlaps two generations.
        if self.process.is_some() {
            self.kill_current(&mut Vec::new());
        }
        match self.tracker.gate(now) {
            SpawnGate::Ready => {}
            SpawnGate::Backoff { retry_after } => {
                return Err(HelperError::Backoff { retry_after });
            }
            SpawnGate::Unavailable => return Err(HelperError::Unavailable),
        }
        self.spawn()
    }

    /// Frame `payload` and write it to the resident helper's stdin.
    ///
    /// Returns [`HelperError::FrameTooLarge`] before touching the pipe when
    /// the payload exceeds the accepted framing bound, [`HelperError::Busy`]
    /// when the outbound queue is full, and [`HelperError::Closed`] when no
    /// process is resident.
    pub fn submit_frame(&mut self, payload: &[u8]) -> Result<(), HelperError> {
        if payload.len() > MAX_FRAME_BYTES {
            return Err(HelperError::FrameTooLarge {
                actual: payload.len(),
                limit: MAX_FRAME_BYTES,
            });
        }
        // `encode_frame` re-checks the bound; the map arm is unreachable after
        // the guard above but kept so the bound is enforced in one place.
        let frame = encode_frame(payload).map_err(|_| HelperError::FrameTooLarge {
            actual: payload.len(),
            limit: MAX_FRAME_BYTES,
        })?;
        let Some(process) = self.process.as_ref() else {
            return Err(HelperError::Closed);
        };
        if process.stop_deadline.is_some() {
            return Err(HelperError::Closed);
        }
        let Some(stdin) = process.stdin.as_ref() else {
            return Err(HelperError::Closed);
        };
        match stdin.try_send(vec![frame]) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(HelperError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(HelperError::Closed),
        }
    }

    /// Drain helper output and apply the restart policy at `now`.
    ///
    /// Never blocks; handles at most [`HELPER_POLL_MAX_FRAMES`] frames per
    /// call. Frames from a superseded generation are discarded (generation
    /// fence).
    pub fn poll(&mut self, now: Instant) -> Vec<HelperEvent> {
        let mut events = Vec::new();
        for _ in 0..HELPER_POLL_MAX_FRAMES {
            match self.inbound_rx.try_recv() {
                Ok(inbound) => self.handle_inbound(now, inbound, &mut events),
                Err(_) => break,
            }
        }
        self.maintain(now, &mut events);
        events
    }

    /// Like [`Self::poll`], but first blocks up to `timeout` for helper output.
    /// `now` drives policy; `timeout` is real time.
    pub fn wait(&mut self, now: Instant, timeout: Duration) -> Vec<HelperEvent> {
        let mut events = Vec::new();
        if let Ok(inbound) = self.inbound_rx.recv_timeout(timeout) {
            self.handle_inbound(now, inbound, &mut events);
        }
        events.extend(self.poll(now));
        events
    }

    /// Graceful stop: close stdin, wait up to [`HELPER_SHUTDOWN_GRACE`] for
    /// exit, then kill the recorded child only. Inbound frames are dropped.
    pub fn shutdown(&mut self) -> Vec<HelperEvent> {
        let mut events = Vec::new();
        let deadline = Instant::now() + self.config.policy.shutdown_grace;
        let Some(process) = self.process.as_mut() else {
            return events;
        };
        process.stdin = None;
        process.stop_deadline = Some(deadline);
        while Instant::now() < deadline {
            if self.child_exited() {
                break;
            }
            let slice =
                COMPONENT_EXIT_RECHECK.min(deadline.saturating_duration_since(Instant::now()));
            if slice.is_zero() {
                break;
            }
            let _ = self.inbound_rx.recv_timeout(slice);
        }
        if self.child_exited() {
            self.reap(StopOutcome::Exited, Instant::now(), &mut events);
        } else {
            self.kill_current(&mut events);
        }
        events
    }

    /// Diagnostic snapshot.
    #[must_use]
    pub fn status(&self) -> HelperStatus {
        let generation = self.next_generation.saturating_sub(1);
        match &self.process {
            Some(process) => {
                let state = if process.stop_deadline.is_some() {
                    HelperState::Stopping
                } else {
                    HelperState::Running
                };
                HelperStatus {
                    state,
                    pid: Some(process.child.id()),
                    generation: process.generation,
                    spawns: self.spawns,
                    last_stop: self.last_stop,
                    version: self.version.clone(),
                    source: self.source,
                }
            }
            None => HelperStatus {
                state: if self.tracker.is_unavailable() {
                    HelperState::Unavailable
                } else {
                    HelperState::Stopped
                },
                pid: None,
                generation,
                spawns: self.spawns,
                last_stop: self.last_stop,
                version: self.version.clone(),
                source: self.source,
            },
        }
    }

    /// Newest bytes the helper wrote to stderr (bounded ring).
    #[must_use]
    pub fn stderr_tail(&self) -> Vec<u8> {
        let ring = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        ring.snapshot()
    }

    /// Crashes counted inside the window ending at `now`.
    #[must_use]
    pub fn recent_crashes(&self, now: Instant) -> usize {
        self.tracker.recent_crashes(now)
    }

    fn spawn(&mut self) -> Result<(), HelperError> {
        // Full resolution and digest verification before every spawn (fail
        // closed; `PATH` is never read).
        let searched = resolve_search(
            self.config.user_root.as_deref(),
            self.config.system_root.as_deref(),
            &self.config.component,
        )?;
        let resolved = &searched.resolved;
        let mut command = Command::new(&resolved.executable_path);
        command
            .env_clear()
            .current_dir(&resolved.version_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in self.config.env.vars() {
            command.env(name, value);
        }
        let mut child = command
            .spawn()
            .map_err(|error| HelperError::Spawn(error.kind()))?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HelperError::Spawn(io::ErrorKind::BrokenPipe));
        };
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let (stdin_tx, stdin_rx) = sync_channel::<Vec<Vec<u8>>>(HELPER_OUTBOUND_QUEUE_BATCHES);
        // Single fenced writer: one thread owns stdin for this generation.
        let started = spawn_writer(stdin, stdin_rx)
            .and_then(|()| spawn_reader(stdout, generation, self.inbound_tx.clone()))
            .and_then(|()| spawn_stderr(stderr, Arc::clone(&self.stderr)));
        if let Err(error) = started {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HelperError::Spawn(error.kind()));
        }
        self.process = Some(Process {
            generation,
            child,
            stdin: Some(stdin_tx),
            stop_deadline: None,
        });
        self.spawns = self.spawns.saturating_add(1);
        self.version = Some(resolved.descriptor.version.clone());
        self.source = Some(searched.source);
        Ok(())
    }

    fn handle_inbound(&mut self, now: Instant, inbound: Inbound, events: &mut Vec<HelperEvent>) {
        let current = self.process.as_ref().map(|process| process.generation);
        match inbound {
            Inbound::Frame {
                generation,
                payload,
            } => {
                if current == Some(generation) {
                    events.push(HelperEvent::Frame(payload));
                }
            }
            Inbound::Closed { generation } => {
                if current != Some(generation) {
                    return;
                }
                // stdout closed: either a graceful shutdown finishing or an
                // unexpected loss. A resident helper that closes its stdout is
                // gone.
                let graceful = self
                    .process
                    .as_ref()
                    .is_some_and(|process| process.stop_deadline.is_some());
                if graceful {
                    self.reap(StopOutcome::Exited, now, events);
                } else {
                    self.reap(StopOutcome::Crashed, now, events);
                }
            }
        }
    }

    fn maintain(&mut self, now: Instant, events: &mut Vec<HelperEvent>) {
        if self.child_exited() {
            let graceful = self
                .process
                .as_ref()
                .is_some_and(|process| process.stop_deadline.is_some());
            let outcome = if graceful {
                StopOutcome::Exited
            } else {
                StopOutcome::Crashed
            };
            self.reap(outcome, now, events);
        }
    }

    fn child_exited(&mut self) -> bool {
        let Some(process) = self.process.as_mut() else {
            return false;
        };
        matches!(process.child.try_wait(), Ok(Some(_)) | Err(_))
    }

    /// Take the current process and apply the restart policy for `outcome`.
    fn reap(&mut self, outcome: StopOutcome, now: Instant, events: &mut Vec<HelperEvent>) {
        let Some(mut process) = self.process.take() else {
            return;
        };
        process.stdin = None;
        let _ = process.child.wait();
        self.last_stop = Some(outcome);
        if outcome == StopOutcome::Crashed {
            self.log_stderr_tail();
            let _ = self.tracker.record_crash(now);
        }
        events.push(HelperEvent::Exited(outcome));
    }

    fn kill_current(&mut self, events: &mut Vec<HelperEvent>) {
        let Some(mut process) = self.process.take() else {
            return;
        };
        process.stdin = None;
        let _ = process.child.kill();
        let _ = process.child.wait();
        self.last_stop = Some(StopOutcome::Killed);
        events.push(HelperEvent::Exited(StopOutcome::Killed));
    }

    fn log_stderr_tail(&self) {
        let ring = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        let tail = stderr_log_tail(&ring.snapshot());
        if !tail.is_empty() {
            eprintln!(
                "bitty: ai helper ({}) crashed; stderr tail: {tail}",
                self.config.component
            );
        }
    }
}

impl Drop for ResidentHelper {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.stdin = None;
            let _ = process.child.kill();
            let _ = process.child.wait();
        }
    }
}

/// Single fenced writer: owns the child stdin for one generation and ends when
/// the supervisor drops the sender (delivering EOF) or the pipe breaks.
fn spawn_writer(mut stdin: ChildStdin, rx: Receiver<Vec<Vec<u8>>>) -> io::Result<()> {
    thread::Builder::new()
        .name("bitty-ai-helper-stdin".into())
        .spawn(move || {
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

/// Reads the helper's stdout, decodes accepted bitty-ipc framing, and reports
/// frames (or closure) tagged with the process generation.
fn spawn_reader(
    mut stdout: ChildStdout,
    generation: u64,
    tx: SyncSender<Inbound>,
) -> io::Result<()> {
    thread::Builder::new()
        .name("bitty-ai-helper-stdout".into())
        .spawn(move || {
            let mut framer = Framer::new();
            let mut buf = vec![0u8; HELPER_READ_CHUNK];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => match framer.push_bytes(&buf[..n]) {
                        Ok(frames) => {
                            for frame in frames {
                                let inbound = Inbound::Frame {
                                    generation,
                                    payload: frame.into_payload(),
                                };
                                if tx.send(inbound).is_err() {
                                    return;
                                }
                            }
                        }
                        // A malformed frame stream ends the reader; the closure
                        // below turns it into a crash for the live generation.
                        Err(_) => break,
                    },
                }
            }
            let _ = tx.send(Inbound::Closed { generation });
        })
        .map(|_| ())
}

fn spawn_stderr(mut stderr: ChildStderr, ring: Arc<Mutex<StderrRing>>) -> io::Result<()> {
    thread::Builder::new()
        .name("bitty-ai-helper-stderr".into())
        .spawn(move || {
            let mut buf = vec![0u8; HELPER_READ_CHUNK];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut guard = ring.lock().unwrap_or_else(PoisonError::into_inner);
                        guard.push(&buf[..n]);
                    }
                }
            }
        })
        .map(|_| ())
}

/// Contract constants surfaced for reviewers and tests: the DIR-030 numbers
/// the AI resident helper inherits unchanged.
#[must_use]
pub const fn helper_policy_defaults() -> (Duration, Duration, usize, Duration) {
    (
        COMPONENT_BACKOFF_INITIAL,
        COMPONENT_BACKOFF_MAX,
        COMPONENT_CRASH_LIMIT,
        COMPONENT_CRASH_WINDOW,
    )
}

/// The allowlisted environment names forwarded to the helper (never `PATH`).
#[must_use]
pub const fn helper_env_allowlist() -> &'static [&'static str] {
    &COMPONENT_ENV_ALLOWLIST
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_env_allowlist_never_forwards_path() {
        assert!(helper_env_allowlist().contains(&"HTTPS_PROXY"));
        assert!(!helper_env_allowlist().contains(&"PATH"));
    }

    #[test]
    fn policy_defaults_match_dir030_numbers() {
        let (initial, max, limit, window) = helper_policy_defaults();
        assert_eq!(initial, COMPONENT_BACKOFF_INITIAL);
        assert_eq!(max, COMPONENT_BACKOFF_MAX);
        assert_eq!(limit, COMPONENT_CRASH_LIMIT);
        assert_eq!(window, COMPONENT_CRASH_WINDOW);
    }

    #[test]
    fn submit_rejects_payloads_over_the_framing_bound() {
        let mut helper = ResidentHelper::new(HelperConfig::new(None, ComponentEnv::empty()));
        let oversize = vec![0u8; MAX_FRAME_BYTES + 1];
        assert_eq!(
            helper.submit_frame(&oversize),
            Err(HelperError::FrameTooLarge {
                actual: MAX_FRAME_BYTES + 1,
                limit: MAX_FRAME_BYTES,
            })
        );
    }

    #[test]
    fn submit_without_a_process_is_closed() {
        let mut helper = ResidentHelper::new(HelperConfig::new(None, ComponentEnv::empty()));
        assert_eq!(helper.submit_frame(b"{}"), Err(HelperError::Closed));
    }

    #[test]
    fn unavailable_before_resolution_when_no_root_is_configured() {
        let mut helper = ResidentHelper::new(HelperConfig::new(None, ComponentEnv::empty()));
        let error = helper
            .ensure_running(Instant::now())
            .expect_err("no component root");
        assert_eq!(error, HelperError::Resolve(ResolveError::NoRoot));
        assert_eq!(helper.status().state, HelperState::Stopped);
    }
}
