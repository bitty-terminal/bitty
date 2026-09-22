#![forbid(unsafe_code)]
//! Guest lifecycle: libvirt boot, SSH readiness and execution, artifact
//! collection, and teardown (CTX-0510 second slice, research 043).
//!
//! The lifecycle runs only what the host can honestly execute. Every live
//! step is a thin wrapper over a pure renderer (`virsh_*_args`,
//! `ssh_exec_args`, [`artifact_paths`]), so the exact commands, paths, and
//! retry arithmetic are pinned by hermetic unit tests that never spawn a
//! subprocess. Live execution additionally requires `execute`
//! (`smoke --execute` or `BITTY_VM_LIVE`), a prepared base image, and the
//! `virsh`/`ssh` tools; otherwise every stage reports `gated` or `dry-run`
//! with its exact reason and touches nothing.
//!
//! The in-guest workload is deliberately small: an SSH readiness probe that
//! also proves remote execution (`echo`), plus host-side artifact collection
//! (`virsh dumpxml`, `virsh screenshot`). Full in-guest suite execution stays
//! recorded plan (`policy::GUEST_SUITE_CANDIDATES`); this slice proves the
//! boot/SSH/artifact/teardown path those suites will ride, without inventing
//! suite contracts.
//!
//! Cleanup is enforced, not best-effort: [`run_guest_lifecycle`] arms a
//! [`RunGuard`] over the per-run directory, the `teardown` stage explicitly
//! removes that directory and verifies it is gone, and the guard removes it
//! on drop if anything escaped the explicit path (the guard disarms only
//! after verified removal).

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::capability::Capabilities;
use crate::policy::RunPlan;
use crate::smoke::{StageReport, StageState};

/// File name (under the run directory) holding the staged libvirt domain XML.
/// `virsh define` consumes this real path, never a rendered string.
pub const DOMAIN_XML_FILE: &str = "domain.xml";
/// File name (under the run directory) for the per-run SSH known-hosts file.
/// Ephemeral guests get fresh host keys per boot, so the run keeps its own
/// file instead of touching the user's `~/.ssh/known_hosts`.
pub const KNOWN_HOSTS_FILE: &str = "known_hosts";
/// Directory (under the run directory) owning collected artifacts.
pub const ARTIFACTS_DIR: &str = "artifacts";
/// Directory (under the artifacts directory) for guest logs and dumpxml.
pub const LOGS_DIR: &str = "logs";
/// Directory (under the artifacts directory) for screenshots.
pub const SCREENSHOTS_DIR: &str = "screenshots";
/// File name (under the logs directory) for the `virsh dumpxml` snapshot.
pub const DUMPXML_FILE: &str = "domain-dumpxml.xml";
/// File name (under the screenshots directory) for the `virsh screenshot`.
/// `virsh screenshot` writes PPM unless the hypervisor says otherwise.
pub const SCREENSHOT_FILE: &str = "screenshot.ppm";

/// Default `ssh -o ConnectTimeout=` value in seconds: fail fast per attempt;
/// the retry loop (not one long attempt) owns the overall deadline.
pub const SSH_CONNECT_TIMEOUT_SECS: u64 = 10;
/// Default deadline for SSH readiness: a cold guest boots, starts sshd, and
/// answers well inside three minutes; past that the run is stuck, not slow.
pub const SSH_READY_TIMEOUT: Duration = Duration::from_secs(180);
/// Default interval between SSH readiness attempts.
pub const SSH_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Remote command proving SSH execution works. It has no side effects and
/// its exact output is asserted by the live path.
pub const SSH_READY_PROBE_COMMAND: &str = "echo bitty-vm-ready";
/// Expected stdout (trimmed) of [`SSH_READY_PROBE_COMMAND`].
pub const SSH_READY_PROBE_EXPECTED: &str = "bitty-vm-ready";

/// Stage name: libvirt domain define + start.
pub const STAGE_BOOT: &str = "boot";
/// Stage name: SSH readiness wait + remote execution probe.
pub const STAGE_SSH: &str = "ssh";
/// Stage name: dumpxml + screenshot collection.
pub const STAGE_ARTIFACTS: &str = "artifacts";
/// Stage name: domain destroy + undefine + run-directory removal.
pub const STAGE_TEARDOWN: &str = "teardown";

