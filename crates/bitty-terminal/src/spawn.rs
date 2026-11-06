//! Default shell resolution and spawn helpers (CTX-0136, bitty#2928).

use std::fmt;

use bitty_runtime::{Runtime, SplitAxis, ViewId};

// ---------------------------------------------------------------------------
// Default shell resolution (CTX-0136, bitty#2928)
// ---------------------------------------------------------------------------

/// POSIX fallback shell: the last Unix layer of the default chain and the
/// retry target for a configured or `$SHELL` program that cannot spawn.
///
/// Windows has no `/bin/sh`; the last layer there is the first
/// [`PLATFORM_SHELL_CANDIDATES`] entry that resolves on `PATH` (bitty#2928).
pub(crate) const FALLBACK_SHELL: &str = "/bin/sh";

/// Ordered platform default candidates for the child shell (bitty#2928).
///
/// A fresh Windows install sets neither `terminal.shell` nor `$SHELL`, so the
/// chain must end in a program that exists there: PowerShell 7+ when
/// installed, else Windows PowerShell, else the command interpreter every
/// Windows host ships. Unix keeps the POSIX fallback, unchanged.
#[cfg(windows)]
pub(crate) const PLATFORM_SHELL_CANDIDATES: &[&str] = &["pwsh.exe", "powershell.exe", "cmd.exe"];
#[cfg(not(windows))]
pub(crate) const PLATFORM_SHELL_CANDIDATES: &[&str] = &[FALLBACK_SHELL];

/// Which precedence layer chose the default program (bitty#2928).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellSource {
    /// Explicit CLI `PROGRAM`, used verbatim.
    Explicit,
    /// Configured `terminal.shell`.
    Configured,
    /// `$SHELL`.
    Environment,
    /// Platform default candidate resolved on `PATH`.
    PlatformDefault,
}

impl ShellSource {
    /// Stable label for logs, `bitty doctor`, and failure messages.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Explicit => "cli program",
            Self::Configured => "terminal.shell",
            Self::Environment => "$SHELL",
            Self::PlatformDefault => "platform default",
        }
    }
}

/// The resolved default child program plus the layer that chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedShell {
    /// Direct `argv[0]`; absolute when it came from a `PATH` lookup.
    pub(crate) program: String,
    /// Precedence layer that chose `program`.
    pub(crate) source: ShellSource,
}

/// Typed failure: no candidate in the default chain resolved to a program.
///
/// Reachable only on Windows: a Unix host's platform default is the POSIX
/// fallback, returned without a filesystem probe exactly as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnresolvedDefaultShell {
    /// Candidates tried, in order.
    pub(crate) tried: Vec<&'static str>,
}

impl fmt::Display for UnresolvedDefaultShell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no default child shell: none of ")?;
        for (index, candidate) in self.tried.iter().enumerate() {
            if index > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{candidate:?}")?;
        }
        write!(
            f,
            " resolved on PATH; set terminal.shell or pass a program argument"
        )
    }
}

impl std::error::Error for UnresolvedDefaultShell {}

/// Validates a configured `terminal.shell` value as a direct `argv[0]`.
///
/// The effective config is validated at load (trimmed non-empty, no control
/// characters, <= [`bitty_config::types::MAX_SHELL_LEN`] bytes); this
/// re-checks defensively so a hand-built `EffectiveConfig` can never route a
/// blank, oversized, or control-laden value to execve. `None` fails closed
/// to the next precedence layer. Never split, joined, or interpolated: the
/// result is a direct `argv[0]` (CTX-0298).
pub(crate) fn configured_shell_argv0(configured: Option<&str>) -> Option<&str> {
    let trimmed = configured?.trim();
    if trimmed.is_empty()
        || trimmed.len() > bitty_config::types::MAX_SHELL_LEN
        || trimmed.chars().any(char::is_control)
    {
        return None;
    }
    Some(trimmed)
}

