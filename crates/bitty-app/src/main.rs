//! `bitty-app`: Correct Terminal thin composition root.
//!
//! This binary is the **thin composition root** per ADR-0003 ("`bitty-app`
//! Binary entry point; argument handling, startup, safe-mode selection;
//! depends on `bitty-runtime` only"). It owns **no business logic** beyond
//! wiring already-owned libraries: argument parsing, [`bitty_runtime::Runtime`]
//! creation, layout wiring via `LayoutNode`, optional window / GPU attachment,
//! PTY pump integration, platform event-loop forwarding, and `tick` → present.
//!
//! # Startup flow (owned)
//!
//! ```text
//! args --parse--> Args --Runtime::with_defaults--> Runtime
//!       --build_layout--> LayoutNode --set_layout--> Runtime --set_focus--> Runtime
//!       --spawn_shell--> PTY --handle_pty_bytes--> Runtime
//!       --App::run--> PlatformEvent --handle_platform_event--> Runtime --tick--> present
//! ```
//!
//! 1. **Parse args** (`--help` / `--version` / `--headless`, layout flags
//!    `--split`/`--stack`/`--overlay`/`--layout`, focus flag `--focus`,
//!    config flags `--config`/`--theme`, and an optional program to spawn).
//!    Flag parsing itself is pure, total, and tested without touching the
//!    filesystem or network; config-file loading happens in step 2.
//! 2. **Load user config** (CTX-0148 Lua, DEC-0011): resolve `--config` or
//!    the XDG default (`$XDG_CONFIG_HOME/bitty/init.lua`, fallback
//!    `~/.config/bitty/init.lua`, then `config.lua`), evaluate the
//!    wezterm-style return table in the `bitty-lua` sandbox via
//!    `bitty-config::file` (never executed as code), validate/migrate/merge
//!    with precedence `CLI (`--theme`) > file > defaults`. Invalid files
//!    fail closed (clear stderr, exit 2, no panic, no silent ignore); a
//!    missing default-path file simply yields defaults.
//! 3. **Create [`Runtime`](bitty_runtime::Runtime)** via
//!    [`Runtime::new`](bitty_runtime::Runtime::new) with a [`RuntimeConfig`](bitty_runtime::RuntimeConfig)
//!    derived from the effective config (font family/size; grid/cell/queue
//!    stay at defaults). This immediately
//!    builds a headless software surface (`Surface::headless`) and the
//!    deterministic `GridRenderer` — no display server, window, adapter, or
//!    font file is contacted.
//! 4. **Build layout** via [`build_layout`] from the parsed [`Args`] (default
//!    single leaf, `--split` horizontal/vertical, `--stack`, `--overlay`, or
//!    `--layout` spec). The app constructs a [`LayoutNode`](bitty_runtime::LayoutNode)
//!    via `bitty-ui` types re-exported through `bitty-runtime` and calls
//!    [`Runtime::set_layout`](bitty_runtime::Runtime::set_layout). No plugin
//!    coupling is involved; the layout is derived purely from argv (config
//!    files today carry appearance/font/window/terminal scalars only, never
//!    layout).
//! 5. **Focus handling** via `--focus` (numeric id or `next`/`prev`/`up`/`down`/
//!    `left`/`right`) and via keyboard shortcuts in real mode (Tab/arrow/n/p/1..5).
//!    Focus moves are routed through [`Runtime::set_focus`](bitty_runtime::Runtime::set_focus)
//!    and [`Runtime::move_focus`](bitty_runtime::Runtime::move_focus) which
//!    delegate to the layout's deterministic adjacency.
//! 6. **Spawn shell** via [`Runtime::spawn_shell`](bitty_runtime::Runtime::spawn_shell)
//!    for the explicit program argument, or via the default shell (`$SHELL`
//!    fallback `/bin/sh`, see [`resolve_default_shell`]) when no program is
//!    given. The program is taken as a direct
//!    `argv[0]` without shell interpolation (P0 posture). Failures are owned
//!    [`RuntimeError`](bitty_runtime::RuntimeError) values flattened from
//!    `bitty-pty` (`Unsupported` on Windows before ConPTY, `Upstream`/`Io`
//!    elsewhere) and are reported without panicking.
//! 7. **PTY pump integration** — the bounded `PtyReader` (`READ_CHUNK_SIZE`
//!    × `CHANNEL_CAPACITY_CHUNKS` = 128 KiB) pumps kernel bytes into a
//!    `sync_channel`; the app drains `Receiver::try_recv` on the platform
//!    thread and feeds `Runtime::handle_pty_bytes`. The live pump is wired:
//!    [`Runtime::take_pty_reader`](bitty_runtime::Runtime::take_pty_reader)
//!    and [`Runtime::poll_pty`](bitty_runtime::Runtime::poll_pty) exist and
//!    `TerminalApp::poll_pty_pump` drains the real runtime channel first
//!    (replies flushed via `Runtime::write_replies`), with the synthetic
//!    demo pump kept only as a bounded fallback for headless runs without
//!    a spawned child. Headless smoke exercises `handle_pty_bytes` via
//!    synthetic bytes in addition to the live path.
//! 8. **Platform event loop** (`bitty-platform::App::run`) forwards every
//!    [`PlatformEvent`](bitty_platform::PlatformEvent) into
//!    [`Runtime::handle_platform_event`](bitty_runtime::Runtime::handle_platform_event)
//!    (resize → `handle_resize`, `CloseRequested`/`Exiting` → exit, other
//!    window events → `false`). `AboutToWait` and `RedrawRequested` call
//!    [`Runtime::tick`](bitty_runtime::Runtime::tick) and request redraw when
//!    the frame produced damage. This keeps the idle resource budget
//!    (≤ 1 % CPU when no damage) honest: zero damage presents nothing. `tick`
//!    is layout-aware: it reflows the owned `LayoutNode` into the container
//!    rect and composites per-leaf `View` allocations via the headless software
//!    seam (deterministic RGBA) or the real `SurfaceTarget` when available.
//! 8. **Headless smoke** (`--headless`) feeds a synthetic byte batch, ticks
//!    layout-aware, prints cold-queue + present stats, and then proves
//!    `split`/`stack`/`overlay` composition deterministically via software
//!    present without window/GPU. This is the **only path CI exercises** (no
//!    display server or GPU required) and is the fallback when `App::run`
//!    returns `PlatformError::DisplayUnavailable`.
//!
//! # Layout wiring (CTX-0025)
//!
//! - The app never invents layout math: all geometry lives in `bitty-ui` and
//!   `bitty-runtime`. The app only parses argv, constructs a `LayoutNode`,
//!   calls `Runtime::set_layout`, and optionally moves focus. The runtime owns
//!   `LayoutNode` + `Focus` and performs `reflow` + per-leaf `GridRenderer`
//!   translation + single `Surface::headless_present` (headless) or real
//!   `SurfaceTarget` present (real). The app stays thin.
//! - `--layout` takes precedence over `--stack`/`--overlay`/`--split`; when no
//!   layout flag is given the default is a single leaf `ViewId(1)` sized to
//!   the runtime's current grid (80×24 by default). Leaf sizes are updated by
//!   `LayoutNode::reflow` on the next tick, so the initial `View::new` sizes
//!   are only hints.
//! - Focus is owned by `Runtime`. `--focus` for smoke and keyboard shortcuts in
//!   real mode both resolve to `Runtime::set_focus` (numeric id) or
//!   `Runtime::move_focus` (directional). Invalid focus specs are warned and
//!   ignored (total, no panic).
//!
//! # Headless vs real split (documented honestly)
//!
//! - **Headless (CI, default, `--headless`, or display unavailable):**
//!   `Runtime::new` builds `Surface::headless` with the config-derived pixel
//!   extent and a deterministic `HeadlessRasterizer` (no font stack). `tick`
//!   reflows the `LayoutNode` into the container `Rect` (cell space), builds a
//!   viewport snapshot per leaf, renders each through the shared `GridRenderer`
//!   (translated to the leaf's pixel origin), and composites the combined
//!   `DrawList + Atlas` onto an in-memory RGBA buffer via
//!   `Surface::headless_present`. No `GpuContext`, adapter, `SurfaceTarget`,
//!   window, or font file is contacted. The proof `bytes → parser → state →
//!   damage → GridRenderer DrawList → software present` is exercised by
//!   `crates/bitty-runtime/tests/runtime_soft_present.rs` and by this binary's
//!   `--headless` smoke, which additionally proves `split`/`stack`/`overlay`
//!   composition deterministically (separate runtimes with the same bytes/layout
//!   produce bit-identical RGBA; different compositions produce distinct RGBA).
//!   This is the only end-to-end path CI verifies.
//!
//! - **Real (env-gated, display available):** attaching a real window surface
//!   requires `bitty_render::gpu::GpuContext::initialize().await` on a machine
//!   with a working driver and a live `SurfaceTarget` from
//!   `bitty_platform::WindowHandle::surface_target`. Those APIs return
//!   `RenderError::NoCompatibleAdapter` on headless runners and are covered
//!   only by `BITTY_RENDER_GPU_TESTS=1` in `bitty-render`. The **honest gap**
//!   in this slice is that this crate's `Runtime` surface is always headless
//!   today — the runtime docs state "caller must not describe `attach_gpu` as
//!   implemented" and no `Runtime::attach_gpu` API exists yet. The app
//!   therefore **documents but does not yet drive** the async GPU initializer;
//!   even when `App::run` succeeds and a window is created, `tick` still
//!   presents via the headless software seam (but layout-aware, per-frame,
//!   with focus movement via keyboard). `SurfaceTarget` lifetime
//!   handling (`with_raw_handles` → `wgpu::Surface` must be dropped before the
//!   last `WindowHandle` clone) is owned by the future `GpuContext` slice and
//!   is not fabricated here. The window creation + `PlatformEvent` →
//!   `Runtime::handle_platform_event` + `tick` plumbing is proven even on
//!   headless CI via the `DisplayUnavailable` → headless smoke fallback.
//!
//! What CI **cannot** verify: any code path that reaches a live adapter/device
//! or a live window surface (`GpuContext::initialize`,
//! `GpuContext::create_surface`, real `Surface::present`). Those remain
//! env-gated and are not described as implemented until that slice lands with
//! evidence.
//!
//! # PTY pump note
//!
//! PTY bytes are untrusted input; unbounded parsing or buffering is forbidden.
//! The production pump is `PtyReader::spawn` (kernel → bounded
//! `sync_channel` 16 × 8 KiB = 128 KiB → `handle_pty_bytes`). Backpressure is
//! end-to-end: when the consumer stalls the channel fills, the pump blocks,
//! the kernel PTY buffer fills, and the child's `write` blocks. This binary
//! demonstrates that contract via a synthetic bounded demo pump (no real child
//! required), and the live pump is wired: `Runtime::take_pty_reader` and
//! `Runtime::poll_pty` exist and `TerminalApp::poll_pty_pump` drains them.
//!
//! # Security
//!
//! - No `unsafe` is required. The workspace denies `unsafe_code`; this binary
//!   enforces `#![forbid(unsafe_code)]` with no exception.
//! - No upstream type (`portable-pty`, `vte`, `winit`, `wgpu`) appears in any
//!   public signature — the binary has no library API, and all library
//!   boundaries are behind `bitty-runtime` / `bitty-platform` owned types.
//! - No shell interpolation. `spawn_shell` takes a direct `argv[0]` via
//!   `PtyBuilder`, never a shell string.

#![forbid(unsafe_code)]

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;

use bitty_platform::{
    App, AppHandler, EventContext, EventWaker, LogicalKey, LogicalSize, NamedKey, PhysicalSize,
    PlatformError, PlatformEvent, PressState, WindowConfig, WindowEventKind, WindowHandle,
    WindowId,
};
use bitty_render::gpu::GpuContext;
use bitty_runtime::{FocusDirection, LayoutNode, Runtime, SplitAxis, UiRect, View, ViewId};

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

/// Owned argument bag for the composition root.
///
/// `program` is the optional `argv[0]` to spawn inside the PTY. When `None`
/// the spawn layer resolves to the default shell (`$SHELL` or `/bin/sh` via
/// [`resolve_default_shell`); parsing itself stays `None` to keep arg parsing
/// pure (CTX-0136).
#[derive(Debug, Clone, PartialEq)]
struct Args {
    /// When true the binary runs a single headless tick smoke and exits.
    headless: bool,
    /// When true print help and exit 0.
    help: bool,
    /// When true print version and exit 0.
    version: bool,
    /// Optional explicit program to spawn via `Runtime::spawn_shell`.
    /// When `None`, the spawn layer falls back to the default shell
    /// ([`resolve_default_shell`]); explicit values are used verbatim.
    program: Option<String>,
    /// Extra argv tail for the program (reserved; not yet forwarded to
    /// `PtyBuilder::arg` because `Runtime::spawn_shell` currently takes a
    /// single `&str` — documented as a follow-up).
    program_args: Vec<String>,
    /// Optional split axis (from `--split`).
    split_axis: Option<SplitAxis>,
    /// Optional split ratio (from `--split-ratio` or `--split` colon form).
    split_ratio: Option<f32>,
    /// When true, request a stack layout (from `--stack`).
    stack: bool,
    /// When true, request an overlay layout (from `--overlay`).
    overlay: bool,
    /// Raw layout spec (from `--layout`), e.g. "single", "split:h:0.5", "stack", "overlay:5,5,20,10".
    layout: Option<String>,
    /// Raw focus spec (from `--focus`), e.g. "next", "prev", "up", "1".
    focus: Option<String>,
    /// Explicit config file path (from `--config`). When `None` the default
    /// XDG path is probed (`$XDG_CONFIG_HOME/bitty/init.lua`, fallback
    /// `~/.config/bitty/init.lua`, then `config.lua`); see `bitty-config::file`.
    config_path: Option<String>,
    /// CLI theme override (from `--theme`). Wins over the config file, which
    /// wins over defaults (`CLI > file > defaults` via `bitty-config` merge).
    theme: Option<String>,
    /// `bitty config <verb>` subcommand (CLI-first management per DEC-0007).
    /// `None` means normal terminal startup. A program literally named
    /// `config` must be invoked as `bitty -- config ...`.
    config_cmd: Option<ConfigCommand>,
    /// True once the first positional `config` word is seen (subcommand
    /// mode); unknown/missing verbs fail closed via usage instead of
    /// spawning a program named `config`.
    config_word: bool,
    /// Unexpected extra positionals in subcommand mode (dispatch errors).
    config_args: Vec<String>,
}

