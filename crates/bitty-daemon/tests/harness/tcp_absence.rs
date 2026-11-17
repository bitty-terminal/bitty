//! External TCP observation for the `bittyd` no-TCP cell (bitty#2921).
//!
//! # Origin
//!
//! Copied from `crates/bitty-terminal/tests/live_instance/tcp_absence.rs`
//! (`CTX-1865`), which is the terminal-instance half of the same control: the
//! `ss -tanp` scan, the `/proc` PID attribution, the loopback control that
//! keeps a zero result from being vacuous, and the loud failure when no `ss`
//! binary exists. Integration-test fixtures are per-crate, and the terminal-side
//! file belongs to another lane, so this crate carries its own copy rather than
//! editing that cell. Two additions are made here and marked below:
//!
//! - [`session_pids`] builds the PID set from a *recorded root PID* instead of a
//!   fixture field, so it fits `bittyd`'s launch shape as well as the instance's;
//! - [`scan_namespace`] reads the TCP table of the daemon's **own network
//!   namespace** from outside the process. It exists because the daemon serves
//!   non-dumpable (`src/service/server.rs`, `harden_core_dumps`), and a
//!   non-dumpable same-uid process cannot be attributed by `ss -p` at all — its
//!   `/proc/<pid>/fd` is refused to the observer, so the `users:(("name",pid=..))`
//!   column never names it. See the cell's header for how the two channels cover
//!   each other.
//!
//! # Why this is not in `harness/mod.rs`
//!
//! It is included through `#[path = "harness/tcp_absence.rs"]` by
//! `tests/no_tcp_listener_absence.rs` only. Declaring it in `harness/mod.rs`
//! would compile it into the conformance binary as well, where the items this
//! cell alone uses would be dead code and trip `-D warnings`.
//!
//! # Loud failure, never a silent pass
//!
//! If no `ss` binary exists, [`ss_binary`] panics with the reason: a host that
//! cannot perform the external scan cannot pass the cell.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

/// Environment override for the `ss` binary under test.
const SS_ENV: &str = "BITTY_TEST_SS";

/// Absolute candidates, then `PATH`. Never a shell, never a glob.
const SS_CANDIDATES: [&str; 4] = ["/usr/sbin/ss", "/sbin/ss", "/usr/bin/ss", "/bin/ss"];

