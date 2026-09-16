//! Host capability probes for the VM tier.
//!
//! Probing is split from decision logic: [`find_on_path`] and
//! [`probe_with`] take their inputs as arguments and are unit-testable
//! without touching the process environment, while [`Capabilities::probe`]
//! wires in the real `PATH` and filesystem. The VM tier is a Linux-host
//! capability; off Linux the probes report "unavailable" instead of
//! guessing.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::policy::GuestArch;

/// Kernel device required for KVM hardware acceleration.
pub const KVM_DEVICE: &str = "/dev/kvm";

/// Resolved host tool and device availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// `/dev/kvm` exists (Linux only; `false` elsewhere).
    pub kvm_device: bool,
    /// QEMU system binary for the requested guest architecture.
    pub qemu_system: Option<PathBuf>,
    /// `qemu-img`, used to create and inspect qcow2 overlays.
    pub qemu_img: Option<PathBuf>,
    /// libvirt `virsh`; needed by the deferred guest-boot stage only.
    pub virsh: Option<PathBuf>,
    /// OpenSSH client; needed by the deferred guest-access stage only.
    pub ssh: Option<PathBuf>,
}

impl Capabilities {
    /// Probe the real host for a guest architecture.
    pub fn probe(arch: GuestArch) -> Self {
        let path = std::env::var_os("PATH");
        Self::probe_with(path.as_deref(), arch, kvm_device_present(), is_executable)
    }

    /// Pure probe with injected `PATH`, KVM presence, and executable test.
    pub fn probe_with(
        path_value: Option<&OsStr>,
        arch: GuestArch,
        kvm_device: bool,
        is_exec: impl Fn(&Path) -> bool,
    ) -> Self {
        let find = |name: &str| find_on_path(path_value, name, &is_exec);
        Self {
            kvm_device,
            qemu_system: find(arch.qemu_system()),
            qemu_img: find("qemu-img"),
            virsh: find("virsh"),
            ssh: find("ssh"),
        }
    }

    /// Hardware acceleration can be attempted: `/dev/kvm` plus a QEMU
    /// system binary. The actual probe still has to succeed at run time.
    pub fn kvm_usable(&self) -> bool {
        self.kvm_device && self.qemu_system.is_some()
    }

    /// Labels of the prerequisites that are missing for a KVM smoke.
    pub fn missing_for_kvm(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.kvm_device {
            missing.push(KVM_DEVICE);
        }
        if self.qemu_system.is_none() {
            missing.push("qemu-system");
        }
        missing
    }

    /// Labels of the prerequisites for creating an overlay (base image
    /// presence is checked against the plan separately).
    pub fn missing_for_overlay(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.qemu_img.is_none() {
            missing.push("qemu-img");
        }
        missing
    }
}

/// Whether `/dev/kvm` is present. Always `false` off Linux.
pub fn kvm_device_present() -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(KVM_DEVICE).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Search `PATH` for an executable by name. `path_value` is the raw `PATH`
/// value; `is_exec` decides whether one candidate is runnable. On Windows,
/// `name.exe` is probed as well.
pub fn find_on_path(
    path_value: Option<&OsStr>,
    name: &str,
    is_exec: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let path_value = path_value?;
    for dir in std::env::split_paths(path_value) {
        let candidate = dir.join(name);
        if is_exec(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if is_exec(&exe) {
                return Some(exe);
            }
        }
    }
    None
}

/// Whether `path` is an existing regular file that can be executed.
pub fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_on_path_scans_in_order_and_stops_at_first_hit() {
        // Build the PATH value with the platform separator (`;` on Windows)
        // and compare with `Path` joins so the test is separator-agnostic.
        let dirs = ["/opt/a", "/opt/b", "/opt/c"];
        let path = std::env::join_paths(dirs).expect("join PATH entries");
        let hit = Path::new(dirs[1]).join("qemu-img");
        let probe = |candidate: &Path| candidate == hit;
        assert_eq!(
            find_on_path(Some(path.as_os_str()), "qemu-img", probe),
            Some(hit)
        );
    }

    #[test]
    fn find_on_path_returns_none_when_missing_or_unset() {
        let probe = |_: &Path| false;
        assert_eq!(
            find_on_path(Some(OsStr::new("/opt/a")), "virsh", probe),
            None
        );
        assert_eq!(find_on_path(None, "virsh", probe), None);
    }

    #[test]
    fn probe_selects_the_binary_for_the_guest_architecture() {
        let path = OsStr::new("/nix/bin");
        let probe = |p: &Path| p.file_name().is_some_and(|n| n == "qemu-system-aarch64");
        let caps = Capabilities::probe_with(Some(path), GuestArch::Aarch64, false, probe);
        assert_eq!(
            caps.qemu_system,
            Some(PathBuf::from("/nix/bin/qemu-system-aarch64"))
        );
        assert!(!caps.kvm_usable());
        assert_eq!(caps.missing_for_kvm(), [KVM_DEVICE]);
    }

    #[test]
    fn kvm_usable_requires_device_and_binary() {
        let caps = Capabilities {
            kvm_device: true,
            qemu_system: Some(PathBuf::from("/usr/bin/qemu-system-x86_64")),
            qemu_img: None,
            virsh: None,
            ssh: None,
        };
        assert!(caps.kvm_usable());
        assert!(caps.missing_for_kvm().is_empty());
        assert_eq!(caps.missing_for_overlay(), ["qemu-img"]);

        let no_device = Capabilities {
            kvm_device: false,
            ..caps
        };
        assert!(!no_device.kvm_usable());
        assert_eq!(no_device.missing_for_kvm(), [KVM_DEVICE]);
    }
}
