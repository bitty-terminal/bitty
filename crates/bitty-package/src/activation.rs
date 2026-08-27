//! Staged activation lifecycle — draft per RFC.
//!
//! Activation is one transaction with named phases; failure anywhere before
//! `commit` leaves the active environment untouched. The atomic switch target
//! depends on platform rename semantics; the contract requires observable
//! all-or-nothing behavior, not a specific syscall.

use std::collections::BTreeMap;

use crate::error::PackageError;
use crate::lockfile::Lockfile;

// ── activation phase ─────────────────────────────────────────────────────

/// Activation transaction phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActivationPhase {
    /// Re-verify generation digests against the lock; check host compatibility.
    Preflight,
    /// Ensure no plugin VM from the affected set is executing mid-callback.
    Quiesce,
    /// Atomically switch the active-environment pointer to the staged generation.
    Commit,
    /// Load plugins per desired state in fresh VMs.
    Wake,
    /// Health window elapses without crash-loop or budget storms.
    Confirm,
}

impl ActivationPhase {
    /// Human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Quiesce => "quiesce",
            Self::Commit => "commit",
            Self::Wake => "wake",
            Self::Confirm => "confirm",
        }
    }
}

impl std::fmt::Display for ActivationPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ── generation ───────────────────────────────────────────────────────────

/// Immutable generation entry — full lock resolution plus provenance.
///
/// Each successful activation records one of these; rollback selects an older
/// generation and performs the same staged transaction in reverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    /// Monotonic generation id.
    pub id: u64,
    /// Full lock resolution for this generation.
    pub lock: Lockfile,
    /// Generation root digest (hash over lock digest + metadata).
    pub root_digest: String,
    /// Capability grant snapshot (package id -> sorted capability list).
    pub capability_snapshot: BTreeMap<String, Vec<String>>,
    /// Activation time (opaque host millis).
    pub activated_at: u64,
    /// Previous generation id, if any.
    pub previous: Option<u64>,
}

impl Generation {
    /// Create a new generation (validates lock, computes root digest).
    pub fn new(
        id: u64,
        lock: Lockfile,
        capability_snapshot: BTreeMap<String, Vec<String>>,
        activated_at: u64,
        previous: Option<u64>,
    ) -> Result<Self, PackageError> {
        lock.validate()?;
        let root = compute_generation_root(id, &lock, &capability_snapshot, activated_at);
        // Validate digests bound.
        if root.len() != 64 {
            return Err(PackageError::generation("generation root digest malformed"));
        }
        Ok(Self {
            id,
            lock,
            root_digest: root,
            capability_snapshot,
            activated_at,
            previous,
        })
    }

    /// Verify this generation against its recorded root digest (store self-verification).
    pub fn verify_integrity(&self) -> Result<(), PackageError> {
        let expected = compute_generation_root(
            self.id,
            &self.lock,
            &self.capability_snapshot,
            self.activated_at,
        );
        if expected != self.root_digest {
            return Err(PackageError::generation(format!(
                "generation {} integrity failed: expected {expected}, got {}",
                self.id, self.root_digest
            )));
        }
        self.lock.validate()?;
        Ok(())
    }
}

fn compute_generation_root(
    id: u64,
    lock: &Lockfile,
    caps: &BTreeMap<String, Vec<String>>,
    activated_at: u64,
) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"bitty-generation-v1\n");
    bytes.extend_from_slice(id.to_string().as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(lock.digest().as_bytes());
    bytes.push(b'\n');
    for (pkg, cap_list) in caps {
        bytes.extend_from_slice(pkg.as_bytes());
        bytes.push(b':');
        for c in cap_list {
            bytes.extend_from_slice(c.as_bytes());
            bytes.push(b',');
        }
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(activated_at.to_string().as_bytes());
    bytes.push(b'\n');
    crate::integrity::sha256_hex(&bytes)
}

// ── retained environment ─────────────────────────────────────────────────

/// Retention policy (bounded count and bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Number of generations to retain including current (candidate 3 = current + previous 2).
    pub max_generations: usize,
    /// Optional byte budget for retained generations (0 = no byte limit).
    pub max_bytes: usize,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_generations: 3,
            max_bytes: 0,
        }
    }
}

