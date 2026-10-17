//! `bitty-sandbox`: inherited-OS-confinement adapter for plugin-spawned tools
//! (CTX-1151, issue #1984).
//!
//! The Layer-2 CLI allowlist (`bitty-plugin-host` `tools`, CTX-0444) mediates
//! tool and argv at the gate, but confinement does not inherit: an admitted
//! GTFOBins-class binary can still exec a shell from inside. This crate adds
//! the inherited restriction on child exec so a plugin-spawned process cannot
//! exec outside the allowlist no matter which escape hatch the admitted
//! binary offers. Confinement setup failure refuses the spawn (fail-closed,
//! diagnostic, no silent pass).
//!
//! # Why a separate crate
//!
//! Every Bitty crate root enforces `#![forbid(unsafe_code)]` except
//! `bitty-winjob`, which keeps its reviewed Win32 FFI behind its own crate
//! boundary (`#![deny(unsafe_code)]` root, one `#[allow(unsafe_code)]`
//! module). Raw Landlock syscalls need `unsafe`, so the enforcement shim
//! cannot live inside `bitty-plugin-host` or `bitty-runtime` without
//! weakening their crate-root guarantees. This crate follows the
//! `bitty-winjob` precedent: the crate root denies `unsafe_code`, and only
//! the private Linux module is allowed it. Every `unsafe` block there
//! carries a `SAFETY` rationale, and the public surface is a safe API.
//!
//! No new external dependency: the only syscall dependency is the workspace
//! `libc` 0.2 line (already pinned by `bitty-pty`, already in the lockfile),
//! Linux only. There is no `landlock` crate in the tree, so the ABI v1 calls
//! go through raw `libc::syscall` numbers (`444`/`445`/`446`, exposed by
//! `libc` as `SYS_landlock_*` on every supported Linux target).
//!
//! # Posture per OS
//!
//! | OS | Mechanism | Posture |
//! |----|-----------|---------|
//! | Linux (Landlock ABI v1+, kernel 5.13+) | Landlock `EXECUTE` ruleset,
//! applied in the child via `pre_exec` `restrict_self` | Enforced; any setup
//! failure refuses the spawn fail-closed |
//! | macOS | `sandbox-exec` profile | Refused fail-closed (sequel; no
//! profile is cheap to verify without macOS CI) |
//! | Windows | Job Object / restricted token (cf. `bitty-winjob`) | Refused
//! fail-closed (sequel) |
//! | Other (Android, BSDs, …) | — | Refused fail-closed |
//!
//! [`ConfinementPosture::for_os`] is the pure, testable form of this table
//! (injected-OS-label style, mirroring the keyring backend selector).
//!
//! # Linux mechanism (ABI v1, exec-only)
//!
//! The ruleset handles exactly one access class,
//! `LANDLOCK_ACCESS_FS_EXECUTE`. One `PATH_BENEATH` rule per allowlisted
//! executable grants `EXECUTE` on that file's inode, plus one rule for its
//! ELF interpreter (the kernel checks `EXECUTE` on `PT_INTERP` when loading
//! a dynamically linked binary; shared libraries mapped later need no
//! grant). Every other exec in the confined process tree then fails with
//! `EACCES`. Reads, writes, and memory maps are unhandled, so the tool's
//! normal file I/O keeps working — only *which binaries may exec* is
//! constrained, which is precisely the shell-escape vector. Statically
//! linked tools need no interpreter rule; script tools (shebang) refuse
//! fail-closed, since their interpreter closure cannot be enumerated safely
//! (sequel work).
//!
//! Sequencing keeps post-fork work to a single syscall: the parent resolves
//! the tool through its own `PATH` ([`resolve_tool_execs`]), creates the
//! ruleset, pins each path with an `O_PATH` open plus `add_rule`, and only
//! `restrict_self` runs in the child's `pre_exec` hook. The returned
//! [`ExecConfinement`] guard owns the ruleset fd and must stay alive until
//! `Command::spawn` returns; dropping it closes the fd. Any failure before
//! the spawn returns [`SandboxError`] and no process is started.
//!
//! `PATH` handling mirrors the OS lookup the child will perform (`execvp`
//! searches the *caller's* environment, which `pre_exec` still sees, so the
//! cleared child env cannot smuggle a different lookup): only absolute,
//! non-empty `PATH` entries are searched, empty entries (POSIX for "cwd")
//! are never probed, and resolution happens in the parent before the fork.
//! A rule pins the inode open at setup time, so a swap between setup and
//! exec can only deny, never widen.
//!
//! # Non-goals
//!
//! * Command-reference secret helpers (`secret_tiers::execute_command_ref`)
//!   are deliberately *not* wired here: they are operator-configured
//!   programs (frequently shell scripts such as `pass`, needing `sh`, `gpg`,
//!   and more), so a static exec allowlist would break them. Their surface
//!   stays shell-free and bounded as before; per-helper exec profiles are
//!   sequel work. This crate confines *allowlisted tools* at the Layer-2
//!   spawn choke point (`bitty-runtime` `spawn_process`), which is the
//!   issue's subject.
//! * Generic (non-tool) execution requests carry no allowlist basis and are
//!   untouched; their scope and consent gates govern them as before.
//!
//! # Bounds (no invented values)
//!
//! * Tool names `1..=64` bytes, bare file names only (mirrors the Layer-2
//!   tool-name bound; identity policy itself stays in `bitty-plugin-host`).
//! * At most [`MAX_EXEC_PATHS`] (16) allowlisted executables per spawn.
//! * Paths longer than [`MAX_PATH_BYTES`] (4096, the `ctl` cwd bound) are
//!   skipped; at most [`MAX_PATH_SEARCH_DIRS`] (64) `PATH` entries are
//!   searched.
//! * Diagnostics are truncated to [`SANDBOX_DIAGNOSTIC_BYTES`] (128) bytes on
//!   a char boundary and quote tool names and operation tags only.
//!
//! ```rust
//! use bitty_sandbox::ConfinementPosture;
//!
//! assert!(ConfinementPosture::for_os("linux").is_enforced());
//! assert!(!ConfinementPosture::for_os("windows").is_enforced());
//! ```

