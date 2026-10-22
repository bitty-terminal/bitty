//! Process, threading, and protocol state machine for the execution
//! component host client.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bitty_execution::component::protocol::{
    CONNECTION_ID, ErrorKind, ExitKind, FrameReader, MAX_OUTPUT_CHUNK_BYTES, MAX_OUTPUT_READ_BYTES,
    MAX_TAIL_LINES, Message, PROTOCOL_VERSION, TerminateMode, TerminateOutcome, WireCancelEffect,
    WireDeadlineClock, WireEnv, WireEnvVar, WireKillScope, WireOomVerdict,
};

use super::{
    ComponentCancelEffect, ComponentDeadlineClock, ComponentEnvironment, ComponentError,
    ComponentExit, ComponentExitKind, ComponentHostConfig, ComponentJobSnapshot,
    ComponentKillScope, ComponentOomVerdict, ComponentOutput, ComponentRun, ComponentSpawnKind,
    ComponentSpawnSpec, ComponentStream, ComponentTerminateMode, ComponentTerminateOutcome,
    EXECUTION_COMPONENT_CANCEL_GRACE, EXECUTION_COMPONENT_INBOUND_FRAMES,
    EXECUTION_COMPONENT_MAX_JOBS, EXECUTION_COMPONENT_NAME, EXECUTION_COMPONENT_OUTBOUND_FRAMES,
    EXECUTION_COMPONENT_POLL_MAX_FRAMES, EXECUTION_COMPONENT_POLL_SLICE,
    EXECUTION_COMPONENT_PROTOCOL_MAX, EXECUTION_COMPONENT_PROTOCOL_MIN,
    EXECUTION_COMPONENT_RETAINED_BYTES, EXECUTION_COMPONENT_STDERR_BYTES,
    EXECUTION_COMPONENT_WAIT_GRACE,
};
use crate::component::{ResolveError, StderrRing, resolve_search, stderr_log_tail};

/// One item read from the component's stdout by the reader thread.
enum Inbound {
    /// One decoded protocol frame.
    Message(Message),
    /// The decoder or the pipe failed; the connection is finished.
    Fault {
        /// Whether the peer violated the protocol (rather than a pipe error).
        protocol: bool,
        /// Bounded reason.
        reason: String,
    },
    /// Clean EOF between frames.
    Closed,
}

/// Point-in-time job record owned by the host.
#[derive(Debug, Default)]
struct JobState {
    generation: Option<u64>,
    started: bool,
    canceled: Option<ComponentTerminateOutcome>,
    exit: Option<ComponentExit>,
    stdout: Tail,
    stderr: Tail,
    error: Option<ComponentError>,
}

impl JobState {
    fn snapshot(&self, job: u64) -> ComponentJobSnapshot {
        ComponentJobSnapshot {
            job,
            generation: self.generation,
            started: self.started,
            canceled: self.canceled,
            exit: self.exit,
            stdout: self.stdout.text(),
            stderr: self.stderr.text(),
            stdout_total: self.stdout.total,
            stderr_total: self.stderr.total,
            stdout_truncated: self.stdout.dropped > 0
                || self.exit.is_some_and(|exit| {
                    exit.stdout_evicted > 0 || exit.stdout_total > self.stdout.total
                }),
            stderr_truncated: self.stderr.dropped > 0
                || self.exit.is_some_and(|exit| {
                    exit.stderr_evicted > 0 || exit.stderr_total > self.stderr.total
                }),
        }
    }
}

/// Newest-wins bounded byte tail.
///
/// Eviction is chunk-granular, matching the linked supervisor's per-stream
/// store: whole drain chunks are dropped oldest-first, so the retained byte
/// count can be a little below the cap instead of splitting a chunk.
#[derive(Debug, Default)]
struct Tail {
    chunks: std::collections::VecDeque<Vec<u8>>,
    stored: usize,
    total: u64,
    dropped: u64,
}

impl Tail {
    fn push(&mut self, data: &[u8]) {
        self.total = self.total.saturating_add(data.len() as u64);
        if data.is_empty() {
            return;
        }
        self.chunks.push_back(data.to_vec());
        self.stored = self.stored.saturating_add(data.len());
        while self.stored > EXECUTION_COMPONENT_RETAINED_BYTES {
            match self.chunks.pop_front() {
                Some(evicted) => {
                    self.stored = self.stored.saturating_sub(evicted.len());
                    self.dropped = self.dropped.saturating_add(evicted.len() as u64);
                }
                None => break,
            }
        }
    }

