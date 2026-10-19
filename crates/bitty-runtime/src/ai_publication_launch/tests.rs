//! Fixture tests for the provider-publication launch seam.
//!
//! Every server is an in-process rustls peer over the launch's real
//! `socketpair(2)` control end. Nothing here opens a listener, resolves a
//! host, reads a host path, or spawns a helper binary. The bootstrap record
//! consumer is the same fixture thread, so the tests exercise the exact
//! handle handoff the spawn adapter will use.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bitty_network_publication::{CONTROL_BOOTSTRAP_MAX_BYTES, RANDOM_ID_BYTES};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error as RustlsError, ServerConfig,
    ServerConnection, SignatureScheme, SupportedProtocolVersion,
};
use zeroize::Zeroizing;

use super::{
    CHILD_BOOTSTRAP_DESCRIPTOR, CHILD_CONTROL_DESCRIPTOR, CONTROL_ALPN, CONTROL_ESTABLISH_TIMEOUT,
    CONTROL_PROFILE, DELEGATION_PURPOSE, LaunchError, LaunchIdentity, PreparedLaunch, TlsRefusal,
    prepare, write_and_zeroize,
};

const CORE_INSTANCE_NONCE: [u8; RANDOM_ID_BYTES] = [0x11; RANDOM_ID_BYTES];
const PLACEMENT_BOOTSTRAP: RawFd = CHILD_BOOTSTRAP_DESCRIPTOR as RawFd;
const PLACEMENT_CONTROL: RawFd = CHILD_CONTROL_DESCRIPTOR as RawFd;

fn identity(component_name: &str) -> LaunchIdentity {
    LaunchIdentity::new(
        component_name,
        "1.2.3",
        "bitty-ai",
        [0xab; RANDOM_ID_BYTES],
        7,
    )
}

fn prepared(component_name: &str) -> PreparedLaunch {
    prepare(identity(component_name), CORE_INSTANCE_NONCE).expect("launch preparation")
}

fn take_bootstrap_reader(prepared: &mut PreparedLaunch) -> File {
    let reader = prepared
        .handles
        .as_mut()
        .expect("platform handles")
        .take_bootstrap_reader()
        .expect("child bootstrap reader");
    File::from(reader)
}

fn take_control_child(prepared: &mut PreparedLaunch) -> UnixStream {
    prepared
        .handles
        .as_mut()
        .expect("platform handles")
        .take_control_child()
        .expect("child control stream")
}

fn reservation_descriptors(prepared: &PreparedLaunch) -> Vec<RawFd> {
    prepared
        .handles
        .as_ref()
        .expect("platform handles")
        .spawn_reservation_descriptors()
}

struct MaterialSnapshot {
    core_cert: CertificateDer<'static>,
    helper_cert: CertificateDer<'static>,
    helper_key: Vec<u8>,
}

fn snapshot(prepared: &PreparedLaunch) -> MaterialSnapshot {
    let material = prepared.material.as_ref().expect("launch material");
    MaterialSnapshot {
        core_cert: material.core_cert_der().clone(),
        helper_cert: material.helper_cert_der().clone(),
        helper_key: material.helper_key_der().to_vec(),
    }
}

#[test]
fn bootstrap_record_matches_the_documented_field_order() {
    let mut launch = prepared("bitty-ai");
    let mut bootstrap = take_bootstrap_reader(&mut launch);
    let material = snapshot(&launch);
    let expected_pin = launch.helper_cert_sha256().expect("material pin");

    launch.write_bootstrap().expect("bootstrap write");
    let mut encoded = Vec::new();
    bootstrap
        .read_to_end(&mut encoded)
        .expect("read bootstrap record");

    assert!(encoded.len() <= CONTROL_BOOTSTRAP_MAX_BYTES);
    let mut reader = RecordReader::new(&encoded);
    assert_eq!(reader.u16(), 1, "bootstrap format version");
    assert_eq!(reader.u16(), 1, "control protocol version");
    assert_eq!(reader.u8(), 1, "core-supervisor role");
    assert_eq!(reader.u8(), 2, "ai-helper role");
    assert_eq!(reader.take(RANDOM_ID_BYTES), CORE_INSTANCE_NONCE);
    assert_eq!(reader.take(RANDOM_ID_BYTES), launch.launch_nonce());
    assert_eq!(reader.len_prefixed(), b"bitty-ai");
    assert_eq!(reader.len_prefixed(), b"1.2.3");
    assert_eq!(reader.len_prefixed(), b"bitty-ai");
    assert_eq!(reader.take(32), [0xab; 32]);
    assert_eq!(reader.u16(), 7);
    assert_eq!(reader.take(RANDOM_ID_BYTES), launch.control_channel_nonce());
    assert_eq!(reader.take(RANDOM_ID_BYTES), launch.data_channel_nonce());
    assert_eq!(reader.len_prefixed(), DELEGATION_PURPOSE.as_bytes());
    assert_eq!(reader.len_prefixed(), CONTROL_PROFILE.as_bytes());
    assert_eq!(reader.u8(), 1, "launch profile");
    assert_eq!(reader.u8(), 1, "key type");
    assert_eq!(reader.len_prefixed(), material.core_cert.as_ref());
    assert_eq!(reader.len_prefixed(), material.helper_cert.as_ref());
    assert_eq!(reader.len_prefixed(), material.helper_key);
    assert!(reader.is_empty(), "no trailing bytes after the helper key");

    assert_eq!(
        launch.helper_cert_sha256().expect("material pin"),
        expected_pin
    );
}

