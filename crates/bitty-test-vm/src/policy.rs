//! Base-image and qcow2-overlay policy (research 043; CTX-0507 first slice).
//!
//! The policy has one non-negotiable rule: **a run never installs or boots
//! from an ISO**. Guest operating systems are installed once into prepared
//! base images (`<vm-root>/images/base/<guest>.qcow2`) by a manual,
//! far-future procedure that consumes user-supplied media from `$ISO_PATH`;
//! every test run then boots a disposable overlay
//! (`<vm-root>/runs/<run-id>/<guest>-overlay.qcow2`) whose only backing file
//! is that base image. The overlay is deleted with its run directory after
//! the run; the base image is never a boot disk and never mutated.
//!
//! [`RunPlan`] is the encoded model. [`RunPlan::validate`] is the machine
//! check; [`validate_paths`] is the pure, injectable core so policy rules
//! can be proven against hostile layouts in unit tests without constructing
//! a hostile plan.

use std::fmt;
use std::path::{Path, PathBuf};

/// Directory (under the VM root) that holds prepared base images.
pub const BASE_IMAGES_DIR: &str = "images/base";
/// Directory (under the VM root) that holds per-run scratch trees.
pub const RUNS_DIR: &str = "runs";
/// Disk format for base images and overlays. ISOs are never run disks.
pub const DISK_FORMAT: &str = "qcow2";

/// Cadence at which a guest enters the matrix.
///
/// Ordered by cost: `Pr < Main < Nightly`. A cadence runs its own guests
/// plus every lower cadence (PR is the smallest, cheapest set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cadence {
    /// Every pull request: Arch Linux x86_64 under KVM.
    Pr,
    /// Merge/main: adds Ubuntu LTS, Fedora, Alpine x86_64.
    Main,
    /// Nightly: adds Linux ARM64 under QEMU TCG (not per-commit).
    Nightly,
}

impl Cadence {
    /// All cadences in ascending cost order.
    pub const ALL: [Self; 3] = [Self::Pr, Self::Main, Self::Nightly];

    /// Stable machine spelling (`pr`, `main`, `nightly`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pr => "pr",
            Self::Main => "main",
            Self::Nightly => "nightly",
        }
    }

    /// Parse a cadence from its stable spelling.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pr" => Some(Self::Pr),
            "main" => Some(Self::Main),
            "nightly" => Some(Self::Nightly),
            _ => None,
        }
    }

    /// Guests included when this cadence runs (its own and cheaper rows).
    pub fn guests(self) -> impl Iterator<Item = &'static Guest> {
        GUESTS
            .iter()
            .filter(move |guest| guest.first_cadence <= self)
    }
}

/// Guest CPU architecture; selects the QEMU system binary and machine type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestArch {
    /// x86_64 guest, accelerated by KVM on x86_64 hosts.
    X86_64,
    /// aarch64 guest; this slice runs it under TCG only.
    Aarch64,
}

impl GuestArch {
    /// Stable machine spelling (`x86_64`, `aarch64`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// QEMU system binary that runs this architecture.
    pub fn qemu_system(self) -> &'static str {
        match self {
            Self::X86_64 => "qemu-system-x86_64",
            Self::Aarch64 => "qemu-system-aarch64",
        }
    }

    /// QEMU machine type used by the bounded accelerator probe.
    pub fn qemu_machine(self) -> &'static str {
        match self {
            Self::X86_64 => "q35",
            Self::Aarch64 => "virt",
        }
    }

    /// libvirt domain architecture spelling.
    pub fn domain_arch(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}

/// Acceleration backend for a guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    /// Same-architecture hardware acceleration (`/dev/kvm`).
    Kvm,
    /// Pure emulation; slow, nightly-only for cross-architecture guests.
    Tcg,
}

impl Accel {
    /// Stable machine spelling (`kvm`, `tcg`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kvm => "kvm",
            Self::Tcg => "tcg",
        }
    }

    /// QEMU `-accel <value>` spelling.
    pub fn qemu_value(self) -> &'static str {
        self.as_str()
    }
}

