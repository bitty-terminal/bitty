#![forbid(unsafe_code)]

//! Lifecycle enforcement tests — CTX-0597 (RUN-26, FS-2/FS-4/FS-6).
//!
//! Headless and deterministic: all time comes from [`ManualClock`], so the
//! sliding-window and reactivation behavior is fully reproducible with no
//! wall-clock dependence, no I/O, and no VM.

use bitty_plugin_host::{
    BudgetDimension, Clock, EnforcementAction, LifecycleEnforcer, ManualClock,
    PluginLifecycleStatus, ReloadOutcome, ReloadReport, ReloadResources, reload_generation,
};

const OWNER: &str = "xuepoo.lifecycle";
const DIMENSION: BudgetDimension = BudgetDimension::Instructions;
const OBSERVED: u64 = 12_000_000;
const LIMIT: u64 = 10_000_000;

fn report_at(
    enforcer: &mut LifecycleEnforcer,
    clock: &ManualClock,
    generation: u64,
) -> EnforcementAction {
    enforcer
        .report_violation(
            OWNER,
            generation,
            DIMENSION,
            OBSERVED,
            LIMIT,
            clock.now_secs(),
        )
        .expect("owner table has room")
        .action
}

// ── FS-2: ladder with fake clock ────────────────────────────────────────────

#[test]
fn three_escalations_in_window_suspend_generation() {
    let clock = ManualClock::new(1_000);
    let mut enforcer = LifecycleEnforcer::new();

    assert_eq!(
        report_at(&mut enforcer, &clock, 1),
        EnforcementAction::Refuse
    );
    clock.advance(10);
    assert_eq!(
        report_at(&mut enforcer, &clock, 1),
        EnforcementAction::TerminateCallback
    );
    clock.advance(10);
    assert_eq!(
        report_at(&mut enforcer, &clock, 1),
        EnforcementAction::SuspendGeneration
    );
    assert_eq!(enforcer.status(OWNER), PluginLifecycleStatus::Suspended);
    assert_eq!(enforcer.generation(OWNER), Some(1));
}

#[test]
fn sliding_window_forgets_old_escalations() {
    let clock = ManualClock::new(0);
    let mut enforcer = LifecycleEnforcer::new();

    assert_eq!(
        report_at(&mut enforcer, &clock, 1),
        EnforcementAction::Refuse
    );
    // First escalation is 61s old: outside the 60s window, ladder restarts.
    clock.advance(61);
    assert_eq!(
        report_at(&mut enforcer, &clock, 1),
        EnforcementAction::Refuse
    );
    assert_eq!(enforcer.status(OWNER), PluginLifecycleStatus::Active);
    // The t=61 escalation is still inside the window at t=62: second rung.
    clock.advance(1);
    assert_eq!(
        report_at(&mut enforcer, &clock, 1),
        EnforcementAction::TerminateCallback
    );
}

#[test]
fn violation_while_suspended_disables_plugin() {
    let clock = ManualClock::new(500);
    let mut enforcer = LifecycleEnforcer::new();

    for _ in 0..3 {
        report_at(&mut enforcer, &clock, 2);
        clock.advance(5);
    }
    assert_eq!(enforcer.status(OWNER), PluginLifecycleStatus::Suspended);
    assert_eq!(
        report_at(&mut enforcer, &clock, 2),
        EnforcementAction::DisablePlugin
    );
    assert_eq!(enforcer.status(OWNER), PluginLifecycleStatus::Disabled);
}

#[test]
fn reactivation_requires_explicit_user_action() {
    let clock = ManualClock::new(900);
    let mut enforcer = LifecycleEnforcer::new();

    for _ in 0..3 {
        report_at(&mut enforcer, &clock, 3);
        clock.advance(5);
    }
    assert_eq!(enforcer.status(OWNER), PluginLifecycleStatus::Suspended);
    // Time passing alone never reactivates.
    clock.advance(10_000);
    assert_eq!(enforcer.status(OWNER), PluginLifecycleStatus::Suspended);
    // Only the explicit call restores Active, and it restarts the ladder.
    enforcer
        .reactivate(OWNER)
        .expect("suspended owner reactivates");
    assert_eq!(enforcer.status(OWNER), PluginLifecycleStatus::Active);
    assert_eq!(
        report_at(&mut enforcer, &clock, 3),
        EnforcementAction::Refuse
    );
}