    fn text(&self) -> String {
        let mut bytes = Vec::with_capacity(self.stored);
        for chunk in &self.chunks {
            bytes.extend_from_slice(chunk);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Core-side host for one execution component process.
///
/// Construct with [`ComponentExecutionHost::connect`]; the constructor
/// resolves and verifies the install, spawns the coprocess, starts the three
/// I/O threads, and completes the handshake. Every blocking wait is bounded
/// and returns [`ComponentError::Timeout`] instead of hanging. Dropping the
/// host kills the recorded component process (and its Unix process group).
pub struct ComponentExecutionHost {
    config: ComponentHostConfig,
    child: Child,
    pid: u32,
    /// Whether the component was placed in its own Unix process group.
    #[cfg(unix)]
    process_group: bool,
    stdin_tx: Option<SyncSender<Message>>,
    inbound: Receiver<Inbound>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<StderrRing>>,
    component_version: String,
    next_id: u64,
    jobs: BTreeMap<u64, JobState>,
    pending_read: Option<u64>,
    abandoned_reads: std::collections::VecDeque<u64>,
    read_result: Option<ComponentOutput>,
    read_error: Option<ComponentError>,
    shutdown_ack: bool,
    shutting_down: bool,
    stdout_closed: bool,
    exited: bool,
    fault: Option<ComponentError>,
}

impl std::fmt::Debug for ComponentExecutionHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentExecutionHost")
            .field("pid", &self.pid)
            .field("component_version", &self.component_version)
            .field("jobs", &self.jobs.len())
            .field("fault", &self.fault)
            .finish()
    }
}

impl ComponentExecutionHost {
    /// Resolve, verify, spawn, and handshake one `execution` component.
    ///
    /// # Errors
    ///
    /// - [`ComponentError::SafeMode`] before any resolution or process
    ///   contact when `config.safe_mode` is set.
    /// - [`ComponentError::NotInstalled`] / [`ComponentError::Incompatible`]
    ///   / [`ComponentError::UnsafeInstall`] for a missing or tampered
    ///   install.
    /// - [`ComponentError::Io`] for spawn/pipe failures.
    /// - [`ComponentError::Timeout`] when `HelloAck` misses the deadline.
    /// - [`ComponentError::Protocol`] for a wrong component/version or a
    ///   malformed handshake reply.
    pub fn connect(config: ComponentHostConfig) -> Result<Self, ComponentError> {
        if config.safe_mode {
            return Err(ComponentError::SafeMode);
        }
        let searched = resolve_search(
            config.user_root.as_deref(),
            config.system_root.as_deref(),
            EXECUTION_COMPONENT_NAME,
        )
        .map_err(map_resolve_error)?;
        let resolved = searched.resolved;
        // `resolve` already checks the descriptor against the shared
        // `bitty-network-wire` protocol version (currently 1); the execution
        // protocol is independently v1. The explicit range check below keeps
        // this boundary honest if either codec ever moves.
        if resolved.descriptor.protocol_min > EXECUTION_COMPONENT_PROTOCOL_MAX
            || resolved.descriptor.protocol_max < EXECUTION_COMPONENT_PROTOCOL_MIN
        {
            return Err(ComponentError::Incompatible {
                min: resolved.descriptor.protocol_min,
                max: resolved.descriptor.protocol_max,
            });
        }

        let mut command = Command::new(&resolved.executable_path);
        command
            .current_dir(&resolved.version_dir)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in config.env.vars() {
            command.env(name, value);
        }
        #[cfg(unix)]
        let process_group = {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
            true
        };

        let mut child = command.spawn().map_err(|error| ComponentError::Io {
            operation: "spawn",
            kind: error.kind(),
        })?;
        let pid = child.id();
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ComponentError::Io {
                operation: "stdin pipe",
                kind: io::ErrorKind::BrokenPipe,
            });
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ComponentError::Io {
                operation: "stdout pipe",
                kind: io::ErrorKind::BrokenPipe,
            });
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ComponentError::Io {
                operation: "stderr pipe",
                kind: io::ErrorKind::BrokenPipe,
            });
        };

        let (inbound_tx, inbound) = sync_channel(EXECUTION_COMPONENT_INBOUND_FRAMES);
        let (outbound_tx, outbound) = sync_channel(EXECUTION_COMPONENT_OUTBOUND_FRAMES);
        let stderr_ring = Arc::new(Mutex::new(StderrRing::new(
            EXECUTION_COMPONENT_STDERR_BYTES,
        )));
        let (reader, writer, stderr_reader) = match spawn_component_threads(
            stdout,
            inbound_tx,
            stdin,
            outbound,
            stderr,
            Arc::clone(&stderr_ring),
        ) {
            Ok(threads) => threads,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ComponentError::Io {
                    operation: "io thread",
                    kind: error.kind(),
                });
            }
        };

        let mut host = Self {
            config,
            child,
            pid,
            #[cfg(unix)]
            process_group,
            stdin_tx: Some(outbound_tx),
            inbound,
            reader: Some(reader),
            writer: Some(writer),
            stderr_reader: Some(stderr_reader),
            stderr: stderr_ring,
            component_version: String::new(),
            next_id: 1,
            jobs: BTreeMap::new(),
            pending_read: None,
            abandoned_reads: std::collections::VecDeque::new(),
            read_result: None,
            read_error: None,
            shutdown_ack: false,
            shutting_down: false,
            stdout_closed: false,
            exited: false,
            fault: None,
        };

        let hello = Message::Hello {
            min: EXECUTION_COMPONENT_PROTOCOL_MIN,
            max: EXECUTION_COMPONENT_PROTOCOL_MAX,
            component: EXECUTION_COMPONENT_NAME.to_owned(),
            version: host.config.core_version.clone(),
        };
        if let Err(error) = host.send(hello) {
            host.abort_process();
            return Err(error);
        }
        match host.handshake() {
            Ok(version) => host.component_version = version,
            Err(error) => {
                host.abort_process();
                return Err(error);
            }
        }
        Ok(host)
    }

    /// Component version reported by `HelloAck`.
    #[must_use]
    pub fn component_version(&self) -> &str {
        &self.component_version
    }

    /// Recorded component process id.
    #[must_use]
    pub fn component_pid(&self) -> u32 {
        self.pid
    }

    /// Whether the component process is still observable.
    #[must_use]
    pub fn is_running(&mut self) -> bool {
        if self.exited {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.exited = true;
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Newest bounded component stderr, escaped for diagnostics.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        let ring = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        stderr_log_tail(&ring.snapshot())
    }

    /// Queue one `Spawn`, returning its host-assigned job id.
    ///
    /// This never waits for `Spawned` or a child; use [`Self::poll`] or
    /// [`Self::wait_terminal`] for progress. The id is unique for the
    /// component's lifetime.
    ///
    /// # Errors
    ///
    /// Returns a validation error before any frame is queued, `JobLimit`
    /// while the host tracks [`EXECUTION_COMPONENT_MAX_JOBS`], or
    /// `Unavailable`/`ComponentLost` when the request cannot be queued.
    pub fn spawn(&mut self, spec: &ComponentSpawnSpec) -> Result<u64, ComponentError> {
        self.ensure_usable()?;
        spec.validate()?;
        if self.jobs.len() >= EXECUTION_COMPONENT_MAX_JOBS {
            return Err(ComponentError::JobLimit {
                limit: EXECUTION_COMPONENT_MAX_JOBS,
            });
        }
        let job = self.mint_id()?;
        let env = match &spec.env {
            ComponentEnvironment::Isolated => WireEnv::Isolated,
            ComponentEnvironment::Explicit(vars) => WireEnv::Explicit(
                vars.iter()
                    .map(|(name, value)| WireEnvVar {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect(),
            ),
        };
        let message = Message::Spawn {
            job,
            program: spec.program.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            env,
            kind: wire_kind(spec.kind),
            io: bitty_execution::component::protocol::SpawnIo::Pipes,
            lifetime: bitty_execution::component::protocol::SpawnLifetime::Detached,
            hard_ms: spec.hard_timeout_ms,
            idle_ms: spec.idle_timeout_ms,
            retention_ms: spec.retention_ms,
        };
        self.jobs.insert(job, JobState::default());
        if let Err(error) = self.send(message) {
            self.jobs.remove(&job);
            return Err(error);
        }
        Ok(job)
    }

    /// Drain inbound frames for at most `timeout` (`Duration::ZERO` is a
    /// non-blocking pump).
    ///
    /// At most [`EXECUTION_COMPONENT_POLL_MAX_FRAMES`] frames are processed,
    /// so one call stays bounded under a flooding component.
    ///
    /// # Errors
    ///
    /// Returns a stored fault (protocol violation, lost component) if the
    /// connection is already unusable. A poll-slice timeout is not an error.
    pub fn poll(&mut self, timeout: Duration) -> Result<(), ComponentError> {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let mut frames = 0usize;
        let mut first = true;
        loop {
            let item = if first && !timeout.is_zero() {
                first = false;
                match self.inbound.recv_timeout(timeout) {
                    Ok(item) => item,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(self.connection_lost("inbound channel closed".to_owned()));
                    }
                }
            } else {
                match self.inbound.try_recv() {
                    Ok(item) => item,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        return Err(self.connection_lost("inbound channel closed".to_owned()));
                    }
                }
            };
            self.handle_inbound(item)?;
            frames += 1;
            if frames >= EXECUTION_COMPONENT_POLL_MAX_FRAMES {
                break;
            }
            if !timeout.is_zero() && Instant::now() >= deadline {
                break;
            }
        }
        Ok(())
    }

    /// Snapshot of one tracked job.
    #[must_use]
    pub fn job(&self, job: u64) -> Option<ComponentJobSnapshot> {
        self.jobs.get(&job).map(|state| state.snapshot(job))
    }

    /// Wait for a job's terminal `Exit` frame.
    ///
    /// # Errors
    ///
    /// `Timeout` when `deadline` passes without a terminal frame, or the
    /// stored connection fault.
    pub fn wait_terminal(
        &mut self,
        job: u64,
        deadline: Instant,
    ) -> Result<ComponentJobSnapshot, ComponentError> {
        loop {
            if let Some(state) = self.jobs.get(&job) {
                if let Some(exit) = state.exit {
                    return Ok(ComponentJobSnapshot {
                        exit: Some(exit),
                        ..state.snapshot(job)
                    });
                }
                if let Some(error) = &state.error {
                    return Err(error.clone());
                }
            } else {
                return Err(ComponentError::Unavailable {
                    reason: format!("job {job} is no longer tracked"),
                });
            }
            if let Some(fault) = &self.fault {
                return Err(fault.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(ComponentError::Timeout {
                    operation: "job terminal wait",
                });
            }
            let slice = (deadline - now).min(EXECUTION_COMPONENT_POLL_SLICE);
            match self.poll(slice) {
                Ok(()) => {}
                Err(ComponentError::Timeout { .. }) => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Send a typed `Terminate` for a tracked job.
    ///
    /// The terminal `Exit` frame still arrives through [`Self::wait_terminal`].
    ///
    /// # Errors
    ///
    /// `Unavailable` before `Spawned` delivered the generation, or a queue
    /// / connection failure.
    pub fn terminate(
        &mut self,
        job: u64,
        mode: ComponentTerminateMode,
        grace: Duration,
    ) -> Result<(), ComponentError> {
        let generation = self
            .jobs
            .get(&job)
            .and_then(|state| state.generation)
            .ok_or_else(|| ComponentError::Unavailable {
                reason: format!("job {job} has no execution generation yet"),
            })?;
        let grace_ms =
            u32::try_from(grace.as_millis()).map_err(|_| ComponentError::InvalidRequest {
                reason: "terminate grace exceeds 32 bits".to_owned(),
            })?;
        if grace_ms > bitty_execution::component::protocol::MAX_CANCEL_GRACE_MS {
            return Err(ComponentError::InvalidRequest {
                reason: "terminate grace exceeds the wire bound".to_owned(),
            });
        }
        self.send(Message::Terminate {
            job,
            generation,
            mode: wire_terminate_mode(mode),
            grace_ms,
        })
    }

    /// Read retained output for a tracked job.
    ///
    /// The request is bounded by the wire (240 KiB, 10 000 tail lines) and
    /// waits at most `timeout` for the matching `OutputView`.
    pub fn read_output(
        &mut self,
        job: u64,
        stream: ComponentStream,
        tail_lines: u32,
        max_bytes: u32,
        timeout: Duration,
    ) -> Result<ComponentOutput, ComponentError> {
        self.ensure_usable()?;
        if !self.jobs.contains_key(&job) {
            return Err(ComponentError::Unavailable {
                reason: format!("job {job} is not tracked"),
            });
        }
        if tail_lines > MAX_TAIL_LINES {
            return Err(ComponentError::InvalidRequest {
                reason: "tail_lines exceeds the wire bound".to_owned(),
            });
        }
        if max_bytes as usize > MAX_OUTPUT_READ_BYTES {
            return Err(ComponentError::InvalidRequest {
                reason: "max_bytes exceeds the wire bound".to_owned(),
            });
        }
        let request = self.mint_id()?;
        self.pending_read = Some(request);
        self.read_result = None;
        self.read_error = None;
        if let Err(error) = self.send(Message::ReadOutput {
            request,
            job,
            stream: wire_stream(stream),
            tail_lines,
            max_bytes,
        }) {
            self.pending_read = None;
            return Err(error);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            if self.pending_read.is_none() {
                if let Some(output) = self.read_result.take() {
                    return Ok(output);
                }
                if let Some(error) = self.read_error.take() {
                    return Err(error);
                }
            }
            if let Some(fault) = &self.fault {
                return Err(fault.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                self.pending_read = None;
                self.remember_abandoned_read(request);
                return Err(ComponentError::Timeout {
                    operation: "output read",
                });
            }
            let slice = (deadline - now).min(EXECUTION_COMPONENT_POLL_SLICE);
            match self.poll(slice) {
                Ok(()) => {}
                Err(ComponentError::Timeout { .. }) => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Spawn one job and wait for its terminal outcome, applying the linked
    /// path's wait/cancel graces.
    ///
    /// This is the synchronous convenience used by the execution-job bridge;
    /// it blocks only the caller's thread and never the readers.
    pub fn spawn_and_wait(
        &mut self,
        spec: &ComponentSpawnSpec,
    ) -> Result<ComponentRun, ComponentError> {
        if spec.hard_timeout_ms == 0 {
            return Err(ComponentError::InvalidRequest {
                reason: "spawn_and_wait requires a finite hard deadline".to_owned(),
            });
        }
        let job = self.spawn(spec)?;
        let hard = Duration::from_millis(u64::from(spec.hard_timeout_ms));
        let deadline = Instant::now()
            .checked_add(hard.saturating_add(EXECUTION_COMPONENT_WAIT_GRACE))
            .unwrap_or_else(Instant::now);
        match self.wait_terminal(job, deadline) {
            Ok(_) => self.take_run(job),
            Err(ComponentError::Timeout { .. }) => {
                if self
                    .jobs
                    .get(&job)
                    .is_some_and(|state| state.generation.is_none())
                {
                    self.abort_process();
                    self.jobs.remove(&job);
                    return Err(ComponentError::ComponentLost {
                        reason: "no Spawned reply before the job deadline".to_owned(),
                    });
                }
                if let Err(error) =
                    self.terminate(job, ComponentTerminateMode::Immediate, Duration::ZERO)
                {
                    self.abort_process();
                    self.jobs.remove(&job);
                    return Err(error);
                }
                let cancel_deadline = Instant::now()
                    .checked_add(EXECUTION_COMPONENT_CANCEL_GRACE)
                    .unwrap_or_else(Instant::now);
                match self.wait_terminal(job, cancel_deadline) {
                    Ok(_) => self.take_run(job),
                    Err(error) => {
                        self.abort_process();
                        self.jobs.remove(&job);
                        Err(match error {
                            ComponentError::Timeout { .. } => ComponentError::Unavailable {
                                reason: format!(
                                    "job {job} did not report a terminal outcome after terminate"
                                ),
                            },
                            other => other,
                        })
                    }
                }
            }
            Err(error) => {
                self.jobs.remove(&job);
                Err(error)
            }
        }
    }

    /// Send `Shutdown`, wait for `ShutdownAck`, then close stdin and reap the
    /// process within the configured deadline.
    ///
    /// Idempotent. A component that misses the deadline is killed and the
    /// call returns [`ComponentError::Timeout`] after the kill.
    pub fn shutdown(&mut self) -> Result<(), ComponentError> {
        if self.exited {
            return Ok(());
        }
        self.shutting_down = true;
        if self.fault.is_some() {
            self.abort_process();
            return Ok(());
        }
        let send_result = self.send(Message::Shutdown);
        let deadline = Instant::now()
            .checked_add(self.config.shutdown_timeout)
            .unwrap_or_else(Instant::now);
        while !self.shutdown_ack && !self.stdout_closed {
            if self.fault.is_some() || send_result.is_err() {
                self.abort_process();
                return send_result;
            }
            let now = Instant::now();
            if now >= deadline {
                self.abort_process();
                return Err(ComponentError::Timeout {
                    operation: "shutdown acknowledgement",
                });
            }
            let slice = (deadline - now).min(EXECUTION_COMPONENT_POLL_SLICE);
            match self.poll(slice) {
                Ok(()) => {}
                Err(ComponentError::Timeout { .. }) => {}
                Err(error) => {
                    self.abort_process();
                    return Err(error);
                }
            }
        }
        // Closing stdin lets the writer thread finish; the component exits
        // after the ack (or after a stdout close that prevents one).
        self.stdin_tx = None;
        if !self.wait_child_exit(deadline) {
            self.abort_process();
            return Err(ComponentError::Timeout {
                operation: "shutdown exit",
            });
        }
        if !self.shutdown_ack {
            return Err(ComponentError::ComponentLost {
                reason: "component closed stdout before ShutdownAck".to_owned(),
            });
        }
        Ok(())
    }

    /// Whether an explicit `ShutdownAck` was observed.
    #[must_use]
    pub fn shutdown_acknowledged(&self) -> bool {
        self.shutdown_ack
    }

    /// Remove one terminal job from the host table, returning its snapshot.
    ///
    /// Low-level [`Self::spawn`] callers own the job slots: read retained
    /// output first, then release the record so the bounded 64-job table can
    /// accept another spawn. Returns `None` for an unknown or still-running
    /// job (nothing is removed).
    pub fn release(&mut self, job: u64) -> Option<ComponentJobSnapshot> {
        let state = self.jobs.get(&job)?;
        state.exit?;
        let snapshot = state.snapshot(job);
        self.jobs.remove(&job);
        Some(snapshot)
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn handshake(&mut self) -> Result<String, ComponentError> {
        let deadline = Instant::now()
            .checked_add(self.config.handshake_timeout)
            .unwrap_or_else(Instant::now);
        let item = self.recv_deadline(deadline, "handshake")?;
        match item {
            Inbound::Message(Message::HelloAck {
                protocol,
                component,
                version,
            }) => {
                if protocol != PROTOCOL_VERSION
                    || !(EXECUTION_COMPONENT_PROTOCOL_MIN..=EXECUTION_COMPONENT_PROTOCOL_MAX)
                        .contains(&protocol)
                {
                    return Err(ComponentError::Incompatible {
                        min: protocol,
                        max: protocol,
                    });
                }
                if component != EXECUTION_COMPONENT_NAME {
                    return Err(ComponentError::Protocol {
                        reason: format!("component identified itself as '{component}'"),
                    });
                }
                Ok(version)
            }
            Inbound::Message(Message::Error { kind, message, .. }) => {
                Err(ComponentError::Unavailable {
                    reason: format!("handshake refused ({kind}): {message}"),
                })
            }
            Inbound::Message(other) => Err(ComponentError::Protocol {
                reason: format!("expected HelloAck, received tag 0x{:02x}", other.tag()),
            }),
            Inbound::Fault { protocol, reason } => Err(if protocol {
                ComponentError::Protocol { reason }
            } else {
                ComponentError::ComponentLost { reason }
            }),
            Inbound::Closed => Err(ComponentError::ComponentLost {
                reason: "component exited before HelloAck".to_owned(),
            }),
        }
    }

    fn recv_deadline(
        &mut self,
        deadline: Instant,
        operation: &'static str,
    ) -> Result<Inbound, ComponentError> {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(ComponentError::Timeout { operation });
        }
        match self.inbound.recv_timeout(deadline - now) {
            Ok(item) => Ok(item),
            Err(RecvTimeoutError::Timeout) => Err(ComponentError::Timeout { operation }),
            Err(RecvTimeoutError::Disconnected) => {
                Err(self.connection_lost("inbound channel closed".to_owned()))
            }
        }
    }

    fn handle_inbound(&mut self, item: Inbound) -> Result<(), ComponentError> {
        match item {
            Inbound::Message(message) => self.handle_message(message),
            Inbound::Fault { protocol, reason } => Err(if protocol {
                self.protocol_fault(reason)
            } else {
                self.connection_lost(reason)
            }),
            Inbound::Closed => {
                self.stdout_closed = true;
                if self.shutting_down || self.shutdown_ack {
                    Ok(())
                } else {
                    Err(self.connection_lost("component closed stdout".to_owned()))
                }
            }
        }
    }

    fn handle_message(&mut self, message: Message) -> Result<(), ComponentError> {
        match message {
            Message::HelloAck { .. } => Err(self.protocol_fault("unexpected HelloAck".to_owned())),
            Message::Spawned { job, generation } => {
                let Some(state) = self.jobs.get_mut(&job) else {
                    return Err(self.protocol_fault(format!("Spawned for unknown job {job}")));
                };
                if state.generation.replace(generation).is_some() {
                    return Err(self.protocol_fault(format!("duplicate Spawned for job {job}")));
                }
                Ok(())
            }
            Message::Started { job } => {
                let Some(state) = self.jobs.get_mut(&job) else {
                    return Err(self.protocol_fault(format!("Started for unknown job {job}")));
                };
                state.started = true;
                Ok(())
            }
            Message::Output { job, stream, data } => {
                if data.len() > MAX_OUTPUT_CHUNK_BYTES {
                    return Err(self.protocol_fault(format!(
                        "output chunk is {} bytes (limit {MAX_OUTPUT_CHUNK_BYTES})",
                        data.len()
                    )));
                }
                let Some(already_exited) = self.jobs.get(&job).map(|state| state.exit.is_some())
                else {
                    return Err(self.protocol_fault(format!("Output for unknown job {job}")));
                };
                if already_exited {
                    return Err(self.protocol_fault(format!("Output after Exit for job {job}")));
                }
                let Some(state) = self.jobs.get_mut(&job) else {
                    return Err(self.protocol_fault(format!("Output for unknown job {job}")));
                };
                match stream {
                    bitty_execution::component::protocol::Stream::Stdout => {
                        state.stdout.push(&data)
                    }
                    bitty_execution::component::protocol::Stream::Stderr => {
                        state.stderr.push(&data)
                    }
                }
                Ok(())
            }
            Message::Exit {
                job,
                outcome,
                detail,
                cancel,
                clock,
                kill_scope,
                oom,
                oom_gap: _,
                stdout_total,
                stdout_evicted,
                stderr_total,
                stderr_evicted,
            } => {
                let exit = convert_exit(
                    outcome,
                    detail,
                    cancel,
                    clock,
                    kill_scope,
                    oom,
                    stdout_total,
                    stdout_evicted,
                    stderr_total,
                    stderr_evicted,
                )
                .map_err(|error| self.protocol_fault(error))?;
                let (observed_stdout, observed_stderr, already_exited) = match self.jobs.get(&job) {
                    Some(state) => (state.stdout.total, state.stderr.total, state.exit.is_some()),
                    None => {
                        return Err(self.protocol_fault(format!("Exit for unknown job {job}")));
                    }
                };
                if already_exited {
                    return Err(self.protocol_fault(format!("duplicate Exit for job {job}")));
                }
                if exit.stdout_total < observed_stdout {
                    return Err(self.protocol_fault(format!(
                        "Exit stdout total {} is below the {observed_stdout} observed bytes",
                        exit.stdout_total
                    )));
                }
                if exit.stderr_total < observed_stderr {
                    return Err(self.protocol_fault(format!(
                        "Exit stderr total {} is below the {observed_stderr} observed bytes",
                        exit.stderr_total
                    )));
                }
                if let Some(state) = self.jobs.get_mut(&job) {
                    state.exit = Some(exit);
                }
                Ok(())
            }
            Message::Canceled { job, outcome } => {
                let Some(state) = self.jobs.get_mut(&job) else {
                    return Err(self.protocol_fault(format!("Canceled for unknown job {job}")));
                };
                state.canceled = Some(convert_terminate_outcome(outcome));
                Ok(())
            }
            Message::OutputView {
                request,
                job,
                stream: _,
                data,
                total,
                dropped,
                truncated,
                lines,
            } => {
                if self.take_abandoned_read(request) {
                    return Ok(());
                }
                if self.pending_read != Some(request) {
                    return Err(
                        self.protocol_fault(format!("OutputView for unexpected request {request}"))
                    );
                }
                if data.len() > MAX_OUTPUT_READ_BYTES {
                    return Err(self.protocol_fault(format!(
                        "OutputView is {} bytes (limit {MAX_OUTPUT_READ_BYTES})",
                        data.len()
                    )));
                }
                if total < data.len() as u64 {
                    return Err(self.protocol_fault(format!(
                        "OutputView total {total} is below its {} byte payload",
                        data.len()
                    )));
                }
                if !self.jobs.contains_key(&job) {
                    return Err(self.protocol_fault(format!("OutputView for unknown job {job}")));
                }
                self.pending_read = None;
                self.read_result = Some(ComponentOutput {
                    data,
                    total,
                    dropped,
                    truncated,
                    lines,
                });
                Ok(())
            }
            Message::Error {
                request,
                kind,
                message,
            } => self.handle_error_frame(request, kind, message),
            Message::ShutdownAck => {
                self.shutdown_ack = true;
                Ok(())
            }
            Message::Hello { .. }
            | Message::Spawn { .. }
            | Message::Terminate { .. }
            | Message::ReadOutput { .. }
            | Message::Shutdown => Err(self.protocol_fault(format!(
                "component sent a host-only message (tag 0x{:02x})",
                message.tag()
            ))),
        }
    }

    fn handle_error_frame(
        &mut self,
        request: u64,
        kind: ErrorKind,
        message: String,
    ) -> Result<(), ComponentError> {
        if self.take_abandoned_read(request) {
            return Ok(());
        }
        let error = match kind {
            ErrorKind::InvalidRequest | ErrorKind::Budget | ErrorKind::Unsupported => {
                ComponentError::InvalidRequest {
                    reason: format!("{}: {message}", kind.as_str()),
                }
            }
            ErrorKind::Protocol => ComponentError::Protocol {
                reason: format!("{}: {message}", kind.as_str()),
            },
            ErrorKind::ComponentLost => ComponentError::ComponentLost { reason: message },
            ErrorKind::Timeout => ComponentError::Timeout {
                operation: "component request",
            },
            ErrorKind::UnknownJob | ErrorKind::StaleGeneration => ComponentError::Unavailable {
                reason: format!("{}: {message}", kind.as_str()),
            },
            ErrorKind::Denied | ErrorKind::Internal => ComponentError::Unavailable {
                reason: format!("{}: {message}", kind.as_str()),
            },
        };
        if request == CONNECTION_ID {
            return Err(self.fault_from_error(error));
        }
        if self.pending_read == Some(request) {
            self.pending_read = None;
            self.read_error = Some(error);
            return Ok(());
        }
        if let Some(state) = self.jobs.get_mut(&request) {
            state.error = Some(error);
            return Ok(());
        }
        Err(self.protocol_fault(format!("error frame for unknown request {request}")))
    }

    fn take_run(&mut self, job: u64) -> Result<ComponentRun, ComponentError> {
        let state = self
            .jobs
            .remove(&job)
            .ok_or_else(|| ComponentError::Unavailable {
                reason: format!("job {job} is no longer tracked"),
            })?;
        let exit = state.exit.ok_or_else(|| ComponentError::Unavailable {
            reason: format!("job {job} has no terminal outcome"),
        })?;
        let stdout_total = exit.stdout_total.max(state.stdout.total);
        let stderr_total = exit.stderr_total.max(state.stderr.total);
        Ok(ComponentRun {
            job,
            exit,
            stdout: state.stdout.text(),
            stderr: state.stderr.text(),
            stdout_total,
            stderr_total,
            stdout_truncated: exit.stdout_evicted > 0
                || state.stdout.dropped > 0
                || exit.stdout_total > state.stdout.total,
            stderr_truncated: exit.stderr_evicted > 0
                || state.stderr.dropped > 0
                || exit.stderr_total > state.stderr.total,
        })
    }

    /// Record a read request whose caller has already timed out.
    ///
    /// The set is bounded; a late `OutputView`/`Error` for one of these ids
    /// is ignored instead of poisoning the whole connection. Once evicted,
    /// a very late reply is still a protocol fault, which is the fail-closed
    /// behavior.
    fn remember_abandoned_read(&mut self, request: u64) {
        const MAX_ABANDONED_READS: usize = 64;
        if self.abandoned_reads.len() >= MAX_ABANDONED_READS {
            self.abandoned_reads.pop_front();
        }
        self.abandoned_reads.push_back(request);
    }

    /// Remove and report whether `request` was an abandoned read.
    fn take_abandoned_read(&mut self, request: u64) -> bool {
        if let Some(position) = self
            .abandoned_reads
            .iter()
            .position(|candidate| *candidate == request)
        {
            self.abandoned_reads.remove(position);
            return true;
        }
        false
    }

    /// Mint one id for either a job or a read request.
    ///
    /// One monotonic counter covers both namespaces, so an `Error.request`
    /// value can never be ambiguous between a job id and a read id. The id
    /// is never reused for the component's lifetime.
    fn mint_id(&mut self) -> Result<u64, ComponentError> {
        loop {
            let id = self.next_id;
            self.next_id =
                self.next_id
                    .checked_add(1)
                    .ok_or_else(|| ComponentError::Unavailable {
                        reason: "id space exhausted".to_owned(),
                    })?;
            if id != 0 && !self.jobs.contains_key(&id) && Some(id) != self.pending_read {
                return Ok(id);
            }
        }
    }

    fn ensure_usable(&mut self) -> Result<(), ComponentError> {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }
        if self.exited || self.shutting_down {
            return Err(ComponentError::ComponentLost {
                reason: "component is not accepting jobs".to_owned(),
            });
        }
        self.refresh_process_state();
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }
        Ok(())
    }

    fn send(&mut self, message: Message) -> Result<(), ComponentError> {
        let Some(tx) = self.stdin_tx.clone() else {
            return Err(ComponentError::ComponentLost {
                reason: "component stdin is closed".to_owned(),
            });
        };
        match tx.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(ComponentError::Unavailable {
                reason: "component request queue is full".to_owned(),
            }),
            Err(TrySendError::Disconnected(_)) => {
                Err(self.connection_lost("stdin writer stopped".to_owned()))
            }
        }
    }

    fn refresh_process_state(&mut self) {
        if self.exited {
            return;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.exited = true;
                if !self.shutting_down && self.fault.is_none() {
                    self.fault = Some(ComponentError::ComponentLost {
                        reason: "component process exited".to_owned(),
                    });
                }
            }
            Ok(None) => {}
            Err(error) => {
                if !self.shutting_down && self.fault.is_none() {
                    self.fault = Some(ComponentError::Io {
                        operation: "wait",
                        kind: error.kind(),
                    });
                }
            }
        }
    }

    fn protocol_fault(&mut self, reason: String) -> ComponentError {
        let error = ComponentError::Protocol { reason };
        self.fault.get_or_insert_with(|| error.clone());
        self.abort_process();
        error
    }

    fn connection_lost(&mut self, reason: String) -> ComponentError {
        let error = ComponentError::ComponentLost { reason };
        self.fault.get_or_insert_with(|| error.clone());
        self.abort_process();
        error
    }

    fn fault_from_error(&mut self, error: ComponentError) -> ComponentError {
        self.fault.get_or_insert_with(|| error.clone());
        self.abort_process();
        error
    }

    fn wait_child_exit(&mut self, deadline: Instant) -> bool {
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.exited = true;
                    return true;
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
        false
    }

    fn abort_process(&mut self) {
        self.stdin_tx = None;
        if self.exited {
            return;
        }
        let _ = self.child.kill();
        #[cfg(unix)]
        if self.process_group {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;
            if let Ok(pid) = i32::try_from(self.pid) {
                let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
        let _ = self.child.wait();
        self.exited = true;
    }
}

impl Drop for ComponentExecutionHost {
    fn drop(&mut self) {
        self.abort_process();
        // The reader/writer/stderr threads observe the closed pipes and end;
        // joining them here would be unbounded, so detach them instead.
        self.reader.take();
        self.writer.take();
        self.stderr_reader.take();
    }
}

// ── thread loops ────────────────────────────────────────────────────────────

type IoThreads = (JoinHandle<()>, JoinHandle<()>, JoinHandle<()>);

/// Starts the stdout reader, stdin writer, and stderr reader.
///
/// The three threads hold no policy: they translate bytes to bounded channel
/// messages and end when either pipe closes.
fn spawn_component_threads(
    stdout: ChildStdout,
    inbound: SyncSender<Inbound>,
    stdin: ChildStdin,
    outbound: Receiver<Message>,
    stderr: ChildStderr,
    stderr_ring: Arc<Mutex<StderrRing>>,
) -> io::Result<IoThreads> {
    let reader = thread::Builder::new()
        .name("bitty-execution-host-reader".to_owned())
        .spawn(move || {
            let mut reader = FrameReader::new(stdout);
            loop {
                match reader.read_message() {
                    Ok(Some(message)) => {
                        if inbound.send(Inbound::Message(message)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = inbound.send(Inbound::Closed);
                        break;
                    }
                    Err(error) => {
                        let (protocol, reason) = match &error {
                            bitty_execution::component::protocol::WireError::Io(kind) => {
                                (false, format!("stdout read failed: {kind}"))
                            }
                            other => (true, other.to_string()),
                        };
                        let _ = inbound.send(Inbound::Fault { protocol, reason });
                        break;
                    }
                }
            }
        })?;
    let writer = thread::Builder::new()
        .name("bitty-execution-host-writer".to_owned())
        .spawn(move || {
            let mut stdin = stdin;
            while let Ok(message) = outbound.recv() {
                if bitty_execution::component::protocol::write_frame(&mut stdin, &message).is_err()
                {
                    break;
                }
            }
            let _ = stdin.flush();
        })?;
    let stderr_reader = thread::Builder::new()
        .name("bitty-execution-host-stderr".to_owned())
        .spawn(move || {
            let mut stderr = stderr;
            let mut buffer = [0u8; 4096];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let mut ring = stderr_ring.lock().unwrap_or_else(PoisonError::into_inner);
                        ring.push(&buffer[..count]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })?;
    Ok((reader, writer, stderr_reader))
}

// ── mapping ─────────────────────────────────────────────────────────────────

fn map_resolve_error(error: ResolveError) -> ComponentError {
    match error {
        ResolveError::NotInstalled { name } => ComponentError::NotInstalled { name },
        ResolveError::NoRoot => ComponentError::NotInstalled {
            name: EXECUTION_COMPONENT_NAME.to_owned(),
        },
        ResolveError::IncompatibleProtocol { min, max } => {
            ComponentError::Incompatible { min, max }
        }
        other => ComponentError::UnsafeInstall {
            reason: other.to_string(),
        },
    }
}

fn wire_kind(kind: ComponentSpawnKind) -> bitty_execution::component::protocol::SpawnKind {
    match kind {
        ComponentSpawnKind::Command => bitty_execution::component::protocol::SpawnKind::Command,
        ComponentSpawnKind::Service => bitty_execution::component::protocol::SpawnKind::Service,
        ComponentSpawnKind::Watch => bitty_execution::component::protocol::SpawnKind::Watch,
    }
}

fn wire_stream(stream: ComponentStream) -> bitty_execution::component::protocol::Stream {
    match stream {
        ComponentStream::Stdout => bitty_execution::component::protocol::Stream::Stdout,
        ComponentStream::Stderr => bitty_execution::component::protocol::Stream::Stderr,
    }
}

fn wire_terminate_mode(mode: ComponentTerminateMode) -> TerminateMode {
    match mode {
        ComponentTerminateMode::Graceful => TerminateMode::Graceful,
        ComponentTerminateMode::Immediate => TerminateMode::Immediate,
        ComponentTerminateMode::GracefulThenKill => TerminateMode::GracefulThenKill,
    }
}

fn convert_terminate_outcome(outcome: TerminateOutcome) -> ComponentTerminateOutcome {
    match outcome {
        TerminateOutcome::CancelledBeforeStart => ComponentTerminateOutcome::CancelledBeforeStart,
        TerminateOutcome::CancelledGracefully => ComponentTerminateOutcome::CancelledGracefully,
        TerminateOutcome::Killed => ComponentTerminateOutcome::Killed,
        TerminateOutcome::AlreadyExited => ComponentTerminateOutcome::AlreadyExited,
        TerminateOutcome::PermissionDenied => ComponentTerminateOutcome::PermissionDenied,
        TerminateOutcome::StaleGeneration => ComponentTerminateOutcome::StaleGeneration,
        TerminateOutcome::StillRunning => ComponentTerminateOutcome::StillRunning,
        TerminateOutcome::Unsupported => ComponentTerminateOutcome::Unsupported,
        TerminateOutcome::Unknown => ComponentTerminateOutcome::Unknown,
    }
}

#[allow(clippy::too_many_arguments)]
fn convert_exit(
    outcome: ExitKind,
    detail: i32,
    cancel: WireCancelEffect,
    clock: WireDeadlineClock,
    kill_scope: WireKillScope,
    oom: WireOomVerdict,
    stdout_total: u64,
    stdout_evicted: u64,
    stderr_total: u64,
    stderr_evicted: u64,
) -> Result<ComponentExit, String> {
    if stdout_evicted > stdout_total || stderr_evicted > stderr_total {
        return Err("evicted bytes exceed the stream total".to_owned());
    }
    let kind = match outcome {
        ExitKind::Success => {
            if detail != 0 {
                return Err("success carries a non-zero detail".to_owned());
            }
            ComponentExitKind::Success
        }
        ExitKind::ExitCode => {
            if detail == 0 {
                return Err("exit_code carries a zero status".to_owned());
            }
            ComponentExitKind::ExitCode(detail)
        }
        ExitKind::Signaled => {
            if detail <= 0 {
                return Err("signaled carries a non-positive signal".to_owned());
            }
            ComponentExitKind::Signaled(detail)
        }
        ExitKind::SpawnFailed => ComponentExitKind::SpawnFailed,
        ExitKind::Cancelled => ComponentExitKind::Cancelled(match cancel {
            WireCancelEffect::BeforeStart => ComponentCancelEffect::BeforeStart,
            WireCancelEffect::Graceful => ComponentCancelEffect::Graceful,
            WireCancelEffect::Killed => ComponentCancelEffect::Killed,
            WireCancelEffect::None => {
                return Err("cancelled exit carries no cancel effect".to_owned());
            }
        }),
        ExitKind::TimedOut => ComponentExitKind::TimedOut(match clock {
            WireDeadlineClock::Hard => ComponentDeadlineClock::Hard,
            WireDeadlineClock::Idle => ComponentDeadlineClock::Idle,
            WireDeadlineClock::None => {
                return Err("timed-out exit carries no deadline clock".to_owned());
            }
        }),
        ExitKind::OomKilled => {
            if oom != WireOomVerdict::OomKilled {
                return Err("oom-killed exit lacks OOM evidence".to_owned());
            }
            ComponentExitKind::OomKilled
        }
        ExitKind::SupervisorLost => ComponentExitKind::SupervisorLost,
        ExitKind::Unknown => ComponentExitKind::Unknown,
    };
    if !matches!(kind, ComponentExitKind::Cancelled(_)) && cancel != WireCancelEffect::None {
        return Err("non-cancelled exit carries a cancel effect".to_owned());
    }
    if !matches!(kind, ComponentExitKind::TimedOut(_)) && clock != WireDeadlineClock::None {
        return Err("non-timeout exit carries a deadline clock".to_owned());
    }
    Ok(ComponentExit {
        kind,
        kill_scope: match kill_scope {
            WireKillScope::DirectChild => ComponentKillScope::DirectChild,
            WireKillScope::OwnedTree => ComponentKillScope::OwnedTree,
        },
        oom: match oom {
            WireOomVerdict::Unknown => ComponentOomVerdict::Unknown,
            WireOomVerdict::OomKilled => ComponentOomVerdict::OomKilled,
            WireOomVerdict::NotOom => ComponentOomVerdict::NotOom,
        },
        stdout_total,
        stdout_evicted,
        stderr_total,
        stderr_evicted,
    })
}