#![deny(unsafe_code)]

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux;

use std::fmt;
use std::path::PathBuf;

/// Maximum bytes of a tool name accepted by [`resolve_tool_execs`].
///
/// Mirrors the Layer-2 tool-name bound (`tools::TOOL_NAME_MAX_LEN`); the
/// identity grammar itself stays in `bitty-plugin-host`.
pub const MAX_TOOL_NAME_BYTES: usize = 64;

/// Maximum allowlisted executables confined per spawn.
pub const MAX_EXEC_PATHS: usize = 16;

/// Maximum Landlock rules installed per spawn (tools plus interpreters).
///
/// Each allowlisted executable contributes at most two rules (itself plus
/// its ELF interpreter), so the table stays tiny and auditable.
pub const MAX_CONFINEMENT_RULES: usize = 32;

/// Maximum bytes of one candidate executable path.
///
/// Mirrors the `ctl` cwd bound; longer candidates are skipped, never
/// truncated into a different file.
pub const MAX_PATH_BYTES: usize = 4096;

/// Maximum `PATH` entries searched by [`resolve_tool_execs`].
pub const MAX_PATH_SEARCH_DIRS: usize = 64;

/// Maximum bytes of one [`SandboxError`] diagnostic detail.
pub const SANDBOX_DIAGNOSTIC_BYTES: usize = 128;

// ── posture ───────────────────────────────────────────────────────────────

/// Inherited-confinement posture for one operating system (CTX-1151).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfinementPosture {
    /// Landlock exec confinement is applied; setup failure refuses the spawn.
    Enforced,
    /// No inherited exec mechanism ships in this slice; admitted spawns
    /// refuse fail-closed with a diagnostic naming the sequel.
    Refused,
}