/// One guest operating system in the VM matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guest {
    /// Stable identifier used in paths and domain names (e.g. `arch`).
    pub id: &'static str,
    /// Human-readable name for reports.
    pub display: &'static str,
    /// Guest CPU architecture.
    pub arch: GuestArch,
    /// Acceleration backend.
    pub accel: Accel,
    /// Lowest cadence that runs this guest.
    pub first_cadence: Cadence,
    /// SSH account used for test execution in the guest.
    pub ssh_user: &'static str,
}

/// The staged Linux guest matrix of this slice.
///
/// Windows 11 is part of research 043's PR row but deliberately absent here:
/// it needs image provisioning, OpenSSH/WinRM wiring, and a licensing story
/// that this first slice does not own. It stays a documented plan, not a
/// fake row; see `specifications/vm-tier-policy.md`.
pub const GUESTS: &[Guest] = &[
    Guest {
        id: "arch",
        display: "Arch Linux x86_64",
        arch: GuestArch::X86_64,
        accel: Accel::Kvm,
        first_cadence: Cadence::Pr,
        ssh_user: "bitty",
    },
    Guest {
        id: "ubuntu",
        display: "Ubuntu LTS x86_64",
        arch: GuestArch::X86_64,
        accel: Accel::Kvm,
        first_cadence: Cadence::Main,
        ssh_user: "bitty",
    },
    Guest {
        id: "fedora",
        display: "Fedora x86_64",
        arch: GuestArch::X86_64,
        accel: Accel::Kvm,
        first_cadence: Cadence::Main,
        ssh_user: "bitty",
    },
    Guest {
        id: "alpine",
        display: "Alpine x86_64",
        arch: GuestArch::X86_64,
        accel: Accel::Kvm,
        first_cadence: Cadence::Main,
        ssh_user: "bitty",
    },
    Guest {
        id: "arch-arm64",
        display: "Arch Linux ARM64 (TCG)",
        arch: GuestArch::Aarch64,
        accel: Accel::Tcg,
        first_cadence: Cadence::Nightly,
        ssh_user: "bitty",
    },
];

/// Look up a guest by its stable identifier.
pub fn guest(id: &str) -> Option<&'static Guest> {
    GUESTS.iter().find(|guest| guest.id == id)
}

/// Run identifiers become directory names: lowercase ASCII letters, digits,
/// and `-`, starting with an alphanumeric, at most 64 characters.
pub fn run_id_is_safe(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= 64
        && run_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && run_id
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphanumeric())
}

/// Default run identifier: `<guest>-<unix-seconds>` (no RNG dependency).
pub fn default_run_id(guest_id: &str, unix_seconds: u64) -> String {
    format!("{guest_id}-{unix_seconds}")
}

/// A policy violation found by [`RunPlan::validate`] or [`validate_paths`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Stable rule identifier (grep-able in tests and reports).
    pub rule: &'static str,
    /// Human-readable explanation with the offending value.
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.rule, self.detail)
    }
}

/// Why a [`RunPlan`] could not be constructed at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// `run_id` is empty, too long, or carries path-unsafe characters.
    UnsafeRunId(String),
    /// The VM root is missing or empty.
    MissingRoot,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeRunId(run_id) => write!(
                f,
                "run id {run_id:?} is unsafe: use [a-z0-9-], start alphanumeric, max 64 chars"
            ),
            Self::MissingRoot => write!(f, "VM root is empty: set BITTY_VM_ROOT or pass --root"),
        }
    }
}

impl std::error::Error for PlanError {}

/// Every filesystem path a run derives. Separated from [`RunPlan`] so the
/// policy core is checkable against explicit layouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPaths {
    /// Prepared base image (read-only by convention).
    pub base_image: PathBuf,
    /// Disposable overlay booted by the run.
    pub overlay_image: PathBuf,
    /// Per-run scratch directory owning the overlay.
    pub run_dir: PathBuf,
    /// `<vm-root>/images/base`.
    pub images_dir: PathBuf,
    /// `<vm-root>/runs`.
    pub runs_dir: PathBuf,
}

/// `qemu-img create` arguments: qcow2 overlay over a qcow2 base image. No
/// other backing file and no installation media exist in this model.
pub fn qemu_img_create_args(base_image: &Path, overlay_image: &Path) -> Vec<String> {
    vec![
        "create".to_string(),
        "-f".to_string(),
        DISK_FORMAT.to_string(),
        "-F".to_string(),
        DISK_FORMAT.to_string(),
        "-b".to_string(),
        base_image.to_string_lossy().into_owned(),
        overlay_image.to_string_lossy().into_owned(),
    ]
}

