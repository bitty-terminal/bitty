//! Windows containment: one kill-on-close job object per session.
//!
//! The child is created suspended, assigned to a fresh job, and only then
//! resumed, so no descendant can be born outside the job. The job carries
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and does not grant breakaway, so a
//! member cannot leave it and closing the daemon's only handle kills the whole
//! tree. A stop terminates the job and claims `swept` only after the kernel
//! reports `ActiveProcesses` zero; a job that cannot be created is a typed
//! [`ContainmentRefusal::UnsupportedPlatform`].
//!
//! Every Win32 call stays behind the reviewed `bitty-winjob` adapter; this
//! module composes its safe API and adds no FFI.
//!
//! [`spawn_conpty`] is the worker spawn entry point: [`ConPtyMaster::spawn_in_job`]
//! attaches both `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST` with `bInheritHandles = FALSE`, so the
//! child joins the job at creation time and no daemon handle crosses into the
//! child. [`spawn_with`] provides the non-PTY `Command` path for containment
//! tests.

use std::io;
use std::os::windows::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Child, Command, ExitStatus};

use bitty_winjob::{
    CREATE_SUSPENDED_FLAG, ChildSpec, ConPtyChild, ConPtyMaster, JobMember, JobObject,
    process_is_running, resume_suspended_process, terminate_process,
};

use crate::orphan::identity::ProcessIdentity;
use crate::platform::PlatformCell;
use crate::registry::CleanupClass;

use super::{
    CleanupOutcome, ContainmentError, ContainmentKind, ContainmentLimits, ContainmentRecord,
    ContainmentRefusal, ContainmentReport, SessionMode, classify_spawn_error, wait_bounded,
};

/// Exit code the daemon uses when it terminates a job tree. Static and
/// non-zero so a stopped worker is distinguishable from a clean exit.
const JOB_TERMINATION_EXIT_CODE: u32 = 1;

/// Probe the Windows job-object plan by creating and dropping an empty job.
///
/// A job object needs no privilege, so a creation failure means the cell has
/// no usable mechanism; the report then refuses instead of approximating with
/// a bare pid or a process-group convention.
pub fn probe() -> ContainmentReport {
    let cell = PlatformCell::current();
    match JobObject::new() {
        Ok(job) => {
            drop(job);
            ContainmentReport {
                cell,
                kind: ContainmentKind::WindowsJob,
                refusal: None,
                fallback_reason: None,
                cgroup_root: None,
            }
        }
        Err(_) => ContainmentReport {
            cell,
            kind: ContainmentKind::Refused,
            refusal: Some(ContainmentRefusal::UnsupportedPlatform { cell }),
            fallback_reason: None,
            cgroup_root: None,
        },
    }
}

/// Spawn a child into a fresh kill-on-close job.
///
/// The process is created with [`CREATE_SUSPENDED_FLAG`], assigned to the job
/// while it cannot run, and resumed only after the assignment succeeded. A
/// failure before the resume never lets the child execute.
pub fn spawn_with(
    report: &ContainmentReport,
    _name: &str,
    command: &mut Command,
    limits: ContainmentLimits,
    _mode: SessionMode,
) -> Result<ContainedChild, ContainmentError> {
    if report.kind() != ContainmentKind::WindowsJob {
        return Err(ContainmentError::Refused(
            report
                .refusal()
                .cloned()
                .unwrap_or(ContainmentRefusal::UnsupportedPlatform {
                    cell: report.cell(),
                }),
        ));
    }
    let job = JobObject::new().map_err(ContainmentError::Spawn)?;
    command.creation_flags(CREATE_SUSPENDED_FLAG);
    let mut child = command.spawn().map_err(classify_spawn_error)?;
    let pid = child.id();
    let member = match job.assign_pid(pid) {
        Ok(member) => member,
        Err(_) => {
            // The child was created suspended and never ran, so it cannot have
            // descendants. Its own handle pins the pid; kill and reap it.
            let cleanup = abandon_suspended_child(&mut child, pid);
            return Err(ContainmentError::Uncontained(cleanup));
        }
    };
    if resume_suspended_process(pid).is_err() {
        // The process is in the job but never ran; terminate the job tree and
        // report what the job could verify.
        let mut contained = ContainedChild {
            child: ChildProcess::Command(child),
            job,
            member,
            pid,
        };
        let cleanup = contained.terminate(limits).class();
        return Err(ContainmentError::Uncontained(cleanup));
    }
    Ok(ContainedChild {
        child: ChildProcess::Command(child),
        job,
        member,
        pid,
    })
}

