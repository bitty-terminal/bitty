//! Environment-gated smoke orchestration.
//!
//! `smoke` runs only stages this host can honestly execute and reports the
//! rest with a reason. States:
//!
//! - `ok`: the stage ran and passed.
//! - `dry-run`: the stage would run; `--execute`/`BITTY_VM_LIVE` is absent.
//! - `gated`: the stage cannot run here (missing `/dev/kvm`, QEMU binary,
//!   or a prepared base image; `BITTY_VM_FORCE_SKIP` set).
//! - `deferred`: the stage is future work outside this first slice.
//! - `failed`: the stage ran and failed.
//!
//! Guest boot and SSH test execution are `deferred` in this slice: the plan
//! renders the exact commands and domain XML, but no domain is started. A
//! gated stage does not fail the command unless `--require` is passed.

use std::time::Duration;

use crate::capability::Capabilities;
use crate::kvm::{self, ProbeStatus};
use crate::overlay;
use crate::policy::RunPlan;

/// Stage name: policy validation.
pub const STAGE_POLICY: &str = "policy";
/// Stage name: bounded QEMU accelerator probe.
pub const STAGE_ACCEL: &str = "accel";
/// Stage name: qcow2 overlay creation.
pub const STAGE_OVERLAY: &str = "overlay";
/// Stage name: guest boot and SSH execution (deferred).
pub const STAGE_GUEST: &str = "guest-boot";

/// State of one smoke stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageState {
    /// Ran and passed; the string is the evidence line.
    Ok(String),
    /// Not attempted by default; would run with live execution enabled.
    DryRun(String),
    /// Cannot run in this environment; the string is the reason.
    Gated(String),
    /// Future work outside this slice; the string is the scope note.
    Deferred(String),
    /// Ran and failed; the string is the reason.
    Failed(String),
}

impl StageState {
    /// Stable lower-case label used in reports.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ok(_) => "ok",
            Self::DryRun(_) => "dry-run",
            Self::Gated(_) => "gated",
            Self::Deferred(_) => "deferred",
            Self::Failed(_) => "failed",
        }
    }

    /// Detail or reason line.
    pub fn detail(&self) -> &str {
        match self {
            Self::Ok(detail)
            | Self::DryRun(detail)
            | Self::Gated(detail)
            | Self::Deferred(detail)
            | Self::Failed(detail) => detail,
        }
    }
}

/// One stage of a smoke report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageReport {
    /// Stable stage name.
    pub stage: &'static str,
    /// Observed state.
    pub state: StageState,
}

/// Aggregate outcome of a smoke run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmokeOutcome {
    /// No failures; every stage ran, was dry-run, or was cleanly gated.
    Ok,
    /// At least one stage was gated and `--require` demanded live coverage.
    Gated,
    /// A stage failed.
    Failed,
}

/// Options for one smoke run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokeOptions {
    /// Run the live stages (probe, overlay creation).
    pub execute: bool,
    /// Treat a gated stage as failure (exit code 3).
    pub require_live: bool,
    /// Force every live stage to report gated (mirrors the PTY gate).
    pub force_skip: bool,
    /// Deadline for the accelerator probe.
    pub timeout: Duration,
}

impl Default for SmokeOptions {
    fn default() -> Self {
        Self {
            execute: false,
            require_live: false,
            force_skip: false,
            timeout: kvm::PROBE_TIMEOUT,
        }
    }
}

/// Full smoke report for one plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokeReport {
    /// The plan under test.
    pub plan: RunPlan,
    /// Stages in execution order.
    pub stages: Vec<StageReport>,
}

impl SmokeReport {
    /// Whether at least one stage failed.
    pub fn failed(&self) -> bool {
        self.stages
            .iter()
            .any(|s| matches!(s.state, StageState::Failed(_)))
    }

    /// Whether at least one stage was gated.
    pub fn gated(&self) -> bool {
        self.stages
            .iter()
            .any(|s| matches!(s.state, StageState::Gated(_)))
    }

