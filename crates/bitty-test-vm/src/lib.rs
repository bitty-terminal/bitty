#![forbid(unsafe_code)]
//! VM test-tier controller for the `bitty` workspace (CTX-0507, CTX-0510,
//! research 043).
//!
//! First slice: it fixes the **base-image + qcow2 overlay policy** (never
//! boot an ISO per run), encodes the staged guest cadence (PR -> Arch and
//! Windows 11; main adds Ubuntu/Fedora/Alpine; nightly adds Linux ARM64
//! under TCG), and provides a small `bitty-vm` controller (`guests`,
//! `doctor`, `plan`, `smoke`).
//!
//! Second slice (CTX-0510): the guest lifecycle in [`guest`] (libvirt boot,
//! SSH readiness and execution, artifact collection, teardown with enforced
//! run-directory removal) behind `BITTY_VM_LIVE` / `--execute`.
//!
//! What this slice does **not** do, on purpose:
//!
//! - It never installs a guest and contains no ISO automation. Base images
//!   are prepared manually, far-future work, from user-supplied media in
//!   `$ISO_PATH`; see `specifications/vm-tier-policy.md` in the repository.
//! - It never fakes a live run. `smoke` executes only what the host can
//!   honestly run (a bounded QEMU accelerator probe, qcow2 overlay
//!   creation, and — with a prepared base image plus libvirt — the guest
//!   lifecycle in [`guest`]); everything else is reported as gated,
//!   dry-run, or deferred with a reason.
//! - Full in-guest suite execution stays recorded plan
//!   (`policy::GUEST_SUITE_CANDIDATES`); the lifecycle proves the
//!   boot/SSH/artifact/teardown path those suites will ride.
//!
//! All host-specific values come from configuration or the environment
//! (`BITTY_VM_ROOT`, `--root`); nothing here hardcodes a checkout, home,
//! or machine path. The library is dependency-free (std only) and compiles
//! on every platform; probes report "unavailable" off Linux.

pub mod capability;
pub mod cli;
pub mod config;
pub mod guest;
pub mod kvm;
pub mod overlay;
pub mod policy;
pub mod smoke;

pub use capability::Capabilities;
pub use config::VmConfig;
pub use policy::{Accel, Cadence, GUESTS, Guest, GuestArch, RunPlan, Violation};
