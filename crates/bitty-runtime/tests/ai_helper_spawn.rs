//! CTX-1253 (bitty #2123, DIR-030 S-1): Core-side resident-singleton helper
//! spawn path against a real coprocess.
//!
//! The helper is the in-repo `bitty-ai-helper-fixture` binary installed into a
//! scratch component root as `bitty-ai` with a generated `bitty-component.toml`
//! descriptor. It speaks the accepted bitty-ipc length-prefixed framing and
//! never touches the network or any AI build, so these tests are hermetic on
//! every platform. Policy time is injected (`now`), so backoff needs no real
//! waiting beyond process start and exit.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bitty_runtime::ai_helper::{
    HELPER_COMPONENT_NAME, HelperConfig, HelperError, HelperEvent, HelperState, ResidentHelper,
    StopOutcome,
};
use bitty_runtime::ai_request::AiRequestProducer;
use bitty_runtime::component::{
    COMPONENT_CRASH_LIMIT, COMPONENT_IDLE_TIMEOUT, ComponentEnv, ResolveError,
};

/// Real-time bound for any single wait on the fixture process.
const REAL_WAIT: Duration = Duration::from_secs(20);
/// One blocking wait slice.
const WAIT_SLICE: Duration = Duration::from_millis(50);
const FIXTURE: &str = env!("CARGO_BIN_EXE_bitty-ai-helper-fixture");
const NAME: &str = "ai";
const VERSION: &str = "0.0.1";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bitty-ctx1253-{tag}-{}", std::process::id()));
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

/// Install the fixture as component `NAME` in `mode`; returns the executable
/// path.
fn install(root: &Path, mode: &str) -> PathBuf {
    let version_dir = root.join(NAME).join(VERSION);
    std::fs::create_dir_all(&version_dir).expect("version dir");
    std::fs::write(root.join(NAME).join("current"), format!("{VERSION}\n")).expect("current");
    let executable = version_dir.join(executable_file_name());
    std::fs::copy(FIXTURE, &executable).expect("copy fixture");
    let bytes = std::fs::read(&executable).expect("read fixture");
    let digest = bitty_package::integrity::sha256_hex(&bytes);
    write_descriptor(root, &digest);
    std::fs::write(version_dir.join("fixture-mode"), mode).expect("mode");
    executable
}

