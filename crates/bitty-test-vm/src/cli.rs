//! `bitty-vm` command-line controller skeleton.
//!
//! Commands (repo-conventional: `cargo run -p bitty-test-vm --bin bitty-vm
//! -- <command>`; no `xtask` crate exists in this workspace):
//!
//! - `guests`   — print the staged guest matrix.
//! - `doctor`   — report host capabilities and configuration honestly.
//! - `plan`     — render a dry-run run plan; never executes anything.
//! - `smoke`    — run the gated accelerator/overlay stages; `--execute`
//!   enables live actions and `--require` turns a gated stage into exit 3.
//!
//! Exit codes: 0 ok, 1 failed, 2 usage/configuration error, 3 gated under
//! `--require`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::capability::Capabilities;
use crate::config::VmConfig;
use crate::overlay;
use crate::policy::{self, Cadence, Guest, GuestArch, RunPlan};
use crate::smoke::{self, SmokeOptions, SmokeOutcome};

/// Success.
pub const EXIT_OK: u8 = 0;
/// A stage or policy check failed.
pub const EXIT_FAILED: u8 = 1;
/// Usage or configuration error.
pub const EXIT_USAGE: u8 = 2;
/// Gated stage under `--require`.
pub const EXIT_GATED: u8 = 3;

/// Human-readable usage text.
pub fn usage() -> String {
    format!(
        "usage: bitty-vm <command> [options]\n\
         \n\
         commands:\n\
         \x20 guests [--cadence <pr|main|nightly>]   list the staged guest matrix\n\
         \x20 doctor                                  report capabilities and configuration\n\
         \x20 plan --guest <id> [options]             render a dry-run run plan (executes nothing)\n\
         \x20 smoke --guest <id> [options]            run the gated accelerator/overlay stages\n\
         \n\
         plan options:\n\
         \x20 --cadence <c>   cadence context (default pr)\n\
         \x20 --run-id <id>   run identifier (default <guest>-<unix-seconds>)\n\
         \x20 --root <dir>    VM root (default ${root_env})\n\
         \x20 --xml           also print the libvirt domain XML\n\
         \n\
         smoke options: plan options plus\
         \x20 --execute       allow live stages (probe, overlay creation, guest lifecycle); ${live_env} opts in\
         \x20 --require       exit 3 when a stage is gated and could not run here (implies --execute,\
         so dry-run stages never count as satisfied)
         \n\
         environment: {root_env} (VM root), {live_env} (live opt-in), \
         {force_env} (force all live stages gated), {iso_env} (manual base-image creation only; \
         never used to run a VM)\n\
         exit codes: 0 ok, 1 failed, 2 usage, 3 gated under --require",
        root_env = crate::config::VM_ROOT_ENV,
        live_env = crate::config::LIVE_ENV,
        force_env = crate::config::FORCE_SKIP_ENV,
        iso_env = crate::config::ISO_PATH_ENV,
    )
}

/// Parsed controller command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print usage and exit 0.
    Help,
    /// List the guest matrix, optionally filtered to one cadence.
    Guests {
        /// Cadence filter (default: all staged rows with their cadences).
        cadence: Option<Cadence>,
    },
    /// Report host capabilities and configuration.
    Doctor,
    /// Render a dry-run run plan.
    Plan(PlanArgs),
    /// Run the gated smoke stages.
    Smoke(SmokeArgs),
}

/// Options of the `plan` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanArgs {
    /// Guest id.
    pub guest: String,
    /// Cadence context.
    pub cadence: Cadence,
    /// Explicit run id.
    pub run_id: Option<String>,
    /// Explicit VM root override.
    pub root: Option<PathBuf>,
    /// Also print the libvirt domain XML.
    pub xml: bool,
}

/// Options of the `smoke` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokeArgs {
    /// Guest id.
    pub guest: String,
    /// Cadence context.
    pub cadence: Cadence,
    /// Explicit run id.
    pub run_id: Option<String>,
    /// Explicit VM root override.
    pub root: Option<PathBuf>,
    /// Allow live stages.
    pub execute: bool,
    /// Fail (exit 3) when a stage is gated.
    pub require_live: bool,
}