impl ConfinementPosture {
    /// Posture for an injected `std::env::consts::OS` value.
    ///
    /// Pure and testable: `"linux"` enforces, everything else refuses
    /// fail-closed (macOS `sandbox-exec` profiles and Windows
    /// Job-Object confinement are sequel work, documented above).
    #[must_use]
    pub const fn for_os(os: &str) -> Self {
        // `str` equality in `const fn` is byte comparison; match on the
        // short OS labels `std::env::consts::OS` produces (mirrors the
        // keyring backend selector shape).
        if os.len() == 5 && matches!(os.as_bytes(), b"linux") {
            Self::Enforced
        } else {
            Self::Refused
        }
    }

    /// Whether admitted spawns are confined on this posture.
    #[must_use]
    pub const fn is_enforced(self) -> bool {
        matches!(self, Self::Enforced)
    }

    /// Fail-closed diagnostic fragment for the refused posture.
    #[must_use]
    pub const fn refuse_reason(self) -> &'static str {
        match self {
            Self::Enforced => "inherited exec confinement enforced",
            Self::Refused => {
                "inherited exec confinement is Linux-only in this slice (macOS sandbox-exec / Windows job-object sequel); refusing the spawn fail-closed"
            }
        }
    }
}

impl fmt::Display for ConfinementPosture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enforced => f.write_str("enforced"),
            Self::Refused => f.write_str("refused"),
        }
    }
}

// ── error ─────────────────────────────────────────────────────────────────

/// Typed confinement failure (fail-closed, names-only diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxErrorKind {
    /// An input violated a bound (empty/oversize tool name, path separator
    /// in a tool name, empty or oversize exec list, oversize path).
    InvalidInput,
    /// The platform ships no inherited exec mechanism in this slice
    /// (macOS, Windows, other Unix).
    UnsupportedPlatform,
    /// The Linux kernel refused the Landlock ruleset (pre-5.13 kernel,
    /// disabled LSM, or a seccomp filter denying the syscalls).
    UnsupportedKernel,
    /// The tool resolved to no executable through the parent `PATH`.
    UnresolvableTool,
    /// A confinement step after a successful probe failed (rule open/add).
    SetupFailed,
}

impl SandboxErrorKind {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid-input",
            Self::UnsupportedPlatform => "unsupported-platform",
            Self::UnsupportedKernel => "unsupported-kernel",
            Self::UnresolvableTool => "unresolvable-tool",
            Self::SetupFailed => "setup-failed",
        }
    }
}

impl fmt::Display for SandboxErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Confinement failure: kind plus a bounded, names-only detail.
///
/// The detail quotes tool names, operation tags, and system paths only —
/// never secrets, tokens, or child output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxError {
    kind: SandboxErrorKind,
    detail: String,
}

impl SandboxError {
    /// Build an error, truncating `detail` to [`SANDBOX_DIAGNOSTIC_BYTES`]
    /// bytes on a char boundary.
    #[must_use]
    pub fn new(kind: SandboxErrorKind, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self {
            kind,
            detail: truncate_detail(&detail),
        }
    }

    /// Machine-readable failure kind.
    #[must_use]
    pub const fn kind(&self) -> SandboxErrorKind {
        self.kind
    }