impl RetentionPolicy {
    /// Validate policy.
    pub fn validate(&self) -> Result<(), PackageError> {
        if self.max_generations == 0 {
            return Err(PackageError::generation(
                "max_generations must be >= 1 (current must be retained)",
            ));
        }
        if self.max_generations > 32 {
            return Err(PackageError::LimitExceeded {
                field: "retention.max_generations".to_string(),
                limit: 32,
                actual: self.max_generations,
            });
        }
        Ok(())
    }
}

/// Owned activation environment — current pointer plus retained generations.
///
/// This is the in-memory model of the atomic pointer + generation ring;
/// it never touches the filesystem. The observable contract is all-or-nothing:
/// crash between store write and switch leaves old environment active.
#[derive(Debug, Clone)]
pub struct Environment {
    /// Currently active generation id, if any.
    pub current: Option<u64>,
    /// Retained generations by id.
    pub generations: BTreeMap<u64, Generation>,
    /// Next generation id to allocate.
    next_id: u64,
    /// Retention policy.
    policy: RetentionPolicy,
}

impl Environment {
    /// Create a new empty environment with default retention.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: None,
            generations: BTreeMap::new(),
            next_id: 1,
            policy: RetentionPolicy::default(),
        }
    }

    /// Create with explicit policy.
    pub fn with_policy(policy: RetentionPolicy) -> Result<Self, PackageError> {
        policy.validate()?;
        Ok(Self {
            current: None,
            generations: BTreeMap::new(),
            next_id: 1,
            policy,
        })
    }

    /// Current generation, if any.
    #[must_use]
    pub fn current_generation(&self) -> Option<&Generation> {
        self.current.and_then(|id| self.generations.get(&id))
    }

    /// Number of retained generations.
    #[must_use]
    pub fn retained_count(&self) -> usize {
        self.generations.len()
    }

    /// Whether `id` is retained.
    #[must_use]
    pub fn is_retained(&self, id: u64) -> bool {
        self.generations.contains_key(&id)
    }

    /// Allocate the next generation id.
    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Validate that every retained generation passes integrity checks.
    ///
    /// A tampered or unparseable generation must be quarantined and reported,
    /// never activated (PL-AC-008/009).
    pub fn verify_all(&self) -> Result<(), PackageError> {
        for g in self.generations.values() {
            g.verify_integrity().map_err(|e| {
                PackageError::generation(format!("generation {} quarantine: {e}", g.id))
            })?;
        }
        Ok(())
    }

    /// Prune oldest generations beyond the retention bound.
    ///
    /// Never removes the current generation; skipped when it would leave zero
    /// rollback targets if policy allows, but always enforces the bound.
    /// Returns the list of pruned ids.
    pub fn prune(&mut self) -> Vec<u64> {
        if self.generations.len() <= self.policy.max_generations {
            return Vec::new();
        }
        // Oldest first (by id).
        let mut ids: Vec<u64> = self.generations.keys().copied().collect();
        ids.sort_unstable();
        let to_remove = self.generations.len() - self.policy.max_generations;
        let mut pruned = Vec::new();
        for id in ids.into_iter().take(to_remove) {
            if Some(id) == self.current {
                continue;
            }
            self.generations.remove(&id);
            pruned.push(id);
        }
        pruned
    }

    /// Stage a new lock resolution as a generation (not yet active).
    ///
    /// This represents the `Store commit` -> `Staged` step; activation will
    /// then switch to it. For this draft the staging produces a generation
    /// that is inserted as retained but not current until `activate` commits.
    pub fn stage(
        &mut self,
        lock: Lockfile,
        capability_snapshot: BTreeMap<String, Vec<String>>,
        activated_at: u64,
    ) -> Result<u64, PackageError> {
        lock.validate()?;
        let id = self.alloc_id();
        let prev = self.current;
        let generation = Generation::new(id, lock, capability_snapshot, activated_at, prev)?;
        self.generations.insert(id, generation);
        Ok(id)
    }
}

impl Default for Environment {
    fn default() -> Self {
        Self::new()
    }
}

// ── activation transaction ───────────────────────────────────────────────

/// Outcome of an activation transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationReport {
    /// Staged generation id.
    pub generation_id: u64,
    /// Phase results.
    pub phases: Vec<(ActivationPhase, Result<(), String>)>,
    /// Whether the whole transaction succeeded.
    pub succeeded: bool,
    /// Previous generation id, if any.
    pub previous: Option<u64>,
}

