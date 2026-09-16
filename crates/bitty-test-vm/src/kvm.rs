//! Bounded QEMU accelerator probe: the smallest honest KVM smoke.
//!
//! The probe starts a paused QEMU machine (`-S`) with the requested
//! accelerator, negotiates QMP capabilities, asks QEMU to quit, and waits
//! under a hard deadline. It proves that the accelerator actually
//! initialises on this host (KVM opens `/dev/kvm` and creates a vCPU; TCG
//! is CPU-only emulation) without any guest image, ISO, or libvirt.
//!
//! It only ever kills the child it spawned, and it never runs by default:
//! `smoke --execute` (or `BITTY_VM_LIVE`) is required, and the live
//! integration test is additionally gated.

use std::io::{self, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::policy::{Accel, GuestArch};

/// Hard deadline for one probe, from spawn to reap.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Poll interval while waiting for QEMU to exit.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// QMP handshake followed by a quit request.
pub const QMP_HANDSHAKE: &[u8] = b"{\"execute\":\"qmp_capabilities\"}\n{\"execute\":\"quit\"}\n";

/// Probe result classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    /// The accelerator initialised and QEMU processed the QMP quit.
    Usable,
    /// QEMU ran but refused or failed; `detail` carries the reason.
    Unusable,
    /// QEMU did not exit within the deadline and was killed.
    TimedOut,
}

/// One probe result with a human-readable detail line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// Classification.
    pub status: ProbeStatus,
    /// Evidence or failure reason.
    pub detail: String,
}

/// QEMU arguments of the bounded probe (paused machine, QMP over stdio).
pub fn probe_args(arch: GuestArch, accel: Accel) -> Vec<String> {
    vec![
        "-accel".to_string(),
        accel.qemu_value().to_string(),
        "-machine".to_string(),
        arch.qemu_machine().to_string(),
        "-m".to_string(),
        "128".to_string(),
        "-display".to_string(),
        "none".to_string(),
        "-nodefaults".to_string(),
        "-S".to_string(),
        "-qmp".to_string(),
        "stdio".to_string(),
    ]
}

/// Run one bounded probe. Returns `Err` only when QEMU cannot be spawned at
/// all; a QEMU that exits with a failure is reported as
/// [`ProbeStatus::Unusable`].
pub fn run_accel_probe(
    qemu_system: &Path,
    arch: GuestArch,
    accel: Accel,
    timeout: Duration,
) -> io::Result<ProbeOutcome> {
    let mut child = Command::new(qemu_system)
        .args(probe_args(arch, accel))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(QMP_HANDSHAKE);
        // Dropping stdin closes the pipe; QEMU has both requests buffered.
    }

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }

    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(classify(
        output.status.success(),
        &stdout,
        &stderr,
        timed_out,
        timeout,
    ))
}

/// Pure classification of one probe result.
pub fn classify(
    success: bool,
    stdout: &str,
    stderr: &str,
    timed_out: bool,
    timeout: Duration,
) -> ProbeOutcome {
    if timed_out {
        return ProbeOutcome {
            status: ProbeStatus::TimedOut,
            detail: format!("QEMU did not quit within {}s; killed", timeout.as_secs()),
        };
    }
    if success && stdout.contains("\"QMP\"") {
        return ProbeOutcome {
            status: ProbeStatus::Usable,
            detail: "QEMU initialised the accelerator and processed the QMP quit".to_string(),
        };
    }
    let reason = crate::overlay::first_non_empty_line(stderr)
        .or_else(|| crate::overlay::first_non_empty_line(stdout))
        .unwrap_or("no QEMU output");
    ProbeOutcome {
        status: ProbeStatus::Unusable,
        detail: format!("QEMU did not become ready: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_args_pin_the_accelerator_and_machine() {
        let args = probe_args(GuestArch::X86_64, Accel::Kvm);
        assert!(args.windows(2).any(|w| w == ["-accel", "kvm"]));
        assert!(args.windows(2).any(|w| w == ["-machine", "q35"]));
        assert!(args.windows(2).any(|w| w == ["-qmp", "stdio"]));
        assert!(args.iter().any(|a| a == "-S"));

        let arm = probe_args(GuestArch::Aarch64, Accel::Tcg);
        assert!(arm.windows(2).any(|w| w == ["-accel", "tcg"]));
        assert!(arm.windows(2).any(|w| w == ["-machine", "virt"]));
    }

    #[test]
    fn classify_requires_a_qmp_greeting() {
        let timeout = Duration::from_secs(1);
        let usable = classify(true, "{\"QMP\": {\"version\": {}}}", "", false, timeout);
        assert_eq!(usable.status, ProbeStatus::Usable);

        let silent = classify(true, "booted", "", false, timeout);
        assert_eq!(silent.status, ProbeStatus::Unusable);

        let failed = classify(
            false,
            "",
            "/dev/kvm: No such file or directory",
            false,
            timeout,
        );
        assert_eq!(failed.status, ProbeStatus::Unusable);
        assert!(failed.detail.contains("/dev/kvm"));

        let timed_out = classify(false, "", "", true, Duration::from_secs(5));
        assert_eq!(timed_out.status, ProbeStatus::TimedOut);
        assert!(timed_out.detail.contains("5s"));
    }
}
