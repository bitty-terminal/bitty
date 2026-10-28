//! bitty#2494: Core host client and linked-supervisor parity.
//!
//! The component under test is the in-repo `bitty-execution-component-fixture`
//! binary, which delegates to the pinned extension's real component server.
//! The child is `bitty-execution-job-fixture`, so the linked registry and the
//! component supervise byte-identical commands. Everything is local and
//! hermetic; no network, shell, or ambient environment is involved.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bitty_execution::{
    CancelEffect, DeadlineClock, ExecutionOutcome, JobIo, JobKind, JobLifetime, JobRegistry,
    JobSpec, JobState, JobTimeouts, OutputStream, ReadOutput,
};
use bitty_ipc::execution::EnvPolicy;
use bitty_package::integrity::sha256_hex;
use bitty_runtime::component::{ComponentEnv, executable_file_name};
use bitty_runtime::execution_component::{
    ComponentEnvironment, ComponentError, ComponentExecutionHost, ComponentExitKind,
    ComponentHostConfig, ComponentSpawnSpec, ComponentStream, ComponentTerminateMode,
};

const COMPONENT_FIXTURE: &str = env!("CARGO_BIN_EXE_bitty-execution-component-fixture");
const JOB_FIXTURE: &str = env!("CARGO_BIN_EXE_bitty-execution-job-fixture");
const COMPONENT_VERSION: &str = "0.0.1";
const REAL_WAIT: Duration = Duration::from_secs(20);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx2494-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch root");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Install the component fixture into `root` with a valid descriptor and
/// optional fault mode.
fn install(root: &Path, mode: Option<&str>) -> PathBuf {
    let version_dir = root.join("execution").join(COMPONENT_VERSION);
    std::fs::create_dir_all(&version_dir).expect("version dir");
    std::fs::write(
        root.join("execution").join("current"),
        format!("{COMPONENT_VERSION}\n"),
    )
    .expect("current");
    let executable = version_dir.join(executable_file_name("bitty-execution"));
    std::fs::copy(COMPONENT_FIXTURE, &executable).expect("copy component fixture");
    let digest = sha256_hex(&std::fs::read(&executable).expect("read fixture"));
    write_descriptor(&version_dir, "execution", COMPONENT_VERSION, 2, 2, &digest);
    if let Some(mode) = mode {
        std::fs::write(version_dir.join("fixture-mode"), mode).expect("fixture mode");
    }
    executable
}

fn write_descriptor(
    version_dir: &Path,
    name: &str,
    version: &str,
    min: u16,
    max: u16,
    digest: &str,
) {
    let text = format!(
        "[component]\nname = \"{name}\"\nversion = \"{version}\"\nprotocol = [{min}, {max}]\nexecutable = \"bitty-{name}\"\nsha256 = \"{digest}\"\n"
    );
    std::fs::write(version_dir.join("bitty-component.toml"), text).expect("descriptor");
}

fn host_config(root: &Path) -> ComponentHostConfig {
    ComponentHostConfig {
        user_root: Some(root.to_path_buf()),
        system_root: None,
        env: ComponentEnv::empty(),
        core_version: "0.0.26-test".to_owned(),
        handshake_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(7),
        safe_mode: false,
    }
}

fn connect(root: &Path) -> ComponentExecutionHost {
    ComponentExecutionHost::connect(host_config(root)).expect("component host connects")
}

fn spec(mode: &str, timeout_ms: u32) -> ComponentSpawnSpec {
    let env = ComponentEnvironment::explicit(vec![(
        "BITTY_EXECUTION_JOB_MODE".to_owned(),
        mode.to_owned(),
    )])
    .expect("explicit env");
    ComponentSpawnSpec::new(JOB_FIXTURE.to_owned(), Vec::new())
        .with_env(env)
        .with_hard_timeout_ms(timeout_ms)
}

fn wait_for_started(host: &mut ComponentExecutionHost, job: u64) {
    let deadline = Instant::now() + REAL_WAIT;
    while Instant::now() < deadline {
        host.poll(Duration::from_millis(10)).expect("poll");
        if host
            .job(job)
            .is_some_and(|snapshot| snapshot.started || snapshot.exit.is_some())
        {
            return;
        }
    }
    panic!("job {job} never started");
}