/// Picks the default program from injected values (bitty#2928).
///
/// Precedence: configured `terminal.shell` > `$SHELL` > the first entry in
/// `candidates` that `lookup` resolves. Pure and total: the caller injects
/// both the candidate list and the lookup, so tests never touch the
/// environment, the filesystem, or the network. Every candidate is trimmed
/// but never split, joined, or interpolated; `$SHELL` is trusted only as a
/// binary path, and a blank or control-laden configured value fails closed
/// to `$SHELL` (CTX-0298).
pub(crate) fn select_default_shell(
    config_shell: Option<&str>,
    shell_env: Option<&str>,
    candidates: &[&'static str],
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedShell, UnresolvedDefaultShell> {
    if let Some(configured) = configured_shell_argv0(config_shell) {
        return Ok(ResolvedShell {
            program: configured.to_string(),
            source: ShellSource::Configured,
        });
    }
    if let Some(env) = shell_env.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(ResolvedShell {
            program: env.to_string(),
            source: ShellSource::Environment,
        });
    }
    for candidate in candidates {
        if let Some(program) = lookup(candidate) {
            return Ok(ResolvedShell {
                program,
                source: ShellSource::PlatformDefault,
            });
        }
    }
    Err(UnresolvedDefaultShell {
        tried: candidates.to_vec(),
    })
}

/// Resolves the default child program for this host (bitty#2928):
/// configured `terminal.shell` > `$SHELL` > platform default.
///
/// The platform default is [`FALLBACK_SHELL`] on Unix (returned as-is, no
/// filesystem probe — byte-identical to the historical chain) and the first
/// [`PLATFORM_SHELL_CANDIDATES`] entry on `PATH` on Windows, resolved with
/// the same bounded lookup `bitty doctor` uses.
pub(crate) fn resolve_default_shell(
    config_shell: Option<&str>,
    shell_env: Option<&str>,
) -> Result<ResolvedShell, UnresolvedDefaultShell> {
    select_default_shell(
        config_shell,
        shell_env,
        PLATFORM_SHELL_CANDIDATES,
        platform_lookup,
    )
}

/// Resolves the platform default candidate `name` to a spawnable program.
///
/// Unix: the POSIX fallback is returned unchanged, so a missing `/bin/sh`
/// still surfaces as a spawn failure instead of a resolution failure.
/// Windows: bounded `PATH` search, so a bare candidate name is only used
/// when it actually exists.
fn platform_lookup(name: &str) -> Option<String> {
    #[cfg(windows)]
    {
        let path_env = std::env::var("PATH").ok();
        crate::doctor::find_on_path_with(path_env.as_deref(), name)
            .map(|path| path.to_string_lossy().into_owned())
    }
    #[cfg(not(windows))]
    {
        Some(name.to_string())
    }
}

/// Resolves the program to spawn: an explicit `program` unchanged when
/// present, else the default shell chain (configured `terminal.shell` >
/// `$SHELL` > platform default).
///
pub(crate) fn resolve_spawn_program(
    program: Option<&str>,
    config_shell: Option<&str>,
    shell_env: Option<&str>,
) -> Result<ResolvedShell, UnresolvedDefaultShell> {
    match program {
        Some(program) => Ok(ResolvedShell {
            program: program.to_string(),
            source: ShellSource::Explicit,
        }),
        None => resolve_default_shell(config_shell, shell_env),
    }
}

/// The retry target for a configured or `$SHELL` program that cannot spawn:
/// the platform default on its own (bitty#2928).
fn platform_default_shell() -> Result<ResolvedShell, UnresolvedDefaultShell> {
    select_default_shell(None, None, PLATFORM_SHELL_CANDIDATES, platform_lookup)
}

/// Why the startup shell did not start (bitty#2928).
#[derive(Debug)]
pub(crate) enum StartupShellError {
    /// The default chain resolved to nothing; startup cannot continue.
    Unresolved(UnresolvedDefaultShell),
    /// The program resolved but the PTY spawn failed.
    Spawn {
        /// Program attempted last (direct `argv[0]`).
        program: String,
        /// Underlying runtime failure.
        error: bitty_runtime::RuntimeError,
    },
}

impl StartupShellError {
    /// True when startup must stop instead of continuing without a child: an
    /// unresolvable default is never a headless-smoke case (bitty#2928).
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(self, Self::Unresolved(_))
    }
}

impl fmt::Display for StartupShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolved(err) => err.fmt(f),
            Self::Spawn { program, error } => {
                write!(f, "child shell {program:?} failed to start: {error}")
            }
        }
    }
}

impl std::error::Error for StartupShellError {}

impl From<UnresolvedDefaultShell> for StartupShellError {
    fn from(value: UnresolvedDefaultShell) -> Self {
        Self::Unresolved(value)
    }
}

