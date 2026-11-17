//! External no-TCP scan for one live instance (`P0-AC-023`, bitty#2921 /
//! CTX-1865).
//!
//! The acceptance criterion is an *external* scan: the daemon/instance process
//! must hold no listening TCP socket and no outbound TCP connection across a
//! full lifecycle. This module is that scan, and it is deliberately built out
//! of observations the test does not control:
//!
//! - the socket list comes from `ss -tanp` (iproute2), not from `/proc` or from
//!   anything the binary reports about itself;
//! - the PID set is the instance's own process group, established by
//!   [`TestInstance::spawn_test_mode`]'s `process_group(0)` and read back from
//!   `/proc/<pid>/stat` — never a name or pattern match, so a socket belonging
//!   to a process this test did not start can never be attributed to it (and
//!   never gets counted as a violation either);
//! - [`self_check`] proves the filter is *not vacuous* by opening a real
//!   loopback listener in the test process and requiring the same scan to see
//!   it.
//!
//! # Loud failure, never a silent pass
//!
//! If no `ss` binary exists this module panics with the reason. A host that
//! cannot perform the external scan cannot pass the cell.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

use super::TestInstance;

/// Environment override for the `ss` binary under test.
const SS_ENV: &str = "BITTY_TEST_SS";

/// Absolute candidates, then `PATH`. Never a shell, never a glob.
const SS_CANDIDATES: [&str; 4] = ["/usr/sbin/ss", "/sbin/ss", "/usr/bin/ss", "/bin/ss"];

/// One scan of one PID set.
#[derive(Debug, Clone)]
pub(crate) struct TcpScan {
    /// PIDs the scan attributed sockets to (and filtered by).
    pub(crate) pids: Vec<u32>,
    /// `LISTEN` rows attributable to `pids`.
    pub(crate) listening: Vec<String>,
    /// `ESTAB` rows attributable to `pids`.
    pub(crate) established: Vec<String>,
    /// Every other attributable TCP row (`TIME-WAIT` and friends).
    pub(crate) other: Vec<String>,
    /// The exact command line that produced this scan.
    pub(crate) command: String,
    /// Total TCP rows `ss` reported, for context (not filtered).
    pub(crate) total_rows: usize,
}

impl TcpScan {
    /// Assert the PID set holds no TCP socket at all, and print the evidence.
    ///
    /// Zero in *every* state is the claim: a listener or an outbound
    /// connection would show up as a row, and a half-open or lingering socket
    /// would show up in [`TcpScan::other`].
    pub(crate) fn assert_no_tcp(&self, phase: &str) {
        println!(
            "AC023 {phase}: pids={:?} tcp_rows_attributed={} (listen={}, established={}, other={}), rows_total={} via `{}`",
            self.pids,
            self.listening.len() + self.established.len() + self.other.len(),
            self.listening.len(),
            self.established.len(),
            self.other.len(),
            self.total_rows,
            self.command
        );
        let mut violations = Vec::new();
        for (label, rows) in [
            ("LISTEN", &self.listening),
            ("ESTABLISHED", &self.established),
            ("OTHER", &self.other),
        ] {
            for row in rows {
                violations.push(format!("{label} {row}"));
            }
        }
        assert!(
            violations.is_empty(),
            "{} must hold zero TCP sockets, but `{}` attributed {} row(s) to PIDs {:?}:\n{}",
            phase,
            self.command,
            violations.len(),
            self.pids,
            violations.join("\n")
        );
    }
}

/// Resolve the `ss` binary, or panic loudly with why the cell cannot run.
fn ss_binary() -> PathBuf {
    if let Some(explicit) = std::env::var_os(SS_ENV) {
        let path = PathBuf::from(explicit);
        assert!(
            path.is_file(),
            "{SS_ENV}={} is not a file: the no-TCP cell cannot run its external scan, and \
             refusing to pass without one",
            path.display()
        );
        return path;
    }
    for candidate in SS_CANDIDATES {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("ss");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    panic!(
        "AC023 no-TCP cell cannot run: no `ss` binary in {SS_CANDIDATES:?} or on PATH, so the \
         external scan the criterion names cannot be performed. Install iproute2 or point \
         {SS_ENV} at it. This cell fails rather than reporting a pass it did not observe."
    );
}

/// Scan the TCP sockets `ss` attributes to `pids`.
///
/// Panics when `ss` cannot be run or exits unsuccessfully: an unreadable scan
/// is not evidence of zero sockets.
pub(crate) fn scan_pids(pids: &[u32]) -> TcpScan {
    let ss = ss_binary();
    let command = format!("{} -tanpH", ss.display());
    let output = Command::new(&ss)
        .args(["-tanp", "-H"])
        .output()
        .unwrap_or_else(|err| panic!("AC023 cannot run `{command}`: {err}"));
    assert!(
        output.status.success(),
        "AC023 `{command}` failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut listening = Vec::new();
    let mut established = Vec::new();
    let mut other = Vec::new();
    let mut total_rows = 0;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        total_rows += 1;
        if !attributed_to(trimmed, pids) {
            continue;
        }
        match trimmed.split_whitespace().next() {
            Some("LISTEN") => listening.push(trimmed.to_owned()),
            Some("ESTAB") => established.push(trimmed.to_owned()),
            _ => other.push(trimmed.to_owned()),
        }
    }

    TcpScan {
        pids: pids.to_vec(),
        listening,
        established,
        other,
        command,
        total_rows,
    }
}

/// Scan one live instance's session tree: its process group plus descendants.
pub(crate) fn scan(instance: &TestInstance) -> TcpScan {
    scan_pids(&instance.session_pids())
}

/// Whether an `ss` row names one of `pids` as the socket's owner.
///
/// The `users:(("name",pid=1234,fd=7))` column is only emitted with `-p`, and
/// only for sockets the caller may inspect; a row without it is therefore
/// never attributed. The trailing comma keeps `pid=12` from matching
/// `pid=123`.
fn attributed_to(row: &str, pids: &[u32]) -> bool {
    pids.iter().any(|pid| row.contains(&format!("pid={pid},")))
}

/// Prove a scan can see a listening TCP socket before trusting a zero result.
///
/// Opens a loopback listener in this (test) process, scans for this process's
/// PID, and requires the listener to be reported as `LISTEN`. Returns the
/// count observed so the caller can print it.
pub(crate) fn self_check() -> usize {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback control listener");
    let pid = std::process::id();
    let scan = scan_pids(&[pid]);
    let observed = scan.listening.len();
    assert!(
        observed >= 1,
        "AC023 control failed: `{}` did not attribute the loopback listener held by pid {pid} to \
         it, so a zero result for the instance would be vacuous. Scan rows: listen={:?} \
         established={:?} other={:?} (of {} total rows)",
        scan.command,
        scan.listening,
        scan.established,
        scan.other,
        scan.total_rows
    );
    drop(listener);
    observed
}