fn shutdown(host: &mut ComponentExecutionHost) {
    host.shutdown().expect("component shutdown");
    assert!(host.shutdown_acknowledged(), "ShutdownAck must be observed");
    assert!(!host.is_running(), "component process must be reaped");
}

// ── linked baseline ────────────────────────────────────────────────────────

struct LinkedRun {
    outcome: ExecutionOutcome,
    stdout: String,
    stderr: String,
    stdout_total: u64,
    stderr_total: u64,
}

fn linked_run(mode: &str, timeout_ms: u64) -> LinkedRun {
    let registry = JobRegistry::new();
    let env = EnvPolicy::explicit(vec![(
        "BITTY_EXECUTION_JOB_MODE".to_owned(),
        mode.to_owned(),
    )])
    .expect("explicit env");
    let spec = JobSpec::new(JOB_FIXTURE.to_owned(), Vec::new())
        .with_env(env)
        .with_kind(JobKind::Command)
        .with_io(JobIo::Pipes)
        .with_lifetime(JobLifetime::Detached)
        .with_timeouts(
            JobTimeouts::default()
                .with_hard(Duration::from_millis(timeout_ms))
                .with_retention(Duration::ZERO),
        );
    let job = registry.spawn(spec).expect("linked spawn");
    let deadline = Instant::now() + REAL_WAIT;
    loop {
        let snapshot = registry.get(job).expect("linked snapshot");
        if snapshot.state.is_terminal() {
            break;
        }
        assert!(Instant::now() < deadline, "linked job never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    let snapshot = registry.get(job).expect("linked terminal snapshot");
    let outcome = snapshot.state.outcome().expect("linked outcome");
    let stdout = registry
        .read_output(job, ReadOutput::new(OutputStream::Stdout))
        .expect("linked stdout");
    let stderr = registry
        .read_output(job, ReadOutput::new(OutputStream::Stderr))
        .expect("linked stderr");
    LinkedRun {
        outcome,
        stdout: restore_trailing_newline(&stdout),
        stderr: restore_trailing_newline(&stderr),
        stdout_total: stdout.total_bytes,
        stderr_total: stderr.total_bytes,
    }
}

/// Mirrors `execution_jobs::restore_trailing_newline` for the retained view.
fn restore_trailing_newline(view: &bitty_execution::OutputView) -> String {
    let mut text = view.text.clone();
    if view.stored_bytes > 0 && text.len().saturating_add(1) == view.stored_bytes {
        text.push('\n');
    }
    text
}

/// Compares two large outputs without formatting both fully on failure.
fn assert_text_eq(label: &str, component: &str, linked: &str) {
    if component == linked {
        return;
    }
    let first_diff = component
        .bytes()
        .zip(linked.bytes())
        .position(|(left, right)| left != right);
    let window = |text: &str| {
        let mut start = first_diff.unwrap_or(0).saturating_sub(20);
        let mut end = (start + 80).min(text.len());
        while start > 0 && !text.is_char_boundary(start) {
            start -= 1;
        }
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        text[start..end].to_owned()
    };
    panic!(
        "{label}: component {} bytes, linked {} bytes, first diff {first_diff:?}; component window {:?}, linked window {:?}",
        component.len(),
        linked.len(),
        window(component),
        window(linked)
    );
}

// ── parity ─────────────────────────────────────────────────────────────────

#[test]
fn parity_stdout_stderr_and_exit_status() {
    let scratch = Scratch::new("parity-streams");
    install(scratch.path(), None);
    let mut host = connect(scratch.path());

    for mode in ["stdout-stderr", "exit-7"] {
        let linked = linked_run(mode, 10_000);
        let run = host
            .spawn_and_wait(&spec(mode, 10_000))
            .expect("component run");
        match (mode, linked.outcome, run.exit.kind) {
            ("stdout-stderr", ExecutionOutcome::Success, ComponentExitKind::Success) => {}
            ("exit-7", ExecutionOutcome::ExitCode(7), ComponentExitKind::ExitCode(7)) => {}
            (mode, linked, component) => {
                panic!("{mode}: linked {linked:?} != component {component:?}")
            }
        }
        assert_text_eq(&format!("{mode} stdout"), &run.stdout, &linked.stdout);
        assert_text_eq(&format!("{mode} stderr"), &run.stderr, &linked.stderr);
        assert_eq!(run.stdout_total, linked.stdout_total, "{mode} stdout bytes");
        assert_eq!(run.stderr_total, linked.stderr_total, "{mode} stderr bytes");
        assert!(!run.stdout_truncated && !run.stderr_truncated);
    }
    shutdown(&mut host);
}

#[test]
fn parity_newest_wins_output_and_trailing_newline() {
    let scratch = Scratch::new("parity-output");
    install(scratch.path(), None);
    let mut host = connect(scratch.path());

    for mode in ["newest", "newline-tail"] {
        let linked = linked_run(mode, 10_000);
        let run = host
            .spawn_and_wait(&spec(mode, 10_000))
            .expect("component run");
        assert_eq!(run.exit.kind, ComponentExitKind::Success, "{mode}");
        assert_text_eq(&format!("{mode} stdout tail"), &run.stdout, &linked.stdout);
        assert_eq!(run.stdout_total, linked.stdout_total, "{mode} stdout total");
        assert_eq!(run.stderr_total, linked.stderr_total, "{mode} stderr total");
    }
    shutdown(&mut host);
}

#[test]
fn parity_timeout_and_cancel() {
    let scratch = Scratch::new("parity-timeout");
    install(scratch.path(), None);
    let mut host = connect(scratch.path());

    // Timeout: the linked registry and the component both use the hard clock.
    let linked = linked_run("sleep", 300);
    assert_eq!(
        linked.outcome,
        ExecutionOutcome::TimedOut(DeadlineClock::Hard)
    );
    let run = host
        .spawn_and_wait(&spec("sleep", 300))
        .expect("component timeout");
    assert_eq!(
        run.exit.kind,
        ComponentExitKind::TimedOut(
            bitty_runtime::execution_component::ComponentDeadlineClock::Hard
        )
    );
    assert_eq!(run.stdout, "started\n");
    assert_eq!(run.stdout_total, 8);

    // Cancel: only after the process exists, so both paths exercise the
    // owned-tree kill rather than a pre-start cancel.
    let registry = JobRegistry::new();
    let env = EnvPolicy::explicit(vec![(
        "BITTY_EXECUTION_JOB_MODE".to_owned(),
        "sleep".to_owned(),
    )])
    .expect("env");
    let job = registry
        .spawn(
            JobSpec::new(JOB_FIXTURE.to_owned(), Vec::new())
                .with_env(env)
                .with_kind(JobKind::Command)
                .with_io(JobIo::Pipes)
                .with_lifetime(JobLifetime::Detached)
                .with_timeouts(
                    JobTimeouts::default()
                        .with_hard(Duration::from_secs(10))
                        .with_retention(Duration::ZERO),
                ),
        )
        .expect("linked cancel spawn");
    let deadline = Instant::now() + REAL_WAIT;
    loop {
        let snapshot = registry.get(job).expect("snapshot");
        if matches!(snapshot.state, JobState::Running) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "linked job never reached Running: {:?}",
            snapshot.state
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    registry.cancel(job).expect("linked cancel");
    let linked_cancel = wait_linked_outcome(&registry, job);
    assert!(
        matches!(linked_cancel, ExecutionOutcome::Cancelled(_)),
        "{linked_cancel:?}"
    );

    let component_job = host.spawn(&spec("sleep", 10_000)).expect("component spawn");
    wait_for_started(&mut host, component_job);
    host.terminate(
        component_job,
        ComponentTerminateMode::Immediate,
        Duration::ZERO,
    )
    .expect("component terminate");
    let terminal = host
        .wait_terminal(component_job, Instant::now() + REAL_WAIT)
        .expect("component terminal");
    let component_exit = terminal.exit.expect("exit");
    match component_exit.kind {
        ComponentExitKind::Cancelled(_) => {}
        other => panic!("expected component Cancelled, got {other:?}"),
    }
    assert!(matches!(
        linked_cancel,
        ExecutionOutcome::Cancelled(CancelEffect::Killed)
            | ExecutionOutcome::Cancelled(CancelEffect::Graceful)
            | ExecutionOutcome::Cancelled(CancelEffect::BeforeStart)
    ));
    shutdown(&mut host);
}

fn wait_linked_outcome(registry: &JobRegistry, job: bitty_execution::JobId) -> ExecutionOutcome {
    let deadline = Instant::now() + REAL_WAIT;
    loop {
        let snapshot = registry.get(job).expect("snapshot");
        if let Some(outcome) = snapshot.state.outcome() {
            return outcome;
        }
        assert!(Instant::now() < deadline, "linked cancel never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ── host behavior ──────────────────────────────────────────────────────────

#[test]
fn spawn_is_non_blocking_and_poll_does_not_wait_for_a_long_job() {
    let scratch = Scratch::new("nonblocking");
    install(scratch.path(), None);
    let mut host = connect(scratch.path());
    let job = host.spawn(&spec("sleep", 10_000)).expect("spawn");
    let started = Instant::now();
    host.poll(Duration::ZERO).expect("non-blocking poll");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "poll blocked for {elapsed:?} while a job was running"
    );
    wait_for_started(&mut host, job);
    host.terminate(job, ComponentTerminateMode::Immediate, Duration::ZERO)
        .expect("terminate");
    let _ = host
        .wait_terminal(job, Instant::now() + REAL_WAIT)
        .expect("terminal");
    shutdown(&mut host);
}

#[test]
fn read_output_returns_the_bounded_tail_and_totals() {
    let scratch = Scratch::new("read-output");
    install(scratch.path(), None);
    let mut host = connect(scratch.path());
    let job = host.spawn(&spec("stdout-stderr", 10_000)).expect("spawn");
    let _ = host
        .wait_terminal(job, Instant::now() + REAL_WAIT)
        .expect("terminal");
    let stdout = host
        .read_output(job, ComponentStream::Stdout, 0, 0, Duration::from_secs(2))
        .expect("stdout read");
    assert_eq!(stdout.data, b"out-line");
    assert_eq!(stdout.total, 9);
    assert_eq!(stdout.lines, 1);
    assert!(!stdout.truncated);
    let stderr = host
        .read_output(job, ComponentStream::Stderr, 0, 0, Duration::from_secs(2))
        .expect("stderr read");
    assert_eq!(stderr.data, b"err-line");
    assert_eq!(stderr.total, 9);
    let released = host.release(job).expect("release terminal job");
    assert!(released.exit.is_some());
    assert!(host.release(job).is_none(), "release is not repeatable");
    shutdown(&mut host);
}

#[test]
fn shutdown_terminates_live_jobs_and_reaps_the_component() {
    let scratch = Scratch::new("shutdown");
    install(scratch.path(), None);
    let mut host = connect(scratch.path());
    let job = host.spawn(&spec("sleep", 30_000)).expect("spawn");
    wait_for_started(&mut host, job);
    host.shutdown().expect("shutdown");
    assert!(host.shutdown_acknowledged());
    assert!(!host.is_running());
    assert!(host.job(job).is_some_and(|snapshot| snapshot.started));
}

#[test]
fn component_crash_during_spawn_is_reported_not_fatal() {
    let scratch = Scratch::new("crash");
    install(scratch.path(), Some("crash-on-spawn"));
    let mut host = connect(scratch.path());
    let error = host
        .spawn_and_wait(&spec("stdout-stderr", 5_000))
        .expect_err("crashed component must not appear successful");
    assert!(
        matches!(
            error,
            ComponentError::ComponentLost { .. } | ComponentError::Unavailable { .. }
        ),
        "{error:?}"
    );
    assert!(!host.is_running());
    // Shutdown remains idempotent on an already-dead component.
    host.shutdown().expect("shutdown after crash");
}

#[test]
fn malformed_oversized_zero_and_truncated_frames_fail_closed() {
    for mode in [
        "malformed-frame",
        "oversized-frame",
        "zero-frame",
        "truncated-frame",
    ] {
        let scratch = Scratch::new(mode);
        install(scratch.path(), Some(mode));
        let mut host = ComponentExecutionHost::connect(host_config(scratch.path()))
            .unwrap_or_else(|error| panic!("{mode}: handshake failed: {error}"));
        let error = host
            .poll(Duration::from_secs(2))
            .expect_err(&format!("{mode} must be a protocol fault"));
        assert!(
            matches!(error, ComponentError::Protocol { .. }),
            "{mode}: {error:?}"
        );
        assert!(
            !host.is_running(),
            "{mode}: offending process must be killed"
        );
        host.shutdown().expect("shutdown after protocol fault");
    }
}

#[test]
fn silent_handshake_times_out_and_is_killed() {
    let scratch = Scratch::new("silent");
    install(scratch.path(), Some("silent-handshake"));
    let mut config = host_config(scratch.path());
    config.handshake_timeout = Duration::from_millis(250);
    let error =
        ComponentExecutionHost::connect(config).expect_err("silent handshake must time out");
    assert!(matches!(error, ComponentError::Timeout { .. }), "{error:?}");
}

#[test]
fn safe_mode_absent_and_incompatible_installs_fail_closed() {
    let scratch = Scratch::new("fail-closed");
    install(scratch.path(), None);

    let mut config = host_config(scratch.path());
    config.safe_mode = true;
    assert!(matches!(
        ComponentExecutionHost::connect(config),
        Err(ComponentError::SafeMode)
    ));

    let absent = Scratch::new("absent");
    std::fs::create_dir_all(absent.path()).expect("absent root");
    let mut config = host_config(absent.path());
    config.system_root = None;
    assert!(matches!(
        ComponentExecutionHost::connect(config),
        Err(ComponentError::NotInstalled { .. })
    ));

    let incompatible = Scratch::new("incompatible");
    install(incompatible.path(), None);
    let version_dir = incompatible
        .path()
        .join("execution")
        .join(COMPONENT_VERSION);
    let executable = version_dir.join(executable_file_name("bitty-execution"));
    let digest = sha256_hex(&std::fs::read(&executable).expect("read"));
    write_descriptor(&version_dir, "execution", COMPONENT_VERSION, 1, 1, &digest);
    assert!(matches!(
        ComponentExecutionHost::connect(host_config(incompatible.path())),
        Err(ComponentError::Incompatible { .. })
    ));

    let tampered = Scratch::new("tampered");
    install(tampered.path(), None);
    let version_dir = tampered.path().join("execution").join(COMPONENT_VERSION);
    let digest = sha256_hex(b"not the installed executable");
    write_descriptor(&version_dir, "execution", COMPONENT_VERSION, 2, 2, &digest);
    assert!(matches!(
        ComponentExecutionHost::connect(host_config(tampered.path())),
        Err(ComponentError::UnsafeInstall { .. })
    ));
}

#[test]
fn v2_family_seam_accepts_what_the_v1_seam_refuses() {
    let scratch = Scratch::new("family-seam");
    install(scratch.path(), None);
    let seam = bitty_runtime::component::seam_for(
        bitty_runtime::execution_component::EXECUTION_COMPONENT_NAME,
    );
    assert_eq!(
        (seam.protocol_min(), seam.protocol_max()),
        (
            bitty_runtime::execution_component::EXECUTION_COMPONENT_PROTOCOL_MIN,
            bitty_runtime::execution_component::EXECUTION_COMPONENT_PROTOCOL_MAX,
        )
    );

    let error = bitty_runtime::component::resolve_search(
        Some(scratch.path()),
        None,
        bitty_runtime::execution_component::EXECUTION_COMPONENT_NAME,
    )
    .expect_err("the shared v1 broker seam must refuse the v2-only descriptor");
    assert!(
        matches!(
            error,
            bitty_runtime::component::ResolveError::IncompatibleProtocol { min: 2, max: 2, .. }
        ),
        "{error:?}"
    );

    let found = bitty_runtime::component::resolve_search_for_protocol(
        Some(scratch.path()),
        None,
        bitty_runtime::execution_component::EXECUTION_COMPONENT_NAME,
        seam.protocol_min(),
    )
    .expect("the execution family seam accepts the same install");
    assert_eq!(
        (
            found.resolved.descriptor.protocol_min,
            found.resolved.descriptor.protocol_max
        ),
        (2, 2)
    );
    // The fixture is the real v2 component: the seam also completes the
    // handshake that a v1 host cannot.
    let host = connect(scratch.path());
    drop(host);
}

#[test]
fn v2_handshake_refuses_a_v1_ack() {
    let scratch = Scratch::new("v1-ack");
    install(scratch.path(), Some("v1-ack"));
    let error = ComponentExecutionHost::connect(host_config(scratch.path()))
        .expect_err("a v1 HelloAck must be refused");
    assert!(matches!(
        error,
        ComponentError::Incompatible { min: 1, max: 1 }
    ));
}

#[test]
fn v2_typed_error_namespaces_route_job_and_read_failures() {
    let scratch = Scratch::new("job-error");
    install(scratch.path(), Some("job-error-on-spawn"));
    let mut host = connect(scratch.path());
    let error = host
        .spawn_and_wait(&spec("stdout-stderr", 5_000))
        .expect_err("job-correlated error must fail the submitted job");
    assert!(
        matches!(error, ComponentError::InvalidRequest { .. }),
        "{error:?}"
    );
    // The request-level error is terminal for this synthetic connection. Drop
    // performs the host-owned process-group/pipe cleanup; no fallback or
    // second mutation is attempted.
    drop(host);

    let scratch = Scratch::new("read-error");
    install(scratch.path(), Some("read-error"));
    let mut host = connect(scratch.path());
    let job = host.spawn(&spec("stdout-stderr", 5_000)).expect("spawn");
    host.wait_terminal(job, Instant::now() + REAL_WAIT)
        .expect("terminal");
    let error = host
        .read_output(job, ComponentStream::Stdout, 0, 0, Duration::from_secs(2))
        .expect_err("request-correlated error must fail only the read");
    assert!(
        matches!(error, ComponentError::InvalidRequest { .. }),
        "{error:?}"
    );
    // The synthetic server is intentionally only a namespace-routing probe;
    // drop it after the request-level result and exercise graceful shutdown
    // separately against the real server above.
    drop(host);
}

#[test]
fn connection_level_error_fails_the_host_connection() {
    let scratch = Scratch::new("connection-error");
    install(scratch.path(), Some("connection-error"));
    let mut host = connect(scratch.path());
    let error = host
        .poll(Duration::from_secs(2))
        .expect_err("request zero must be a connection-level failure");
    assert!(
        matches!(error, ComponentError::ComponentLost { .. }),
        "{error:?}"
    );
    assert!(!host.is_running(), "lost component must be reaped");
    host.shutdown().expect("shutdown after connection loss");
}

#[test]
fn spawn_bounds_are_enforced_before_the_wire() {
    let scratch = Scratch::new("bounds");
    install(scratch.path(), None);
    let mut host = connect(scratch.path());

    let mut too_many = spec("stdout-stderr", 5_000);
    too_many.args = vec!["x".to_owned(); 65];
    assert!(matches!(
        host.spawn(&too_many),
        Err(ComponentError::InvalidRequest { .. })
    ));

    let mut nul = spec("stdout-stderr", 5_000);
    nul.program.push('\0');
    assert!(matches!(
        host.spawn(&nul),
        Err(ComponentError::InvalidRequest { .. })
    ));

    let job = host.spawn(&spec("stdout-stderr", 5_000)).expect("spawn");
    let error = host
        .read_output(
            job,
            ComponentStream::Stdout,
            0,
            u32::try_from(240 * 1024 + 1).expect("fits"),
            Duration::from_millis(100),
        )
        .expect_err("over-bound read");
    assert!(matches!(error, ComponentError::InvalidRequest { .. }));
    let _ = host
        .wait_terminal(job, Instant::now() + REAL_WAIT)
        .expect("terminal");
    shutdown(&mut host);
}