/// Spawns the default shell chain (configured `terminal.shell`, else
/// `$SHELL`, else the platform default) inside `runtime`.
///
/// Tries the resolved default first; when it differs from the platform
/// default and its spawn fails, retries once with the platform default
/// before surfacing the error. Callers decide whether to continue without a
/// child: an unresolvable default is fatal, a failed spawn is fail-soft
/// unless `--fail-loud` (CTX-0481).
pub(crate) fn spawn_default_shell(
    runtime: &mut Runtime,
    config_shell: Option<&str>,
    shell_env: Option<&str>,
) -> Result<(), StartupShellError> {
    let resolved = resolve_default_shell(config_shell, shell_env)?;
    let fallback = platform_default_shell()?;
    spawn_with_fallback(
        |candidate| runtime.spawn_shell(candidate),
        &resolved.program,
        &fallback.program,
    )
    .map_err(|(program, error)| StartupShellError::Spawn { program, error })
}

/// Spawn core with the startup fallback chain: try `default` first; when it
/// differs from `fallback` and fails, retry once with `fallback` before
/// surfacing the error. The `spawn` closure performs the direct-argv exec
/// into the target session (primary shell or one split pane). Returns the
/// program attempted last alongside its failure so the caller can name it.
fn spawn_with_fallback(
    mut spawn: impl FnMut(&str) -> Result<(), bitty_runtime::RuntimeError>,
    default: &str,
    fallback: &str,
) -> Result<(), (String, bitty_runtime::RuntimeError)> {
    match spawn(default) {
        Ok(()) => {
            crate::logging::info(|| format!("bitty: spawned default shell {default:?}"));
            Ok(())
        }
        Err(err) if default != fallback => {
            crate::logging::warn(|| {
                format!(
                    "bitty: spawn_shell({default:?}) failed: {err} — trying fallback {fallback:?}"
                )
            });
            match spawn(fallback) {
                Ok(()) => {
                    crate::logging::info(|| format!("bitty: spawned fallback shell {fallback:?}"));
                    Ok(())
                }
                Err(fallback_err) => {
                    crate::logging::warn(|| {
                        format!("bitty: spawn_shell({fallback:?}) failed: {fallback_err}")
                    });
                    Err((fallback.to_string(), fallback_err))
                }
            }
        }
        Err(err) => {
            crate::logging::warn(|| format!("bitty: spawn_shell({default:?}) failed: {err}"));
            Err((default.to_string(), err))
        }
    }
}

/// Frozen spawn recipe so every split leaf replays the exact startup
/// resolution (explicit program verbatim, else the default-shell chain:
/// configured `terminal.shell` > `$SHELL` > platform default, bitty#2928).
/// Captured once at startup from CLI args + effective config + `$SHELL`;
/// values are direct argv throughout, never split, joined, or interpolated.
#[derive(Debug, Clone, Default)]
pub(crate) struct SpawnSpec {
    pub(crate) program: Option<String>,
    pub(crate) program_args: Vec<String>,
    pub(crate) shell_env: Option<String>,
    pub(crate) config_shell: Option<String>,
}

impl SpawnSpec {
    /// Resolves `(program, args)` exactly as startup does: the explicit
    /// program wins verbatim with its tail args, otherwise the default shell
    /// chain from the injected configured shell and `$SHELL` values. The
    /// platform default layer resolves on `PATH` on Windows, so a pane can
    /// fail to resolve only when startup itself would have failed.
    pub(crate) fn resolve(&self) -> Result<(String, Vec<String>), UnresolvedDefaultShell> {
        match self.program.as_deref() {
            Some(program) => Ok((program.to_string(), self.program_args.clone())),
            None => resolve_default_shell(self.config_shell.as_deref(), self.shell_env.as_deref())
                .map(|resolved| (resolved.program, Vec::new())),
        }
    }
}

/// Spawns the [`SpawnSpec`] program as leaf `view`'s private shell, sized to
/// `cols` x `rows` cells (CTX-0176). Same sandbox as startup: direct argv,
/// explicit program verbatim with no fallback, default shell with the
/// platform-default retry. Failures are logged by the fallback core and
/// returned so the caller degrades loudly: the pane then stays empty
/// (CTX-0359: never a silent mirror of another pane's grid).
pub(crate) fn spawn_pane_shell(
    runtime: &mut Runtime,
    spec: &SpawnSpec,
    view: ViewId,
    cols: u16,
    rows: u16,
) -> Result<(), bitty_runtime::RuntimeError> {
    spawn_pane_shell_in(runtime, spec, view, cols, rows, None)
}