#[test]
fn bootstrap_write_is_one_shot_and_closes_the_write_end() {
    let mut launch = prepared("bitty-ai");
    let mut bootstrap = take_bootstrap_reader(&mut launch);

    launch.write_bootstrap().expect("first write");
    assert!(matches!(
        launch.write_bootstrap(),
        Err(LaunchError::BootstrapAlreadyWritten)
    ));

    let mut encoded = Vec::new();
    bootstrap
        .read_to_end(&mut encoded)
        .expect("read bootstrap record");
    assert!(!encoded.is_empty());

    let mut extra = [0u8; 1];
    assert_eq!(
        bootstrap.read(&mut extra).expect("read after close"),
        0,
        "the write end must be closed"
    );
}

#[test]
fn establish_before_bootstrap_is_rejected() {
    let mut launch = prepared("bitty-ai");
    let error = launch
        .establish(Instant::now() + CONTROL_ESTABLISH_TIMEOUT)
        .expect_err("establishment before bootstrap must fail");
    assert!(matches!(error, LaunchError::BootstrapNotWritten));
}

#[test]
fn oversize_or_empty_identity_is_rejected_before_channels_exist() {
    let oversize = "n".repeat(129);
    for name in ["", oversize.as_str()] {
        let error = prepare(identity(name), CORE_INSTANCE_NONCE)
            .expect_err("invalid identity must be rejected");
        assert!(matches!(error, LaunchError::Manifest(_)), "{error:?}");
    }
}

#[test]
fn bootstrap_buffer_is_zeroized_on_success_and_on_failure() {
    let mut written = Zeroizing::new(vec![0x5a; 64]);
    let mut sink = Vec::new();
    write_and_zeroize(&mut sink, &mut written).expect("write");
    assert_eq!(sink, vec![0x5a; 64]);
    assert!(written.iter().all(|byte| *byte == 0));

    let mut failed = Zeroizing::new(vec![0x5a; 64]);
    let error = write_and_zeroize(&mut FailingWriter, &mut failed).expect_err("write refused");
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(failed.iter().all(|byte| *byte == 0));
}

#[test]
fn debug_output_does_not_expose_key_material() {
    let launch = prepared("bitty-ai");
    let material = snapshot(&launch);
    let helper_key_hex = hex(&material.helper_key);
    let core_pin_hex = hex(&material.core_cert);

    let rendered = format!("{launch:?}");
    assert!(!rendered.contains(&helper_key_hex));
    assert!(!rendered.contains(&core_pin_hex));
    assert!(!rendered.contains("PRIVATE KEY"));
}

#[test]
fn establishment_succeeds_against_the_in_process_fixture_server() {
    let mut launch = prepared("bitty-ai");
    let expected_pin = launch.helper_cert_sha256().expect("material pin");
    let bootstrap = take_bootstrap_reader(&mut launch);
    let control_child = take_control_child(&mut launch);
    let material = snapshot(&launch);
    launch.write_bootstrap().expect("bootstrap write");

    let server = spawn_fixture_server(bootstrap, control_child, material, false, CONTROL_ALPN);
    let established = launch
        .establish(Instant::now() + CONTROL_ESTABLISH_TIMEOUT)
        .expect("mutual TLS establishment");

    assert!(!established.is_handshaking());
    assert_eq!(established.alpn_protocol(), Some(CONTROL_ALPN));
    assert_eq!(established.helper_cert_sha256(), expected_pin);

    let outcome = server
        .join()
        .expect("server thread")
        .expect("server handshake");
    assert!(!outcome.handshaking);
    assert_eq!(outcome.alpn.as_deref(), Some(CONTROL_ALPN));
}