/// Resolved live tools for one lifecycle run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestTools {
    /// libvirt `virsh` binary.
    pub virsh: PathBuf,
    /// OpenSSH client binary.
    pub ssh: PathBuf,
}

/// Options for one lifecycle run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestOptions {
    /// Deadline for SSH readiness (first successful probe).
    pub ssh_timeout: Duration,
    /// Interval between SSH readiness attempts.
    pub ssh_poll: Duration,
    /// Per-attempt `ssh -o ConnectTimeout=` in seconds.
    pub connect_timeout_secs: u64,
}

impl Default for GuestOptions {
    fn default() -> Self {
        Self {
            ssh_timeout: SSH_READY_TIMEOUT,
            ssh_poll: SSH_POLL_INTERVAL,
            connect_timeout_secs: SSH_CONNECT_TIMEOUT_SECS,
        }
    }
}

/// Every filesystem path the lifecycle derives. All of them stay under the
/// plan's run directory, so removing that directory removes the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPaths {
    /// `<run-dir>/domain.xml`: the staged domain definition.
    pub domain_xml: PathBuf,
    /// `<run-dir>/known_hosts`: per-run SSH known-hosts file.
    pub known_hosts: PathBuf,
    /// `<run-dir>/artifacts`: artifact root.
    pub dir: PathBuf,
    /// `<run-dir>/artifacts/logs`: dumpxml and logs.
    pub logs_dir: PathBuf,
    /// `<run-dir>/artifacts/screenshots`: screenshots.
    pub screenshots_dir: PathBuf,
    /// `<run-dir>/artifacts/logs/domain-dumpxml.xml`.
    pub dumpxml: PathBuf,
    /// `<run-dir>/artifacts/screenshots/screenshot.ppm`.
    pub screenshot: PathBuf,
}

/// Derive every lifecycle path for one plan. Pure: creates nothing.
pub fn artifact_paths(plan: &RunPlan) -> ArtifactPaths {
    let run_dir = plan.run_dir();
    let dir = run_dir.join(ARTIFACTS_DIR);
    let logs_dir = dir.join(LOGS_DIR);
    let screenshots_dir = dir.join(SCREENSHOTS_DIR);
    ArtifactPaths {
        domain_xml: run_dir.join(DOMAIN_XML_FILE),
        known_hosts: run_dir.join(KNOWN_HOSTS_FILE),
        dir: dir.clone(),
        logs_dir: logs_dir.clone(),
        screenshots_dir: screenshots_dir.clone(),
        dumpxml: logs_dir.join(DUMPXML_FILE),
        screenshot: screenshots_dir.join(SCREENSHOT_FILE),
    }
}

/// Check the live prerequisites for one plan: policy validation, a prepared
/// base image, and the `virsh`/`ssh` tools. Returns the resolved tools when
/// a live run may proceed, or the exact user-facing reason it cannot.
pub fn guest_prereqs(plan: &RunPlan, caps: &Capabilities) -> Result<GuestTools, String> {
    if let Err(violations) = plan.validate() {
        let detail: Vec<String> = violations.iter().map(ToString::to_string).collect();
        return Err(format!("run plan violates policy: {}", detail.join("; ")));
    }
    let base = plan.base_image();
    if !base.is_file() {
        return Err(format!(
            "base image not prepared: {} (manual creation from user media is far-future; see specifications/vm-tier-policy.md)",
            base.display()
        ));
    }
    let Some(virsh) = caps.virsh.clone() else {
        return Err("virsh is not on PATH".to_string());
    };
    let Some(ssh) = caps.ssh.clone() else {
        return Err("ssh is not on PATH".to_string());
    };
    Ok(GuestTools { virsh, ssh })
}

/// Number of SSH readiness attempts for a deadline/poll pair. At least one:
/// a zero timeout still probes once rather than reporting success.
pub fn readiness_attempts(timeout: Duration, poll: Duration) -> u64 {
    let poll_secs = poll.as_secs().max(1);
    timeout.as_secs().div_ceil(poll_secs).max(1)
}

