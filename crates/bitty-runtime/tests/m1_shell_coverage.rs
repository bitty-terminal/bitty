//! M1-27 (CTX-0588, Issue #1153): six-shell coverage evidence with ZERO
//! shell integration active, plus injected `OSC 7` / `OSC 133` tests.
//!
//! Acceptance source: `compatibility-milestone-rfc.md` "Shell integration"
//! evidence row — shells (bash, zsh, fish, PowerShell, cmd, nushell) must
//! operate normally *without* shell integration, injected-script tests must
//! cover `OSC 7` cwd and `OSC 133` zones, and no prompt-text heuristic may
//! exist.
//!
//! ## What this suite proves
//!
//! 1. For every shell in the M1 roster that is actually installed on the
//!    runner, the real shell is spawned through the owned PTY with **zero
//!    integration active** (runtime default config: no plugin, no panel, no
//!    injected integration script; user/system rc files disabled) and its
//!    output is captured through the real parser into terminal state.
//! 2. For every such shell, the shell itself emits injected `OSC 7` (cwd) and
//!    `OSC 133` (`A`/`B`/`C`/`D` with exit status) sequences into the PTY, and
//!    the parser/state records exactly one cwd report and the four zone
//!    boundaries in order.
//! 3. Plain prompt-looking text produces no cwd report and no zones, so no
//!    prompt-text heuristic can be masquerading as integration.
//!
//! ## Honesty rules
//!
//! * A shell that is not installed is **skipped with an explicit, documented
//!   reason** printed as `SHELL-COVERAGE shell=<name> status=skipped reason=…`;
//!   it is never faked and never silently passes.
//! * A shell that *is* installed but fails to spawn/emit fails the run.
//! * [`at_least_one_tier1_shell_is_available`] fails when the roster yields
//!   zero runnable shells, so an all-skipped run cannot masquerade as
//!   evidence. Every ADR-0002 Tier 1 runner ships at least one roster shell
//!   (`bash`/`sh` on Unix, `cmd.exe`/PowerShell on Windows).
//!
//! ## Platform gating
//!
//! Cross-platform by construction: no Unix-only path is hardcoded. Every
//! live-spawn test opens with `require_pty!()` (the repo pty-gate marker), and
//! programs are resolved from `PATH`/`PATHEXT` per platform. Because the suite
//! is a normal workspace integration test it runs on every Tier 1 CI leg via
//! `cargo test --workspace`; shells whose candidates are absent on that leg
//! skip with a recorded reason.
//!
//! ## Reproduce
//!
//! ```text
//! cargo test -p bitty-runtime --test m1_shell_coverage --locked -- --test-threads=1 --nocapture
//! ```

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bitty_runtime::Runtime;
use bitty_runtime::shell_integration::ShellIntegration;
use bitty_term_state::ZoneKind;
use bitty_test_support::require_pty;

/// Bound for a one-shot shell to emit its bytes; a healthy shell finishes in
/// milliseconds, so this only fires when something is genuinely stuck.
const SHELL_TIMEOUT: Duration = Duration::from_secs(15);

/// Environment variable listing executable extensions on Windows.
#[cfg(windows)]
const PATHEXT_ENV: &str = "PATHEXT";

/// Command-language family, selecting how a payload reaches stdout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// POSIX shell: `printf '%s' '<literal>'`.
    Posix,
    /// `cmd.exe`: `echo <plain>`; raw bytes via `type "<file>"`.
    Cmd,
    /// PowerShell: `[Console]::Out.Write('<plain>')`; raw bytes via
    /// `[System.IO.File]::ReadAllText('<file>')`.
    PowerShell,
    /// nushell: `print '<plain>'`; raw bytes via `print -n (open --raw '<file>')`.
    Nushell,
}

impl Kind {
    /// Script printing `text` verbatim (plain single-line markers).
    fn plain(self, text: &str) -> String {
        match self {
            Kind::Posix => format!("printf '%s' '{text}'"),
            Kind::Cmd => format!("echo {text}"),
            Kind::PowerShell => format!("[Console]::Out.Write('{text}')"),
            Kind::Nushell => format!("print '{text}'"),
        }
    }

    /// Script dumping the raw bytes of `path` to stdout.
    ///
    /// POSIX shells and `cmd.exe` emit the injected stream inline and never
    /// call this helper; PowerShell and nushell read a staged file.
    fn raw_file(self, path: &Path) -> String {
        match self {
            Kind::Posix => format!("printf '%s' \"$(cat '{0}')\"", path.display()),
            // Unreachable: `cmd.exe` re-quotes any argument containing a
            // space, and its parser does not understand MSVC backslash-escaped
            // quotes, so a quoted file path cannot survive the Windows command
            // line. `injection_argv` echoes the cmd payload inline instead.
            Kind::Cmd => format!("type {}", path.display()),
            Kind::PowerShell => format!(
                "[Console]::Out.Write([System.IO.File]::ReadAllText('{}'))",
                path.display()
            ),
            Kind::Nushell => format!("print -n (open --raw '{}')", path.display()),
        }
    }
}