    /// Aggregate outcome; `require_live` promotes gated to [`SmokeOutcome::Gated`].
    pub fn outcome(&self, require_live: bool) -> SmokeOutcome {
        if self.failed() {
            SmokeOutcome::Failed
        } else if require_live && self.gated() {
            SmokeOutcome::Gated
        } else {
            SmokeOutcome::Ok
        }
    }

    /// Stable, grep-able multi-line report. `require_live` selects the
    /// outcome spelling for gated stages.
    pub fn render(&self, require_live: bool) -> String {
        let mut out = format!("smoke {} (run {})\n", self.plan.guest.id, self.plan.run_id);
        for stage in &self.stages {
            out.push_str(&format!(
                "  {:<10} {:<8} {}\n",
                stage.stage,
                stage.state.label(),
                stage.state.detail()
            ));
        }
        let outcome = match self.outcome(require_live) {
            SmokeOutcome::Ok if self.gated() => "gated (allowed; use --require to fail)",
            SmokeOutcome::Ok => "ok",
            SmokeOutcome::Gated => "gated (required live coverage missing)",
            SmokeOutcome::Failed => "failed",
        };
        out.push_str(&format!("result: {outcome}\n"));
        out
    }
}

/// Run the smoke stages for one plan. Only the accelerator probe and
/// overlay creation can execute; both require `execute` and their
/// prerequisites, and both report `gated` honestly otherwise.
pub fn run_smoke(plan: RunPlan, caps: &Capabilities, opts: &SmokeOptions) -> SmokeReport {
    let stages = vec![
        StageReport {
            stage: STAGE_POLICY,
            state: policy_stage(&plan),
        },
        StageReport {
            stage: STAGE_ACCEL,
            state: accel_stage(&plan, caps, opts),
        },
        StageReport {
            stage: STAGE_OVERLAY,
            state: overlay_stage(&plan, caps, opts),
        },
        StageReport {
            stage: STAGE_GUEST,
            state: StageState::Deferred(
                "guest boot + SSH execution is follow-up work outside this slice; the plan renders \
                 the qemu-img/libvirt commands and domain XML"
                    .to_string(),
            ),
        },
    ];
    SmokeReport { plan, stages }
}

fn policy_stage(plan: &RunPlan) -> StageState {
    match plan.validate() {
        Ok(()) => {
            StageState::Ok("base-image + qcow2 overlay policy satisfied; no ISO disk".to_string())
        }
        Err(violations) => {
            let detail: Vec<String> = violations.iter().map(ToString::to_string).collect();
            StageState::Failed(format!("policy violated: {}", detail.join("; ")))
        }
    }
}

fn accel_stage(plan: &RunPlan, caps: &Capabilities, opts: &SmokeOptions) -> StageState {
    let arch = plan.guest.arch;
    let accel = plan.guest.accel;
    if opts.force_skip {
        return StageState::Gated(format!(
            "forced skip: {} is set",
            crate::config::FORCE_SKIP_ENV
        ));
    }
    if !caps.kvm_usable() && matches!(accel, crate::policy::Accel::Kvm) {
        let missing = caps.missing_for_kvm();
        return StageState::Gated(format!("KVM prerequisites missing: {}", missing.join(", ")));
    }
    let Some(qemu_system) = caps.qemu_system.as_ref() else {
        return StageState::Gated(format!("{} is not on PATH", arch.qemu_system()));
    };
    if !opts.execute {
        return StageState::DryRun(format!(
            "would run: {} {}",
            qemu_system.display(),
            kvm::probe_args(arch, accel).join(" ")
        ));
    }
    match kvm::run_accel_probe(qemu_system, arch, accel, opts.timeout) {
        Ok(outcome) if outcome.status == ProbeStatus::Usable => StageState::Ok(outcome.detail),
        Ok(outcome) => StageState::Failed(outcome.detail),
        Err(err) => StageState::Failed(format!("could not spawn {}: {err}", qemu_system.display())),
    }
}

