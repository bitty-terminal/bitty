//! Process identity facts for the orphan journal (contract `D-06`).
//!
//! A record is actionable only when the daemon can prove the exact process it
//! recorded is still that process. The identity is `(boot_id, pid, process
//! start time, process group, uid)`; a bare pid is never enough. `BootId`
//! fences a reboot, the start time fences pid reuse, the uid fences a process
//! that changed ownership, and the containment identity (recorded separately
//! by the journal) fences the whole tree.
//!
//! Linux reads `/proc`; other platform cells return `Unreadable` (and no boot
//! id) and therefore quarantine every record. This module does not signal.

use std::fmt;
use std::num::NonZeroU32;

/// Canonical UUID text length (the Linux boot id shape).
const BOOT_ID_CHARS: usize = 36;

#[cfg(any(target_os = "linux", target_os = "android"))]
const PROC_BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

#[cfg(any(target_os = "linux", target_os = "android"))]
const PROC_ROOT: &str = "/proc";

/// A boot identity that fences records across a reboot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BootId(Box<str>);

impl BootId {
    /// Parse a canonical lowercase UUID.
    ///
    /// # Errors
    ///
    /// Returns [`BootIdError`] for a value that is not 36 characters with
    /// hyphens at the UUID positions and lowercase hex elsewhere.
    pub fn parse(value: &str) -> Result<Self, BootIdError> {
        let bytes = value.as_bytes();
        if bytes.len() != BOOT_ID_CHARS {
            return Err(BootIdError);
        }
        for (index, byte) in bytes.iter().enumerate() {
            let valid = match index {
                8 | 13 | 18 | 23 => *byte == b'-',
                _ => matches!(*byte, b'0'..=b'9' | b'a'..=b'f'),
            };
            if !valid {
                return Err(BootIdError);
            }
        }
        Ok(Self(value.into()))
    }

    /// Read the Linux kernel boot id.
    ///
    /// Returns `None` on failure or on a platform cell that has no probe. A
    /// journal must not open with a guessed value.
    #[must_use]
    pub fn read_system() -> Option<Self> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let text = std::fs::read_to_string(PROC_BOOT_ID_PATH).ok()?;
            Self::parse(text.trim()).ok()
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            None
        }
    }

    /// The canonical text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BootId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A boot id that is not canonical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootIdError;

impl fmt::Display for BootIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("boot id is not a canonical lowercase UUID")
    }
}

impl std::error::Error for BootIdError {}

/// A positive process id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pid(NonZeroU32);

impl Pid {
    /// Build a pid; zero is not a process.
    #[must_use]
    pub fn new(value: u32) -> Option<Self> {
        NonZeroU32::new(value).map(Self)
    }

    /// The numeric pid.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// A positive process group id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessGroupId(NonZeroU32);

impl ProcessGroupId {
    /// Build a process group id; zero is not a group.
    #[must_use]
    pub fn new(value: u32) -> Option<Self> {
        NonZeroU32::new(value).map(Self)
    }

    /// The numeric group id.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Process start time in clock ticks since boot (`/proc/<pid>/stat` field 22).
///
/// This is not wall-clock time and is only compared for equality with the
/// value recorded at spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessStartTime(u64);

impl ProcessStartTime {
    /// Build a start time.
    #[must_use]
    pub const fn new(ticks_since_boot: u64) -> Self {
        Self(ticks_since_boot)
    }

    /// Ticks since boot.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The exact process tuple recorded before a worker can be observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pid: Pid,
    start_time: ProcessStartTime,
    process_group: ProcessGroupId,
    uid: u32,
}

impl ProcessIdentity {
    /// Build a process identity from the observed tuple.
    #[must_use]
    pub const fn new(
        pid: Pid,
        start_time: ProcessStartTime,
        process_group: ProcessGroupId,
        uid: u32,
    ) -> Self {
        Self {
            pid,
            start_time,
            process_group,
            uid,
        }
    }

    /// The recorded pid.
    #[must_use]
    pub const fn pid(self) -> Pid {
        self.pid
    }

    /// The recorded start time.
    #[must_use]
    pub const fn start_time(self) -> ProcessStartTime {
        self.start_time
    }

    /// The recorded process group.
    #[must_use]
    pub const fn process_group(self) -> ProcessGroupId {
        self.process_group
    }

