//! Safe Job Object API over [`crate::ffi`].

use std::fmt;
use std::io;
use std::os::windows::io::{AsHandle as _, BorrowedHandle, OwnedHandle};

use crate::ffi;

/// Process creation flag that starts the primary thread suspended
/// (`CREATE_SUSPENDED`). Pass it to
/// `std::os::windows::process::CommandExt::creation_flags`, assign the
/// child with [`JobObject::assign_pid`], then call
/// [`resume_suspended_process`] — on every path, including assignment
/// failure, or the child never runs.
pub const CREATE_SUSPENDED_FLAG: u32 = windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

/// An anonymous Job Object.
///
/// Created kill-on-close ([`JobObject::new`]) by default: the handle is
/// non-inheritable and this value holds the job's only handle, so dropping
/// it closes that handle and the kernel terminates every process still in
/// the job. [`JobObject::new_detached`] skips the limit instead, so its
/// members outlive the last handle close (DEC-0102: detached-lifetime and
/// service jobs on Windows, matching Unix process groups). Either flavour
/// still ends every member on [`JobObject::terminate`].
///
/// `JobObject` is `Send + Sync` (it holds only an [`OwnedHandle`] plus a
/// flag): every operation is a single kernel call on the handle, which the
/// kernel serializes, and no method mutates Rust-side state.
pub struct JobObject {
    handle: OwnedHandle,
    kill_on_close: bool,
}

impl fmt::Debug for JobObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobObject").finish_non_exhaustive()
    }
}

impl JobObject {
    /// Creates an empty kill-on-close job.
    ///
    /// # Errors
    ///
    /// Returns the system error when the job cannot be created or its
    /// kill-on-close limit cannot be set.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            handle: ffi::create_job(true)?,
            kill_on_close: true,
        })
    }

    /// Creates an empty job whose members outlive the last handle close.
    ///
    /// DEC-0102 (#1580): detached-lifetime and service jobs on Windows must
    /// outlive the bitty process like Unix process groups do, so they run
    /// in a job without `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Dropping this
    /// value leaves its members running as plain OS orphans; end them with
    /// [`JobObject::terminate`] instead.
    ///
    /// # Errors
    ///
    /// Returns the system error when the job cannot be created.
    pub fn new_detached() -> io::Result<Self> {
        Ok(Self {
            handle: ffi::create_job(false)?,
            kill_on_close: false,
        })
    }

    /// Whether dropping this job kills its remaining members.
    #[must_use]
    pub fn kills_on_close(&self) -> bool {
        self.kill_on_close
    }

    /// Adds process `pid` to this job and returns a handle that observes its
    /// exit.
    ///
    /// Processes `pid` creates after this call join the job too; processes
    /// it created before do not. Assign a [`CREATE_SUSPENDED_FLAG`] child
    /// before resuming it to close that window.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] for pid 0, and the system
    /// error when the process cannot be opened or assigned (for example
    /// access denied, or a job that forbids nesting).
    pub fn assign_pid(&self, pid: u32) -> io::Result<JobMember> {
        require_pid(pid)?;
        let process = ffi::open_member(pid)?;
        ffi::assign(self.handle.as_handle(), process.as_handle())?;
        Ok(JobMember { pid, process })
    }

    /// Terminates every process currently in the job with `exit_code`.
    ///
    /// # Errors
    ///
    /// Returns the system error when the kernel refuses.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        ffi::terminate_job(self.handle.as_handle(), exit_code)
    }

    /// Number of processes alive in the job right now.
    ///
    /// # Errors
    ///
    /// Returns the system error when the job cannot be queried.
    pub fn active_processes(&self) -> io::Result<u32> {
        ffi::active_processes(self.handle.as_handle())
    }

    /// Borrows the job handle for at-creation registration (the ConPTY
    /// `PROC_THREAD_ATTRIBUTE_JOB_LIST` path). The borrow keeps the job
    /// alive across the spawn call.
    pub(crate) fn as_handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
}

/// An open handle to one job member, used to observe its exit.
///
/// Observation never reaps anything: while this handle is open the process
/// object stays alive after exit, so its pid cannot be reused by another
/// process.
pub struct JobMember {
    pid: u32,
    process: OwnedHandle,
}