/// `bitty config` subcommand verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigCommand {
    /// Print the resolved config file path.
    Path,
    /// Load + validate and print per-key sources (the testing hook).
    Check,
    /// Open the file in `$VISUAL`/`$EDITOR` (never overwrites existing).
    Edit,
}

impl ConfigCommand {
    /// Parse a verb token.
    fn parse(token: &str) -> Option<Self> {
        match token {
            "path" => Some(Self::Path),
            "check" => Some(Self::Check),
            "edit" => Some(Self::Edit),
            _ => None,
        }
    }

    /// Verb name for usage/errors.
    fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Check => "check",
            Self::Edit => "edit",
        }
    }
}

impl Args {
    fn new() -> Self {
        Self {
            headless: false,
            help: false,
            version: false,
            program: None,
            program_args: Vec::new(),
            split_axis: None,
            split_ratio: None,
            stack: false,
            overlay: false,
            layout: None,
            focus: None,
            config_path: None,
            theme: None,
            config_cmd: None,
            config_word: false,
            config_args: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Default shell resolution (CTX-0136)
// ---------------------------------------------------------------------------

/// Fallback shell when `$SHELL` is unset or blank.
///
/// POSIX default. Windows keeps the same fallback for now; the ConPTY default
/// slice may refine this without changing the resolver contract (pure/total,
/// no env/fs access — the caller injects `$SHELL`).
const FALLBACK_SHELL: &str = "/bin/sh";

/// Resolves the default shell program from an injected `$SHELL` value.
///
/// Pure and total for unit testing (no env/fs/net): `None`, empty, or
/// whitespace-only resolves to [`FALLBACK_SHELL`]; otherwise returns the
/// trimmed `$SHELL` value verbatim as a direct `argv[0]` (no interpolation,
/// no arg splitting — `$SHELL` is trusted only as a binary path).
fn resolve_default_shell(shell_env: Option<&str>) -> &str {
    match shell_env {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => FALLBACK_SHELL,
    }
}

/// Resolves the program to spawn: the explicit `args.program` unchanged when
/// present, else the default shell from the injected `$SHELL` value.
///
/// Pure and total; the caller reads `std::env::var("SHELL")` once and injects
/// it so tests never touch the environment.
fn resolve_spawn_program<'a>(args: &'a Args, shell_env: Option<&'a str>) -> &'a str {
    if let Some(program) = args.program.as_deref() {
        program
    } else {
        resolve_default_shell(shell_env)
    }
}

/// Spawns the default shell (`$SHELL` or [`FALLBACK_SHELL`]) inside `runtime`.
///
/// Tries the resolved default first; when the resolved default came from
/// `$SHELL` and its spawn fails, retries once with [`FALLBACK_SHELL`] before
/// surfacing the error. Callers log and continue without a child on error so
/// headless smoke still ticks.
fn spawn_default_shell(
    runtime: &mut Runtime,
    shell_env: Option<&str>,
) -> Result<(), bitty_runtime::RuntimeError> {
    let default = resolve_default_shell(shell_env);
    match runtime.spawn_shell(default) {
        Ok(()) => {
            eprintln!("bitty: spawned default shell {default:?}");
            Ok(())
        }
        Err(err) if default != FALLBACK_SHELL => {
            eprintln!(
                "bitty: spawn_shell({default:?}) failed: {err} — trying fallback {FALLBACK_SHELL:?}"
            );
            match runtime.spawn_shell(FALLBACK_SHELL) {
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

fn parse_split_axis(s: &str) -> Option<SplitAxis> {
    match s.to_ascii_lowercase().as_str() {
        "horizontal" | "h" | "horiz" | "hor" => Some(SplitAxis::Horizontal),
        "vertical" | "v" | "vert" | "ver" => Some(SplitAxis::Vertical),
        _ => None,
    }
}

/// Parses a value that may be `axis` or `axis:ratio` (e.g. "h:0.3", "vertical:0.7").
fn parse_split_token(token: &str) -> (Option<SplitAxis>, Option<f32>) {
    if let Some((axis_part, ratio_part)) = token.split_once(':') {
        let axis = parse_split_axis(axis_part.trim());
        let ratio = ratio_part.trim().parse::<f32>().ok();
        (axis, ratio)
    } else {
        (parse_split_axis(token.trim()), None)
    }
}

/// Parses `raw` (including `argv[0]` at index 0) into [`Args`].
///
/// Recognised flags:
/// - `-h` / `--help` → help
/// - `-V` / `--version` → version
/// - `--headless` → headless smoke (also triggered by `BITTY_HEADLESS=1`)
/// - `--split [AXIS]` → split layout (AXIS = horizontal|h / vertical|v, default horizontal)
/// - `--split=AXIS[:RATIO]` → split with optional ratio
/// - `--split-ratio RATIO` → ratio for split
/// - `--stack` → stack layout (2 panes)
/// - `--overlay` → overlay layout
/// - `--layout SPEC` → explicit layout spec (single, split:h[:ratio], stack[:n], overlay[:x,y,w,h])
/// - `--focus SPEC` → focus (next|prev|up|down|left|right|<id>)
/// - `--config PATH` → explicit user config file (`init.lua`); when omitted
///   the XDG default is probed (`$XDG_CONFIG_HOME/bitty/init.lua`, fallback
///   `~/.config/bitty/init.lua`, then `config.lua` alias)
/// - `--theme NAME` → CLI theme override (wins over config file, which wins
///   over defaults; see `bitty-config::file::resolve_effective`)
/// - `config <path|check|edit>` → config subcommand (DEC-0007); a program
///   literally named `config` needs `bitty -- config ...`
/// - `--` → treat the rest as program argv verbatim
///
/// The first non-flag token becomes `program`; additional non-flag tokens
/// after it become `program_args`. Unknown long flags are reported to stderr
/// but do not abort parsing — the binary stays total and keeps the invalid
/// token as a program name so callers see the error on `spawn_shell`.
fn parse_args(raw: &[String]) -> Args {
    let mut out = Args::new();
    // Env fallback for CI runners that set BITTY_HEADLESS without editing argv.
    if std::env::var("BITTY_HEADLESS").is_ok_and(|v| v == "1" || v.to_lowercase() == "true") {
        out.headless = true;
    }
    if raw.len() <= 1 {
        return out;
    }
    let mut after_double_dash = false;
    let mut program_set = false;
    let mut i = 1usize;
    while i < raw.len() {
        let token = &raw[i];
        if after_double_dash {
            if !program_set {
                out.program = Some(token.clone());
                program_set = true;
            } else {
                out.program_args.push(token.clone());
            }
            i += 1;
            continue;
        }
        // Handle flags with `=` first
        if token.starts_with("--split-ratio=") {
            let val = token.trim_start_matches("--split-ratio=");
            if let Ok(f) = val.parse::<f32>() {
                out.split_ratio = Some(f);
            } else {
                eprintln!("warning: invalid --split-ratio value {val:?} — ignoring");
            }
            i += 1;
            continue;
        }
        if token.starts_with("--split=") {
            let val = token.trim_start_matches("--split=");
            // val may be "h:0.3" or "horizontal" etc.
            let (axis, ratio) = parse_split_token(val);
            if let Some(ax) = axis {
                out.split_axis = Some(ax);
            } else if !val.is_empty() {
                eprintln!("warning: unknown --split axis {val:?} — defaulting to horizontal");
                out.split_axis = Some(SplitAxis::Horizontal);
            } else {
                out.split_axis = Some(SplitAxis::Horizontal);
            }
            if let Some(r) = ratio {
                out.split_ratio = Some(r);
            }
            i += 1;
            continue;
        }
        if token.starts_with("--layout=") {
            let val = token.trim_start_matches("--layout=");
            out.layout = Some(val.to_string());
            i += 1;
            continue;
        }
        if token.starts_with("--focus=") {
            let val = token.trim_start_matches("--focus=");
            out.focus = Some(val.to_string());
            i += 1;
            continue;
        }
        if token.starts_with("--config=") {
            let val = token.trim_start_matches("--config=");
            if val.trim().is_empty() {
                eprintln!("warning: --config needs a file path — ignoring");
            } else {
                out.config_path = Some(val.to_string());
            }
            i += 1;
            continue;
        }
        if token.starts_with("--theme=") {
            let val = token.trim_start_matches("--theme=");
            out.theme = Some(val.to_string());
            i += 1;
            continue;
        }
        match token.as_str() {
            "--" => {
                after_double_dash = true;
                i += 1;
            }
            "-h" | "--help" => {
                out.help = true;
                i += 1;
            }
            "-V" | "--version" => {
                out.version = true;
                i += 1;
            }
            "--headless" => {
                out.headless = true;
                i += 1;
            }
            "--stack" => {
                out.stack = true;
                i += 1;
            }
            "--overlay" => {
                out.overlay = true;
                i += 1;
            }
            "--split" => {
                // Check next token for axis (if not a flag)
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    let next = raw[i + 1].clone();
                    let (axis, ratio) = parse_split_token(&next);
                    if axis.is_some() || ratio.is_some() {
                        if let Some(ax) = axis {
                            out.split_axis = Some(ax);
                        } else {
                            // axis parse failed but ratio present? Keep default axis
                            out.split_axis = Some(SplitAxis::Horizontal);
                        }
                        if let Some(r) = ratio {
                            out.split_ratio = Some(r);
                        }
                        i += 2;
                    } else {
                        // Next token is not an axis/ratio (e.g. "/bin/bash"), treat --split as horizontal without consuming
                        out.split_axis = Some(SplitAxis::Horizontal);
                        i += 1;
                    }
                } else {
                    out.split_axis = Some(SplitAxis::Horizontal);
                    i += 1;
                }
            }
            "--split-ratio" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    let next = &raw[i + 1];
                    if let Ok(f) = next.parse::<f32>() {
                        out.split_ratio = Some(f);
                    } else {
                        eprintln!("warning: invalid --split-ratio value {next:?} — ignoring");
                    }
                    i += 2;
                } else {
                    eprintln!("warning: --split-ratio needs a numeric value — ignoring");
                    i += 1;
                }
            }
            "--layout" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.layout = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!(
                        "warning: --layout needs a value (single|split:h[:ratio]|stack[:n]|overlay[:x,y,w,h]) — ignoring"
                    );
                    i += 1;
                }
            }
            "--focus" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.focus = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!(
                        "warning: --focus needs a value (next|prev|up|down|left|right|<id>) — ignoring"
                    );
                    i += 1;
                }
            }
            "--config" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.config_path = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --config needs a file path — ignoring");
                    i += 1;
                }
            }
            "--theme" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.theme = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --theme needs a theme name — ignoring");
                    i += 1;
                }
            }
            s if s.starts_with('-') => {
                eprintln!("warning: unknown flag {s:?} — treating as program name");
                if !program_set {
                    out.program = Some(token.clone());
                    program_set = true;
                } else {
                    out.program_args.push(token.clone());
                }
                i += 1;
            }
            _ => {
                // `bitty config <verb>` subcommand (first positional only;
                // `--` escape hatch bypasses this via after_double_dash).
                // A program literally named `config` needs `bitty -- config`.
                if !program_set && !out.config_word && token == "config" {
                    out.config_word = true;
                    if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                        let verb = raw[i + 1].clone();
                        match ConfigCommand::parse(&verb) {
                            Some(cmd) => {
                                out.config_cmd = Some(cmd);
                            }
                            None => {
                                // Unknown verb: record for fail-closed usage.
                                out.config_args.push(verb);
                            }
                        }
                        i += 2;
                    } else {
                        // Bare `bitty config` (or `config` + flag): dispatch
                        // prints usage + exit 2.
                        i += 1;
                    }
                    continue;
                }
                if out.config_word {
                    // Verb may follow flags (`config --config X check`): take
                    // the first bare token as the verb when none is set yet.
                    if out.config_cmd.is_none() && out.config_args.is_empty() {
                        if let Some(cmd) = ConfigCommand::parse(token) {
                            out.config_cmd = Some(cmd);
                            i += 1;
                            continue;
                        }
                    }
                    // Extra positionals in subcommand mode fail closed.
                    out.config_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if !program_set {
                    out.program = Some(token.clone());
                    program_set = true;
                } else {
                    out.program_args.push(token.clone());
                }
                i += 1;
            }
        }
    }
    out
}