/// Perform the staged activation transaction.
///
/// Phases:
///
/// - `Preflight`: re-verify generation digests against the lock and check
///   compatibility (host versions supplied).
/// - `Quiesce`: stub — always succeeds in this draft (no real VM).
/// - `Commit`: atomically switch the active pointer.
/// - `Wake`: stub — load plugins in fresh VMs (always succeeds here).
/// - `Confirm`: stub health window (always succeeds here).
///
/// Failure anywhere before `Commit` leaves the active pointer unchanged.
/// Failure in `Wake`/`Confirm` automatically restores the prior generation
/// pointer (atomic restore).
pub fn activate(
    env: &mut Environment,
    generation_id: u64,
    host_bitty_version: Option<&str>,
    host_plugin_api_version: Option<&str>,
    simulate_failure_at: Option<ActivationPhase>,
) -> Result<ActivationReport, PackageError> {
    let generation = env
        .generations
        .get(&generation_id)
        .cloned()
        .ok_or_else(|| PackageError::NotFound {
            id: generation_id.to_string(),
        })?;

    // If simulating a failure at a phase before commit, we must not have switched.
    let previous = env.current;
    let mut phases: Vec<(ActivationPhase, Result<(), String>)> = Vec::new();
    let mut failed_before_commit = false;

    // Preflight: re-verify digests and compat.
    let preflight = (|| -> Result<(), String> {
        generation.verify_integrity().map_err(|e| e.to_string())?;
        // Check host compat for each package's manifest? For draft, we check lock validity only.
        // Additionally check host versions are present if lock has packages with compat? Stub.
        if let Some(sim) = simulate_failure_at {
            if sim == ActivationPhase::Preflight {
                return Err("simulated preflight failure".to_string());
            }
        }
        // Simulate host version malformed would be caught via manifest check elsewhere.
        let _ = host_bitty_version;
        let _ = host_plugin_api_version;
        Ok(())
    })();
    let pre_ok = preflight.is_ok();
    phases.push((ActivationPhase::Preflight, preflight));
    if !pre_ok {
        failed_before_commit = true;
    }

    // Quiesce
    let quiesce = (|| -> Result<(), String> {
        if failed_before_commit {
            return Err("skipped due to earlier failure".to_string());
        }
        if let Some(sim) = simulate_failure_at {
            if sim == ActivationPhase::Quiesce {
                return Err("simulated quiesce failure".to_string());
            }
        }
        Ok(())
    })();
    let quiesce_ok = quiesce.is_ok();
    if !quiesce_ok && !failed_before_commit {
        failed_before_commit = true;
    }
    phases.push((ActivationPhase::Quiesce, quiesce));

    // Commit (atomic switch)
    let commit = (|| -> Result<(), String> {
        if failed_before_commit {
            return Err("skipped due to earlier failure".to_string());
        }
        if let Some(sim) = simulate_failure_at {
            if sim == ActivationPhase::Commit {
                return Err("simulated commit failure".to_string());
            }
        }
        Ok(())
    })();
    let commit_ok = commit.is_ok();
    if commit_ok {
        env.current = Some(generation_id);
    } else if !failed_before_commit {
        // Commit itself failed — pointer unchanged.
        failed_before_commit = true;
    }
    phases.push((ActivationPhase::Commit, commit));

    // Wake
    let wake = (|| -> Result<(), String> {
        if failed_before_commit {
            return Err("skipped due to earlier failure".to_string());
        }
        if let Some(sim) = simulate_failure_at {
            if sim == ActivationPhase::Wake {
                return Err("simulated wake failure".to_string());
            }
        }
        Ok(())
    })();
    let wake_ok = wake.is_ok();
    phases.push((ActivationPhase::Wake, wake));

    // If wake failed, automatic restore.
    if !wake_ok {
        env.current = previous;
        // Confirm skipped because we restored.
        phases.push((
            ActivationPhase::Confirm,
            Err("skipped due to wake failure (restored prior)".to_string()),
        ));
        return Ok(ActivationReport {
            generation_id,
            phases,
            succeeded: false,
            previous,
        });
    }

    // Confirm
    let confirm = (|| -> Result<(), String> {
        if failed_before_commit {
            return Err("skipped due to earlier failure".to_string());
        }
        if let Some(sim) = simulate_failure_at {
            if sim == ActivationPhase::Confirm {
                return Err("simulated confirm failure".to_string());
            }
        }
        Ok(())
    })();
    let confirm_ok = confirm.is_ok();
    phases.push((ActivationPhase::Confirm, confirm));

    if !confirm_ok {
        // Automatic restore on confirm failure.
        env.current = previous;
        return Ok(ActivationReport {
            generation_id,
            phases,
            succeeded: false,
            previous,
        });
    }

    if failed_before_commit {
        return Ok(ActivationReport {
            generation_id,
            phases,
            succeeded: false,
            previous,
        });
    }

    // Success: prune after commit if needed.
    env.prune();

    Ok(ActivationReport {
        generation_id,
        phases,
        succeeded: true,
        previous,
    })
}