#[derive(Default)]
struct Common {
    guest: Option<String>,
    cadence: Option<Cadence>,
    run_id: Option<String>,
    root: Option<PathBuf>,
    xml: bool,
    execute: bool,
    require_live: bool,
    help: bool,
}

/// Parse controller arguments into a [`Command`].
pub fn parse_args(args: &[String]) -> Result<Command, String> {
    let Some((subcommand, rest)) = args.split_first() else {
        return Err(usage());
    };
    match subcommand.as_str() {
        "help" | "--help" | "-h" => Ok(Command::Help),
        "guests" => {
            let common = parse_common(rest, false)?;
            if common.help {
                return Ok(Command::Help);
            }
            if common.guest.is_some()
                || common.run_id.is_some()
                || common.root.is_some()
                || common.xml
                || common.execute
                || common.require_live
            {
                return Err(format!("guests accepts only --cadence\n{}", usage()));
            }
            Ok(Command::Guests {
                cadence: common.cadence,
            })
        }
        "doctor" => {
            if rest.is_empty() {
                Ok(Command::Doctor)
            } else if matches!(rest, [flag] if flag == "--help" || flag == "-h") {
                Ok(Command::Help)
            } else {
                Err(format!("doctor takes no arguments\n{}", usage()))
            }
        }
        "plan" => {
            let common = parse_common(rest, false)?;
            if common.help {
                return Ok(Command::Help);
            }
            Ok(Command::Plan(PlanArgs {
                guest: require_guest(common.guest)?,
                cadence: common.cadence.unwrap_or(Cadence::Pr),
                run_id: common.run_id,
                root: common.root,
                xml: common.xml,
            }))
        }
        "smoke" => {
            let common = parse_common(rest, true)?;
            if common.help {
                return Ok(Command::Help);
            }
            Ok(Command::Smoke(SmokeArgs {
                guest: require_guest(common.guest)?,
                cadence: common.cadence.unwrap_or(Cadence::Pr),
                run_id: common.run_id,
                root: common.root,
                execute: common.execute,
                require_live: common.require_live,
            }))
        }
        other => Err(format!("unknown command {other:?}\n{}", usage())),
    }
}

fn parse_common(rest: &[String], smoke: bool) -> Result<Common, String> {
    let mut common = Common::default();
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--guest" => common.guest = Some(take_value(rest, &mut index, "--guest")?),
            "--cadence" => {
                let raw = take_value(rest, &mut index, "--cadence")?;
                common.cadence = Some(Cadence::parse(&raw).ok_or_else(|| {
                    format!("invalid cadence {raw:?}: expected pr, main, or nightly")
                })?);
            }
            "--run-id" => common.run_id = Some(take_value(rest, &mut index, "--run-id")?),
            "--root" => {
                common.root = Some(PathBuf::from(take_value(rest, &mut index, "--root")?));
            }
            "--xml" if !smoke => common.xml = true,
            "--execute" if smoke => common.execute = true,
            "--require" if smoke => common.require_live = true,
            "--help" | "-h" => common.help = true,
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
        index += 1;
    }
    Ok(common)
}