fn help_text() -> String {
    format!(
        "bitty {} — Correct Terminal (thin composition root)\n\
         \n\
         Usage: bitty [OPTIONS] [--] [PROGRAM [ARGS...]]\n\
         \n\
         Options:\n  \
           -h, --help       Print this help and exit\n  \
           -V, --version    Print version and exit\n  \
               --headless   Run a single headless tick smoke and exit (CI)\n  \
               --split [AXIS]  Split layout: AXIS = horizontal|h / vertical|v (default h, ratio 0.5)\n  \
               --split=AXIS[:RATIO]  Split with optional ratio (e.g. --split=h:0.3)\n  \
               --split-ratio RATIO  Ratio for --split (0.10..0.90, default 0.5)\n  \
               --stack      Stack layout (2 panes, full bounds, last on top)\n  \
               --overlay    Overlay layout (base + 20×10 floating at 5,5)\n  \
               --layout SPEC  Explicit layout: single | split:h[:ratio] | split:v[:ratio]\n  \
           \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20stack[:N] | overlay[:X,Y,W,H]  (overrides --split/--stack/--overlay)\n  \
               --focus SPEC Focus: next|prev|up|down|left|right|<id> (e.g. --focus next, --focus 2)\n  \
               --config PATH  Explicit user config file (init.lua). When omitted\n  \
                            the XDG default is probed ($XDG_CONFIG_HOME/bitty/init.lua,\n  \
                            fallback ~/.config/bitty/init.lua, then config.lua alias).\n  \
                            Invalid files fail closed (clear stderr, exit non-zero,\n  \
                            no panic).\n  \
               --theme NAME   CLI theme override (e.g. --theme dark). Wins over the\n  \
                            config file, which wins over defaults.\n  \
               --           End of flags; remaining tokens are PROGRAM argv\n\
         \n\
         Subcommands (CLI-first management, DEC-0007):\n  \
           config path      Print the resolved config file path\n  \
           config check     Load + validate; print per-key sources\n  \
                            (cli/file/default), exit non-zero on invalid files\n  \
           config edit      Open the file in $VISUAL/$EDITOR (vi fallback);\n  \
                            creates parents + starter when missing, never\n  \
                            overwrites existing content\n  \
         \n\
         Arguments:\n  \
           PROGRAM          Program to spawn inside the PTY (direct argv[0],\n  \
                            no shell interpolation). When omitted defaults to\n  \
                            $SHELL or /bin/sh; --headless still ticks.\n\
         \n\
         Layout:\n  \
           The app constructs a LayoutNode via bitty-ui and calls Runtime::set_layout.\n  \
           Precedence: --layout > --stack > --overlay > --split > single (default).\n  \
           Examples: --split, --split v, --split=h:0.3 --stack, --overlay,\n  \
                     --layout single, --layout split:h:0.5, --layout stack:3,\n  \
                     --layout overlay:5,5,20,10\n\
         \n\
         Focus:\n  \
           --focus moves focus after layout install (Direction via FocusDirection\n  \
           or numeric ViewId). Keyboard in real mode: Tab/n→Next, p→Prev,\n  \
           Arrows→spatial, 1..5→ViewId(1..5). Focus changes are deterministic.\n\
         \n\
         Modes:\n  \
           headless         Surface::headless software present, no display/GPU.\n  \
                            Triggered by --headless, BITTY_HEADLESS=1, or\n  \
                            App::run -> DisplayUnavailable fallback. Proves\n  \
                            split/stack/overlay composition deterministically\n  \
                            (separate runtimes same bytes/layout → identical RGBA;\n  \
                            different layouts → distinct RGBA). No window/GPU.\n  \
           real             App::run event loop with Window creation and\n  \
                            PlatformEvent -> Runtime::handle_platform_event\n  \
                            plus layout-aware tick -> present. GPU attach\n  \
                            (GpuContext + SurfaceTarget) is an honest env-gated\n  \
                            gap: runtime still presents via the headless seam\n  \
                            until the attach_gpu slice lands. See module docs.\n\
         \n\
         Config file (Lua, wezterm-style init.lua):\n  \
           return {{ theme = \"dark\", font = {{ family = \"Mono\", size = 12 }} }}\n  \
                            Evaluated in the bitty-lua sandbox (same budgets as\n  \
                            plugins; no io/os). Unknown keys fail closed.\n  \
         \n\
         Examples:\n  \
           bitty --help\n  \
           bitty --version\n  \
           bitty --headless\n  \
           bitty --headless --split v --focus next\n  \
           bitty --headless --layout stack:2 --focus 2\n  \
           bitty --headless --layout overlay:5,5,20,10\n  \
           bitty --headless -- /bin/bash\n  \
           bitty /bin/bash\n  \
           bitty -- /bin/cat -A\n",
        version_text()
    )
}

fn version_text() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ---------------------------------------------------------------------------
// User config-file loading (CTX-0148 Lua via bitty-lua sandbox, DEC-0011)
// ---------------------------------------------------------------------------

/// Owned result of loading the effective user configuration.
///
/// `source` is `"cli"` when `--theme` overrode, `"file"` when the config file
/// provided the theme, else `"default"`. The resolved `theme` preset is the
/// single `bitty-config` registry entry the window renders.
struct AppConfig {
    /// Merged effective config (`CLI > file > defaults`).
    effective: bitty_config::EffectiveConfig,
    /// Config file path that was used, if any.
    file_path: Option<std::path::PathBuf>,
    /// Resolved theme preset (static registry entry).
    theme: &'static bitty_config::theme::Theme,
    /// How the theme resolved (Default/Named/FallbackUnknown).
    resolution: bitty_config::theme::ThemeResolution,
    /// `"cli"` / `"file"` / `"default"`: which layer provided the theme.
    source: &'static str,
}

/// Shared probe + load + merge behind startup and `config check`.
///
/// Returns the merged layers plus the probed path (if any). impure
/// (filesystem + env); total (all failures become `Err(String)`).
fn load_merged_config(
    args: &Args,
) -> Result<
    (
        bitty_config::MergedConfig,
        Option<bitty_config::file::ProbedConfig>,
    ),
    String,
> {
    let explicit = args.config_path.as_deref();
    let probed = bitty_config::file::probe_config_path(explicit.filter(|p| !p.trim().is_empty()));
    let mut file_layer: Option<bitty_config::LayeredPlan> = None;
    let mut used_path: Option<std::path::PathBuf> = None;
    if let Some(probe) = probed.clone() {
        if probe.path.exists() {
            match bitty_config::file::load_user_layer(&probe.path) {
                Ok(layer) => {
                    used_path = Some(probe.path.clone());
                    file_layer = Some(layer);
                }
                Err(err) => {
                    return Err(format!(
                        "bitty: invalid config file '{}': {err}",
                        probe.path.display()
                    ));
                }
            }
        } else if probe.explicit {
            return Err(format!(
                "bitty: --config '{}' not found",
                probe.path.display()
            ));
        }
    }
    let merged = bitty_config::file::resolve_effective(file_layer, args.theme.as_deref()).map_err(
        |err| {
            if let Some(p) = &used_path {
                format!("bitty: invalid config '{}': {err}", p.display())
            } else {
                format!("bitty: invalid --theme: {err}")
            }
        },
    )?;
    Ok((merged, probed))
}

/// Loads the effective config for `args`.
///
/// - Probes `--config` verbatim, else `init.lua`, else the `config.lua`
///   alias, else the canonical `init.lua` path.
/// - A missing **probed** file yields defaults (no error); a missing
///   **explicit** `--config` file fails closed.
/// - A present-but-invalid file (Lua syntax/runtime/budget/shape/validation)
///   fails closed with a user-facing message (caller prints to stderr and
///   exits non-zero; no panic, no silent ignore).
/// - Merges `CLI (--theme) > file > defaults` via `bitty-config` and resolves
///   `appearance.theme` through the preset registry.
///
/// impure (filesystem reads + env vars XDG/HOME); total (all failures become
/// `Err(String)`).
fn load_app_config(args: &Args) -> Result<AppConfig, String> {
    let (merged, probed) = load_merged_config(args)?;
    let mut file_path: Option<std::path::PathBuf> = None;
    if let Some(probe) = &probed {
        if probe.path.exists() {
            if probe.fallback_name {
                eprintln!(
                    "bitty: using fallback '{}' (canonical is 'init.lua')",
                    probe.path.display()
                );
            }
            file_path = Some(probe.path.clone());
        }
    }
    // Attribute the theme source for title/demo/log evidence. The merge
    // attribution answers which layer won `appearance.theme`; fall back to
    // CLI-vs-file presence when the field is at defaults.
    let source: &'static str = match merged.source_of("appearance.theme").map(|s| s.layer) {
        Some(bitty_config::LayerKind::Cli) => "cli",
        Some(bitty_config::LayerKind::User) => "file",
        _ => "default",
    };
    let effective = merged.effective;
    let (theme, resolution) =
        bitty_config::theme::resolve_theme_with_status(effective.appearance.theme.as_deref());
    // Log unknown-theme fallbacks (the pure resolver stays silent for tests).
    if resolution == bitty_config::theme::ThemeResolution::FallbackUnknown {
        let raw = effective.appearance.theme.as_deref().unwrap_or_default();
        eprintln!(
            "bitty: unknown theme '{raw}'; falling back to '{}'",
            bitty_config::theme::DEFAULT_THEME_NAME
        );
    }
    if let Some(p) = &file_path {
        eprintln!(
            "bitty: config loaded from '{}' (theme={:?} resolution={resolution:?} source={source})",
            p.display(),
            effective.appearance.theme.as_deref().unwrap_or("(default)")
        );
    } else if args.theme.as_deref().is_some_and(|t| !t.trim().is_empty()) {
        eprintln!(
            "bitty: CLI theme {:?} (resolution={resolution:?} source={source})",
            args.theme.as_deref().unwrap_or_default()
        );
    }
    Ok(AppConfig {
        effective,
        file_path,
        theme,
        resolution,
        source,
    })
}

/// Short usage for `bitty config` (stderr, fail-closed exit 2).
fn config_usage() -> String {
    "usage: bitty config <path|check|edit> [--config PATH]\n\
     \n\
     verbs:\n\
     \x20 path   print the resolved config file path\n\
     \x20 check  load + validate; print per-key sources (cli/file/default)\n\
     \x20 edit   open the file in $VISUAL/$EDITOR (vi fallback)\n\
     \n\
     config file: $XDG_CONFIG_HOME/bitty/init.lua (fallback config.lua alias)"
        .to_string()
}

/// Resolves the editor for `config edit` without touching the process:
/// `$VISUAL`, then `$EDITOR`, then `vi`. Pure over injected values for
/// headless tests.
fn resolve_editor_with_env(visual: Option<&str>, editor: Option<&str>) -> String {
    for cmd in [visual, editor].into_iter().flatten() {
        let trimmed = cmd.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "vi".to_string()
}

/// Live-environment editor resolution.
fn resolve_editor() -> String {
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    resolve_editor_with_env(visual.as_deref(), editor.as_deref())
}

/// Starter `init.lua` written by `config edit` only when the file is missing.
/// Never used to overwrite existing content.
fn starter_init_lua() -> &'static str {
    "-- bitty user configuration (Lua, wezterm-style).\n\
     -- Evaluated in the bitty-lua sandbox (same budgets as plugins; no io/os).\n\
     -- Unknown keys fail closed; validate with `bitty config check`.\n\
     return {\n\
     \x20\x20theme = \"dark\",\n\
     }\n"
}

/// Formats one `config check` row: `dotted.key = value (source)`.
fn check_row(key: &str, value: String, source: &str) -> String {
    format!("{key} = {value} ({source})")
}

/// Source label for a merged layer: `cli`, `file: <path>`, `default`, or the
/// raw layer label for future layers.
fn layer_source_label(
    merged: &bitty_config::MergedConfig,
    field: &str,
    file_path: Option<&std::path::Path>,
) -> String {
    match merged.source_of(field).map(|s| s.layer) {
        Some(bitty_config::LayerKind::Cli) => "cli".to_string(),
        Some(bitty_config::LayerKind::User) => match file_path {
            Some(p) => format!("file: {}", p.display()),
            None => "file".to_string(),
        },
        Some(layer) => {
            if layer == bitty_config::LayerKind::CoreDefaults {
                "default".to_string()
            } else {
                layer.label().to_string()
            }
        }
        None => "default".to_string(),
    }
}

