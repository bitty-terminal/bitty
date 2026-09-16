#![forbid(unsafe_code)]
//! Env-gated live check of the base-image/overlay policy with a real
//! `qemu-img` (CTX-0507).
//!
//! Disabled by default: without `BITTY_VM_LIVE` every test prints a SKIP
//! notice and returns. It creates a blank qcow2 **fixture** base image (a
//! disk-format fixture, not an OS image and not an ISO), creates a real
//! overlay through the library, and verifies with `qemu-img info` that the
//! overlay's only backing file is that base image. Everything lives under a
//! unique temporary root that is removed on drop; the base image is never
//! modified.
//!
//! Enable:
//! ```text
//! BITTY_VM_LIVE=1 cargo test -p bitty-test-vm --test live_overlay
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use bitty_test_vm::config::LIVE_ENV;
use bitty_test_vm::overlay;
use bitty_test_vm::policy::{Cadence, RunPlan, guest};

fn live_enabled() -> bool {
    std::env::var_os(LIVE_ENV).is_some()
}

fn qemu_img() -> Option<PathBuf> {
    bitty_test_vm::capability::find_on_path(
        std::env::var_os("PATH").as_deref(),
        "qemu-img",
        bitty_test_vm::capability::is_executable,
    )
}

fn skip(name: &str, reason: &str) {
    eprintln!("SKIP live_overlay/{name}: {reason}");
}

/// Temporary VM root removed on drop (no `.trash` retention for caches).
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        Self(std::env::temp_dir().join(format!("bitty-test-vm-{tag}-{}", std::process::id())))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn overlay_backs_a_real_qcow2_base_image() {
    let name = "overlay_backs_a_real_qcow2_base_image";
    if !live_enabled() {
        skip(name, &format!("{LIVE_ENV} is not set"));
        return;
    }
    let Some(qemu_img) = qemu_img() else {
        skip(name, "qemu-img is not on PATH");
        return;
    };

    let temp = TempRoot::new("overlay");
    let guest = guest("arch").expect("arch guest exists");
    let plan = RunPlan::new(guest, Cadence::Pr, "test-1", &temp.0).expect("plan builds");

    let base = plan.base_image();
    std::fs::create_dir_all(base.parent().expect("base dir")).expect("create base dir");
    let status = Command::new(&qemu_img)
        .args(["create", "-f", "qcow2"])
        .arg(&base)
        .arg("8M")
        .status()
        .expect("run qemu-img create");
    assert!(status.success(), "fixture base image creation must succeed");

    let overlay = overlay::create_overlay(&qemu_img, &plan).expect("overlay creation");
    assert_eq!(overlay, plan.overlay_image());

    let backing = overlay::overlay_backing_file(&qemu_img, &overlay).expect("backing file");
    assert_eq!(
        Path::new(&backing),
        base,
        "overlay must back the base image"
    );

    // The base image is untouched by overlay creation: qemu-img still
    // reports it as a plain qcow2 without a backing file.
    let info = Command::new(&qemu_img)
        .args(["info", "--output=json"])
        .arg(&base)
        .output()
        .expect("qemu-img info");
    let json = String::from_utf8_lossy(&info.stdout);
    assert!(json.contains("\"format\": \"qcow2\""));
    assert!(
        !json.contains("\"backing-filename\""),
        "base must stay unbacked"
    );
}

#[test]
fn overlay_creation_refuses_to_reuse_a_run_id() {
    let name = "overlay_creation_refuses_to_reuse_a_run_id";
    if !live_enabled() {
        skip(name, &format!("{LIVE_ENV} is not set"));
        return;
    }
    let Some(qemu_img) = qemu_img() else {
        skip(name, "qemu-img is not on PATH");
        return;
    };

    let temp = TempRoot::new("overlay-reuse");
    let guest = guest("arch").expect("arch guest exists");
    let plan = RunPlan::new(guest, Cadence::Pr, "test-2", &temp.0).expect("plan builds");
    let base = plan.base_image();
    std::fs::create_dir_all(base.parent().expect("base dir")).expect("create base dir");
    let status = Command::new(&qemu_img)
        .args(["create", "-f", "qcow2"])
        .arg(&base)
        .arg("8M")
        .status()
        .expect("run qemu-img create");
    assert!(status.success());

    overlay::create_overlay(&qemu_img, &plan).expect("first overlay creation");
    let second = overlay::create_overlay(&qemu_img, &plan);
    let error = second.expect_err("second creation must be refused");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
}

#[test]
fn overlay_creation_refuses_a_missing_base_image() {
    let name = "overlay_creation_refuses_a_missing_base_image";
    if !live_enabled() {
        skip(name, &format!("{LIVE_ENV} is not set"));
        return;
    }
    let Some(qemu_img) = qemu_img() else {
        skip(name, "qemu-img is not on PATH");
        return;
    };

    let temp = TempRoot::new("overlay-missing");
    let guest = guest("arch").expect("arch guest exists");
    let plan = RunPlan::new(guest, Cadence::Pr, "test-3", &temp.0).expect("plan builds");
    let error = overlay::create_overlay(&qemu_img, &plan).expect_err("must refuse");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
}
