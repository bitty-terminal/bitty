//! bitty#2605: the shared `bittyd` conformance harness.
//!
//! One corpus (`tests/harness/corpus.json`) runs against two drivers:
//!
//! - the in-process reference fake, labeled `candidate-not-bittyd`;
//! - the built `bittyd` binary over its real Unix endpoint (Linux only).
//!
//! Evidence is written per (cell, suite) under `target/conformance/`. A fake
//! run never marks a real row passed: the rows are separate files and the
//! parity report names only the shared vectors it compared.

mod harness;

use std::collections::BTreeMap;

use serde_json::{Value, json};

use harness::corpus::{AuditExpect, Corpus, DriverKind, Expect};
use harness::evidence::{self, Evidence};
use harness::fake::FakeDriver;
use harness::runner::{
    Driver, Outcome, Reply, RowStatus, VectorOutcome, check_audit, check_expect,
    known_defect_matches, run_corpus,
};

#[cfg(target_os = "linux")]
use harness::real::RealDriver;

#[test]
fn corpus_is_well_formed_and_frozen() {
    let corpus = Corpus::load().expect("the embedded corpus parses");
    assert_eq!(corpus.vectors.len(), 66, "corpus size changed");
    for id in [
        "C-01", "C-02", "C-03", "C-04", "C-05", "C-06", "C-07", "C-08", "C-12", "C-13", "C-14",
        "C-15", "C-16", "C-17",
    ] {
        assert!(corpus.vector(id).is_some(), "missing Core vector {id}");
    }
    for id in ["V-45", "V-46", "V-47", "V-48", "V-49"] {
        assert!(
            corpus.vector(id).is_some(),
            "missing terminal content vector {id}"
        );
    }
    for id in ["V-51", "V-52", "V-53", "V-54", "V-55", "V-56"] {
        assert!(
            corpus.vector(id).is_some(),
            "missing session-record vector {id}"
        );
    }
    {
        let vector = corpus.vector("V-03").expect("worker refusal vector");
        assert!(
            vector.requires.is_empty(),
            "V-03 must run on every real cell, not skip on a capability"
        );
    }
    for id in ["V-42", "V-50"] {
        let vector = corpus.vector(id).expect("amendment-gated worker vector");
        assert!(
            vector.requires.is_empty(),
            "{id} is gated by the amendment marker, not a cell capability"
        );
        for driver in [DriverKind::Fake, DriverKind::Real] {
            let reason = vector.skip_reason(driver).unwrap_or_default();
            assert!(
                reason.starts_with("d01-amendment-required:"),
                "{id} must carry the explicit D-01 amendment skip for {}",
                driver.as_str()
            );
        }
    }
    assert_eq!(
        corpus.vector("V-43").expect("worker vector").requires,
        ["worker_sessions"],
        "V-43 still needs a cell that serves workers, which no build does today"
    );
    for id in ["V-16", "V-17"] {
        let vector = corpus.vector(id).expect("stop vector");
        assert_eq!(vector.decisions, ["D-05"], "{id} decisions");
    }
    let retention = corpus.vector("V-20").expect("retention vector");
    assert_eq!(retention.decisions, ["D-14"], "V-20 decisions");
    assert_eq!(
        retention.requires,
        ["retention_clock_seam"],
        "V-20 must skip when the injected clock seam is absent"
    );
    for id in ["V-45", "V-46", "V-47", "V-49"] {
        assert_eq!(
            corpus.vector(id).expect("terminal content vector").requires,
            ["terminal_spawn"],
            "{id} depends on the accepted 1.3 terminal-spawn slice, not workers"
        );
    }
    assert_eq!(
        corpus.vector("V-44").expect("timeout vector").requires,
        ["request_deadline_seam"],
        "the real request-deadline vector must skip when the test seam is absent"
    );
    for vector in &corpus.vectors {
        assert!(
            !vector.forbidden.is_empty(),
            "{} has no forbidden observations",
            vector.id
        );
        assert!(!vector.steps.is_empty(), "{} has no steps", vector.id);
        assert!(
            !vector.state.is_empty(),
            "{} has no post-state assertions",
            vector.id
        );
        assert!(!vector.drivers.is_empty(), "{} has no driver", vector.id);
        for driver in [DriverKind::Fake, DriverKind::Real] {
            if !vector.runs_on(driver) {
                assert!(
                    vector.skip_reason(driver).is_some(),
                    "{} does not run on {} and has no typed skip reason",
                    vector.id,
                    driver.as_str()
                );
            }
        }
    }
    assert_eq!(
        corpus
            .frozen
            .get("session.attach.request_role")
            .map(String::as_str),
        Some("attach_role"),
        "the corpus freezes the attach request role"
    );
    let known: Vec<&str> = corpus
        .vectors
        .iter()
        .filter(|vector| vector.known_defect.is_some())
        .map(|vector| vector.id.as_str())
        .collect();
    assert!(
        known.is_empty(),
        "a known-defect marker is left in the corpus: {known:?}"
    );
}