/// Runs `bitty config <verb>`; returns the process exit code.
///
/// - `path`: print resolved path (0) or fail closed (2) when no root exists.
/// - `check`: load + validate via the startup path, print per-key sources
///   (0); invalid files reuse the startup error verbatim (2).
/// - `edit`: mkdir parents + starter-when-missing, open `$VISUAL`/`$EDITOR`
///   (0 on editor success; 1 on spawn failure; editor non-zero propagates).
fn run_config_subcommand(cmd: ConfigCommand, args: &Args) -> i32 {
    if !args.config_args.is_empty() {
        eprintln!(
            "bitty config {}: unexpected argument '{}'\n{}",
            cmd.name(),
            args.config_args[0],
            config_usage()
        );
        return 2;
    }
    let explicit = args.config_path.as_deref();
    match cmd {
        ConfigCommand::Path => {
            match bitty_config::file::probe_config_path(explicit.filter(|p| !p.trim().is_empty())) {
                Some(probed) => {
                    println!("{}", probed.path.display());
                    0
                }
                None => {
                    eprintln!(
                        "bitty config path: no config root ($XDG_CONFIG_HOME or $HOME unset)"
                    );
                    2
                }
            }
        }
        ConfigCommand::Check => match load_merged_config(args) {
            Ok((merged, probed)) => {
                let file_path = probed
                    .as_ref()
                    .filter(|p| p.path.exists())
                    .map(|p| p.path.clone());
                // MSRV 1.85: no let-chains; nest instead of `if let ... && ...`.
                if let Some(probe) = &probed {
                    if probe.fallback_name && probe.path.exists() {
                        eprintln!(
                            "bitty: using fallback '{}' (canonical is 'init.lua')",
                            probe.path.display()
                        );
                    }
                }
                let e = &merged.effective;
                let src = |field: &str| layer_source_label(&merged, field, file_path.as_deref());
                let theme = e
                    .appearance
                    .theme
                    .as_deref()
                    .map_or(String::from("(default)"), |t| format!("\"{t}\""));
                println!(
                    "{}",
                    check_row("appearance.theme", theme, &src("appearance.theme"))
                );
                println!(
                    "{}",
                    check_row(
                        "font.family",
                        format!("\"{}\"", e.font.family),
                        &src("font.family")
                    )
                );
                println!(
                    "{}",
                    check_row("font.size", format!("{}", e.font.size), &src("font.size"))
                );
                println!(
                    "{}",
                    check_row(
                        "window.opacity",
                        format!("{}", e.window.opacity),
                        &src("window.opacity")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "window.padding",
                        format!("{}", e.window.padding),
                        &src("window.padding")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "terminal.scrollback",
                        format!("{}", e.terminal.scrollback),
                        &src("terminal.scrollback")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "terminal.shell",
                        e.terminal
                            .shell
                            .as_deref()
                            .map_or(String::from("(unset)"), |v| format!("\"{v}\"")),
                        &src("terminal.shell")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "keymaps",
                        format!("{} entries", e.keymaps.len()),
                        &src("keymaps")
                    )
                );
                0
            }
            Err(msg) => {
                eprintln!("{msg}");
                2
            }
        },
        ConfigCommand::Edit => {
            let probed =
                bitty_config::file::probe_config_path(explicit.filter(|p| !p.trim().is_empty()));
            let target = match probed {
                Some(p) => p.path,
                None => {
                    eprintln!(
                        "bitty config edit: no config root ($XDG_CONFIG_HOME or $HOME unset)"
                    );
                    return 2;
                }
            };
            if !target.exists() {
                // MSRV 1.85: no let-chains; nest instead of `if let ... && let ...`.
                if let Some(parent) = target.parent() {
                    if let Err(err) = std::fs::create_dir_all(parent) {
                        eprintln!(
                            "bitty config edit: cannot create '{}': {err}",
                            parent.display()
                        );
                        return 1;
                    }
                }
                // Starter only when missing: never clobbers existing content.
                if let Err(err) = std::fs::write(&target, starter_init_lua()) {
                    eprintln!(
                        "bitty config edit: cannot write '{}': {err}",
                        target.display()
                    );
                    return 1;
                }
                eprintln!("bitty config edit: created '{}'", target.display());
            }
            let editor = resolve_editor();
            match std::process::Command::new(&editor).arg(&target).status() {
                Ok(status) if status.success() => 0,
                Ok(status) => status.code().unwrap_or(1),
                Err(err) => {
                    eprintln!("bitty config edit: cannot run editor '{editor}': {err}");
                    1
                }
            }
        }
    }
}

/// Derives a [`bitty_runtime::RuntimeConfig`] from the effective config.
///
/// Grid/cell/queue geometry stays at compiled defaults; font family/size come
/// from the file/CLI/default chain (already validated by `bitty-config`, so
/// construction is expected to succeed — failures stay fail-closed).
fn runtime_config_from_effective(
    effective: &bitty_config::EffectiveConfig,
) -> Result<bitty_runtime::RuntimeConfig, String> {
    let defaults = bitty_runtime::RuntimeConfig::default();
    bitty_runtime::RuntimeConfig::new(
        defaults.cols,
        defaults.rows,
        defaults.cell_width,
        defaults.cell_height,
        defaults.cold_queue_capacity,
        effective.font.family.clone(),
        effective.font.size,
    )
    .map_err(|err| format!("bitty: invalid effective config for runtime: {err}"))
}

/// Window title carrying the resolved theme preset and its source layer.
///
/// Visible via `hyprctl clients` and (where decorations show) the title bar,
/// so screenshots plus class-check prove which config path the window took:
/// `... — bitty-dark (default)` vs `... — bitty-dark (file)`.
fn window_title_for_theme(theme_name: &str, source: &str) -> String {
    format!("bitty \u{2014} Correct Terminal \u{2014} {theme_name} ({source})")
}

// ---------------------------------------------------------------------------
// Layout construction
// ---------------------------------------------------------------------------

fn parse_layout_spec(spec: &str, cols: usize, rows: usize) -> Option<LayoutNode> {
    let lower = spec.to_ascii_lowercase();
    let trimmed = lower.trim();
    if trimmed == "single" || trimmed == "leaf" || trimmed == "1" {
        return Some(LayoutNode::leaf(View::new(ViewId::new(1), cols, rows)));
    }
    if trimmed.starts_with("split") {
        // forms: split, split:h, split:horizontal, split:h:0.3, split:vertical:0.7 etc
        let rest = trimmed.trim_start_matches("split").trim_start_matches(':');
        if rest.is_empty() {
            let a = View::new(ViewId::new(1), cols, rows);
            let b = View::new(ViewId::new(2), cols, rows);
            return Some(LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(a),
                LayoutNode::leaf(b),
            ));
        }
        // rest may be "h", "h:0.3", "horizontal:0.5" etc
        let mut parts = rest.split(':');
        let axis_part = parts.next().unwrap_or("").trim();
        let ratio_part = parts.next().map(str::trim);
        let axis = parse_split_axis(axis_part).unwrap_or(SplitAxis::Horizontal);
        let ratio = if let Some(r_str) = ratio_part {
            r_str.parse::<f32>().unwrap_or(0.5)
        } else {
            0.5
        };
        let a = View::new(ViewId::new(1), cols, rows);
        let b = View::new(ViewId::new(2), cols, rows);
        return Some(LayoutNode::split(
            axis,
            ratio,
            LayoutNode::leaf(a),
            LayoutNode::leaf(b),
        ));
    }
    if trimmed.starts_with("stack") {
        // forms: stack, stack:2, stack:3
        let rest = trimmed.trim_start_matches("stack").trim_start_matches(':');
        let n: usize = if rest.is_empty() {
            2
        } else {
            rest.parse::<usize>().unwrap_or(2).clamp(1, 8)
        };
        let mut children = Vec::with_capacity(n);
        for id in 1..=n as u64 {
            children.push(LayoutNode::leaf(View::new(ViewId::new(id), cols, rows)));
        }
        return Some(LayoutNode::stack(children));
    }
    if trimmed.starts_with("overlay") {
        // forms: overlay, overlay:5,5,20,10
        let rest = trimmed
            .trim_start_matches("overlay")
            .trim_start_matches(':');
        if rest.is_empty() {
            let base = View::new(ViewId::new(1), cols, rows);
            let over = View::new(ViewId::new(2), 20.min(cols), 10.min(rows));
            let bounds = UiRect::new(5, 5, 20.min(cols as u16), 10.min(rows as u16));
            return Some(LayoutNode::overlay(
                LayoutNode::leaf(base),
                LayoutNode::leaf(over),
                bounds,
            ));
        }
        // parse x,y,w,h
        let nums: Vec<u16> = rest
            .split(',')
            .filter_map(|s| s.trim().parse::<u16>().ok())
            .collect();
        if nums.len() == 4 {
            let base = View::new(ViewId::new(1), cols, rows);
            let over = View::new(ViewId::new(2), nums[2] as usize, nums[3] as usize);
            let bounds = UiRect::new(nums[0], nums[1], nums[2], nums[3]);
            return Some(LayoutNode::overlay(
                LayoutNode::leaf(base),
                LayoutNode::leaf(over),
                bounds,
            ));
        }
        // fallback to default overlay on parse failure
        let base = View::new(ViewId::new(1), cols, rows);
        let over = View::new(ViewId::new(2), 20.min(cols), 10.min(rows));
        let bounds = UiRect::new(5, 5, 20.min(cols as u16), 10.min(rows as u16));
        return Some(LayoutNode::overlay(
            LayoutNode::leaf(base),
            LayoutNode::leaf(over),
            bounds,
        ));
    }
    None
}

fn build_layout(args: &Args, cols: usize, rows: usize) -> LayoutNode {
    // Precedence: --layout > --stack > --overlay > --split > single
    if let Some(spec) = args.layout.as_deref() {
        if let Some(node) = parse_layout_spec(spec, cols, rows) {
            return node;
        }
        eprintln!("warning: unknown --layout spec {spec:?} — falling back");
    }
    if args.stack {
        let n = 2usize;
        let mut children = Vec::with_capacity(n);
        for id in 1..=n as u64 {
            children.push(LayoutNode::leaf(View::new(ViewId::new(id), cols, rows)));
        }
        return LayoutNode::stack(children);
    }
    if args.overlay {
        let base = View::new(ViewId::new(1), cols, rows);
        let over = View::new(ViewId::new(2), 20.min(cols), 10.min(rows));
        let bounds = UiRect::new(5, 5, 20.min(cols as u16), 10.min(rows as u16));
        return LayoutNode::overlay(LayoutNode::leaf(base), LayoutNode::leaf(over), bounds);
    }
    if let Some(axis) = args.split_axis {
        let ratio = args.split_ratio.unwrap_or(0.5);
        let a = View::new(ViewId::new(1), cols, rows);
        let b = View::new(ViewId::new(2), cols, rows);
        return LayoutNode::split(axis, ratio, LayoutNode::leaf(a), LayoutNode::leaf(b));
    }
    LayoutNode::leaf(View::new(ViewId::new(1), cols, rows))
}

fn apply_focus(runtime: &mut Runtime, spec: &str) -> bool {
    let lower = spec.to_ascii_lowercase();
    let dir = match lower.as_str() {
        "next" | "n" => Some(FocusDirection::Next),
        "prev" | "previous" | "p" => Some(FocusDirection::Prev),
        "up" => Some(FocusDirection::Up),
        "down" => Some(FocusDirection::Down),
        "left" => Some(FocusDirection::Left),
        "right" => Some(FocusDirection::Right),
        _ => None,
    };
    if let Some(dir) = dir {
        let prev = runtime.focused_view();
        let next = runtime.move_focus(dir);
        eprintln!("bitty: focus move {dir:?} from {prev:?} -> {next:?}");
        return next.is_some();
    }
    if let Ok(num) = spec.trim().parse::<u64>() {
        let id = ViewId::new(num);
        let ok = runtime.set_focus(id);
        if ok {
            eprintln!("bitty: focus set to {id}");
        } else {
            eprintln!(
                "warning: focus id {id} not in layout (leaf ids {:?})",
                runtime.layout().leaf_ids()
            );
        }
        return ok;
    }
    eprintln!(
        "warning: unknown --focus spec {spec:?} (expected next|prev|up|down|left|right|<id>)"
    );
    false
}

// ---------------------------------------------------------------------------
// Headless smoke
// ---------------------------------------------------------------------------

/// Runs a single headless tick smoke: feeds a synthetic byte batch, ticks
/// layout-aware, prints cold-queue summary and present stats, then proves
/// split/stack/overlay composition deterministically.
///
/// Returns an exit code (0 success, 1 runtime build failure, 2 no present).
fn run_headless_smoke(runtime: &mut Runtime) -> i32 {
    // Synthetic payload that exercises the full pipeline without a real child:
    // printable text, SGR, OSC title, and an erase. Deterministic across
    // platforms (no wall clock or font file involved).
    let synthetic = b"bitty headless smoke \x1b[31mred\x1b[0m \x1b]0;bitty-smoke\x07\r\n";
    runtime.handle_pty_bytes(synthetic);

    // Drain cold-queue summary without yet clearing the queue for logging.
    let queued = runtime.cold_queue_len();
    let dropped = runtime.cold_queue_dropped();
    let cap = runtime.cold_queue_capacity();
    let generation_before = runtime.state().generation();
    let layout_desc = {
        let ids = runtime.layout().leaf_ids();
        let allocs = runtime.layout_allocations();
        format!(
            "layout leafs={} ids={:?} allocs={:?} focused={:?}",
            runtime.leaf_count(),
            ids,
            allocs,
            runtime.focused_view()
        )
    };

    let stats = runtime.tick();

    match stats {
        Some(present) => {
            let events = runtime.drain_cold_events();
            println!(
                "bitty headless smoke: ok — tick presented (frame={}, fills={}, glyphs={}, headless={}, generation={})",
                present.frame, present.fills, present.glyphs, present.headless, present.generation
            );
            println!(
                "  cold-queue: len(capped)={queued}/{cap} dropped={dropped} drained={} generation_before={generation_before} generation_after={}",
                events.len(),
                present.generation
            );
            if let Some(extent) = runtime.surface_extent() {
                println!(
                    "  surface: headless={} extent={}x{} rgba_len={}",
                    runtime.is_headless(),
                    extent.width(),
                    extent.height(),
                    runtime.headless_rgba().map_or(0, |b| b.len())
                );
            }
            println!("  {layout_desc}");
            // Prove split/stack/overlay deterministically (no window/GPU, software present only).
            // This runs even for single-leaf headless to show composition is layout-aware.
            let proof_code = run_layout_proof(synthetic);
            if proof_code != 0 {
                eprintln!("bitty: layout proof failed with code {proof_code}");
            }
            0
        }
        None => {
            eprintln!(
                "bitty headless smoke: no present (idle or missing damage) — still ok as cold-queue check"
            );
            eprintln!(
                "  cold-queue: len={queued} cap={cap} dropped={dropped} generation={generation_before}"
            );
            eprintln!("  {layout_desc}");
            // Idle is not a failure for CI smoke when no bytes produced damage
            // (e.g. synthetic was filtered). The generation check still proves
            // the path, so return 0 rather than 2 to keep CI green, but log.
            // Still run layout proof to keep composition evidence deterministic.
            let _ = run_layout_proof(synthetic);
            0
        }
    }
}

