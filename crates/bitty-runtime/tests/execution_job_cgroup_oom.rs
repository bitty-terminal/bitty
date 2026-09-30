//! CTX-0880 (#1537): per-job cgroup v2 OOM evidence.
//!
//! - Hosts without delegation (or registries built without cgroups) keep
//!   `Unknown` evidence with a typed gap, and a SIGKILL death stays
//!   `Signaled(9)`. These run everywhere.
//! - With a real delegated subtree (Linux, discovered from this process's
//!   own cgroup) a memory-limited job that allocates past its limit ends
//!   `OomKilled`, a plain SIGKILL stays `Signaled(9)` with `NotOom`, and
//!   every leaf is removed after its job. These skip with a printed reason
//!   when no delegation exists (CI runners typically have none).
//!
//! Setting `memory.max` / `memory.swap.max` on a leaf is test-only: the
//! runtime never sets job limits (CTX-0519 owns them). Children are this
//! test binary, selected by an explicit environment variable. Scratch files
//! live under `std::env::temp_dir()`.

use std::time::{Duration, Instant};

#[cfg(unix)]
use bitty_runtime::ExecutionOutcome;
use bitty_runtime::{
    CgroupUnavailable, JobCgroups, JobId, JobPrincipal, JobRegistry, JobSignal, JobSnapshot,
    JobSpec, JobState, OomEvidence, OomEvidenceGap, OomVerdict, ProcessTreeBackend, SignalOutcome,
};

const HELPER_ENV: &str = "__BITTY_CGROUP_OOM_HELPER";

/// Marker the memory hog waits for before allocating (the test creates it
/// once the leaf's limit is set).
const MARKER_ENV: &str = "__BITTY_CGROUP_OOM_MARKER";

/// Test-only leaf memory limit (16 MiB).
#[cfg(target_os = "linux")]
const TEST_MEMORY_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// What the hog tries to allocate: far past the limit (512 MiB).
const HOG_BYTES: usize = 512 * 1024 * 1024;

/// Allocation step of the hog (1 MiB, every page touched).
const HOG_CHUNK_BYTES: usize = 1024 * 1024;

/// Page stride used to touch the hog's memory.
const PAGE_BYTES: usize = 4096;

/// Upper bound for any single wait in these probes.
const WAIT_BOUND: Duration = Duration::from_secs(30);

/// Pause between polls.
const POLL: Duration = Duration::from_millis(5);

/// POSIX `SIGKILL`.
#[cfg(unix)]
const SIGKILL: i32 = 9;

/// Child entry point: selected by `HELPER_ENV`, a no-op in the parent suite.
#[test]
fn __bitty_cgroup_oom_helper_entry__() {
    match std::env::var(HELPER_ENV).as_deref() {
        Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
        Ok("hog") => hog(),
        _ => {}
    }
}

/// Waits for the marker, then allocates and touches memory until killed.
fn hog() {
    let marker = std::env::var_os(MARKER_ENV).map(std::path::PathBuf::from);
    let deadline = Instant::now() + WAIT_BOUND;
    while let Some(marker) = &marker {
        if marker.exists() || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL);
    }
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut total = 0;
    while total < HOG_BYTES {
        let mut chunk = vec![0u8; HOG_CHUNK_BYTES];
        for index in (0..chunk.len()).step_by(PAGE_BYTES) {
            chunk[index] = 1;
        }
        held.push(chunk);
        total += HOG_CHUNK_BYTES;
    }
    // Only reached when the limit did not bite; the exit code shows it.
    std::process::exit(i32::from(held.len() > 1));
}

fn helper_spec(mode: &str, marker: Option<&std::path::Path>) -> JobSpec {
    let mut vars = vec![(HELPER_ENV.to_owned(), mode.to_owned())];
    if let Some(marker) = marker {
        vars.push((MARKER_ENV.to_owned(), marker.to_string_lossy().into_owned()));
    }
    JobSpec::new(
        std::env::current_exe()
            .expect("test binary path")
            .to_string_lossy()
            .into_owned(),
        vec![
            "__bitty_cgroup_oom_helper_entry__".to_owned(),
            "--nocapture".to_owned(),
        ],
    )
    .with_env(bitty_ipc::execution::EnvPolicy::explicit(vars).expect("explicit env"))
}

fn wait_for(
    registry: &JobRegistry,
    id: JobId,
    what: &str,
    predicate: impl Fn(&JobSnapshot) -> bool,
) -> JobSnapshot {
    let deadline = Instant::now() + WAIT_BOUND;
    loop {
        let snapshot = registry.get(id).expect("job stays tracked");
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "job {id} did not reach {what} in time"
        );
        std::thread::sleep(POLL);
    }
}