impl fmt::Debug for JobMember {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobMember")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl JobMember {
    /// The member's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Wraps an already-open member handle. The pid must be pinned by a
    /// live handle the caller keeps (the ConPTY session's process handle),
    /// so it cannot have been recycled.
    pub(crate) fn from_spawned(pid: u32, process: OwnedHandle) -> Self {
        Self { pid, process }
    }

    /// The member's exit code once it has exited, `None` while it runs.
    ///
    /// # Errors
    ///
    /// Returns the system error when the process cannot be waited on or
    /// queried.
    pub fn exit_code(&self) -> io::Result<Option<u32>> {
        ffi::exit_code(self.process.as_handle())
    }
}

/// Resumes every thread of process `pid`, which was created with
/// [`CREATE_SUSPENDED_FLAG`].
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] for pid 0,
/// [`io::ErrorKind::NotFound`] when the process owns no thread (it is gone),
/// and the system error when a thread cannot be enumerated or resumed.
pub fn resume_suspended_process(pid: u32) -> io::Result<()> {
    require_pid(pid)?;
    if ffi::resume_threads(pid)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the suspended process owns no thread to resume",
        ));
    }
    Ok(())
}

/// Whether process `pid` is still running; a pid with no live process is
/// `false`.
///
/// Test and diagnostic probe only; no product path relies on it. A bare
/// pid is not pinned: once its process exits and the last handle closes,
/// Windows may hand the pid to an unrelated process, and this probe would
/// then report that process. Only call it for a pid a live handle still
/// pins (a [`JobMember`], or an unreaped `std::process::Child` the caller
/// or its parent keeps), or accept the recycling caveat.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] for pid 0 and the system error
/// when the process exists but cannot be queried (for example access
/// denied).
pub fn process_is_running(pid: u32) -> io::Result<bool> {
    require_pid(pid)?;
    ffi::process_is_running(pid)
}

/// Terminates the single process `pid` with `exit_code`: last-resort
/// cleanup for a child that could not join a job.
///
/// # Precondition
///
/// `pid` must be pinned by a live handle the caller (or a parent holding
/// the `std::process::Child`) keeps open for the whole call, so the pid
/// cannot have been recycled to an unrelated process. Never pass a pid
/// learned from output, a file, or a snapshot.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] for pid 0 and the system error
/// when the process cannot be opened or terminated.
pub fn terminate_process(pid: u32, exit_code: u32) -> io::Result<()> {
    require_pid(pid)?;
    ffi::terminate_process(pid, exit_code)
}