fn executable_file_name() -> String {
    format!("bitty-{NAME}{}", std::env::consts::EXE_SUFFIX)
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

fn helper(root: &Path) -> ResidentHelper {
    ResidentHelper::new(HelperConfig::new(
        Some(root.to_owned()),
        ComponentEnv::empty(),
    ))
}

/// Pump `poll`/`wait` until an `Exited` event arrives (real-time bounded).
fn until_exited(helper: &mut ResidentHelper, now: Instant) -> Vec<HelperEvent> {
    let deadline = Instant::now() + REAL_WAIT;
    let mut events = Vec::new();
    while Instant::now() < deadline {
        for event in helper.wait(now, WAIT_SLICE) {
            let exited = matches!(event, HelperEvent::Exited(_));
            events.push(event);
            if exited {
                return events;
            }
        }
    }
    panic!("helper never exited: {events:?}");
}

/// Pump `poll`/`wait` until a `Frame` event arrives (real-time bounded).
fn until_frame(helper: &mut ResidentHelper, now: Instant) -> Vec<u8> {
    let deadline = Instant::now() + REAL_WAIT;
    while Instant::now() < deadline {
        for event in helper.wait(now, WAIT_SLICE) {
            if let HelperEvent::Frame(payload) = event {
                return payload;
            }
        }
    }
    panic!("helper never produced a frame");
}

#[test]
fn missing_component_fails_closed_as_not_installed() {
    let scratch = Scratch::new("missing");
    let mut helper = helper(&scratch.0);
    let error = helper
        .ensure_running(Instant::now())
        .expect_err("not installed");
    assert_eq!(
        error,
        HelperError::Resolve(ResolveError::NotInstalled {
            name: HELPER_COMPONENT_NAME.to_owned()
        })
    );
    assert_eq!(helper.status().state, HelperState::Stopped);
    assert_eq!(helper.status().spawns, 0);
}

#[test]
fn digest_mismatch_fails_closed_before_spawn() {
    let scratch = Scratch::new("digest");
    let executable = install(&scratch.0, "echo");
    // Tamper the executable after the descriptor was written: the pinned
    // digest no longer matches, so resolution must fail closed.
    std::fs::write(&executable, b"not the verified helper").expect("tamper");
    let mut helper = helper(&scratch.0);
    let error = helper
        .ensure_running(Instant::now())
        .expect_err("digest mismatch");
    assert!(
        matches!(
            error,
            HelperError::Resolve(ResolveError::DigestMismatch { .. })
        ),
        "{error:?}"
    );
    let status = helper.status();
    assert_eq!(status.state, HelperState::Stopped);
    assert_eq!(status.spawns, 0, "nothing may spawn on a digest mismatch");
    assert_eq!(status.pid, None);
}

#[test]
fn resolve_reports_the_verified_descriptor_version() {
    let scratch = Scratch::new("resolve");
    install(&scratch.0, "echo");
    let helper = helper(&scratch.0);
    let resolved = helper.resolve().expect("resolve");
    assert_eq!(resolved.descriptor.name, NAME);
    assert_eq!(resolved.descriptor.version, VERSION);
}

#[test]
fn resident_helper_spawns_and_round_trips_a_frame() {
    let scratch = Scratch::new("spawn");
    install(&scratch.0, "echo");
    let mut helper = helper(&scratch.0);
    let now = Instant::now();
    helper.ensure_running(now).expect("spawn");
    let status = helper.status();
    assert_eq!(status.state, HelperState::Running);
    assert!(status.pid.is_some(), "resident process has a pid");
    assert_eq!(status.version.as_deref(), Some(VERSION));
    assert_eq!(status.spawns, 1);

    let payload = br#"{"v":1,"id":1,"method":"ai.model_select"}"#;
    helper.submit_frame(payload).expect("submit");
    let echoed = until_frame(&mut helper, now);
    let mut expected = b"ai-fixture:".to_vec();
    expected.extend_from_slice(payload);
    assert_eq!(echoed, expected);

    // A graceful shutdown ends the resident process and reports it once.
    let events = helper.shutdown();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HelperEvent::Exited(StopOutcome::Exited))),
        "{events:?}"
    );
    assert_eq!(helper.status().state, HelperState::Stopped);
}

