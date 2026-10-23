//! bitty#2495 / CTX-1479: hermetic storage-component host tests.
//!
//! This harness-free test binary is also the component fixture: the host passes
//! `--root`, so `main` serves the real pinned `bitty-storage` protocol-v1
//! server (or a fault-injection mode selected by a `fixture-mode` file next to
//! the executable). Otherwise `main` runs the tests below. A stripped copy of
//! the test binary is installed because the unstripped executable exceeds the
//! component's 256 MiB ceiling. Every root is a scratch directory and every
//! child runs with a cleared environment; no network, shell, or ambient XDG
//! state is involved.

mod storage_component_fixture;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bitty_package::integrity::sha256_hex;
use bitty_runtime::SessionFileBackend;
use bitty_runtime::plugin_runtime::store::KvCommitBackend;
use bitty_terminal::storage_backends::{StorageKvBackend, StorageSessionBackend};
use bitty_terminal::storage_component_host::{
    MAX_COMPONENT_VALUE_BYTES, StorageComponentBytesBackend, StorageComponentClient,
    StorageComponentConfig, StorageComponentFault, StorageComponentGate, StorageComponentKvBackend,
    StorageComponentSessionBackend, StorageHostError, StorageMigrationOutcome,
    StorageRootSelection, StorageRow, migrate_linked_file,
};

const COMPONENT_VERSION: &str = "0.0.1";
const DEADLINE: Duration = Duration::from_secs(2);
const HANDSHAKE: Duration = Duration::from_secs(2);
const GOLDEN_SESSION: &[u8] = include_bytes!("fixtures/session-parity/basic.session");

const ENABLED: StorageComponentGate = StorageComponentGate {
    preferred: true,
    safe_mode: false,
    capability_granted: true,
};

/// One stripped copy of this test binary serves as the fixture; debug test
/// executables are far larger than the component's 256 MiB ceiling.
fn fixture_path() -> PathBuf {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let dir =
                std::env::temp_dir().join(format!("bitty-ctx2495-fixture-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("fixture dir");
            let output = dir.join(format!(
                "bitty-storage-component-fixture{}",
                std::env::consts::EXE_SUFFIX
            ));
            std::fs::copy(
                std::env::current_exe().expect("current test executable"),
                &output,
            )
            .expect("copy test executable");
            let strip = std::env::var_os("STRIP").unwrap_or_else(|| "strip".into());
            let status = Command::new(strip)
                .arg(&output)
                .status()
                .expect("run strip for the storage fixture");
            assert!(status.success(), "strip the storage fixture");
            output
        })
        .clone()
}

