//! CTX-0516 integration: Phase-2 persistence plus Phase-3 detached
//! supervisor coordination over real child processes.
//!
//! Every child under test is this test binary, selected by an explicit
//! environment variable and reached through an argv-first job spec:
//! hermetic, portable, and shell-free (no `sh`/`sleep` PATH dependency), so
//! the same file runs on Unix and Windows CI.
//!
//! Scratch directories live under the platform temp dir
//! (`std::env::temp_dir`, never a hardcoded home or checkout path) and are
//! removed best-effort at the end of each test.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bitty_runtime::{
    AdoptedJob, AdoptionKind, DaemonError, HandoffOffer, IdAllocator, JobId, JobRegistry, JobSpec,
    JobState, JobStop, JobStore, OutputStream, PersistError, ReadOutput, ResumeDecision,
    ScheduleDecision, SchedulePolicy, SupervisorDaemon, adoption_plan, clear_handoff, read_handoff,
    reconcile, write_handoff,
};

const HELPER_ENV: &str = "__BITTY_JOB_PERSIST_HELPER";
const CANARY: &str = "CANARY-CTX0516-PERSISTENCE-PROBE";

/// Child entry point: selected by `HELPER_ENV`, runs only in spawned
/// children (the parent suite runs it as a no-op and it passes).
#[test]
fn __bitty_job_persist_helper_entry__() {
    match std::env::var(HELPER_ENV).as_deref() {
        // Exit immediately without emitting anything.
        Ok("quiet") => {}
        // Stay alive well past every test deadline.
        Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
        // Print the canary once, then exit.
        Ok("say") => println!("{CANARY}"),
        // Print one stderr line, then exit.
        Ok("err") => eprintln!("{CANARY}-on-stderr"),
        _ => {}
    }
}

fn helper_exe() -> String {
    std::env::current_exe()
        .expect("test binary path")
        .to_string_lossy()
        .into_owned()
}

/// Argv-first spec for the test binary in `mode`, with the explicit
/// environment the closed-env backend requires.
fn helper_spec(mode: &str) -> JobSpec {
    JobSpec::new(
        helper_exe(),
        vec![
            "__bitty_job_persist_helper_entry__".to_owned(),
            "--nocapture".to_owned(),
        ],
    )
    .with_env(
        bitty_ipc::execution::EnvPolicy::explicit(vec![(HELPER_ENV.to_owned(), mode.to_owned())])
            .expect("explicit env"),
    )
}