/// `virsh define <xml-path>` arguments. The XML path is a real file staged
/// by [`write_domain_xml`], never a rendered string.
pub fn virsh_define_args(xml_path: &Path) -> Vec<String> {
    vec![
        "define".to_string(),
        xml_path.to_string_lossy().into_owned(),
    ]
}

/// `virsh start <domain>` arguments.
pub fn virsh_start_args(domain: &str) -> Vec<String> {
    vec!["start".to_string(), domain.to_string()]
}

/// `virsh destroy <domain>` arguments (teardown; the domain may already be
/// stopped, so a failure here is reported, not fatal to undefining).
pub fn virsh_destroy_args(domain: &str) -> Vec<String> {
    vec!["destroy".to_string(), domain.to_string()]
}

/// `virsh undefine <domain>` arguments (teardown).
pub fn virsh_undefine_args(domain: &str) -> Vec<String> {
    vec!["undefine".to_string(), domain.to_string()]
}

/// `virsh dumpxml <domain>` arguments (artifact collection).
pub fn virsh_dumpxml_args(domain: &str) -> Vec<String> {
    vec!["dumpxml".to_string(), domain.to_string()]
}

/// `virsh screenshot <domain> <file>` arguments (artifact collection).
pub fn virsh_screenshot_args(domain: &str, file: &Path) -> Vec<String> {
    vec![
        "screenshot".to_string(),
        domain.to_string(),
        file.to_string_lossy().into_owned(),
    ]
}

/// Base `ssh` arguments for one guest target: batch mode (never prompt),
/// a bounded connect timeout, strict host-key checking against the per-run
/// known-hosts file (ephemeral keys stay out of the user's `known_hosts`).
pub fn ssh_base_args(target: &str, known_hosts: &Path, connect_timeout_secs: u64) -> Vec<String> {
    vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={connect_timeout_secs}"),
        "-o".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
        "-o".to_string(),
        format!(
            "UserKnownHostsFile={}",
            known_hosts.to_string_lossy().into_owned()
        ),
        target.to_string(),
    ]
}

/// Full `ssh` invocation running one remote command in the guest.
pub fn ssh_exec_args(
    target: &str,
    known_hosts: &Path,
    connect_timeout_secs: u64,
    remote_command: &str,
) -> Vec<String> {
    let mut args = ssh_base_args(target, known_hosts, connect_timeout_secs);
    args.push(remote_command.to_string());
    args
}

/// Stage the plan's domain XML at `<run-dir>/domain.xml`, creating the run
/// directory. Returns the staged path for `virsh define`.
pub fn write_domain_xml(plan: &RunPlan) -> io::Result<PathBuf> {
    let paths = artifact_paths(plan);
    if let Some(parent) = paths.domain_xml.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&paths.domain_xml, plan.domain_xml())?;
    Ok(paths.domain_xml)
}

/// Remove the plan's run directory (overlay, staged XML, artifacts) when it
/// exists; succeed when it is already gone. This is the cleanup-enforcement
/// primitive: every lifecycle end funnels through it.
pub fn cleanup_run_dir(plan: &RunPlan) -> io::Result<()> {
    let run_dir = plan.run_dir();
    if run_dir.exists() {
        std::fs::remove_dir_all(&run_dir)?;
    }
    Ok(())
}

/// Drop guard enforcing run-directory removal. Armed over the run directory
/// at lifecycle start and disarmed only after the `teardown` stage verifies
/// explicit removal; if any earlier stage returns or panics first, dropping
/// the guard still removes the directory.
#[derive(Debug)]
pub struct RunGuard {
    run_dir: PathBuf,
    disarmed: bool,
}

impl RunGuard {
    /// Arm the guard over `run_dir`. Creates nothing.
    pub fn arm(run_dir: PathBuf) -> Self {
        Self {
            run_dir,
            disarmed: false,
        }
    }

    /// Disarm after the teardown stage verified explicit removal.
    pub fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.disarmed && self.run_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.run_dir);
        }
    }
}

/// Collected artifacts for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactReport {
    /// Staged `virsh dumpxml` snapshot path.
    pub dumpxml: PathBuf,
    /// Screenshot path when `virsh screenshot` succeeded.
    pub screenshot: Option<PathBuf>,
    /// Non-fatal notes (e.g. a hypervisor without screenshot support).
    pub notes: Vec<String>,
}