fn cleanup_fixture() {
    let path = fixture_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

fn main() {
    if let Some(root) = storage_component_fixture::root_argument() {
        std::process::exit(i32::from(storage_component_fixture::run(root)));
    }
    let tests: &[(&str, fn())] = &[
        (
            "protocol_roundtrip_and_clean_shutdown",
            protocol_roundtrip_and_clean_shutdown,
        ),
        (
            "policy_denies_before_resolution_and_install_errors_fail_closed",
            policy_denies_before_resolution_and_install_errors_fail_closed,
        ),
        (
            "handshake_and_wire_faults_fail_closed",
            handshake_and_wire_faults_fail_closed,
        ),
        (
            "mutation_errors_poison_and_never_replay",
            mutation_errors_poison_and_never_replay,
        ),
        (
            "component_session_and_kv_backends_match_the_linked_model",
            component_session_and_kv_backends_match_the_linked_model,
        ),
        (
            "migration_is_explicit_confined_and_idempotent",
            migration_is_explicit_confined_and_idempotent,
        ),
        (
            "storage_root_is_owner_only_and_rejects_symlinks",
            storage_root_is_owner_only_and_rejects_symlinks,
        ),
        (
            "stderr_ring_is_bounded_and_escaped",
            stderr_ring_is_bounded_and_escaped,
        ),
    ];
    let mut failed = 0;
    for (name, test) in tests {
        print!("test {name} ... ");
        match std::panic::catch_unwind(test) {
            Ok(()) => println!("ok"),
            Err(_) => {
                println!("FAILED");
                failed += 1;
            }
        }
    }
    cleanup_fixture();
    if failed > 0 {
        std::process::exit(1);
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx2495-{tag}-{}-{}",
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

    fn subdir(&self, name: &str) -> PathBuf {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).expect("scratch subdir");
        dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Install the fixture into the user component tier with a valid descriptor.
fn install(install_root: &Path, mode: Option<&str>) -> PathBuf {
    let version_dir = install_root.join("storage").join(COMPONENT_VERSION);
    std::fs::create_dir_all(&version_dir).expect("version dir");
    std::fs::write(
        install_root.join("storage").join("current"),
        format!("{COMPONENT_VERSION}\n"),
    )
    .expect("current");
    let executable = version_dir.join("bitty-storage");
    let fixture = fixture_path();
    if std::fs::hard_link(&fixture, &executable).is_err() {
        std::fs::copy(&fixture, &executable).expect("copy fixture");
    }
    let digest = sha256_hex(&std::fs::read(&executable).expect("read fixture"));
    let descriptor = format!(
        "[component]\nname = \"storage\"\nversion = \"{COMPONENT_VERSION}\"\nprotocol = [1, 1]\nexecutable = \"bitty-storage\"\nsha256 = \"{digest}\"\n"
    );
    std::fs::write(version_dir.join("bitty-component.toml"), descriptor).expect("descriptor");
    if let Some(mode) = mode {
        std::fs::write(version_dir.join("fixture-mode"), mode).expect("fixture mode");
    }
    executable
}

fn config(install_root: &Path, base: &Path, gate: StorageComponentGate) -> StorageComponentConfig {
    StorageComponentConfig {
        user_root: Some(install_root.to_path_buf()),
        system_root: None,
        storage_root: StorageRootSelection::under(base, "default").expect("root selection"),
        gate,
        operation_deadline: DEADLINE,
        handshake_timeout: HANDSHAKE,
        shutdown_timeout: DEADLINE,
    }
}

fn connect(install_root: &Path, base: &Path) -> Arc<StorageComponentClient> {
    StorageComponentClient::connect(&config(install_root, base, ENABLED))
        .expect("component host connects")
}

fn protocol_roundtrip_and_clean_shutdown() {
    let scratch = Scratch::new("roundtrip");
    let install_root = scratch.subdir("extensions");
    let base = scratch.subdir("state");
    install(&install_root, None);
    let client = connect(&install_root, &base);
    assert!(client.is_running());
    assert!(!client.commit_was_submitted());

    let namespace = "sessions";
    assert_eq!(client.open_namespace(namespace, DEADLINE).expect("open"), 0);

    let first = client
        .commit_batch(
            namespace,
            0,
            vec![StorageRow {
                key: "alpha".to_owned(),
                value: b"one".to_vec(),
            }],
            Vec::new(),
            DEADLINE,
        )
        .expect("commit");
    assert!(first.submitted);
    assert!(client.commit_was_submitted());

    let (revision, rows) = client.read_snapshot(namespace, DEADLINE).expect("read");
    assert_eq!(revision, first.revision);
    assert_eq!(
        rows,
        vec![StorageRow {
            key: "alpha".to_owned(),
            value: b"one".to_vec(),
        }]
    );

    let second = client
        .commit_batch(
            namespace,
            revision,
            vec![StorageRow {
                key: "beta".to_owned(),
                value: b"two".to_vec(),
            }],
            vec!["alpha".to_owned()],
            DEADLINE,
        )
        .expect("replace");
    let (revision, rows) = client.read_snapshot(namespace, DEADLINE).expect("read");
    assert_eq!(revision, second.revision);
    assert_eq!(
        rows,
        vec![StorageRow {
            key: "beta".to_owned(),
            value: b"two".to_vec(),
        }]
    );

    let rolled_back = client
        .rollback(namespace, revision, DEADLINE)
        .expect("rollback");
    assert!(rolled_back > revision);
    let (_, rows) = client.read_snapshot(namespace, DEADLINE).expect("read");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "alpha");

    // Local bounds fail before anything reaches the wire and leave the client
    // usable (the exchange itself never started).
    assert_eq!(
        client
            .commit_batch(
                namespace,
                rolled_back,
                vec![StorageRow {
                    key: "../escape".to_owned(),
                    value: Vec::new(),
                }],
                Vec::new(),
                DEADLINE,
            )
            .expect_err("traversal key"),
        StorageHostError::Bounds
    );
    assert_eq!(
        client
            .commit_batch(
                namespace,
                rolled_back,
                vec![StorageRow {
                    key: "large".to_owned(),
                    value: vec![0u8; MAX_COMPONENT_VALUE_BYTES + 1],
                }],
                Vec::new(),
                DEADLINE,
            )
            .expect_err("oversized value"),
        StorageHostError::Bounds
    );
    assert_eq!(
        client
            .commit_batch(
                namespace,
                rolled_back,
                vec![
                    StorageRow {
                        key: "duplicate".to_owned(),
                        value: b"one".to_vec(),
                    },
                    StorageRow {
                        key: "duplicate".to_owned(),
                        value: b"two".to_vec(),
                    },
                ],
                Vec::new(),
                DEADLINE,
            )
            .expect_err("duplicate key"),
        StorageHostError::Bounds
    );
    assert!(client.is_running());

    client.shutdown(DEADLINE).expect("clean shutdown");
    assert!(!client.is_running());
    assert_eq!(client.accounting().crashes.load(Ordering::Relaxed), 0);
    assert_eq!(client.accounting().eof.load(Ordering::Relaxed), 0);
    assert!(client.stderr_tail().is_empty());
}

fn policy_denies_before_resolution_and_install_errors_fail_closed() {
    let scratch = Scratch::new("policy");
    let install_root = scratch.subdir("extensions");
    let base = scratch.subdir("state");

    let disabled = config(&install_root, &base, StorageComponentGate::DISABLED);
    assert_eq!(
        StorageComponentClient::connect(&disabled).expect_err("default off"),
        StorageHostError::Denied
    );

    let safe = config(
        &install_root,
        &base,
        StorageComponentGate {
            preferred: true,
            safe_mode: true,
            capability_granted: true,
        },
    );
    assert_eq!(
        StorageComponentClient::connect(&safe).expect_err("safe mode"),
        StorageHostError::Denied
    );

    let no_capability = config(
        &install_root,
        &base,
        StorageComponentGate {
            preferred: true,
            safe_mode: false,
            capability_granted: false,
        },
    );
    assert_eq!(
        StorageComponentClient::connect(&no_capability).expect_err("capability"),
        StorageHostError::Denied
    );

    // Gate allows resolution, but nothing is installed.
    let missing = config(&install_root, &base, ENABLED);
    assert_eq!(
        StorageComponentClient::connect(&missing).expect_err("missing install"),
        StorageHostError::Resolution
    );

    let mut relative = config(&install_root, &base, ENABLED);
    relative.user_root = Some(PathBuf::from("relative/extensions"));
    assert_eq!(
        StorageComponentClient::connect(&relative).expect_err("relative install root"),
        StorageHostError::Resolution
    );

    // A tampered executable fails the descriptor digest check before spawn.
    let executable = install(&install_root, None);
    let mut tampered = std::fs::OpenOptions::new()
        .append(true)
        .open(&executable)
        .expect("open executable");
    use std::io::Write as _;
    tampered.write_all(b"tamper").expect("tamper");
    drop(tampered);
    assert_eq!(
        StorageComponentClient::connect(&missing).expect_err("tampered install"),
        StorageHostError::Resolution
    );

    // Root selection itself is explicit and confined.
    assert!(StorageRootSelection::under("relative", "default").is_err());
    assert!(StorageRootSelection::under(&base, "../default").is_err());
    let file_base = scratch.path().join("not-a-dir");
    std::fs::write(&file_base, b"x").expect("file base");
    let selection = StorageRootSelection::under(&file_base, "default").expect("selection");
    assert_eq!(
        selection.prepare().expect_err("file base"),
        StorageHostError::Confinement
    );
}

fn handshake_and_wire_faults_fail_closed() {
    // Handshake failures surface from `connect`.
    for mode in [
        "silent-handshake",
        "wrong-protocol",
        "wrong-component",
        "wrong-version",
    ] {
        let scratch = Scratch::new(mode);
        let install_root = scratch.subdir("extensions");
        let base = scratch.subdir("state");
        install(&install_root, Some(mode));
        let expected = if mode == "silent-handshake" {
            StorageHostError::Timeout
        } else {
            StorageHostError::Protocol
        };
        assert_eq!(
            StorageComponentClient::connect(&config(&install_root, &base, ENABLED))
                .expect_err(mode),
            expected,
            "mode {mode}"
        );
    }

    // Post-handshake failures surface from the first exchange.
    for (mode, expected) in [
        ("malformed-frame", StorageHostError::Protocol),
        ("oversized-frame", StorageHostError::Protocol),
        ("truncated-frame", StorageHostError::Eof),
        ("crash-after-ack", StorageHostError::Eof),
        ("unknown-error", StorageHostError::Protocol),
        ("unsolicited-frame", StorageHostError::Protocol),
        ("empty-page-loop", StorageHostError::Protocol),
    ] {
        let scratch = Scratch::new(mode);
        let install_root = scratch.subdir("extensions");
        let base = scratch.subdir("state");
        install(&install_root, Some(mode));
        let mut mode_config = config(&install_root, &base, ENABLED);
        if mode == "empty-page-loop" {
            // 4096 one-row pages are legal; give every exchange headroom so a
            // loaded CI runner cannot turn the page-bound assertion into a
            // per-exchange timeout.
            mode_config.operation_deadline = Duration::from_secs(10);
        }
        let operation_deadline = mode_config.operation_deadline;
        let client = match StorageComponentClient::connect(&mode_config) {
            Ok(client) => client,
            Err(error) => {
                // `unsolicited-frame` may fail during the handshake exchange
                // or on the first request depending on reader scheduling.
                assert_eq!(error, expected, "mode {mode} handshake");
                continue;
            }
        };
        let error = if mode == "empty-page-loop" {
            client
                .open_namespace("sessions", operation_deadline)
                .expect("namespace opens");
            client
                .read_snapshot("sessions", operation_deadline)
                .expect_err("snapshot fault")
        } else {
            client
                .open_namespace("sessions", operation_deadline)
                .expect_err("wire fault")
        };
        assert_eq!(error, expected, "mode {mode}");
        assert!(matches!(
            client.open_namespace("sessions", operation_deadline),
            Err(StorageHostError::Poisoned)
        ));
    }
}

fn mutation_errors_poison_and_never_replay() {
    let scratch = Scratch::new("mutation-error");
    let install_root = scratch.subdir("extensions");
    let base = scratch.subdir("state");
    install(&install_root, Some("error-stale"));
    let client = connect(&install_root, &base);
    client.open_namespace("sessions", DEADLINE).expect("open");
    let error = client
        .commit_batch(
            "sessions",
            0,
            vec![StorageRow {
                key: "alpha".to_owned(),
                value: b"one".to_vec(),
            }],
            Vec::new(),
            DEADLINE,
        )
        .expect_err("stale revision");
    assert_eq!(
        error,
        StorageHostError::Component(StorageComponentFault::StaleRevision)
    );
    assert!(client.commit_was_submitted());
    assert!(!client.is_running());
    assert_eq!(
        client
            .open_namespace("sessions", DEADLINE)
            .expect_err("poisoned"),
        StorageHostError::Poisoned
    );
}

fn component_session_and_kv_backends_match_the_linked_model() {
    let scratch = Scratch::new("parity");
    let install_root = scratch.subdir("extensions");
    let base = scratch.subdir("state");
    install(&install_root, None);
    let client = connect(&install_root, &base);

    // Session bytes come from the linked Codec/adapters; both sides must
    // persist exactly the same image and decode it identically.
    let linked_session = StorageSessionBackend::new();
    let snapshot = linked_session
        .decode_snapshot(GOLDEN_SESSION)
        .expect("decode golden");
    let bytes = linked_session.encode_snapshot(&snapshot).expect("encode");
    let session_dir = scratch.subdir("sessions");
    let session_path = session_dir.join("session");
    let component_session =
        StorageComponentSessionBackend::new(Arc::clone(&client), &session_dir, DEADLINE)
            .expect("session backend");
    assert!(
        component_session.session_file().is_none()
            && component_session.session_file_for(None, None).is_none(),
        "component discovery stays off until the linked path is migrated by an explicit task"
    );
    component_session
        .commit_session_bytes(&session_path, &bytes)
        .expect("component commit");
    let loaded = component_session
        .load_session_bytes(&session_path)
        .expect("component load");
    assert_eq!(loaded, bytes);
    let decoded = component_session
        .decode_snapshot(&loaded)
        .expect("component decode");
    assert_eq!(
        component_session
            .encode_snapshot(&decoded)
            .expect("component encode"),
        bytes
    );

    let linked_path = session_dir.join("linked.session");
    linked_session
        .commit_session_bytes(&linked_path, &bytes)
        .expect("linked commit");
    assert_eq!(std::fs::read(&linked_path).expect("linked bytes"), bytes);

    // The component adapter refuses paths outside its explicit root.
    let outside = scratch.path().join("outside.session");
    assert!(
        component_session
            .commit_session_bytes(&outside, &bytes)
            .is_err()
    );

    // Whole-object bounds are enforced before any component write. The v1
    // batch ceiling is lower than the 1 MiB snapshot read bound, so a
    // too-large object must fail closed and leave the previous state intact.
    let bounded =
        StorageComponentBytesBackend::new(Arc::clone(&client), "bounded".to_owned(), DEADLINE)
            .expect("bounded namespace");
    bounded.commit(b"small").expect("small object");
    assert_eq!(
        bounded
            .commit(&vec![
                0u8;
                bitty_terminal::storage_component_host::MAX_BATCH_BYTES
            ])
            .expect_err("batch ceiling"),
        StorageHostError::Bounds
    );
    assert_eq!(
        bounded.load().expect("previous object"),
        Some(b"small".to_vec())
    );
    let oversized = vec![0u8; bitty_terminal::storage_component_host::MAX_COMPONENT_BYTES + 1];
    assert_eq!(
        bounded.commit(&oversized).expect_err("oversized object"),
        StorageHostError::Bounds
    );
    assert_eq!(
        bounded.load().expect("previous object after oversized"),
        Some(b"small".to_vec())
    );

    // Plugin KV parity over the same opaque store image.
    let store_dir = scratch.subdir("plugins/acme");
    let store_path = store_dir.join("store.json");
    let store_bytes = br#"{"color":"blue"}"#;
    let linked_kv = StorageKvBackend::new();
    linked_kv
        .commit_store_bytes(&store_path, store_bytes)
        .expect("linked kv commit");
    let component_kv = StorageComponentKvBackend::new(Arc::clone(&client), &store_dir, DEADLINE)
        .expect("kv backend");
    component_kv
        .commit_store_bytes(&store_path, store_bytes)
        .expect("component kv commit");
    assert_eq!(
        component_kv
            .load_store_bytes(&store_path)
            .expect("component kv load")
            .as_deref(),
        Some(&store_bytes[..])
    );
    assert_eq!(
        std::fs::read(&store_path).expect("linked kv bytes"),
        store_bytes
    );

    client.shutdown(DEADLINE).expect("shutdown");
}

fn migration_is_explicit_confined_and_idempotent() {
    let scratch = Scratch::new("migration");
    let install_root = scratch.subdir("extensions");
    let base = scratch.subdir("state");
    install(&install_root, None);
    let client = connect(&install_root, &base);
    let backend = StorageComponentBytesBackend::new(
        Arc::clone(&client),
        "migrated-session".to_owned(),
        DEADLINE,
    )
    .expect("namespace");

    let linked_root = scratch.subdir("legacy-sessions");
    let linked_file = linked_root.join("session");
    let linked_bytes = b"linked-before-component";
    std::fs::write(&linked_file, linked_bytes).expect("linked source");

    let selected = StorageRootSelection::under(&base, "default").expect("selection");
    let marker_root = selected.prepare().expect("prepared root");
    let marker = marker_root.join("migrated-session.marker");

    assert_eq!(
        migrate_linked_file(&backend, &linked_root, &linked_file, &marker_root, &marker)
            .expect("migrate"),
        StorageMigrationOutcome::Migrated {
            bytes: linked_bytes.len()
        }
    );
    assert_eq!(
        backend.load().expect("load migrated"),
        Some(linked_bytes.to_vec())
    );
    // The linked source is never renamed, truncated, or deleted.
    assert_eq!(
        std::fs::read(&linked_file).expect("source intact"),
        linked_bytes
    );
    assert!(marker.is_file());

    assert_eq!(
        migrate_linked_file(&backend, &linked_root, &linked_file, &marker_root, &marker)
            .expect("second migration"),
        StorageMigrationOutcome::AlreadyMigrated
    );

    let outside = scratch.path().join("outside.txt");
    std::fs::write(&outside, b"outside").expect("outside file");
    assert_eq!(
        migrate_linked_file(&backend, &linked_root, &outside, &marker_root, &marker).unwrap_err(),
        StorageHostError::Confinement
    );

    #[cfg(unix)]
    {
        let symlink = linked_root.join("symlink");
        std::os::unix::fs::symlink(&outside, &symlink).expect("symlink");
        assert_eq!(
            migrate_linked_file(&backend, &linked_root, &symlink, &marker_root, &marker)
                .unwrap_err(),
            StorageHostError::Confinement
        );
    }

    let missing = linked_root.join("missing");
    let marker_two = marker_root.join("missing.marker");
    assert_eq!(
        migrate_linked_file(&backend, &linked_root, &missing, &marker_root, &marker_two)
            .expect("missing source"),
        StorageMigrationOutcome::NoSource
    );

    client.shutdown(DEADLINE).expect("shutdown");
}

fn storage_root_is_owner_only_and_rejects_symlinks() {
    let scratch = Scratch::new("root");
    let base = scratch.subdir("state");
    let selection = StorageRootSelection::under(&base, "default").expect("selection");
    let root = selection.prepare().expect("prepare");
    assert!(root.starts_with(std::fs::canonicalize(&base).expect("base")));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&root)
            .expect("root metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    let outside = scratch.subdir("outside");
    let symlink_root = base
        .join(bitty_runtime::SESSION_APP_DIR_NAME)
        .join("storage-component")
        .join("linked");
    std::fs::create_dir_all(symlink_root.parent().expect("parent")).expect("parent");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &symlink_root).expect("root symlink");
    #[cfg(unix)]
    {
        let linked = StorageRootSelection::under(&base, "linked").expect("selection");
        assert_eq!(
            linked.prepare().expect_err("symlink root"),
            StorageHostError::Confinement
        );
    }
}

fn stderr_ring_is_bounded_and_escaped() {
    let scratch = Scratch::new("stderr");
    let install_root = scratch.subdir("extensions");
    let base = scratch.subdir("state");
    install(&install_root, Some("stderr-noise"));
    let client = connect(&install_root, &base);
    client.shutdown(DEADLINE).expect("shutdown");
    let tail = client.stderr_tail();
    assert!(!tail.is_empty());
    assert!(tail.len() <= 4096, "tail must be the bounded log slice");
    assert!(tail.ends_with("TAIL"), "newest bytes win: {tail:?}");
}