/// One shell in the M1 roster.
#[derive(Clone, Copy, Debug)]
struct ShellSpec {
    /// Stable roster name used in test names and evidence lines.
    name: &'static str,
    /// Executable names probed in order.
    candidates: &'static [&'static str],
    /// Flags that disable every user/system startup file.
    no_rc: &'static [&'static str],
    /// Flag group introducing the one-shot command string.
    cmd_flag: &'static [&'static str],
    /// Command-language family.
    kind: Kind,
    /// Platform note recorded when the shell is unavailable.
    absent_reason: &'static str,
}

#[cfg(unix)]
const ROSTER: &[ShellSpec] = &[
    ShellSpec {
        name: "bash",
        candidates: &["bash"],
        no_rc: &["--noprofile", "--norc"],
        cmd_flag: &["-c"],
        kind: Kind::Posix,
        absent_reason: "bash not found on PATH",
    },
    ShellSpec {
        name: "zsh",
        candidates: &["zsh"],
        no_rc: &["-f"],
        cmd_flag: &["-c"],
        kind: Kind::Posix,
        absent_reason: "zsh not installed on this runner",
    },
    ShellSpec {
        name: "fish",
        candidates: &["fish"],
        no_rc: &["--no-config"],
        cmd_flag: &["-c"],
        kind: Kind::Posix,
        absent_reason: "fish not installed on this runner",
    },
    ShellSpec {
        name: "powershell",
        candidates: &["pwsh", "powershell"],
        no_rc: &["-NoProfile", "-NonInteractive"],
        cmd_flag: &["-Command"],
        kind: Kind::PowerShell,
        absent_reason: "PowerShell (pwsh/powershell) not installed on this runner",
    },
    ShellSpec {
        name: "nushell",
        candidates: &["nu", "nushell"],
        no_rc: &["--no-config-file"],
        cmd_flag: &["-c"],
        kind: Kind::Nushell,
        absent_reason: "nushell (nu) not installed on this runner",
    },
    ShellSpec {
        name: "cmd",
        candidates: &["cmd.exe", "cmd"],
        no_rc: &["/D"],
        cmd_flag: &["/C"],
        kind: Kind::Cmd,
        absent_reason: "cmd.exe is a Windows-only program",
    },
];

#[cfg(windows)]
const ROSTER: &[ShellSpec] = &[
    ShellSpec {
        name: "cmd",
        candidates: &["cmd.exe", "cmd"],
        no_rc: &["/D"],
        cmd_flag: &["/C"],
        kind: Kind::Cmd,
        absent_reason: "cmd.exe not found in the system directory",
    },
    ShellSpec {
        name: "powershell",
        candidates: &["powershell.exe", "powershell", "pwsh.exe", "pwsh"],
        no_rc: &["-NoProfile", "-NonInteractive"],
        cmd_flag: &["-Command"],
        kind: Kind::PowerShell,
        absent_reason: "PowerShell not found on this runner",
    },
    ShellSpec {
        name: "bash",
        candidates: &["bash.exe", "bash"],
        no_rc: &["--noprofile", "--norc"],
        cmd_flag: &["-c"],
        kind: Kind::Posix,
        absent_reason: "bash (Git Bash) not found on PATH",
    },
    ShellSpec {
        name: "nushell",
        candidates: &["nu.exe", "nu", "nushell.exe", "nushell"],
        no_rc: &["--no-config-file"],
        cmd_flag: &["-c"],
        kind: Kind::Nushell,
        absent_reason: "nushell (nu) not installed on this runner",
    },
    ShellSpec {
        name: "zsh",
        candidates: &["zsh.exe", "zsh"],
        no_rc: &["-f"],
        cmd_flag: &["-c"],
        kind: Kind::Posix,
        absent_reason: "zsh is not a Windows shell",
    },
    ShellSpec {
        name: "fish",
        candidates: &["fish.exe", "fish"],
        no_rc: &["--no-config"],
        cmd_flag: &["-c"],
        kind: Kind::Posix,
        absent_reason: "fish is not a Windows shell",
    },
];