#[test]
fn request_producer_injects_snapshot_version_before_submit() {
    let scratch = Scratch::new("producer");
    install(&scratch.0, "echo");
    let mut helper = helper(&scratch.0);
    let now = Instant::now();
    helper.ensure_running(now).expect("spawn");

    let mut producer = AiRequestProducer::new();
    let publication = producer.publish_metadata();
    producer
        .acknowledge_local_metadata(&publication)
        .expect("initial metadata publication");
    producer
        .submit(&mut helper, 41, "ai.provider_list", br#"{"unknown":true}"#)
        .expect("submit through shared producer");
    let echoed = until_frame(&mut helper, now);
    let mut expected = b"ai-fixture:".to_vec();
    expected.extend_from_slice(
        br#"{"v":1,"id":41,"method":"ai.provider_list","params":{"unknown":true,"snapshot_version":0}}"#,
    );
    assert_eq!(echoed, expected);
    helper.shutdown();
}

#[test]
fn request_producer_submits_refreshed_and_invalidated_versions() {
    let scratch = Scratch::new("producer-version");
    install(&scratch.0, "echo");
    let mut helper = helper(&scratch.0);
    let now = Instant::now();
    helper.ensure_running(now).expect("spawn");

    let mut producer = AiRequestProducer::new();
    let metadata = bitty_runtime::ai_request::ServiceMetadata::try_new("provider", "v1", &[])
        .expect("metadata");
    producer
        .metadata_mut()
        .refresh(&[metadata])
        .expect("refresh");
    let publication = producer.publish_metadata();
    producer
        .acknowledge_local_metadata(&publication)
        .expect("refreshed metadata publication");
    producer
        .submit(&mut helper, 42, "ai.provider_list", b"{}")
        .expect("submit refreshed version");
    let refreshed = until_frame(&mut helper, now);
    assert!(refreshed.ends_with(
        br#"{"v":1,"id":42,"method":"ai.provider_list","params":{"snapshot_version":1}}"#
    ));

    producer.metadata_mut().invalidate();
    let publication = producer.publish_metadata();
    producer
        .acknowledge_local_metadata(&publication)
        .expect("invalidated metadata publication");
    producer
        .submit(&mut helper, 43, "ai.provider_list", b"{}")
        .expect("submit invalidated version");
    let invalidated = until_frame(&mut helper, now);
    assert!(invalidated.ends_with(
        br#"{"v":1,"id":43,"method":"ai.provider_list","params":{"snapshot_version":2}}"#
    ));
    helper.shutdown();
}

#[test]
fn resident_helper_is_not_idle_stopped() {
    let scratch = Scratch::new("resident");
    install(&scratch.0, "mute");
    let mut helper = helper(&scratch.0);
    let now = Instant::now();
    helper.ensure_running(now).expect("spawn");
    let pid = helper.status().pid;

    // Advance injected time far past the DIR-030 idle timeout with no
    // traffic: a resident singleton must keep running.
    let later = now + COMPONENT_IDLE_TIMEOUT * 10;
    let events = helper.poll(later);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HelperEvent::Exited(_))),
        "resident helper must not idle-stop: {events:?}"
    );
    assert_eq!(helper.status().state, HelperState::Running);
    assert_eq!(helper.status().pid, pid);
}

#[test]
fn crash_restarts_are_bounded_and_latch_unavailable() {
    let scratch = Scratch::new("crash");
    install(&scratch.0, "crash");
    let mut helper = helper(&scratch.0);
    let mut now = Instant::now();
    let mut crashes = 0usize;
    loop {
        match helper.ensure_running(now) {
            Ok(()) => {}
            Err(HelperError::Backoff { retry_after }) => {
                // Inside the crash delay: the supervisor refuses with a
                // typed remaining-delay, then succeeds once it elapses.
                assert!(retry_after > Duration::ZERO);
                now += retry_after;
                continue;
            }
            Err(HelperError::Unavailable) => break,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
        helper.submit_frame(b"trigger").expect("submit");
        let events = until_exited(&mut helper, now);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HelperEvent::Exited(StopOutcome::Crashed))),
            "expected a crash outcome: {events:?}"
        );
        crashes += 1;
        assert!(
            crashes <= COMPONENT_CRASH_LIMIT,
            "restarts must be bounded by the crash limit"
        );
    }
    assert_eq!(crashes, COMPONENT_CRASH_LIMIT, "latched after the limit");
    assert_eq!(helper.status().state, HelperState::Unavailable);
    assert_eq!(helper.status().spawns, COMPONENT_CRASH_LIMIT as u64);
    assert_eq!(
        helper.ensure_running(now).expect_err("latched"),
        HelperError::Unavailable
    );
}

#[test]
fn framing_fault_is_treated_as_a_crash() {
    let scratch = Scratch::new("framing");
    install(&scratch.0, "bad-frame");
    let mut helper = helper(&scratch.0);
    let now = Instant::now();
    helper.ensure_running(now).expect("spawn");
    // The fixture writes an over-bound length prefix; the framer refuses it
    // and the supervisor fails closed as a crash rather than buffering
    // unboundedly.
    let events = until_exited(&mut helper, now);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HelperEvent::Exited(StopOutcome::Crashed))),
        "{events:?}"
    );
}
