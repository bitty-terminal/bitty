//! CTX-0906 (#1577, DIR-030): native component broker against a real
//! coprocess.
//!
//! The component is the in-repo `bitty-component-fixture` binary, installed
//! into a scratch component root as `bitty-<name>` with a generated
//! descriptor. It speaks wire protocol v1 and never touches the network, so
//! these tests are hermetic on every platform. Policy time is injected
//! (`now`), so backoff and idle behavior need no real waiting beyond process
//! start and exit.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bitty_network_wire::{ErrorKind, Method};
use bitty_plugin_host::capability::CapabilityId;
use bitty_plugin_host::manifest::NetworkEgress;
use bitty_runtime::component::{
    BrokerConfig, BrokerError, BrokerEvent, BrokerEventKind, COMPONENT_HANDSHAKE_TIMEOUT,
    COMPONENT_IDLE_TIMEOUT, COMPONENT_MAX_IN_FLIGHT, COMPONENT_SHUTDOWN_GRACE, ComponentBroker,
    ComponentEnv, ComponentRequest, ComponentState, PluginGrant, RequestId, ResolveError,
    StopOutcome, executable_file_name,
};

/// Real-time bound for any single wait on the fixture process.
const REAL_WAIT: Duration = Duration::from_secs(20);
/// One blocking wait slice.
const WAIT_SLICE: Duration = Duration::from_millis(100);
const FIXTURE: &str = env!("CARGO_BIN_EXE_bitty-component-fixture");
const NAME: &str = "fixture";
const VERSION: &str = "0.0.1";
const PLUGIN: &str = "acme.component-test";
const URL: &str = "https://api.example.com/v1/ping";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bitty-ctx0906-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch root");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Install the fixture as component `NAME` in `mode`; returns the
/// executable path.
fn install(root: &Path, mode: &str) -> PathBuf {
    let version_dir = root.join(NAME).join(VERSION);
    std::fs::create_dir_all(&version_dir).expect("version dir");
    std::fs::write(root.join(NAME).join("current"), format!("{VERSION}\n")).expect("current");
    let executable = version_dir.join(executable_file_name(&format!("bitty-{NAME}")));
    std::fs::copy(FIXTURE, &executable).expect("copy fixture");
    let bytes = std::fs::read(&executable).expect("read fixture");
    let digest = bitty_package::integrity::sha256_hex(&bytes);
    write_descriptor(root, &digest);
    std::fs::write(version_dir.join("fixture-mode"), mode).expect("mode");
    executable
}

fn write_descriptor(root: &Path, digest: &str) {
    let text = format!(
        "[component]\nname = \"{NAME}\"\nversion = \"{VERSION}\"\nprotocol = [1, 1]\nexecutable = \"bitty-{NAME}\"\nsha256 = \"{digest}\"\n"
    );
    std::fs::write(
        root.join(NAME).join(VERSION).join("bitty-component.toml"),
        text,
    )
    .expect("descriptor");
}

fn broker(root: &Path, env: ComponentEnv) -> ComponentBroker {
    ComponentBroker::new(BrokerConfig::new(Some(root.to_owned()), env))
}

fn grant() -> PluginGrant {
    let caps = [CapabilityId::parse("network.connect:api.example.com").expect("cap")];
    let egress = [NetworkEgress {
        host: "api.example.com".into(),
        ports: vec![443],
    }];
    PluginGrant::compute(PLUGIN, &caps, &egress).expect("grant")
}

fn request() -> ComponentRequest {
    ComponentRequest::new(Method::Get, URL)
}

/// Wait (real time, bounded) until `id` reaches a terminal event.
fn until_terminal(broker: &mut ComponentBroker, now: Instant, id: RequestId) -> Vec<BrokerEvent> {
    let deadline = Instant::now() + REAL_WAIT;
    let mut events = Vec::new();
    while Instant::now() < deadline {
        for event in broker.wait(now, WAIT_SLICE) {
            let done = event.id == id && event.is_terminal();
            events.push(event);
            if done {
                return events;
            }
        }
    }
    panic!("request {id} did not finish: {events:?}");
}

