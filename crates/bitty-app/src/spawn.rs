//! Default shell resolution and spawn helpers (CTX-0136).

use bitty_runtime::{Runtime, SplitAxis, ViewId};

use crate::cli::Args;

// ---------------------------------------------------------------------------
// Default shell resolution (CTX-0136)
// ---------------------------------------------------------------------------

/// Fallback shell when `$SHELL` is unset or blank.
///
/// POSIX default. Windows keeps the same fallback for now; the ConPTY default
/// slice may refine this without changing the resolver contract (pure/total,
/// no env/fs access — the caller injects `$SHELL`).
pub(crate) const FALLBACK_SHELL: &str = "/bin/sh";

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

/// Resolves the default shell chain from injected values: configured
/// `terminal.shell` first when present and usable, then `$SHELL`, else
/// [`FALLBACK_SHELL`].
///
/// Pure and total for unit testing (no env/fs/net). Every candidate is
/// trimmed but never split, joined, or interpolated; `$SHELL` is trusted
/// only as a binary path. A blank or control-laden configured value fails
/// closed to `$SHELL` (CTX-0298).
pub(crate) fn resolve_default_shell<'a>(
    config_shell: Option<&'a str>,
    shell_env: Option<&'a str>,
) -> &'a str {
    if let Some(configured) = configured_shell_argv0(config_shell) {
        return configured;
    }
    match shell_env {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => FALLBACK_SHELL,
    }
}

/// Resolves the program to spawn: the explicit `args.program` unchanged when
/// present, else the default shell chain (configured `terminal.shell` >
/// `$SHELL` > [`FALLBACK_SHELL`]).
///
/// Pure and total; the caller reads `std::env::var("SHELL")` and the
/// effective config once and injects them so tests never touch the
/// environment.
pub(crate) fn resolve_spawn_program<'a>(
    args: &'a Args,
    config_shell: Option<&'a str>,
    shell_env: Option<&'a str>,
) -> &'a str {
    if let Some(program) = args.program.as_deref() {
        program
    } else {
        resolve_default_shell(config_shell, shell_env)
    }
}

/// Spawns the default shell chain (configured `terminal.shell`, else
/// `$SHELL`, else [`FALLBACK_SHELL`]) inside `runtime`.
///
/// Tries the resolved default first; when the resolved default is not
/// [`FALLBACK_SHELL`] and its spawn fails, retries once with
/// [`FALLBACK_SHELL`] before surfacing the error. Callers log and continue
/// without a child on error so headless smoke still ticks.
pub(crate) fn spawn_default_shell(
    runtime: &mut Runtime,
    config_shell: Option<&str>,
    shell_env: Option<&str>,
) -> Result<(), bitty_runtime::RuntimeError> {
    let default = resolve_default_shell(config_shell, shell_env);
    spawn_with_fallback(|candidate, _| runtime.spawn_shell(candidate), default)
}

/// Spawn core with the startup fallback chain: try `default` first; when it
/// differs from [`FALLBACK_SHELL`] and fails, retry once with the fallback
/// before surfacing the error. The `spawn` closure performs the direct-argv
/// exec into the target session (primary shell or one split pane). Callers
/// log and continue without a child on error so headless smoke still ticks.
fn spawn_with_fallback(
    mut spawn: impl FnMut(&str, &[&str]) -> Result<(), bitty_runtime::RuntimeError>,
    default: &str,
) -> Result<(), bitty_runtime::RuntimeError> {
    let no_args: &[&str] = &[];
    match spawn(default, no_args) {
        Ok(()) => {
            eprintln!("bitty: spawned default shell {default:?}");
            Ok(())
        }
        Err(err) if default != FALLBACK_SHELL => {
            eprintln!(
                "bitty: spawn_shell({default:?}) failed: {err} — trying fallback {FALLBACK_SHELL:?}"
            );
            match spawn(FALLBACK_SHELL, no_args) {
                Ok(()) => {
                    eprintln!("bitty: spawned fallback shell {FALLBACK_SHELL:?}");
                    Ok(())
                }
                Err(fallback_err) => {
                    eprintln!("bitty: spawn_shell({FALLBACK_SHELL:?}) failed: {fallback_err}");
                    Err(fallback_err)
                }
            }
        }
        Err(err) => {
            eprintln!("bitty: spawn_shell({default:?}) failed: {err}");
            Err(err)
        }
    }
}

/// Frozen spawn recipe so every split leaf replays the exact startup
/// resolution (explicit program verbatim, else the default-shell chain:
/// configured `terminal.shell` > `$SHELL` > `/bin/sh`). Captured once at
/// startup from CLI args + effective config + `$SHELL`; values are direct
/// argv throughout, never split, joined, or interpolated.
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
    /// chain from the injected configured shell and `$SHELL` values. Pure;
    /// the caller reads config/env once and injects them.
    pub(crate) fn resolve(&self) -> (String, Vec<String>) {
        match self.program.as_deref() {
            Some(program) => (program.to_string(), self.program_args.clone()),
            None => (
                resolve_default_shell(self.config_shell.as_deref(), self.shell_env.as_deref())
                    .to_string(),
                Vec::new(),
            ),
        }
    }
}

/// Spawns the [`SpawnSpec`] program as leaf `view`'s private shell, sized to
/// `cols` x `rows` cells (CTX-0176). Same sandbox as startup: direct argv,
/// explicit program verbatim with no fallback, default shell with the
/// [`FALLBACK_SHELL`] retry. Failures are logged by the fallback core and
/// returned so the caller degrades loudly: the pane then stays empty
/// (CTX-0359: never a silent mirror of another pane's grid).
pub(crate) fn spawn_pane_shell(
    runtime: &mut Runtime,
    spec: &SpawnSpec,
    view: ViewId,
    cols: u16,
    rows: u16,
) -> Result<(), bitty_runtime::RuntimeError> {
    let (program, args) = spec.resolve();
    if spec.program.is_some() {
        // Explicit program: verbatim, no fallback (startup parity).
        let tail: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        return runtime.spawn_shell_for_view(view, &program, &tail, cols, rows);
    }
    spawn_with_fallback(
        |candidate, _| runtime.spawn_shell_for_view(view, candidate, &[], cols, rows),
        &program,
    )
}

/// Spawns a private shell for every layout leaf except the focused one
/// (CTX-0176), which keeps the already-spawned primary session. Each pane
/// shell is sized to its leaf allocation. Best-effort: per-leaf failures
/// warn loudly and leave that pane empty (CTX-0359: never a silent mirror
/// of the primary grid). Call only after a successful primary spawn.
pub(crate) fn spawn_startup_pane_shells(runtime: &mut Runtime, spec: &SpawnSpec) {
    let primary = runtime.focused_view();
    let allocs = runtime.layout_allocations();
    for (id, rect) in &allocs {
        if Some(*id) == primary {
            continue;
        }
        if let Err(err) =
            spawn_pane_shell(runtime, spec, *id, rect.width.max(1), rect.height.max(1))
        {
            eprintln!("warning: startup pane {id:?} shell spawn failed ({err}) — pane stays empty");
        }
    }
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