/// Looks up the first resolvable candidate using `lookup`.
///
/// Pure over its inputs so the resolution contract is testable without a
/// process environment; [`which`] supplies the real environment.
fn find_program(candidates: &[&str], lookup: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    candidates.iter().find_map(|candidate| lookup(candidate))
}

/// Executable name variants for `program` on the current platform.
fn candidate_names(program: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let mut names = vec![program.to_string()];
        if Path::new(program).extension().is_none() {
            let pathext =
                std::env::var(PATHEXT_ENV).unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            for ext in pathext.split(';').filter(|ext| !ext.is_empty()) {
                names.push(format!("{program}{ext}"));
            }
        }
        names
    }
    #[cfg(unix)]
    {
        vec![program.to_string()]
    }
}

/// Resolves `program` against the process `PATH` (plus `PATHEXT` on Windows).
fn which(program: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for candidate in candidate_names(program) {
            let full = dir.join(&candidate);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

/// Resolves the first installed candidate for `spec`.
fn resolve(spec: &ShellSpec) -> Option<PathBuf> {
    find_program(spec.candidates, &which)
}

/// Returns the roster entry with `name`.
fn spec_named(name: &str) -> &'static ShellSpec {
    ROSTER
        .iter()
        .find(|spec| spec.name == name)
        .unwrap_or_else(|| panic!("unknown M1 shell '{name}'"))
}

/// Marker text a smoke run prints and the suite asserts on.
fn marker(name: &str) -> String {
    format!("m1-shell-smoke-{name}")
}

/// The exact injected byte stream: `OSC 7` cwd, then `OSC 133` A/B/C, the
/// marker, and `OSC 133;D;0`.
fn injected_bytes(name: &str) -> String {
    format!(
        "\x1b]7;file://bitty.invalid/ctx-0588/{name}\x07\
         \x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\
         {}\x1b]133;D;0\x07",
        marker(name)
    )
}

/// Expected cwd report from [`injected_bytes`].
fn injected_cwd(name: &str) -> String {
    format!("file://bitty.invalid/ctx-0588/{name}")
}

/// Builds a shell argv suffix from no-rc flags + command flag + script.
fn argv_with(spec: &ShellSpec, script: &str) -> Vec<String> {
    let mut args: Vec<String> = spec.no_rc.iter().map(|arg| (*arg).to_string()).collect();
    args.extend(spec.cmd_flag.iter().map(|arg| (*arg).to_string()));
    args.push(script.to_string());
    args
}

/// Stages `bytes` in a process-private temp file and returns its path.
fn stage_payload_file(name: &str, bytes: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("bitty-ctx0588-{name}-{}.bin", std::process::id()));
    std::fs::write(&path, bytes.as_bytes()).expect("stage injected payload file");
    path
}

/// Builds the injected `OSC 7`/`OSC 133` argv for `spec`.
///
/// The POSIX family and `cmd.exe` pass the raw bytes inline (POSIX via
/// `printf`, cmd via `echo`); PowerShell and nushell read them from a staged
/// temp file. Returns `(argv, temp_file)`; the caller removes `temp_file`
/// when present.
fn injection_argv(spec: &ShellSpec, name: &str) -> (Vec<String>, Option<PathBuf>) {
    let bytes = injected_bytes(name);
    match spec.kind {
        Kind::Posix => (argv_with(spec, &format!("printf '%s' '{bytes}'")), None),
        // `cmd.exe` re-quotes any argument containing a space, and its parser
        // does not understand MSVC backslash-escaped inner quotes, so a quoted
        // temp-file path cannot survive the Windows command line. The injected
        // stream carries no cmd metacharacter, so `echo` is byte-faithful
        // here; `cmd_payload_is_echoed_inline_never_via_a_quoted_file_path`
        // pins that invariant.
        Kind::Cmd => (argv_with(spec, &format!("echo {bytes}")), None),
        Kind::PowerShell | Kind::Nushell => {
            let path = stage_payload_file(name, &bytes);
            let script = spec.kind.raw_file(&path);
            (argv_with(spec, &script), Some(path))
        }
    }
}

/// Spawns `spec` with `args` through the real runtime PTY.
///
/// Panics when a resolved shell cannot be spawned: a shell that exists but
/// cannot run is a coverage defect, never a skip.
fn spawn(spec: &ShellSpec, program: &Path, args: &[String]) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let program = program.to_string_lossy().into_owned();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    rt.spawn_shell_with_args(&program, &arg_refs)
        .unwrap_or_else(|err| panic!("spawn {} ({program}): {err:?}", spec.name));
    rt
}

