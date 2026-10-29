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
    assert_eq!(corpus.vectors.len(), 42, "corpus size changed");
    for id in [
        "C-01", "C-02", "C-03", "C-04", "C-05", "C-06", "C-07", "C-08",
    ] {
        assert!(corpus.vector(id).is_some(), "missing Core vector {id}");
    }
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
            "V-18", "V-39", "V-40", "C-01", "C-02", "C-03", "C-11", "C-07", "C-08"
        ],
        "the fake suite skips exactly the real-only vectors"
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
        .map(|row| row.id.as_str())
        .collect();
    assert_eq!(
        skipped,
        ["V-06", "V-33", "C-10", "F-01"],
        "only the injected-clock, adapter-owned, and injected-queue vectors skip on Linux"
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

#[cfg(target_os = "linux")]
#[test]
fn fake_and_real_parity() {
    let corpus = Corpus::load().expect("corpus");
    let mut fake = FakeDriver::new();
    let fake_rows = run_corpus(&corpus, &mut fake);
    let mut real = RealDriver::new();
    let real_rows = run_corpus(&corpus, &mut real);

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
    let _ = evidence::write_parity(compared, &divergences, &not_compared);
    assert_no_failures(&fake_rows, "fake");
    assert_no_failures(&real_rows, "real");
    assert_eq!(compared, 29, "the shared passing set changed");
    assert!(
        divergences.is_empty(),
        "parity divergences: {divergences:?}"
    );
    let not_compared_ids: Vec<&str> = not_compared
        .iter()
        .map(|entry| entry.split(':').next().unwrap_or(entry))
        .collect();
    assert_eq!(
        not_compared_ids,
        [
            "V-06", "V-18", "V-33", "V-39", "V-40", "C-01", "C-02", "C-03", "C-11", "C-07", "C-08",
            "C-10", "F-01"
        ],
        "the parity report changed its not-compared set"
    );
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