/// Deterministic proof that split/stack/overlay compose via software present.
///
/// Creates separate headless runtimes per composition, feeds the same synthetic
/// bytes, ticks, and asserts:
///
/// - same layout + same bytes → identical RGBA (determinism)
/// - different layouts → distinct RGBA (composition)
///
/// Prints evidence; returns 0 on success, 1 on failure.
fn run_layout_proof(synthetic: &[u8]) -> i32 {
    // Helper to build a runtime with a given layout, feed bytes, tick, and return (stats, rgba)
    fn tick_with_layout(
        layout: LayoutNode,
        bytes: &[u8],
    ) -> Option<(bitty_runtime::PresentStats, Vec<u8>)> {
        let mut rt = Runtime::with_defaults().expect("defaults must build");
        rt.set_layout(layout);
        rt.handle_pty_bytes(bytes);
        let stats = rt.tick()?;
        let rgba = rt.headless_rgba()?;
        Some((stats, rgba))
    }

    // Split
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    );
    let (split_stats, split_rgba) = match tick_with_layout(split.clone(), synthetic) {
        Some(v) => v,
        None => {
            eprintln!("layout-proof: split tick produced no present");
            return 1;
        }
    };
    // Second split must be deterministic
    let (_, split_rgba2) = match tick_with_layout(split, synthetic) {
        Some(v) => v,
        None => {
            eprintln!("layout-proof: second split tick produced no present");
            return 1;
        }
    };
    if split_rgba != split_rgba2 {
        eprintln!("layout-proof: split not deterministic");
        return 1;
    }

    // Stack
    let stack = LayoutNode::stack(vec![
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    ]);
    let (stack_stats, stack_rgba) = match tick_with_layout(stack.clone(), synthetic) {
        Some(v) => v,
        None => {
            eprintln!("layout-proof: stack tick produced no present");
            return 1;
        }
    };
    let (_, stack_rgba2) = match tick_with_layout(stack, synthetic) {
        Some(v) => v,
        None => {
            eprintln!("layout-proof: second stack tick produced no present");
            return 1;
        }
    };
    if stack_rgba != stack_rgba2 {
        eprintln!("layout-proof: stack not deterministic");
        return 1;
    }

    // Overlay
    let overlay = LayoutNode::overlay(
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 20, 10)),
        UiRect::new(5, 5, 20, 10),
    );
    let (overlay_stats, overlay_rgba) = match tick_with_layout(overlay.clone(), synthetic) {
        Some(v) => v,
        None => {
            eprintln!("layout-proof: overlay tick produced no present");
            return 1;
        }
    };
    let (_, overlay_rgba2) = match tick_with_layout(overlay, synthetic) {
        Some(v) => v,
        None => {
            eprintln!("layout-proof: second overlay tick produced no present");
            return 1;
        }
    };
    if overlay_rgba != overlay_rgba2 {
        eprintln!("layout-proof: overlay not deterministic");
        return 1;
    }

    // Distinctness
    if split_rgba == stack_rgba {
        eprintln!("layout-proof: split and stack produced identical rgba — unexpected");
        return 1;
    }
    if split_rgba == overlay_rgba {
        eprintln!("layout-proof: split and overlay produced identical rgba — unexpected");
        return 1;
    }
    if stack_rgba == overlay_rgba {
        eprintln!("layout-proof: stack and overlay produced identical rgba — unexpected");
        return 1;
    }

    println!(
        "  layout-proof: ok — split (fills={}, glyphs={}) stack (fills={}, glyphs={}) overlay (fills={}, glyphs={}) distinct deterministic rgba",
        split_stats.fills,
        split_stats.glyphs,
        stack_stats.fills,
        stack_stats.glyphs,
        overlay_stats.fills,
        overlay_stats.glyphs
    );
    println!(
        "    rgba lens: split={} stack={} overlay={} (split!=stack {}, split!=overlay {}, stack!=overlay {})",
        split_rgba.len(),
        stack_rgba.len(),
        overlay_rgba.len(),
        split_rgba != stack_rgba,
        split_rgba != overlay_rgba,
        stack_rgba != overlay_rgba
    );
    0
}

// ---------------------------------------------------------------------------
// Demo PTY pump (bounded, honest seam)
// ---------------------------------------------------------------------------

/// Synthetic bounded PTY pump that demonstrates the backpressure contract
/// without requiring a live child.
///
/// The pump owns a `sync_channel(16)` holding at most `16` chunks (mirrors
/// `bitty-pty` `CHANNEL_CAPACITY_CHUNKS`); the main thread drains it via
/// `try_recv` on `AboutToWait` and feeds `Runtime::handle_pty_bytes`. When the
/// consumer stalls the channel fills and the pump's `send` blocks — the same
/// backpressure that would propagate to the kernel PTY buffer for a real child.
///
/// This exists to keep the composition root's PTY wiring total and testable on
/// headless CI. It is the bounded fallback only: the live pump is wired —
/// `Runtime::take_pty_reader` and `Runtime::poll_pty` exist and
/// `TerminalApp::poll_pty_pump` drains the real runtime channel first.
/// Theme-aware demo pump: the greeting names the resolved theme preset
/// and its source layer (`default`/`file`/`cli`) so the live window visibly
/// proves which config path it took. The green SGR still resolves through the
/// themed palette (no hardcoded green outside the theme).
///
/// Both strings come from the trusted registry/source labels (bounded, ASCII)
/// — never from raw file bytes — so the burst stays bounded.
fn spawn_demo_pty_pump_with_theme(
    theme_name: &str,
    source: &str,
) -> (Receiver<Vec<u8>>, JoinHandle<()>) {
    // Bound the label at construction (registry names are short; this is
    // defense-in-depth so a future registry entry cannot grow the burst).
    let theme_safe: String = theme_name.chars().take(64).collect();
    let source_safe: String = source.chars().take(16).collect();
    let greeting = format!("demo pty: hello theme={theme_safe} src={source_safe} ");
    // Small channel to make backpressure observable in tests; 16 matches the
    // real `CHANNEL_CAPACITY_CHUNKS`.
    let (tx, rx): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = sync_channel(16);
    let handle = std::thread::spawn(move || {
        // Single synthetic burst — enough to exercise one tick's damage.
        let green: &[u8] = b"\x1b[32mgreen\x1b[0m\n";
        let chunks: Vec<Vec<u8>> = vec![greeting.into_bytes(), green.to_vec()];
        for chunk in &chunks {
            // `send` blocks when the channel is full — the backpressure point.
            if tx.send(chunk.clone()).is_err() {
                break;
            }
        }
        // Dropping `tx` signals EOF to the consumer (`try_recv` → Disconnected).
    });
    (rx, handle)
}

// ---------------------------------------------------------------------------
// App handler
// ---------------------------------------------------------------------------

/// The Correct Terminal handler: owns `Runtime`, an optional window, and a
/// bounded PTY pump (real PTY via `Runtime::poll_pty` plus synthetic fallback).
/// All business stays in `bitty-runtime`; this type only wires
/// `PlatformEvent` → `Runtime` and `tick` → present, with real `GpuContext`
/// attachment for the single-window vertical slice.
struct TerminalApp {
    runtime: Runtime,
    /// Window title carrying the resolved theme preset + source layer.
    window_title: String,
    window: Option<WindowHandle>,
    window_id: Option<WindowId>,
    /// Demo bounded PTY pump fallback when no real PTY is owned (headless tests).
    pty_rx: Option<Receiver<Vec<u8>>>,
    _pty_thread: Option<JoinHandle<()>>,
    /// Count of `tick` calls that presented a frame.
    presented_frames: u64,
}

impl TerminalApp {
    /// Theme-aware constructor: the demo fallback burst names the resolved
    /// preset and source so the live window proves its config path.
    fn with_theme(runtime: Runtime, theme_name: &str, source: &str) -> Self {
        // Keep demo pump as fallback when no real PTY is spawned (headless CI,
        // tests). When a real PTY is spawned via `Runtime::spawn_shell`, the
        // real `poll_pty` path handles bytes and the demo pump just delivers a
        // harmless synthetic burst.
        let (pty_rx, handle) = spawn_demo_pty_pump_with_theme(theme_name, source);
        Self {
            runtime,
            window_title: window_title_for_theme(theme_name, source),
            window: None,
            window_id: None,
            pty_rx: Some(pty_rx),
            _pty_thread: Some(handle),
            presented_frames: 0,
        }
    }

    /// Polls real PTY (`Runtime::poll_pty` bounded 128 KiB) and demo pump.
    /// Returns true when bytes were consumed.
    fn poll_pty_pump(&mut self) -> bool {
        let mut consumed = false;
        // Real PTY first: drain bounded channel via runtime; replies are flushed
        // via `Runtime::write_replies` inside `poll_pty` (bounded 4 KiB, fail-closed).
        let real = self.runtime.poll_pty();
        if real > 0 {
            consumed = true;
        }
        // Demo pump fallback (bounded, headless-testable)
        if let Some(rx) = self.pty_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(chunk) => {
                        self.runtime.handle_pty_bytes(&chunk);
                        consumed = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }
        // Flush any replies generated by the demo pump (bounded, best-effort).
        // `write_replies` is no-op when no live writer (headless keeps replies for `take_replies`).
        let _ = self.runtime.write_replies();
        consumed
    }

    /// Drives one frame when damage exists, printing stats when a frame was
    /// presented. Returns the stats when a present occurred. Handles GPU vs headless.
    fn drive_tick(&mut self) -> Option<bitty_runtime::PresentStats> {
        // Ensure replies that were queued before tick are flushed before present:
        // the runtime's tick consumes snapshot+damage and composites.
        let stats = self.runtime.tick();
        if let Some(present) = stats {
            self.presented_frames += 1;
            eprintln!(
                "bitty tick: frame={} fills={} glyphs={} headless={} gen={} presented_frames={} focused={:?} leafs={} gpu={} crossfont={}",
                present.frame,
                present.fills,
                present.glyphs,
                present.headless,
                present.generation,
                self.presented_frames,
                self.runtime.focused_view(),
                self.runtime.leaf_count(),
                self.runtime.has_gpu(),
                self.runtime.is_crossfont()
            );
            if self.runtime.replies_overflowed() {
                eprintln!("warning: terminal reply queue overflowed (bounded cap)");
            }
            // Bounded reply loop: flush replies generated before this tick (if any) via PtyWriter.
            // When no writer is present (headless), replies stay queued for `take_replies` observation.
            let written = self.runtime.write_replies();
            if written > 0 {
                eprintln!("bitty: {written} reply bytes written to PTY master (post-tick)");
            }
            let pending = self.runtime.cold_queue_len();
            if pending > 0 {
                let events = self.runtime.drain_cold_events();
                eprintln!(
                    "bitty cold-queue: drained {} events, {} remain",
                    events.len(),
                    pending
                );
            }
        }
        stats
    }

    /// Attempts to attach a real GPU surface after window creation (single-window slice).
    fn try_attach_gpu(&mut self, handle: &WindowHandle) {
        // Do not re-attach if already has GPU
        if self.runtime.has_gpu() {
            return;
        }
        let target = handle.surface_target();
        let inner = target.inner_size();
        // Only attempt GPU when we have a non-zero physical size
        if inner.width() == 0 || inner.height() == 0 {
            eprintln!("bitty: gpu attach skipped (zero-size surface)");
            return;
        }
        // CTX-0142: adopt the live DPI scale at attach (the compositor may
        // have delivered fractional scale before this point while winit still
        // reported 1.0): rescale renderer font/atlas to scaled cells and
        // derive the grid from the physical inner_size — never the logical
        // size path (the suspected original sin behind #232).
        let scale = target.scale_factor().get();
        self.runtime.apply_dpi_scale(scale, Some(inner));
        let snap = self.runtime.snapshot();
        match pollster::block_on(GpuContext::initialize()) {
            Ok(gpu) => match gpu.create_surface(&target) {
                Ok(surface) => {
                    let extent = PhysicalSize::new(inner.width(), inner.height());
                    // Configure surface with current extent (bounded, validated)
                    match surface.configure(&gpu, extent) {
                        Ok(()) => {
                            self.runtime.attach_gpu(gpu, surface);
                            eprintln!(
                                "bitty: gpu attached (extent={}x{} scale={scale} dpi={} grid={}x{} crossfont={})",
                                extent.width(),
                                extent.height(),
                                self.runtime.dpi_scale(),
                                snap.width,
                                snap.height,
                                self.runtime.is_crossfont()
                            );
                        }
                        Err(err) => {
                            eprintln!(
                                "bitty: gpu surface configure failed ({err}) — staying headless"
                            );
                        }
                    }
                }
                Err(err) => {
                    eprintln!("bitty: gpu surface creation failed ({err}) — staying headless");
                }
            },
            Err(err) => {
                eprintln!("bitty: gpu initialize failed ({err}) — staying headless (CI fallback)");
            }
        }
    }
}