/// Polls the runtime until `done` is true or [`SHELL_TIMEOUT`] elapses.
fn wait_until(rt: &mut Runtime, done: &dyn Fn(&Runtime) -> bool) -> bool {
    let deadline = Instant::now() + SHELL_TIMEOUT;
    loop {
        if done(rt) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = rt.poll_pty_timeout(Duration::from_millis(50));
    }
}

/// Grid text of the primary snapshot (marker observation).
fn snapshot_text(rt: &Runtime) -> String {
    rt.snapshot().cells.iter().map(|cell| cell.glyph).collect()
}

/// Asserts no shell-integration plugin is active for this run.
fn assert_zero_integration(rt: &Runtime, context: &str) {
    assert_eq!(
        rt.plugin_host().registry().len(),
        0,
        "{context}: zero-integration run must have no plugin registered"
    );
}

/// Smoke: the real shell runs and echoes through the PTY with zero
/// integration active.
fn run_smoke(name: &str) {
    let spec = spec_named(name);
    let Some(program) = resolve(spec) else {
        eprintln!(
            "SHELL-COVERAGE shell={name} status=skipped reason=\"{}\"",
            spec.absent_reason
        );
        return;
    };
    let args = argv_with(spec, &spec.kind.plain(&marker(name)));
    let mut rt = spawn(spec, &program, &args);
    assert_zero_integration(&rt, name);

    let expected = marker(name);
    let found = wait_until(&mut rt, &|rt| snapshot_text(rt).contains(&expected));
    assert!(
        found,
        "SHELL-COVERAGE shell={name} status=failed: marker '{expected}' never arrived; got {:?}",
        snapshot_text(&rt)
    );
    // Zero integration active: the shell's own startup must not emit OSC 7/133.
    assert!(
        ShellIntegration::cwd(rt.state()).is_none(),
        "SHELL-COVERAGE shell={name}: no integration means no OSC 7 cwd report"
    );
    assert_eq!(
        rt.state().zone_len(),
        0,
        "SHELL-COVERAGE shell={name}: no integration means zero OSC 133 zones"
    );
    eprintln!(
        "SHELL-COVERAGE shell={name} program={} status=ran mode=smoke",
        program.display()
    );
}

/// Injected: the shell emits `OSC 7`/`OSC 133`; the parser/state captures them.
fn run_injection(name: &str) {
    let spec = spec_named(name);
    let Some(program) = resolve(spec) else {
        eprintln!(
            "SHELL-COVERAGE shell={name} status=skipped reason=\"{}\"",
            spec.absent_reason
        );
        return;
    };
    let (args, temp) = injection_argv(spec, name);
    let mut rt = spawn(spec, &program, &args);

    let expected_cwd = injected_cwd(name);
    let captured = wait_until(&mut rt, &|rt| {
        ShellIntegration::cwd(rt.state()) == Some(expected_cwd.as_str())
            && rt.state().zone_len() >= 4
    });

    let observed_cwd = ShellIntegration::cwd(rt.state()).map(str::to_owned);
    let zones: Vec<ZoneKind> = rt.state().zones().map(|zone| zone.kind).collect();
    assert!(
        captured,
        "SHELL-COVERAGE shell={name} status=failed: injected OSC not captured; \
         cwd={observed_cwd:?} zones={zones:?} grid={:?}",
        snapshot_text(&rt)
    );
    // The injected bytes must yield exactly one cwd report and one full
    // A/B/C/D cycle (no rc-file integration can add extra zones).
    assert_eq!(
        observed_cwd.as_deref(),
        Some(expected_cwd.as_str()),
        "SHELL-COVERAGE shell={name}: OSC 7 cwd must round-trip exactly"
    );
    assert_eq!(
        zones,
        vec![
            ZoneKind::PromptStart,
            ZoneKind::InputStart,
            ZoneKind::OutputStart,
            ZoneKind::OutputEnd,
        ],
        "SHELL-COVERAGE shell={name}: OSC 133 must yield A/B/C/D in order"
    );
    assert_eq!(
        ShellIntegration::last_exit_code(rt.state()),
        Some(0),
        "SHELL-COVERAGE shell={name}: OSC 133;D;0 must report exit code 0"
    );
    assert!(
        snapshot_text(&rt).contains(&marker(name)),
        "SHELL-COVERAGE shell={name}: marker between OSC 133 C and D must print"
    );
    eprintln!(
        "SHELL-COVERAGE shell={name} program={} status=ran mode=injection cwd={expected_cwd} zones=4",
        program.display()
    );

    drop(rt);
    if let Some(path) = temp {
        let _ = std::fs::remove_file(path);
    }
}

// --- anti-vacuity and purity -------------------------------------------------