/// Pure policy check over an explicit layout.
///
/// Rejected: ISO extensions, a missing backing file, an overlay that is the
/// base image, an overlay outside its run directory, a base image outside
/// `images/base`, and a base image inside the run scratch tree.
pub fn validate_paths(paths: &RunPaths) -> Result<(), Vec<Violation>> {
    let mut violations = Vec::new();
    let mut push = |rule: &'static str, detail: String| {
        violations.push(Violation { rule, detail });
    };

    let base = &paths.base_image;
    let overlay = &paths.overlay_image;

    if !has_extension(base, DISK_FORMAT) {
        push(
            "base-qcow2",
            format!("base image {} is not .{DISK_FORMAT}", base.display()),
        );
    }
    if !has_extension(overlay, DISK_FORMAT) {
        push(
            "overlay-qcow2",
            format!("overlay {} is not .{DISK_FORMAT}", overlay.display()),
        );
    }
    if has_extension(base, "iso") || has_extension(overlay, "iso") {
        push(
            "never-iso",
            "runs boot only qcow2 overlays; an ISO is never a run disk".to_string(),
        );
    }
    if base == overlay {
        push(
            "overlay-not-base",
            format!("overlay equals base image {}", base.display()),
        );
    }
    if !overlay.starts_with(&paths.run_dir) {
        push(
            "overlay-under-run-dir",
            format!(
                "overlay {} is outside its run directory {}",
                overlay.display(),
                paths.run_dir.display()
            ),
        );
    }
    if !base.starts_with(&paths.images_dir) {
        push(
            "base-under-images-dir",
            format!(
                "base image {} is outside {}",
                base.display(),
                paths.images_dir.display()
            ),
        );
    }
    if base.starts_with(&paths.runs_dir) {
        push(
            "base-outside-runs",
            format!(
                "base image {} is inside the run scratch tree {}",
                base.display(),
                paths.runs_dir.display()
            ),
        );
    }

    let args = qemu_img_create_args(base, overlay);
    let backing = args
        .windows(2)
        .any(|pair| pair[0] == "-b" && Path::new(&pair[1]) == base);
    if !backing {
        push(
            "overlay-backing-is-base",
            format!("overlay command does not back {}", base.display()),
        );
    }
    if args.iter().any(|arg| has_extension(Path::new(arg), "iso")) {
        push(
            "never-iso",
            "overlay command references an ISO path".to_string(),
        );
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// A fully derived plan for one guest run: base image, overlay, domain name,
/// and SSH target. Constructing a plan never touches the filesystem or
/// libvirt; validation is pure policy checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    /// Guest being planned.
    pub guest: &'static Guest,
    /// Cadence context of the run.
    pub cadence: Cadence,
    /// Run identifier (directory name under `<vm-root>/runs`).
    pub run_id: String,
    /// VM root: holds `images/base` and `runs`.
    pub vm_root: PathBuf,
}

impl RunPlan {
    /// Build a plan, rejecting an unsafe run id or an empty VM root.
    pub fn new(
        guest: &'static Guest,
        cadence: Cadence,
        run_id: impl Into<String>,
        vm_root: impl Into<PathBuf>,
    ) -> Result<Self, PlanError> {
        let run_id = run_id.into();
        if !run_id_is_safe(&run_id) {
            return Err(PlanError::UnsafeRunId(run_id));
        }
        let vm_root = vm_root.into();
        if vm_root.as_os_str().is_empty() {
            return Err(PlanError::MissingRoot);
        }
        Ok(Self {
            guest,
            cadence,
            run_id,
            vm_root,
        })
    }

    /// Every path this run derives.
    pub fn paths(&self) -> RunPaths {
        let images_dir = self.vm_root.join(BASE_IMAGES_DIR);
        let runs_dir = self.vm_root.join(RUNS_DIR);
        let run_dir = runs_dir.join(&self.run_id);
        RunPaths {
            base_image: images_dir.join(format!("{}.{}", self.guest.id, DISK_FORMAT)),
            overlay_image: run_dir.join(format!("{}-overlay.{}", self.guest.id, DISK_FORMAT)),
            run_dir,
            images_dir,
            runs_dir,
        }
    }