/// Spawn one child attached to a fresh Windows pseudoconsole inside a new
/// kill-on-close job object.
///
/// [`ConPtyMaster::spawn_in_job`] attaches both
/// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and `PROC_THREAD_ATTRIBUTE_JOB_LIST`
/// with `bInheritHandles = FALSE`, so the child is assigned to the job at
/// creation time (no instruction executes outside the job) and no daemon
/// handle crosses into the child.
///
/// # Errors
///
/// Returns [`ContainmentError::Refused`] when `report` did not select the
/// Windows job plan, [`ContainmentError::Exec`] when `CreateProcessW`
/// refused the executable, [`ContainmentError::Spawn`] when the job,
/// pseudoconsole, or process could not be created, or
/// [`ContainmentError::IdentityUnavailable`] with the observed cleanup class
/// when the leader's member handle could not be opened after spawn.
pub fn spawn_conpty(
    report: &ContainmentReport,
    spec: &ChildSpec,
    limits: ContainmentLimits,
) -> Result<(ConPtyMaster, ContainedChild), ContainmentError> {
    if report.kind() != ContainmentKind::WindowsJob {
        return Err(ContainmentError::Refused(
            report
                .refusal()
                .cloned()
                .unwrap_or(ContainmentRefusal::UnsupportedPlatform {
                    cell: report.cell(),
                }),
        ));
    }
    let job = JobObject::new().map_err(ContainmentError::Spawn)?;
    let (master, child) = ConPtyMaster::spawn_in_job(&job, spec).map_err(classify_spawn_error)?;
    let pid = child.pid();
    let member = match child.open_member() {
        Ok(member) => member,
        Err(_) => {
            let _ = job.terminate(JOB_TERMINATION_EXIT_CODE);
            let _ = child.kill(JOB_TERMINATION_EXIT_CODE);
            let reaped = child.wait().is_ok();
            let empty = wait_bounded(limits.kill_wait(), limits, || {
                job.active_processes().is_ok_and(|active| active == 0)
            });
            // Close the pseudoconsole only after the child is reaped.
            drop(master);
            let cleanup = if reaped && empty {
                CleanupClass::Swept
            } else {
                CleanupClass::Unverified
            };
            return Err(ContainmentError::IdentityUnavailable(cleanup));
        }
    };
    Ok((
        master,
        ContainedChild {
            child: ChildProcess::ConPty(child),
            job,
            member,
            pid,
        },
    ))
}

/// Kill a suspended child that never joined a job.
///
/// A suspended process has run no instruction and owns no descendant, so
/// terminating it and observing it gone is a complete cleanup of what exists.
fn abandon_suspended_child(child: &mut Child, pid: u32) -> CleanupClass {
    let _ = terminate_process(pid, JOB_TERMINATION_EXIT_CODE);
    let reaped = child.wait().is_ok();
    if reaped && process_is_running(pid).is_ok_and(|running| !running) {
        CleanupClass::Swept
    } else {
        CleanupClass::Unverified
    }
}

#[derive(Debug)]
enum ChildProcess {
    Command(Child),
    ConPty(ConPtyChild),
}

/// One child contained by a job object for its whole lifetime.
///
/// Dropping the value closes the job's only handle; kill-on-close then ends
/// every remaining member, so an abnormal teardown cannot leak the tree.
#[derive(Debug)]
pub struct ContainedChild {
    child: ChildProcess,
    job: JobObject,
    /// Open handle to the leader, used to observe its exit without reaping.
    member: JobMember,
    pid: u32,
}

impl ContainedChild {
    /// The direct child's process id.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// The journal identity this plan can re-verify after a restart.
    ///
    /// A Windows job handle is process-local and `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
    /// ends the tree when the daemon exits, so no journal identity is committed.
    #[must_use]
    pub const fn durable_identity(&self) -> Option<ProcessIdentity> {
        None
    }

    /// The recorded containment identity, suitable for the orphan journal.
    ///
    /// The job handle itself is process-local and cannot be recorded; the
    /// record names the mechanism, and kill-on-close is what makes a daemon
    /// crash safe for the whole tree.
    #[must_use]
    pub fn containment(&self) -> ContainmentRecord {
        ContainmentRecord::windows_job()
    }