#[test]
fn at_least_one_tier1_shell_is_available() {
    require_pty!();
    let available: Vec<&str> = ROSTER
        .iter()
        .filter(|spec| resolve(spec).is_some())
        .map(|spec| spec.name)
        .collect();
    assert!(
        !available.is_empty(),
        "SHELL-COVERAGE status=failed: no M1 roster shell resolved on this Tier 1 runner; \
         an all-skipped run is not evidence"
    );
    eprintln!("SHELL-COVERAGE available={available:?}");
}

#[test]
fn resolver_prefers_earlier_candidates_and_reports_absence() {
    let lookup = |name: &str| -> Option<PathBuf> {
        match name {
            "second" => Some(PathBuf::from("stub-second")),
            _ => None,
        }
    };
    assert_eq!(
        find_program(&["first", "second"], &lookup),
        Some(PathBuf::from("stub-second")),
        "candidate order must be honored"
    );
    assert_eq!(
        find_program(&["missing", "absent"], &lookup),
        None,
        "absent programs must resolve to None (explicit skip reason)"
    );
}

#[test]
fn cmd_payload_is_echoed_inline_never_via_a_quoted_file_path() {
    // Windows regression (CI run 35521362359): portable-pty re-quotes any
    // argv element containing a space, and `cmd.exe`'s parser does not accept
    // the MSVC backslash-escaped inner quotes, so `type "<tmp path>"` failed
    // with "The filename, directory name, or volume label syntax is
    // incorrect". The bytes therefore travel inline (`echo`), which is safe
    // here: the injected stream contains no cmd metacharacters.
    let spec = spec_named("cmd");
    let (args, temp) = injection_argv(spec, "cmd");
    assert!(temp.is_none(), "cmd must not stage a temp payload file");
    let script = args.last().expect("script arg");
    assert!(
        script.starts_with("echo "),
        "cmd injection must echo the payload inline, got {script:?}"
    );
    for meta in ['"', '^', '&', '|', '<', '>', '(', ')', '%'] {
        assert!(
            !script.contains(meta),
            "inline cmd payload must be metacharacter-free, found {meta:?} in {script:?}"
        );
    }
    assert!(
        script.contains("\u{1b}]7;") && script.contains("\u{1b}]133;D;0"),
        "inline cmd payload must carry the injected OSC 7/133 stream"
    );
}

#[test]
fn prompt_like_text_produces_no_cwd_or_zones() {
    // No prompt-text heuristic: ordinary prompt/output strings must never
    // synthesize OSC 7 or OSC 133 state.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    for chunk in [
        &b"user@host:~/dev$ "[..],
        b"root# ",
        b"cargo build\n",
        b"Compiling bitty-runtime v0.0.20\n",
        b"\x1b[32m$\x1b[0m ",
    ] {
        rt.handle_pty_bytes(chunk);
    }
    assert!(
        ShellIntegration::cwd(rt.state()).is_none(),
        "prompt text must not fabricate an OSC 7 cwd report"
    );
    assert_eq!(
        rt.state().zone_len(),
        0,
        "prompt text must not fabricate OSC 133 zones"
    );
    assert!(!ShellIntegration::has_observation(rt.state()));
}

// --- per-shell smoke suites (zero integration active) -----------------------

#[test]
fn smoke_bash_runs_without_integration() {
    require_pty!();
    run_smoke("bash");
}

#[test]
fn smoke_zsh_runs_without_integration() {
    require_pty!();
    run_smoke("zsh");
}

#[test]
fn smoke_fish_runs_without_integration() {
    require_pty!();
    run_smoke("fish");
}

#[test]
fn smoke_powershell_runs_without_integration() {
    require_pty!();
    run_smoke("powershell");
}

#[test]
fn smoke_cmd_runs_without_integration() {
    require_pty!();
    run_smoke("cmd");
}

#[test]
fn smoke_nushell_runs_without_integration() {
    require_pty!();
    run_smoke("nushell");
}

// --- injected OSC 7 / OSC 133 suites ---------------------------------------

#[test]
fn injected_osc7_osc133_bash() {
    require_pty!();
    run_injection("bash");
}

#[test]
fn injected_osc7_osc133_zsh() {
    require_pty!();
    run_injection("zsh");
}

#[test]
fn injected_osc7_osc133_fish() {
    require_pty!();
    run_injection("fish");
}

#[test]
fn injected_osc7_osc133_powershell() {
    require_pty!();
    run_injection("powershell");
}

#[test]
fn injected_osc7_osc133_cmd() {
    require_pty!();
    run_injection("cmd");
}

#[test]
fn injected_osc7_osc133_nushell() {
    require_pty!();
    run_injection("nushell");
}