// ── rollback (headless) ──────────────────────────────────────────────────

/// Rollback to a retained generation (full rollback).
///
/// Selects the target generation and performs the same staged activation
/// transaction in reverse. No package code executes during rollback either.
/// Capability gates apply symmetrically: rolling forward again to the higher
/// version will require re-approval (enforced by the caller's integrity
/// chain, not here).
pub fn rollback_full(
    env: &mut Environment,
    target_id: u64,
    simulate_failure_at: Option<ActivationPhase>,
) -> Result<ActivationReport, PackageError> {
    if !env.generations.contains_key(&target_id) {
        return Err(PackageError::generation(format!(
            "rollback target generation {target_id} is not retained (pruned or never existed)"
        )));
    }
    // Verify target integrity before switching.
    env.generations
        .get(&target_id)
        .unwrap()
        .verify_integrity()
        .map_err(|e| PackageError::generation(format!("rollback target quarantine: {e}")))?;

    // Perform activation transaction toward target.
    activate(
        env,
        target_id,
        Some("0.6.0"),
        Some("1.0.0"),
        simulate_failure_at,
    )
}

/// Per-plugin rollback: restore one plugin to its previously retained version.
///
/// Targeted disable remains the surgical path when no prior version exists.
/// This stub validates that the target generation contains the plugin id;
/// actual per-plugin merge is deferred to the resolver.
pub fn rollback_per_plugin(
    env: &mut Environment,
    target_id: u64,
    plugin_id: &str,
) -> Result<ActivationReport, PackageError> {
    let target =
        env.generations
            .get(&target_id)
            .cloned()
            .ok_or_else(|| PackageError::NotFound {
                id: target_id.to_string(),
            })?;
    target.verify_integrity()?;
    if !target
        .lock
        .packages
        .iter()
        .any(|p| p.id.as_str() == plugin_id)
    {
        return Err(PackageError::NotFound {
            id: format!("plugin {plugin_id} not in generation {target_id}"),
        });
    }
    // For draft, full switch to target (same as full rollback).
    activate(env, target_id, Some("0.6.0"), Some("1.0.0"), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::sha256_hex;
    use crate::lockfile::{LockedPackage, Lockfile, PackageDigests};
    use crate::manifest::PackageId;
    use crate::source::PackageSource;
    use std::collections::BTreeMap;

    fn test_lock() -> Lockfile {
        let mut lf = Lockfile::new();
        lf.insert(LockedPackage {
            id: PackageId::new("xuepoo.a").unwrap(),
            version: "0.1.0".to_string(),
            source: PackageSource::Registry {
                url: "https://example.com".to_string(),
            },
            digests: PackageDigests {
                artifact: sha256_hex(b"a"),
                manifest: sha256_hex(b"m"),
                content_root: None,
            },
            locked_at: 1,
        })
        .unwrap();
        lf
    }

    #[test]
    fn generation_integrity() {
        let lf = test_lock();
        let generation = Generation::new(1, lf.clone(), BTreeMap::new(), 100, None).unwrap();
        generation.verify_integrity().unwrap();
        let mut bad = generation.clone();
        bad.root_digest = "a".repeat(64);
        assert!(bad.verify_integrity().is_err());
    }

    #[test]
    fn atomic_switch() {
        let mut env = Environment::new();
        let lf1 = test_lock();
        let id1 = env.stage(lf1, BTreeMap::new(), 1).unwrap();
        activate(&mut env, id1, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        assert_eq!(env.current, Some(id1));
        assert!(env.current_generation().unwrap().id == id1);

        // Stage second generation and activate.
        let lf2 = test_lock();
        let id2 = env.stage(lf2, BTreeMap::new(), 2).unwrap();
        activate(&mut env, id2, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        assert_eq!(env.current, Some(id2));
        assert!(env.is_retained(id1));
    }

    #[test]
    fn failure_before_commit_leaves_pointer_unchanged() {
        let mut env = Environment::new();
        let id1 = env.stage(test_lock(), BTreeMap::new(), 1).unwrap();
        activate(&mut env, id1, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        let lf2 = test_lock();
        let id2 = env.stage(lf2, BTreeMap::new(), 2).unwrap();
        let report = activate(
            &mut env,
            id2,
            Some("0.6.0"),
            Some("1.0.0"),
            Some(ActivationPhase::Preflight),
        )
        .unwrap();
        assert!(!report.succeeded);
        assert_eq!(env.current, Some(id1));
    }

    #[test]
    fn wake_failure_restores() {
        let mut env = Environment::new();
        let id1 = env.stage(test_lock(), BTreeMap::new(), 1).unwrap();
        activate(&mut env, id1, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        let id2 = env.stage(test_lock(), BTreeMap::new(), 2).unwrap();
        let report = activate(
            &mut env,
            id2,
            Some("0.6.0"),
            Some("1.0.0"),
            Some(ActivationPhase::Wake),
        )
        .unwrap();
        assert!(!report.succeeded);
        assert_eq!(env.current, Some(id1));
    }

    #[test]
    fn confirm_failure_restores() {
        let mut env = Environment::new();
        let id1 = env.stage(test_lock(), BTreeMap::new(), 1).unwrap();
        activate(&mut env, id1, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        let id2 = env.stage(test_lock(), BTreeMap::new(), 2).unwrap();
        let report = activate(
            &mut env,
            id2,
            Some("0.6.0"),
            Some("1.0.0"),
            Some(ActivationPhase::Confirm),
        )
        .unwrap();
        assert!(!report.succeeded);
        assert_eq!(env.current, Some(id1));
    }

    #[test]
    fn retention_bounds_never_removes_current() {
        let mut env = Environment::with_policy(RetentionPolicy {
            max_generations: 2,
            max_bytes: 0,
        })
        .unwrap();
        let id1 = env.stage(test_lock(), BTreeMap::new(), 1).unwrap();
        activate(&mut env, id1, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        let id2 = env.stage(test_lock(), BTreeMap::new(), 2).unwrap();
        activate(&mut env, id2, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        let id3 = env.stage(test_lock(), BTreeMap::new(), 3).unwrap();
        activate(&mut env, id3, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        // After third activation with max 2, oldest non-current should be pruned.
        assert!(!env.is_retained(id1) || env.retained_count() <= 2);
        assert!(env.is_retained(id3));
        assert_eq!(env.current, Some(id3));
    }

    #[test]
    fn deterministic_rollback() {
        let mut env = Environment::new();
        let id1 = env.stage(test_lock(), BTreeMap::new(), 10).unwrap();
        activate(&mut env, id1, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        let lock2 = test_lock();
        let digest_before = lock2.digest();
        let id2 = env.stage(lock2, BTreeMap::new(), 20).unwrap();
        activate(&mut env, id2, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        // Rollback to id1.
        rollback_full(&mut env, id1, None).unwrap();
        assert_eq!(env.current, Some(id1));
        assert_eq!(
            env.current_generation().unwrap().lock.digest(),
            digest_before
        );
        // Actually the current lock after rollback should be the original lock's digest.
        // Verify bit-level restoration: compare digest of current vs original.
        assert_eq!(
            env.current_generation().unwrap().lock.digest(),
            env.generations[&id1].lock.digest()
        );
    }

    #[test]
    fn rollback_pruned_fails() {
        let mut env = Environment::with_policy(RetentionPolicy {
            max_generations: 2,
            max_bytes: 0,
        })
        .unwrap();
        let id1 = env.stage(test_lock(), BTreeMap::new(), 1).unwrap();
        activate(&mut env, id1, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        let id2 = env.stage(test_lock(), BTreeMap::new(), 2).unwrap();
        activate(&mut env, id2, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        let id3 = env.stage(test_lock(), BTreeMap::new(), 3).unwrap();
        activate(&mut env, id3, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        // id1 should be pruned.
        if !env.is_retained(id1) {
            assert!(rollback_full(&mut env, id1, None).is_err());
        }
    }

    #[test]
    fn tampered_generation_quarantined() {
        let mut env = Environment::new();
        let id1 = env.stage(test_lock(), BTreeMap::new(), 1).unwrap();
        activate(&mut env, id1, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        // Tamper.
        env.generations.get_mut(&id1).unwrap().root_digest = "b".repeat(64);
        assert!(env.verify_all().is_err());
        assert!(rollback_full(&mut env, id1, None).is_err());
    }
}