/// One `ss` scan of one PID set.
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
    /// Every TCP row attributed to the scanned PID set.
    pub(crate) fn attributed(&self) -> usize {
        self.listening.len() + self.established.len() + self.other.len()
    }

    /// Assert the PID set holds no TCP socket at all, and print the evidence.
    ///
    /// Zero in *every* state is the claim: a listener or an outbound
    /// connection would show up as a row, and a half-open or lingering socket
    /// would show up in [`TcpScan::other`].
    pub(crate) fn assert_no_tcp(&self, phase: &str) {
        println!(
            "NOTCP {phase}: pids={:?} tcp_rows_attributed={} (listen={}, established={}, other={}), rows_total={} via `{}`",
            self.pids,
            self.attributed(),
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
pub(crate) fn ss_binary() -> PathBuf {
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
        "NOTCP no-TCP cell cannot run: no `ss` binary in {SS_CANDIDATES:?} or on PATH, so the \
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
        .unwrap_or_else(|err| panic!("NOTCP cannot run `{command}`: {err}"));
    assert!(
        output.status.success(),
        "NOTCP `{command}` failed with {}: {}",
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
        "NOTCP control failed: `{}` did not attribute the loopback listener held by pid {pid} to \
         it, so a zero result for the daemon would be vacuous. Scan rows: listen={:?} \
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

/// The two `/proc/<pid>/stat` fields this module reads.
struct ProcEntry {
    /// Parent PID (field 4).
    ppid: u32,
    /// Process-group id (field 5).
    pgid: u32,
}

/// Every live process as `(pid, { ppid, pgid })`, read from `/proc`.
///
/// `comm` (field 2) is parenthesised and may itself contain spaces or
/// parentheses, so parsing starts after the *last* `)`; the remaining fields
/// are `state ppid pgrp ...`, which puts `ppid` at index 1 and `pgrp` at
/// index 2.
///
/// A process that exits mid-walk is simply absent: `/proc` is a snapshot, and
/// every caller here tolerates a shrinking tree.
fn proc_table() -> Vec<(u32, ProcEntry)> {
    let mut table = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("tcp_absence: cannot read /proc: {err}");
            return table;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|raw| raw.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let Some(rest) = stat.get(close + 1..) else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let _state = fields.next();
        let (Some(ppid), Some(pgid)) = (
            fields.next().and_then(|raw| raw.parse::<u32>().ok()),
            fields.next().and_then(|raw| raw.parse::<u32>().ok()),
        ) else {
            continue;
        };
        table.push((pid, ProcEntry { ppid, pgid }));
    }
    table
}

/// The tree rooted at `root`: its process group, plus every descendant.
///
/// `root` is a PID this cell recorded at spawn and spawned with
/// `process_group(0)`, so it leads its own group and `/proc/<pid>/stat` is the
/// only source used. No name or pattern match is involved, so a socket
/// belonging to a process this cell did not start can never be attributed to
/// it (and never counted as a violation either).
///
/// A child that calls `setsid()` leaves the group but is still found through
/// its parent link while its parent lives; once the recorded `root` is reaped
/// the descendants it detached are no longer reachable from it, which is why
/// the callers word their post-mortem phases to match.
pub(crate) fn session_pids(root: u32) -> Vec<u32> {
    let table = proc_table();
    let mut pids: Vec<u32> = table
        .iter()
        .filter(|(_, process)| process.pgid == root)
        .map(|(pid, _)| *pid)
        .collect();
    // A reaped process is absent from `/proc`: it must not be reported as a
    // live member of its own session.
    if table.iter().any(|(pid, _)| *pid == root) {
        pids.push(root);
    }
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, process) in &table {
            if process.ppid == parent && !pids.contains(pid) {
                pids.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// The network-namespace identity of `pid`, or `None` when it has no `/proc`
/// entry.
///
/// Two PIDs share a TCP table exactly when these strings are equal, so a cell
/// can state which namespace it observed instead of assuming one.
pub(crate) fn namespace_identity(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/ns/net"))
        .ok()
        .map(|target| target.to_string_lossy().into_owned())
}

/// One scan of one process's network-namespace TCP table.
///
/// This is a second, independent channel from [`TcpScan`]: it does not need
/// `-p` attribution, so it also covers a process the observer may not inspect
/// (`PR_SET_DUMPABLE=0`). Its scope is the whole namespace, so it is decisive
/// when every member of that namespace belongs to the cell — the driver and the
/// daemon do — and the arm asserts the daemon is in the driver's namespace and
/// that the namespace is not the cell's own. Reading also never depends on the
/// observed process's uid: `/proc/<pid>/net/tcp{,6}` is not pid-attributed.
#[derive(Debug, Clone)]
pub(crate) struct NamespaceScan {
    /// The PID whose namespace was read.
    pub(crate) pid: u32,
    /// The exact files that produced this scan.
    pub(crate) command: String,
    /// Every TCP row in the namespace (header excluded).
    pub(crate) rows: Vec<String>,
}

impl NamespaceScan {
    /// Assert the namespace holds no TCP socket at all, and print the evidence.
    pub(crate) fn assert_no_tcp(&self, phase: &str) {
        println!(
            "NOTCP {phase}: namespace_of_pid={} tcp_rows_in_namespace={} via `{}`",
            self.pid,
            self.rows.len(),
            self.command
        );
        assert!(
            self.rows.is_empty(),
            "{} must hold zero TCP sockets, but `{}` reported {} row(s) in the namespace of \
             pid {}:\n{}",
            phase,
            self.command,
            self.rows.len(),
            self.pid,
            self.rows.join("\n")
        );
    }
}

/// Read the TCP table of `pid`'s network namespace from outside the process.
///
/// `/proc/<pid>/net/tcp{,6}` is the kernel's per-namespace table, readable for
/// a same-uid process even when `PR_SET_DUMPABLE=0` refuses `/proc/<pid>/fd`.
/// An unreadable table panics: no table is not evidence of no sockets.
pub(crate) fn scan_namespace(pid: u32) -> NamespaceScan {
    let mut rows = Vec::new();
    for table in ["tcp", "tcp6"] {
        let path = format!("/proc/{pid}/net/{table}");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("NOTCP cannot read `{path}` for pid {pid}: {err}"));
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("sl ") {
                continue;
            }
            rows.push(format!("{table}: {trimmed}"));
        }
    }
    NamespaceScan {
        pid,
        command: format!("/proc/{pid}/net/tcp{{,6}}"),
        rows,
    }
}