#[test]
fn wrong_helper_pin_is_refused() {
    let mut launch = prepared("bitty-ai");
    let bootstrap = take_bootstrap_reader(&mut launch);
    let control_child = take_control_child(&mut launch);
    let material = snapshot(&launch);
    launch.write_bootstrap().expect("bootstrap write");

    let server = spawn_fixture_server(bootstrap, control_child, material, false, CONTROL_ALPN);
    let error = launch
        .establish_with_expected_pin(Instant::now() + CONTROL_ESTABLISH_TIMEOUT, [0u8; 32])
        .expect_err("wrong helper pin must fail");
    assert!(
        matches!(error, LaunchError::TlsRefused(_)),
        "unexpected error: {error:?}"
    );
    let _ = server.join();
}

#[test]
fn missing_alpn_is_refused_after_the_handshake() {
    let mut launch = prepared("bitty-ai");
    let bootstrap = take_bootstrap_reader(&mut launch);
    let control_child = take_control_child(&mut launch);
    let material = snapshot(&launch);
    launch.write_bootstrap().expect("bootstrap write");

    let server = spawn_fixture_server(bootstrap, control_child, material, false, b"");
    let error = launch
        .establish(Instant::now() + CONTROL_ESTABLISH_TIMEOUT)
        .expect_err("missing ALPN must fail");
    assert!(
        matches!(error, LaunchError::TlsRefused(TlsRefusal::AlpnMismatch)),
        "unexpected error: {error:?}"
    );
    let outcome = server
        .join()
        .expect("server thread")
        .expect("server handshake");
    assert!(!outcome.handshaking);
    assert_eq!(outcome.alpn, None);
}

#[test]
fn tls12_only_server_is_refused() {
    let mut launch = prepared("bitty-ai");
    let bootstrap = take_bootstrap_reader(&mut launch);
    let control_child = take_control_child(&mut launch);
    let material = snapshot(&launch);
    launch.write_bootstrap().expect("bootstrap write");

    let server = spawn_fixture_server(bootstrap, control_child, material, true, CONTROL_ALPN);
    let error = launch
        .establish(Instant::now() + CONTROL_ESTABLISH_TIMEOUT)
        .expect_err("TLS 1.2-only server must fail");
    assert!(
        matches!(error, LaunchError::TlsRefused(_)),
        "unexpected error: {error:?}"
    );
    let _ = server.join();
}

#[test]
fn establishment_deadline_expires_without_a_server() {
    let mut launch = prepared("bitty-ai");
    let _bootstrap = take_bootstrap_reader(&mut launch);
    let _control_child = take_control_child(&mut launch);
    launch.write_bootstrap().expect("bootstrap write");

    let started = Instant::now();
    let error = launch
        .establish(started + Duration::from_millis(100))
        .expect_err("an absent server must hit the deadline");
    assert!(
        matches!(error, LaunchError::EstablishTimeout),
        "unexpected error: {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "deadline was not enforced"
    );
}

#[test]
fn abort_closes_handles_and_rejects_later_operations() {
    let mut launch = prepared("bitty-ai");
    let mut bootstrap = take_bootstrap_reader(&mut launch);
    let mut control = take_control_child(&mut launch);
    launch.abort();

    assert!(matches!(
        launch.write_bootstrap(),
        Err(LaunchError::Aborted)
    ));
    assert!(matches!(
        launch.establish(Instant::now() + CONTROL_ESTABLISH_TIMEOUT),
        Err(LaunchError::Aborted)
    ));
    assert!(matches!(
        launch.child_descriptors(),
        Err(LaunchError::HandleMissing)
    ));

    let mut byte = [0u8; 1];
    assert_eq!(
        bootstrap.read(&mut byte).expect("bootstrap read"),
        0,
        "the parent's bootstrap write end must be closed"
    );
    assert_eq!(
        control.read(&mut byte).expect("control read"),
        0,
        "the parent's control end must be closed"
    );
}