#[test]
fn audit_not_contains_resolves_captured_secrets() {
    let mut vars = BTreeMap::new();
    vars.insert(
        "create_token".to_owned(),
        Value::String("deadbeef".to_owned()),
    );
    let expect = AuditExpect {
        not_contains: vec!["@create_token".to_owned()],
        ..AuditExpect::default()
    };
    assert!(
        check_audit(&expect, "a=session.create token=deadbeef", &vars).is_err(),
        "a leaking audit line must fail the vector"
    );
    assert!(
        check_audit(&expect, "a=session.create r=ok", &vars).is_ok(),
        "a clean audit line passes"
    );
}

#[test]
fn cell_vars_seed_an_absolute_cwd_and_the_driver_cell_capability() {
    for served in [true, false] {
        let mut vars = BTreeMap::new();
        harness::runner::cell_vars(&mut vars, served);
        assert_eq!(
            vars.get("worker_sessions"),
            Some(&Value::Bool(served)),
            "the handshake vector must assert the capability the driver models"
        );
        let cwd = vars
            .get("cwd")
            .and_then(Value::as_str)
            .expect("a seeded cwd");
        assert!(
            std::path::Path::new(cwd).is_absolute(),
            "the seeded cwd {cwd} must be absolute on this cell"
        );
    }
}

#[test]
fn not_equal_any_rejects_duplicate_ids() {
    let corpus = Corpus::load().expect("corpus");
    let c01 = corpus.vector("C-01").expect("C-01");
    let third = c01
        .steps
        .iter()
        .find(|step| step.params.get("idempotency_key").and_then(Value::as_str) == Some("c01-c"))
        .expect("C-01 third create");
    let mut expect = Expect::default();
    expect.not_equal_any = third.expect.not_equal_any.clone();
    assert!(
        expect.not_equal_any.contains_key("session.session_id")
            && expect.not_equal_any.contains_key("session.terminal_id"),
        "C-01 must assert session and terminal id uniqueness"
    );
    expect
        .not_equal_any
        .retain(|path, _| path == "session.session_id");
    let mut vars = BTreeMap::new();
    vars.insert("a_sid".to_owned(), Value::String("a".repeat(32)));
    vars.insert("b_sid".to_owned(), Value::String("b".repeat(32)));
    let mut driver = FakeDriver::new();
    let reply = |id: &str| {
        Outcome::Reply(Reply {
            ok: true,
            result: json!({"session": {"session_id": id}}),
            error: None,
        })
    };
    assert!(
        check_expect(
            &expect,
            &reply(&"a".repeat(32)),
            &mut driver,
            "client",
            &vars
        )
        .is_err(),
        "a duplicate of the first id must fail"
    );
    assert!(
        check_expect(
            &expect,
            &reply(&"b".repeat(32)),
            &mut driver,
            "client",
            &vars
        )
        .is_err(),
        "a duplicate of the second id must fail"
    );
    assert!(
        check_expect(
            &expect,
            &reply(&"c".repeat(32)),
            &mut driver,
            "client",
            &vars
        )
        .is_ok(),
        "a distinct id must pass"
    );
}