fn wait_running(registry: &JobRegistry, id: JobId) -> JobSnapshot {
    wait_for(registry, id, "running", |snapshot| {
        snapshot.state == JobState::Running
    })
}

fn wait_terminal(registry: &JobRegistry, id: JobId) -> JobSnapshot {
    wait_for(registry, id, "a terminal state", |snapshot| {
        snapshot.state.is_terminal()
    })
}

/// SIGKILLs a running job through its owned tree and returns the stopped
/// snapshot, or `None` where no tree backend exists.
fn kill_running(registry: &JobRegistry, owner: &JobPrincipal, id: JobId) -> Option<JobSnapshot> {
    wait_running(registry, id);
    if !ProcessTreeBackend::detect().kills_owned_tree() {
        return None;
    }
    assert_eq!(
        registry.signal_as(owner, id, JobSignal::Kill),
        Ok(SignalOutcome::Delivered)
    );
    Some(wait_terminal(registry, id))
}

#[test]
fn registries_without_cgroups_keep_unknown_evidence() {
    let plain = JobRegistry::new();
    assert_eq!(plain.cgroup_unavailable(), None);
    assert_eq!(plain.job_cgroup_base(), None);
    let undelegated = JobRegistry::with_job_cgroups(4, Err(CgroupUnavailable::NotWritable));
    assert_eq!(
        undelegated.cgroup_unavailable(),
        Some(CgroupUnavailable::NotWritable)
    );
    let unconfigured = if cfg!(target_os = "linux") {
        OomEvidenceGap::NotConfigured
    } else {
        OomEvidenceGap::UnsupportedPlatform
    };
    let owner = JobPrincipal::new("owner-a").expect("principal");
    for (registry, gap) in [
        (plain, unconfigured),
        (undelegated, OomEvidenceGap::Undelegated),
    ] {
        let id = registry
            .spawn_as(owner.clone(), helper_spec("sleep", None))
            .expect("tracked");
        let running = wait_running(&registry, id);
        assert_eq!(running.oom_evidence, OomEvidence::Missing(gap));
        let Some(stopped) = kill_running(&registry, &owner, id) else {
            assert!(registry.cancel_as(&owner, id).is_ok());
            continue;
        };
        #[cfg(unix)]
        assert_eq!(
            stopped.state,
            JobState::Done(ExecutionOutcome::Signaled(SIGKILL)),
            "no evidence: a SIGKILL death is never claimed as an OOM kill"
        );
        assert_eq!(stopped.oom_evidence, OomEvidence::Missing(gap));
        assert_eq!(stopped.oom_evidence.verdict(), OomVerdict::Unknown);
    }
}

#[test]
fn discovery_off_linux_reports_the_platform() {
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        JobCgroups::discover().map(drop),
        Err(CgroupUnavailable::UnsupportedPlatform)
    );
    #[cfg(target_os = "linux")]
    {
        // Either outcome is valid on Linux; an error must be typed.
        if let Err(reason) = JobCgroups::discover() {
            assert_ne!(reason, CgroupUnavailable::UnsupportedPlatform);
        }
    }
}