    /// The prepared base image for this guest.
    pub fn base_image(&self) -> PathBuf {
        self.paths().base_image
    }

    /// This run's scratch directory.
    pub fn run_dir(&self) -> PathBuf {
        self.paths().run_dir
    }

    /// The disposable overlay booted by this run.
    pub fn overlay_image(&self) -> PathBuf {
        self.paths().overlay_image
    }

    /// libvirt domain name for this run.
    pub fn domain_name(&self) -> String {
        format!("bitty-vm-{}-{}", self.run_id, self.guest.id)
    }

    /// SSH target used for test execution. The address is the domain name;
    /// the guest IP is resolved from the QEMU guest agent at run time, so
    /// this is a run-time hint, not a resolvable name.
    pub fn ssh_target(&self) -> String {
        format!("{}@{}", self.guest.ssh_user, self.domain_name())
    }

    /// `qemu-img create` arguments for the overlay.
    pub fn qemu_img_create_args(&self) -> Vec<String> {
        let paths = self.paths();
        qemu_img_create_args(&paths.base_image, &paths.overlay_image)
    }

    /// libvirt domain type: `kvm` for hardware acceleration, `qemu` for TCG.
    pub fn domain_type(&self) -> &'static str {
        match self.guest.accel {
            Accel::Kvm => "kvm",
            Accel::Tcg => "qemu",
        }
    }

    /// Dry-run libvirt domain XML for this plan: one virtio disk (the
    /// overlay, never the base image, never an ISO), one virtio network on
    /// libvirt's default network, and the guest-agent channel used to learn
    /// the guest IP for SSH.
    pub fn domain_xml(&self) -> String {
        let overlay = self.overlay_image();
        format!(
            "<domain type='{domain_type}'>\n  \
             <name>{name}</name>\n  \
             <memory unit='MiB'>2048</memory>\n  \
             <vcpu>2</vcpu>\n  \
             <os>\n    \
             <type arch='{arch}'>hvm</type>\n  \
             </os>\n  \
             <devices>\n    \
             <disk type='file' device='disk'>\n      \
             <driver name='qemu' type='{format}'/>\n      \
             <source file='{overlay}'/>\n      \
             <target dev='vda' bus='virtio'/>\n    \
             </disk>\n    \
             <interface type='network'>\n      \
             <source network='default'/>\n      \
             <model type='virtio'/>\n    \
             </interface>\n    \
             <channel type='unix'>\n      \
             <target type='virtio' name='org.qemu.guest_agent.0'/>\n    \
             </channel>\n    \
             <console type='pty'/>\n  \
             </devices>\n\
             </domain>\n",
            domain_type = self.domain_type(),
            name = xml_escape(&self.domain_name()),
            arch = self.guest.arch.domain_arch(),
            format = DISK_FORMAT,
            overlay = xml_escape(&overlay.to_string_lossy()),
        )
    }

    /// Machine check of every policy rule. `Ok(())` means the plan may be
    /// executed by the (environment-gated) live path.
    pub fn validate(&self) -> Result<(), Vec<Violation>> {
        let mut violations: Vec<Violation> = Vec::new();
        if !run_id_is_safe(&self.run_id) {
            violations.push(Violation {
                rule: "run-id-safe",
                detail: format!("unsafe run id {:?}", self.run_id),
            });
        }
        if self.vm_root.as_os_str().is_empty() {
            violations.push(Violation {
                rule: "root-set",
                detail: "VM root must not be empty".to_string(),
            });
        }
        if self.guest.ssh_user.is_empty() {
            violations.push(Violation {
                rule: "ssh-user-set",
                detail: format!("guest {} has no SSH user", self.guest.id),
            });
        }

        if let Err(mut found) = validate_paths(&self.paths()) {
            violations.append(&mut found);
        }
        if self.domain_xml().to_ascii_lowercase().contains(".iso") {
            violations.push(Violation {
                rule: "never-iso",
                detail: "domain XML references an ISO path".to_string(),
            });
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

fn has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(expected))
}