fn wait_for(
    registry: &JobRegistry,
    id: JobId,
    what: &str,
    predicate: impl Fn(&bitty_runtime::JobSnapshot) -> bool,
) -> bitty_runtime::JobSnapshot {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = registry.get(id).expect("job stays tracked");
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "job {id} did not reach {what} in time"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_terminal(registry: &JobRegistry, id: JobId) -> bitty_runtime::JobSnapshot {
    wait_for(registry, id, "a terminal state", |snapshot| {
        snapshot.state.is_terminal()
    })
}

fn wait_running(registry: &JobRegistry, id: JobId) -> bitty_runtime::JobSnapshot {
    wait_for(registry, id, "running", |snapshot| {
        snapshot.state == JobState::Running
    })
}

/// Polls the retained stdout window until it carries `needle`.
fn wait_output(registry: &JobRegistry, id: JobId, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let view = registry
            .read_output(id, ReadOutput::new(OutputStream::Stdout))
            .expect("readable");
        if view.text.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "retained output never carried the canary"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Fresh scratch store directory, removed best-effort by the caller.
fn scratch_store(tag: &str) -> (JobStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx0516-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    (JobStore::new(dir.clone()), dir)
}

fn drop_scratch(dir: &PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn checkpoint_roundtrip_preserves_metadata_index_and_cursors() {
    let registry = JobRegistry::new();
    let said = registry.spawn(helper_spec("say")).expect("tracked");
    wait_terminal(&registry, said);
    wait_output(&registry, said, CANARY);
    let quiet = registry.spawn(helper_spec("quiet")).expect("tracked");
    wait_terminal(&registry, quiet);

    let (store, dir) = scratch_store("roundtrip");
    let summary = store.checkpoint(&registry).expect("checkpoint");
    assert_eq!(summary.jobs, 2);
    assert_eq!(summary.event_head_seq, registry.event_head_seq());

    let loaded = store.load().expect("load");
    assert_eq!(loaded.jobs.len(), 2);
    assert_eq!(loaded.cursor.event_head_seq, registry.event_head_seq());
    assert_eq!(
        loaded.cursor.max_issued_id,
        said.get().max(quiet.get()),
        "the cursor must cover the highest issued id"
    );
    for record in &loaded.jobs {
        record.rebuild_spec().expect("persisted spec stays valid");
    }
    let said_row = loaded
        .jobs
        .iter()
        .find(|row| row.id == said)
        .expect("said row");
    assert_eq!(said_row.state, JobState::Done(JobStop::Exited));
    assert!(said_row.finished_at_ms.is_some());
    assert!(said_row.output.stdout_total_bytes > 0);
    drop_scratch(&dir);
}

#[test]
fn manifest_carries_no_raw_output_while_logs_carry_the_bytes() {
    let registry = JobRegistry::new();
    let said = registry.spawn(helper_spec("say")).expect("tracked");
    wait_terminal(&registry, said);
    wait_output(&registry, said, CANARY);

    let (store, dir) = scratch_store("byteshape");
    store.checkpoint(&registry).expect("checkpoint");

    let manifest = std::fs::read_to_string(dir.join(bitty_runtime::MANIFEST_FILE_NAME))
        .expect("manifest readable");
    assert!(
        !manifest.contains(CANARY),
        "raw stdout must never enter the metadata manifest"
    );
    let loaded = store.load().expect("load");
    let said_row = loaded
        .jobs
        .iter()
        .find(|row| row.id == said)
        .expect("said row");
    let spilled = store
        .read_spilled(said_row, OutputStream::Stdout)
        .expect("spilled log");
    assert!(
        spilled.contains(CANARY),
        "the file-held log carries the retained bytes"
    );
    assert!(
        said_row.output.stdout_total_bytes >= u64::try_from(CANARY.len() + 1).expect("canary fits"),
        "the index total covers everything the child wrote (the canary plus \
         the test-harness chatter the helper binary emits on stdout)"
    );
    drop_scratch(&dir);
}

#[test]
fn restart_marks_live_jobs_unknown_and_keeps_terminal_facts() {
    let registry = JobRegistry::new();
    let live = registry.spawn(helper_spec("sleep")).expect("tracked");
    wait_running(&registry, live);
    let done = registry.spawn(helper_spec("quiet")).expect("tracked");
    wait_terminal(&registry, done);

    let (store, dir) = scratch_store("reconcile");
    store.checkpoint(&registry).expect("checkpoint");
    let reconciled = reconcile(&store.load().expect("load"));
    assert_eq!(reconciled.len(), 2);

    let live_row = reconciled
        .iter()
        .find(|row| row.record.id == live)
        .expect("live row");
    assert_eq!(live_row.decision, ResumeDecision::UnknownOutcome);
    assert!(
        !live_row.record.state.is_terminal(),
        "reconciliation must never rewrite a live job as terminal"
    );
    let done_row = reconciled
        .iter()
        .find(|row| row.record.id == done)
        .expect("done row");
    assert_eq!(done_row.decision, ResumeDecision::TerminalFacts);
    assert_eq!(done_row.record.state, JobState::Done(JobStop::Exited));

    // Adoption observes facts and never restarts the unknown job.
    let plan = adoption_plan(&reconciled);
    assert_eq!(plan.len(), 2);
    let adopted_live = plan
        .iter()
        .find(|adopted| adopted.id == live)
        .expect("adopted live");
    assert_eq!(
        adopted_live.kind,
        AdoptionKind::UnknownRequiresExplicitRespawn
    );
    let adopted_done: &AdoptedJob = plan
        .iter()
        .find(|adopted| adopted.id == done)
        .expect("adopted done");
    assert_eq!(adopted_done.kind, AdoptionKind::SurvivingFacts);

    assert_eq!(
        registry.cancel(live),
        Ok(bitty_runtime::JobCancel::Requested)
    );
    drop_scratch(&dir);
}

#[test]
fn allocator_floor_stays_disjoint_from_persisted_ids() {
    let registry = JobRegistry::new();
    let mut issued = Vec::new();
    for _ in 0..3 {
        let id = registry.spawn(helper_spec("quiet")).expect("tracked");
        wait_terminal(&registry, id);
        issued.push(id);
    }
    let (store, dir) = scratch_store("allocator");
    let summary = store.checkpoint(&registry).expect("checkpoint");
    let loaded = store.load().expect("load");

    let mut allocator = IdAllocator::resume_from(loaded.cursor.max_issued_id);
    assert_eq!(allocator.floor(), summary.max_issued_id + 1);
    for _ in 0..3 {
        let fresh = allocator.allocate().expect("id space");
        assert!(
            !issued.contains(&fresh),
            "post-restart id {fresh} must not reuse a persisted id"
        );
    }
    drop_scratch(&dir);
}

#[test]
fn corrupt_and_oversized_manifests_fail_closed() {
    let registry = JobRegistry::new();
    let id = registry.spawn(helper_spec("quiet")).expect("tracked");
    wait_terminal(&registry, id);
    let (store, dir) = scratch_store("corrupt");
    store.checkpoint(&registry).expect("checkpoint");
    let manifest_path = dir.join(bitty_runtime::MANIFEST_FILE_NAME);
    let valid = std::fs::read_to_string(&manifest_path).expect("manifest");

    // Wrong magic.
    std::fs::write(&manifest_path, "NOPE\tv1\t0\t0\n").expect("write");
    assert!(matches!(store.load(), Err(PersistError::Corrupt { .. })));

    // Unsupported version.
    let bad_version = valid.replacen("v1", "v9999", 1);
    std::fs::write(&manifest_path, bad_version).expect("write");
    assert!(matches!(store.load(), Err(PersistError::Corrupt { .. })));

    // Truncated row.
    let mut truncated = valid.clone();
    truncated.truncate(valid.len().saturating_sub(4));
    std::fs::write(&manifest_path, truncated).expect("write");
    assert!(store.load().is_err());

    // Duplicate ids: the first job line appended twice.
    let first_row = valid.lines().nth(1).expect("one job line").to_owned();
    let mut duplicated = valid.clone();
    duplicated.push_str(&first_row);
    duplicated.push('\n');
    std::fs::write(&manifest_path, duplicated).expect("write");
    assert!(matches!(store.load(), Err(PersistError::Corrupt { .. })));

    // Over-bound program smuggled into a valid row.
    let hostile_program = "x".repeat(200_000);
    let mut lines: Vec<String> = valid.lines().map(str::to_owned).collect();
    let row_fields: Vec<String> = lines[1].split('\t').map(str::to_owned).collect();
    let mut patched = row_fields;
    patched[6] = hostile_program;
    lines[1] = patched.join("\t");
    let hostile = lines.join("\n") + "\n";
    std::fs::write(&manifest_path, hostile).expect("write");
    assert!(store.load().is_err(), "an over-bound spec must fail closed");

    // Oversized manifest.
    let big = "z".repeat(bitty_runtime::MAX_MANIFEST_BYTES + 1);
    std::fs::write(&manifest_path, big).expect("write");
    assert!(matches!(store.load(), Err(PersistError::TooLarge { .. })));

    drop_scratch(&dir);
}

#[test]
fn daemon_claim_is_exclusive_and_stale_locks_are_adoptable() {
    let (_store, dir) = scratch_store("daemon");
    let owner = SupervisorDaemon::claim(&dir).expect("first claim");
    assert!(owner.is_owner());
    owner.heartbeat().expect("heartbeat");

    // A live owner denies the second claimant without touching the lock.
    match SupervisorDaemon::claim(&dir) {
        Err(DaemonError::AlreadyOwned { .. }) => {}
        other => panic!("second claim must deny, got {other:?}"),
    }

    // A stopped owner makes the lock adoptable (crash adoption): age both
    // the claim stamp and the heartbeat, as a long-dead owner would leave
    // them. Ageing the heartbeat alone must NOT adopt — a fresh claim may
    // not have written its first beat yet.
    let stale = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
        .saturating_sub(bitty_runtime::STALE_HEARTBEAT_MS + 5_000);
    std::fs::write(
        dir.join(bitty_runtime::SUPERVISOR_HEARTBEAT_NAME),
        stale.to_string(),
    )
    .expect("age the heartbeat");
    std::mem::forget(owner);
    match SupervisorDaemon::claim(&dir) {
        Err(DaemonError::AlreadyOwned { .. }) => {}
        other => panic!("a fresh claim with a stale beat must still deny, got {other:?}"),
    }
    std::fs::write(
        dir.join(bitty_runtime::SUPERVISOR_LOCK_NAME),
        format!(
            "BITTY-SUPERVISOR\tv{}\t{}\t{stale}",
            bitty_runtime::SUPERVISOR_FORMAT_VERSION,
            std::process::id()
        ),
    )
    .expect("age the claim");
    let adopted = SupervisorDaemon::claim(&dir).expect("stale lock adopts");
    assert!(adopted.is_owner());
    adopted.heartbeat().expect("heartbeat after adopt");

    // Release frees the directory; a missing lock releases cleanly.
    adopted.release().expect("release");
    assert!(read_handoff(&dir).expect("no handoff").is_none());
    let reclaim = SupervisorDaemon::claim(&dir).expect("reclaim after release");
    reclaim.release().expect("second release");
    drop_scratch(&dir);
}

#[test]
fn handoff_roundtrip_names_jobs_and_cursor_then_clears() {
    let registry = JobRegistry::new();
    let first = registry.spawn(helper_spec("sleep")).expect("tracked");
    wait_running(&registry, first);
    let second = registry.spawn(helper_spec("quiet")).expect("tracked");
    wait_terminal(&registry, second);

    let (_store, dir) = scratch_store("handoff");
    let offer = HandoffOffer::from_registry(&registry, 1_717_000_000_000);
    assert_eq!(offer.job_ids.len(), 2);
    assert_eq!(offer.event_cursor, registry.event_head_seq());
    write_handoff(&dir, &offer).expect("write handoff");
    let back = read_handoff(&dir).expect("read handoff").expect("present");
    assert_eq!(back, offer);
    clear_handoff(&dir).expect("clear");
    assert!(read_handoff(&dir).expect("read").is_none());

    // A foreign note with duplicate ids fails closed.
    let duplicated = HandoffOffer {
        from_pid: 11,
        at_ms: 12,
        event_cursor: 13,
        job_ids: vec![first, first],
    };
    assert!(write_handoff(&dir, &duplicated).is_err());

    assert_eq!(
        registry.cancel(first),
        Ok(bitty_runtime::JobCancel::Requested)
    );
    drop_scratch(&dir);
}

#[test]
fn schedule_admits_fifo_and_defers_past_the_ceiling() {
    let policy = SchedulePolicy::new(1).expect("policy");
    let first = JobId::from_raw(40).expect("id");
    let second = JobId::from_raw(41).expect("id");
    assert_eq!(policy.admit(0), ScheduleDecision::Admit);
    assert_eq!(policy.admit(1), ScheduleDecision::Defer);
    assert_eq!(policy.select_next(&[first, second]), Some(first));
    assert_eq!(policy.select_next(&[]), None);
}