/// A fake base (plain temp dir, no kernel counters): the spawn proceeds,
/// and the job records the missing counter instead of guessing.
#[cfg(target_os = "linux")]
#[test]
fn a_base_without_kernel_counters_records_the_gap_and_still_spawns() {
    let root = std::env::temp_dir().join(format!("bitty-ctx0880-fake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("fake root");
    std::fs::write(root.join("cgroup.subtree_control"), "memory").expect("fake control");
    let registry = JobRegistry::with_job_cgroups(4, JobCgroups::under(&root));
    let base = registry.job_cgroup_base().expect("fake base created");
    let id = registry.spawn(helper_spec("quiet", None)).expect("tracked");
    let stopped = wait_terminal(&registry, id);
    assert_eq!(stopped.state, JobState::Done(ExecutionOutcome::Success));
    assert_eq!(
        stopped.oom_evidence,
        OomEvidence::Missing(OomEvidenceGap::CounterUnreadable)
    );
    let leaves = std::fs::read_dir(&base)
        .expect("base readable")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .count();
    assert_eq!(leaves, 0, "the leaf without a counter was removed");
    // A real cgroupfs base holds only kernel files; the fake one keeps the
    // control file `under` wrote, so remove it for an immediate rmdir.
    let _ = std::fs::remove_file(base.join("cgroup.subtree_control"));
    drop(registry);
    assert!(!base.exists(), "the base is removed on drop");
    let _ = std::fs::remove_dir_all(&root);
}

/// The live, delegated half. Returns `None` (after printing why) when this
/// host has no usable delegated cgroup v2 subtree.
#[cfg(target_os = "linux")]
fn delegated_registry(test: &str) -> Option<JobRegistry> {
    match JobCgroups::discover() {
        Ok(cgroups) => {
            eprintln!("{test}: ran against delegated base {:?}", cgroups.base());
            Some(JobRegistry::with_job_cgroups(4, Ok(cgroups)))
        }
        Err(reason) => {
            eprintln!("{test}: skipped, no delegated cgroup v2 subtree ({reason})");
            None
        }
    }
}

/// Leaf directories currently under `base`.
#[cfg(target_os = "linux")]
fn leaves(base: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(base)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

/// Drops the registry and waits (bounded) until its job base is removed.
#[cfg(target_os = "linux")]
fn drop_and_expect_base_removed(registry: JobRegistry, base: &std::path::Path) {
    drop(registry);
    let deadline = Instant::now() + WAIT_BOUND;
    while base.exists() {
        assert!(
            Instant::now() < deadline,
            "job base {base:?} was not removed"
        );
        std::thread::sleep(POLL);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_memory_limited_job_past_its_limit_ends_oom_killed() {
    let Some(registry) = delegated_registry("oom_killed") else {
        return;
    };
    let base = registry.job_cgroup_base().expect("delegated base");
    let marker = std::env::temp_dir().join(format!(
        "bitty-ctx0880-marker-{}-{}",
        std::process::id(),
        base.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_file(&marker);
    let id = registry
        .spawn(helper_spec("hog", Some(&marker)))
        .expect("tracked");
    let running = wait_running(&registry, id);
    assert_eq!(running.oom_evidence, OomEvidence::Tracked);
    let leaf = {
        let found = leaves(&base);
        assert_eq!(found.len(), 1, "one leaf per running job");
        found[0].clone()
    };
    // Test-only limit: no swap, so the kernel OOM-kills instead of paging.
    std::fs::write(leaf.join("memory.swap.max"), "0").expect("memory.swap.max");
    std::fs::write(leaf.join("memory.max"), TEST_MEMORY_MAX_BYTES.to_string()).expect("memory.max");
    std::fs::write(&marker, b"go").expect("marker");
    let stopped = wait_terminal(&registry, id);
    let _ = std::fs::remove_file(&marker);
    assert_eq!(stopped.state, JobState::Done(ExecutionOutcome::OomKilled));
    assert_eq!(stopped.oom_evidence, OomEvidence::OomKilled);
    assert!(!leaf.exists(), "the leaf is removed after the job");
    assert_eq!(registry.unremoved_cgroup_leaves(), 0);
    drop_and_expect_base_removed(registry, &base);
}

#[cfg(target_os = "linux")]
#[test]
fn a_plain_sigkill_in_a_leaf_stays_signaled_with_not_oom() {
    let Some(registry) = delegated_registry("sigkill") else {
        return;
    };
    let base = registry.job_cgroup_base().expect("delegated base");
    let owner = JobPrincipal::new("owner-a").expect("principal");
    let id = registry
        .spawn_as(owner.clone(), helper_spec("sleep", None))
        .expect("tracked");
    assert_eq!(
        wait_running(&registry, id).oom_evidence,
        OomEvidence::Tracked
    );
    assert_eq!(leaves(&base).len(), 1);
    let stopped = kill_running(&registry, &owner, id).expect("linux has a tree backend");
    assert_eq!(
        stopped.state,
        JobState::Done(ExecutionOutcome::Signaled(SIGKILL))
    );
    assert_eq!(stopped.oom_evidence, OomEvidence::NotOom);
    assert!(leaves(&base).is_empty(), "the leaf is removed after exit");
    // A cancelled job also takes its final reading and removes its leaf.
    let cancelled = registry
        .spawn_as(owner.clone(), helper_spec("sleep", None))
        .expect("tracked");
    wait_running(&registry, cancelled);
    assert!(registry.cancel_as(&owner, cancelled).is_ok());
    let stopped = wait_terminal(&registry, cancelled);
    assert!(matches!(
        stopped.state,
        JobState::Done(ExecutionOutcome::Cancelled(_))
    ));
    assert_eq!(stopped.oom_evidence, OomEvidence::NotOom);
    assert!(leaves(&base).is_empty());
    drop_and_expect_base_removed(registry, &base);
}