/// Wait (real time, bounded) until `component` reports `state`.
fn until_state(broker: &mut ComponentBroker, now: Instant, state: ComponentState) {
    let deadline = Instant::now() + REAL_WAIT;
    while Instant::now() < deadline {
        let _ = broker.wait(now, WAIT_SLICE);
        if broker.status(NAME).map(|status| status.state) == Some(state) {
            return;
        }
    }
    panic!(
        "component never reached {state:?}: {:?}",
        broker.status(NAME)
    );
}

fn failed_kind(events: &[BrokerEvent], id: RequestId) -> Option<ErrorKind> {
    events.iter().find_map(|event| match &event.kind {
        BrokerEventKind::Failed { kind, .. } if event.id == id => Some(*kind),
        _ => None,
    })
}

#[test]
fn echo_round_trip_carries_grant_attribution_and_cleared_env() {
    let scratch = Scratch::new("echo");
    install(&scratch.0, "echo");
    let host: BTreeMap<&str, &str> = [
        ("LANG", "C.UTF-8"),
        ("PATH", "fixture-path"),
        ("FIXTURE_SECRET_TOKEN", "do-not-forward"),
    ]
    .into_iter()
    .collect();
    let env = ComponentEnv::capture(|name| host.get(name).map(OsString::from));
    let mut broker = broker(&scratch.0, env);
    let now = Instant::now();
    let id = broker
        .submit(now, NAME, &grant(), request())
        .expect("submit");
    let events = until_terminal(&mut broker, now, id);

    let head = events
        .iter()
        .find_map(|event| match &event.kind {
            BrokerEventKind::Head { status, headers } => Some((*status, headers.clone())),
            _ => None,
        })
        .expect("head");
    assert_eq!(head.0, 200);
    let headers: BTreeMap<String, String> = head.1.into_iter().collect();
    assert_eq!(headers["x-plugin-id"], PLUGIN);
    assert_eq!(headers["x-grant"], "api.example.com:443");
    assert_eq!(headers["x-cwd-mode"], "version-dir");
    let env_names: Vec<&str> = headers["x-env"]
        .split(',')
        .filter(|n| !n.is_empty())
        .collect();
    assert!(env_names.contains(&"LANG"), "{env_names:?}");
    assert!(
        !env_names
            .iter()
            .any(|n| n.eq_ignore_ascii_case("PATH") || n.contains("SECRET")),
        "{env_names:?}"
    );
    let body: Vec<u8> = events
        .iter()
        .filter_map(|event| match &event.kind {
            BrokerEventKind::Body { data, .. } => Some(data.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(body, URL.as_bytes());
    assert!(
        events
            .iter()
            .all(|event| event.plugin_id == PLUGIN && event.component == NAME)
    );

    let status = broker.status(NAME).expect("status");
    assert_eq!(status.state, ComponentState::Running);
    assert_eq!(status.negotiated, Some((1, "0.0.1".to_owned())));
    assert_eq!(status.spawns, 1);
    assert!(status.pid.is_some());
    assert!(broker.stderr_tail(NAME).is_some());
    let _ = broker.shutdown();
}

#[test]
fn missing_component_fails_closed() {
    let scratch = Scratch::new("missing");
    let mut broker = broker(&scratch.0, ComponentEnv::empty());
    let error = broker
        .submit(Instant::now(), NAME, &grant(), request())
        .expect_err("not installed");
    assert_eq!(
        error,
        BrokerError::Resolve(ResolveError::NotInstalled { name: NAME.into() })
    );
    let mut no_root = ComponentBroker::new(BrokerConfig::new(None, ComponentEnv::empty()));
    assert_eq!(
        no_root.submit(Instant::now(), NAME, &grant(), request()),
        Err(BrokerError::Resolve(ResolveError::NoRoot))
    );
}

#[test]
fn digest_mismatch_fails_closed_before_every_spawn() {
    let scratch = Scratch::new("digest");
    install(&scratch.0, "echo");
    write_descriptor(&scratch.0, &"0".repeat(64));
    let mut broker = broker(&scratch.0, ComponentEnv::empty());
    assert!(matches!(
        broker.submit(Instant::now(), NAME, &grant(), request()),
        Err(BrokerError::Resolve(ResolveError::DigestMismatch { .. }))
    ));
    assert_eq!(broker.status(NAME).map(|status| status.spawns), Some(0));

    // A valid install spawns; tampering after an idle stop is caught at the
    // next spawn.
    let executable = install(&scratch.0, "echo");
    let t0 = Instant::now();
    let id = broker
        .submit(t0, NAME, &grant(), request())
        .expect("submit");
    until_terminal(&mut broker, t0, id);
    let idle = t0 + COMPONENT_IDLE_TIMEOUT;
    let _ = broker.poll(idle);
    until_state(&mut broker, idle, ComponentState::Stopped);
    let mut bytes = std::fs::read(&executable).expect("read");
    bytes.push(0);
    std::fs::write(&executable, bytes).expect("tamper");
    assert!(matches!(
        broker.submit(idle, NAME, &grant(), request()),
        Err(BrokerError::Resolve(ResolveError::DigestMismatch { .. }))
    ));
    assert_eq!(broker.status(NAME).map(|status| status.spawns), Some(1));
}

#[test]
fn idle_stop_closes_stdin_and_the_component_exits() {
    let scratch = Scratch::new("idle");
    install(&scratch.0, "echo");
    let mut broker = broker(&scratch.0, ComponentEnv::empty());
    let t0 = Instant::now();
    let id = broker
        .submit(t0, NAME, &grant(), request())
        .expect("submit");
    until_terminal(&mut broker, t0, id);
    // Not idle yet just before the timeout.
    let _ = broker.poll(t0 + COMPONENT_IDLE_TIMEOUT - Duration::from_secs(1));
    assert_eq!(
        broker.status(NAME).map(|s| s.state),
        Some(ComponentState::Running)
    );
    let idle = t0 + COMPONENT_IDLE_TIMEOUT;
    let _ = broker.poll(idle);
    until_state(&mut broker, idle, ComponentState::Stopped);
    assert_eq!(
        broker.status(NAME).and_then(|s| s.last_stop),
        Some(StopOutcome::Exited)
    );

    // The next request spawns a fresh process.
    let id = broker
        .submit(idle, NAME, &grant(), request())
        .expect("respawn");
    until_terminal(&mut broker, idle, id);
    assert_eq!(broker.status(NAME).map(|s| s.spawns), Some(2));
    let _ = broker.shutdown();
}

#[test]
fn component_ignoring_eof_is_killed_after_the_grace() {
    let scratch = Scratch::new("ignore-eof");
    install(&scratch.0, "ignore-eof");
    let mut broker = broker(&scratch.0, ComponentEnv::empty());
    let t0 = Instant::now();
    let id = broker
        .submit(t0, NAME, &grant(), request())
        .expect("submit");
    until_terminal(&mut broker, t0, id);
    let idle = t0 + COMPONENT_IDLE_TIMEOUT;
    let _ = broker.poll(idle);
    assert_eq!(
        broker.status(NAME).map(|s| s.state),
        Some(ComponentState::Stopping)
    );
    let _ = broker.poll(idle + COMPONENT_SHUTDOWN_GRACE);
    let status = broker.status(NAME).expect("status");
    assert_eq!(status.state, ComponentState::Stopped);
    assert_eq!(status.last_stop, Some(StopOutcome::Killed));
    assert_eq!(status.pid, None);
}

#[test]
fn crash_fails_in_flight_backs_off_and_latches_unavailable() {
    let scratch = Scratch::new("crash");
    install(&scratch.0, "crash");
    let mut broker = broker(&scratch.0, ComponentEnv::empty());
    let mut now = Instant::now();
    for crash in 1..=5u32 {
        let id = broker
            .submit(now, NAME, &grant(), request())
            .expect("submit");
        let events = until_terminal(&mut broker, now, id);
        assert_eq!(
            failed_kind(&events, id),
            Some(ErrorKind::ComponentLost),
            "crash {crash}"
        );
        if crash < 5 {
            // Backoff 1 s doubling: refused immediately, allowed after it.
            assert!(matches!(
                broker.submit(now, NAME, &grant(), request()),
                Err(BrokerError::Backoff { .. })
            ));
            now += Duration::from_secs(1u64 << (crash - 1));
        }
    }
    assert_eq!(
        broker.submit(now, NAME, &grant(), request()),
        Err(BrokerError::Unavailable)
    );
    let status = broker.status(NAME).expect("status");
    assert_eq!(status.state, ComponentState::Unavailable);
    assert_eq!(status.spawns, 5);
    assert_eq!(status.last_stop, Some(StopOutcome::Crashed));
}

#[test]
fn in_flight_cap_and_cancel() {
    let scratch = Scratch::new("hold");
    install(&scratch.0, "hold");
    let mut broker = broker(&scratch.0, ComponentEnv::empty());
    let now = Instant::now();
    let ids: Vec<RequestId> = (0..COMPONENT_MAX_IN_FLIGHT)
        .map(|_| {
            broker
                .submit(now, NAME, &grant(), request())
                .expect("submit")
        })
        .collect();
    assert_eq!(
        broker.submit(now, NAME, &grant(), request()),
        Err(BrokerError::Busy)
    );
    broker.cancel(ids[0]);
    let extra = broker
        .submit(now, NAME, &grant(), request())
        .expect("slot freed");
    assert_ne!(extra, ids[0]);
    until_state(&mut broker, now, ComponentState::Running);
    assert_eq!(
        broker.status(NAME).map(|s| s.in_flight),
        Some(COMPONENT_MAX_IN_FLIGHT)
    );
    // Nothing idles while requests are in flight.
    let _ = broker.poll(now + COMPONENT_IDLE_TIMEOUT * 2);
    assert_eq!(
        broker.status(NAME).map(|s| s.state),
        Some(ComponentState::Running)
    );
    // Shutdown fails every in-flight request with component_lost.
    let events = broker.shutdown();
    assert_eq!(events.len(), COMPONENT_MAX_IN_FLIGHT);
    assert!(events.iter().all(|event| matches!(
        event.kind,
        BrokerEventKind::Failed {
            kind: ErrorKind::ComponentLost,
            ..
        }
    )));
    assert_eq!(
        broker.status(NAME).map(|s| s.state),
        Some(ComponentState::Stopped)
    );
}

#[test]
fn handshake_version_mismatch_counts_as_crash() {
    let scratch = Scratch::new("bad-ack");
    install(&scratch.0, "bad-ack");
    let mut broker = broker(&scratch.0, ComponentEnv::empty());
    let now = Instant::now();
    let id = broker
        .submit(now, NAME, &grant(), request())
        .expect("submit");
    let events = until_terminal(&mut broker, now, id);
    assert_eq!(failed_kind(&events, id), Some(ErrorKind::ComponentLost));
    assert_eq!(
        broker.status(NAME).and_then(|s| s.last_stop),
        Some(StopOutcome::Crashed)
    );
}

#[test]
fn handshake_timeout_counts_as_crash() {
    let scratch = Scratch::new("mute");
    install(&scratch.0, "mute");
    let mut broker = broker(&scratch.0, ComponentEnv::empty());
    let t0 = Instant::now();
    let id = broker
        .submit(t0, NAME, &grant(), request())
        .expect("submit");
    assert!(
        broker
            .poll(t0 + COMPONENT_HANDSHAKE_TIMEOUT - Duration::from_millis(1))
            .is_empty()
    );
    assert_eq!(
        broker.status(NAME).map(|s| s.state),
        Some(ComponentState::Handshaking)
    );
    let events = broker.poll(t0 + COMPONENT_HANDSHAKE_TIMEOUT);
    assert_eq!(failed_kind(&events, id), Some(ErrorKind::ComponentLost));
    assert_eq!(
        broker.status(NAME).and_then(|s| s.last_stop),
        Some(StopOutcome::Crashed)
    );
}

#[test]
fn core_enforces_the_response_budget() {
    let scratch = Scratch::new("flood");
    install(&scratch.0, "flood");
    let mut broker = broker(&scratch.0, ComponentEnv::empty());
    let now = Instant::now();
    let mut req = request();
    req.max_body_bytes = 16;
    let id = broker.submit(now, NAME, &grant(), req).expect("submit");
    let events = until_terminal(&mut broker, now, id);
    assert_eq!(failed_kind(&events, id), Some(ErrorKind::Budget));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, BrokerEventKind::Body { .. }))
    );
    let _ = broker.shutdown();
}