/// [`spawn_pane_shell`] with an explicit working directory (#1528,
/// `bitty ctl terminal spawn --cwd`). `Some` replaces the `OSC 7`
/// inheritance and is re-checked at the spawn boundary; the fallback retry
/// uses the same directory, so no candidate starts anywhere else.
pub(crate) fn spawn_pane_shell_in(
    runtime: &mut Runtime,
    spec: &SpawnSpec,
    view: ViewId,
    cols: u16,
    rows: u16,
    cwd: Option<&std::path::Path>,
) -> Result<(), bitty_runtime::RuntimeError> {
    let (program, args) = spec.resolve().map_err(|err| {
        // Startup already resolved this chain, so a failure here is a
        // late-environment change (a removed `PATH` entry). The pane stays
        // empty and the message names the candidates that were tried.
        crate::logging::warn(|| format!("bitty: pane shell not spawned: {err}"));
        bitty_runtime::RuntimeError::Pty(err.to_string())
    })?;
    if spec.program.is_some() {
        // Explicit program: verbatim, no fallback (startup parity).
        let tail: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        return runtime.spawn_shell_for_view_in(view, &program, &tail, cols, rows, cwd);
    }
    let fallback = platform_default_shell().map_err(|err| {
        crate::logging::warn(|| format!("bitty: pane shell not spawned: {err}"));
        bitty_runtime::RuntimeError::Pty(err.to_string())
    })?;
    spawn_with_fallback(
        |candidate| runtime.spawn_shell_for_view_in(view, candidate, &[], cols, rows, cwd),
        &program,
        &fallback.program,
    )
    .map_err(|(_program, error)| error)
}

/// Spawns a private shell for every layout leaf except the focused one
/// (CTX-0176), which keeps the already-spawned primary session, plus every
/// restored attached pin (CTX-1082). Each pane shell is sized to its
/// **decorated present frame** (the cells actually painted), not the larger
/// raw slot allocation; each pinned shell uses the same composited frame.
/// Best-effort: per-leaf failures warn loudly and leave that pane empty
/// (CTX-0359: never a silent mirror of another pane's grid). Call only after
/// a successful primary spawn.
///
/// CTX-1707 (#2896): sizing a shell to the raw allocation makes the PTY
/// advertise rows/columns the painter never shows (decoration consumes part
/// of the slot), so a full-screen TUI loses its top rows until an unrelated
/// geometry change re-syncs the pane. This is the same rule the keyboard
/// split path (`commit_new_leaf`) and the pinned-pane loop below already
/// follow; the allocation is only a degenerate-container fallback.
///
/// Returns the number of panes whose shell failed (CTX-0481): the
/// `--fail-loud` startup path turns any non-zero count into a non-zero
/// exit instead of the default warning-only continuation.
pub(crate) fn spawn_startup_pane_shells(runtime: &mut Runtime, spec: &SpawnSpec) -> usize {
    let primary = runtime.focused_view();
    let allocs = runtime.layout_allocations();
    let frames = runtime.present_frames();
    let mut failed = 0usize;
    for (id, rect) in &allocs {
        if Some(*id) == primary {
            continue;
        }
        // CW-15: a restored session respawns only attached leaves (pending
        // restores). Session-less (`Detached`) leaves carry no pending
        // entry and stay empty by design instead of gaining shells they
        // never had. Fresh starts still spawn every leaf.
        if runtime.session_restored() && !runtime.session_pending_contains(id) {
            continue;
        }
        // CTX-1707: the decorated content frame is the painted grid. Fall
        // back to the allocation only when the leaf has no frame at all
        // (degenerate container); that keeps the startup path total.
        let (cols, rows) = frames
            .iter()
            .find(|frame| frame.view == *id)
            .map(|frame| (frame.cols.max(1), frame.rows.max(1)))
            .unwrap_or((rect.width.max(1), rect.height.max(1)));
        if let Err(err) = spawn_pane_shell(runtime, spec, *id, cols, rows) {
            crate::logging::warn(|| {
                format!(
                    "warning: startup pane {id:?} shell spawn failed ({err}) — pane stays empty"
                )
            });
            failed += 1;
        }
    }
    // CTX-1082: restored attached pins live in no layout, so the
    // allocation loop above never sees them — yet they composite over the
    // active scene at startup. Respawn each pending one sized to its
    // composited frame (present frames include pins). Fresh starts skip:
    // the pinned store is empty then, and session-less pins carry no
    // pending entry by design. Unpinned ids rejoin the layout loop above,
    // so no path changes there. Pinned ids stay out of the layout filter:
    // they are spawned here, by identity, never as layout leaves.
    if runtime.session_restored() {
        let frames = runtime.present_frames();
        for id in runtime.pinned_views() {
            if !runtime.session_pending_contains(&id) || runtime.has_pane_session(&id) {
                continue;
            }
            let Some(frame) = frames.iter().find(|frame| frame.view == id) else {
                // Degenerate container (no frames at all): the deferred
                // switch path retries with layout-independent fallback dims.
                continue;
            };
            if let Err(err) =
                spawn_pane_shell(runtime, spec, id, frame.cols.max(1), frame.rows.max(1))
            {
                crate::logging::warn(|| {
                    format!(
                        "warning: startup pinned pane {id:?} shell spawn failed ({err}) — pane stays empty"
                    )
                });
                failed += 1;
            }
        }
    }
    failed
}