#[test]
fn known_defect_must_match_the_recorded_step_and_code() {
    let defect = harness::corpus::KnownDefect {
        issue: "bitty#2768".to_owned(),
        reason: "a repeat detach resolves to NotFound".to_owned(),
        drivers: vec![DriverKind::Real],
        step: 6,
        code: Some("NotFound".to_owned()),
        path: None,
    };
    let failure = "session.detach: expected ok=true, observed ok=false (NotFound / NotFound)";
    assert!(known_defect_matches(&defect, Some(6), Some(failure)));
    assert!(
        !known_defect_matches(&defect, Some(5), Some(failure)),
        "a failure at another step must not be masked"
    );
    assert!(
        !known_defect_matches(
            &defect,
            Some(6),
            Some("(StaleGeneration / StaleGeneration)")
        ),
        "a different refusal code must not be masked"
    );
}

#[test]
fn zero_compared_parity_is_a_skip() {
    let doc = evidence::render_parity(0, &[], &["cell-not-servable".to_owned()]);
    assert_eq!(doc["status"], "skip");
}

#[test]
fn fake_conformance_corpus() {
    let corpus = Corpus::load().expect("corpus");
    let mut driver = FakeDriver::new();
    let rows = run_corpus(&corpus, &mut driver);
    let facts = driver.facts();
    assert_eq!(
        facts.capabilities.get("worker_sessions"),
        Some(&false),
        "the reference cell honours D-01: the worker kind is reserved"
    );
    assert_eq!(
        facts.features.get("terminal_spawn"),
        Some(&true),
        "the reference cell models the accepted 1.3 terminal-spawn slice"
    );
    let evidence = Evidence {
        suite: "fake",
        label: evidence::driver_label(DriverKind::Fake),
        facts: &facts,
        corpus_contract: &corpus.contract,
        frozen: &corpus.frozen,
        rows: &rows,
    };
    let _ = evidence::write(&evidence);
    assert_no_failures(&rows, "fake");
    assert!(
        rows.iter().all(|row| row.driver == DriverKind::Fake),
        "the fake suite never carries a real row"
    );
    assert!(
        rows.iter()
            .all(|row| row.status != RowStatus::Skip || row.skip_reason.is_some()),
        "every fake skip is typed"
    );
    let skipped: Vec<&str> = rows
        .iter()
        .filter(|row| row.status == RowStatus::Skip)
        .map(|row| row.id.as_str())
        .collect();
    assert_eq!(
        skipped,
        [
            "V-17", "V-18", "V-39", "V-40", "V-42", "V-43", "C-01", "C-02", "C-03", "C-11", "C-07",
            "C-08", "C-12", "C-15", "C-16", "V-49", "V-50", "V-55", "V-56", "V-57"
        ],
        "the fake suite skips exactly the real-only and amendment-gated vectors"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn real_conformance_corpus() {
    let corpus = Corpus::load().expect("corpus");
    let mut driver = RealDriver::new();
    let rows = run_corpus(&corpus, &mut driver);
    let facts = driver.facts();
    let evidence = Evidence {
        suite: "real",
        label: evidence::driver_label(DriverKind::Real),
        facts: &facts,
        corpus_contract: &corpus.contract,
        frozen: &corpus.frozen,
        rows: &rows,
    };
    let _ = evidence::write(&evidence);
    assert_no_failures(&rows, "real");
    assert!(
        rows.iter().all(|row| row.driver == DriverKind::Real),
        "the real suite never carries a fake row"
    );
    let skipped: Vec<&str> = rows
        .iter()
        .filter(|row| row.status == RowStatus::Skip)
        .filter(|row| {
            !row.skip_reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("cell-not-servable:"))
        })
        .map(|row| row.id.as_str())
        .collect();
    assert_eq!(
        skipped,
        ["V-06", "V-33", "V-42", "C-10", "F-01", "V-50"],
        "only the injected-clock, adapter-owned, injected-queue, and amendment-gated vectors skip on Linux"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn real_conformance_corpus() {
    let corpus = Corpus::load().expect("corpus");
    let rows: Vec<VectorOutcome> = corpus
        .vectors
        .iter()
        .filter(|vector| vector.runs_on(DriverKind::Real))
        .map(|vector| VectorOutcome {
            id: vector.id.clone(),
            title: vector.title.clone(),
            area: vector.area.clone(),
            decisions: vector.decisions.clone(),
            driver: DriverKind::Real,
            status: RowStatus::Skip,
            skip_reason: Some(
                "cell-not-servable (the served loop is Linux-only in this slice)".to_owned(),
            ),
            deferred: vector.deferred.clone(),
            known_defect: None,
            instance_id: String::new(),
            steps: 0,
            signature: Vec::new(),
            failure_step: None,
            failure: None,
        })
        .collect();
    let facts = harness::runner::ServiceFacts {
        cell: evidence::cell(),
        version: bitty_daemon::CRATE_VERSION.to_owned(),
        ..harness::runner::ServiceFacts::default()
    };
    let evidence = Evidence {
        suite: "real",
        label: evidence::driver_label(DriverKind::Real),
        facts: &facts,
        corpus_contract: &corpus.contract,
        frozen: &corpus.frozen,
        rows: &rows,
    };
    let _ = evidence::write(&evidence);
    assert!(rows.iter().all(|row| row.status == RowStatus::Skip));
}

/// Compare the fake and real rows for every vector both drivers ran.
///
/// Returns the number of compared rows, the divergences, and the
/// not-compared entries (`"<id>: <reason>"`).
#[cfg(target_os = "linux")]
fn compare_corpus_rows(
    corpus: &Corpus,
    fake_rows: &[VectorOutcome],
    real_rows: &[VectorOutcome],
) -> (usize, Vec<String>, Vec<String>) {
    let mut compared = 0;
    let mut divergences = Vec::new();
    let mut not_compared = Vec::new();
    for vector in &corpus.vectors {
        let fake_row = fake_rows.iter().find(|row| row.id == vector.id);
        let real_row = real_rows.iter().find(|row| row.id == vector.id);
        let (Some(fake_row), Some(real_row)) = (fake_row, real_row) else {
            continue;
        };
        if vector.known_defect.is_some() {
            not_compared.push(format!("{}: known defect", vector.id));
            continue;
        }
        match (fake_row.status, real_row.status) {
            (RowStatus::Pass, RowStatus::Pass) => {
                compared += 1;
                if fake_row.signature != real_row.signature {
                    divergences.push(format!("{}: typed signature differs", vector.id));
                }
            }
            (RowStatus::Skip, RowStatus::Skip) => {
                not_compared.push(format!("{}: both drivers skip", vector.id));
            }
            (RowStatus::Skip, _) | (_, RowStatus::Skip) => {
                not_compared.push(format!("{}: one driver skips", vector.id));
            }
            _ => {
                divergences.push(format!("{}: one driver fails", vector.id));
            }
        }
    }
    (compared, divergences, not_compared)
}

/// The exact parity expectation for the modeled real cell.
///
/// A served plan runs the accepted 1.3 terminal-spawn vectors on both drivers.
/// A refused plan records the typed `cell-not-servable: terminal_spawn` skip
/// for them, while the registry-only terminal (`V-48`), the worker refusal
/// (`V-03`), and every non-spawn vector still pass on both drivers.
#[cfg(target_os = "linux")]
fn expected_parity(real_serves_spawn: bool, test_seams: bool) -> (usize, Vec<&'static str>) {
    let mut expected_not_compared = vec![
        "V-06", "V-17", "V-18", "V-33", "V-39", "V-40", "V-42", "V-43", "C-01", "C-02", "C-03",
        "C-11", "C-07", "C-08", "C-10", "C-12", "F-01", "C-15", "C-16", "V-49", "V-50", "V-55",
        "V-56", "V-57",
    ];
    if !test_seams {
        // Without the real seams V-20 and V-44 are fake-only rows: the real
        // driver records the typed `cell-not-servable` seam skip.
        expected_not_compared.extend(["V-20", "V-44"]);
    }
    let expected_compared = match (real_serves_spawn, test_seams) {
        (true, true) => 42,
        (true, false) => 40,
        // The terminal-spawn vectors skip on the refused cell. V-48 needs no
        // process or containment plan, so it stays compared. V-52 needs a
        // bound worker, so it joins them.
        (false, true) => {
            expected_not_compared.extend(["V-45", "V-46", "V-47", "V-52"]);
            38
        }
        (false, false) => {
            expected_not_compared.extend(["V-45", "V-46", "V-47", "V-52"]);
            36
        }
    };
    expected_not_compared.sort_unstable();
    (expected_compared, expected_not_compared)
}

#[cfg(target_os = "linux")]
#[test]
fn fake_and_real_parity() {
    let corpus = Corpus::load().expect("corpus");
    let mut fake = FakeDriver::new();
    let fake_rows = run_corpus(&corpus, &mut fake);
    let mut real = RealDriver::new();
    let real_rows = run_corpus(&corpus, &mut real);

    let (compared, divergences, not_compared) =
        compare_corpus_rows(&corpus, &fake_rows, &real_rows);
    let _ = evidence::write_parity(compared, &divergences, &not_compared);
    assert_no_failures(&fake_rows, "fake");
    assert_no_failures(&real_rows, "real");
    assert!(
        real.facts().capabilities.get("worker_sessions").copied() == Some(false),
        "D-01: a served build never advertises worker_sessions"
    );
    let real_serves_spawn = real
        .facts()
        .features
        .get("terminal_spawn")
        .copied()
        .unwrap_or(false);
    let (expected_compared, expected_not_compared) =
        expected_parity(real_serves_spawn, cfg!(feature = "test-seams"));
    assert_eq!(
        compared, expected_compared,
        "the shared passing set changed"
    );
    assert!(
        divergences.is_empty(),
        "parity divergences: {divergences:?}"
    );
    let mut not_compared_ids: Vec<&str> = not_compared
        .iter()
        .map(|entry| entry.split(':').next().unwrap_or(entry))
        .collect();
    not_compared_ids.sort_unstable();
    assert_eq!(
        not_compared_ids, expected_not_compared,
        "the parity report changed its not-compared set"
    );
}

/// Exercise the refused-containment parity path on a host that can spawn.
///
/// The real driver still runs the daemon with the host's plan; it models
/// `terminal_spawn: false`, exactly what a refused probe reports. The registry-
/// only `V-48` must stay compared: it creates no process and needs no
/// containment plan.
#[cfg(target_os = "linux")]
#[test]
fn refused_containment_parity_keeps_the_registry_only_terminal() {
    let corpus = Corpus::load().expect("corpus");
    let mut fake = FakeDriver::new();
    let fake_rows = run_corpus(&corpus, &mut fake);
    let mut real = RealDriver::modeling_refused_containment();
    let real_rows = run_corpus(&corpus, &mut real);

    let (compared, divergences, not_compared) =
        compare_corpus_rows(&corpus, &fake_rows, &real_rows);
    assert_no_failures(&fake_rows, "fake");
    assert_no_failures(&real_rows, "real");
    assert_eq!(
        real.facts().features.get("terminal_spawn").copied(),
        Some(false),
        "the simulation must model a refused containment plan"
    );
    let (expected_compared, expected_not_compared) =
        expected_parity(false, cfg!(feature = "test-seams"));
    assert_eq!(
        compared, expected_compared,
        "the refused-cell shared passing set changed"
    );
    assert!(
        divergences.is_empty(),
        "refused-cell parity divergences: {divergences:?}"
    );
    let mut not_compared_ids: Vec<&str> = not_compared
        .iter()
        .map(|entry| entry.split(':').next().unwrap_or(entry))
        .collect();
    not_compared_ids.sort_unstable();
    assert_eq!(
        not_compared_ids, expected_not_compared,
        "the refused-cell not-compared set changed"
    );
    assert!(
        !not_compared_ids.contains(&"V-48"),
        "the registry-only terminal never needs a containment plan"
    );
    // Evidence is written by `fake_and_real_parity` only, so the simulation
    // cannot race the served-cell report for the same file name.
}

#[cfg(not(target_os = "linux"))]
#[test]
fn fake_and_real_parity() {
    let _ = evidence::write_parity(
        0,
        &[],
        &["cell-not-servable: the real-service row is a typed skip on this cell".to_owned()],
    );
}

fn assert_no_failures(rows: &[VectorOutcome], suite: &str) {
    let failures: Vec<String> = rows
        .iter()
        .filter(|row| row.status == RowStatus::Fail)
        .map(|row| {
            format!(
                "{}: {}",
                row.id,
                row.failure.as_deref().unwrap_or("unknown failure")
            )
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{suite} conformance failures: {failures:#?}"
    );
}

/// Helper child for the peer-exit vector (C-02).
///
/// The parent starts this test binary with `--exact peer_exit_helper --ignored`,
/// stops the daemon, releases the child, and resumes the daemon only after the
/// child has exited. The child speaks the real wire and then exits without
/// reading the reply.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn peer_exit_helper() {
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixStream;

    if std::env::var("BITTY_CONFORMANCE_HELPER").as_deref() != Ok("peer-exit") {
        return;
    }
    let socket = std::env::var("BITTY_CONFORMANCE_SOCKET").expect("socket");
    let capability = std::env::var("BITTY_CONFORMANCE_CAPABILITY").expect("capability");
    let method = std::env::var("BITTY_CONFORMANCE_METHOD").expect("method");
    let params: serde_json::Value =
        serde_json::from_str(&std::env::var("BITTY_CONFORMANCE_PARAMS").expect("params"))
            .expect("params json");
    let stream = UnixStream::connect(&socket).expect("connect");
    bitty_daemon::service::frame::enable_credentials(&stream).expect("credentials");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("timeout");
    let mut writer = stream.try_clone().expect("clone");
    send(
        &mut writer,
        "h1",
        "daemon.negotiate",
        &serde_json::json!({
            "client_version": {"major": 1, "minor": 0},
            "operator_capability": capability,
        }),
    );
    let mut reader = std::io::BufReader::new(stream);
    let mut header = [0u8; 4];
    std::io::Read::read_exact(&mut reader, &mut header).expect("handshake header");
    let length = u32::from_be_bytes(header) as usize;
    let mut payload = vec![0u8; length];
    std::io::Read::read_exact(&mut reader, &mut payload).expect("handshake body");
    writeln!(std::io::stdout(), "ready").expect("ready");
    std::io::stdout().flush().expect("flush");
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .expect("release");
    send(&mut writer, "h2", &method, &params);
    std::process::exit(0);
}

#[cfg(target_os = "linux")]
fn send(
    stream: &mut std::os::unix::net::UnixStream,
    id: &str,
    method: &str,
    params: &serde_json::Value,
) {
    use std::io::Write;
    let payload = serde_json::to_vec(&serde_json::json!({
        "v": 1,
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("encode");
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    stream.write_all(&frame).expect("write frame");
}