fn take_value(rest: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    rest.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn require_guest(guest: Option<String>) -> Result<String, String> {
    guest.ok_or_else(|| format!("--guest <id> is required\n{}", usage()))
}

/// Run the controller. Returns the process exit code.
pub fn run(args: impl IntoIterator<Item = String>) -> ExitCode {
    let args: Vec<String> = args.into_iter().collect();
    match parse_args(&args) {
        Ok(Command::Help) => {
            println!("{}", usage());
            ExitCode::from(EXIT_OK)
        }
        Ok(command) => dispatch(command),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn dispatch(command: Command) -> ExitCode {
    match command {
        Command::Help => {
            println!("{}", usage());
            ExitCode::from(EXIT_OK)
        }
        Command::Guests { cadence } => {
            print_guests(cadence);
            ExitCode::from(EXIT_OK)
        }
        Command::Doctor => {
            print_doctor();
            ExitCode::from(EXIT_OK)
        }
        Command::Plan(args) => run_plan(args),
        Command::Smoke(args) => run_smoke(args),
    }
}

/// Whether live stages may run: `--execute`, `BITTY_VM_LIVE`, or
/// `--require` (demanding live coverage while refusing to execute would
/// accept every dry-run stage as satisfied, so `--require` implies
/// `--execute`).
pub fn effective_execute(execute: bool, require_live: bool) -> bool {
    execute || require_live
}

/// Resolve and cross-check the guest against the requested cadence.
pub fn resolve_guest(id: &str, cadence: Cadence) -> Result<&'static Guest, String> {
    let guest =
        policy::guest(id).ok_or_else(|| format!("unknown guest {id:?}; run `bitty-vm guests`"))?;
    if guest.first_cadence > cadence {
        return Err(format!(
            "guest {} enters at cadence {} (requested {}); pass --cadence {} or higher",
            guest.id,
            guest.first_cadence.as_str(),
            cadence.as_str(),
            guest.first_cadence.as_str()
        ));
    }
    Ok(guest)
}

/// Resolve the VM root from the `--root` override or the environment.
pub fn resolve_root(config: &VmConfig) -> Result<PathBuf, String> {
    config.vm_root.clone().ok_or_else(|| {
        format!(
            "VM root is not configured: set {} or pass --root <dir>",
            crate::config::VM_ROOT_ENV
        )
    })
}

/// Resolve the run id from the override or the clock.
pub fn resolve_run_id(guest: &Guest, run_id: Option<&str>) -> String {
    match run_id {
        Some(id) => id.to_string(),
        None => {
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            policy::default_run_id(guest.id, seconds)
        }
    }
}

fn print_guests(cadence: Option<Cadence>) {
    println!("guest        cadence  arch      accel  display");
    let guests: Vec<&'static Guest> = match cadence {
        Some(cadence) => cadence.guests().collect(),
        None => policy::GUESTS.iter().collect(),
    };
    for guest in guests {
        println!(
            "{:<12} {:<8} {:<9} {:<6} {}",
            guest.id,
            guest.first_cadence.as_str(),
            guest.arch.as_str(),
            guest.accel.as_str(),
            guest.display
        );
    }
}

fn print_doctor() {
    let config = VmConfig::from_env();
    let caps = Capabilities::probe(GuestArch::X86_64);
    println!("bitty-vm doctor");
    println!(
        "  {:<22} {}",
        crate::config::VM_ROOT_ENV,
        config.vm_root.as_ref().map_or_else(
            || "(unset; required for plan/smoke)".to_string(),
            |root| root.display().to_string()
        )
    );
    println!(
        "  {:<22} {}",
        crate::config::LIVE_ENV,
        if config.live {
            "set (live opt-in)"
        } else {
            "unset"
        }
    );
    println!(
        "  {:<22} {}",
        crate::config::FORCE_SKIP_ENV,
        if config.force_skip {
            "set (live stages gated)"
        } else {
            "unset"
        }
    );
    println!(
        "  {:<22} {}",
        crate::config::ISO_PATH_ENV,
        if config.iso_path_set {
            "set (manual base-image creation only; the controller never uses it)"
        } else {
            "unset (only needed for manual base-image creation)"
        }
    );
    println!(
        "  {:<22} {}",
        crate::capability::KVM_DEVICE,
        if caps.kvm_device {
            "present"
        } else {
            "absent (KVM guests gated)"
        }
    );
    for (label, path) in [
        ("qemu-system-x86_64", caps.qemu_system.as_ref()),
        ("qemu-img", caps.qemu_img.as_ref()),
        ("virsh", caps.virsh.as_ref()),
        ("ssh", caps.ssh.as_ref()),
    ] {
        println!(
            "  {:<22} {}",
            label,
            path.map_or_else(|| "missing".to_string(), |path| path.display().to_string())
        );
    }
    println!(
        "  guest boot / SSH exec / artifacts: implemented behind --execute/{live} \
         (gated without a prepared base image, virsh, or ssh)",
        live = crate::config::LIVE_ENV,
    );
}

fn run_plan(args: PlanArgs) -> ExitCode {
    let guest = match resolve_guest(&args.guest, args.cadence) {
        Ok(guest) => guest,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let config = VmConfig::from_env().with_root_override(args.root);
    let root = match resolve_root(&config) {
        Ok(root) => root,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let run_id = resolve_run_id(guest, args.run_id.as_deref());
    let plan = match RunPlan::new(guest, args.cadence, run_id, root) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("invalid run plan: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if let Err(violations) = plan.validate() {
        eprintln!("run plan violates the VM tier policy:");
        for violation in violations {
            eprintln!("  {violation}");
        }
        return ExitCode::from(EXIT_FAILED);
    }

    println!(
        "run plan: {} ({}) cadence={}",
        plan.guest.id,
        plan.guest.display,
        plan.cadence.as_str()
    );
    println!("run id:   {}", plan.run_id);
    println!("vm root:  {}", plan.vm_root.display());
    println!(
        "base:     {} (prepared manually; never an ISO at run time)",
        plan.base_image().display()
    );
    println!("overlay:  {}", plan.overlay_image().display());
    println!("domain:   {}", plan.domain_name());
    println!("ssh:      {}", plan.ssh_target());
    println!(
        "xml path: {} (staged here by the live lifecycle for `virsh define`)",
        crate::guest::artifact_paths(&plan).domain_xml.display()
    );
    println!(
        "artifacts: {} (dumpxml + screenshot; removed with the run dir)",
        crate::guest::artifact_paths(&plan).dir.display()
    );
    let suites: Vec<_> = policy::guest_suites(plan.guest).map(|s| s.id).collect();
    println!(
        "suites:   {} (in-guest candidates; recorded plan, not scheduled work)",
        suites.join(", ")
    );
    println!("commands (dry run; nothing executed):");
    println!(
        "  {}",
        overlay::render_command("qemu-img", &plan.qemu_img_create_args())
    );
    println!("  virsh define {}", plan.domain_name());
    println!("  virsh start {}", plan.domain_name());
    println!(
        "  ssh {}   # guest IP via the QEMU guest agent at run time",
        plan.ssh_target()
    );
    println!("policy:   ok (base image + qcow2 overlay; no ISO disk)");
    if args.xml {
        println!("domain xml (dry run):");
        print!("{}", plan.domain_xml());
    }
    ExitCode::from(EXIT_OK)
}

fn run_smoke(args: SmokeArgs) -> ExitCode {
    let guest = match resolve_guest(&args.guest, args.cadence) {
        Ok(guest) => guest,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let config = VmConfig::from_env()
        .with_root_override(args.root.clone())
        .with_execute(effective_execute(args.execute, args.require_live));
    let root = match resolve_root(&config) {
        Ok(root) => root,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let run_id = resolve_run_id(guest, args.run_id.as_deref());
    let plan = match RunPlan::new(guest, args.cadence, run_id, root) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("invalid run plan: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let caps = Capabilities::probe(guest.arch);
    let options = SmokeOptions {
        execute: config.live,
        require_live: args.require_live,
        force_skip: config.force_skip,
        timeout: crate::kvm::PROBE_TIMEOUT,
    };
    let report = smoke::run_smoke(plan, &caps, &options);
    print!("{}", report.render(args.require_live));
    match report.outcome(args.require_live) {
        SmokeOutcome::Ok => ExitCode::from(EXIT_OK),
        SmokeOutcome::Gated => ExitCode::from(EXIT_GATED),
        SmokeOutcome::Failed => ExitCode::from(EXIT_FAILED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_guests_and_cadence() {
        assert_eq!(
            parse_args(&args(&["guests"])).expect("parses"),
            Command::Guests { cadence: None }
        );
        assert_eq!(
            parse_args(&args(&["guests", "--cadence", "main"])).expect("parses"),
            Command::Guests {
                cadence: Some(Cadence::Main)
            }
        );
        assert!(parse_args(&args(&["guests", "--xml"])).is_err());
    }

    #[test]
    fn parses_doctor_and_help() {
        assert_eq!(
            parse_args(&args(&["doctor"])).expect("parses"),
            Command::Doctor
        );
        assert_eq!(
            parse_args(&args(&["--help"])).expect("parses"),
            Command::Help
        );
        assert_eq!(
            parse_args(&args(&["plan", "-h"])).expect("parses"),
            Command::Help
        );
        assert!(parse_args(&args(&["doctor", "extra"])).is_err());
    }

    #[test]
    fn parses_plan_defaults_and_flags() {
        assert_eq!(
            parse_args(&args(&["plan", "--guest", "arch"])).expect("parses"),
            Command::Plan(PlanArgs {
                guest: "arch".to_string(),
                cadence: Cadence::Pr,
                run_id: None,
                root: None,
                xml: false,
            })
        );
        assert_eq!(
            parse_args(&args(&[
                "plan",
                "--guest",
                "arch-arm64",
                "--cadence",
                "nightly",
                "--run-id",
                "arm-1",
                "--root",
                "/vm",
                "--xml",
            ]))
            .expect("parses"),
            Command::Plan(PlanArgs {
                guest: "arch-arm64".to_string(),
                cadence: Cadence::Nightly,
                run_id: Some("arm-1".to_string()),
                root: Some(PathBuf::from("/vm")),
                xml: true,
            })
        );
    }

    #[test]
    fn parses_smoke_flags_and_rejects_plan_only_flags() {
        assert_eq!(
            parse_args(&args(&[
                "smoke",
                "--guest",
                "arch",
                "--execute",
                "--require"
            ]))
            .expect("parses"),
            Command::Smoke(SmokeArgs {
                guest: "arch".to_string(),
                cadence: Cadence::Pr,
                run_id: None,
                root: None,
                execute: true,
                require_live: true,
            })
        );
        assert!(parse_args(&args(&["plan", "--guest", "arch", "--execute"])).is_err());
        assert!(parse_args(&args(&["smoke", "--guest", "arch", "--xml"])).is_err());
    }

    #[test]
    fn reports_missing_values_and_unknown_arguments() {
        for bad in [
            vec!["plan", "--guest"],
            vec!["plan"],
            vec!["smoke"],
            vec!["nonsense"],
            vec!["plan", "--guest", "arch", "--cadence", "hourly"],
            vec!["plan", "--guest", "arch", "--wat"],
        ] {
            assert!(parse_args(&args(&bad)).is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn require_implies_execute() {
        assert!(!effective_execute(false, false));
        assert!(effective_execute(true, false));
        assert!(effective_execute(false, true));
        assert!(effective_execute(true, true));
    }

    #[test]
    fn resolve_guest_cross_checks_the_cadence() {
        assert_eq!(
            resolve_guest("arch", Cadence::Pr).expect("pr guest").id,
            "arch"
        );
        assert!(resolve_guest("ubuntu", Cadence::Pr).is_err());
        assert!(resolve_guest("ubuntu", Cadence::Main).is_ok());
        assert!(resolve_guest("arch-arm64", Cadence::Main).is_err());
        assert!(resolve_guest("arch-arm64", Cadence::Nightly).is_ok());
        assert!(resolve_guest("beos", Cadence::Nightly).is_err());
    }

    #[test]
    fn resolve_root_prefers_the_override() {
        let config =
            VmConfig::from_values(Some(std::ffi::OsStr::new("/env-root")), None, None, None)
                .with_root_override(Some(PathBuf::from("/cli-root")));
        assert_eq!(
            resolve_root(&config).expect("root"),
            PathBuf::from("/cli-root")
        );

        let empty = VmConfig::from_values(None, None, None, None);
        assert!(resolve_root(&empty).is_err());
    }

    #[test]
    fn resolve_run_id_uses_the_override_or_a_safe_default() {
        let guest = policy::guest("arch").expect("guest");
        assert_eq!(resolve_run_id(guest, Some("fixed-1")), "fixed-1");
        let generated = resolve_run_id(guest, None);
        assert!(policy::run_id_is_safe(&generated), "{generated}");
        assert!(generated.starts_with("arch-"));
    }

    #[test]
    fn usage_names_every_environment_variable() {
        let usage = usage();
        for name in [
            crate::config::VM_ROOT_ENV,
            crate::config::LIVE_ENV,
            crate::config::FORCE_SKIP_ENV,
            crate::config::ISO_PATH_ENV,
        ] {
            assert!(usage.contains(name), "usage must mention {name}");
        }
    }
}