#[test]
fn launch_and_channel_nonces_are_fresh_per_launch() {
    let first = prepared("bitty-ai");
    let second = prepared("bitty-ai");

    assert_ne!(first.launch_nonce(), second.launch_nonce());
    assert_ne!(
        first.control_channel_nonce(),
        second.control_channel_nonce()
    );
    assert_ne!(first.data_channel_nonce(), second.data_channel_nonce());
    assert_ne!(first.launch_nonce(), first.control_channel_nonce());
    assert_ne!(first.control_channel_nonce(), first.data_channel_nonce());
}

#[test]
fn child_descriptors_are_fixed_and_disappear_after_spawn() {
    let mut launch = prepared("bitty-ai");
    let descriptors = launch.child_descriptors().expect("child descriptors");
    assert_eq!(descriptors.bootstrap().get(), 3);
    assert_eq!(descriptors.control().get(), 4);

    let mut command = std::process::Command::new("true");
    launch
        .install_child_descriptors(&mut command)
        .expect("pre-exec registration");

    launch.after_spawn();
    assert!(matches!(
        launch.child_descriptors(),
        Err(LaunchError::HandleMissing)
    ));
    assert!(matches!(
        launch.install_child_descriptors(&mut command),
        Err(LaunchError::HandleMissing)
    ));
}

/// The spawn guard child runs only when the parent test re-executes this
/// binary through `sh`, which closes descriptors 3 through 9 first.
const SPAWN_GUARD_CHILD_ENV: &str = "BITTY_PUBLICATION_SPAWN_GUARD_CHILD";

#[test]
fn nonexistent_executable_is_reported_when_placement_descriptors_are_free() {
    if std::env::var_os(SPAWN_GUARD_CHILD_ENV).is_some() {
        return;
    }
    let executable = std::env::current_exe().expect("test executable path");
    // The wrapper owns no extra descriptors and closes the placement range
    // before exec, so the guarded child starts with 3/4 free. Without the
    // parent-side reservation, std's exec-error channel lands there and a
    // failed exec is reported as a successful spawn.
    let script = "exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-; \
                  exec \"$0\" publication_spawn_fd_guard_child --test-threads=1 --nocapture";
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg(executable)
        .env(SPAWN_GUARD_CHILD_ENV, "1")
        .status()
        .expect("guarded child test process");
    assert!(
        status.success(),
        "guarded spawn-failure child failed: {status}"
    );
}

#[test]
fn publication_spawn_fd_guard_child() {
    if std::env::var_os(SPAWN_GUARD_CHILD_ENV).is_none() {
        return;
    }

    // While the launch channels are created, hold 3/4 with harmless dummy
    // descriptors so the channels land above the placement range. Freeing the
    // dummies afterwards puts 3/4 back in the lowest-free set that std uses
    // for its exec-error channel.
    let dummies = [
        File::open("/dev/null").expect("dummy descriptor one"),
        File::open("/dev/null").expect("dummy descriptor two"),
    ];
    let dummy_descriptors: Vec<RawFd> = dummies.iter().map(AsRawFd::as_raw_fd).collect();
    assert!(
        dummy_descriptors.contains(&PLACEMENT_BOOTSTRAP)
            && dummy_descriptors.contains(&PLACEMENT_CONTROL),
        "guarded child did not start with the placement descriptors free: {dummy_descriptors:?}"
    );

    let mut launch = prepared("bitty-ai");
    drop(dummies);

    let mut command = std::process::Command::new("/nonexistent/bitty-publication-spawn-probe");
    launch
        .install_child_descriptors(&mut command)
        .expect("pre-exec registration");
    let reservation = reservation_descriptors(&launch);

    let result = command.spawn();
    launch.after_spawn();

    match result {
        Err(error) => eprintln!("spawn failed closed as required: {error}"),
        Ok(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("spawn of a nonexistent executable reported success");
        }
    }
    assert_eq!(
        reservation.len(),
        2,
        "the spawn reservation must cover two descriptors"
    );
    assert!(matches!(
        launch.child_descriptors(),
        Err(LaunchError::HandleMissing)
    ));
    assert!(reservation_descriptors(&launch).is_empty());
    #[cfg(target_os = "linux")]
    {
        for fd in &reservation {
            assert!(
                !descriptor_is_open(*fd),
                "reservation fd {fd} leaked after a failed spawn"
            );
        }
        assert!(
            !descriptor_is_open(PLACEMENT_BOOTSTRAP) && !descriptor_is_open(PLACEMENT_CONTROL),
            "placement descriptors were not released after a failed spawn"
        );
    }
}