/// True when `token` looks like a negative number (`-5`, `-0.1`) rather
/// than a flag (`-v`, `--headless`): a leading `-` followed by a digit or
/// `.`. Lets `--font-size -5` / `--opacity -0.1` reach merge-time validation
/// (fail-closed) instead of being mistaken for a missing value.
pub(crate) fn looks_like_negative_number(token: &str) -> bool {
    let mut chars = token.chars();
    if chars.next() != Some('-') {
        return false;
    }
    matches!(chars.next(), Some(c) if c.is_ascii_digit() || c == '.')
}

pub(crate) fn parse_split_axis(s: &str) -> Option<SplitAxis> {
    match s.to_ascii_lowercase().as_str() {
        "horizontal" | "h" | "horiz" | "hor" => Some(SplitAxis::Horizontal),
        "vertical" | "v" | "vert" | "ver" => Some(SplitAxis::Vertical),
        _ => None,
    }
}

/// Parses a value that may be `axis` or `axis:ratio` (e.g. "h:0.3", "vertical:0.7").
pub(crate) fn parse_split_token(token: &str) -> (Option<SplitAxis>, Option<f32>) {
    if let Some((axis_part, ratio_part)) = token.split_once(':') {
        let axis = parse_split_axis(axis_part.trim());
        let ratio = ratio_part.trim().parse::<f32>().ok();
        (axis, ratio)
    } else {
        (parse_split_axis(token.trim()), None)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use bitty_platform::PhysicalSize;
    use bitty_runtime::{LayoutNode, RuntimeConfig, SplitAxis, UiRect, View, ViewId};

    fn split_runtime() -> Runtime {
        let mut rt = Runtime::new(RuntimeConfig {
            cell_width: 10,
            cell_height: 22,
            window_padding: 8,
            ..RuntimeConfig::default()
        })
        .expect("runtime builds");
        // Fractional HiDPI window: a two-pane container with decoration.
        rt.apply_dpi_scale(1.6, Some(PhysicalSize::new(2505, 1460)));
        rt.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 77, 41)),
            LayoutNode::leaf(View::new(ViewId::new(2), 77, 41)),
        ));
        rt.set_window_cells(UiRect::new(0, 0, 155, 41));
        rt.reflow_layout();
        rt
    }

    #[test]
    fn startup_pane_shells_size_to_the_painted_frame_not_the_slot() {
        // CTX-1707 / issue #2896: the decorated content frame is smaller
        // than the raw allocation (outer gap + border + content inset), so
        // sizing the shell to the allocation advertises rows the painter
        // windows away and a full-screen TUI loses its top lines.
        #[cfg(unix)]
        bitty_test_support::require_pty!();
        let mut rt = split_runtime();
        let focused = rt.focused_view().expect("focused leaf");
        let frames = rt.present_frames();
        let allocs = rt.layout_allocations();
        assert_eq!(frames.len(), 2);
        assert_eq!(allocs.len(), 2);

        let spec = SpawnSpec {
            program: Some("/bin/sh".to_string()),
            program_args: vec!["-c".to_string(), "sleep 2".to_string()],
            shell_env: None,
            config_shell: None,
        };
        assert_eq!(
            spawn_startup_pane_shells(&mut rt, &spec),
            0,
            "the test shell must spawn"
        );

        for frame in &frames {
            if frame.view == focused {
                continue;
            }
            let (alloc_w, alloc_h) = allocs
                .iter()
                .find(|(id, _)| *id == frame.view)
                .map(|(_, rect)| (rect.width, rect.height))
                .expect("allocation");
            assert!(
                (frame.cols, frame.rows) != (alloc_w, alloc_h),
                "the fixture must exercise the decoration inset"
            );
            assert_eq!(
                rt.pane_pty_size(&frame.view),
                Some((frame.cols, frame.rows)),
                "the pane PTY must match its painted grid"
            );
        }
    }
}
