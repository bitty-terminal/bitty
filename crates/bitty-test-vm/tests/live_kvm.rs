#![forbid(unsafe_code)]
//! Env-gated bounded QEMU accelerator probe (CTX-0507).
//!
//! Disabled by default: without `BITTY_VM_LIVE` every test prints a SKIP
//! notice and returns. When enabled it proves that the host's accelerator
//! actually initialises — KVM when `/dev/kvm` exists, TCG otherwise — by
//! starting a paused QEMU machine and asking it to quit over QMP under a
//! hard deadline. It only ever kills the child it spawned.
//!
//! Enable:
//! ```text
//! BITTY_VM_LIVE=1 cargo test -p bitty-test-vm --test live_kvm
//! ```

use std::path::PathBuf;

use bitty_test_vm::capability;
use bitty_test_vm::config::LIVE_ENV;
use bitty_test_vm::kvm::{self, ProbeStatus};
use bitty_test_vm::policy::{Accel, GuestArch};

fn live_enabled() -> bool {
    std::env::var_os(LIVE_ENV).is_some()
}

fn qemu_system(name: &str) -> Option<PathBuf> {
    capability::find_on_path(
        std::env::var_os("PATH").as_deref(),
        name,
        capability::is_executable,
    )
}

fn skip(name: &str, reason: &str) {
    eprintln!("SKIP live_kvm/{name}: {reason}");
}

#[test]
fn kvm_accelerator_initialises_and_quits() {
    let name = "kvm_accelerator_initialises_and_quits";
    if !live_enabled() {
        skip(name, &format!("{LIVE_ENV} is not set"));
        return;
    }
    if !capability::kvm_device_present() {
        skip(name, "no /dev/kvm on this host");
        return;
    }
    let Some(qemu) = qemu_system(GuestArch::X86_64.qemu_system()) else {
        skip(name, "qemu-system-x86_64 is not on PATH");
        return;
    };

    let outcome = kvm::run_accel_probe(&qemu, GuestArch::X86_64, Accel::Kvm, kvm::PROBE_TIMEOUT)
        .expect("QEMU spawns");
    assert_eq!(
        outcome.status,
        ProbeStatus::Usable,
        "KVM probe failed: {}",
        outcome.detail
    );
}

#[test]
fn tcg_accelerator_initialises_and_quits() {
    let name = "tcg_accelerator_initialises_and_quits";
    if !live_enabled() {
        skip(name, &format!("{LIVE_ENV} is not set"));
        return;
    }
    let Some(qemu) = qemu_system(GuestArch::X86_64.qemu_system()) else {
        skip(name, "qemu-system-x86_64 is not on PATH");
        return;
    };

    let outcome = kvm::run_accel_probe(&qemu, GuestArch::X86_64, Accel::Tcg, kvm::PROBE_TIMEOUT)
        .expect("QEMU spawns");
    assert_eq!(
        outcome.status,
        ProbeStatus::Usable,
        "TCG probe failed: {}",
        outcome.detail
    );
}