fn xml_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(guest_id: &str, cadence: Cadence, run_id: &str) -> RunPlan {
        let guest = guest(guest_id).expect("test guest exists");
        RunPlan::new(guest, cadence, run_id, PathBuf::from("/vm-root")).expect("plan builds")
    }

    fn paths(base: &str, overlay: &str, run_dir: &str) -> RunPaths {
        RunPaths {
            base_image: PathBuf::from(base),
            overlay_image: PathBuf::from(overlay),
            run_dir: PathBuf::from(run_dir),
            images_dir: PathBuf::from("/vm-root/images/base"),
            runs_dir: PathBuf::from("/vm-root/runs"),
        }
    }

    fn rules(violations: Vec<Violation>) -> Vec<&'static str> {
        violations.iter().map(|v| v.rule).collect()
    }

    #[test]
    fn cadence_matrix_is_staged_by_cost() {
        let pr: Vec<_> = Cadence::Pr.guests().map(|g| g.id).collect();
        assert_eq!(pr, ["arch"]);

        let main: Vec<_> = Cadence::Main.guests().map(|g| g.id).collect();
        assert_eq!(main, ["arch", "ubuntu", "fedora", "alpine"]);

        let nightly: Vec<_> = Cadence::Nightly.guests().map(|g| g.id).collect();
        assert_eq!(
            nightly,
            ["arch", "ubuntu", "fedora", "alpine", "arch-arm64"]
        );

        let arm = guest("arch-arm64").expect("arm guest exists");
        assert_eq!(arm.accel, Accel::Tcg);
        assert_eq!(arm.arch, GuestArch::Aarch64);
        assert_eq!(arm.first_cadence, Cadence::Nightly);
    }

    #[test]
    fn cadence_parse_roundtrips_stable_spellings() {
        for cadence in Cadence::ALL {
            assert_eq!(Cadence::parse(cadence.as_str()), Some(cadence));
        }
        assert_eq!(Cadence::parse("PR"), None);
        assert_eq!(Cadence::parse(""), None);
    }

    #[test]
    fn run_id_rules_reject_path_characters() {
        assert!(run_id_is_safe("arch-1700000000"));
        assert!(run_id_is_safe("a"));
        assert!(run_id_is_safe(&"a".repeat(64)));
        assert!(!run_id_is_safe(""));
        assert!(!run_id_is_safe(".."));
        assert!(!run_id_is_safe("a/b"));
        assert!(!run_id_is_safe("a\\b"));
        assert!(!run_id_is_safe("../escape"));
        assert!(!run_id_is_safe("-leading"));
        assert!(!run_id_is_safe("UPPER"));
        assert!(!run_id_is_safe(&"a".repeat(65)));
    }

    #[test]
    fn new_rejects_unsafe_ids_and_empty_root() {
        let guest = guest("arch").expect("arch exists");
        assert_eq!(
            RunPlan::new(guest, Cadence::Pr, "../x", "/vm-root"),
            Err(PlanError::UnsafeRunId("../x".to_string()))
        );
        assert_eq!(
            RunPlan::new(guest, Cadence::Pr, "ok", PathBuf::new()),
            Err(PlanError::MissingRoot)
        );
    }

    #[test]
    fn derived_paths_follow_the_overlay_policy() {
        let plan = plan("arch", Cadence::Pr, "arch-1");
        assert_eq!(
            plan.base_image(),
            PathBuf::from("/vm-root/images/base/arch.qcow2")
        );
        assert_eq!(
            plan.overlay_image(),
            PathBuf::from("/vm-root/runs/arch-1/arch-overlay.qcow2")
        );
        assert_eq!(plan.domain_name(), "bitty-vm-arch-1-arch");
        assert_eq!(plan.ssh_target(), "bitty@bitty-vm-arch-1-arch");
        assert!(plan.validate().is_ok(), "clean plan must validate");
    }

    #[test]
    fn overlay_command_backs_the_base_image() {
        let plan = plan("arch", Cadence::Pr, "arch-1");
        let args = plan.qemu_img_create_args();
        assert_eq!(
            args,
            [
                "create",
                "-f",
                "qcow2",
                "-F",
                "qcow2",
                "-b",
                "/vm-root/images/base/arch.qcow2",
                "/vm-root/runs/arch-1/arch-overlay.qcow2",
            ]
        );
        assert!(!args.iter().any(|a| a.ends_with(".iso")));
    }

    #[test]
    fn domain_xml_boots_the_overlay_and_has_no_iso() {
        let plan = plan("arch", Cadence::Pr, "arch-1");
        let xml = plan.domain_xml();
        assert!(xml.contains("<domain type='kvm'>"));
        assert!(xml.contains("arch-1/arch-overlay.qcow2"));
        assert!(!xml.contains("arch.qcow2'"), "base must not be a disk");
        assert!(!xml.to_ascii_lowercase().contains(".iso"));
        assert!(xml.contains("<name>bitty-vm-arch-1-arch</name>"));
        assert!(xml.contains("org.qemu.guest_agent.0"));
    }

    #[test]
    fn tcg_guest_uses_qemu_domain_type_and_virt_machine() {
        let plan = plan("arch-arm64", Cadence::Nightly, "arm-1");
        assert_eq!(plan.domain_type(), "qemu");
        assert_eq!(plan.guest.arch.qemu_machine(), "virt");
        assert!(plan.domain_xml().contains("<domain type='qemu'>"));
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn validate_paths_accepts_the_policy_layout() {
        let layout = paths(
            "/vm-root/images/base/arch.qcow2",
            "/vm-root/runs/arch-1/arch-overlay.qcow2",
            "/vm-root/runs/arch-1",
        );
        assert_eq!(validate_paths(&layout), Ok(()));
    }

    #[test]
    fn validate_paths_rejects_iso_disks() {
        let base_iso = paths(
            "/vm-root/images/base/arch.iso",
            "/vm-root/runs/arch-1/arch-overlay.qcow2",
            "/vm-root/runs/arch-1",
        );
        let found = rules(validate_paths(&base_iso).expect_err("ISO base must fail"));
        assert!(found.contains(&"base-qcow2"), "{found:?}");
        assert!(found.contains(&"never-iso"), "{found:?}");

        let overlay_iso = paths(
            "/vm-root/images/base/arch.qcow2",
            "/vm-root/runs/arch-1/arch-overlay.iso",
            "/vm-root/runs/arch-1",
        );
        let found = rules(validate_paths(&overlay_iso).expect_err("ISO overlay must fail"));
        assert!(found.contains(&"overlay-qcow2"), "{found:?}");
        assert!(found.contains(&"never-iso"), "{found:?}");
    }

    #[test]
    fn validate_paths_rejects_base_in_run_tree_and_shared_overlay() {
        let base_in_runs = paths(
            "/vm-root/runs/arch-1/arch.qcow2",
            "/vm-root/runs/arch-1/arch-overlay.qcow2",
            "/vm-root/runs/arch-1",
        );
        let found = rules(validate_paths(&base_in_runs).expect_err("base in runs must fail"));
        assert!(found.contains(&"base-outside-runs"), "{found:?}");
        assert!(found.contains(&"base-under-images-dir"), "{found:?}");

        let overlay_outside = paths(
            "/vm-root/images/base/arch.qcow2",
            "/vm-root/runs/other/arch-overlay.qcow2",
            "/vm-root/runs/arch-1",
        );
        let found =
            rules(validate_paths(&overlay_outside).expect_err("overlay outside run dir must fail"));
        assert!(found.contains(&"overlay-under-run-dir"), "{found:?}");

        let shared = paths(
            "/vm-root/images/base/arch.qcow2",
            "/vm-root/images/base/arch.qcow2",
            "/vm-root/runs/arch-1",
        );
        let found = rules(validate_paths(&shared).expect_err("overlay == base must fail"));
        assert!(found.contains(&"overlay-not-base"), "{found:?}");
    }

    #[test]
    fn xml_escape_handles_hostile_path_characters() {
        let guest = guest("arch").expect("arch exists");
        let plan = RunPlan::new(guest, Cadence::Pr, "arch-1", "/vm-root & <home>").expect("plan");
        let xml = plan.domain_xml();
        assert!(xml.contains("/vm-root &amp; &lt;home&gt;"));
        assert!(!xml.contains("& <home>"));
    }

    #[test]
    fn default_run_id_is_safe() {
        let run_id = default_run_id("arch", 1_700_000_000);
        assert_eq!(run_id, "arch-1700000000");
        assert!(run_id_is_safe(&run_id));
    }
}