#[test]
fn reservation_is_held_across_a_successful_spawn_and_released_afterwards() {
    let mut launch = prepared("bitty-ai");
    let expected_pin = launch.helper_cert_sha256().expect("material pin");
    let material = snapshot(&launch);

    let mut command = std::process::Command::new("true");
    launch
        .install_child_descriptors(&mut command)
        .expect("pre-exec registration");
    let reservation = reservation_descriptors(&launch);
    assert_eq!(reservation.len(), 2, "spawn reservation must be installed");
    #[cfg(target_os = "linux")]
    for fd in &reservation {
        assert_cloexec(*fd);
    }

    let mut child = command.spawn().expect("fixture process spawn");
    assert!(child.wait().expect("fixture process wait").success());

    // Keep the same launch's child ends for the in-process fixture handshake,
    // then release the parent's copies and the reservation.
    let bootstrap = take_bootstrap_reader(&mut launch);
    let control_child = take_control_child(&mut launch);
    launch.after_spawn();
    assert!(reservation_descriptors(&launch).is_empty());
    #[cfg(target_os = "linux")]
    for fd in reservation {
        assert!(
            !descriptor_is_open(fd),
            "reservation fd {fd} survived after_spawn"
        );
    }

    launch.write_bootstrap().expect("bootstrap write");
    let server = spawn_fixture_server(bootstrap, control_child, material, false, CONTROL_ALPN);
    let established = launch
        .establish(Instant::now() + CONTROL_ESTABLISH_TIMEOUT)
        .expect("mutual TLS establishment after a reserved spawn");
    assert!(!established.is_handshaking());
    assert_eq!(established.alpn_protocol(), Some(CONTROL_ALPN));
    assert_eq!(established.helper_cert_sha256(), expected_pin);

    let outcome = server
        .join()
        .expect("server thread")
        .expect("server handshake");
    assert!(!outcome.handshaking);
    assert_eq!(outcome.alpn.as_deref(), Some(CONTROL_ALPN));
}

#[test]
fn dropping_the_launch_releases_the_spawn_reservation() {
    let mut launch = prepared("bitty-ai");
    let mut command = std::process::Command::new("true");
    launch
        .install_child_descriptors(&mut command)
        .expect("pre-exec registration");
    let reservation = reservation_descriptors(&launch);
    assert_eq!(reservation.len(), 2, "spawn reservation must be installed");

    drop(launch);
    #[cfg(target_os = "linux")]
    for fd in reservation {
        assert!(
            !descriptor_is_open(fd),
            "reservation fd {fd} leaked after dropping the launch"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn reservation_descriptors_do_not_cross_exec() {
    let mut launch = prepared("bitty-ai");
    let mut command = std::process::Command::new("sleep");
    command.arg("30");
    launch
        .install_child_descriptors(&mut command)
        .expect("pre-exec registration");
    let reservation = reservation_descriptors(&launch);
    assert_eq!(reservation.len(), 2);
    let reserved_targets: Vec<std::path::PathBuf> = reservation
        .iter()
        .map(|fd| {
            std::fs::read_link(format!("/proc/self/fd/{fd}"))
                .unwrap_or_else(|error| panic!("read reservation fd {fd} target: {error}"))
        })
        .collect();

    let mut child = command.spawn().expect("sleep spawn");
    wait_for_exec(child.id(), "sleep");
    let targets = child_fd_targets(child.id());
    let leaked: Vec<_> = targets
        .iter()
        .filter(|(_, target)| reserved_targets.contains(target))
        .collect();
    assert!(
        leaked.is_empty(),
        "reservation descriptors survived exec: {leaked:?}"
    );
    let names: Vec<&str> = targets.iter().map(|(name, _)| name.as_str()).collect();
    let bootstrap_name = PLACEMENT_BOOTSTRAP.to_string();
    let control_name = PLACEMENT_CONTROL.to_string();
    assert!(
        names.contains(&bootstrap_name.as_str()) && names.contains(&control_name.as_str()),
        "placed child descriptors are missing after exec: {names:?}"
    );

    launch.after_spawn();
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "linux")]
fn descriptor_is_open(fd: RawFd) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).is_ok()
}

#[cfg(target_os = "linux")]
fn assert_cloexec(fd: RawFd) {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
        .unwrap_or_else(|error| panic!("read fdinfo for reservation fd {fd}: {error}"));
    let flags = info
        .lines()
        .find_map(|line| line.strip_prefix("flags:\t"))
        .unwrap_or_else(|| panic!("fdinfo for reservation fd {fd} has no flags line"));
    let flags = u32::from_str_radix(flags.trim(), 8).expect("octal descriptor flags");
    assert_ne!(
        flags & libc::O_CLOEXEC as u32,
        0,
        "reservation fd {fd} is not close-on-exec"
    );
}