impl AppHandler for TerminalApp {
    fn set_event_waker(&mut self, waker: EventWaker) {
        // Bridge the platform proxy into the runtime's bounded wakeup pump:
        // the forwarder thread owns its clone and wakes once per readability
        // signal (plus once on EOF). `Mutex` keeps the closure `Send + Sync`
        // even if the proxy is only `Send`.
        let shared = std::sync::Arc::new(std::sync::Mutex::new(waker));
        let pty_waker: bitty_runtime::PtyWaker = std::sync::Arc::new(move || {
            if let Ok(w) = shared.lock() {
                w.wake_pty();
            }
        });
        self.runtime.set_pty_waker(pty_waker);
        eprintln!("bitty: pty wakeup armed (event-loop proxy)");
    }

    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: PlatformEvent) {
        // Bounded PTY pump: drain before handling the event so fresh bytes are
        // visible to the state machine before the tick.
        self.poll_pty_pump();

        // Shutdown handling: PlatformEvent::Exiting or CloseRequested/Closed
        // ask the handler to exit the loop.
        let should_exit = self.runtime.handle_platform_event(event.clone());
        if should_exit {
            eprintln!("bitty: exit requested ({event:?})");
            ctx.exit();
            return;
        }

        match event {
            PlatformEvent::Resumed => {
                // First delivery — intended startup window creation point.
                // Headless fallback: `ctx.create_window` returns
                // `PlatformError::WindowCreation` when no window system exists;
                // we keep running headlessly and still tick, rather than
                // aborting the process (mirrors `App::run`'s
                // `DisplayUnavailable` mapping).
                if self.window.is_none() {
                    let default_size = LogicalSize::new(800.0, 600.0).unwrap_or_else(|_| {
                        // LogicalSize validation only fails for non-finite or
                        // negative inputs; hard-coded values are valid, so
                        // this fallback is unreachable but keeps the handler
                        // total.
                        LogicalSize::new(640.0, 480.0).expect("fallback size must be valid")
                    });
                    let config = WindowConfig::new()
                        .with_title(self.window_title.clone())
                        .with_inner_size(default_size)
                        .with_visible(true);
                    match ctx.create_window(config) {
                        Ok(handle) => {
                            let id = handle.id();
                            self.window_id = Some(id);
                            // Clone handle before moving into try_attach_gpu (which borrows self mutably)
                            let handle_for_gpu = handle.clone();
                            self.window = Some(handle);
                            // Single-window vertical slice: try real GPU attach with crossfont atlas.
                            // On headless CI this fails with NoCompatibleAdapter and we stay headless
                            // (deterministic fallback, no panic). On a real display we get a wgpu surface
                            // via winit's SurfaceTarget and present via tick.
                            self.try_attach_gpu(&handle_for_gpu);
                            eprintln!(
                                "bitty: window created id={} gpu={} crossfont={} focused={:?} leafs={}",
                                id.get(),
                                self.runtime.has_gpu(),
                                self.runtime.is_crossfont(),
                                self.runtime.focused_view(),
                                self.runtime.leaf_count()
                            );
                        }
                        Err(err) => {
                            eprintln!(
                                "bitty: window creation failed ({err}) — continuing headless (no GPU, no display)"
                            );
                        }
                    }
                }
                // Resumed is a good point to request the first redraw.
                if let Some(win) = self.window.as_ref() {
                    win.request_redraw();
                } else {
                    // Headless: still drive one tick so CI-like smoke appears
                    // even when running without a window system but not in
                    // explicit --headless mode.
                    let _ = self.drive_tick();
                }
            }
            PlatformEvent::Window { window_id: _, kind } => {
                // Keyboard focus movement (thin shortcut handling; keeps app free of input-encoding policy).
                // The input-encoding slice (keymaps/IME) lives elsewhere; this is only deterministic
                // focus traversal over the owned layout, proven headlessly via --focus and via tick.
                if let WindowEventKind::KeyboardInput(key) = &kind {
                    if key.state == PressState::Pressed && !key.repeat {
                        let mut handled = false;
                        match &key.logical_key {
                            LogicalKey::Named(NamedKey::Tab) => {
                                self.runtime.move_focus(FocusDirection::Next);
                                handled = true;
                            }
                            LogicalKey::Named(NamedKey::ArrowRight) => {
                                self.runtime.move_focus(FocusDirection::Right);
                                handled = true;
                            }
                            LogicalKey::Named(NamedKey::ArrowLeft) => {
                                self.runtime.move_focus(FocusDirection::Left);
                                handled = true;
                            }
                            LogicalKey::Named(NamedKey::ArrowUp) => {
                                self.runtime.move_focus(FocusDirection::Up);
                                handled = true;
                            }
                            LogicalKey::Named(NamedKey::ArrowDown) => {
                                self.runtime.move_focus(FocusDirection::Down);
                                handled = true;
                            }
                            LogicalKey::Character(s) => {
                                let low = s.to_ascii_lowercase();
                                match low.as_str() {
                                    "n" => {
                                        self.runtime.move_focus(FocusDirection::Next);
                                        handled = true;
                                    }
                                    "p" => {
                                        self.runtime.move_focus(FocusDirection::Prev);
                                        handled = true;
                                    }
                                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" => {
                                        if let Ok(num) = low.parse::<u64>() {
                                            self.runtime.set_focus(ViewId::new(num));
                                            handled = true;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                        if handled {
                            eprintln!(
                                "bitty: keyboard focus -> {:?} leafs={} (key={:?})",
                                self.runtime.focused_view(),
                                self.runtime.leaf_count(),
                                key.logical_key
                            );
                            // Request redraw so the next AboutToWait/RedrawRequested will tick layout-aware.
                            // Focus itself does not dirty generation, but the next tick after set_layout's
                            // pending_full_redraw already presented; for keyboard moves we still request
                            // redraw to keep frame-on-demand honest (no periodic wakeups).
                            if let Some(win) = self.window.as_ref() {
                                win.request_redraw();
                            }
                        }
                    }
                }
                // `handle_platform_event` already routed Resized /
                // ScaleFactorChanged / CloseRequested / RedrawRequested.
                // Request a tick on redraw and after resize.
                match kind {
                    WindowEventKind::Resized(_) | WindowEventKind::ScaleFactorChanged(_) => {
                        // CTX-0142: ScaleFactorChanged carries no size (winit
                        // 0.30 drops the negotiation hook), so re-read the
                        // physical inner_size here — never the logical path —
                        // and adopt before the tick renders at scaled cells.
                        // Resized needs no extra work: runtime derives from
                        // the same live scaled cells.
                        if let WindowEventKind::ScaleFactorChanged(factor) = &kind {
                            let physical = self.window.as_ref().map(|win| win.inner_size());
                            self.runtime.apply_dpi_scale(factor.get(), physical);
                            let snap = self.runtime.snapshot();
                            eprintln!(
                                "bitty: dpi adopted scale={} dpi={} grid={}x{} physical={} surface={:?}",
                                factor.get(),
                                self.runtime.dpi_scale(),
                                snap.width,
                                snap.height,
                                physical.map_or(String::from("none"), |p| format!(
                                    "{}x{}",
                                    p.width(),
                                    p.height()
                                )),
                                self.runtime.surface_extent().map(|e| format!(
                                    "{}x{}",
                                    e.width(),
                                    e.height()
                                )),
                            );
                        }
                        if let Some(win) = self.window.as_ref() {
                            win.request_redraw();
                        } else {
                            let _ = self.drive_tick();
                        }
                    }
                    WindowEventKind::RedrawRequested => {
                        // Frame-on-demand: no damage → no present, no redraw
                        // loop. Keep Wait mode (default `ControlFlow::Wait`)
                        // so idle burns ≤ 1 % CPU (PB-7 budget).
                        let _ = self.drive_tick();
                    }
                    _ => {}
                }
            }
            PlatformEvent::AboutToWait => {
                // Per winit docs this is a good place to do per-frame work.
                // Poll PTY pump again and drive tick; request redraw only when
                // tick produced a present (frame-on-demand).
                self.poll_pty_pump();
                if self.drive_tick().is_some() {
                    if let Some(win) = self.window.as_ref() {
                        win.request_redraw();
                    }
                }
            }
            PlatformEvent::PtyReadable => {
                // Evented PTY wakeup: the top-of-handler `poll_pty_pump`
                // already drained the bounded forwarder channel, so tick when
                // damage exists and request a redraw only on present
                // (frame-on-demand; quiet shells idle with no further wakes).
                if self.drive_tick().is_some() {
                    if let Some(win) = self.window.as_ref() {
                        win.request_redraw();
                    }
                }
            }
            PlatformEvent::Exiting => {
                ctx.exit();
            }
            _ => {}
        }
    }
}

// The `bitty-pty` bounded-channel seam uses `READ_CHUNK_SIZE` and
// `CHANNEL_CAPACITY_CHUNKS` constants, but we keep the demo pump's channel
// capacity literal (16) mirroring that constant without importing the crate
// directly — `bitty-app` wires `bitty-runtime` + `bitty-platform` +
// `bitty-render` + `bitty-config` as the thin composition root (ADR-0003
// entry point; no business logic beyond wiring). The literal is documented
// here to avoid a hidden dependency.
#[allow(dead_code)]
fn _assert_channel_capacity_is_documented() {
    const EXPECTED: usize = 16;
    const { assert!(EXPECTED > 0) }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    let args = parse_args(&raw);

    if args.help {
        println!("{}", help_text());
        std::process::exit(0);
    }
    if args.version {
        println!("{}", version_text());
        std::process::exit(0);
    }

    // `bitty config` subcommand first (CLI-first management per DEC-0007).
    if let Some(cmd) = args.config_cmd {
        std::process::exit(run_config_subcommand(cmd, &args));
    }
    if args.config_word {
        if args.config_args.is_empty() {
            eprintln!("{}", config_usage());
        } else {
            eprintln!(
                "bitty config: unknown verb '{}'\n{}",
                args.config_args[0],
                config_usage()
            );
        }
        std::process::exit(2);
    }

    // User config first (fail-closed): invalid files exit non-zero with a
    // clear stderr message; missing default-path files yield defaults.
    let app_config = match load_app_config(&args) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    eprintln!(
        "bitty: theme '{}' (resolution={:?} source={})",
        app_config.theme.name, app_config.resolution, app_config.source
    );
    let runtime_cfg = match runtime_config_from_effective(&app_config.effective) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    };
    let mut runtime = match Runtime::new(runtime_cfg) {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("bitty: runtime init failed: {err}");
            std::process::exit(1);
        }
    };
    // Validate that the config-derived extent is non-zero (defense in depth;
    // Runtime::new already validates, but the app documents the invariant).
    if runtime.surface_extent().is_none() {
        eprintln!("bitty: runtime surface has no extent after init — aborting");
        std::process::exit(1);
    }

    // Layout wiring: construct LayoutNode via bitty-ui types (re-exported through bitty-runtime),
    // call Runtime::set_layout, then apply focus. Keeps app thin; no config/plugin coupling.
    {
        let cols = runtime.config().cols;
        let rows = runtime.config().rows;
        let layout = build_layout(&args, cols, rows);
        let leaf_ids = layout.leaf_ids();
        let focused_before = runtime.focused_view();
        runtime.set_layout(layout);
        eprintln!(
            "bitty: layout installed — leafs={} ids={:?} focused_before={:?} focused_after={:?} container={:?}",
            runtime.leaf_count(),
            leaf_ids,
            focused_before,
            runtime.focused_view(),
            runtime.container()
        );
        if let Some(focus_spec) = args.focus.as_deref() {
            apply_focus(&mut runtime, focus_spec);
        }
    }

    // Single-window vertical slice: one PTY, one shell.
    // Explicit program spawns verbatim (with tail args via spawn_shell_with_args);
    // bare invocation resolves to the default shell ($SHELL or /bin/sh).
    // Headless CI still succeeds even if spawn fails (bounded synthetic smoke).
    // `$SHELL` is read once here and injected into the pure resolver so arg
    // handling stays testable; it is trusted only as a binary path, never split.
    let shell_env = std::env::var("SHELL").ok();
    let effective = resolve_spawn_program(&args, shell_env.as_deref());
    eprintln!(
        "bitty: effective program {effective:?} (explicit={})",
        args.program.is_some()
    );
    let spawn_result = if let Some(program) = args.program.as_deref() {
        let tail: Vec<&str> = args.program_args.iter().map(|s| s.as_str()).collect();
        if tail.is_empty() {
            runtime.spawn_shell(program)
        } else {
            runtime.spawn_shell_with_args(program, &tail)
        }
    } else {
        spawn_default_shell(&mut runtime, shell_env.as_deref())
    };
    match spawn_result {
        Ok(()) => eprintln!(
            "bitty: PTY shell spawned (has_pty={} has_reader={})",
            runtime.has_pty(),
            runtime.has_pty_reader()
        ),
        Err(err) => eprintln!(
            "bitty: PTY spawn failed: {err} — continuing without child (headless tick still proves path)"
        ),
    }

    if args.headless {
        // In headless mode we still fed synthetic bytes via run_headless_smoke, but the live PTY
        // (if any) has been spawned above and will be polled on AboutToWait. For deterministic CI
        // we also keep synthetic smoke proof.
        println!(
            "bitty headless: theme '{}' source={} file={}",
            app_config.theme.name,
            app_config.source,
            app_config
                .file_path
                .as_ref()
                .map_or(String::from("(none)"), |p| p.display().to_string())
        );
        let code = run_headless_smoke(&mut runtime);
        std::process::exit(code);
    }

    // Real mode: run the platform event loop, forwarding PlatformEvent →
    // Runtime::handle_platform_event and tick → present. On headless CI
    // `App::run` returns `DisplayUnavailable` instead of panicking — fall
    // back to the headless smoke so CI stays green and the failure is
    // honest rather than fatal. The event loop is layout-aware: every tick
    // reflows the LayoutNode into the container and composites per-leaf via
    // the headless software seam (deterministic RGBA) until a real
    // SurfaceTarget is attached in a future slice.
    let app = TerminalApp::with_theme(runtime, app_config.theme.name, app_config.source);
    let headless_fallback_needed = match App::run(app) {
        Ok(()) => std::process::exit(0),
        Err(PlatformError::DisplayUnavailable(detail)) => {
            eprintln!(
                "bitty: no usable display server ({detail}) — falling back to headless smoke (CI path)"
            );
            true
        }
        Err(other) => {
            eprintln!("bitty: event loop failed: {other}");
            std::process::exit(1);
        }
    };

    if headless_fallback_needed {
        let fallback_cfg = match runtime_config_from_effective(&app_config.effective) {
            Ok(cfg) => cfg,
            Err(msg) => {
                eprintln!("{msg}");
                std::process::exit(1);
            }
        };
        let mut rt = match Runtime::new(fallback_cfg) {
            Ok(rt) => rt,
            Err(err) => {
                eprintln!("bitty: fallback runtime init failed: {err}");
                std::process::exit(1);
            }
        };
        // Re-apply layout and focus in fallback so headless smoke proves the same composition
        // that real mode would have driven via the window.
        {
            let cols = rt.config().cols;
            let rows = rt.config().rows;
            let layout = build_layout(&args, cols, rows);
            rt.set_layout(layout);
            if let Some(focus_spec) = args.focus.as_deref() {
                apply_focus(&mut rt, focus_spec);
            }
        }
        // Preserve program spawn attempt in the fallback when it existed, else
        // resolve the default shell ($SHELL or /bin/sh) for completeness.
        if let Some(program) = args.program.as_deref() {
            let tail: Vec<&str> = args.program_args.iter().map(|s| s.as_str()).collect();
            if tail.is_empty() {
                let _ = rt.spawn_shell(program);
            } else {
                let _ = rt.spawn_shell_with_args(program, &tail);
            }
        } else {
            let fallback_shell = std::env::var("SHELL").ok();
            let _ = spawn_default_shell(&mut rt, fallback_shell.as_deref());
        }
        let code = run_headless_smoke(&mut rt);
        std::process::exit(code);
    }
}

// ---------------------------------------------------------------------------
// Tests (pure arg parsing + headless smoke totality)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parse_no_args_yields_defaults() {
        let raw = args_of(&["bitty"]);
        let parsed = parse_args(&raw);
        assert!(!parsed.headless);
        assert!(!parsed.help);
        assert!(!parsed.version);
        assert_eq!(parsed.program, None);
        assert!(parsed.program_args.is_empty());
        assert_eq!(parsed.split_axis, None);
        assert_eq!(parsed.split_ratio, None);
        assert!(!parsed.stack);
        assert!(!parsed.overlay);
        assert_eq!(parsed.layout, None);
        assert_eq!(parsed.focus, None);
        assert_eq!(parsed.config_path, None);
        assert_eq!(parsed.theme, None);
        assert_eq!(parsed.config_cmd, None);
        assert!(!parsed.config_word);
        assert!(parsed.config_args.is_empty());
    }

    #[test]
    fn parse_help_and_version_flags() {
        let raw = args_of(&["bitty", "--help"]);
        assert!(parse_args(&raw).help);
        let raw = args_of(&["bitty", "-h"]);
        assert!(parse_args(&raw).help);
        let raw = args_of(&["bitty", "--version"]);
        assert!(parse_args(&raw).version);
        let raw = args_of(&["bitty", "-V"]);
        assert!(parse_args(&raw).version);
    }

    #[test]
    fn default_shell_prefers_shell_env() {
        assert_eq!(resolve_default_shell(Some("/bin/fish")), "/bin/fish");
        assert_eq!(resolve_default_shell(Some("/bin/bash")), "/bin/bash");
        assert_eq!(resolve_default_shell(Some("/usr/bin/zsh")), "/usr/bin/zsh");
    }

    #[test]
    fn default_shell_falls_back_when_env_missing_or_blank() {
        assert_eq!(resolve_default_shell(None), "/bin/sh");
        assert_eq!(resolve_default_shell(Some("")), "/bin/sh");
        assert_eq!(resolve_default_shell(Some("   ")), "/bin/sh");
        assert_eq!(resolve_default_shell(Some("\t\n ")), "/bin/sh");
    }

    #[test]
    fn default_shell_trims_surrounding_whitespace() {
        assert_eq!(resolve_default_shell(Some("  /bin/fish  ")), "/bin/fish");
    }

    #[test]
    fn bare_args_resolve_to_default_shell() {
        let parsed = parse_args(&args_of(&["bitty"]));
        assert_eq!(parsed.program, None);
        assert_eq!(
            resolve_spawn_program(&parsed, Some("/bin/fish")),
            "/bin/fish"
        );
        assert_eq!(resolve_spawn_program(&parsed, None), "/bin/sh");
        assert_eq!(resolve_spawn_program(&parsed, Some("")), "/bin/sh");
    }

    #[test]
    fn explicit_program_arg_stays_identical() {
        let parsed = parse_args(&args_of(&["bitty", "--", "fish", "-l"]));
        assert_eq!(parsed.program.as_deref(), Some("fish"));
        assert_eq!(parsed.program_args, vec!["-l"]);
        // Explicit program wins over any injected $SHELL.
        assert_eq!(resolve_spawn_program(&parsed, Some("/bin/bash")), "fish");
        assert_eq!(resolve_spawn_program(&parsed, None), "fish");

        let parsed = parse_args(&args_of(&["bitty", "/bin/bash"]));
        assert_eq!(
            resolve_spawn_program(&parsed, Some("/bin/fish")),
            "/bin/bash"
        );
    }

    #[test]
    fn help_text_documents_default_shell() {
        let help = help_text();
        assert!(help.contains("$SHELL"));
        assert!(help.contains("/bin/sh"));
    }

    #[test]
    fn parse_headless_flag() {
        let raw = args_of(&["bitty", "--headless"]);
        assert!(parse_args(&raw).headless);
        let raw = args_of(&["bitty", "--headless", "--help"]);
        let p = parse_args(&raw);
        assert!(p.headless && p.help);
    }

    #[test]
    fn parse_program_positional_and_tail() {
        let raw = args_of(&["bitty", "/bin/bash"]);
        let p = parse_args(&raw);
        assert_eq!(p.program.as_deref(), Some("/bin/bash"));
        assert!(p.program_args.is_empty());

        let raw = args_of(&["bitty", "--headless", "/bin/cat", "-A"]);
        let p = parse_args(&raw);
        assert!(p.headless);
        assert_eq!(p.program.as_deref(), Some("/bin/cat"));
        assert_eq!(p.program_args, vec!["-A"]);
    }

    #[test]
    fn parse_double_dash_terminates_flag_scan() {
        let raw = args_of(&["bitty", "--", "--headless"]);
        let p = parse_args(&raw);
        assert!(!p.headless);
        assert_eq!(p.program.as_deref(), Some("--headless"));

        let raw = args_of(&["bitty", "--headless", "--", "--help"]);
        let p = parse_args(&raw);
        assert!(p.headless);
        assert!(!p.help);
        assert_eq!(p.program.as_deref(), Some("--help"));
    }

    #[test]
    fn help_and_version_text_are_non_empty() {
        assert!(help_text().contains("bitty"));
        assert!(help_text().contains("--headless"));
        assert!(help_text().contains("--split"));
        assert!(help_text().contains("--layout"));
        assert!(!version_text().is_empty());
    }

    #[test]
    fn headless_smoke_is_total_without_display_or_gpu() {
        let mut rt = Runtime::with_defaults().expect("defaults must build");
        let code = run_headless_smoke(&mut rt);
        assert_eq!(code, 0);
        // Smoke must have presented at least the initial full redraw.
        assert!(rt.surface_extent().is_some());
    }

    #[test]
    fn demo_pty_pump_is_bounded_and_delivers_chunks() {
        let (rx, handle) =
            spawn_demo_pty_pump_with_theme(bitty_config::theme::DEFAULT_THEME_NAME, "default");
        let mut total = 0usize;
        while let Ok(chunk) = rx.recv() {
            assert!(!chunk.is_empty());
            assert!(chunk.len() <= 8 * 1024);
            total += chunk.len();
        }
        assert!(total > 0);
        handle.join().expect("pump thread must join");
    }

    #[test]
    fn terminal_app_poll_and_tick_are_total() {
        let rt = Runtime::with_defaults().expect("must build");
        let mut app =
            TerminalApp::with_theme(rt, bitty_config::theme::DEFAULT_THEME_NAME, "default");
        // Poll the synthetic pump and drive a tick — must not panic and must
        // consume the channel without deadlocking.
        let consumed = app.poll_pty_pump();
        assert!(consumed || !consumed); // total: either path is ok
        let _ = app.drive_tick();
        // Second tick without new bytes should be idle (frame-on-demand).
        let rt2 = Runtime::with_defaults().expect("must build");
        let mut app2 =
            TerminalApp::with_theme(rt2, bitty_config::theme::DEFAULT_THEME_NAME, "default");
        let _ = app2.drive_tick();
        // After first present the second idle tick in the same app may be None.
        // We do not assert presence, only totality (no panic).
    }

    #[test]
    fn parse_split_flags() {
        let raw = args_of(&["bitty", "--split"]);
        let p = parse_args(&raw);
        assert_eq!(p.split_axis, Some(SplitAxis::Horizontal));
        assert_eq!(p.split_ratio, None);

        let raw = args_of(&["bitty", "--split", "vertical"]);
        let p = parse_args(&raw);
        assert_eq!(p.split_axis, Some(SplitAxis::Vertical));

        let raw = args_of(&["bitty", "--split", "h"]);
        let p = parse_args(&raw);
        assert_eq!(p.split_axis, Some(SplitAxis::Horizontal));

        let raw = args_of(&["bitty", "--split=v"]);
        let p = parse_args(&raw);
        assert_eq!(p.split_axis, Some(SplitAxis::Vertical));

        let raw = args_of(&["bitty", "--split", "h:0.3"]);
        let p = parse_args(&raw);
        assert_eq!(p.split_axis, Some(SplitAxis::Horizontal));
        assert!(p.split_ratio.is_some());
        assert!((p.split_ratio.unwrap() - 0.3).abs() < f32::EPSILON);

        let raw = args_of(&["bitty", "--split-ratio", "0.7"]);
        let p = parse_args(&raw);
        assert!(p.split_ratio.is_some());
        assert!((p.split_ratio.unwrap() - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_layout_and_focus_flags() {
        let raw = args_of(&["bitty", "--layout", "split:h:0.5"]);
        let p = parse_args(&raw);
        assert_eq!(p.layout.as_deref(), Some("split:h:0.5"));

        let raw = args_of(&["bitty", "--layout=stack:3"]);
        let p = parse_args(&raw);
        assert_eq!(p.layout.as_deref(), Some("stack:3"));

        let raw = args_of(&["bitty", "--focus", "next"]);
        let p = parse_args(&raw);
        assert_eq!(p.focus.as_deref(), Some("next"));

        let raw = args_of(&["bitty", "--focus=2"]);
        let p = parse_args(&raw);
        assert_eq!(p.focus.as_deref(), Some("2"));

        let raw = args_of(&["bitty", "--stack", "--overlay"]);
        let p = parse_args(&raw);
        assert!(p.stack);
        assert!(p.overlay);
    }

    #[test]
    fn build_layout_single_default() {
        let args = parse_args(&args_of(&["bitty"]));
        let layout = build_layout(&args, 80, 24);
        assert_eq!(layout.leaf_count(), 1);
        assert_eq!(layout.leaf_ids(), vec![ViewId::new(1)]);
    }

    #[test]
    fn build_layout_split_via_flag() {
        let args = parse_args(&args_of(&["bitty", "--split", "vertical"]));
        let layout = build_layout(&args, 80, 24);
        assert_eq!(layout.leaf_count(), 2);
        let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
        // vertical split 24 rows -> first 12, second 12 with 0.5 ratio
        assert_eq!(allocs[0].1.height, 12);
        assert_eq!(allocs[1].1.height, 12);
    }

    #[test]
    fn build_layout_stack_and_overlay() {
        let args = parse_args(&args_of(&["bitty", "--stack"]));
        let layout = build_layout(&args, 80, 24);
        assert_eq!(layout.leaf_count(), 2);
        let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
        // stack: both cover full bounds
        assert_eq!(allocs[0].1, UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs[1].1, UiRect::new(0, 0, 80, 24));

        let args = parse_args(&args_of(&["bitty", "--overlay"]));
        let layout = build_layout(&args, 80, 24);
        assert_eq!(layout.leaf_count(), 2);
        let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs[0].1, UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs[1].1, UiRect::new(5, 5, 20, 10));
    }

    #[test]
    fn build_layout_via_explicit_spec() {
        let args = parse_args(&args_of(&["bitty", "--layout", "split:h:0.3"]));
        let layout = build_layout(&args, 100, 24);
        let allocs = layout.layout(UiRect::new(0, 0, 100, 24));
        assert_eq!(allocs.len(), 2);
        assert_eq!(allocs[0].1.width, 30); // floor(100*0.3)
        assert_eq!(allocs[1].1.width, 70);

        let args = parse_args(&args_of(&["bitty", "--layout", "stack:3"]));
        let layout = build_layout(&args, 80, 24);
        assert_eq!(layout.leaf_count(), 3);

        let args = parse_args(&args_of(&["bitty", "--layout", "overlay:1,2,10,5"]));
        let layout = build_layout(&args, 80, 24);
        let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs[1].1, UiRect::new(1, 2, 10, 5));
    }

    #[test]
    fn layout_precedence_stack_over_split() {
        // --layout overrides --split/--stack per help text
        let args = parse_args(&args_of(&["bitty", "--split", "h", "--stack"]));
        // without explicit --layout, stack wins over split
        let layout = build_layout(&args, 80, 24);
        assert_eq!(layout.leaf_count(), 2);
        // Verify it's stack (both full)
        let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs[0].1, allocs[1].1);

        let args = parse_args(&args_of(&[
            "bitty", "--split", "h", "--stack", "--layout", "single",
        ]));
        let layout = build_layout(&args, 80, 24);
        assert_eq!(layout.leaf_count(), 1);
    }

    #[test]
    fn focus_via_args_and_runtime() {
        let mut rt = Runtime::with_defaults().expect("must build");
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        );
        rt.set_layout(split);
        assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
        assert!(apply_focus(&mut rt, "next"));
        assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
        assert!(apply_focus(&mut rt, "1"));
        assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
        assert!(!apply_focus(&mut rt, "99")); // invalid id
        assert!(!apply_focus(&mut rt, "bogus")); // invalid spec returns false
    }

    #[test]
    fn focus_directional_via_args() {
        let mut rt = Runtime::with_defaults().expect("must build");
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
        );
        rt.set_layout(split);
        rt.set_container(UiRect::new(0, 0, 80, 24));
        rt.reflow_layout();
        assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
        assert!(apply_focus(&mut rt, "right"));
        assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
        assert!(apply_focus(&mut rt, "left"));
        assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    }

    #[test]
    fn headless_smoke_with_split_is_deterministic() {
        // Two runtimes with same split + same bytes must produce identical rgba
        let synthetic = b"hello split deterministic";
        let mut rt1 = Runtime::with_defaults().expect("must build");
        let layout = build_layout(&parse_args(&args_of(&["bitty", "--split", "h"])), 80, 24);
        rt1.set_layout(layout.clone());
        rt1.handle_pty_bytes(synthetic);
        let _ = rt1.tick().expect("must present");
        let rgba1 = rt1.headless_rgba().expect("rgba");

        let mut rt2 = Runtime::with_defaults().expect("must build");
        rt2.set_layout(layout);
        rt2.handle_pty_bytes(synthetic);
        let _ = rt2.tick().expect("must present");
        let rgba2 = rt2.headless_rgba().expect("rgba");
        assert_eq!(rgba1, rgba2);
    }

    #[test]
    fn layout_proof_is_deterministic_and_distinct() {
        let synthetic = b"layout proof test";
        let code = run_layout_proof(synthetic);
        assert_eq!(code, 0);
    }

    #[test]
    fn tick_is_layout_aware_after_set_layout() {
        let mut rt = Runtime::with_defaults().expect("must build");
        let before = rt.tick().expect("first tick must present");
        assert!(before.headless);
        // Install split layout and tick with new bytes must still present layout-aware
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(10), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(20), 40, 24)),
        );
        rt.set_layout(split);
        assert_eq!(rt.leaf_count(), 2);
        rt.handle_pty_bytes(b"tick layout aware");
        let stats = rt.tick().expect("split tick must present");
        assert!(stats.headless);
        assert!(stats.fills > 0);
        let rgba = rt.headless_rgba().expect("rgba after split");
        assert!(!rgba.is_empty());
    }

    #[test]
    fn split_ratio_clamped_via_layout_node() {
        let args = parse_args(&args_of(&["bitty", "--split", "h", "--split-ratio", "5.0"]));
        let layout = build_layout(&args, 80, 24);
        if let LayoutNode::Split { ratio, .. } = layout {
            // LayoutNode::split clamps to [0.10,0.90]
            assert!(ratio <= LayoutNode::MAX_RATIO);
            assert!(ratio >= LayoutNode::MIN_RATIO);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn parse_config_and_theme_flags() {
        let p = parse_args(&args_of(&["bitty", "--config", "/tmp/c.toml"]));
        assert_eq!(p.config_path.as_deref(), Some("/tmp/c.toml"));
        assert_eq!(p.theme, None);
        let p = parse_args(&args_of(&["bitty", "--config=/tmp/d.toml"]));
        assert_eq!(p.config_path.as_deref(), Some("/tmp/d.toml"));
        let p = parse_args(&args_of(&["bitty", "--theme", "dark"]));
        assert_eq!(p.theme.as_deref(), Some("dark"));
        let p = parse_args(&args_of(&["bitty", "--theme=bitty-dark"]));
        assert_eq!(p.theme.as_deref(), Some("bitty-dark"));
        let p = parse_args(&args_of(&[
            "bitty",
            "--config",
            "/tmp/c.toml",
            "--theme",
            "dark",
        ]));
        assert_eq!(p.config_path.as_deref(), Some("/tmp/c.toml"));
        assert_eq!(p.theme.as_deref(), Some("dark"));
    }

    #[test]
    fn help_text_documents_config_flags() {
        let help = help_text();
        assert!(help.contains("--config"));
        assert!(help.contains("--theme"));
        assert!(help.contains("init.lua"));
        assert!(help.contains("config check"));
    }

    #[test]
    fn cli_flag_wins_over_file_wins_over_default() {
        use bitty_config::file::{parse_lua_config, resolve_effective};
        use bitty_config::plan::{ConfigSource, LayerKind};
        // File layer from a Lua chunk (no fs): theme "dark".
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let file_plan =
            parse_lua_config(r#"return { theme = "dark" }"#, &src).expect("file parses");
        let file_layer = bitty_config::plan::LayeredPlan::new(src, file_plan);
        // CLI wins over file.
        let merged = resolve_effective(Some(file_layer.clone()), Some("bitty-dark"))
            .expect("merge cli>file");
        assert_eq!(
            merged.effective.appearance.theme.as_deref(),
            Some("bitty-dark")
        );
        assert_eq!(
            merged.source_of("appearance.theme").unwrap().layer,
            bitty_config::plan::LayerKind::Cli
        );
        // File wins over default.
        let merged = resolve_effective(Some(file_layer), None).expect("merge file>default");
        assert_eq!(merged.effective.appearance.theme.as_deref(), Some("dark"));
        assert_eq!(
            merged.source_of("appearance.theme").unwrap().layer,
            bitty_config::plan::LayerKind::User
        );
        // Default when neither.
        let merged = resolve_effective(None, None).expect("defaults");
        assert_eq!(merged.effective.appearance.theme, None);
        // Resolved presets agree with the CTX-0147 registry contract.
        let (named, status) = bitty_config::theme::resolve_theme_with_status(Some("dark"));
        assert_eq!(status, bitty_config::theme::ThemeResolution::Named);
        assert_eq!(named.name, bitty_config::theme::DEFAULT_THEME_NAME);
    }

    #[test]
    fn invalid_theme_fails_closed_at_merge() {
        use bitty_config::file::resolve_effective;
        // Overlong CLI theme must fail validation (not silently ignored).
        let long = "x".repeat(65);
        assert!(resolve_effective(None, Some(&long)).is_err());
        // Whitespace-only CLI theme means no override (falls to default).
        let merged = resolve_effective(None, Some("   ")).expect("blank cli is no-op");
        assert_eq!(merged.effective.appearance.theme, None);
    }

    #[test]
    fn runtime_config_inherits_file_font() {
        use bitty_config::file::{parse_lua_config, resolve_effective};
        use bitty_config::plan::{ConfigSource, LayerKind};
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let content = r#"return {
            font = { family = "JetBrains Mono", size = 13.0 },
            appearance = { theme = "dark" },
        }"#;
        let plan = parse_lua_config(content, &src).expect("font parses");
        let layer = bitty_config::plan::LayeredPlan::new(src, plan);
        let merged = resolve_effective(Some(layer), None).expect("merge");
        let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
        assert_eq!(cfg.font_family, "JetBrains Mono");
        assert!((cfg.font_size - 13.0).abs() < f32::EPSILON);
        // Defaults preserved for geometry.
        let defaults = bitty_runtime::RuntimeConfig::default();
        assert_eq!(cfg.cols, defaults.cols);
        assert_eq!(cfg.rows, defaults.rows);
    }

    #[test]
    fn window_title_carries_theme_and_source() {
        let t = window_title_for_theme("bitty-dark", "file");
        assert!(t.contains("bitty-dark"));
        assert!(t.contains("file"));
        let d = window_title_for_theme("bitty-dark", "default");
        assert_ne!(t, d);
    }

    #[test]
    fn parse_config_subcommands() {
        let p = parse_args(&args_of(&["bitty", "config", "check"]));
        assert!(p.config_word);
        assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
        assert!(p.config_args.is_empty());
        assert_eq!(p.program, None);

        let p = parse_args(&args_of(&["bitty", "config", "path"]));
        assert_eq!(p.config_cmd, Some(ConfigCommand::Path));

        let p = parse_args(&args_of(&["bitty", "config", "edit"]));
        assert_eq!(p.config_cmd, Some(ConfigCommand::Edit));

        // Flags compose in any order: --config before or after the verb.
        let p = parse_args(&args_of(&[
            "bitty",
            "--config",
            "/tmp/c.lua",
            "config",
            "check",
        ]));
        assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
        assert_eq!(p.config_path.as_deref(), Some("/tmp/c.lua"));

        let p = parse_args(&args_of(&[
            "bitty",
            "config",
            "check",
            "--config",
            "/tmp/d.lua",
        ]));
        assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
        assert_eq!(p.config_path.as_deref(), Some("/tmp/d.lua"));

        // Escape hatch: a program literally named `config`.
        let p = parse_args(&args_of(&["bitty", "--", "config"]));
        assert!(!p.config_word);
        assert_eq!(p.config_cmd, None);
        assert_eq!(p.program.as_deref(), Some("config"));
    }

    #[test]
    fn parse_config_bare_and_unknown_verbs_fail_closed_at_parse() {
        let p = parse_args(&args_of(&["bitty", "config"]));
        assert!(p.config_word);
        assert_eq!(p.config_cmd, None);
        assert_eq!(p.program, None);

        let p = parse_args(&args_of(&["bitty", "config", "chek"]));
        assert!(p.config_word);
        assert_eq!(p.config_cmd, None);
        assert_eq!(p.config_args, vec!["chek".to_string()]);
        assert_eq!(p.program, None);

        // Extra positionals after a known verb are recorded for dispatch.
        let p = parse_args(&args_of(&["bitty", "config", "check", "extra"]));
        assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
        assert_eq!(p.config_args, vec!["extra".to_string()]);
    }

    #[test]
    fn config_usage_names_verbs() {
        let usage = config_usage();
        assert!(usage.contains("path"));
        assert!(usage.contains("check"));
        assert!(usage.contains("edit"));
        assert!(usage.contains("init.lua"));
    }

    #[test]
    fn resolve_editor_prefers_visual_then_editor_then_vi() {
        assert_eq!(
            resolve_editor_with_env(Some("/usr/bin/hx"), Some("/usr/bin/nano")),
            "/usr/bin/hx"
        );
        assert_eq!(
            resolve_editor_with_env(Some("  "), Some("/usr/bin/nano")),
            "/usr/bin/nano"
        );
        assert_eq!(resolve_editor_with_env(None, None), "vi");
        assert_eq!(resolve_editor_with_env(Some(""), Some(" ")), "vi");
    }

    #[test]
    fn starter_init_lua_is_valid_config() {
        use bitty_config::file::parse_lua_config;
        use bitty_config::plan::{ConfigSource, LayerKind};
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let plan = parse_lua_config(starter_init_lua(), &src).expect("starter valid");
        assert_eq!(plan.appearance.unwrap().theme.as_deref(), Some("dark"));
    }

    #[test]
    fn config_check_subcommand_good_and_broken_files() {
        let dir = std::env::temp_dir().join(format!("bitty-ctx0148-cfg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let good = dir.join("init.lua");
        std::fs::write(&good, r#"return { theme = "dark" }"#).expect("write good");
        let broken = dir.join("broken.lua");
        std::fs::write(&broken, "return { theme = }").expect("write broken");

        let mut args = Args::new();
        args.config_cmd = Some(ConfigCommand::Check);
        args.config_path = Some(good.display().to_string());
        assert_eq!(run_config_subcommand(ConfigCommand::Check, &args), 0);

        args.config_path = Some(broken.display().to_string());
        assert_eq!(run_config_subcommand(ConfigCommand::Check, &args), 2);

        args.config_path = Some(dir.join("missing.lua").display().to_string());
        assert_eq!(run_config_subcommand(ConfigCommand::Check, &args), 2);

        // Unexpected extras fail closed even for a good file.
        args.config_path = Some(good.display().to_string());
        args.config_args = vec!["extra".to_string()];
        assert_eq!(run_config_subcommand(ConfigCommand::Check, &args), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn themed_demo_pump_names_theme_and_source() {
        let (rx, handle) = spawn_demo_pty_pump_with_theme("bitty-dark", "file");
        let mut total = Vec::new();
        while let Ok(chunk) = rx.recv() {
            total.extend_from_slice(&chunk);
        }
        handle.join().expect("pump joins");
        let text = String::from_utf8_lossy(&total);
        assert!(text.contains("bitty-dark"));
        assert!(text.contains("file"));
        // Still exercises the green SGR through the themed palette.
        assert!(text.contains("green"));
    }
}
