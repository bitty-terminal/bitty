#![forbid(unsafe_code)]
//! Env-gated live check of the guest lifecycle (CTX-0510).
//!
//! Disabled by default: without `BITTY_VM_LIVE` every test prints a SKIP
//! notice and returns. With it, the tests prove the lifecycle reporting is
//! honest on a real host:
//!
//! - Without a prepared base image (the normal case on a fresh host), a
//!   live `smoke` run reports every lifecycle stage `gated` with the exact
//!   missing piece and creates nothing.
//! - With a prepared base image **and** a reachable libvirtd, the same run
//!   executes the real lifecycle: `virsh define` of the staged domain XML,
//!   start, SSH readiness plus execution probe, dumpxml/screenshot
//!   collection, destroy/undefine, and enforced run-directory removal.
//!
//! Enable:
//! ```text
//! BITTY_VM_LIVE=1 cargo test -p bitty-test-vm --test live_guest
//! ```

use std::path::PathBuf;

use bitty_test_vm::capability::Capabilities;
use bitty_test_vm::config::LIVE_ENV;
use bitty_test_vm::policy::{Cadence, RunPlan, guest};
use bitty_test_vm::smoke::{SmokeOptions, SmokeOutcome};

fn live_enabled() -> bool {
    std::env::var_os(LIVE_ENV).is_some()
}

fn skip(name: &str, reason: &str) {
    eprintln!("SKIP live_guest/{name}: {reason}");
}

/// Temporary VM root removed on drop (no `.trash` retention for caches).
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        Self(std::env::temp_dir().join(format!("bitty-test-vm-guest-{tag}-{}", std::process::id())))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn libvirtd_reachable(virsh: &std::path::Path) -> bool {
    std::process::Command::new(virsh)
        .args(["list", "--all"])
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn lifecycle_without_a_base_image_is_gated_and_creates_nothing() {
    let name = "lifecycle_without_a_base_image_is_gated_and_creates_nothing";
    if !live_enabled() {
        skip(name, &format!("{LIVE_ENV} is not set"));
        return;
    }

    let temp = TempRoot::new("gated");
    let arch = guest("arch").expect("arch guest exists");
    let plan = RunPlan::new(arch, Cadence::Pr, "live-gated-1", &temp.0).expect("plan builds");
    let caps = Capabilities::probe(arch.arch);
    let opts = SmokeOptions {
        execute: true,
        ..SmokeOptions::default()
    };
    let report = bitty_test_vm::smoke::run_smoke(plan.clone(), &caps, &opts);
    let rendered = report.render(false);
    eprintln!("{rendered}");

    let lifecycle: Vec<_> = report
        .stages
        .iter()
        .filter(|s| matches!(s.stage, "boot" | "ssh" | "artifacts" | "teardown"))
        .collect();
    assert_eq!(lifecycle.len(), 4, "{rendered}");
    for stage in &lifecycle {
        assert!(
            matches!(&stage.state, bitty_test_vm::smoke::StageState::Gated(_)),
            "{stage:?}:\n{rendered}"
        );
    }
    assert!(
        !plan.run_dir().exists(),
        "gated lifecycle must not create the run directory"
    );
}

#[test]
fn lifecycle_boots_collects_and_cleans_up() {
    let name = "lifecycle_boots_collects_and_cleans_up";
    if !live_enabled() {
        skip(name, &format!("{LIVE_ENV} is not set"));
        return;
    }
    let caps = Capabilities::probe(bitty_test_vm::policy::GuestArch::X86_64);
    let Some(virsh) = caps.virsh.clone() else {
        skip(name, "virsh is not on PATH");
        return;
    };
    if caps.ssh.is_none() {
        skip(name, "ssh is not on PATH");
        return;
    }
    if !libvirtd_reachable(&virsh) {
        skip(name, "no reachable libvirtd (virsh list failed)");
        return;
    }

    let vm_root = bitty_test_vm::config::vm_root_from(std::env::var_os("BITTY_VM_ROOT").as_deref());
    let Some(vm_root) = vm_root else {
        skip(name, "BITTY_VM_ROOT is not set (no prepared base image)");
        return;
    };
    let arch = guest("arch").expect("arch guest exists");
    if !vm_root.join("images/base/arch.qcow2").is_file() {
        skip(name, "prepared base image images/base/arch.qcow2 is absent");
        return;
    }

    let plan = RunPlan::new(arch, Cadence::Pr, "live-boot-1", &vm_root).expect("plan builds");
    let opts = SmokeOptions {
        execute: true,
        ..SmokeOptions::default()
    };
    let report = bitty_test_vm::smoke::run_smoke(plan.clone(), &caps, &opts);
    let rendered = report.render(false);
    eprintln!("{rendered}");
    assert_eq!(
        report.outcome(false),
        SmokeOutcome::Ok,
        "full lifecycle must pass where every prerequisite is present:\n{rendered}"
    );
    assert!(
        !plan.run_dir().exists(),
        "lifecycle must remove its run directory"
    );
}