#[test]
fn reactivate_rejects_unknown_and_active_owners() {
    let mut enforcer = LifecycleEnforcer::new();
    assert!(enforcer.reactivate("xuepoo.ghost").is_err());

    let clock = ManualClock::new(0);
    report_at(&mut enforcer, &clock, 1);
    assert!(enforcer.reactivate(OWNER).is_err());
}

// ── FS-4: structured records ────────────────────────────────────────────────

#[test]
fn every_action_emits_structured_record() {
    let clock = ManualClock::new(2_000);
    let mut enforcer = LifecycleEnforcer::new();

    for _ in 0..3 {
        report_at(&mut enforcer, &clock, 7);
        clock.advance(5);
    }
    let records = enforcer.records_for(OWNER);
    assert_eq!(records.len(), 3);
    let expected = [
        EnforcementAction::Refuse,
        EnforcementAction::TerminateCallback,
        EnforcementAction::SuspendGeneration,
    ];
    for (record, action) in records.iter().zip(expected) {
        assert_eq!(record.owner, OWNER);
        assert_eq!(record.generation, 7);
        assert_eq!(record.dimension, DIMENSION);
        assert_eq!(record.observed, OBSERVED);
        assert_eq!(record.limit, LIMIT);
        assert_eq!(record.action, action);
    }
    // Monotonic timestamps follow the fake clock.
    assert!(records[0].at_secs < records[1].at_secs);
    assert!(records[1].at_secs < records[2].at_secs);
}

// ── FS-6: dispose-before-activate, restore-or-disable ───────────────────────

#[derive(Debug, PartialEq, Eq)]
struct Step(&'static str);

#[derive(Debug, Default)]
struct FakeResources {
    log: Vec<String>,
    fail_activate: bool,
    fail_restore: bool,
}

impl ReloadResources for FakeResources {
    type Error = Step;

    fn dispose_generation(&mut self, generation: u64) -> Result<(), Step> {
        self.log.push(format!("dispose {generation}"));
        Ok(())
    }

    fn activate_generation(&mut self, generation: u64) -> Result<(), Step> {
        self.log.push(format!("activate {generation}"));
        if self.fail_activate {
            return Err(Step("activate failed"));
        }
        Ok(())
    }

    fn restore_generation(&mut self, generation: u64) -> Result<(), Step> {
        self.log.push(format!("restore {generation}"));
        if self.fail_restore {
            return Err(Step("restore failed"));
        }
        Ok(())
    }

    fn disable_plugin(&mut self) -> Result<(), Step> {
        self.log.push("disable".to_string());
        Ok(())
    }
}

#[test]
fn reload_disposes_before_activating() {
    let mut resources = FakeResources::default();
    let report: ReloadReport<Step> = reload_generation(&mut resources, 4, 5);
    assert_eq!(report.outcome, ReloadOutcome::Activated);
    assert_eq!(report.error, None);
    assert_eq!(resources.log, vec!["dispose 4", "activate 5"]);
}

#[test]
fn failed_reload_restores_previous_generation() {
    let mut resources = FakeResources {
        fail_activate: true,
        ..FakeResources::default()
    };
    let report: ReloadReport<Step> = reload_generation(&mut resources, 4, 5);
    assert_eq!(report.outcome, ReloadOutcome::Restored);
    assert_eq!(report.error, Some(Step("activate failed")));
    assert_eq!(resources.log, vec!["dispose 4", "activate 5", "restore 4"]);
}

#[test]
fn failed_restore_disables_cleanly() {
    let mut resources = FakeResources {
        fail_activate: true,
        fail_restore: true,
        ..FakeResources::default()
    };
    let report: ReloadReport<Step> = reload_generation(&mut resources, 4, 5);
    assert_eq!(report.outcome, ReloadOutcome::Disabled);
    assert_eq!(report.error, Some(Step("activate failed")));
    assert_eq!(
        resources.log,
        vec!["dispose 4", "activate 5", "restore 4", "disable"]
    );
}