    /// Bounded human diagnostic (tool names and operation tags only).
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Truncate to [`SANDBOX_DIAGNOSTIC_BYTES`] bytes on a char boundary.
fn truncate_detail(detail: &str) -> String {
    if detail.len() <= SANDBOX_DIAGNOSTIC_BYTES {
        return detail.to_owned();
    }
    let mut end = SANDBOX_DIAGNOSTIC_BYTES;
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail[..end].to_owned()
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tool spawn confinement {}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for SandboxError {}

// ── resolution ────────────────────────────────────────────────────────────

/// Resolve a bare `tool` name to absolute executable paths via the parent
/// `PATH`.
///
/// Only absolute, non-empty `PATH` entries are searched (empty entries mean
/// "cwd" in POSIX and are never probed); candidates must stat as files.
/// At most [`MAX_EXEC_PATHS`] hits are returned, deduplicated. An empty hit
/// set fails as [`SandboxErrorKind::UnresolvableTool`] — the caller must
/// refuse the spawn, since there is nothing to confine it to.
///
/// Identity policy (which tools exist) stays in `bitty-plugin-host`; this
/// function enforces only path safety (bare name, bounds), so the test seam
/// can resolve fixture tools (`sh`, `true`) with the production code path.
pub fn resolve_tool_execs(tool: &str) -> Result<Vec<PathBuf>, SandboxError> {
    validate_tool_name(tool)?;
    let mut found: Vec<PathBuf> = Vec::new();
    let mut searched = 0usize;
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if searched >= MAX_PATH_SEARCH_DIRS {
                break;
            }
            searched += 1;
            if dir.as_os_str().is_empty() || !dir.is_absolute() {
                continue;
            }
            let candidate = dir.join(tool);
            if candidate.as_os_str().len() > MAX_PATH_BYTES {
                continue;
            }
            let is_file = std::fs::metadata(&candidate)
                .map(|meta| meta.is_file())
                .unwrap_or(false);
            if !is_file {
                continue;
            }
            if !found.contains(&candidate) {
                found.push(candidate);
            }
            if found.len() >= MAX_EXEC_PATHS {
                break;
            }
        }
    }
    if found.is_empty() {
        return Err(SandboxError::new(
            SandboxErrorKind::UnresolvableTool,
            format!("tool '{tool}' resolved to no executable in PATH"),
        ));
    }
    Ok(found)
}

/// Bare-name path safety for a tool label (bounds, no separators, no NUL).
///
/// This is path safety only, not the Layer-2 identity grammar: the caller
/// (the tools gate) owns which names are admitted.
fn validate_tool_name(tool: &str) -> Result<(), SandboxError> {
    if tool.is_empty() || tool.len() > MAX_TOOL_NAME_BYTES {
        return Err(SandboxError::new(
            SandboxErrorKind::InvalidInput,
            "tool name must be 1..=64 bytes",
        ));
    }
    if tool.contains('/') || tool.contains('\\') || tool.contains('\0') {
        return Err(SandboxError::new(
            SandboxErrorKind::InvalidInput,
            "tool name must be a bare file name",
        ));
    }
    Ok(())
}

// ── enforcement ───────────────────────────────────────────────────────────

/// Live confinement for one child command: owns the Landlock ruleset fd on
/// Linux and must stay alive until `Command::spawn` returns.
///
/// Dropping closes the ruleset fd. The child's `pre_exec` hook captured
/// only the fd *number*; the parent's guard keeps that number valid across
/// the fork, and `CLOEXEC` (which Landlock fds carry by design) keeps it
/// out of the exec'd program.
#[derive(Debug)]
pub struct ExecConfinement {
    // Held for the `Drop` close: the ruleset fd number was copied into the
    // child `pre_exec` hook, and the parent copy must stay open across the
    // fork, so the guard owns it until after `Command::spawn` returns.
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    ruleset: linux::RulesetGuard,
}

