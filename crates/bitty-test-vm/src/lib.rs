#![forbid(unsafe_code)]
//! VM test-tier controller for the `bitty` workspace (CTX-0507, research 043).
//!
//! First slice of the VM test tier: it fixes the **base-image + qcow2
//! overlay policy** (never boot an ISO per run), encodes the staged guest
//! cadence (PR -> Arch; main adds Ubuntu/Fedora/Alpine; nightly adds
//! Linux ARM64 under TCG), and provides a small `bitty-vm` controller
//! skeleton (`guests`, `doctor`, `plan`, `smoke`).
//!
//! What this slice does **not** do, on purpose:
//!
//! - It never installs a guest and contains no ISO automation. Base images
//!   are prepared manually, far-future work, from user-supplied media in
//!   `$ISO_PATH`; see `specifications/vm-tier-policy.md` in the repository.
//! - It never fakes a live run. `smoke` executes only what the host can
//!   honestly run (a bounded QEMU accelerator probe, and qcow2 overlay
//!   creation when a prepared base image exists); everything else is
//!   reported as gated or deferred with a reason.
//! - Guest boot, SSH test execution, and libvirt domain definition are
//!   follow-up work; `plan` renders the intended commands/domain XML as a
//!   dry run so the next slice has a reviewed target.
//!
//! All host-specific values come from configuration or the environment
//! (`BITTY_VM_ROOT`, `--root`); nothing here hardcodes a checkout, home,
//! or machine path. The library is dependency-free (std only) and compiles
//! on every platform; probes report "unavailable" off Linux.

pub mod capability;
pub mod cli;
pub mod config;
pub mod kvm;
pub mod overlay;
pub mod policy;
pub mod smoke;

pub use capability::Capabilities;
pub use config::VmConfig;
pub use policy::{Accel, Cadence, GUESTS, Guest, GuestArch, RunPlan, Violation};