fn require_pid(pid: u32) -> io::Result<()> {
    if pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pid 0 is the system idle process, never a job member",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::windows::process::CommandExt as _;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    /// Upper bound for any single wait in these tests.
    const WAIT_BOUND: Duration = Duration::from_secs(20);

    /// Pause between polls.
    const POLL: Duration = Duration::from_millis(10);

    /// Exit code the tests terminate jobs with.
    const KILL_CODE: u32 = 77;

    /// `cmd /c ping` keeps a two-process tree (cmd plus ping) alive for
    /// about half a minute with inbox programs only.
    fn spawn_suspended_tree() -> Child {
        Command::new("cmd.exe")
            .args(["/d", "/c", "ping", "-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_SUSPENDED_FLAG)
            .spawn()
            .expect("spawn suspended cmd")
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT_BOUND;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn pid_zero_is_refused_everywhere() {
        let job = JobObject::new().expect("job");
        assert_eq!(
            job.assign_pid(0).expect_err("pid 0").kind(),
            io::ErrorKind::InvalidInput
        );
        for result in [
            resume_suspended_process(0),
            terminate_process(0, KILL_CODE),
            process_is_running(0).map(drop),
        ] {
            assert_eq!(
                result.expect_err("pid 0").kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn a_new_job_is_empty() {
        let job = JobObject::new().expect("job");
        assert_eq!(job.active_processes().expect("query"), 0);
    }

    #[test]
    fn a_suspended_child_joins_runs_and_dies_with_the_job() {
        let job = JobObject::new().expect("job");
        let mut child = spawn_suspended_tree();
        let pid = child.id();
        let member = job.assign_pid(pid).expect("assign before resume");
        assert_eq!(member.pid(), pid);
        resume_suspended_process(pid).expect("resume");
        assert!(process_is_running(pid).expect("probe"));
        assert_eq!(member.exit_code().expect("observe"), None);
        // cmd starts ping inside the job: the tree grows past the leader.
        wait_until("ping to join the job", || {
            job.active_processes().is_ok_and(|active| active >= 2)
        });
        job.terminate(KILL_CODE).expect("terminate the job");
        wait_until("the leader exit to be observed", || {
            member.exit_code().is_ok_and(|code| code.is_some())
        });
        assert_eq!(member.exit_code().expect("observe"), Some(KILL_CODE));
        // Observation reaped nothing: it is repeatable.
        assert_eq!(member.exit_code().expect("observe"), Some(KILL_CODE));
        wait_until("the job to drain", || {
            job.active_processes().is_ok_and(|active| active == 0)
        });
        assert!(!process_is_running(pid).expect("probe"));
        let status = child.wait().expect("reap");
        assert_eq!(
            status.code(),
            Some(i32::try_from(KILL_CODE).expect("small"))
        );
    }

    #[test]
    fn lifetimes_report_their_close_behavior() {
        assert!(JobObject::new().expect("job").kills_on_close());
        assert!(!JobObject::new_detached().expect("job").kills_on_close());
    }

    /// `ping -n 30` sleeps about half a minute as a single process (no
    /// grandchild to leak), with inbox programs only.
    fn spawn_suspended_sleeper() -> Child {
        Command::new("ping.exe")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_SUSPENDED_FLAG)
            .spawn()
            .expect("spawn suspended ping")
    }

    #[test]
    fn a_detached_job_survives_its_handle_close() {
        let job = JobObject::new_detached().expect("job");
        let mut child = spawn_suspended_sleeper();
        let pid = child.id();
        let member = job.assign_pid(pid).expect("assign before resume");
        resume_suspended_process(pid).expect("resume");
        wait_until("the sleeper to join the job", || {
            job.active_processes().is_ok_and(|active| active == 1)
        });
        drop(job);
        // Kill-on-close would act on the close itself; outlive a window
        // past it, then assert the member is still there. The open
        // `JobMember` pins the pid, so the probes below are exact.
        std::thread::sleep(Duration::from_secs(1));
        assert!(process_is_running(pid).expect("probe"));
        assert_eq!(member.exit_code().expect("observe"), None);
        // The job handle is gone by design: cleanup is per-pid, and the
        // member handle pins `pid` (the `terminate_process` precondition).
        terminate_process(pid, KILL_CODE).expect("terminate the orphan");
        wait_until("the orphan to exit", || {
            member.exit_code().is_ok_and(|code| code.is_some())
        });
        let status = child.wait().expect("reap");
        assert_eq!(
            status.code(),
            Some(i32::try_from(KILL_CODE).expect("small"))
        );
    }

    #[test]
    fn terminating_a_detached_job_still_kills_its_tree() {
        let job = JobObject::new_detached().expect("job");
        let mut child = spawn_suspended_tree();
        let pid = child.id();
        let member = job.assign_pid(pid).expect("assign before resume");
        resume_suspended_process(pid).expect("resume");
        // cmd starts ping inside the job: the tree grows past the leader.
        wait_until("ping to join the job", || {
            job.active_processes().is_ok_and(|active| active >= 2)
        });
        job.terminate(KILL_CODE).expect("terminate the job");
        wait_until("the job to drain", || {
            job.active_processes().is_ok_and(|active| active == 0)
        });
        wait_until("the leader exit to be observed", || {
            member.exit_code().is_ok_and(|code| code.is_some())
        });
        let status = child.wait().expect("reap");
        assert_eq!(
            status.code(),
            Some(i32::try_from(KILL_CODE).expect("small"))
        );
    }

    #[test]
    fn dropping_the_job_kills_its_members() {
        let job = JobObject::new().expect("job");
        let mut child = spawn_suspended_tree();
        let pid = child.id();
        let member = job.assign_pid(pid).expect("assign");
        resume_suspended_process(pid).expect("resume");
        drop(job);
        wait_until("kill-on-close to end the leader", || {
            member.exit_code().is_ok_and(|code| code.is_some())
        });
        let _ = child.wait();
    }

    #[test]
    fn resuming_a_gone_process_fails() {
        let mut child = spawn_suspended_tree();
        let pid = child.id();
        terminate_process(pid, KILL_CODE).expect("terminate suspended child");
        let _ = child.wait();
        assert!(resume_suspended_process(pid).is_err());
        assert!(!process_is_running(pid).expect("probe"));
    }
}