    /// The recorded process uid.
    #[must_use]
    pub const fn uid(self) -> u32 {
        self.uid
    }
}

/// What a probe observes for one pid right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessObservation {
    /// The pid does not exist.
    Absent,
    /// The pid exists with an observable start time, process group, and uid.
    Present {
        /// Current start time.
        start_time: ProcessStartTime,
        /// Current process group.
        process_group: ProcessGroupId,
        /// Current process uid.
        uid: u32,
    },
    /// The pid exists but its stat could not be read or parsed.
    Unreadable,
}

/// Read-only process facts used to classify an orphan record.
///
/// The probe never signals and never decides; the journal classifies, and only
/// an exact match reaches a [`crate::orphan::OrphanReaper`].
pub trait ProcessProbe {
    /// The current boot id, or `None` when the platform cell cannot supply it.
    fn boot_id(&self) -> Option<BootId>;

    /// Observe one pid.
    fn observe(&self, pid: Pid) -> ProcessObservation;
}

/// An orphan whose boot id matched and whose containment identity may be
/// verified.
///
/// Construction is private to the journal: this is the only value an
/// [`crate::orphan::OrphanReaper`] can be asked to terminate, which is what
/// makes "never signal an unverified record" structural rather than a calling
/// convention. The identity is `None` for a reserved cgroup record (no pid was
/// committed) or a pid-reuse case; the reaper acts on the recorded containment
/// identity and never on a reused pid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedOrphan {
    boot_id: BootId,
    identity: Option<ProcessIdentity>,
    containment: crate::containment::ContainmentRecord,
}

impl VerifiedOrphan {
    pub(crate) fn new(
        boot_id: BootId,
        identity: Option<ProcessIdentity>,
        containment: crate::containment::ContainmentRecord,
    ) -> Self {
        Self {
            boot_id,
            identity,
            containment,
        }
    }

    /// The matched boot id.
    #[must_use]
    pub fn boot_id(&self) -> &BootId {
        &self.boot_id
    }

    /// The matched process tuple, when the recorded pid still matched.
    #[must_use]
    pub const fn identity(&self) -> Option<ProcessIdentity> {
        self.identity
    }

    /// The recorded containment identity.
    #[must_use]
    pub const fn containment(&self) -> &crate::containment::ContainmentRecord {
        &self.containment
    }
}

/// Production probe backed by Linux `/proc`.
///
/// On macOS, Windows, and other cells [`ProcessProbe::boot_id`] returns `None`
/// and [`ProcessProbe::observe`] returns
/// [`ProcessObservation::Unreadable`]. Those cells therefore quarantine every
/// record and never signal; the daemon reports them as unverified rather than
/// guessing.
#[derive(Debug, Clone, Default)]
pub struct SystemProbe {
    boot_id: Option<BootId>,
}

impl SystemProbe {
    /// Read the current boot id once and cache it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            boot_id: BootId::read_system(),
        }
    }
}

impl ProcessProbe for SystemProbe {
    fn boot_id(&self) -> Option<BootId> {
        self.boot_id.clone()
    }

    fn observe(&self, pid: Pid) -> ProcessObservation {
        observe(pid)
    }
}

/// Observe one process's exact identity facts without signalling it.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn observe(pid: Pid) -> ProcessObservation {
    let stat_path = format!("{PROC_ROOT}/{}/stat", pid.get());
    let stat = match std::fs::read_to_string(stat_path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ProcessObservation::Absent;
        }
        Err(_) => return ProcessObservation::Unreadable,
    };
    let Some((start_time, process_group)) = parse_proc_stat(&stat) else {
        return ProcessObservation::Unreadable;
    };
    let status_path = format!("{PROC_ROOT}/{}/status", pid.get());
    let status = match std::fs::read_to_string(status_path) {
        Ok(status) => status,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ProcessObservation::Absent;
        }
        Err(_) => return ProcessObservation::Unreadable,
    };
    let Some(uid) = parse_proc_uid(&status) else {
        return ProcessObservation::Unreadable;
    };
    ProcessObservation::Present {
        start_time,
        process_group,
        uid,
    }
}

/// Observe is Linux-only; other cells have no `/proc` probe in this slice.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn observe(_pid: Pid) -> ProcessObservation {
    ProcessObservation::Unreadable
}