/// Full stage vector of one lifecycle run, in execution order
/// (`boot`, `ssh`, `artifacts`, `teardown`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleReport {
    /// Stages in execution order.
    pub stages: Vec<StageReport>,
}

/// Run one subcommand and return its stdout on success. `Err` carries the
/// program, arguments, and first non-empty stderr line; only the spawned
/// child is ever affected.
fn run_command(program: &Path, args: &[String]) -> io::Result<String> {
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = crate::overlay::first_non_empty_line(&stderr).unwrap_or("no output");
        return Err(io::Error::other(format!(
            "{} {} failed with {}: {reason}",
            program.display(),
            args.join(" "),
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Define and start the plan's domain. The domain XML is staged to its real
/// path first; `virsh define` consumes that path.
pub fn boot_guest(tools: &GuestTools, plan: &RunPlan) -> io::Result<PathBuf> {
    let xml_path = write_domain_xml(plan)?;
    run_command(&tools.virsh, &virsh_define_args(&xml_path))?;
    run_command(&tools.virsh, &virsh_start_args(&plan.domain_name()))?;
    Ok(xml_path)
}

/// Wait for SSH readiness and prove remote execution: each attempt runs the
/// readiness probe command, and the first attempt whose trimmed stdout
/// matches the expectation wins. Returns the 1-based attempt count as
/// evidence. Only the spawned `ssh` children are ever affected.
pub fn wait_for_ssh(
    tools: &GuestTools,
    target: &str,
    known_hosts: &Path,
    remote_command: &str,
    opts: &GuestOptions,
) -> io::Result<u64> {
    let attempts = readiness_attempts(opts.ssh_timeout, opts.ssh_poll);
    let mut last_error = String::from("no attempts ran");
    for attempt in 1..=attempts {
        let args = ssh_exec_args(
            target,
            known_hosts,
            opts.connect_timeout_secs,
            remote_command,
        );
        match run_command(&tools.ssh, &args) {
            Ok(stdout) if stdout.trim() == SSH_READY_PROBE_EXPECTED => return Ok(attempt),
            Ok(stdout) => {
                last_error = format!("unexpected probe output: {stdout:?}");
            }
            Err(err) => {
                last_error = err.to_string();
            }
        }
        if attempt < attempts {
            std::thread::sleep(opts.ssh_poll);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("SSH {target} never became ready in {attempts} attempt(s): {last_error}"),
    ))
}

/// Collect artifacts for one running domain: `virsh dumpxml` (required) and
/// `virsh screenshot` (best-effort; headless guests may offer none, which is
/// recorded in [`ArtifactReport::notes`] instead of failing the stage).
pub fn collect_artifacts(tools: &GuestTools, plan: &RunPlan) -> io::Result<ArtifactReport> {
    let paths = artifact_paths(plan);
    std::fs::create_dir_all(&paths.logs_dir)?;
    std::fs::create_dir_all(&paths.screenshots_dir)?;
    let domain = plan.domain_name();
    let dumpxml = run_command(&tools.virsh, &virsh_dumpxml_args(&domain))?;
    std::fs::write(&paths.dumpxml, dumpxml)?;
    let mut notes = Vec::new();
    let screenshot = match run_command(
        &tools.virsh,
        &virsh_screenshot_args(&domain, &paths.screenshot),
    ) {
        Ok(_) if paths.screenshot.is_file() => Some(paths.screenshot.clone()),
        Ok(_) => {
            notes.push("virsh screenshot reported success but wrote no file".to_string());
            None
        }
        Err(err) => {
            notes.push(format!("screenshot unavailable: {err}"));
            None
        }
    };
    Ok(ArtifactReport {
        dumpxml: paths.dumpxml,
        screenshot,
        notes,
    })
}

/// Tear down one domain: destroy, then undefine. Both are attempted; the
/// first error is returned so a leaked domain is never reported as torn
/// down. Destroy failing because the domain already stopped still returns
/// that error (honest), while undefining proceeds regardless.
pub fn teardown_guest(tools: &GuestTools, plan: &RunPlan) -> io::Result<()> {
    let domain = plan.domain_name();
    let destroy = run_command(&tools.virsh, &virsh_destroy_args(&domain));
    let undefine = run_command(&tools.virsh, &virsh_undefine_args(&domain));
    match (destroy, undefine) {
        (Ok(_), Ok(_)) => Ok(()),
        (Err(err), _) | (_, Err(err)) => Err(err),
    }
}

/// Render one lifecycle stage report line set for the dry-run path (nothing
/// executed, nothing created).
pub fn dry_run_stages(plan: &RunPlan) -> Vec<StageReport> {
    let paths = artifact_paths(plan);
    let target = plan.ssh_target();
    vec![
        StageReport {
            stage: STAGE_BOOT,
            state: StageState::DryRun(format!(
                "would run: {} && {}",
                crate::overlay::render_command("virsh", &virsh_define_args(&paths.domain_xml)),
                crate::overlay::render_command("virsh", &virsh_start_args(&plan.domain_name()))
            )),
        },
        StageReport {
            stage: STAGE_SSH,
            state: StageState::DryRun(format!(
                "would run: {} (readiness probe, up to {} attempts)",
                crate::overlay::render_command(
                    "ssh",
                    &ssh_exec_args(
                        &target,
                        &paths.known_hosts,
                        SSH_CONNECT_TIMEOUT_SECS,
                        SSH_READY_PROBE_COMMAND
                    )
                ),
                readiness_attempts(SSH_READY_TIMEOUT, SSH_POLL_INTERVAL)
            )),
        },
        StageReport {
            stage: STAGE_ARTIFACTS,
            state: StageState::DryRun(format!(
                "would collect: virsh dumpxml + screenshot into {}",
                paths.dir.display()
            )),
        },
        StageReport {
            stage: STAGE_TEARDOWN,
            state: StageState::DryRun(format!(
                "would run: virsh destroy + undefine {}; remove {}",
                plan.domain_name(),
                plan.run_dir().display()
            )),
        },
    ]
}

/// Report every lifecycle stage as gated with one reason (forced skip or a
/// missing prerequisite). Nothing is executed and nothing is created.
pub fn gated_stages(reason: &str) -> Vec<StageReport> {
    [STAGE_BOOT, STAGE_SSH, STAGE_ARTIFACTS, STAGE_TEARDOWN]
        .iter()
        .map(|stage| StageReport {
            stage,
            state: StageState::Gated(reason.to_string()),
        })
        .collect()
}

/// Run the full guest lifecycle for one validated plan: boot, SSH readiness
/// plus execution probe, artifact collection, teardown, and enforced
/// run-directory removal. The [`RunGuard`] guarantees the run directory is
/// removed even when a stage returns early.
pub fn run_guest_lifecycle(
    plan: &RunPlan,
    tools: &GuestTools,
    opts: &GuestOptions,
) -> LifecycleReport {
    let mut stages: Vec<StageReport> = Vec::with_capacity(4);
    if let Err(violations) = plan.validate() {
        let detail: Vec<String> = violations.iter().map(ToString::to_string).collect();
        let blocked = format!("blocked: run plan violates policy: {}", detail.join("; "));
        stages.push(StageReport {
            stage: STAGE_BOOT,
            state: StageState::Failed(blocked.clone()),
        });
        for stage in [STAGE_SSH, STAGE_ARTIFACTS, STAGE_TEARDOWN] {
            stages.push(StageReport {
                stage,
                state: StageState::Gated(blocked.clone()),
            });
        }
        return LifecycleReport { stages };
    }

    let mut guard = RunGuard::arm(plan.run_dir());
    let paths = artifact_paths(plan);
    let target = plan.ssh_target();

    let booted = match boot_guest(tools, plan) {
        Ok(xml_path) => {
            stages.push(StageReport {
                stage: STAGE_BOOT,
                state: StageState::Ok(format!(
                    "defined {} and started {}",
                    xml_path.display(),
                    plan.domain_name()
                )),
            });
            true
        }
        Err(err) => {
            stages.push(StageReport {
                stage: STAGE_BOOT,
                state: StageState::Failed(format!("boot failed: {err}")),
            });
            false
        }
    };

    if booted {
        match wait_for_ssh(
            tools,
            &target,
            &paths.known_hosts,
            SSH_READY_PROBE_COMMAND,
            opts,
        ) {
            Ok(attempt) => stages.push(StageReport {
                stage: STAGE_SSH,
                state: StageState::Ok(format!(
                    "executed readiness probe in {target} (attempt {attempt})"
                )),
            }),
            Err(err) => stages.push(StageReport {
                stage: STAGE_SSH,
                state: StageState::Failed(format!("SSH execution failed: {err}")),
            }),
        }
        match collect_artifacts(tools, plan) {
            Ok(report) => {
                let mut detail = format!("dumpxml {}", report.dumpxml.display());
                match &report.screenshot {
                    Some(path) => detail.push_str(&format!("; screenshot {}", path.display())),
                    None => detail.push_str("; no screenshot"),
                }
                for note in &report.notes {
                    detail.push_str(&format!(" ({note})"));
                }
                stages.push(StageReport {
                    stage: STAGE_ARTIFACTS,
                    state: StageState::Ok(detail),
                });
            }
            Err(err) => stages.push(StageReport {
                stage: STAGE_ARTIFACTS,
                state: StageState::Failed(format!("artifact collection failed: {err}")),
            }),
        }
    } else {
        for stage in [STAGE_SSH, STAGE_ARTIFACTS] {
            stages.push(StageReport {
                stage,
                state: StageState::Gated("boot failed; never attempted".to_string()),
            });
        }
    }

    match teardown_guest(tools, plan) {
        Ok(()) => match cleanup_run_dir(plan) {
            Ok(()) if !plan.run_dir().exists() => {
                guard.disarm();
                stages.push(StageReport {
                    stage: STAGE_TEARDOWN,
                    state: StageState::Ok(format!(
                        "destroyed + undefined {}; removed {}",
                        plan.domain_name(),
                        plan.run_dir().display()
                    )),
                });
            }
            Ok(()) => stages.push(StageReport {
                stage: STAGE_TEARDOWN,
                state: StageState::Failed(format!(
                    "run directory {} survived removal",
                    plan.run_dir().display()
                )),
            }),
            Err(err) => stages.push(StageReport {
                stage: STAGE_TEARDOWN,
                state: StageState::Failed(format!("run directory removal failed: {err}")),
            }),
        },
        Err(err) => stages.push(StageReport {
            stage: STAGE_TEARDOWN,
            state: StageState::Failed(format!("teardown failed: {err}")),
        }),
    }

    LifecycleReport { stages }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Cadence, guest};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique scratch root per hermetic test (parallel-safe); removed on drop.
    struct HermeticRoot(PathBuf);

    impl HermeticRoot {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "bitty-test-vm-guest-{tag}-{}-{n}",
                std::process::id()
            )))
        }

        fn plan(&self, guest_id: &str, run_id: &str) -> RunPlan {
            let guest = guest(guest_id).expect("test guest exists");
            RunPlan::new(guest, Cadence::Pr, run_id, &self.0).expect("plan builds")
        }
    }

    impl Drop for HermeticRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn artifact_paths_stay_under_the_run_dir() {
        let root = HermeticRoot::new("paths");
        let plan = root.plan("arch", "arch-1");
        let paths = artifact_paths(&plan);
        let run_dir = plan.run_dir();
        for path in [
            &paths.domain_xml,
            &paths.known_hosts,
            &paths.dir,
            &paths.logs_dir,
            &paths.screenshots_dir,
            &paths.dumpxml,
            &paths.screenshot,
        ] {
            assert!(path.starts_with(&run_dir), "{path:?} escapes {run_dir:?}");
        }
        assert_eq!(
            paths.domain_xml.file_name().and_then(|n| n.to_str()),
            Some(DOMAIN_XML_FILE)
        );
        assert_eq!(
            paths.dumpxml.file_name().and_then(|n| n.to_str()),
            Some(DUMPXML_FILE)
        );
        assert_eq!(
            paths.screenshot.file_name().and_then(|n| n.to_str()),
            Some(SCREENSHOT_FILE)
        );
    }

    #[test]
    fn virsh_renderers_name_the_domain_and_the_staged_xml() {
        let root = HermeticRoot::new("virsh");
        let plan = root.plan("arch", "arch-1");
        let paths = artifact_paths(&plan);
        assert_eq!(
            virsh_define_args(&paths.domain_xml),
            vec![
                "define".to_string(),
                paths.domain_xml.to_string_lossy().into_owned()
            ]
        );
        assert_eq!(
            virsh_start_args(&plan.domain_name()),
            vec!["start".to_string(), plan.domain_name()]
        );
        assert_eq!(
            virsh_undefine_args(&plan.domain_name()),
            vec!["undefine".to_string(), plan.domain_name()]
        );
        assert_eq!(
            virsh_dumpxml_args(&plan.domain_name()),
            vec!["dumpxml".to_string(), plan.domain_name()]
        );
        let shot = virsh_screenshot_args(&plan.domain_name(), &paths.screenshot);
        assert_eq!(shot[0], "screenshot");
        assert_eq!(shot[1], plan.domain_name());
        assert!(shot[2].ends_with(SCREENSHOT_FILE));
    }

    #[test]
    fn ssh_args_are_non_interactive_and_scoped_to_the_run() {
        let root = HermeticRoot::new("ssh");
        let plan = root.plan("arch", "arch-1");
        let paths = artifact_paths(&plan);
        let args = ssh_exec_args(
            &plan.ssh_target(),
            &paths.known_hosts,
            SSH_CONNECT_TIMEOUT_SECS,
            SSH_READY_PROBE_COMMAND,
        );
        let joined = args.join(" ");
        assert!(joined.contains("BatchMode=yes"), "{joined}");
        assert!(
            joined.contains(&format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}")),
            "{joined}"
        );
        assert!(joined.contains("StrictHostKeyChecking=yes"), "{joined}");
        assert!(
            joined.contains(&paths.known_hosts.to_string_lossy().into_owned()),
            "{joined}"
        );
        assert!(joined.contains(&plan.ssh_target()), "{joined}");
        assert!(
            args.last()
                .is_some_and(|last| last == SSH_READY_PROBE_COMMAND)
        );
        assert!(
            !joined.contains(".ssh/known_hosts"),
            "run must not touch the user known-hosts: {joined}"
        );
    }

    #[test]
    fn readiness_attempts_cover_the_deadline_and_probe_at_least_once() {
        assert_eq!(
            readiness_attempts(Duration::from_secs(180), Duration::from_secs(5)),
            36
        );
        assert_eq!(
            readiness_attempts(Duration::from_secs(10), Duration::from_secs(30)),
            1
        );
        assert_eq!(
            readiness_attempts(Duration::from_secs(0), Duration::from_secs(5)),
            1
        );
    }

    #[test]
    fn write_domain_xml_stages_the_real_define_path() {
        let root = HermeticRoot::new("xml");
        let plan = root.plan("arch", "arch-1");
        let staged = write_domain_xml(&plan).expect("stage domain XML");
        assert_eq!(staged, artifact_paths(&plan).domain_xml);
        let body = std::fs::read_to_string(&staged).expect("staged XML is readable");
        assert_eq!(body, plan.domain_xml());
        assert!(body.contains(&plan.overlay_image().to_string_lossy().into_owned()));
    }

    #[test]
    fn cleanup_removes_the_run_dir_and_tolerates_absence() {
        let root = HermeticRoot::new("cleanup");
        let plan = root.plan("arch", "arch-1");
        write_domain_xml(&plan).expect("stage something to remove");
        assert!(plan.run_dir().exists());
        cleanup_run_dir(&plan).expect("cleanup removes the run dir");
        assert!(!plan.run_dir().exists());
        cleanup_run_dir(&plan).expect("cleanup tolerates an absent run dir");
    }

    #[test]
    fn run_guard_removes_the_dir_on_drop_until_disarmed() {
        let root = HermeticRoot::new("guard");
        let plan = root.plan("arch", "arch-1");
        let run_dir = plan.run_dir();
        std::fs::create_dir_all(&run_dir).expect("run dir");
        std::fs::write(run_dir.join("marker"), []).expect("marker");
        {
            let _guard = RunGuard::arm(run_dir.clone());
        }
        assert!(!run_dir.exists(), "armed guard removes on drop");

        std::fs::create_dir_all(&run_dir).expect("run dir again");
        {
            let mut guard = RunGuard::arm(run_dir.clone());
            guard.disarm();
        }
        assert!(run_dir.exists(), "disarmed guard keeps the dir");
        std::fs::remove_dir_all(&run_dir).expect("test cleanup");
    }

    #[test]
    fn guest_prereqs_report_the_exact_missing_piece() {
        let root = HermeticRoot::new("prereqs");
        let plan = root.plan("arch", "arch-1");
        let with_tools = Capabilities {
            kvm_device: true,
            qemu_system: Some(PathBuf::from("/usr/bin/qemu-system-x86_64")),
            qemu_img: Some(PathBuf::from("/usr/bin/qemu-img")),
            virsh: Some(PathBuf::from("/usr/bin/virsh")),
            ssh: Some(PathBuf::from("/usr/bin/ssh")),
        };
        let missing_base = guest_prereqs(&plan, &with_tools).expect_err("no base image staged");
        assert!(
            missing_base.contains("base image not prepared"),
            "{missing_base}"
        );

        std::fs::create_dir_all(plan.base_image().parent().expect("base dir")).expect("base dir");
        std::fs::write(plan.base_image(), []).expect("fixture base marker");
        let tools = guest_prereqs(&plan, &with_tools).expect("prereqs satisfied");
        assert_eq!(tools.virsh, PathBuf::from("/usr/bin/virsh"));
        assert_eq!(tools.ssh, PathBuf::from("/usr/bin/ssh"));

        for (field, label) in [
            ("virsh", "virsh is not on PATH"),
            ("ssh", "ssh is not on PATH"),
        ] {
            let mut caps = with_tools.clone();
            if field == "virsh" {
                caps.virsh = None;
            } else {
                caps.ssh = None;
            }
            let reason = guest_prereqs(&plan, &caps).expect_err("tool must be missing");
            assert!(reason.contains(label), "{reason}");
        }
        std::fs::remove_file(plan.base_image()).expect("fixture cleanup");
    }

    #[test]
    fn dry_run_stages_execute_nothing_and_create_nothing() {
        let root = HermeticRoot::new("dryrun");
        let plan = root.plan("arch", "arch-1");
        let stages = dry_run_stages(&plan);
        let states: Vec<_> = stages.iter().map(|s| (s.stage, s.state.label())).collect();
        assert_eq!(
            states,
            [
                ("boot", "dry-run"),
                ("ssh", "dry-run"),
                ("artifacts", "dry-run"),
                ("teardown", "dry-run"),
            ]
        );
        assert!(stages[0].state.detail().contains("virsh define"));
        assert!(stages[1].state.detail().contains(SSH_READY_PROBE_COMMAND));
        assert!(
            !plan.run_dir().exists(),
            "dry run must not create the run dir"
        );
    }

    #[test]
    fn invalid_plan_fails_boot_and_gates_the_rest_without_touching_fs() {
        use crate::policy::Accel;
        use crate::policy::Guest;
        use crate::policy::GuestArch;
        static BAD_GUEST: Guest = Guest {
            id: "bad",
            display: "Bad guest",
            arch: GuestArch::X86_64,
            accel: Accel::Kvm,
            first_cadence: Cadence::Pr,
            ssh_user: "",
        };
        let plan = RunPlan {
            guest: &BAD_GUEST,
            cadence: Cadence::Pr,
            run_id: "bad-1".to_string(),
            vm_root: PathBuf::from("/nonexistent-vm-root"),
        };
        assert!(plan.validate().is_err());
        let tools = GuestTools {
            virsh: PathBuf::from("/nonexistent/virsh"),
            ssh: PathBuf::from("/nonexistent/ssh"),
        };
        let report = run_guest_lifecycle(&plan, &tools, &GuestOptions::default());
        let states: Vec<_> = report
            .stages
            .iter()
            .map(|s| (s.stage, s.state.label()))
            .collect();
        assert_eq!(
            states,
            [
                ("boot", "failed"),
                ("ssh", "gated"),
                ("artifacts", "gated"),
                ("teardown", "gated"),
            ]
        );
        assert!(
            !plan.run_dir().exists(),
            "rejected plan must not create anything"
        );
    }
}