/// Confine `command` to exec only `exec_paths`, fail-closed.
///
/// Validates the exec list on every platform (empty, overlong, NUL-bearing
/// entries deny as [`SandboxErrorKind::InvalidInput`]), then on Linux
/// installs the Landlock `EXECUTE` ruleset with a `pre_exec`
/// `restrict_self` hook and returns the [`ExecConfinement`] guard. On any
/// other OS returns [`SandboxErrorKind::UnsupportedPlatform`] — the caller
/// must refuse the spawn. On any failure no process is started; the caller
/// must propagate the error *instead of* spawning.
pub fn confine_command_to_execs(
    command: &mut std::process::Command,
    exec_paths: &[PathBuf],
) -> Result<ExecConfinement, SandboxError> {
    validate_exec_paths(exec_paths)?;
    #[cfg(target_os = "linux")]
    {
        linux::apply(command, exec_paths)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = command;
        Err(SandboxError::new(
            SandboxErrorKind::UnsupportedPlatform,
            ConfinementPosture::for_os(std::env::consts::OS).refuse_reason(),
        ))
    }
}

/// Bounds and NUL safety for one confinement exec list (every platform).
fn validate_exec_paths(exec_paths: &[PathBuf]) -> Result<(), SandboxError> {
    if exec_paths.is_empty() {
        return Err(SandboxError::new(
            SandboxErrorKind::InvalidInput,
            "confinement exec list must not be empty",
        ));
    }
    if exec_paths.len() > MAX_EXEC_PATHS {
        return Err(SandboxError::new(
            SandboxErrorKind::InvalidInput,
            "confinement exec list exceeds 16 entries",
        ));
    }
    for path in exec_paths {
        if path.as_os_str().is_empty() || path.as_os_str().len() > MAX_PATH_BYTES {
            return Err(SandboxError::new(
                SandboxErrorKind::InvalidInput,
                "confinement exec path must be 1..=4096 bytes",
            ));
        }
        // Portable NUL check (`Path` has no portable byte view; the
        // lossy form preserves NUL on every platform).
        if path.to_string_lossy().contains('\0') {
            return Err(SandboxError::new(
                SandboxErrorKind::InvalidInput,
                "confinement exec path must not contain NUL",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posture_enforces_linux_only() {
        assert!(ConfinementPosture::for_os("linux").is_enforced());
        assert_eq!(ConfinementPosture::for_os("linux").to_string(), "enforced");
        for os in ["macos", "windows", "android", "freebsd", "plan9", ""] {
            assert!(
                !ConfinementPosture::for_os(os).is_enforced(),
                "{os} must refuse"
            );
        }
        assert_eq!(ConfinementPosture::for_os("windows").to_string(), "refused");
        assert!(
            ConfinementPosture::for_os("windows")
                .refuse_reason()
                .contains("fail-closed"),
            "refusal must name the fail-closed degrade"
        );
    }

    #[test]
    fn error_detail_truncates_on_char_boundary() {
        let long = "x".repeat(SANDBOX_DIAGNOSTIC_BYTES + 40);
        let error = SandboxError::new(SandboxErrorKind::SetupFailed, long);
        assert!(error.detail().len() <= SANDBOX_DIAGNOSTIC_BYTES);
        assert_eq!(error.kind(), SandboxErrorKind::SetupFailed);
        let text = error.to_string();
        assert!(text.contains("setup-failed"), "{text}");
        assert!(text.contains("tool spawn confinement"), "{text}");
        // Multi-byte truncation never splits a char.
        let wide = "é".repeat(SANDBOX_DIAGNOSTIC_BYTES);
        let error = SandboxError::new(SandboxErrorKind::SetupFailed, wide);
        assert!(error.detail().len() <= SANDBOX_DIAGNOSTIC_BYTES);
    }

    #[test]
    fn resolve_rejects_non_bare_names() {
        for bad in [
            "",
            "/usr/bin/git",
            "./git",
            "../evil",
            "git\\evil",
            "has\0nul",
        ] {
            let error = resolve_tool_execs(bad).expect_err("non-bare name must deny");
            assert_eq!(error.kind(), SandboxErrorKind::InvalidInput, "{bad}");
        }
        let long = "t".repeat(MAX_TOOL_NAME_BYTES + 1);
        let error = resolve_tool_execs(&long).expect_err("oversize name must deny");
        assert_eq!(error.kind(), SandboxErrorKind::InvalidInput);
    }

    #[test]
    fn resolve_missing_tool_is_unresolvable() {
        let error = resolve_tool_execs("bitty-definitely-missing-tool-xyz")
            .expect_err("missing tool must deny");
        assert_eq!(error.kind(), SandboxErrorKind::UnresolvableTool);
        assert!(
            error
                .to_string()
                .contains("bitty-definitely-missing-tool-xyz")
        );
    }

    #[test]
    fn confine_rejects_bad_exec_lists_everywhere() {
        let mut command = std::process::Command::new("bitty-definitely-missing-tool-xyz");
        let error =
            confine_command_to_execs(&mut command, &[]).expect_err("empty exec list must deny");
        assert_eq!(error.kind(), SandboxErrorKind::InvalidInput);
        let long = vec![PathBuf::from("x".repeat(MAX_PATH_BYTES + 1)); 1];
        let error =
            confine_command_to_execs(&mut command, &long).expect_err("oversize path must deny");
        assert_eq!(error.kind(), SandboxErrorKind::InvalidInput);
        let many = vec![PathBuf::from("git"); MAX_EXEC_PATHS + 1];
        let error = confine_command_to_execs(&mut command, &many)
            .expect_err("overlong exec list must deny");
        assert_eq!(error.kind(), SandboxErrorKind::InvalidInput);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_probe_succeeds_on_supported_kernel() {
        // Hard requirement, not a skip: Linux CI (GitHub runners, the
        // `bitty-act` image on a modern host kernel) carries Landlock
        // ABI v1+; without it every admitted spawn refuses fail-closed.
        linux::probe_supported().expect("Landlock ABI v1+ must be present on Linux CI");
    }

    /// Spawn `command`, skipping the test when the host kernel forbids
    /// *unprivileged* `restrict_self` (raw `EPERM` from the child `pre_exec`
    /// hook after a successful setup).
    ///
    /// Setup already proved the Landlock numbers, the LSM, and the rule
    /// calls cooperate; a spawn-time `EPERM` therefore names the host, not
    /// this code — typically a container/BPF policy that gates
    /// self-confinement on privilege (observed on locked-down dev
    /// sandboxes; GitHub-hosted runners enforce normally). The skip prints
    /// loudly so capability-skips never pass silently, and the same tests
    /// verify for real wherever the host permits (including under
    /// `unshare -rm` locally).
    ///
    /// Any other spawn error panics: in particular a raw `EACCES` here is
    /// meaningful (the confined exec itself was denied — the mechanism
    /// working), never a skip.
    #[cfg(target_os = "linux")]
    fn spawn_or_skip_restrict_blocked(
        mut command: std::process::Command,
    ) -> Option<std::process::Output> {
        match command.output() {
            Ok(out) => Some(out),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                eprintln!(
                    "SKIP: host forbids unprivileged landlock_restrict_self ({error}); re-run under `unshare -rm` or on CI for full verification"
                );
                None
            }
            Err(error) => panic!("confined spawn failed unexpectedly: {error:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn admitted_exec_runs_under_confinement() {
        // Control: the allowlisted binary itself still execs under the
        // ruleset (reads/maps are unhandled, only exec is constrained).
        let sh = resolve_tool_execs("sh").expect("PATH must resolve sh on Linux CI");
        let mut command = std::process::Command::new(&sh[0]);
        command.arg("-c").arg("exit 0");
        let _guard = confine_command_to_execs(&mut command, &sh).expect("confinement sets up");
        let Some(out) = spawn_or_skip_restrict_blocked(command) else {
            return;
        };
        assert!(
            out.status.success(),
            "allowlisted exec must run, got {}",
            out.status
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shell_escape_denied_at_os_level() {
        // The CTX-1151 acceptance shape: an admitted binary (here `sh`,
        // standing in for the allowlisted tool via the test seam) attempts
        // to exec outside the allowlist, and the OS denies it. The escape
        // target travels as `$0` (no path quoting games); the confined run
        // must report the exec failure while the unconfined control runs it.
        let sh = resolve_tool_execs("sh").expect("PATH must resolve sh on Linux CI");
        let escape = resolve_tool_execs("true").expect("PATH must resolve true on Linux CI");
        let script = "\"$0\"; echo \"REACHED:$?\"";

        let mut confined = std::process::Command::new(&sh[0]);
        confined.arg("-c").arg(script).arg(&escape[0]);
        let _guard = confine_command_to_execs(&mut confined, &sh).expect("confinement sets up");
        let Some(out) = spawn_or_skip_restrict_blocked(confined) else {
            return;
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("REACHED:12"),
            "confined inner exec must fail with 126/127, got status={} stdout={stdout:?}",
            out.status
        );

        let mut control = std::process::Command::new(&sh[0]);
        control.arg("-c").arg(script).arg(&escape[0]);
        let out = control.output().expect("control spawn starts");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("REACHED:0"),
            "unconfined control must run the target, got status={} stdout={stdout:?}",
            out.status
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_allowlisted_initial_exec_denied() {
        // Even the first exec is checked: a binary outside the allowlist
        // never starts. The kernel's `EACCES` surfaces as a spawn error
        // (Rust reports child exec failure through the spawn pipe, like
        // `ENOENT` for missing binaries), so a raw `EACCES` here *is* the
        // OS-level denial — while a raw `EPERM` names a host that forbids
        // unprivileged `restrict_self` and skips (see the helper above).
        let sh = resolve_tool_execs("sh").expect("PATH must resolve sh on Linux CI");
        let other = resolve_tool_execs("true").expect("PATH must resolve true on Linux CI");
        let mut command = std::process::Command::new(&other[0]);
        let _guard = confine_command_to_execs(&mut command, &sh).expect("confinement sets up");
        match command.output() {
            Err(error) if error.raw_os_error() == Some(libc::EACCES) => {}
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                eprintln!(
                    "SKIP: host forbids unprivileged landlock_restrict_self ({error}); re-run under `unshare -rm` or on CI for full verification"
                );
            }
            Err(error) => panic!("expected EACCES denial, got {error:?}"),
            Ok(out) => assert!(
                !out.status.success(),
                "non-allowlisted exec must be denied at OS level"
            ),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn elf_interp_finds_loader_for_dynamic_tools() {
        let sh = resolve_tool_execs("sh").expect("PATH must resolve sh on Linux CI");
        let interp = linux::elf_interp(&sh[0]).expect("sh must name an interpreter");
        assert!(
            interp.to_string_lossy().contains("ld-linux"),
            "unexpected interpreter {interp:?}"
        );
        // Scripts name no ELF interpreter: they refuse fail-closed at exec
        // rather than gaining an unenforceable rule.
        let dir = std::env::temp_dir().join(format!(
            "bitty-sandbox-interp-{}-{}",
            std::process::id(),
            "probe"
        ));
        std::fs::create_dir_all(&dir).expect("temp dir must build");
        let script = dir.join("probe.sh");
        // pty-gate-exempt: inert script fixture for elf_interp; never executed.
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("fixture must write");
        assert_eq!(linux::elf_interp(&script), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn setup_failure_refuses_before_any_spawn() {
        // An exec list with no openable file fails in the parent during
        // setup; the caller must propagate instead of spawning (no child
        // can exist: `Command::spawn` is never reached on this path).
        let mut command = std::process::Command::new("bitty-definitely-missing-tool-xyz");
        let missing = PathBuf::from("/no/such/bitty-sandbox-target");
        let error = confine_command_to_execs(&mut command, &[missing])
            .expect_err("unopenable exec target must deny at setup");
        assert_eq!(error.kind(), SandboxErrorKind::SetupFailed);
    }
}