/// Parse the real uid line from `/proc/<pid>/status`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn parse_proc_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Parse `/proc/<pid>/stat` for the process group and start time.
///
/// The command name is enclosed in parentheses and may itself contain spaces,
/// parentheses, or newlines; the final `)` separates it from the numeric
/// fields. Field 5 is the process group and field 22 is the start time.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn parse_proc_stat(stat: &str) -> Option<(ProcessStartTime, ProcessGroupId)> {
    let comm_end = stat.rfind(')')?;
    let rest = stat.get(comm_end + 1..)?.trim_start();
    let mut fields = rest.split_ascii_whitespace();
    let _state = fields.next()?;
    let _parent = fields.next()?;
    let process_group = ProcessGroupId::new(fields.next()?.parse().ok()?)?;
    let start_time = fields.nth(16)?.parse().ok()?;
    Some((ProcessStartTime::new(start_time), process_group))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT_A: &str = "11111111-1111-4111-8111-111111111111";
    const BOOT_B: &str = "22222222-2222-4222-8222-222222222222";

    #[test]
    fn boot_id_round_trips_and_rejects_non_canonical_input() {
        let boot = BootId::parse(BOOT_A).expect("canonical boot id");
        assert_eq!(boot.as_str(), BOOT_A);
        assert_eq!(boot.to_string(), BOOT_A);
        assert_ne!(boot, BootId::parse(BOOT_B).expect("canonical boot id"));

        for invalid in [
            "",
            "1111111-1111-4111-8111-111111111111",
            "11111111-1111-4111-8111-11111111111",
            "11111111-1111-4111-8111-1111111111111",
            "11111111-1111-4111-8111-11111111111g",
            "111111111111411181111111111111111111",
            "AAAAAAAA-1111-4111-8111-111111111111",
            "11111111_1111-4111-8111-111111111111",
        ] {
            assert_eq!(BootId::parse(invalid), Err(BootIdError), "{invalid}");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn proc_stat_parser_extracts_the_group_and_start_time() {
        let stat = "4321 (bittyd (worker) ) S 1 4321 4321 0 -1 4194560 100 0 0 0 1 2 3 4 20 0 1 0 987654 1234 56 0 0";
        let (start_time, process_group) = parse_proc_stat(stat).expect("valid stat line");
        assert_eq!(process_group.get(), 4321);
        assert_eq!(start_time.get(), 987_654);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn proc_stat_parser_uses_the_last_command_paren() {
        let stat = "9 (odd ) comm (here)) S 1 77 9 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 42 0 0";
        let (start_time, process_group) = parse_proc_stat(stat).expect("valid stat line");
        assert_eq!(process_group.get(), 77);
        assert_eq!(start_time.get(), 42);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn proc_stat_parser_rejects_truncated_or_zero_fields() {
        assert_eq!(parse_proc_stat(""), None);
        assert_eq!(parse_proc_stat("4321 (bittyd) S 1"), None);
        assert_eq!(
            parse_proc_stat("4321 (bittyd) S 1 0 4321 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 42"),
            None,
            "a zero process group is not a process group"
        );
        assert_eq!(
            parse_proc_stat(
                "4321 (bittyd) S 1 4321 4321 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 notanumber"
            ),
            None
        );
    }

    #[test]
    fn identity_types_reject_zero_pids_and_groups() {
        assert_eq!(Pid::new(0), None);
        assert_eq!(ProcessGroupId::new(0), None);
        let pid = Pid::new(1234).expect("positive");
        let group = ProcessGroupId::new(1234).expect("positive");
        let identity = ProcessIdentity::new(pid, ProcessStartTime::new(99), group, 1000);
        assert_eq!(identity.pid(), pid);
        assert_eq!(identity.start_time().get(), 99);
        assert_eq!(identity.process_group(), group);
        assert_eq!(identity.uid(), 1000);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn system_probe_reads_the_boot_id_and_current_process() {
        let boot = BootId::read_system().expect("Linux exposes a boot id");
        assert_eq!(boot.as_str().len(), BOOT_ID_CHARS);

        let probe = SystemProbe::new();
        assert_eq!(probe.boot_id(), Some(boot));
        let pid = Pid::new(std::process::id()).expect("current pid is positive");
        match probe.observe(pid) {
            ProcessObservation::Present {
                start_time,
                process_group,
                uid,
            } => {
                assert!(start_time.get() > 0);
                assert!(process_group.get() > 0);
                assert_eq!(uid, nix::unistd::geteuid().as_raw());
            }
            other => panic!("current process must be observable, got {other:?}"),
        }

        let absent_pid = Pid::new(u32::MAX).expect("positive");
        assert_eq!(probe.observe(absent_pid), ProcessObservation::Absent);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn proc_status_parser_reads_the_real_uid() {
        let status = "Name:\tbittyd\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(parse_proc_uid(status), Some(1000));
        assert_eq!(parse_proc_uid("Name:\tbittyd\n"), None);
        assert_eq!(parse_proc_uid("Uid:\tnotanumber\n"), None);
    }
}