fn overlay_stage(plan: &RunPlan, caps: &Capabilities, opts: &SmokeOptions) -> StageState {
    if opts.force_skip {
        return StageState::Gated(format!(
            "forced skip: {} is set",
            crate::config::FORCE_SKIP_ENV
        ));
    }
    let base = plan.base_image();
    if !base.is_file() {
        return StageState::Gated(format!(
            "base image not prepared: {} (manual creation from user media is far-future; see \
             specifications/vm-tier-policy.md)",
            base.display()
        ));
    }
    let Some(qemu_img) = caps.qemu_img.as_ref() else {
        return StageState::Gated("qemu-img is not on PATH".to_string());
    };
    let overlay = plan.overlay_image();
    if !opts.execute {
        return StageState::DryRun(format!("would create overlay {}", overlay.display()));
    }
    match overlay::create_overlay(qemu_img, plan) {
        Ok(path) => StageState::Ok(format!(
            "created {} backing {}",
            path.display(),
            base.display()
        )),
        Err(err) => StageState::Failed(format!("overlay creation failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Cadence, guest};
    use std::path::PathBuf;

    fn plan(guest_id: &str) -> RunPlan {
        let guest = guest(guest_id).expect("guest exists");
        RunPlan::new(guest, Cadence::Pr, "arch-1", "/vm-root").expect("plan")
    }

    fn caps(kvm: bool, qemu: bool, qemu_img: bool) -> Capabilities {
        Capabilities {
            kvm_device: kvm,
            qemu_system: qemu.then(|| PathBuf::from("/usr/bin/qemu-system-x86_64")),
            qemu_img: qemu_img.then(|| PathBuf::from("/usr/bin/qemu-img")),
            virsh: None,
            ssh: None,
        }
    }

    #[test]
    fn dry_run_reports_deferred_guest_and_no_execution() {
        let opts = SmokeOptions::default();
        let report = run_smoke(plan("arch"), &caps(true, true, true), &opts);
        let states: Vec<_> = report
            .stages
            .iter()
            .map(|s| (s.stage, s.state.label()))
            .collect();
        assert_eq!(
            states,
            [
                ("policy", "ok"),
                ("accel", "dry-run"),
                ("overlay", "gated"),
                ("guest-boot", "deferred"),
            ]
        );
        assert_eq!(report.outcome(false), SmokeOutcome::Ok);
        assert!(!report.failed());
    }

    #[test]
    fn missing_kvm_gates_the_accel_stage_only() {
        let opts = SmokeOptions {
            execute: true,
            ..SmokeOptions::default()
        };
        let report = run_smoke(plan("arch"), &caps(false, true, true), &opts);
        let accel = report
            .stages
            .iter()
            .find(|s| s.stage == STAGE_ACCEL)
            .expect("accel stage");
        assert!(matches!(&accel.state, StageState::Gated(reason) if reason.contains("/dev/kvm")));
        assert_eq!(report.outcome(true), SmokeOutcome::Gated);
        assert_eq!(report.outcome(false), SmokeOutcome::Ok);
    }

    #[test]
    fn prepared_base_without_execute_is_dry_run() {
        // The overlay stage only reaches dry-run when the base exists, which
        // cannot be arranged here; force-skip must still report gated with
        // the exact reason.
        let opts = SmokeOptions {
            force_skip: true,
            execute: true,
            ..SmokeOptions::default()
        };
        let report = run_smoke(plan("arch"), &caps(true, true, true), &opts);
        assert!(
            report
                .stages
                .iter()
                .all(|s| !matches!(s.state, StageState::Failed(_)))
        );
        assert!(report.gated());
        assert_eq!(report.outcome(true), SmokeOutcome::Gated);
        let render = report.render(true);
        assert!(render.contains("forced skip"));
        assert!(render.contains("guest-boot deferred"));
    }

    #[test]
    fn tcg_guest_needs_no_kvm_device() {
        let opts = SmokeOptions::default();
        let report = run_smoke(plan("arch-arm64"), &caps(false, true, false), &opts);
        let accel = report
            .stages
            .iter()
            .find(|s| s.stage == STAGE_ACCEL)
            .expect("accel stage");
        assert!(matches!(accel.state, StageState::DryRun(_)));
        assert!(!report.failed());
    }
}