    /// Wait for the child and return its exit status.
    ///
    /// # Errors
    ///
    /// Propagates a wait failure.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        match &mut self.child {
            ChildProcess::Command(child) => child.wait(),
            ChildProcess::ConPty(child) => child.wait().map(ExitStatus::from_raw),
        }
    }

    /// Take the child's standard output, when it was piped.
    #[must_use]
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        match &mut self.child {
            ChildProcess::Command(child) => child.stdout.take(),
            ChildProcess::ConPty(_) => None,
        }
    }

    /// Whether the direct child has been reaped.
    ///
    /// # Errors
    ///
    /// Propagates a `try_wait` failure other than an already-reaped child.
    pub fn has_exited(&mut self) -> io::Result<bool> {
        self.try_wait().map(|status| status.is_some())
    }

    /// Non-blocking wait: the exit status once the direct child has exited.
    ///
    /// # Errors
    ///
    /// Propagates a wait failure.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match &mut self.child {
            ChildProcess::Command(child) => child.try_wait(),
            ChildProcess::ConPty(child) => {
                child.try_wait().map(|code| code.map(ExitStatus::from_raw))
            }
        }
    }

    /// Whether the direct child has exited, without reaping it.
    ///
    /// The open [`JobMember`] handle keeps the process object alive after
    /// exit, so its pid cannot be recycled while the job still owns it.
    #[must_use]
    pub fn leader_exited(&self) -> bool {
        self.member.exit_code().is_ok_and(|code| code.is_some())
    }

    /// Nothing to drain: the job owns every member and the kernel reaps them.
    pub const fn drain_recorded(&self) {}

    /// Terminate the job tree and wait up to [`ContainmentLimits::kill_wait`]
    /// for `ActiveProcesses` to reach zero and the leader handle to signal.
    ///
    /// Windows has no `SIGHUP` or `SIGTERM`: supervision calls this when the
    /// graceful stop window expires, on an immediate `session.kill`, or after
    /// observing a natural leader exit so any background descendant in the job
    /// is ended before the sweep check runs.
    pub fn kill_tree(&self, limits: ContainmentLimits) {
        let _ = self.job.terminate(JOB_TERMINATION_EXIT_CODE);
        let _ = wait_bounded(limits.kill_wait(), limits, || {
            self.leader_exited() && self.job.active_processes().is_ok_and(|active| active == 0)
        });
    }

    /// Settle the job after the direct child was reaped.
    ///
    /// Returns `Some(swept)` once the job reports no active process, and
    /// `None` while any member is still alive (or the query fails). Never
    /// waits.
    #[must_use]
    pub fn sweep_if_empty(&self) -> Option<CleanupOutcome> {
        match self.job.active_processes() {
            Ok(0) => Some(CleanupOutcome {
                class: CleanupClass::Swept,
                signalled: 0,
            }),
            Ok(_) | Err(_) => None,
        }
    }

    /// Terminate the job tree with a bounded verification window.
    ///
    /// Windows has no graceful signal for an arbitrary process, so the stop
    /// terminates the job immediately. `swept` requires the job's
    /// `ActiveProcesses` to reach zero **and** the leader's exit to be
    /// observed on its own handle before anything is reaped; anything else is
    /// `unverified`.
    pub fn terminate(&mut self, limits: ContainmentLimits) -> CleanupOutcome {
        let signalled = self
            .job
            .active_processes()
            .map_or(0, |active| usize::try_from(active).unwrap_or(usize::MAX));
        let _ = self.job.terminate(JOB_TERMINATION_EXIT_CODE);
        // `ActiveProcesses` and the leader's process object do not settle in
        // one order: the accounting can read zero while the process handle is
        // not signalled yet. A verdict taken from one non-blocking `try_wait`
        // there reads `Ok(None)` and turns a real sweep into `unverified`
        // (bitty#2982, Windows runner: `Unverified` with `signalled=1` after
        // 22 ms). Wait for both signals, and only then reap: the open
        // `JobMember` handle keeps the process object, and its pid, alive
        // across the wait.
        let drained = wait_bounded(limits.kill_wait(), limits, || {
            self.leader_exited() && self.job.active_processes().is_ok_and(|active| active == 0)
        });
        if !drained {
            // A member created in the termination window may still be alive;
            // one more terminate is safe, and the final queries below stay
            // fail-closed.
            let _ = self.job.terminate(JOB_TERMINATION_EXIT_CODE);
        }
        let leader_exited = self.leader_exited();
        // Reap only what the non-consuming handle already reported gone, so
        // the verdict never rests on a `try_wait` that merely lost the race.
        // `wait` cannot block here: the leader's process object is signalled.
        let reaped = if leader_exited {
            self.wait().is_ok()
        } else {
            matches!(self.try_wait(), Ok(Some(_)))
        };
        let empty = self.job.active_processes().is_ok_and(|active| active == 0);
        CleanupOutcome {
            class: if empty && reaped && leader_exited {
                CleanupClass::Swept
            } else {
                CleanupClass::Unverified
            },
            signalled,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;
    use std::time::Duration;

    use super::*;

    fn report() -> ContainmentReport {
        ContainmentReport::windows_job(PlatformCell::current())
    }

    fn limits() -> ContainmentLimits {
        ContainmentLimits::new(Duration::from_millis(100), Duration::from_secs(5))
    }

    #[test]
    fn the_live_probe_selects_the_job_plan_or_a_typed_refusal() {
        let report = probe();
        assert_eq!(report.cell(), PlatformCell::current());
        match report.kind() {
            ContainmentKind::WindowsJob => {
                assert_eq!(report.cleanup_claim(), "swept");
                assert!(report.is_enforced());
            }
            ContainmentKind::Refused => {
                assert!(matches!(
                    report.refusal(),
                    Some(ContainmentRefusal::UnsupportedPlatform { cell }) if *cell == report.cell()
                ));
            }
            other => panic!("Windows selected a foreign plan: {other}"),
        }
    }

    #[test]
    fn a_foreign_plan_is_refused_before_a_child_exists() {
        let foreign = ContainmentReport::process_group(PlatformCell::WindowsX86_64);
        let mut command = Command::new("cmd.exe");
        command.arg("/d").arg("/c").arg("exit 0");
        let error = spawn_with(
            &foreign,
            "bittyd-test-foreign",
            &mut command,
            limits(),
            SessionMode::ProcessGroup,
        )
        .expect_err("a process-group plan is not the Windows plan");
        assert!(matches!(error, ContainmentError::Refused(_)));
    }

    #[test]
    fn a_job_child_is_assigned_before_it_runs_and_the_job_sweeps_it() {
        let report = report();
        let mut command = Command::new("cmd.exe");
        command
            .args(["/d", "/c", "ping", "-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_with(
            &report,
            "bittyd-test-job",
            &mut command,
            limits(),
            SessionMode::ProcessGroup,
        )
        .expect("a contained spawn");
        assert_eq!(child.containment().mechanism(), "windows_job");
        assert!(!child.leader_exited(), "the resumed child must be running");
        let outcome = child.terminate(limits());
        assert_eq!(
            outcome.class(),
            CleanupClass::Swept,
            "{report} signalled={}",
            outcome.signalled()
        );
        assert!(outcome.signalled() >= 1, "the job held at least the leader");
        assert!(
            child.sweep_if_empty().is_some(),
            "a terminated job must verify empty"
        );
    }

    #[test]
    fn a_natural_exit_of_an_empty_job_reports_a_sweep() {
        let report = report();
        let mut command = Command::new("cmd.exe");
        command
            .args(["/d", "/c", "exit", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_with(
            &report,
            "bittyd-test-job-exit",
            &mut command,
            limits(),
            SessionMode::ProcessGroup,
        )
        .expect("a contained spawn");
        let _ = child.wait().expect("reap the natural exit");
        let outcome = wait_bounded(limits().kill_wait(), limits(), || {
            child.sweep_if_empty().is_some()
        });
        assert!(outcome, "the empty job must verify gone: {report}");
        assert_eq!(
            child.sweep_if_empty().map(CleanupOutcome::class),
            Some(CleanupClass::Swept)
        );
    }

    #[test]
    fn a_conpty_child_joins_the_job_at_creation_and_sweeps_on_terminate() {
        use std::ffi::{OsStr, OsString};

        let report = report();
        let args = [
            OsString::from("/d"),
            OsString::from("/c"),
            OsString::from("ping"),
            OsString::from("-n"),
            OsString::from("30"),
            OsString::from("127.0.0.1"),
        ];
        let env = [(OsString::from("TERM"), OsString::from("xterm-256color"))];
        let spec = ChildSpec {
            program: OsStr::new("cmd.exe"),
            args: &args,
            env: &env,
            cwd: None,
            cols: 80,
            rows: 24,
        };
        let (master, mut child) =
            spawn_conpty(&report, &spec, limits()).expect("a contained ConPTY spawn");
        assert_eq!(child.containment().mechanism(), "windows_job");
        assert_eq!(child.durable_identity(), None);
        assert!(
            !child.leader_exited(),
            "the ConPTY child must be running inside the job"
        );
        master.resize(120, 40).expect("resize pseudoconsole");
        let outcome = child.terminate(limits());
        assert_eq!(
            outcome.class(),
            CleanupClass::Swept,
            "{report} signalled={}",
            outcome.signalled()
        );
        assert!(outcome.signalled() >= 1);
        assert!(child.sweep_if_empty().is_some());
        // Close the pseudoconsole after the child has been reaped.
        drop(master);
    }
}