#[cfg(target_os = "linux")]
fn wait_for_exec(pid: u32, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let comm = format!("/proc/{pid}/comm");
    loop {
        if std::fs::read_to_string(&comm).is_ok_and(|name| name.trim() == expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "process {pid} did not exec {expected}"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(target_os = "linux")]
fn child_fd_targets(pid: u32) -> Vec<(String, std::path::PathBuf)> {
    let directory = format!("/proc/{pid}/fd");
    let entries =
        std::fs::read_dir(&directory).unwrap_or_else(|error| panic!("read {directory}: {error}"));
    let mut targets = Vec::new();
    for entry in entries {
        let entry = entry.expect("child fd entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(target) = std::fs::read_link(entry.path()) {
            targets.push((name, target));
        }
    }
    targets
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("write refused"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ServerOutcome {
    handshaking: bool,
    alpn: Option<Vec<u8>>,
}

fn spawn_fixture_server(
    mut bootstrap: File,
    mut control_child: UnixStream,
    material: MaterialSnapshot,
    tls12_only: bool,
    alpn: &'static [u8],
) -> thread::JoinHandle<io::Result<ServerOutcome>> {
    thread::spawn(move || {
        let mut record = Vec::new();
        bootstrap.read_to_end(&mut record)?;
        if record.is_empty() {
            return Err(io::Error::other("fixture read an empty bootstrap record"));
        }

        let config = server_config(&material, tls12_only, alpn);
        control_child.set_read_timeout(Some(Duration::from_secs(10)))?;
        control_child.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut connection = ServerConnection::new(Arc::new(config)).map_err(io::Error::other)?;
        while connection.is_handshaking() {
            let (read, written) = connection.complete_io(&mut control_child)?;
            if read == 0 && written == 0 {
                break;
            }
        }
        Ok(ServerOutcome {
            handshaking: connection.is_handshaking(),
            alpn: connection.alpn_protocol().map(<[u8]>::to_vec),
        })
    })
}

fn server_config(material: &MaterialSnapshot, tls12_only: bool, alpn: &[u8]) -> ServerConfig {
    const TLS13_VERSIONS: &[&SupportedProtocolVersion] = &[&rustls::version::TLS13];
    const TLS12_VERSIONS: &[&SupportedProtocolVersion] = &[&rustls::version::TLS12];
    let versions = if tls12_only {
        TLS12_VERSIONS
    } else {
        TLS13_VERSIONS
    };
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let supported = provider.signature_verification_algorithms;
    let verifier = Arc::new(PinnedClientVerifier {
        expected_core_cert: material.core_cert.clone(),
        supported,
    });
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .expect("fixture protocol versions")
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![material.helper_cert.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(material.helper_key.clone())),
        )
        .expect("fixture helper identity");
    if !alpn.is_empty() {
        config.alpn_protocols = vec![alpn.to_vec()];
    }
    config.send_tls13_tickets = 0;
    config.max_early_data_size = 0;
    config
}

/// Fixture-side verifier that accepts exactly the Core leaf generated by the
/// launch under test. This mirrors the helper's pin rule without adding a
/// SHA-256 dependency to the test: the certificate is compared byte for byte.
#[derive(Debug)]
struct PinnedClientVerifier {
    expected_core_cert: CertificateDer<'static>,
    supported: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, RustlsError> {
        if !intermediates.is_empty() || end_entity.as_ref() != self.expected_core_cert.as_ref() {
            return Err(pin_rejection());
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(message, certificate, signature, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(message, certificate, signature, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

fn pin_rejection() -> RustlsError {
    RustlsError::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

/// Sequential reader for the documented manifest envelope.
struct RecordReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> RecordReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> &'a [u8] {
        let end = self.position + length;
        assert!(end <= self.bytes.len(), "manifest is truncated");
        let slice = &self.bytes[self.position..end];
        self.position = end;
        slice
    }

    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }

    fn u16(&mut self) -> u16 {
        u16::from_be_bytes(self.take(2).try_into().expect("two bytes"))
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().expect("four bytes"))
    }

    fn len_prefixed(&mut self) -> &'a [u8] {
        let length = self.u32() as usize;
        self.take(length)
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }
}
