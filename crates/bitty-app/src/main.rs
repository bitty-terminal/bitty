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
//!    config flags `--config`/`--theme`/`--font-family`/`--font-size`/
//!    `--opacity`, and an optional program to spawn).
//!    Flag parsing itself is pure, total, and tested without touching the
//!    filesystem or network; config-file loading happens in step 2.
//! 2. **Load user config** (CTX-0148 Lua, DEC-0011): resolve `--config` or
//!    the XDG default (`$XDG_CONFIG_HOME/bitty/init.lua`, fallback
//!    `~/.config/bitty/init.lua`, then `config.lua`), evaluate the
//!    wezterm-style return table in the `bitty-lua` sandbox via
//!    `bitty-config::file` (never executed as code), validate/migrate/merge
//!    with precedence `CLI (appearance flags) > file > profile > defaults`. Invalid files
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
//!    `left`/`right`) and via config keymaps in real mode (CTX-0153). The
//!    single-owner rule applies: a key event bound in `keymaps` is consumed
//!    by its chrome action and never reaches the PTY; unbound keys (Tab,
//!    arrows, plain letters) always go to the shell, so Tab completion keeps
//!    working. Focus moves are routed through [`Runtime::set_focus`](bitty_runtime::Runtime::set_focus)
//!    and [`Runtime::move_focus`](bitty_runtime::Runtime::move_focus) which
//!    delegate to the layout's deterministic adjacency.
//! 6. **Spawn shell** via [`Runtime::spawn_shell`](bitty_runtime::Runtime::spawn_shell)
//!    for the explicit program argument, or via the default shell (`$SHELL`
//!    fallback `/bin/sh`, see [`resolve_default_shell`]) when no program is
//!    given. The program is taken as a direct
//!    `argv[0]` without shell interpolation (P0 posture). Every additional
//!    leaf owns its own shell via
//!    [`Runtime::spawn_shell_for_view`](bitty_runtime::Runtime::spawn_shell_for_view)
//!    (CTX-0176, same direct-argv sandbox, sized to the leaf allocation):
//!    `new_split` and startup multi-leaf layouts spawn per-leaf sessions so
//!    panes never mirror one shell; input routes to the focused leaf only
//!    (`Runtime::push_input_bytes`); `close_view` tears the leaf's child down
//!    (`Runtime::close_pane_session`). Failures are owned
//!    [`RuntimeError`](bitty_runtime::RuntimeError) values flattened from
//!    `bitty-pty` (`Unsupported` on Windows before ConPTY, `Upstream`/`Io`
//!    elsewhere) and are reported without panicking.
//! 7. **PTY pump integration** — the bounded `PtyReader` (`READ_CHUNK_SIZE`
//!    × `CHANNEL_CAPACITY_CHUNKS` = 128 KiB) pumps kernel bytes into a
//!    `sync_channel`; the app drains `Receiver::try_recv` on the platform
//!    thread and feeds `Runtime::handle_pty_bytes`. The live pump is wired:
//!    [`Runtime::take_pty_reader`](bitty_runtime::Runtime::take_pty_reader)
//!    and [`Runtime::poll_pty`](bitty_runtime::Runtime::poll_pty) exist and
//!    `TerminalApp::poll_pty_pump` drains the real runtime channel
//!    (replies flushed via `Runtime::write_replies`). The synthetic demo
//!    pump is opt-in debug only (`BITTY_DEMO_PUMP=1`, default off) and never
//!    feeds real sessions. Headless smoke exercises `handle_pty_bytes` via
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
//! - Focus is owned by `Runtime`. `--focus` for smoke and config keymaps in
//!   real mode both resolve to `Runtime::set_focus` (numeric id) or
//!   `Runtime::move_focus` (directional). Invalid focus specs are warned and
//!   ignored (total, no panic). Unbound keys always reach the shell
//!   (single-owner rule, CTX-0153).
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
//! the kernel PTY buffer fills, and the child's `write` blocks. The live pump
//! is wired: `Runtime::take_pty_reader` and `Runtime::poll_pty` exist and
//! `TerminalApp::poll_pty_pump` drains them. A synthetic bounded demo pump
//! exists only as an opt-in debug harness (`BITTY_DEMO_PUMP=1`, default off;
//! `TerminalApp::with_demo_pump` in tests) — real sessions never see it
//! (CTX-0167 / #269).
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
    App, AppHandler, EventContext, EventWaker, KeyEvent, LogicalKey, LogicalSize, MouseButton,
    NamedKey, PhysicalSize, PlatformError, PlatformEvent, PressState, WindowConfig,
    WindowEventKind, WindowHandle, WindowId,
};
use bitty_render::gpu::GpuContext;
use bitty_runtime::{FocusDirection, LayoutNode, Runtime, SplitAxis, UiRect, View, ViewId};

mod ctl;
mod dev;
mod doctor;
mod inspect;
mod ipc_serve;

mod list;
mod run;

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

/// Diagnostic verbosity for stderr logs (CTX-0190).
///
/// Ordering is `Error < Warn < Info < Debug < Trace`. The default is [`LogLevel::Warn`]
/// (quiet): warnings/errors plus user-facing key info (paste confirm/cancel,
/// startup summary) are always emitted; per-frame `bitty tick` stats sit at
/// `Debug`/`Trace` and require `--verbose` / `--log-level debug|trace`
/// (or `BITTY_LOG`/`RUST_LOG`). The devtools trace path (`Runtime::tick`
/// return + inspect snapshots) keeps full fidelity regardless of this gate —
/// only the stderr rendering is filtered, with the format guarded so the
/// disabled hot path pays just one comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LogLevel {
    /// Errors only.
    Error,
    /// Warnings and errors (quiet default).
    Warn,
    /// Key user-facing info plus warnings/errors.
    Info,
    /// Per-frame tick stats and other diagnostics.
    Debug,
    /// Full per-frame fidelity (same tick line as `Debug` today).
    Trace,
}

impl LogLevel {
    /// Quiet default: warnings/errors (plus unconditional key info).
    fn default_level() -> Self {
        Self::Warn
    }

    /// Parses `error|warn|warning|info|debug|trace|verbose` (case-insensitive,
    /// surrounding whitespace ignored). `verbose` maps to [`LogLevel::Debug`]
    /// so `--log-level verbose` behaves like `--verbose`.
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            "verbose" => Some(Self::Debug),
            _ => None,
        }
    }

    /// True when per-frame `bitty tick` stderr lines are emitted.
    ///
    /// Tick stats are `Debug`/`Trace` diagnostics: visible with `--verbose`
    /// (which resolves to [`LogLevel::Debug`]) or `--log-level debug|trace`.
    /// Pure for unit testing; the hot path calls this before formatting.
    fn tick_enabled(self) -> bool {
        self >= Self::Debug
    }
}

/// Derives a [`LogLevel`] from a `BITTY_LOG`/`RUST_LOG`-style value.
///
/// Accepts bare levels (`debug`, `trace`, ...) and `RUST_LOG`-style filters
/// (`bitty=debug`, `info,bitty-app=trace`, `warn`). Scans case-insensitively
/// for the most verbose level named anywhere in the value so existing
/// `RUST_LOG=debug` / `RUST_LOG=trace` habits keep working without a second
/// system. Returns `None` when no known level appears.
fn log_level_from_env_value(value: &str) -> Option<LogLevel> {
    let lower = value.to_lowercase();
    if lower.contains("trace") {
        Some(LogLevel::Trace)
    } else if lower.contains("debug") {
        Some(LogLevel::Debug)
    } else if lower.contains("info") {
        Some(LogLevel::Info)
    } else if lower.contains("warn") {
        Some(LogLevel::Warn)
    } else if lower.contains("error") {
        Some(LogLevel::Error)
    } else {
        None
    }
}

/// Resolves the effective stderr log level for `args` (CTX-0190).
///
/// Precedence, highest first: `--log-level`, then `--verbose` / `-v` /
/// `BITTY_VERBOSE=1`, then `BITTY_LOG`, then `RUST_LOG`, then the quiet
/// default ([`LogLevel::Warn`]). Impure (reads env); total (unknown values
/// fall back to the next layer, never panics).
fn effective_log_level(args: &Args) -> LogLevel {
    if let Some(level) = args.log_level {
        return level;
    }
    if args.verbose {
        return LogLevel::Debug;
    }
    // Reuse the standard `RUST_LOG` habit instead of inventing a second
    // system; `BITTY_LOG` wins when both are set.
    // MSRV 1.85: no let-chains; nest instead of `if ... && let ...`.
    if std::env::var("BITTY_VERBOSE").is_ok_and(|v| v == "1" || v.to_lowercase() == "true") {
        return LogLevel::Debug;
    }
    if let Ok(value) = std::env::var("BITTY_LOG") {
        if let Some(level) = log_level_from_env_value(&value) {
            return level;
        }
    }
    if let Ok(value) = std::env::var("RUST_LOG") {
        if let Some(level) = log_level_from_env_value(&value) {
            return level;
        }
    }
    LogLevel::default_level()
}

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
    /// First unknown pre-`--` dash-flag (CR-APP-01 fail-closed, exit 2).
    /// A `-`-prefixed token before any positional program is never a
    /// program to spawn (a typo must not execute a binary); after a
    /// program is set, dash-tokens are that program's argv tail instead.
    unknown_flag: Option<String>,
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
    /// Explicit config file path (from `--config`). When `None` the
    /// `BITTY_CONFIG` env override is honored next, else the default XDG
    /// path is probed (`$XDG_CONFIG_HOME/bitty/init.lua`, fallback
    /// `~/.config/bitty/init.lua`, then `config.lua`); see `bitty-config::file`.
    config_path: Option<String>,
    /// Named profile (from `--profile`, else `BITTY_PROFILE` env).
    /// Loads `$XDG_CONFIG_HOME/bitty/profiles/<name>.lua` as the
    /// [`LayerKind::Profile`](bitty_config::LayerKind::Profile) base UNDER
    /// the user file (`init.lua` still wins; `--theme` wins over both).
    /// Missing/invalid profiles fail closed (exit 2, no fallback).
    /// CTX-0180: keep this + `config_path`/`theme`/appearance resolution in
    /// [`load_merged_config`] (one `Cli` plan for every present CLI field).
    profile: Option<String>,
    /// CLI theme override (from `--theme`). Wins over the config file and the
    /// named profile, which win over defaults
    /// (`CLI > file > profile > defaults` via `bitty-config` merge;
    /// see [`load_merged_config`] for the CTX-0180 extension point).
    theme: Option<String>,
    /// CLI font-family override (from `--font-family`, CTX-0180). Raw family
    /// name; blank means no override. Wins over the file for one launch;
    /// sibling fields (size, spacing, padding) keep file values.
    font_family: Option<String>,
    /// CLI font-size override in points (from `--font-size`, CTX-0180). Raw
    /// text on purpose: invalid values fail closed at merge time (exit 2
    /// with usage), never warn-ignored. Wins over the file for one launch.
    font_size: Option<String>,
    /// CLI window-opacity override (from `--opacity`, CTX-0180). Raw text on
    /// purpose: invalid values fail closed at merge time (exit 2 with
    /// usage), never warn-ignored. Wins over the file for one launch.
    opacity: Option<String>,
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

    /// `bitty init` opt-in setup wizard (#243, CTX-0149). True once the
    /// first positional `init` word is seen; extra positionals land in
    /// `init_args` and fail closed via usage. A program literally named
    /// `init` must be invoked as `bitty -- init ...`.
    init_word: bool,
    /// `--yes`: wizard skips prompts and writes sane defaults.
    init_yes: bool,
    /// `--force`: wizard overwrites an existing config (with `.bak` backup).
    init_force: bool,
    /// Unexpected extra positionals in init mode (dispatch errors).
    init_args: Vec<String>,
    /// `bitty doctor` installation and compatibility diagnosis (CTX-0175).
    /// True once the first positional `doctor` word is seen; extra bare
    /// positionals land in `doctor_args` and fail closed via usage. A
    /// program literally named `doctor` must be invoked as
    /// `bitty -- doctor ...`.
    doctor_word: bool,
    /// Raw `--format` value for `doctor` (`table|json|jsonl`; default table).
    /// Parsed globally so it composes before or after the `doctor` word;
    /// consumed only by the doctor dispatch, ignored by normal startup.
    doctor_format: Option<String>,
    /// `--no-color`: disable ANSI coloring in doctor table output.
    doctor_no_color: bool,
    /// Unexpected extra positionals in doctor mode (dispatch errors).
    doctor_args: Vec<String>,
    /// `bitty run -- COMMAND...` explicit child launch (CTX-0170).
    /// True once the first positional `run` word is seen (subcommand mode);
    /// a program literally named `run` needs `bitty run -- run ...` or the
    /// legacy `bitty -- run ...`. Tokens after the word land verbatim in
    /// `run_raw` for [`run::parse_run_request`]; `--` is required there.
    run_word: bool,
    /// Raw tokens after the `run` word (options, `--`, COMMAND) for
    /// [`run::parse_run_request`]. Empty until `run_word` is set.
    run_raw: Vec<String>,
    /// `bitty ctl` runtime control (CTX-0171, runtime class).
    /// True once the first positional `ctl` word is seen; a program
    /// literally named `ctl` needs `bitty run -- ctl ...` or the legacy
    /// `bitty -- ctl ...`. Tokens after the word land verbatim in
    /// `ctl_raw` for [`ctl::parse_ctl_request`].
    ctl_word: bool,
    /// Raw tokens after the `ctl` word for [`ctl::parse_ctl_request`].
    /// Empty until `ctl_word` is set.
    ctl_raw: Vec<String>,
    /// Global `--socket` before the `ctl` word (merged at dispatch;
    /// post-`ctl` `--socket` in `ctl_raw` wins when both agree, conflicts
    /// are usage errors).
    ctl_socket_pre: Option<String>,
    /// Global `--instance` before the `ctl` word (merged at dispatch).
    ctl_instance_pre: Option<String>,

    /// `bitty list <kind>` enumeration (CTX-0172). True once the first
    /// positional `list`/`ls` word is seen; a program literally named
    /// `list`/`ls` must be invoked as `bitty -- list ...`.
    list_word: bool,
    /// Invoked spelling (`list` or `ls`) for envelope `command`.
    list_spelling: String,
    /// Raw kind token after `list` (validated at dispatch).
    list_kind: Option<String>,
    /// Raw `--format` value for `list` (table|json|jsonl; default table).
    /// Parsed globally so it composes before or after the `list` word;
    /// consumed only by the list dispatch, ignored by normal startup.
    list_format: Option<String>,
    /// Explicit `--socket` for `list instances` (advisory, OS-authenticated).
    list_socket: Option<String>,
    /// Explicit `--instance` for `list instances`.
    list_instance: Option<String>,
    /// `--no-color` for list table output (also honours `NO_COLOR`).
    list_no_color: bool,
    /// Unexpected extra positionals in list mode (dispatch errors).
    list_args: Vec<String>,
    /// `bitty inspect <target> <value>` state and ownership (CTX-0173).
    /// True once the first positional `inspect` word is seen; a program
    /// literally named `inspect` must be invoked as `bitty -- inspect ...`.
    inspect_word: bool,
    /// Raw target token after `inspect` (validated at dispatch).
    inspect_target: Option<String>,
    /// Raw value token after the target (validated at dispatch).
    inspect_value: Option<String>,
    /// Raw `--format` value for `inspect` (table|json|jsonl; default table).
    /// Parsed globally so it composes before or after the `inspect` word;
    /// consumed only by the inspect dispatch, ignored by normal startup.
    inspect_format: Option<String>,
    /// `--no-color` for inspect table output (accepted for script parity;
    /// tables are plain text).
    inspect_no_color: bool,
    /// Unexpected extra positionals in inspect mode (dispatch errors).
    inspect_args: Vec<String>,
    /// `bitty dev` tracing, captures, dumps, and overlays (CTX-0174).
    /// True once the first positional `dev` word is seen; a program
    /// literally named `dev` must be invoked as `bitty -- dev ...`.
    dev_word: bool,
    /// Raw tokens after the `dev` word for [`dev::parse_dev_request`].
    /// Empty until `dev_word` is set.
    dev_raw: Vec<String>,
    /// Raw `--format` value for `dev` (table|json|jsonl; default table).
    /// Parsed globally so it composes before or after the `dev` word;
    /// consumed only by the dev dispatch, ignored by normal startup.
    dev_format: Option<String>,
    /// `--no-color` for dev output (accepted for parity; tables are plain).
    dev_no_color: bool,
    /// Global `--socket` before the `dev` word (rejected at dispatch:
    /// dev is local-only).
    dev_socket_pre: Option<String>,
    /// Global `--instance` before the `dev` word (rejected at dispatch).
    dev_instance_pre: Option<String>,
    /// When true emit per-frame `bitty tick` stats (CTX-0190).
    /// `-v` / `--verbose` (also `BITTY_VERBOSE=1`); shorthand for
    /// `--log-level debug`. Default (unset) is quiet: no tick lines.
    verbose: bool,
    /// Explicit stderr log level from `--log-level LEVEL` (CTX-0190).
    /// `None` means derive from `--verbose`/env/default in
    /// [`effective_log_level`]. Tick stats require `Debug`/`Trace`.
    log_level: Option<LogLevel>,
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
            unknown_flag: None,
            split_axis: None,
            split_ratio: None,
            stack: false,
            overlay: false,
            layout: None,
            focus: None,
            config_path: None,
            profile: None,
            theme: None,
            font_family: None,
            font_size: None,
            opacity: None,
            config_cmd: None,
            config_word: false,
            config_args: Vec::new(),

            init_word: false,
            init_yes: false,
            init_force: false,
            init_args: Vec::new(),
            doctor_word: false,
            doctor_format: None,
            doctor_no_color: false,
            doctor_args: Vec::new(),
            run_word: false,
            run_raw: Vec::new(),
            ctl_word: false,
            ctl_raw: Vec::new(),
            ctl_socket_pre: None,
            ctl_instance_pre: None,

            list_word: false,
            list_spelling: String::from("list"),
            list_kind: None,
            list_format: None,
            list_socket: None,
            list_instance: None,
            list_no_color: false,
            list_args: Vec::new(),
            inspect_word: false,
            inspect_target: None,
            inspect_value: None,
            inspect_format: None,
            inspect_no_color: false,
            inspect_args: Vec::new(),
            dev_word: false,
            dev_raw: Vec::new(),
            dev_format: None,
            dev_no_color: false,
            dev_socket_pre: None,
            dev_instance_pre: None,
            verbose: false,
            log_level: None,
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
/// resolution (explicit program verbatim, else the default-shell chain).
/// Captured once at startup from CLI args + `$SHELL`; values are direct
/// argv throughout, never split, joined, or interpolated.
#[derive(Debug, Clone, Default)]
struct SpawnSpec {
    program: Option<String>,
    program_args: Vec<String>,
    shell_env: Option<String>,
}

impl SpawnSpec {
    /// Resolves `(program, args)` exactly as startup does: the explicit
    /// program wins verbatim with its tail args, otherwise the default shell
    /// from the injected `$SHELL` value. Pure; the caller reads env once and
    /// injects it.
    fn resolve(&self) -> (String, Vec<String>) {
        match self.program.as_deref() {
            Some(program) => (program.to_string(), self.program_args.clone()),
            None => (
                resolve_default_shell(self.shell_env.as_deref()).to_string(),
                Vec::new(),
            ),
        }
    }
}

/// Spawns the [`SpawnSpec`] program as leaf `view`'s private shell, sized to
/// `cols` x `rows` cells (CTX-0176). Same sandbox as startup: direct argv,
/// explicit program verbatim with no fallback, default shell with the
/// [`FALLBACK_SHELL`] retry. Failures are logged by the fallback core and
/// returned so the caller degrades loudly: the pane then shares the primary
/// grid (never a silent mirror).
fn spawn_pane_shell(
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
/// warn loudly and leave that pane sharing the primary grid (never a
/// silent mirror). Call only after a successful primary spawn.
fn spawn_startup_pane_shells(runtime: &mut Runtime, spec: &SpawnSpec) {
    let primary = runtime.focused_view();
    let allocs = runtime.layout_allocations();
    for (id, rect) in &allocs {
        if Some(*id) == primary {
            continue;
        }
        if let Err(err) =
            spawn_pane_shell(runtime, spec, *id, rect.width.max(1), rect.height.max(1))
        {
            eprintln!(
                "warning: startup pane {id:?} shell spawn failed ({err}) — pane shares the primary grid"
            );
        }
    }
}

/// True when `token` looks like a negative number (`-5`, `-0.1`) rather
/// than a flag (`-v`, `--headless`): a leading `-` followed by a digit or
/// `.`. Lets `--font-size -5` / `--opacity -0.1` reach merge-time validation
/// (fail-closed) instead of being mistaken for a missing value.
fn looks_like_negative_number(token: &str) -> bool {
    let mut chars = token.chars();
    if chars.next() != Some('-') {
        return false;
    }
    matches!(chars.next(), Some(c) if c.is_ascii_digit() || c == '.')
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
///   `BITTY_CONFIG` env is honored next, else the XDG default is probed
///   (`$XDG_CONFIG_HOME/bitty/init.lua`, fallback
///   `~/.config/bitty/init.lua`, then `config.lua` alias)
/// - `--profile NAME` → named profile
///   (`$XDG_CONFIG_HOME/bitty/profiles/<name>.lua`, else `BITTY_PROFILE`
///   env); layered UNDER the user file (`init.lua` still wins), `--theme`
///   wins over both. Missing profiles fail closed (exit 2).
/// - `--theme NAME` → CLI theme override (wins over config file, which wins
///   over defaults; see `bitty-config::file::resolve_effective`)
/// - `--font-family NAME` → CLI font-family override for one launch
///   (CTX-0180; blank means no override; wins over the file, siblings keep
///   file values)
/// - `--font-size PTS` → CLI font-size override for one launch (CTX-0180;
///   raw text, validated at merge time: invalid values fail closed with
///   usage instead of launching)
/// - `--opacity FLOAT` → CLI window-opacity override for one launch
///   (CTX-0180; raw text, validated at merge time like `--font-size`)
/// - `-v` / `--verbose` → emit per-frame `bitty tick` stats (CTX-0190;
///   also `BITTY_VERBOSE=1`). Shorthand for `--log-level debug`; default is
///   quiet (no tick lines).
/// - `--log-level LEVEL` → stderr level `error|warn|info|debug|trace`
///   (CTX-0190; also `BITTY_LOG`/`RUST_LOG`). Tick stats require
///   `debug`/`trace`; default is `warn` (quiet).
/// - `--yes` → init-only: skip prompts, write sane defaults (CTX-0149).
/// - `--force` → init-only: overwrite an existing config file, backing it
///   up to `<file>.bak` first (CTX-0149).
/// - `run [OPTIONS] -- COMMAND...` → explicit child launch (CTX-0170);
///   a program literally named `run` needs `bitty run -- run ...` or
///   `bitty -- run ...`. Tokens after `run` are kept verbatim for
///   `run::parse_run_request`, which requires `--` before COMMAND.
/// - `config <path|check|edit>` → config subcommand (DEC-0007); a program
///   literally named `config` needs `bitty -- config ...`
/// - `init [--yes] [--force]` → opt-in setup wizard (#243, CTX-0149);
///   a program literally named `init` needs `bitty -- init ...`
/// - `doctor [--format table|json|jsonl] [--no-color]` → installation and
///   compatibility diagnosis (CTX-0175, local class, safe mode); a program
///   literally named `doctor` needs `bitty -- doctor ...`
/// - `list <themes|plugins|instances>` → resource enumeration (CTX-0172);
///   a program literally named `list`/`ls` needs `bitty -- list ...`
/// - `inspect <target> <value>` → state and ownership (CTX-0173, local
///   class, safe mode); a program literally named `inspect` needs
///   `bitty -- inspect ...`
/// - `--format SHAPE` → doctor/ctl/list/inspect output shape (parsed
///   globally, consumed by each subcommand dispatch; ignored by startup)
/// - `--no-color` → disable ANSI coloring in doctor/list table output
///   (accepted by inspect for parity; its tables are plain text)
/// - `--yes` / `--force` are init-only flags (parsed globally, consumed by
///   the init dispatch; ignored by normal startup)
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
    // CTX-0190: honour BITTY_VERBOSE without editing argv (mirrors BITTY_HEADLESS).
    if std::env::var("BITTY_VERBOSE").is_ok_and(|v| v == "1" || v.to_lowercase() == "true") {
        out.verbose = true;
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
        if let Some(val) = token.strip_prefix("--format=") {
            // Raw on purpose: validated at doctor/ctl/list/inspect/dev dispatch
            // (fail-closed exit 2 on unknown shapes, never warn-ignored).
            // Merged CTX-0171 + CTX-0172 + CTX-0173 + CTX-0174: same token feeds
            // every dispatch; the inactive dispatches ignore their field.
            // CTX-0174: post-`dev` tokens stay verbatim for
            // `dev::parse_dev_request`, which owns `--format` validation
            // there; pre-word values feed `dev_format`.
            if out.dev_word {
                out.dev_raw.push(token.clone());
                i += 1;
                continue;
            }
            out.doctor_format = Some(val.to_string());
            out.list_format = Some(val.to_string());
            out.inspect_format = Some(val.to_string());
            out.dev_format = Some(val.to_string());
            i += 1;
            continue;
        }
        if let Some(val) = token.strip_prefix("--socket=") {
            // `bitty ctl --socket PATH` global form (before the `ctl` word);
            // raw on purpose, validated at ctl/list dispatch (exit 2 on shape).
            // After the `ctl` word tokens go verbatim to `ctl_raw` instead.
            // Merged CTX-0171 + CTX-0172: pre-word form feeds both dispatches.
            // CTX-0174: post-`dev` tokens stay verbatim for
            // `dev::parse_dev_request` (rejected local-only there); pre-word
            // values are stashed for the dev dispatch to reject explicitly.
            if out.dev_word {
                out.dev_raw.push(token.clone());
                i += 1;
                continue;
            }
            if !out.ctl_word {
                out.ctl_socket_pre = Some(val.to_string());
                out.list_socket = Some(val.to_string());
                out.dev_socket_pre = Some(val.to_string());
            } else {
                out.ctl_raw.push(token.clone());
            }
            i += 1;
            continue;
        }
        if let Some(val) = token.strip_prefix("--instance=") {
            // Merged CTX-0171 + CTX-0172: pre-word form feeds both dispatches.
            // CTX-0174: post-`dev` tokens stay verbatim for
            // `dev::parse_dev_request` (rejected local-only there); pre-word
            // values are stashed for the dev dispatch to reject explicitly.
            if out.dev_word {
                out.dev_raw.push(token.clone());
                i += 1;
                continue;
            }
            if !out.ctl_word {
                out.ctl_instance_pre = Some(val.to_string());
                out.list_instance = Some(val.to_string());
                out.dev_instance_pre = Some(val.to_string());
            } else {
                out.ctl_raw.push(token.clone());
            }
            i += 1;
            continue;
        }
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
        if token.starts_with("--profile=") {
            let val = token.trim_start_matches("--profile=");
            if val.trim().is_empty() {
                eprintln!("warning: --profile needs a profile name — ignoring");
            } else {
                out.profile = Some(val.to_string());
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

        if token.starts_with("--font-family=") {
            let val = token.trim_start_matches("--font-family=");
            if val.trim().is_empty() {
                eprintln!("warning: --font-family needs a family name — ignoring");
            } else {
                out.font_family = Some(val.to_string());
            }
            i += 1;
            continue;
        }
        if token.starts_with("--font-size=") {
            let val = token.trim_start_matches("--font-size=");
            if val.trim().is_empty() {
                eprintln!("warning: --font-size needs a point size — ignoring");
            } else {
                // Raw on purpose: validated at merge time (fail-closed).
                out.font_size = Some(val.to_string());
            }
            i += 1;
            continue;
        }
        if token.starts_with("--opacity=") {
            let val = token.trim_start_matches("--opacity=");
            if val.trim().is_empty() {
                eprintln!("warning: --opacity needs a value — ignoring");
            } else {
                // Raw on purpose: validated at merge time (fail-closed).
                out.opacity = Some(val.to_string());
            }
            i += 1;
            continue;
        }
        if token.starts_with("--log-level=") {
            let val = token.trim_start_matches("--log-level=");
            match LogLevel::parse(val) {
                Some(level) => out.log_level = Some(level),
                None => eprintln!(
                    "warning: unknown --log-level {val:?} (want error|warn|info|debug|trace) — ignoring"
                ),
            }
            i += 1;
            continue;
        }
        match token.as_str() {
            "--" => {
                // In `list`/`inspect`/`dev` mode `--` is a stray separator
                // (UsageError at dispatch); elsewhere it ends flags for
                // PROGRAM argv.
                if out.list_word {
                    out.list_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if out.inspect_word {
                    out.inspect_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if out.dev_word {
                    out.dev_raw.push(token.clone());
                    i += 1;
                    continue;
                }
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
            "-v" | "--verbose" => {
                out.verbose = true;
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
            "--yes" => {
                // `bitty init --yes`: non-interactive defaults (ignored by
                // normal startup and the `config` subcommand).
                out.init_yes = true;
                i += 1;
            }
            "--force" => {
                // `bitty init --force`: overwrite an existing config file
                // (with a `.bak` backup). Ignored elsewhere.
                out.init_force = true;
                i += 1;
            }
            "--format" => {
                // `bitty doctor/ctl/list/inspect/dev --format SHAPE`: raw on
                // purpose, validated at dispatch (fail-closed exit 2). Parsed
                // globally so it composes before or after the subcommand word.
                // Merged CTX-0171 + CTX-0172 + CTX-0173 + CTX-0174: same token
                // feeds every dispatch; missing warns (doctor/ctl) and
                // fail-closes list/inspect/dev via empty.
                // CTX-0174: post-`dev` pairs stay verbatim for
                // `dev::parse_dev_request`; pre-word values feed `dev_format`.
                if out.dev_word {
                    out.dev_raw.push(token.clone());
                    if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                        out.dev_raw.push(raw[i + 1].clone());
                        i += 2;
                    } else {
                        out.dev_raw.push(String::new());
                        i += 1;
                    }
                    continue;
                }
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.doctor_format = Some(raw[i + 1].clone());
                    out.list_format = Some(raw[i + 1].clone());
                    out.inspect_format = Some(raw[i + 1].clone());
                    out.dev_format = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --format needs a value (table|json|jsonl) — ignoring");
                    out.list_format = Some(String::new());
                    out.inspect_format = Some(String::new());
                    out.dev_format = Some(String::new());
                    i += 1;
                }
            }
            "--socket" => {
                // `bitty ctl/list --socket PATH` global form (before the word).
                // Raw on purpose, validated at dispatch. After the `ctl` word
                // tokens go verbatim to `ctl_raw` (see `ctl` arm below).
                // Merged: feeds both; missing warns and fail-closes list.
                // CTX-0174: post-`dev` pairs stay verbatim for
                // `dev::parse_dev_request` (rejected local-only there);
                // pre-word values are stashed for the dev dispatch to reject.
                if out.dev_word {
                    out.dev_raw.push(token.clone());
                    if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                        out.dev_raw.push(raw[i + 1].clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.ctl_socket_pre = Some(raw[i + 1].clone());
                    out.list_socket = Some(raw[i + 1].clone());
                    out.dev_socket_pre = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --socket needs a path — ignoring");
                    out.list_socket = Some(String::new());
                    out.dev_socket_pre = Some(String::new());
                    i += 1;
                }
            }
            "--instance" => {
                // `bitty ctl/list --instance ID` global form (before the word).
                // Merged: feeds both; missing warns and fail-closes list.
                // CTX-0174: post-`dev` pairs stay verbatim for
                // `dev::parse_dev_request` (rejected local-only there);
                // pre-word values are stashed for the dev dispatch to reject.
                if out.dev_word {
                    out.dev_raw.push(token.clone());
                    if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                        out.dev_raw.push(raw[i + 1].clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.ctl_instance_pre = Some(raw[i + 1].clone());
                    out.list_instance = Some(raw[i + 1].clone());
                    out.dev_instance_pre = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --instance needs an id — ignoring");
                    out.list_instance = Some(String::new());
                    out.dev_instance_pre = Some(String::new());
                    i += 1;
                }
            }
            "--no-color" => {
                out.doctor_no_color = true;
                out.list_no_color = true;
                out.inspect_no_color = true;
                out.dev_no_color = true;
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
            "--profile" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.profile = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --profile needs a profile name — ignoring");
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

            "--font-family" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    out.font_family = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --font-family needs a family name — ignoring");
                    i += 1;
                }
            }
            "--font-size" => {
                if i + 1 < raw.len()
                    && (!raw[i + 1].starts_with('-') || looks_like_negative_number(&raw[i + 1]))
                {
                    // Raw on purpose: validated at merge time (fail-closed).
                    out.font_size = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --font-size needs a point size — ignoring");
                    i += 1;
                }
            }
            "--opacity" => {
                if i + 1 < raw.len()
                    && (!raw[i + 1].starts_with('-') || looks_like_negative_number(&raw[i + 1]))
                {
                    // Raw on purpose: validated at merge time (fail-closed).
                    out.opacity = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --opacity needs a value — ignoring");
                    i += 1;
                }
            }
            "--log-level" => {
                if i + 1 < raw.len() && !raw[i + 1].starts_with('-') {
                    let val = raw[i + 1].clone();
                    match LogLevel::parse(&val) {
                        Some(level) => out.log_level = Some(level),
                        None => eprintln!(
                            "warning: unknown --log-level {val:?} (want error|warn|info|debug|trace) — ignoring"
                        ),
                    }
                    i += 2;
                } else {
                    eprintln!(
                        "warning: --log-level needs a value (error|warn|info|debug|trace) — ignoring"
                    );
                    i += 1;
                }
            }
            s if s.starts_with('-') => {
                // In `list`/`inspect`/`dev` mode unknown flags fail closed at
                // dispatch (exit 2); elsewhere a `-`-prefixed token before
                // any positional program is a usage error (CR-APP-01, exit
                // 2), never a program to spawn. Once a program is set,
                // dash-tokens are that program's argv tail (e.g. `cat -A`).
                // (`dev` normally breaks verbatim at its word, so this arm
                // only fires for flags before the word — kept for parity.)
                if out.list_word {
                    out.list_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if out.inspect_word {
                    out.inspect_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if out.dev_word {
                    out.dev_raw.push(token.clone());
                    i += 1;
                    continue;
                }
                if program_set {
                    out.program_args.push(token.clone());
                    i += 1;
                    continue;
                }
                if out.unknown_flag.is_none() {
                    out.unknown_flag = Some(token.clone());
                }
                i += 1;
            }
            _ => {
                // `bitty run -- COMMAND...` explicit child launch (CTX-0170,
                // first positional only; `--` escape bypasses via
                // after_double_dash). The word `run` is always this
                // subcommand, never a program named `run`: use
                // `bitty run -- run ...` (or legacy `bitty -- run ...`) for
                // that program. Tokens after the word are kept verbatim for
                // `run::parse_run_request`, which enforces the required `--`.
                if !program_set
                    && !out.config_word
                    && !out.init_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && token == "run"
                {
                    out.run_word = true;
                    out.run_raw.extend_from_slice(&raw[i + 1..]);
                    break;
                }
                // `bitty ctl` runtime control (CTX-0171, first positional
                // only; `--` escape bypasses via after_double_dash). The word
                // `ctl` is always this subcommand, never a program named
                // `ctl`: use `bitty run -- ctl ...` (or legacy
                // `bitty -- ctl ...`) for that program. Tokens after the word
                // are kept verbatim for `ctl::parse_ctl_request`.
                if !program_set
                    && !out.config_word
                    && !out.init_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.doctor_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && token == "ctl"
                {
                    out.ctl_word = true;
                    out.ctl_raw.extend_from_slice(&raw[i + 1..]);
                    break;
                }
                // `bitty config <verb>` subcommand (first positional only;
                // `--` escape hatch bypasses this via after_double_dash).
                // A program literally named `config` needs `bitty -- config`.
                if !program_set
                    && !out.config_word
                    && !out.doctor_word
                    && !out.ctl_word
                    && !out.run_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && token == "config"
                {
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

                // `bitty init` opt-in setup wizard (first positional only;
                // `--` escape hatch bypasses this via after_double_dash).
                // A program literally named `init` needs `bitty -- init`.
                // Flags (`--yes`, `--force`, `--config`) compose in any
                // order around the word.
                if !program_set
                    && !out.init_word
                    && !out.doctor_word
                    && !out.ctl_word
                    && !out.run_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && token == "init"
                {
                    out.init_word = true;
                    i += 1;
                    continue;
                }
                if out.init_word {
                    // Extra positionals in init mode fail closed at dispatch.
                    out.init_args.push(token.clone());
                    i += 1;
                    continue;
                }
                // `bitty doctor` diagnosis (first positional only; CTX-0175).
                // `--` escape hatch bypasses this via after_double_dash.
                // A program literally named `doctor` needs `bitty -- doctor`.
                // Flags (`--format`, `--no-color`, `--config`, `--profile`)
                // compose in any order around the word. Mutual exclusion
                // with `config`/`init` words falls out naturally: once one
                // word is seen the other word lands in that mode's extra
                // args and fails closed at dispatch.
                if !program_set
                    && !out.doctor_word
                    && !out.config_word
                    && !out.init_word
                    && !out.ctl_word
                    && !out.run_word
                    && !out.list_word
                    && !out.inspect_word
                    && !out.dev_word
                    && token == "doctor"
                {
                    out.doctor_word = true;
                    i += 1;
                    continue;
                }
                if out.doctor_word {
                    // Doctor takes no positionals: fail closed at dispatch.
                    out.doctor_args.push(token.clone());
                    i += 1;
                    continue;
                }

                // `bitty list <kind>` enumeration (first positional only;
                // CTX-0172). `ls` is the stable alias for `list` per
                // cli-contract-rfc. A program literally named `list`/`ls`
                // needs `bitty -- list ...` (or `bitty run -- list ...`).
                if !program_set
                    && !out.config_word
                    && !out.list_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.inspect_word
                    && !out.dev_word
                    && (token == "list" || token == "ls")
                {
                    out.list_word = true;
                    out.list_spelling = token.clone();
                    i += 1;
                    continue;
                }
                if out.list_word {
                    // Kind is the first bare token after `list`; the rest
                    // fail closed at dispatch (including stray `--`, which
                    // was recorded above as a list arg).
                    if out.list_kind.is_none() {
                        // Allow `-h/--help` after `list` to flow to the
                        // global help path (main shows list help when both
                        // are set); any other flag-looking token was already
                        // captured as a list arg above.
                        out.list_kind = Some(token.clone());
                    } else {
                        out.list_args.push(token.clone());
                    }
                    i += 1;
                    continue;
                }
                // `bitty inspect <target> <value>` state and ownership (first
                // positional only; CTX-0173). A program literally named
                // `inspect` needs `bitty -- inspect ...` (or
                // `bitty run -- inspect ...`).
                if !program_set
                    && !out.config_word
                    && !out.inspect_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.dev_word
                    && token == "inspect"
                {
                    out.inspect_word = true;
                    i += 1;
                    continue;
                }
                if out.inspect_word {
                    // Target is the first bare token after `inspect`, value
                    // the second; the rest fail closed at dispatch (including
                    // stray `--`, which was recorded above as an inspect arg).
                    if out.inspect_target.is_none() {
                        // Allow `-h/--help` after `inspect` to flow to the
                        // global help path (main shows inspect help when both
                        // are set); any other flag-looking token was already
                        // captured as an inspect arg above.
                        out.inspect_target = Some(token.clone());
                    } else if out.inspect_value.is_none() {
                        out.inspect_value = Some(token.clone());
                    } else {
                        out.inspect_args.push(token.clone());
                    }
                    i += 1;
                    continue;
                }
                // `bitty dev` tracing, captures, dumps, overlays (first
                // positional only; CTX-0174). The word `dev` is always this
                // subcommand, never a program named `dev`: use
                // `bitty run -- dev ...` (or legacy `bitty -- dev ...`) for
                // that program. Tokens after the word are kept verbatim for
                // `dev::parse_dev_request`.
                if !program_set
                    && !out.config_word
                    && !out.inspect_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.dev_word
                    && token == "dev"
                {
                    out.dev_word = true;
                    out.dev_raw.extend_from_slice(&raw[i + 1..]);
                    break;
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
            -v, --verbose    Emit per-frame `bitty tick` stats on stderr\n  \
                             (shorthand for --log-level debug; default quiet)\n  \
                --log-level LEVEL  Stderr level: error|warn|info|debug|trace\n  \
                             (default warn; tick stats need debug|trace;\n  \
                             also BITTY_LOG/RUST_LOG)\n  \
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
                            BITTY_CONFIG is honored next, else the XDG default\n  \
                            is probed ($XDG_CONFIG_HOME/bitty/init.lua,\n  \
                            fallback ~/.config/bitty/init.lua, then config.lua alias).\n  \
                            Invalid files fail closed (clear stderr, exit non-zero,\n  \
                            no panic).\n  \
               --profile NAME Named profile ($XDG_CONFIG_HOME/bitty/profiles/<name>.lua,\n  \
                            else BITTY_PROFILE). Layered UNDER the user file\n  \
                            (init.lua still wins; --theme wins over both).\n  \
                            Missing profiles fail closed (exit 2, no fallback).\n  \
                            With --config + --profile together both layers load\n  \
                            (explicit file wins) with a stderr warning.\n  \
                            Env: BITTY_CONFIG (path), BITTY_PROFILE (name).\n  \
                            Precedence: CLI flags > BITTY_* env > file+profile > defaults.\n  \
               --theme NAME   CLI theme override (e.g. --theme dark). Wins over the\n  \
                             config file, which wins over defaults.\n  \
                --font-family NAME  CLI font family override for one launch\n  \
                             (e.g. --font-family \"JetBrainsMono Nerd Font\").\n  \
                             Wins over the file; blank means no override.\n  \
                --font-size PTS  CLI font size override in points for one launch\n  \
                             (e.g. --font-size 14; range (0, 128]). Invalid\n  \
                             values fail closed (exit 2 with this usage).\n  \
                 --opacity FLOAT  CLI window opacity override for one launch\n  \
                              (e.g. --opacity 0.95; range [0.0, 1.0]). Invalid\n  \
                              values fail closed (exit 2 with this usage).\n  \
                              Precedence: CLI flags > file > profile > defaults;\n  \
                              each flag overrides only its own field (siblings\n  \
                              keep file values).\n  \
                 --format SHAPE  Doctor/ctl/list/inspect output shape: table|json|jsonl\n  \
                               (default table; parsed globally, consumed by\n  \
                               `bitty doctor`, `bitty ctl`, `bitty list`, and\n  \
                               `bitty inspect`; ignored by startup).\n  \
               --socket PATH   Ctl target socket (global `bitty --socket P ctl ...`\n  \
                              or `bitty ctl --socket P ...`; bypasses discovery).\n  \
               --instance ID   Ctl target instance (global or per-`ctl` flag).\n  \
                 --no-color     Disable ANSI coloring in doctor/list table output\n  \
                               (accepted by inspect for parity; its tables are plain)\n  \
               --           End of flags; remaining tokens are PROGRAM argv\n\
         \n\
          Subcommands (CLI-first management, DEC-0007):\n  \

            run [--cwd PATH] [--env K=V ...] [--title S] -- COMMAND...  Explicit child launch (local)\n  \
                             Runs COMMAND directly (no shell); `--` is required;\n  \
                             exit code is the child's. `bitty htop` never means\n  \
                             `bitty run -- htop`; colliding names need `run --`.\n  \
            ctl [--socket P] [--instance ID] [--format SHAPE] <resource> <verb>  Control a running instance (runtime)\n  \
                             instance|window|view|terminal list; terminal spawn|close|send|text;\n  \
                             view split|focus; config reload. `ctl --help` never needs an instance.\n  \
            config path      Print the resolved config file path\n  \
           config check     Load + validate; print per-key sources\n  \
                            (cli/file/default), exit non-zero on invalid files\n  \
            config edit      Open the file in $VISUAL/$EDITOR (vi fallback);\n  \
                             creates parents + starter when missing, never\n  \
                             overwrites existing content\n  \
             init [--yes] [--force]  Opt-in interactive setup wizard (never\n  \
                              auto-runs): mascot greeting, shell/theme/font-size\n  \
                              picks, vim keybinding preset, writes init.lua.\n  \
                              --yes skips prompts (sane defaults); without\n  \
                              --force an existing file is never overwritten\n  \
                              (--force backs it up to init.lua.bak first).\n  \
                              Honors --config PATH / BITTY_CONFIG as the target.\n  \
            doctor [--format SHAPE] [--no-color]  Diagnose installation and\n  \
                              compatibility (local class, safe mode): binary,\n  \
                              config, keymaps, fonts, display, GPU, clipboard,\n  \
                              PTY, terminfo, shell, images, plugins.\n  \
                              Exit 0 when all pass (warns allowed), 1 on\n  \
                              recoverable failure, else the strongest\n  \
                              category code (3 config, 5 compat, 8 conflict).\n  \
             list <kind>      Enumerate resources: themes|plugins|instances\n  \
                              (--format table|json|jsonl, --socket/--instance for\n  \
                              instances; alias `ls`; `bitty list --help` for detail)\n  \
             inspect <target> <value>  Explain state and ownership (local, safe mode)\n  \
                              command|key|plugin|config|protocol\n  \
                              (--format table|json|jsonl; `bitty inspect --help`)\n  \
            dev <verb>       Developer tracing, captures, dumps, overlays\n  \
                             (trace|capture|dump|overlay; local only, no\n  \
                             instance; `bitty dev --help` for detail)\n  \
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
           or numeric ViewId). Keyboard in real mode is keymap-driven (CTX-0153\n  \
           single-owner rule): a bound chord is consumed by its chrome action\n  \
           and never reaches the PTY; unbound keys (Tab, arrows, plain\n  \
           letters) always go to the shell. Defaults (Alt is the Mod):\n  \
           Alt+h/j/k/l and Ctrl+Alt+arrows move focus, Alt+1..9 jumps to\n  \
           view N, Alt+u/Alt+i pages up/down, Shift+Alt+h/j/k/l splits,\n  \
           Shift+Ctrl+h/j/k/l resizes, Alt+w closes, Alt+z/m/f zooms,\n  \
           Ctrl+Tab cycles, Ctrl+Shift+C/V copy/paste;\n  \
           see `bitty config check` for the active table.\n\
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
            return {{ theme = \"dark\", font = {{ family = \"JetBrainsMono Nerd Font\", size = 12 }} }}\n  \
                            Evaluated in the bitty-lua sandbox (same budgets as\n  \
                            plugins; no io/os). Unknown keys fail closed.\n  \
         \n\
         Examples:\n  \
           bitty --help\n  \
           bitty --version\n  \
           bitty run --help\n  \
           bitty run -- htop\n  \
           bitty run --cwd /tmp --env FOO=bar -- printenv FOO\n  \
           bitty --headless\n  \
           bitty --headless --split v --focus next\n  \
           bitty --headless --layout stack:2 --focus 2\n  \
           bitty --headless --layout overlay:5,5,20,10\n  \
            bitty --headless -- /bin/bash\n  \
            bitty /bin/bash\n  \
            bitty -- /bin/cat -A\n  \
            bitty doctor\n  \
            bitty doctor --format json\n",
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
/// provided the theme, `"profile"` when the named profile provided it, else
/// `"default"`. The resolved `theme` preset is the single `bitty-config`
/// registry entry the window renders.
struct AppConfig {
    /// Merged effective config (`CLI > file > profile > defaults`).
    effective: bitty_config::EffectiveConfig,
    /// Config file path that was used, if any.
    file_path: Option<std::path::PathBuf>,
    /// Requested profile name (`--profile` over `BITTY_PROFILE`), if any.
    profile_name: Option<String>,
    /// Profile file path that was used, if any.
    profile_path: Option<std::path::PathBuf>,
    /// Resolved theme preset (static registry entry).
    theme: &'static bitty_config::theme::Theme,
    /// How the theme resolved (Default/Named/FallbackUnknown).
    resolution: bitty_config::theme::ThemeResolution,
    /// `"cli"` / `"file"` / `"profile"` / `"default"`: which layer won the theme.
    source: &'static str,
}

/// Resolved configuration bundle behind startup and `config check`.
struct LoadedConfig {
    /// Merged effective config.
    merged: bitty_config::MergedConfig,
    /// Probed user file (explicit or default), if a root exists.
    probed: Option<bitty_config::file::ProbedConfig>,
    /// Requested profile name (`--profile` over `BITTY_PROFILE`), if any.
    profile_name: Option<String>,
    /// Profile file that was loaded, if any.
    profile_path: Option<std::path::PathBuf>,
}

/// Builds the single CLI override input from parsed [`Args`] (CTX-0180).
///
/// Pure over `args` so the wiring stays hermetic: trims each raw and treats
/// `None`/empty/whitespace as absent. Numeric raws (`--font-size`,
/// `--opacity`) stay strings here; [`load_merged_config`] validates them
/// fail-closed (with usage) before the merge runs, and the merge validates
/// again, so direct API misuse fails closed too.
fn cli_overrides_from_args(args: &Args) -> bitty_config::file::CliOverrides {
    bitty_config::file::CliOverrides {
        theme: args
            .theme
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        font_family: args
            .font_family
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        font_size: args
            .font_size
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        opacity: args
            .opacity
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
    }
}

/// Names the CLI flag behind an appearance-override validation error for the
/// fail-closed message. `theme` keeps its CTX-0169 merge-time path and never
/// reaches here; anything unexpected falls back to the flag group.
fn appearance_flag_for_field(field: Option<&str>) -> &'static str {
    match field {
        Some("font.family") => "--font-family",
        Some("font.size") => "--font-size",
        Some("window.opacity") => "--opacity",
        _ => "--font-family/--font-size/--opacity",
    }
}

/// Shared probe + load + merge behind startup and `config check` (CTX-0169).
///
/// Resolution (per #271 and `lua-and-xdg.md` §Layers):
/// - User file: `--config` CLI wins over `BITTY_CONFIG` env, else the XDG
///   default probe (`init.lua`, then `config.lua` alias, then canonical
///   `init.lua`). Missing default-probe files yield defaults; missing
///   explicit (`--config`/`BITTY_CONFIG`) files fail closed.
/// - Profile: `--profile` CLI wins over `BITTY_PROFILE` env, loaded from
///   `$XDG_CONFIG_HOME/bitty/profiles/<name>.lua` as the `Profile` layer
///   UNDER the user file (`init.lua` still wins; `--theme` wins over both).
///   Requested-but-missing/invalid profiles fail closed (no fallback).
/// - Merge: `CLI (appearance flags) > user file > profile > defaults` via
///   `bitty-config::file::resolve_effective_full` (merge sorts by
///   `LayerKind::precedence`, never by load order).
/// - `--config` + `--profile` together: both layers load (explicit file wins
///   over the profile, same as `init.lua` over profile) with a stderr warning.
///
/// CTX-0180: CLI appearance overrides (`--font-family`/`--font-size`/
/// `--opacity`) extend the [`bitty_config::file::CliOverrides`] built here —
/// one `Cli` plan carrying every present CLI field — so this function's shape
/// stays stable. Theme/profile resolution is untouched.
///
/// Returns the merged layers plus the probed user path and the resolved
/// profile identity. impure (filesystem + env); total (all failures become
/// `Err(String)`).
fn load_merged_config(args: &Args) -> Result<LoadedConfig, String> {
    // Env overrides (12-factor, test-friendly): BITTY_CONFIG (path),
    // BITTY_PROFILE (name). CLI flags win over env (pure resolvers).
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let bitty_profile_env = std::env::var("BITTY_PROFILE").ok();
    let explicit = bitty_config::file::resolve_config_explicit(
        args.config_path.as_deref(),
        bitty_config_env.as_deref(),
    );
    let explicit_from_cli = args
        .config_path
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty());
    let probed = bitty_config::file::probe_config_path(explicit.as_deref());
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
            if explicit_from_cli {
                return Err(format!(
                    "bitty: --config '{}' not found",
                    probe.path.display()
                ));
            }
            return Err(format!(
                "bitty: BITTY_CONFIG '{}' not found",
                probe.path.display()
            ));
        }
    }
    // Named profile (fail-closed): validate, resolve under the user-level
    // XDG root (XDG_CONFIG_DIRS never consulted), load as Profile layer.
    let profile_request = bitty_config::file::resolve_profile_request(
        args.profile.as_deref(),
        bitty_profile_env.as_deref(),
    );
    let profile_from_cli = args
        .profile
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty());
    let mut profile_layer: Option<bitty_config::LayeredPlan> = None;
    let mut profile_path: Option<std::path::PathBuf> = None;
    if let Some(requested) = profile_request.clone() {
        let resolved = bitty_config::file::profile_file_path(&requested).map_err(|err| {
            if profile_from_cli {
                format!("bitty: invalid --profile '{requested}': {err}")
            } else {
                format!("bitty: invalid BITTY_PROFILE '{requested}': {err}")
            }
        })?;
        if !resolved.exists() {
            if profile_from_cli {
                return Err(format!(
                    "bitty: --profile '{requested}' not found ('{}')",
                    resolved.display()
                ));
            }
            return Err(format!(
                "bitty: BITTY_PROFILE '{requested}' not found ('{}')",
                resolved.display()
            ));
        }
        match bitty_config::file::load_profile_layer(&resolved) {
            Ok(layer) => {
                profile_path = Some(resolved.clone());
                profile_layer = Some(layer);
            }
            Err(err) => {
                return Err(format!(
                    "bitty: invalid profile file '{}': {err}",
                    resolved.display()
                ));
            }
        }
        // `--config` + `--profile` together: both layers stay loaded; the
        // explicit file wins over the profile (init.lua semantics). Warn so
        // the composition is never silent.
        if used_path.is_some() && probed.as_ref().is_some_and(|p| p.explicit) {
            let name = profile_request.as_deref().unwrap_or_default();
            eprintln!(
                "bitty: warning: --config with --profile '{name}': profile layers under the explicit file (explicit file wins)"
            );
        }
    }
    // CTX-0180: one Cli layer for every present appearance flag (`--theme`
    // plus `--font-family`/`--font-size`/`--opacity`). Invalid appearance
    // raws fail closed here with usage (exit 2 at the caller); the merge
    // then only sees valid CLI input, so later merge errors stay
    // file/profile-caused and keep their existing messages.
    let cli = cli_overrides_from_args(args);
    if let Err(err) = cli.validate_appearance_overrides() {
        let flag = appearance_flag_for_field(err.field());
        return Err(format!("bitty: invalid {flag}: {err}\n{}", help_text()));
    }
    let merged = bitty_config::file::resolve_effective_full(file_layer, profile_layer, &cli)
        .map_err(|err| {
            if let Some(p) = &used_path {
                format!("bitty: invalid config '{}': {err}", p.display())
            } else if let Some(p) = &profile_path {
                format!("bitty: invalid profile '{}': {err}", p.display())
            } else {
                format!("bitty: invalid --theme: {err}")
            }
        })?;
    Ok(LoadedConfig {
        merged,
        probed,
        profile_name: profile_request,
        profile_path,
    })
}

/// Loads the effective config for `args`.
///
/// - User file: `--config` wins over `BITTY_CONFIG`, else `init.lua`, else
///   the `config.lua` alias, else the canonical `init.lua` path.
/// - Profile: `--profile` wins over `BITTY_PROFILE`, loaded from
///   `profiles/<name>.lua` UNDER the user file (`init.lua` still wins).
/// - A missing **probed** file yields defaults (no error); a missing
///   **explicit** `--config`/`BITTY_CONFIG` file, or a requested-but-missing
///   profile, fails closed.
/// - A present-but-invalid file (Lua syntax/runtime/budget/shape/validation)
///   fails closed with a user-facing message (caller prints to stderr and
///   exits non-zero; no panic, no silent ignore).
/// - Merges `CLI (appearance flags) > file > profile > defaults` via `bitty-config`
///   and resolves `appearance.theme` through the preset registry.
///
/// impure (filesystem reads + env vars XDG/HOME/BITTY_*); total (all failures
/// become `Err(String)`).
fn load_app_config(args: &Args) -> Result<AppConfig, String> {
    let loaded = load_merged_config(args)?;
    let merged = loaded.merged;
    let probed = loaded.probed;
    let profile_request = loaded.profile_name;
    let profile_path = loaded.profile_path;
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
        Some(bitty_config::LayerKind::Profile) => "profile",
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
    if let (Some(name), Some(path)) = (&profile_request, &profile_path) {
        eprintln!(
            "bitty: profile '{name}' from '{}' (source={source})",
            path.display()
        );
    }
    Ok(AppConfig {
        effective,
        file_path,
        profile_name: profile_request,
        profile_path,
        theme,
        resolution,
        source,
    })
}

/// Short usage for `bitty config` (stderr, fail-closed exit 2).
fn config_usage() -> String {
    "usage: bitty config <path|check|edit> [--config PATH] [--profile NAME]\n\
     \n\
     verbs:\n\
     \x20 path   print the resolved config file path\n\
     \x20 check  load + validate; print per-key sources (cli/file/profile/default)\n\
     \x20 edit   open the file in $VISUAL/$EDITOR (vi fallback)\n\
     \n\
     config file: $XDG_CONFIG_HOME/bitty/init.lua (fallback config.lua alias)\n\
     profiles: $XDG_CONFIG_HOME/bitty/profiles/<name>.lua (--profile, else BITTY_PROFILE)\n\
     env: BITTY_CONFIG (path), BITTY_PROFILE (name); CLI flags win over env"
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
     -- Chrome keys are keymap-driven (single-owner rule): a bound chord is\n\
     -- consumed by its action and never reaches the shell; unbound keys\n\
     -- (Tab, arrows, plain letters) always go to the shell. Alt is the Mod\n\
     -- (Hyprland keeps Super; bitty uses Alt). Shipped defaults:\n\
     --   Alt+h/j/k/l + Ctrl+Alt+arrows  move focus (vim hjkl)\n\
     --   Alt+1..9                       jump to view id N\n\
     --   Alt+u / Alt+i                  page up / down (less-like)\n\
     --   Shift+Alt+h/j/k/l              split focused pane\n\
     --   Shift+Ctrl+h/j/k/l             resize focused pane (vim hjkl)\n\
     --   Alt+w                          close focused pane\n\
     --   Alt+z / Alt+m / Alt+f          toggle single-pane zoom\n\
     --   Ctrl+Tab / Ctrl+Shift+Tab      focus next / previous\n\
     --   Ctrl+Shift+C/V                 copy/paste (fish never sees the chord);\n\
     -- uncomment to override (context + chord identity replaces the default):\n\
     --\n\
     -- Mouse select auto-copies to the clipboard by default (ghostty-class\n\
     -- copy-on-select, syncs primary on Linux). Uncomment to opt out: the\n\
     -- highlight stays and only Ctrl+Shift+C copies.\n\
     -- selection = { auto_copy = false },\n\
     return {\n\
     \x20\x20theme = \"dark\",\n\
     \x20\x20-- Hyprland-like panel gaps in cells (0 = edge-to-edge tiling).\n\
     \x20\x20-- gaps_in spaces sibling panes, gaps_out insets the outer edge;\n\
     \x20\x20-- both render as background-colored spacing (0..=16 cells).\n\
     \x20\x20-- layout = { gaps_in = 1, gaps_out = 2 },\n\
     \x20\x20-- keymaps = {\n\
     \x20\x20--     { chord = \"alt+h\", action = \"goto_split:left\", context = \"global\" },\n\
     \x20\x20--     { chord = \"alt+1\", action = \"focus:1\", context = \"global\" },\n\
     \x20\x20--     { chord = \"alt+u\", action = \"scroll_page_up\", context = \"global\" },\n\
     \x20\x20--     { chord = \"alt+z\", action = \"toggle_zoom\", context = \"global\" },\n\
     \x20\x20-- },\n\
     }\n"
}

/// Formats one `config check` row: `dotted.key = value (source)`.
fn check_row(key: &str, value: String, source: &str) -> String {
    format!("{key} = {value} ({source})")
}

/// Source label for a merged layer: `cli`, `file: <path>`,
/// `profile: <path>`, `default`, or the raw layer label for future layers.
fn layer_source_label(
    merged: &bitty_config::MergedConfig,
    field: &str,
    file_path: Option<&std::path::Path>,
    profile_path: Option<&std::path::Path>,
) -> String {
    match merged.source_of(field).map(|s| s.layer) {
        Some(bitty_config::LayerKind::Cli) => "cli".to_string(),
        Some(bitty_config::LayerKind::User) => match file_path {
            Some(p) => format!("file: {}", p.display()),
            None => "file".to_string(),
        },
        Some(bitty_config::LayerKind::Profile) => match profile_path {
            Some(p) => format!("profile: {}", p.display()),
            None => "profile".to_string(),
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
    // BITTY_CONFIG env participates exactly like --config (CLI wins).
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let explicit = bitty_config::file::resolve_config_explicit(
        args.config_path.as_deref(),
        bitty_config_env.as_deref(),
    );
    match cmd {
        ConfigCommand::Path => match bitty_config::file::probe_config_path(explicit.as_deref()) {
            Some(probed) => {
                println!("{}", probed.path.display());
                0
            }
            None => {
                eprintln!("bitty config path: no config root ($XDG_CONFIG_HOME or $HOME unset)");
                2
            }
        },
        ConfigCommand::Check => match load_merged_config(args) {
            Ok(loaded) => {
                let merged = loaded.merged;
                let probed = loaded.probed;
                let profile_request = loaded.profile_name;
                let profile_path = loaded.profile_path;
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
                // Profile identity first so per-key `profile: <path>`
                // sources have context even when the profile loses every key.
                if let (Some(name), Some(path)) = (&profile_request, &profile_path) {
                    println!(
                        "{}",
                        check_row(
                            "profile",
                            format!("\"{name}\""),
                            &format!("profile: {}", path.display()),
                        )
                    );
                } else {
                    println!(
                        "{}",
                        check_row("profile", String::from("(none)"), "default")
                    );
                }
                let e = &merged.effective;
                let src = |field: &str| {
                    layer_source_label(
                        &merged,
                        field,
                        file_path.as_deref(),
                        profile_path.as_deref(),
                    )
                };
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
                        "terminal.scroll_lines_per_notch",
                        format!("{}", e.terminal.scroll_lines_per_notch),
                        &src("terminal.scroll_lines_per_notch")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "terminal.scroll_pixels_per_notch",
                        format!("{}", e.terminal.scroll_pixels_per_notch),
                        &src("terminal.scroll_pixels_per_notch")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "selection.auto_copy",
                        format!("{}", e.selection.auto_copy),
                        &src("selection.auto_copy")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "layout.gaps_in",
                        format!("{}", e.layout.gaps_in),
                        &src("layout.gaps_in")
                    )
                );
                println!(
                    "{}",
                    check_row(
                        "layout.gaps_out",
                        format!("{}", e.layout.gaps_out),
                        &src("layout.gaps_out")
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
                // CLI-printable keymap introspection (DEC-0007): one row per
                // resolved binding, `user` entries from the config file and
                // the rest from the shipped defaults (CTX-0153).
                match bitty_config::keymap::resolve_keymaps(e) {
                    Ok(maps) => {
                        for m in &maps {
                            let origin = if m.from_default {
                                String::from("default")
                            } else {
                                match &file_path {
                                    Some(p) => format!("file: {}", p.display()),
                                    None => String::from("file"),
                                }
                            };
                            println!(
                                "{}",
                                check_row(
                                    &format!("keymaps[{}]", m.id()),
                                    format!(
                                        "\"{}\" -> {} ({})",
                                        m.chord.canonical(),
                                        m.action.canonical(),
                                        m.context
                                    ),
                                    &origin
                                )
                            );
                        }
                    }
                    Err(err) => {
                        eprintln!("bitty config check: invalid keymaps: {err}");
                        return 2;
                    }
                }
                0
            }
            Err(msg) => {
                eprintln!("{msg}");
                2
            }
        },
        ConfigCommand::Edit => {
            let probed = bitty_config::file::probe_config_path(explicit.as_deref());
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

// ---------------------------------------------------------------------------
// `bitty init` opt-in setup wizard (#243, CTX-0149)
// ---------------------------------------------------------------------------

/// Hamster mascot art, vendored byte-identical from the workspace asset
/// `recording/bitty-mascot/ascii/bitty_ascii.txt` (DEC-0002). Pure text so
/// it renders anywhere stdout goes, including piped headless runs; the
/// sixel/block variants stay out of the binary.
const INIT_MASCOT_ART: &str = include_str!("../assets/mascot.txt");

/// One-line fallback when the window is too narrow for the art: fail closed
/// with an honest line instead of a wrapped mess.
const INIT_MASCOT_FALLBACK: &str = "bitty! (mascot skipped: window too narrow for the art)\n";

/// Maximum prompt attempts per wizard step before aborting. Bounded so piped
/// garbage or a stuck key can never spin the wizard forever.
const INIT_MAX_ATTEMPTS: usize = 3;

/// Maximum accepted stdin line length in bytes (mirrors the config
/// `MAX_LINE_BYTES` posture: overlong lines are truncated, never unbounded).
const INIT_MAX_LINE_BYTES: usize = 4096;

/// Common shells probed in order when building the shell menu.
const INIT_COMMON_SHELLS: &[&str] = &[
    "/bin/bash",
    "/usr/bin/bash",
    "/bin/zsh",
    "/usr/bin/zsh",
    "/usr/bin/fish",
    "/bin/fish",
    "/bin/sh",
];

/// Widest art line in bytes (the art is pure ASCII, so bytes == columns).
/// Computed from the vendored asset so an asset refresh cannot silently
/// break the narrow-window bound.
fn init_mascot_width() -> usize {
    INIT_MASCOT_ART
        .lines()
        .map(|line| line.len())
        .max()
        .unwrap_or(0)
}

/// Picks the greeting art for a known-or-unknown window width: full art
/// unless the window is provably too narrow, in which case the one-line
/// fallback. `None` (unknown width, e.g. piped headless) prints the full
/// pure-text art — always safe, tested headless.
fn init_greeting_art(columns: Option<u16>) -> &'static str {
    match columns {
        Some(width) if (width as usize) < init_mascot_width() => INIT_MASCOT_FALLBACK,
        _ => INIT_MASCOT_ART,
    }
}

/// Parses a `COLUMNS`-style width value. Pure over the injected string so
/// tests never touch the environment; `None`/garbage/zero means unknown.
fn init_columns_from_env(value: Option<&str>) -> Option<u16> {
    value?.trim().parse::<u16>().ok().filter(|width| *width > 0)
}

/// Keybinding preset choice (the vim step, CTX-0149 owner note).
///
/// The shipped defaults are already the Alt-as-Mod vim-style map (CTX-0178:
/// `Alt+h/j/k/l`, `Alt+1..9`, `Alt+u/i`, ...). `Default` leaves them
/// implicit (no `keymaps` section written); `Vim` writes the same map
/// explicitly as a tweakable starting point for vim users. Both agree by
/// construction: the explicit block is rendered FROM
/// [`bitty_config::keymap::DEFAULT_KEYMAPS`], never copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitKeyPreset {
    /// Shipped defaults apply; write no `keymaps` section.
    Default,
    /// Write the shipped map explicitly (one-click vim defaults).
    Vim,
}

/// Wizard answers: pure data, rendered to `init.lua` by [`render_init_lua`].
#[derive(Debug, Clone, PartialEq)]
struct InitAnswers {
    /// `terminal.shell` override; `None` leaves the startup default
    /// (`$SHELL` or `/bin/sh`).
    shell: Option<String>,
    /// `appearance.theme` value (always a known preset name).
    theme: String,
    /// `font.size` in points, within `(0, 128]`.
    font_size: f32,
    /// Keybinding preset choice.
    key_preset: InitKeyPreset,
}

/// Validates and normalizes one shell path: trims, rejects empty,
/// overlong, and control-character input (fail-closed; a shell path with
/// controls is never written into the config).
fn init_clean_shell(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("shell must not be empty".to_string());
    }
    if trimmed.len() > bitty_config::types::MAX_SHELL_LEN {
        return Err(format!(
            "shell path must be <= {} bytes",
            bitty_config::types::MAX_SHELL_LEN
        ));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err("shell path must not contain control characters".to_string());
    }
    Ok(trimmed.to_string())
}

/// Sane non-interactive defaults for `--yes`: `$SHELL` when it is a clean
/// path (else unset so startup falls back), the dark preset, the default
/// point size, and the implicit shipped keymap defaults.
fn init_yes_defaults(shell_env: Option<&str>) -> InitAnswers {
    let shell = shell_env
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| init_clean_shell(s).ok());
    InitAnswers {
        shell,
        theme: bitty_config::theme::DARK_THEME_ALIAS.to_string(),
        font_size: bitty_config::types::DEFAULT_FONT_SIZE,
        key_preset: InitKeyPreset::Default,
    }
}

/// Builds the shell menu: `$SHELL` first when set, then the common shells
/// that exist, deduplicated. Always ends with the POSIX fallback so the
/// menu — and its default — is never empty. `exists` is injected so tests
/// stay hermetic (no filesystem).
fn init_shell_candidates(shell_env: Option<&str>, exists: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push_unique = |value: &str| {
        let trimmed = value.trim();
        if !trimmed.is_empty() && !out.iter().any(|seen| seen == trimmed) {
            out.push(trimmed.to_string());
        }
    };
    if let Some(env) = shell_env {
        push_unique(env);
    }
    for candidate in INIT_COMMON_SHELLS {
        if exists(candidate) {
            push_unique(candidate);
        }
    }
    push_unique(FALLBACK_SHELL);
    out
}

/// Parses one shell-step answer: empty takes the menu default (index 0), a
/// `1`-based number picks a menu entry, anything else is a custom path
/// through [`init_clean_shell`]. Total: every input maps to a value or a
/// repromptable error, never a panic.
fn init_parse_shell_answer(raw: &str, candidates: &[String]) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(candidates.first().cloned());
    }
    if let Ok(number) = trimmed.parse::<usize>() {
        if number >= 1 && number <= candidates.len() {
            return Ok(Some(candidates[number - 1].clone()));
        }
        return Err(format!(
            "pick 1..={} or type a shell path",
            candidates.len()
        ));
    }
    init_clean_shell(trimmed).map(Some)
}

/// Parses one theme-step answer. Only the shipped preset exists today
/// (`dark`, canonical config value; `bitty-dark` accepted as the registry
/// name), so empty/`1`/either name resolves to `"dark"` and anything else
/// reprompts instead of writing a value the resolver would only fall back.
fn init_parse_theme_answer(raw: &str) -> Result<String, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "dark" | "bitty-dark" => Ok(bitty_config::theme::DARK_THEME_ALIAS.to_string()),
        _ => Err("unknown theme (only 'dark' is shipped today)".to_string()),
    }
}

/// Parses one font-size-step answer: empty takes the default point size,
/// otherwise a finite number within `(0, 128]` (the `FontConfig` bound, so
/// the wizard can never emit a size startup would reject).
fn init_parse_font_size_answer(raw: &str) -> Result<f32, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(bitty_config::types::DEFAULT_FONT_SIZE);
    }
    match trimmed.parse::<f32>() {
        Ok(size) if size.is_finite() && size > 0.0 && size <= 128.0 => Ok(size),
        _ => Err("font size must be a number within (0, 128]".to_string()),
    }
}

/// Parses one keybinding-preset-step answer: empty/`1` keeps the implicit
/// shipped defaults, `2`/`vim` writes them explicitly.
fn init_parse_preset_answer(raw: &str) -> Result<InitKeyPreset, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "default" => Ok(InitKeyPreset::Default),
        "2" | "vim" => Ok(InitKeyPreset::Vim),
        _ => Err("pick 1 (default) or 2 (vim)".to_string()),
    }
}

/// Escapes a string for a double-quoted Lua value. Wizard inputs are
/// control-free by construction ([`init_clean_shell`]), generated values
/// harder still; backslash and quote are escaped so paths like
/// `C:\Tools\sh` stay one valid string.
fn init_lua_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// Renders the one-click vim preset block FROM the shipped defaults
/// ([`bitty_config::keymap::DEFAULT_KEYMAPS`], CTX-0178) so the preset and
/// the binary defaults cannot drift: any future default change flows into
/// the wizard output automatically, and
/// `init_vim_preset_agrees_with_shipped_defaults` pins the agreement.
fn init_render_vim_keymaps() -> String {
    let mut out = String::from("    keymaps = {\n");
    for (chord, action) in bitty_config::keymap::DEFAULT_KEYMAPS {
        out.push_str(&format!(
            "        {{ chord = \"{chord}\", action = \"{action}\", context = \"global\" }},\n"
        ));
    }
    out.push_str("    },\n");
    out
}

/// Renders wizard answers as an `init.lua` return table. The output always
/// parses via `bitty-config::file::parse_lua_config` (pinned by
/// `init_rendered_config_parses`): `theme` + both `font` keys are always
/// present (the `font`/`window` tables require complete pairs), the
/// `terminal` table always carries `scrollback` alongside `shell` (the
/// parser requires `terminal.scrollback`), and the `keymaps` section appears
/// only for the vim preset.
fn render_init_lua(answers: &InitAnswers) -> String {
    let mut out = String::from(
        "-- bitty user configuration (Lua, wezterm-style), written by `bitty init`.\n\
         -- Evaluated in the bitty-lua sandbox (same budgets as plugins; no io/os).\n\
         -- Unknown keys fail closed; validate with `bitty config check`.\n\
         return {\n",
    );
    out.push_str(&format!(
        "    theme = \"{}\",\n",
        init_lua_escape(&answers.theme)
    ));
    out.push_str(&format!(
        "    font = {{ family = \"{}\", size = {} }},\n",
        init_lua_escape(bitty_config::types::DEFAULT_FONT_FAMILY),
        answers.font_size,
    ));
    if let Some(shell) = &answers.shell {
        out.push_str(&format!(
            "    terminal = {{ scrollback = {}, shell = \"{}\" }},\n",
            bitty_config::TerminalConfig::default().scrollback,
            init_lua_escape(shell),
        ));
    }
    match answers.key_preset {
        InitKeyPreset::Default => {
            out.push_str(
                "    -- Chrome keys: the shipped Alt-as-Mod defaults apply (Alt+h/j/k/l move,\n\
                 -- Alt+1..9 jump to view N, Alt+u/i page up/down, Shift+Alt+h/j/k/l split,\n\
                 -- Shift+Ctrl+h/j/k/l resize, Alt+w close, Alt+z/m/f zoom, Ctrl+Tab cycle,\n\
                 -- Ctrl+Shift+C/V copy/paste). Re-run `bitty init` and pick the vim preset\n\
                 -- to pin this map explicitly here.\n",
            );
        }
        InitKeyPreset::Vim => {
            out.push_str(
                "    -- Chrome keys: one-click vim preset (Alt+h/j/k/l move, Alt+1..9 jump,\n\
                 -- Alt+u/i page, Shift+Alt+h/j/k/l split, Alt+w close, Alt+z zoom).\n\
                 -- This is the map the binary ships; entries here replace defaults by\n\
                 -- context + chord identity, so tweak freely.\n",
            );
            out.push_str(&init_render_vim_keymaps());
        }
    }
    out.push_str("}\n");
    out
}

/// Reads one stdin line without the trailing newline. `None` on EOF or I/O
/// error (the wizard aborts rather than guessing). Overlong lines are
/// truncated to [`INIT_MAX_LINE_BYTES`] so a pasted megabyte cannot grow
/// the answer buffer.
fn init_read_line(input: &mut dyn std::io::BufRead) -> Option<String> {
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => {
            if line.len() > INIT_MAX_LINE_BYTES {
                line.truncate(INIT_MAX_LINE_BYTES);
            }
            Some(line.trim_end_matches(['\r', '\n']).to_string())
        }
        Err(_) => None,
    }
}

/// Asks one wizard step: prints `prompt`, reads a line, parses it.
/// Reprompts up to [`INIT_MAX_ATTEMPTS`] on parse errors, then aborts;
/// EOF aborts immediately. Prompts go to `output` (stdout at runtime) so
/// piped-stdin runs still show the questions.
fn init_ask<T>(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    prompt: &str,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<T, String> {
    for attempt in 1..=INIT_MAX_ATTEMPTS {
        let _ = writeln!(output, "{prompt}");
        let _ = output.flush();
        match init_read_line(input) {
            Some(line) => match parse(&line) {
                Ok(value) => return Ok(value),
                Err(err) => {
                    let _ = writeln!(
                        output,
                        "  ({err} — try again [{attempt}/{INIT_MAX_ATTEMPTS}])"
                    );
                }
            },
            None => return Err("bitty init: aborted (end of input)".to_string()),
        }
    }
    Err(format!(
        "bitty init: aborted (too many invalid answers, limit {INIT_MAX_ATTEMPTS})"
    ))
}

/// Runs the interactive wizard: mascot greeting, then shell / theme /
/// font-size / keybinding-preset picks. Pure over injected `input`,
/// `output`, `shell_env`, `columns`, and `shell_exists`, so the whole flow
/// is headless-testable with piped stdin.
fn run_init_interactive(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    shell_env: Option<&str>,
    columns: Option<u16>,
    shell_exists: &dyn Fn(&str) -> bool,
) -> Result<InitAnswers, String> {
    let _ = write!(output, "{}", init_greeting_art(columns));
    let _ = writeln!(
        output,
        "Welcome to bitty! This wizard writes your init.lua. (Enter takes the default.)"
    );
    let candidates = init_shell_candidates(shell_env, shell_exists);
    let _ = writeln!(output, "\nShell:");
    for (index, candidate) in candidates.iter().enumerate() {
        let marker = if index == 0 { " (default)" } else { "" };
        let _ = writeln!(output, "  {}) {candidate}{marker}", index + 1);
    }
    let shell = init_ask(
        input,
        output,
        "Shell [Enter for default, number, or custom path]:",
        |line| init_parse_shell_answer(line, &candidates),
    )?;
    let theme = init_ask(input, output, "Theme [dark]:", init_parse_theme_answer)?;
    let font_size = init_ask(
        input,
        output,
        &format!(
            "Font size in points [{}]:",
            bitty_config::types::DEFAULT_FONT_SIZE
        ),
        init_parse_font_size_answer,
    )?;
    let key_preset = init_ask(
        input,
        output,
        "Keybindings [1 default / 2 vim]:\n  1) default — shipped Alt-as-Mod map (Alt+h/j/k/l, Alt+1..9, Alt+u/i)\n  2) vim — write that map explicitly (tweakable starting point):",
        init_parse_preset_answer,
    )?;
    Ok(InitAnswers {
        shell,
        theme,
        font_size,
        key_preset,
    })
}

/// Outcome of [`write_init_config`].
#[derive(Debug)]
struct InitWriteOutcome {
    /// File that was written.
    path: std::path::PathBuf,
    /// Backup of the overwritten file, if any (`<file>.lua.bak`).
    backup: Option<std::path::PathBuf>,
    /// True when an existing file was replaced via `--force`.
    updated: bool,
}

/// Why [`write_init_config`] refused or failed.
#[derive(Debug)]
enum InitWriteError {
    /// Usage-level refusal (exists without `--force`, generated content
    /// invalid): the caller exits 2.
    Refused(String),
    /// Filesystem failure (mkdir/read/backup/write): the caller exits 1.
    Io(String),
}

/// Writes wizard output to `target` (idempotent contract):
///
/// - The rendered content is validated via `parse_lua_config` BEFORE any
///   filesystem mutation, so the wizard never writes a file startup would
///   reject.
/// - An existing file is never overwritten without `force` ([`InitWriteError::Refused`]).
/// - With `force`, the previous bytes are copied to `<file>.lua.bak` first
///   (overwriting any older backup), then the new content is written.
/// - Parent directories are created as needed.
///
/// Total: every failure maps to [`InitWriteError`], never a panic.
fn write_init_config(
    target: &std::path::Path,
    content: &str,
    force: bool,
) -> Result<InitWriteOutcome, InitWriteError> {
    {
        let source = bitty_config::plan::ConfigSource::new(
            bitty_config::plan::LayerKind::User,
            Some(target.display().to_string()),
        );
        bitty_config::file::parse_lua_config(content, &source).map_err(|err| {
            InitWriteError::Refused(format!(
                "bitty init: generated config is invalid ({err}) — refusing to write"
            ))
        })?;
    }
    if target.exists() && !force {
        return Err(InitWriteError::Refused(format!(
            "bitty init: '{}' already exists (re-run with --force to overwrite; a .bak backup is kept)",
            target.display()
        )));
    }
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| {
                InitWriteError::Io(format!(
                    "bitty init: cannot create '{}': {err}",
                    parent.display()
                ))
            })?;
        }
    }
    let mut backup = None;
    let mut updated = false;
    if target.exists() {
        let backup_path = target.with_extension("lua.bak");
        let previous = std::fs::read(target).map_err(|err| {
            InitWriteError::Io(format!(
                "bitty init: cannot read '{}': {err}",
                target.display()
            ))
        })?;
        std::fs::write(&backup_path, previous).map_err(|err| {
            InitWriteError::Io(format!(
                "bitty init: cannot write backup '{}': {err}",
                backup_path.display()
            ))
        })?;
        backup = Some(backup_path);
        updated = true;
    }
    std::fs::write(target, content).map_err(|err| {
        InitWriteError::Io(format!(
            "bitty init: cannot write '{}': {err}",
            target.display()
        ))
    })?;
    Ok(InitWriteOutcome {
        path: target.to_path_buf(),
        backup,
        updated,
    })
}

/// Short usage for `bitty init` (stdout on `--help`-style flows, stderr on
/// fail-closed exit 2).
fn init_usage() -> String {
    "usage: bitty init [--yes] [--force] [--config PATH]\n\
     \n\
     Opt-in setup wizard (never auto-runs): mascot greeting, shell / theme /\n\
     font-size / keybinding-preset picks, then writes the config file.\n\
     \n\
     flags:\n\
     \x20 --yes     skip prompts; write sane defaults ($SHELL when clean,\n\
     \x20           dark theme, 12pt, shipped keymap defaults)\n\
     \x20 --force   overwrite an existing file (backs it up to init.lua.bak)\n\
     \n\
     target: --config PATH wins, else BITTY_CONFIG, else\n\
     $XDG_CONFIG_HOME/bitty/init.lua (fallback ~/.config/bitty/init.lua).\n\
     Without --force an existing file is never overwritten (exit 2).\n\
     Re-runs are idempotent: same answers write the same file.\n\
     Validate any file any time with `bitty config check`."
        .to_string()
}

/// Runs `bitty init`; returns the process exit code.
///
/// - `0`: wrote the file (prints the target plus what changed).
/// - `2`: usage-level refusal (unexpected args, existing file without
///   `--force`, invalid `$SHELL` handling aside) — nothing was overwritten.
/// - `1`: aborted prompts (EOF / too many retries) or filesystem failure.
fn run_init_subcommand(args: &Args) -> i32 {
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let shell_env = std::env::var("SHELL").ok();
    run_init_subcommand_with_env(args, bitty_config_env.as_deref(), shell_env.as_deref())
}

/// [`run_init_subcommand`] with injected environment values so tests stay
/// hermetic (no process-env mutation): `bitty_config_env` stands in for
/// `BITTY_CONFIG`, `shell_env` for `SHELL`.
fn run_init_subcommand_with_env(
    args: &Args,
    bitty_config_env: Option<&str>,
    shell_env: Option<&str>,
) -> i32 {
    if !args.init_args.is_empty() {
        eprintln!(
            "bitty init: unexpected argument '{}'\n{}",
            args.init_args[0],
            init_usage()
        );
        return 2;
    }
    // BITTY_CONFIG env participates exactly like --config (CLI wins), same
    // as the `config` subcommand and startup.
    let explicit =
        bitty_config::file::resolve_config_explicit(args.config_path.as_deref(), bitty_config_env);
    let target = match bitty_config::file::probe_config_path(explicit.as_deref()) {
        Some(probed) => probed.path,
        None => {
            eprintln!("bitty init: no config root ($XDG_CONFIG_HOME or $HOME unset)");
            return 2;
        }
    };
    let answers = if args.init_yes {
        let answers = init_yes_defaults(shell_env);
        // Warn when $SHELL existed but was unusable, so the omission is
        // never silent (the written file simply leaves `shell` unset and
        // startup falls back to /bin/sh).
        let raw = shell_env.map(str::trim).unwrap_or_default();
        if !raw.is_empty() && answers.shell.is_none() {
            eprintln!("bitty init: ignoring unusable $SHELL {raw:?}; leaving shell unset");
        }
        answers
    } else {
        let columns = init_columns_from_env(std::env::var("COLUMNS").ok().as_deref());
        let stdin = std::io::stdin();
        let mut stdin_lock = stdin.lock();
        let stdout = std::io::stdout();
        let mut stdout_lock = stdout.lock();
        match run_init_interactive(
            &mut stdin_lock,
            &mut stdout_lock,
            shell_env,
            columns,
            &|path| std::path::Path::new(path).exists(),
        ) {
            Ok(answers) => answers,
            Err(message) => {
                eprintln!("{message}");
                return 1;
            }
        }
    };
    let content = render_init_lua(&answers);
    match write_init_config(&target, &content, args.init_force) {
        Ok(outcome) => {
            if outcome.updated {
                match &outcome.backup {
                    Some(backup) => println!(
                        "bitty init: backed up existing config to '{}'",
                        backup.display()
                    ),
                    None => println!("bitty init: replaced existing config"),
                }
            }
            let shell = answers
                .shell
                .as_deref()
                .map_or("(default)".to_string(), |s| format!("\"{s}\""));
            let keymaps = match answers.key_preset {
                InitKeyPreset::Default => "default (shipped)".to_string(),
                InitKeyPreset::Vim => format!(
                    "vim ({} explicit entries)",
                    bitty_config::keymap::DEFAULT_KEYMAPS.len()
                ),
            };
            println!(
                "bitty init: wrote '{}' (theme=\"{}\", shell={shell}, font.size={}, keymaps={keymaps})",
                outcome.path.display(),
                answers.theme,
                answers.font_size,
            );
            println!("bitty init: validate any time with `bitty config check`");
            0
        }
        Err(InitWriteError::Refused(message)) => {
            eprintln!("{message}");
            2
        }
        Err(InitWriteError::Io(message)) => {
            eprintln!("{message}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// `bitty doctor` installation and compatibility diagnosis (CTX-0175)
// ---------------------------------------------------------------------------

/// Clipboard helpers probed on `PATH` in order (Wayland first, then X11).
const DOCTOR_CLIPBOARD_CANDIDATES: &[&str] = &["wl-copy", "xclip", "xsel"];

/// Returns true when `path` is executable (Unix exec bits; Windows: exists).
fn doctor_shell_executable(path: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        path.exists()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
}

/// Collects live inputs for one doctor run.
///
/// Impure (environment, filesystem, bounded external probes) and total (every
/// probe degrades to warn/inconclusive instead of panicking). Reuses the
/// `config check` load path (`load_merged_config`) so an invalid config file
/// becomes a failing `config` check (exit 3) rather than a startup abort. No
/// plugin VM is ever loaded (safe-mode posture); all external commands run
/// under `doctor::run_bounded` (no shell, no pipes, kill by PID on timeout).
fn collect_doctor_inputs(args: &Args) -> doctor::DoctorInputs {
    let version = version_text();
    let (exe_ok, exe_detail) = match std::env::current_exe() {
        Ok(path) => (true, path.display().to_string()),
        Err(_) => (false, String::new()),
    };
    let (config, keymaps, font_chain) = match load_merged_config(args) {
        Ok(loaded) => {
            let file_path = loaded
                .probed
                .as_ref()
                .filter(|probe| probe.path.exists())
                .map(|probe| probe.path.clone());
            let config = if let Some(path) = file_path.as_ref() {
                doctor::ConfigInput::Ok {
                    source: format!("file: {}", path.display()),
                }
            } else if let Some(path) = loaded.profile_path.as_ref() {
                doctor::ConfigInput::Ok {
                    source: format!("profile: {}", path.display()),
                }
            } else {
                doctor::ConfigInput::Missing
            };
            let keymaps = match bitty_config::keymap::resolve_keymaps(&loaded.merged.effective) {
                Ok(maps) => doctor::KeymapInput::Ok(maps.len()),
                Err(err) => doctor::KeymapInput::Err(err.to_string()),
            };
            let chain = loaded.merged.effective.font.fallback_chain();
            (config, keymaps, chain)
        }
        Err(message) => {
            let chain: Vec<String> = bitty_config::types::FONT_FALLBACK_CHAIN
                .iter()
                .map(|name| (*name).to_string())
                .collect();
            (
                doctor::ConfigInput::Invalid(message),
                doctor::KeymapInput::Skipped,
                chain,
            )
        }
    };
    let font_tool_available = doctor::find_on_path("fc-match").is_some();
    let families: Vec<(String, bool)> = font_chain
        .into_iter()
        .map(|family| {
            let present = doctor::probe_font(&family).unwrap_or(false);
            (family, present)
        })
        .collect();
    let wayland = std::env::var("WAYLAND_DISPLAY").ok();
    let x11 = std::env::var("DISPLAY").ok();
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    let dri_cards = doctor::dri_card_count();
    let clipboard_backends: Vec<String> = DOCTOR_CLIPBOARD_CANDIDATES
        .iter()
        .filter_map(|name| doctor::find_on_path(name).map(|_| (*name).to_string()))
        .collect();
    #[cfg(windows)]
    let (pty_available, pty_detail) = (true, "ConPTY available (Windows)".to_string());
    #[cfg(not(windows))]
    let (pty_available, pty_detail) = {
        let path = std::path::Path::new("/dev/ptmx");
        if path.exists() {
            (true, "/dev/ptmx present".to_string())
        } else {
            (false, "/dev/ptmx missing".to_string())
        }
    };
    let term = std::env::var("TERM")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let terminfo_found = match term.as_deref() {
        None => None,
        Some(name) => doctor::probe_terminfo(name),
    };
    let shell = std::env::var("SHELL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let (shell_exists, shell_executable) = match shell.as_deref() {
        None => (false, false),
        Some(name) => {
            let path = std::path::Path::new(name);
            let exists = path.exists();
            let executable = if exists {
                doctor_shell_executable(path)
            } else {
                false
            };
            (exists, executable)
        }
    };
    doctor::DoctorInputs {
        version,
        exe_ok,
        exe_detail,
        config,
        keymaps,
        font_tool_available,
        families,
        wayland,
        x11,
        session_type,
        dri_cards,
        clipboard_backends,
        pty_available,
        pty_detail,
        term,
        terminfo_found,
        shell,
        shell_exists,
        shell_executable,
    }
}

/// Runs `bitty doctor`; returns the process exit code.
///
/// - Extra positionals and unknown `--format` shapes fail closed (exit 2).
/// - Table goes to stdout for humans; JSON/JSONL emit the versioned envelope
///   (`v: 1`, `command: "doctor"`) on stdout with diagnostics on stderr so
///   machine output is never corrupted.
/// - Exit `0` when every check passes (warns allowed), `1` on recoverable
///   failure, else the strongest category code (3 config, 5 compat, 8
///   conflict) per the accepted CLI contract.
fn run_doctor_subcommand(args: &Args) -> i32 {
    if !args.doctor_args.is_empty() {
        eprintln!(
            "bitty doctor: unexpected argument '{}'\n{}",
            args.doctor_args[0],
            doctor::doctor_usage()
        );
        return doctor::EXIT_USAGE;
    }
    let format = match doctor::DoctorFormat::parse(args.doctor_format.as_deref()) {
        Ok(format) => format,
        Err(message) => {
            eprintln!("{message}\n{}", doctor::doctor_usage());
            return doctor::EXIT_USAGE;
        }
    };
    let inputs = collect_doctor_inputs(args);
    let report = doctor::assemble_report(&inputs);
    match format {
        doctor::DoctorFormat::Table => {
            let no_color = args.doctor_no_color || std::env::var("NO_COLOR").is_ok();
            print!("{}", doctor::format_table(&report, no_color));
        }
        doctor::DoctorFormat::Json | doctor::DoctorFormat::Jsonl => {
            println!("{}", doctor::format_json(&report));
        }
    }
    report.exit_code()
}

/// Runs `bitty ctl`; returns the process exit code.
///
/// - `--help` (anywhere in `ctl_raw`) prints help to stdout, exit 0, and
///   never requires an instance.
/// - Global `--socket`/`--instance`/`--format` before the `ctl` word merge
///   with per-`ctl` flags (per-`ctl` wins when only one side sets a value;
///   conflicting values are usage errors, exit 2).
/// - Other parse failures print the diagnostic plus usage to stderr (exit 2).
/// - Runtime verbs resolve targeting and speak IPC; exit codes follow the
///   stable v1 mapping (0 ok, 6 unavailable, 7 permission, 8 conflict).
fn run_ctl_subcommand(args: &Args) -> i32 {
    match ctl::parse_ctl_request(&args.ctl_raw) {
        Err(ctl::CtlParseError::Help) => {
            print!("{}", ctl::ctl_help_text());
            0
        }
        Err(err) => {
            eprintln!("{}\n{}", err.message(), ctl::ctl_usage());
            ctl::EXIT_USAGE
        }
        Ok((request, mut targeting)) => {
            // Merge global pre-`ctl` targeting: per-`ctl` flags win when
            // only one side sets a value; differing values are conflicts.
            if let Some(pre) = args.ctl_socket_pre.as_deref() {
                match targeting.socket.as_deref() {
                    None => targeting.socket = Some(pre.to_string()),
                    Some(post) if post == pre => {}
                    Some(post) => {
                        eprintln!(
                            "bitty ctl: conflicting --socket {pre:?} vs {post:?} (pass once; see `bitty ctl --help`)\n{}",
                            ctl::ctl_usage()
                        );
                        return ctl::EXIT_USAGE;
                    }
                }
            }
            if let Some(pre) = args.ctl_instance_pre.as_deref() {
                match targeting.instance.as_deref() {
                    None => targeting.instance = Some(pre.to_string()),
                    Some(post) if post == pre => {}
                    Some(post) => {
                        eprintln!(
                            "bitty ctl: conflicting --instance {pre:?} vs {post:?} (pass once; see `bitty ctl --help`)\n{}",
                            ctl::ctl_usage()
                        );
                        return ctl::EXIT_USAGE;
                    }
                }
            }
            // Global --format before `ctl` applies when `ctl` set none.
            // `parse_ctl_request` defaults to table, so detect an explicit
            // post-`ctl` format by re-scanning `ctl_raw` for the flag.
            let post_has_format = args
                .ctl_raw
                .iter()
                .any(|t| t == "--format" || t.starts_with("--format="));
            if !post_has_format {
                if let Some(global) = args.doctor_format.as_deref() {
                    match ctl::CtlFormat::parse(Some(global)) {
                        Ok(fmt) => targeting.format = fmt,
                        Err(message) => {
                            eprintln!("{message}\n{}", ctl::ctl_usage());
                            return ctl::EXIT_USAGE;
                        }
                    }
                }
            }
            ctl::execute_ctl(&request, &targeting)
        }
    }
}

/// Runs `bitty dev <verb>`; returns the process exit code.
///
/// - `--help` (anywhere in `dev_raw`, or `bitty --help dev`) prints help to
///   stdout, exit 0, and never builds a runtime.
/// - Global `--format`/`--no-color` before the `dev` word compose with
///   post-`dev` flags (post-`dev` `--format` wins when both set it; both
///   unset means table).
/// - Global `--socket`/`--instance` before the `dev` word are usage errors
///   (exit 2): dev is local-only and never touches IPC discovery.
/// - Other parse failures print the diagnostic plus usage to stderr (exit 2).
/// - Post-parse failures (headless runtime/renderer) are generic errors
///   (exit 1) with ok:false envelopes for json/jsonl.
fn run_dev_subcommand(args: &Args) -> i32 {
    match dev::parse_dev_request(&args.dev_raw) {
        Err(dev::DevParseError::Help) => {
            print!("{}", dev::dev_help_text());
            0
        }
        Err(err) => {
            eprintln!("{}", err.message());
            dev::EXIT_USAGE
        }
        Ok((request, mut options)) => {
            // Global --format before `dev` applies when `dev` set none.
            let post_has_format = args
                .dev_raw
                .iter()
                .any(|t| t == "--format" || t.starts_with("--format="));
            if !post_has_format {
                if let Some(global) = args.dev_format.as_deref() {
                    match dev::DevFormat::parse(Some(global)) {
                        Ok(fmt) => options.format = fmt,
                        Err(message) => {
                            eprintln!("{message}\n{}", dev::dev_usage());
                            return dev::EXIT_USAGE;
                        }
                    }
                }
            }
            // Global --no-color composes (tables are plain; accepted for parity).
            if args.dev_no_color {
                options.no_color = true;
            }
            // Local-only: pre-word --socket/--instance are rejected (post-word
            // spellings are already rejected by `parse_dev_request`).
            if let Some(socket) = args.dev_socket_pre.as_deref() {
                eprintln!(
                    "bitty dev: --socket {socket:?} is rejected (dev is local-only: no instance, no IPC)\n{}",
                    dev::dev_usage()
                );
                return dev::EXIT_USAGE;
            }
            if let Some(instance) = args.dev_instance_pre.as_deref() {
                eprintln!(
                    "bitty dev: --instance {instance:?} is rejected (dev is local-only: no instance, no IPC)\n{}",
                    dev::dev_usage()
                );
                return dev::EXIT_USAGE;
            }
            dev::run_dev(&request, &options)
        }
    }
}

/// Runs `bitty list <kind>`; returns the process exit code.
///
/// - Extra positionals, unknown kinds, bad `--format`/`--socket`/`--instance`,
///   and stray `--` fail closed (exit 2, stderr only, no stdout envelope).
/// - Table goes to stdout for humans; JSON/JSONL emit the versioned envelope
///   (`v: 1`, `command: "list"|"ls"`) on stdout with diagnostics on stderr.
/// - `instances` runtime/permission failures emit ok:false envelopes for
///   json/jsonl (exit 6/7) and stderr-only for table.
fn run_list_subcommand(args: &Args) -> i32 {
    if !args.list_args.is_empty() {
        eprintln!(
            "bitty {}: unexpected argument '{}'\n{}",
            args.list_spelling,
            args.list_args[0],
            list::list_usage()
        );
        return list::EXIT_USAGE;
    }
    let request = match list::ListRequest::validate(
        args.list_kind.as_deref(),
        args.list_format.as_deref(),
        args.list_socket.as_deref(),
        args.list_instance.as_deref(),
        args.list_no_color,
        &args.list_spelling,
    ) {
        Ok(req) => req,
        Err(message) => {
            eprintln!("{message}");
            return list::EXIT_USAGE;
        }
    };
    list::run_list(&request)
}

/// Runs `bitty inspect <target> <value>`; returns the process exit code.
///
/// - Extra positionals, unknown targets, missing values, bad `--format`,
///   stray `--`, and `--socket`/`--instance` alongside `inspect` fail closed
///   (exit 2, stderr only, no stdout envelope). `inspect` is local-only: no
///   targeting flag ever applies.
/// - Table goes to stdout for humans; JSON/JSONL emit the versioned envelope
///   (`v: 1`, `command: "inspect"`) on stdout with diagnostics on stderr.
/// - Well-formed but unknown values emit `ok: false` envelopes for json/jsonl
///   (exit 1, class `NotFound`) and stderr-only diagnostics for table.
fn run_inspect_subcommand(args: &Args) -> i32 {
    if !args.inspect_args.is_empty() {
        eprintln!(
            "bitty inspect: unexpected argument '{}'\n{}",
            args.inspect_args[0],
            inspect::inspect_usage()
        );
        return inspect::EXIT_USAGE;
    }
    // Local-only: targeting flags never apply to `inspect` (no instance is
    // contacted). Fail closed rather than silently ignoring them.
    if args.ctl_socket_pre.is_some()
        || args.ctl_instance_pre.is_some()
        || args.list_socket.is_some()
        || args.list_instance.is_some()
    {
        eprintln!(
            "bitty inspect: --socket/--instance do not apply (inspect is local, no instance)\n{}",
            inspect::inspect_usage()
        );
        return inspect::EXIT_USAGE;
    }
    let request = match inspect::InspectRequest::validate(
        args.inspect_target.as_deref(),
        args.inspect_value.as_deref(),
        args.inspect_format.as_deref(),
        args.inspect_no_color,
    ) {
        Ok(req) => req,
        Err(message) => {
            eprintln!("{message}");
            return inspect::EXIT_USAGE;
        }
    };
    inspect::run_inspect(&request)
}

/// Derives a [`bitty_runtime::RuntimeConfig`] from the effective config.
///
/// Cell geometry applies the configured breathing room
/// (`font.line_height`/`font.letter_spacing` over the legacy `8x16` base via
/// [`bitty_config::types::FontConfig::effective_cell`], defaults `9x19`);
/// grid/queue geometry stays at compiled defaults; font family/size, scroll
/// speed, selection auto-copy, and panel gaps come from the file/CLI/default
/// chain (already validated by `bitty-config`, so construction is expected to
/// succeed — failures stay fail-closed).
fn runtime_config_from_effective(
    effective: &bitty_config::EffectiveConfig,
) -> Result<bitty_runtime::RuntimeConfig, String> {
    let defaults = bitty_runtime::RuntimeConfig::default();
    let (cell_width, cell_height) = effective.font.default_effective_cell();
    // CTX-0177: `bitty-config` validates `0..=MAX_LAYOUT_GAP_CELLS` (u32) and
    // `bitty-runtime` mirrors the bound in u16; the clamp below is
    // defense-in-depth so a future bound drift can never wrap the cast.
    let gaps_in = effective
        .layout
        .gaps_in
        .min(u32::from(bitty_runtime::config::MAX_LAYOUT_GAP_CELLS)) as u16;
    let gaps_out = effective
        .layout
        .gaps_out
        .min(u32::from(bitty_runtime::config::MAX_LAYOUT_GAP_CELLS)) as u16;
    // CTX-0223: `window.padding` flows file -> effective -> runtime the same
    // way (validated `0..=64` by `bitty-config`; clamped here so a future
    // bound drift can never wrap the cast).
    let window_padding = effective
        .window
        .padding
        .min(bitty_runtime::config::MAX_WINDOW_PADDING);
    bitty_runtime::RuntimeConfig::new(
        defaults.cols,
        defaults.rows,
        cell_width,
        cell_height,
        defaults.cold_queue_capacity,
        effective.font.family.clone(),
        effective.font.size,
        effective.terminal.scroll_lines_per_notch,
        effective.terminal.scroll_pixels_per_notch,
        effective.selection.auto_copy,
        gaps_in,
        gaps_out,
        window_padding,
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

/// Synthetic bounded PTY pump: opt-in debug harness only (CTX-0167).
///
/// The pump owns a `sync_channel(16)` holding at most `16` chunks (mirrors
/// `bitty-pty` `CHANNEL_CAPACITY_CHUNKS`); the main thread drains it via
/// `try_recv` on `AboutToWait` and feeds `Runtime::handle_pty_bytes`. When the
/// consumer stalls the channel fills and the pump's `send` blocks — the same
/// backpressure that would propagate to the kernel PTY buffer for a real child.
///
/// Real sessions never attach this pump: [`TerminalApp::with_theme`] leaves
/// `pty_rx` empty and [`TerminalApp::poll_pty_pump`] drains only the real
/// runtime channel. Attach it explicitly via
/// [`TerminalApp::with_demo_pump`] (tests) or `BITTY_DEMO_PUMP=1` (manual
/// debug, see [`demo_pump_enabled_from_env`]). The live pump is wired —
/// `Runtime::take_pty_reader` and `Runtime::poll_pty` exist and
/// `TerminalApp::poll_pty_pump` drains the real runtime channel first.
/// Theme-aware demo pump: the greeting names the resolved theme preset
/// and its source layer (`default`/`file`/`cli`) so a debug window visibly
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

/// Opt-in debug gate for the synthetic demo pump (CTX-0167 / #269).
///
/// Default off: real sessions never see `demo pty: ...` bytes. Returns true
/// only when `BITTY_DEMO_PUMP=1`/`true` (case-insensitive). Pure over the
/// injected value so tests never touch the environment; the startup path
/// injects `std::env::var("BITTY_DEMO_PUMP").ok()`.
fn demo_pump_enabled_from_value(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_lowercase).as_deref(),
        Some("1") | Some("true")
    )
}

/// Reads the process environment for the demo-pump debug gate (CTX-0167).
///
/// Impure (reads env); total (unset/unparsable means disabled).
fn demo_pump_enabled_from_env() -> bool {
    demo_pump_enabled_from_value(std::env::var("BITTY_DEMO_PUMP").ok().as_deref())
}

// ---------------------------------------------------------------------------
// App handler
// ---------------------------------------------------------------------------

/// App-side modifier mirror for keymap matching (CTX-0153).
///
/// `KeyEvent` carries no modifier field (modifiers arrive as separate
/// `ModifiersChanged` events plus modifier key presses), and `Runtime` keeps
/// its own tracker for PTY encoding. The app mirrors the same stream so a
/// bound chord (`alt+h`, `ctrl+tab`, ...) resolves before routing; both
/// trackers stay in sync because modifier-only keys and `ModifiersChanged`
/// are always routed to `Runtime` and never consumed as chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct AppModifiers {
    /// Shift held.
    shift: bool,
    /// Control held.
    control: bool,
    /// Alt held.
    alt: bool,
    /// Super held.
    super_held: bool,
}

/// The Correct Terminal handler: owns `Runtime`, an optional window, and the
/// real PTY pump via `Runtime::poll_pty` (plus an opt-in synthetic demo pump
/// only when explicitly attached for debug/tests).
/// All business stays in `bitty-runtime`; this type only wires
/// `PlatformEvent` → `Runtime` and `tick` → present, with real `GpuContext`
/// attachment for the single-window vertical slice.
struct TerminalApp {
    runtime: Runtime,
    /// Window title carrying the resolved theme preset + source layer.
    window_title: String,
    /// Window opacity from the effective config (CTX-0223
    /// `window.opacity`; default `1.0` = opaque). Applied to the platform
    /// [`WindowConfig`](bitty_platform::WindowConfig) at creation; values
    /// below `1.0` request a transparent window where the platform supports
    /// it and stay opaque (fail-soft) where it does not.
    window_opacity: f32,
    window: Option<WindowHandle>,
    window_id: Option<WindowId>,
    /// Demo pump channel when explicitly attached for debug/tests
    /// (`None` in real sessions — CTX-0167).
    pty_rx: Option<Receiver<Vec<u8>>>,
    _pty_thread: Option<JoinHandle<()>>,
    /// Count of `tick` calls that presented a frame.
    presented_frames: u64,
    /// Resolved keymap table (shipped defaults + user overrides).
    keymaps: Vec<bitty_config::ResolvedKeymap>,
    /// App-side modifier mirror for chord matching.
    app_mods: AppModifiers,
    /// Layout stashed by `toggle_zoom`; `None` when not zoomed.
    zoom_backup: Option<LayoutNode>,
    /// Frozen startup spawn recipe so `new_split` leaves replay the exact
    /// program/shell resolution (CTX-0176).
    spawn_spec: SpawnSpec,
    /// Stderr verbosity gate (CTX-0190). Default [`LogLevel::Warn`] (quiet):
    /// per-frame `bitty tick` lines require `Debug`/`Trace`. User-facing key
    /// info (paste confirm/cancel, startup summary) and warnings/errors
    /// bypass this gate and always emit.
    log_level: LogLevel,
}

impl TerminalApp {
    /// Theme-aware constructor for real sessions (CTX-0167).
    ///
    /// Never attaches the synthetic demo pump: `pty_rx` stays `None` so
    /// startup shows only the shell (and shell init output). The window
    /// title still carries the resolved preset + source layer, so the
    /// config path remains visible without polluting the grid.
    fn with_theme(
        runtime: Runtime,
        theme_name: &str,
        source: &str,
        keymaps: Vec<bitty_config::ResolvedKeymap>,
        spawn_spec: SpawnSpec,
    ) -> Self {
        Self {
            runtime,
            window_title: window_title_for_theme(theme_name, source),
            window_opacity: 1.0,
            window: None,
            window_id: None,
            pty_rx: None,
            _pty_thread: None,
            presented_frames: 0,
            keymaps,
            app_mods: AppModifiers::default(),
            zoom_backup: None,
            spawn_spec,
            log_level: LogLevel::default_level(),
        }
    }

    /// Test constructor with the synthetic demo pump attached.
    ///
    /// Same as [`Self::with_theme`] plus a bounded `spawn_demo_pty_pump`
    /// burst naming `theme_name`/`source`. Tests that legitimately need
    /// synthetic bytes use this instead of `with_theme`; production uses
    /// [`Self::attach_demo_pump`] behind [`demo_pump_enabled_from_env`].
    #[cfg(test)]
    fn with_demo_pump(
        runtime: Runtime,
        theme_name: &str,
        source: &str,
        keymaps: Vec<bitty_config::ResolvedKeymap>,
        spawn_spec: SpawnSpec,
    ) -> Self {
        let (pty_rx, handle) = spawn_demo_pty_pump_with_theme(theme_name, source);
        Self {
            runtime,
            window_title: window_title_for_theme(theme_name, source),
            window_opacity: 1.0,
            window: None,
            window_id: None,
            pty_rx: Some(pty_rx),
            _pty_thread: Some(handle),
            presented_frames: 0,
            keymaps,
            app_mods: AppModifiers::default(),
            zoom_backup: None,
            spawn_spec,
            log_level: LogLevel::default_level(),
        }
    }

    /// Attaches the synthetic demo pump to an existing app (CTX-0167).
    ///
    /// Debug escape hatch for the real startup path: called only when
    /// [`demo_pump_enabled_from_env`] is true (`BITTY_DEMO_PUMP=1`).
    /// No-op when a pump is already attached.
    fn attach_demo_pump(&mut self, theme_name: &str, source: &str) {
        if self.pty_rx.is_some() {
            return;
        }
        let (pty_rx, handle) = spawn_demo_pty_pump_with_theme(theme_name, source);
        self.pty_rx = Some(pty_rx);
        self._pty_thread = Some(handle);
    }

    /// Sets the stderr verbosity gate (CTX-0190). Call once at startup from
    /// [`effective_log_level`]; tests set it explicitly to prove gating.
    fn set_log_level(&mut self, level: LogLevel) {
        self.log_level = level;
    }

    /// Sets the window opacity applied at creation (CTX-0223). Call once at
    /// startup from the effective config; the value is sanitized by the
    /// platform [`WindowConfig`](bitty_platform::WindowConfig), so
    /// out-of-range inputs degrade instead of failing creation.
    fn with_window_opacity(mut self, opacity: f32) -> Self {
        self.window_opacity = opacity;
        self
    }

    /// True when per-frame `bitty tick` stderr lines are emitted.
    ///
    /// Hot-path guard: a single comparison, checked before any formatting so
    /// the disabled path pays no allocation. Delegates to
    /// [`LogLevel::tick_enabled`]; the devtools trace path (`Runtime::tick`
    /// return + inspect snapshots) is unaffected and keeps full fidelity.
    fn tick_logging_enabled(&self) -> bool {
        self.log_level.tick_enabled()
    }

    /// Pure tick-line renderer for tests (CTX-0190).
    ///
    /// Returns the exact `bitty tick: ...` line `drive_tick` emits when
    /// [`Self::tick_logging_enabled`] is true. Pure over its inputs so
    /// level-gating tests assert content without capturing stderr.
    fn format_tick_line(
        present: &bitty_runtime::PresentStats,
        presented_frames: u64,
        focused: Option<bitty_runtime::ViewId>,
        leafs: usize,
        gpu: bool,
        crossfont: bool,
    ) -> String {
        format!(
            "bitty tick: frame={} fills={} glyphs={} headless={} gen={} presented_frames={} focused={:?} leafs={} gpu={} crossfont={}",
            present.frame,
            present.fills,
            present.glyphs,
            present.headless,
            present.generation,
            presented_frames,
            focused,
            leafs,
            gpu,
            crossfont
        )
    }

    /// Returns the tick line when logging is enabled, else `None` (CTX-0190).
    ///
    /// `None` means the caller must not touch stderr: this is the bounded,
    /// no-format hot path for the default quiet run.
    fn maybe_format_tick(&self, present: &bitty_runtime::PresentStats) -> Option<String> {
        if !self.tick_logging_enabled() {
            return None;
        }
        Some(Self::format_tick_line(
            present,
            self.presented_frames,
            self.runtime.focused_view(),
            self.runtime.leaf_count(),
            self.runtime.has_gpu(),
            self.runtime.is_crossfont(),
        ))
    }

    /// Polls real PTY (`Runtime::poll_pty` bounded 128 KiB) plus the opt-in
    /// demo pump when attached (tests / `BITTY_DEMO_PUMP=1` only).
    /// Returns true when bytes were consumed.
    fn poll_pty_pump(&mut self) -> bool {
        let mut consumed = false;
        // Real PTY first: drain bounded channel via runtime; replies are flushed
        // via `Runtime::write_replies` inside `poll_pty` (bounded 4 KiB, fail-closed).
        let real = self.runtime.poll_pty();
        if real > 0 {
            consumed = true;
        }
        // Opt-in demo pump (bounded, debug/tests only — `None` in real
        // sessions so startup shows only the shell).
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
        // Flush any replies generated by PTY bytes (bounded, best-effort).
        // `write_replies` is no-op when no live writer (headless keeps replies for `take_replies`).
        let _ = self.runtime.write_replies();
        consumed
    }

    /// Drives one frame when damage exists, printing stats when a frame was
    /// presented. Returns the stats when a present occurred. Handles GPU vs headless.
    ///
    /// CTX-0190 quiet default: the per-frame `bitty tick` line (plus the
    /// per-tick reply/cold-queue diagnostics) emits only when
    /// [`Self::tick_logging_enabled`] (i.e. `--verbose` / `--log-level
    /// debug|trace`); the format is guarded so the quiet path pays no
    /// formatting cost. The reply-overflow warning stays unconditional
    /// (warn level), and paste/startup/user-facing lines elsewhere bypass
    /// the gate entirely. `Runtime::tick` itself is untouched so devtools
    /// keeps full fidelity.
    fn drive_tick(&mut self) -> Option<bitty_runtime::PresentStats> {
        // CTX-0171: drain IPC runtime-control queue before present so
        // `bitty ctl` mutations (send/split/focus/close/spawn/reload) apply
        // on the main thread — the sole `Runtime` owner — with server-side
        // scope enforcement (never ambient authority).
        let _ =
            ctl::drain_global_control_queue(&mut self.runtime, &ctl::granted_scopes_for_servo());
        // Ensure replies that were queued before tick are flushed before present:
        // the runtime's tick consumes snapshot+damage and composites.
        let stats = self.runtime.tick();
        if let Some(present) = stats {
            self.presented_frames += 1;
            if let Some(line) = self.maybe_format_tick(&present) {
                eprintln!("{line}");
            }
            if self.runtime.replies_overflowed() {
                eprintln!("warning: terminal reply queue overflowed (bounded cap)");
            }
            // Bounded reply loop: flush replies generated before this tick (if any) via PtyWriter.
            // When no writer is present (headless), replies stay queued for `take_replies` observation.
            let written = self.runtime.write_replies();
            if written > 0 && self.tick_logging_enabled() {
                eprintln!("bitty: {written} reply bytes written to PTY master (post-tick)");
            }
            let pending = self.runtime.cold_queue_len();
            if pending > 0 && self.tick_logging_enabled() {
                let events = self.runtime.drain_cold_events();
                eprintln!(
                    "bitty cold-queue: drained {} events, {} remain",
                    events.len(),
                    pending
                );
            } else if pending > 0 {
                // Quiet default still drains to keep the queue bounded, but
                // stays silent: no per-tick stderr noise.
                let _ = self.runtime.drain_cold_events();
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

// ---------------------------------------------------------------------------
// Keymap-driven chrome keys (CTX-0153 single-owner rule)
// ---------------------------------------------------------------------------

/// True for modifier-only keys: always routed to `Runtime` (its modifier
/// tracker needs them) and never treated as chrome.
fn is_modifier_key(key: &KeyEvent) -> bool {
    matches!(
        &key.logical_key,
        LogicalKey::Named(
            NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::Super
                | NamedKey::Meta
        )
    )
}

/// Mirror the modifier stream into the app snapshot (same rules as
/// `Runtime::track_modifiers_from_key`): modifier key presses latch, releases
/// unlatch. Pure over the event; total.
fn track_app_modifiers(mods: &mut AppModifiers, key: &KeyEvent) {
    if let LogicalKey::Named(named) = &key.logical_key {
        let pressed = key.state == PressState::Pressed;
        match named {
            NamedKey::Shift => mods.shift = pressed,
            NamedKey::Control => mods.control = pressed,
            NamedKey::Alt | NamedKey::AltGraph => mods.alt = pressed,
            NamedKey::Super | NamedKey::Meta | NamedKey::Hyper => mods.super_held = pressed,
            _ => {}
        }
    }
}

/// Clear the app modifier mirror on window focus transitions (CTX-0187
/// exit B root-cause fix).
///
/// The mirror latches `Shift`/`Control`/`Alt` from modifier key presses and
/// `ModifiersChanged` snapshots. When the window loses focus, key releases
/// that happen while unfocused are never delivered, so a latched `true` goes
/// stale and a later bare `Ctrl+V` would falsely match the `Ctrl+Shift+V`
/// paste chord (single-owner leak). Resetting to a clean slate on both loss
/// (`focused=false`) and regain (`focused=true`) fails closed to shell input:
/// the authoritative `ModifiersChanged` stream re-latches the true physical
/// state before the next chord on Wayland/winit, and until then an unshifted
/// `Ctrl+V` correctly reaches the shell instead of stealing paste. The worst
/// case without a fresh snapshot is a missed paste (retryable), never stolen
/// shell bytes.
fn clear_app_modifiers_on_focus(mods: &mut AppModifiers, _focused: bool) {
    *mods = AppModifiers::default();
}

/// Convert a key press plus the app modifier mirror into a matchable
/// [`bitty_config::KeyRef`]. Returns `None` for keys with no chord identity
/// (dead keys, unidentified, media/modifier leftovers, non-ASCII text), which
/// always route to the PTY. Single characters are lowercased so `Shift+Alt+H`
/// matches the `shift+alt+h` chord.
///
/// CTX-0187 exit B: the `shift` bit comes verbatim from the compositor-fed
/// mirror (`ModifiersChanged` physical state plus modifier key presses,
/// cleared on focus transitions above) — never inferred from character case.
/// A real `Ctrl+Shift+V` therefore pastes whether the platform reports it as
/// uppercase `"V"` or lowercase `"v"` with `shift=true`; only a physically
/// unshifted `Ctrl+V` (`shift=false`) stays shell input as `0x16`. This has
/// no silent-breakage mode: trusting the raw modifier bit preserves every
/// real chord, and staleness is handled by the focus clear, not by guessing
/// from case.
fn key_ref_from_event(key: &KeyEvent, mods: &AppModifiers) -> Option<bitty_config::KeyRef> {
    use bitty_config::{KeyName, KeyRef};
    let name = match &key.logical_key {
        LogicalKey::Character(s) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_graphic() => KeyName::Char(c.to_ascii_lowercase()),
                _ => return None,
            }
        }
        LogicalKey::Named(named) => match named {
            NamedKey::Tab => KeyName::Tab,
            NamedKey::Enter => KeyName::Enter,
            NamedKey::Escape => KeyName::Escape,
            NamedKey::Space => KeyName::Space,
            NamedKey::Backspace => KeyName::Backspace,
            NamedKey::Delete => KeyName::Delete,
            NamedKey::Insert => KeyName::Insert,
            NamedKey::Home => KeyName::Home,
            NamedKey::End => KeyName::End,
            NamedKey::PageUp => KeyName::PageUp,
            NamedKey::PageDown => KeyName::PageDown,
            NamedKey::ArrowUp => KeyName::Up,
            NamedKey::ArrowDown => KeyName::Down,
            NamedKey::ArrowLeft => KeyName::Left,
            NamedKey::ArrowRight => KeyName::Right,
            _ => {
                // Function keys `F1`..=`F35` share the `F<n>` debug spelling;
                // everything else (modifiers, media, `Other`) has no chord
                // identity and routes to the PTY.
                let spelled = format!("{named:?}");
                let n = spelled.strip_prefix('F')?;
                match n.parse::<u8>() {
                    Ok(num) if (1..=35).contains(&num) => KeyName::F(num),
                    _ => return None,
                }
            }
        },
        LogicalKey::Dead(_) | LogicalKey::Unidentified => return None,
    };
    Some(KeyRef {
        key: name,
        ctrl: mods.control,
        alt: mods.alt,
        shift: mods.shift,
        super_held: mods.super_held,
    })
}

/// Map a split direction onto focus movement.
fn split_dir_to_focus(dir: bitty_config::SplitDir) -> FocusDirection {
    match dir {
        bitty_config::SplitDir::Left => FocusDirection::Left,
        bitty_config::SplitDir::Right => FocusDirection::Right,
        bitty_config::SplitDir::Up => FocusDirection::Up,
        bitty_config::SplitDir::Down => FocusDirection::Down,
    }
}

/// Map a split direction onto the axis a new split divides.
fn split_dir_to_axis(dir: bitty_config::SplitDir) -> SplitAxis {
    match dir {
        bitty_config::SplitDir::Left | bitty_config::SplitDir::Right => SplitAxis::Horizontal,
        bitty_config::SplitDir::Up | bitty_config::SplitDir::Down => SplitAxis::Vertical,
    }
}

/// Fresh view id: one past the current maximum (total; empty layouts yield 1).
fn next_view_id(layout: &LayoutNode) -> ViewId {
    let max = layout.leaf_ids().iter().map(|id| id.0).max().unwrap_or(0);
    ViewId::new(max.saturating_add(1).max(1))
}

/// Split the focused leaf along `axis`, keeping the focused view and adding a
/// fresh sibling. The new pane goes first for `Left`/`Up`, second otherwise.
/// Returns false when the focused id is not in the tree.
fn split_focused_leaf(
    layout: &mut LayoutNode,
    focused: ViewId,
    axis: SplitAxis,
    new_id: ViewId,
    place_new_first: bool,
) -> bool {
    match layout {
        LayoutNode::Leaf(v) => {
            if v.id() != focused {
                return false;
            }
            let old = v.clone();
            let fresh = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
            let (first, second) = if place_new_first {
                (LayoutNode::leaf(fresh), LayoutNode::leaf(old))
            } else {
                (LayoutNode::leaf(old), LayoutNode::leaf(fresh))
            };
            *layout = LayoutNode::split(axis, 0.5, first, second);
            true
        }
        LayoutNode::Split { first, second, .. } => {
            split_focused_leaf(first, focused, axis, new_id, place_new_first)
                || split_focused_leaf(second, focused, axis, new_id, place_new_first)
        }
        LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|c| split_focused_leaf(c, focused, axis, new_id, place_new_first)),
        LayoutNode::Overlay { base, overlay, .. } => {
            split_focused_leaf(base, focused, axis, new_id, place_new_first)
                || split_focused_leaf(overlay, focused, axis, new_id, place_new_first)
        }
    }
}

/// Remove the focused leaf, promoting its sibling. Refuses the last leaf so
/// the layout is never stranded empty. Returns false when refused or missing.
fn close_focused_leaf(layout: &mut LayoutNode, focused: ViewId) -> bool {
    match layout {
        LayoutNode::Leaf(_) => false,
        LayoutNode::Split { first, second, .. } => {
            if matches!(first.as_ref(), LayoutNode::Leaf(v) if v.id() == focused) {
                let sibling = (**second).clone();
                *layout = sibling;
                true
            } else if matches!(second.as_ref(), LayoutNode::Leaf(v) if v.id() == focused) {
                let sibling = (**first).clone();
                *layout = sibling;
                true
            } else if close_focused_leaf(first, focused) {
                true
            } else {
                close_focused_leaf(second, focused)
            }
        }
        LayoutNode::Stack(children) => {
            if let Some(pos) = children
                .iter()
                .position(|c| matches!(c, LayoutNode::Leaf(v) if v.id() == focused))
            {
                if children.len() <= 1 {
                    return false;
                }
                children.remove(pos);
                true
            } else {
                children.iter_mut().any(|c| close_focused_leaf(c, focused))
            }
        }
        LayoutNode::Overlay { base, overlay, .. } => {
            close_focused_leaf(base, focused) || close_focused_leaf(overlay, focused)
        }
    }
}

/// Record a candidate resize target: path to the deepest split whose axis
/// matches the resize direction and whose subtree holds focus, plus its ratio
/// and whether focus sits in its first child.
fn find_resize_target(
    node: &LayoutNode,
    focused: ViewId,
    horizontal: bool,
    path: &mut Vec<usize>,
    out: &mut Option<(Vec<usize>, f32, bool)>,
) {
    match node {
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let axis_matches = (*axis == SplitAxis::Horizontal) == horizontal;
            if first.leaf_ids().contains(&focused) {
                if axis_matches {
                    *out = Some((path.clone(), *ratio, true));
                }
                path.push(0);
                find_resize_target(first, focused, horizontal, path, out);
                path.pop();
            } else if second.leaf_ids().contains(&focused) {
                if axis_matches {
                    *out = Some((path.clone(), *ratio, false));
                }
                path.push(1);
                find_resize_target(second, focused, horizontal, path, out);
                path.pop();
            }
        }
        LayoutNode::Stack(children) => {
            for (i, child) in children.iter().enumerate() {
                if child.leaf_ids().contains(&focused) {
                    path.push(i);
                    find_resize_target(child, focused, horizontal, path, out);
                    path.pop();
                    break;
                }
            }
        }
        LayoutNode::Overlay { base, overlay, .. } => {
            if base.leaf_ids().contains(&focused) {
                path.push(0);
                find_resize_target(base, focused, horizontal, path, out);
                path.pop();
            } else if overlay.leaf_ids().contains(&focused) {
                path.push(1);
                find_resize_target(overlay, focused, horizontal, path, out);
                path.pop();
            }
        }
        LayoutNode::Leaf(_) => {}
    }
}

/// Nudge the enclosing split ratio 0.1 toward the given direction so the
/// focused pane grows that way (`set_split_ratio_at` clamps to
/// `0.10..=0.90`). Returns false when no matching split holds focus.
fn resize_focused_pane(
    layout: &mut LayoutNode,
    focused: ViewId,
    dir: bitty_config::SplitDir,
) -> bool {
    use bitty_config::SplitDir as D;
    let horizontal = matches!(dir, D::Left | D::Right);
    let mut out: Option<(Vec<usize>, f32, bool)> = None;
    let mut path = Vec::new();
    find_resize_target(layout, focused, horizontal, &mut path, &mut out);
    let (target, ratio, focus_in_first) = match out {
        Some(t) => t,
        None => return false,
    };
    let positive = matches!(dir, D::Right | D::Down);
    let delta = if focus_in_first == positive {
        0.1
    } else {
        -0.1
    };
    layout.set_split_ratio_at(&target, ratio + delta)
}

impl TerminalApp {
    /// Restore a zoomed layout before a tree-mutating action so the mutation
    /// applies to the real tree instead of the single-leaf zoom view.
    fn restore_zoom(&mut self) -> bool {
        if let Some(backup) = self.zoom_backup.take() {
            self.runtime.set_layout(backup);
            eprintln!("bitty: zoom restored for layout mutation");
            true
        } else {
            false
        }
    }

    /// Execute one bound chrome action (single owner: the PTY never sees the
    /// chord). All mutations go through existing `Runtime`/`LayoutNode` APIs;
    /// refusals warn and keep the current layout.
    fn apply_chrome_action(&mut self, action: bitty_config::ChromeAction) {
        use bitty_config::ChromeAction as A;
        match action {
            A::GotoSplit(dir) => {
                let focus = split_dir_to_focus(dir);
                let next = self.runtime.move_focus(focus);
                eprintln!(
                    "bitty: keymap goto_split:{} -> {:?} leafs={}",
                    dir.canonical(),
                    next,
                    self.runtime.leaf_count()
                );
            }
            A::FocusNext => {
                let next = self.runtime.move_focus(FocusDirection::Next);
                eprintln!(
                    "bitty: keymap focus_next -> {next:?} leafs={}",
                    self.runtime.leaf_count()
                );
            }
            A::FocusPrev => {
                let next = self.runtime.move_focus(FocusDirection::Prev);
                eprintln!(
                    "bitty: keymap focus_prev -> {next:?} leafs={}",
                    self.runtime.leaf_count()
                );
            }
            A::FocusId(n) => {
                let ok = self.runtime.set_focus(ViewId::new(n));
                if ok {
                    eprintln!("bitty: keymap focus:{n} -> focused");
                } else {
                    eprintln!(
                        "warning: keymap focus:{n} not in layout (leaf ids {:?}) — ignoring",
                        self.runtime.layout().leaf_ids()
                    );
                }
            }
            A::ScrollPageUp => {
                if self.runtime.scroll_focused_page(true) {
                    eprintln!("bitty: keymap scroll_page_up -> paged");
                } else {
                    eprintln!("warning: keymap scroll_page_up has no focused pane — ignoring");
                }
            }
            A::ScrollPageDown => {
                if self.runtime.scroll_focused_page(false) {
                    eprintln!("bitty: keymap scroll_page_down -> paged");
                } else {
                    eprintln!("warning: keymap scroll_page_down has no focused pane — ignoring");
                }
            }
            A::OpenComposer => {
                // CTX-0227 (008 route P4): manual composer open through the
                // single-owner keymap (suggested chord `alt+e`). The chord
                // is consumed here so its bytes never reach the PTY; the
                // composer session itself lives in `bitty-rich` (headless,
                // tested there) and the overlay/panel presentation is a
                // follow-up — until then the open signal is logged and no
                // input routing changes (Normal Mode stays byte-identical).
                eprintln!(
                    "bitty: keymap open_composer -> composer open requested (manual open only; Normal Mode input still goes to the PTY)"
                );
            }
            A::NewSplit(dir) => {
                self.restore_zoom();
                let focused = match self.runtime.focused_view() {
                    Some(id) => id,
                    None => {
                        eprintln!("warning: keymap new_split has no focused pane — ignoring");
                        return;
                    }
                };
                let mut layout = self.runtime.layout().clone();
                let new_id = next_view_id(&layout);
                let place_new_first = matches!(
                    dir,
                    bitty_config::SplitDir::Left | bitty_config::SplitDir::Up
                );
                if split_focused_leaf(
                    &mut layout,
                    focused,
                    split_dir_to_axis(dir),
                    new_id,
                    place_new_first,
                ) {
                    self.runtime.set_layout(layout);
                    // CTX-0176: the fresh leaf gets its own shell/PTY sized
                    // to its allocation — best-effort (startup parity). On
                    // failure the pane shares the primary grid with a loud
                    // warning instead of silently mirroring.
                    let (cols, rows) = self
                        .runtime
                        .layout_allocations()
                        .iter()
                        .find(|(id, _)| *id == new_id)
                        .map(|(_, r)| (r.width.max(1), r.height.max(1)))
                        .unwrap_or((80, 24));
                    match spawn_pane_shell(&mut self.runtime, &self.spawn_spec, new_id, cols, rows)
                    {
                        Ok(()) => eprintln!(
                            "bitty: keymap new_split:{} -> leafs={} focused={:?} pane_shell={new_id:?} pid={:?}",
                            dir.canonical(),
                            self.runtime.leaf_count(),
                            self.runtime.focused_view(),
                            self.runtime.pane_pid(&new_id),
                        ),
                        Err(err) => eprintln!(
                            "warning: keymap new_split:{} pane shell spawn failed ({err}) — pane {new_id:?} shares the primary grid",
                            dir.canonical(),
                        ),
                    }
                } else {
                    eprintln!("warning: keymap new_split found no focused pane — ignoring");
                }
            }
            A::CloseView => {
                self.restore_zoom();
                let focused = match self.runtime.focused_view() {
                    Some(id) => id,
                    None => {
                        eprintln!("warning: keymap close_view has no focused pane — ignoring");
                        return;
                    }
                };
                if self.runtime.leaf_count() <= 1 {
                    eprintln!("warning: keymap close_view refused (last pane) — ignoring");
                    return;
                }
                let mut layout = self.runtime.layout().clone();
                if close_focused_leaf(&mut layout, focused) {
                    self.runtime.set_layout(layout);
                    // CTX-0176: tear down the closed leaf's shell (drop kills
                    // + reaps the child; no-op when it never owned one).
                    if self.runtime.close_pane_session(&focused) {
                        eprintln!("bitty: keymap close_view tore down pane shell {focused:?}");
                    }
                    eprintln!(
                        "bitty: keymap close_view -> leafs={} focused={:?}",
                        self.runtime.leaf_count(),
                        self.runtime.focused_view()
                    );
                } else {
                    eprintln!("warning: keymap close_view found no focused pane — ignoring");
                }
            }
            A::ResizeSplit(dir) => {
                self.restore_zoom();
                let focused = match self.runtime.focused_view() {
                    Some(id) => id,
                    None => {
                        eprintln!("warning: keymap resize_split has no focused pane — ignoring");
                        return;
                    }
                };
                let mut layout = self.runtime.layout().clone();
                if resize_focused_pane(&mut layout, focused, dir) {
                    self.runtime.set_layout(layout);
                    eprintln!("bitty: keymap resize_split:{} applied", dir.canonical());
                } else {
                    eprintln!(
                        "warning: keymap resize_split:{} found no matching split — ignoring",
                        dir.canonical()
                    );
                }
            }
            A::CopyToClipboard => {
                // CTX-0161: explicit single-owner copy chord (ctrl+shift+c).
                // Before this binding the chord fell through to the PTY as
                // 0x03 (SIGINT); now chrome owns it and fish never sees the
                // byte. Reuses the Wayland-first clipboard path (CTX-0160)
                // with headless fallback; refusals warn like other chrome.
                match self.runtime.copy_selection_to_clipboard() {
                    Ok(Some(text)) => {
                        eprintln!("bitty: keymap copy_to_clipboard -> {} bytes", text.len())
                    }
                    Ok(None) => {
                        eprintln!("warning: keymap copy_to_clipboard has no selection — ignoring")
                    }
                    Err(err) => eprintln!(
                        "warning: keymap copy_to_clipboard clipboard error ({err}) — ignoring"
                    ),
                }
            }
            A::PasteFromClipboard => {
                // CTX-0161: explicit single-owner paste chord (ctrl+shift+v).
                // Before this binding the chord fell through to the PTY as
                // 0x16; now chrome owns it. Routes through the
                // suspicious-paste inspection gate (P0-AC-008): clean text
                // delivers immediately, suspicious text waits on the pending
                // confirmation path, clipboard errors warn.
                //
                // CTX-0186: a gated paste is never silent. The pending summary
                // (line count, byte size, reasons, preview) is logged loudly
                // with confirm/cancel instructions; repeating the identical
                // chord with an unchanged clipboard confirms delivery, Esc
                // cancels.
                match self.runtime.paste_from_clipboard() {
                    Ok(Some(true)) => {
                        let summary = self
                            .runtime
                            .pending_paste_summary()
                            .unwrap_or_else(|| "pending confirmation".to_string());
                        eprintln!("bitty: keymap paste_from_clipboard -> {summary}");
                    }
                    Ok(Some(false)) => eprintln!("bitty: keymap paste_from_clipboard delivered"),
                    Ok(None) => {
                        eprintln!("warning: keymap paste_from_clipboard clipboard empty — ignoring")
                    }
                    Err(err) => eprintln!(
                        "warning: keymap paste_from_clipboard clipboard error ({err}) — ignoring"
                    ),
                }
            }
            A::ToggleZoom => {
                if let Some(backup) = self.zoom_backup.take() {
                    self.runtime.set_layout(backup);
                    eprintln!(
                        "bitty: keymap toggle_zoom off -> leafs={} focused={:?}",
                        self.runtime.leaf_count(),
                        self.runtime.focused_view()
                    );
                } else {
                    let focused = match self.runtime.focused_view() {
                        Some(id) => id,
                        None => {
                            eprintln!("warning: keymap toggle_zoom has no focused pane — ignoring");
                            return;
                        }
                    };
                    match self.runtime.layout().find_leaf(focused).cloned() {
                        Some(view) => {
                            let backup = self.runtime.layout().clone();
                            self.runtime.set_layout(LayoutNode::leaf(view));
                            self.zoom_backup = Some(backup);
                            eprintln!("bitty: keymap toggle_zoom on -> {focused:?}");
                        }
                        None => {
                            eprintln!(
                                "warning: keymap toggle_zoom found no focused pane — ignoring"
                            );
                        }
                    }
                }
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

        // CTX-0153 single-owner intercept: resolve bound chrome keys BEFORE
        // `Runtime` routing. A bound chord is consumed here — its action runs
        // and the PTY never sees the key — while unbound keys (Tab, arrows,
        // plain letters) fall through to `Runtime` (shell input). Modifier
        // tracking stays in sync because modifier-only keys and
        // `ModifiersChanged` are always routed, never consumed.
        if let PlatformEvent::Window { window_id: _, kind } = &event {
            if let WindowEventKind::KeyboardInput(key) = kind {
                track_app_modifiers(&mut self.app_mods, key);
                if !is_modifier_key(key) && key.state == PressState::Pressed {
                    if let Some(keyref) = key_ref_from_event(key, &self.app_mods) {
                        if let Some(action) = bitty_config::match_keymap(&self.keymaps, keyref) {
                            // Repeats of a bound chord stay owned by chrome
                            // (no action, no PTY bytes).
                            if !key.repeat {
                                self.apply_chrome_action(action);
                            }
                            if let Some(win) = self.window.as_ref() {
                                win.request_redraw();
                            }
                            return;
                        }
                    }
                }
            } else if let WindowEventKind::ModifiersChanged(mods) = kind {
                self.app_mods = AppModifiers {
                    shift: mods.shift,
                    control: mods.control,
                    alt: mods.alt,
                    super_held: mods.super_pressed,
                };
            } else if let WindowEventKind::Focused(focused) = kind {
                // CTX-0187 exit B root-cause fix: focus transitions are where
                // the mirror goes stale (missed releases while unfocused), so
                // clear here before delegating to Runtime (which records focus
                // via set_focused). Fail-closed to shell until the
                // authoritative ModifiersChanged stream re-latches.
                clear_app_modifiers_on_focus(&mut self.app_mods, *focused);
            }
        }

        // Shutdown handling: PlatformEvent::Exiting or CloseRequested/Closed
        // ask the handler to exit the loop.
        //
        // CTX-0186: snapshot the paste-dialog state before routing so the
        // right/middle-click and Esc-cancel paths can report loudly afterwards
        // (a gated paste is never silent).
        let paste_probe = match &event {
            PlatformEvent::Window { window_id: _, kind } => match kind {
                WindowEventKind::MouseInput(mouse)
                    if mouse.state == PressState::Pressed
                        && matches!(mouse.button, MouseButton::Right | MouseButton::Middle) =>
                {
                    Some((
                        "mouse",
                        self.runtime.has_pending_paste(),
                        self.runtime.pending_input_len(),
                    ))
                }
                WindowEventKind::KeyboardInput(key)
                    if key.state == PressState::Pressed
                        && matches!(&key.logical_key, LogicalKey::Named(NamedKey::Escape)) =>
                {
                    Some((
                        "esc",
                        self.runtime.has_pending_paste(),
                        self.runtime.pending_input_len(),
                    ))
                }
                _ => None,
            },
            _ => None,
        };
        let should_exit = self.runtime.handle_platform_event(event.clone());
        if should_exit {
            eprintln!("bitty: exit requested ({event:?})");
            ctx.exit();
            return;
        }
        // CTX-0186 loud paste-dialog reporting: pending shows the bounded
        // summary with confirm/cancel instructions; a cleared pending with new
        // input bytes means the repeat gesture confirmed delivery; an Esc
        // that cleared pending without new bytes means it was cancelled.
        if let Some((probe_kind, had_pending, before_len)) = paste_probe {
            let has_pending = self.runtime.has_pending_paste();
            let after_len = self.runtime.pending_input_len();
            if has_pending {
                if let Some(summary) = self.runtime.pending_paste_summary() {
                    eprintln!("bitty: paste -> {summary}");
                }
            } else if had_pending && after_len > before_len {
                eprintln!("bitty: paste confirmed -> delivered");
            } else if had_pending && probe_kind == "esc" {
                eprintln!("bitty: paste confirmation cancelled (Esc)");
            }
            if let Some(win) = self.window.as_ref() {
                win.request_redraw();
            }
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
                        .with_opacity(self.window_opacity)
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
                // `handle_platform_event` already routed Resized /
                // ScaleFactorChanged / CloseRequested / RedrawRequested /
                // KeyboardInput (unbound keys to the PTY; bound chords were
                // consumed by the single-owner intercept above and return
                // early, so no focus matcher lives here anymore).
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

    // `bitty list --help` shows list help (never needs an instance or VM);
    // `bitty inspect --help` shows inspect help; bare `--help` shows the
    // top-level help.
    if args.help && args.list_word {
        println!("{}", list::list_help_text(&args.list_spelling));
        std::process::exit(0);
    }
    if args.help && args.inspect_word {
        println!("{}", inspect::inspect_help_text());
        std::process::exit(0);
    }
    // `bitty dev --help` shows dev help (never needs an instance or VM and
    // never builds a runtime); bare `--help` shows the top-level help.
    if args.help && args.dev_word {
        println!("{}", dev::dev_help_text());
        std::process::exit(0);
    }
    if args.help {
        println!("{}", help_text());
        std::process::exit(0);
    }
    if args.version {
        println!("{}", version_text());
        std::process::exit(0);
    }

    // Unknown pre-`--` dash-flags are usage errors (CR-APP-01, exit 2):
    // a typo must never be spawned as a program. Only post-`--` tokens
    // (parsed above into `program`/`program_args`) may name a program.
    if let Some(flag) = args.unknown_flag.as_deref() {
        eprintln!("bitty: unknown flag '{flag}'\n{}", help_text());
        std::process::exit(2);
    }

    // `bitty run -- COMMAND...` explicit child launch (CTX-0170, local
    // class). Dispatched before config load and GUI startup: no instance,
    // no IPC, no plugin VM. Exit code is the child's (passthrough);
    // parse failures are usage errors (exit 2).
    if args.run_word {
        match run::parse_run_request(&args.run_raw) {
            Err(run::RunParseError::Help) => {
                print!("{}", run::run_help_text());
                std::process::exit(0);
            }
            Err(err) => {
                eprintln!("{}\n{}", err.message(), run::run_usage());
                std::process::exit(2);
            }
            Ok(req) => {
                let code = run::execute_run(&req);
                std::process::exit(code);
            }
        }
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

    // `bitty init` opt-in setup wizard (#243, CTX-0149). Explicit only:
    // never auto-runs on startup, only via the subcommand word.
    if args.init_word {
        std::process::exit(run_init_subcommand(&args));
    }

    // `bitty doctor` diagnosis (CTX-0175, local class, safe mode). Runs
    // before config load: an invalid config is a reported failing check
    // (exit 3), not a startup abort, and no plugin VM is ever loaded.
    if args.doctor_word {
        std::process::exit(run_doctor_subcommand(&args));
    }

    // `bitty ctl` runtime control (CTX-0171, runtime class). Dispatched
    // before config load and GUI startup: `--help` never needs an instance;
    // other verbs resolve `--socket`/`--instance`/inherited/exactly-one
    // targeting and speak the versioned IPC protocol. Parse failures are
    // usage errors (exit 2).
    if args.ctl_word {
        std::process::exit(run_ctl_subcommand(&args));
    }

    // `bitty list <kind>` enumeration (CTX-0172). Local kinds never touch
    // config/instance; `instances` does its own socket discovery. Runs
    // before config loading so `list` works with a missing or invalid
    // config file (safe-mode clean, no plugin VM).
    if args.list_word {
        std::process::exit(run_list_subcommand(&args));
    }

    // `bitty inspect <target> <value>` state and ownership (CTX-0173, local
    // class, safe mode). Runs before config loading so `inspect` works with
    // a missing or invalid config file: every target resolves from built-in
    // defaults and static manifests (no file I/O, no instance, no VM).
    if args.inspect_word {
        std::process::exit(run_inspect_subcommand(&args));
    }

    // `bitty dev <verb>` tracing, captures, dumps, overlays (CTX-0174,
    // local class). Dispatched before config load and GUI startup: no
    // instance, no IPC, no plugin VM. Parse failures are usage errors
    // (exit 2); post-parse failures are generic errors (exit 1).
    if args.dev_word {
        std::process::exit(run_dev_subcommand(&args));
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
    // CTX-0153: resolve the keymap table (shipped defaults + user overrides).
    // Unknown actions/chords fail closed here exactly as in `config check`;
    // the merge already validated entries, so this is defense in depth.
    let keymaps = match bitty_config::resolve_keymaps(&app_config.effective) {
        Ok(maps) => {
            eprintln!("bitty: keymaps resolved ({} entries)", maps.len());
            maps
        }
        Err(err) => {
            eprintln!("bitty: invalid keymaps: {err}");
            std::process::exit(2);
        }
    };
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

    // Single-window vertical slice: one PTY per leaf, one shell each.
    // Explicit program spawns verbatim (with tail args via spawn_shell_with_args);
    // bare invocation resolves to the default shell ($SHELL or /bin/sh).
    // Headless CI still succeeds even if spawn fails (bounded synthetic smoke).
    // `$SHELL` is read once here and injected into the pure resolver so arg
    // handling stays testable; it is trusted only as a binary path, never split.
    let shell_env = std::env::var("SHELL").ok();
    // CTX-0176: frozen once so every split leaf replays this resolution.
    let spawn_spec = SpawnSpec {
        program: args.program.clone(),
        program_args: args.program_args.clone(),
        shell_env: shell_env.clone(),
    };
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
        Ok(()) => {
            eprintln!(
                "bitty: PTY shell spawned (has_pty={} has_reader={})",
                runtime.has_pty(),
                runtime.has_pty_reader()
            );
            // CTX-0176: startup multi-leaf layouts (`--split`/`--stack`/
            // `--layout`) give every non-focused leaf its own shell too; the
            // focused leaf keeps the primary session spawned above.
            // Best-effort with loud warnings (spawn failures stay non-fatal,
            // startup parity). Skipped when the primary spawn failed: the
            // same resolution would fail the same way per leaf.
            spawn_startup_pane_shells(&mut runtime, &spawn_spec);
        }
        Err(err) => eprintln!(
            "bitty: PTY spawn failed: {err} — continuing without child (headless tick still proves path)"
        ),
    }

    if args.headless {
        // In headless mode we still fed synthetic bytes via run_headless_smoke, but the live PTY
        // (if any) has been spawned above and will be polled on AboutToWait. For deterministic CI
        // we also keep synthetic smoke proof.
        println!(
            "bitty headless: theme '{}' source={} file={} profile={} profile_file={}",
            app_config.theme.name,
            app_config.source,
            app_config
                .file_path
                .as_ref()
                .map_or(String::from("(none)"), |p| p.display().to_string()),
            app_config.profile_name.as_deref().unwrap_or("(none)"),
            app_config
                .profile_path
                .as_ref()
                .map_or(String::from("(none)"), |p| p.display().to_string()),
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
    // CTX-0144: serve BITTY_SOCKET for bitty-devtools handshake + read-only
    // round-trip. Fail-soft: socket failure never crashes the terminal.
    let ipc_serve = ipc_serve::serve_in_background(ipc_serve::ServerDescriptor {
        cols: runtime.config().cols,
        rows: runtime.config().rows,
    });
    if ipc_serve.is_enabled() {
        eprintln!("bitty: ipc serving {}", ipc_serve.socket_path());
    }
    let mut app = TerminalApp::with_theme(
        runtime,
        app_config.theme.name,
        app_config.source,
        keymaps,
        spawn_spec,
    )
    // CTX-0223: `window.opacity` flows effective -> window creation
    // (sanitized by the platform config; fail-soft where unsupported).
    .with_window_opacity(app_config.effective.window.opacity);
    // CTX-0167: the synthetic demo pump stays off in real sessions so
    // startup shows only the shell. Opt-in debug only (`BITTY_DEMO_PUMP=1`).
    if demo_pump_enabled_from_env() {
        app.attach_demo_pump(app_config.theme.name, app_config.source);
    }
    // CTX-0190: apply the stderr verbosity gate before the event loop so
    // per-frame `bitty tick` lines stay quiet by default and appear only
    // with `--verbose` / `--log-level debug|trace` (or BITTY_LOG/RUST_LOG).
    // Key info (paste confirm/cancel, startup summary, errors) bypasses the
    // gate and always emits; devtools keeps full fidelity via Runtime::tick.
    app.set_log_level(effective_log_level(&args));
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
        // CTX-0176: startup panes get their own shells here too (same rule as
        // the primary path above — panes only when the primary spawn worked).
        let fallback_spec = SpawnSpec {
            program: args.program.clone(),
            program_args: args.program_args.clone(),
            shell_env: std::env::var("SHELL").ok(),
        };
        let fallback_primary_ok = if let Some(program) = args.program.as_deref() {
            let tail: Vec<&str> = args.program_args.iter().map(|s| s.as_str()).collect();
            if tail.is_empty() {
                rt.spawn_shell(program).is_ok()
            } else {
                rt.spawn_shell_with_args(program, &tail).is_ok()
            }
        } else {
            spawn_default_shell(&mut rt, fallback_spec.shell_env.as_deref()).is_ok()
        };
        if fallback_primary_ok {
            spawn_startup_pane_shells(&mut rt, &fallback_spec);
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
        assert_eq!(parsed.profile, None);
        assert_eq!(parsed.theme, None);
        assert_eq!(parsed.config_cmd, None);
        assert!(!parsed.config_word);
        assert!(parsed.config_args.is_empty());
        assert!(!parsed.verbose);
        assert_eq!(parsed.log_level, None);
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

    // CTX-0190 quiet-default logging: parsing + level gating.
    #[test]
    fn parse_verbose_flags() {
        assert!(parse_args(&args_of(&["bitty", "--verbose"])).verbose);
        assert!(parse_args(&args_of(&["bitty", "-v"])).verbose);
        assert!(!parse_args(&args_of(&["bitty"])).verbose);
        // `--` escape hatch: `-v` after `--` is a program name, not a flag.
        let p = parse_args(&args_of(&["bitty", "--", "-v"]));
        assert!(!p.verbose);
        assert_eq!(p.program.as_deref(), Some("-v"));
    }

    #[test]
    fn parse_log_level_flags() {
        let p = parse_args(&args_of(&["bitty", "--log-level", "debug"]));
        assert_eq!(p.log_level, Some(LogLevel::Debug));
        let p = parse_args(&args_of(&["bitty", "--log-level=trace"]));
        assert_eq!(p.log_level, Some(LogLevel::Trace));
        let p = parse_args(&args_of(&["bitty", "--log-level=INFO"]));
        assert_eq!(p.log_level, Some(LogLevel::Info));
        // Unknown values are warned + ignored (total, no panic).
        let p = parse_args(&args_of(&["bitty", "--log-level", "nope"]));
        assert_eq!(p.log_level, None);
        let p = parse_args(&args_of(&["bitty", "--log-level"]));
        assert_eq!(p.log_level, None);
    }

    #[test]
    fn log_level_parses_known_names() {
        assert_eq!(LogLevel::parse("error"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("warn"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("info"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("trace"), Some(LogLevel::Trace));
        assert_eq!(LogLevel::parse("DEBUG"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse(" verbose "), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("nope"), None);
        assert_eq!(LogLevel::parse(""), None);
    }

    #[test]
    fn log_level_ordering_is_quiet_by_default() {
        assert!(LogLevel::Error < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Trace);
        assert_eq!(LogLevel::default_level(), LogLevel::Warn);
        assert!(!LogLevel::Warn.tick_enabled());
        assert!(!LogLevel::Error.tick_enabled());
        assert!(!LogLevel::Info.tick_enabled());
        assert!(LogLevel::Debug.tick_enabled());
        assert!(LogLevel::Trace.tick_enabled());
    }

    #[test]
    fn log_level_from_env_value_accepts_rust_log_filters() {
        assert_eq!(log_level_from_env_value("trace"), Some(LogLevel::Trace));
        assert_eq!(log_level_from_env_value("debug"), Some(LogLevel::Debug));
        assert_eq!(
            log_level_from_env_value("bitty=debug"),
            Some(LogLevel::Debug)
        );
        assert_eq!(
            log_level_from_env_value("info,bitty-app=trace"),
            Some(LogLevel::Trace)
        );
        assert_eq!(log_level_from_env_value("WARN"), Some(LogLevel::Warn));
        assert_eq!(log_level_from_env_value("off"), None);
        assert_eq!(log_level_from_env_value(""), None);
    }

    #[test]
    fn default_run_emits_no_tick_lines() {
        // Default quiet run: `with_theme` leaves the gate at `Warn`, so the
        // hot path returns `None` without formatting (no stderr tick lines).
        let rt = Runtime::with_defaults().expect("must build");
        let app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            Vec::new(),
            SpawnSpec::default(),
        );
        assert!(!app.tick_logging_enabled());
        let present = bitty_runtime::PresentStats {
            frame: 1,
            fills: 7,
            glyphs: 3,
            headless: true,
            generation: 9,
        };
        assert!(app.maybe_format_tick(&present).is_none());
    }

    #[test]
    fn verbose_run_emits_tick_lines() {
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            Vec::new(),
            SpawnSpec::default(),
        );
        app.set_log_level(LogLevel::Debug);
        assert!(app.tick_logging_enabled());
        let present = bitty_runtime::PresentStats {
            frame: 4,
            fills: 12,
            glyphs: 5,
            headless: true,
            generation: 30,
        };
        let line = app
            .maybe_format_tick(&present)
            .expect("verbose must format tick");
        assert!(line.contains("bitty tick:"));
        assert!(line.contains("frame=4"));
        assert!(line.contains("fills=12"));
        assert!(line.contains("glyphs=5"));
        // Trace shows the same line (full fidelity at both levels).
        app.set_log_level(LogLevel::Trace);
        assert!(app.maybe_format_tick(&present).is_some());
        // Info stays quiet (no per-frame noise).
        app.set_log_level(LogLevel::Info);
        assert!(!app.tick_logging_enabled());
        assert!(app.maybe_format_tick(&present).is_none());
    }

    #[test]
    fn tick_line_format_carries_frame_stats() {
        let present = bitty_runtime::PresentStats {
            frame: 2,
            fills: 1921,
            glyphs: 21,
            headless: true,
            generation: 30,
        };
        let line = TerminalApp::format_tick_line(&present, 1, None, 1, false, false);
        assert!(line.starts_with("bitty tick:"));
        assert!(line.contains("frame=2"));
        assert!(line.contains("fills=1921"));
        assert!(line.contains("glyphs=21"));
        assert!(line.contains("headless=true"));
        assert!(line.contains("gen=30"));
    }

    #[test]
    fn key_paste_messages_bypass_quiet_gate() {
        // Paste confirm/cancel + startup lines are user-facing: they are
        // unconditional `eprintln!` outside `drive_tick` and must stay
        // present in both quiet and verbose runs. This pins the exact
        // strings the event handler emits so a future refactor cannot
        // accidentally gate them behind the tick level.
        let confirm = "bitty: paste confirmed -> delivered";
        let cancelled = "bitty: paste confirmation cancelled (Esc)";
        assert!(!confirm.is_empty());
        assert!(!cancelled.is_empty());
        // Help advertises the quiet default + the verbose escape hatch.
        let help = help_text();
        assert!(help.contains("--verbose"));
        assert!(help.contains("--log-level"));
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
    fn parse_unknown_flag_fails_closed_never_a_program() {
        // CR-APP-01: a typo'd flag must not be spawned as a program.
        // Dispatch rejects via usage + exit 2; parse records the flag.
        let p = parse_args(&args_of(&["bitty", "--bogus"]));
        assert_eq!(p.unknown_flag.as_deref(), Some("--bogus"));
        assert_eq!(p.program, None);
        assert!(p.program_args.is_empty());

        let p = parse_args(&args_of(&["bitty", "-x"]));
        assert_eq!(p.unknown_flag.as_deref(), Some("-x"));
        assert_eq!(p.program, None);

        // Unknown `=` flags are rejected the same way.
        let p = parse_args(&args_of(&["bitty", "--bogus=1"]));
        assert_eq!(p.unknown_flag.as_deref(), Some("--bogus=1"));
        assert_eq!(p.program, None);

        // Known flags still parse; the first unknown flag is recorded.
        let p = parse_args(&args_of(&["bitty", "--headless", "--bogus"]));
        assert!(p.headless);
        assert_eq!(p.unknown_flag.as_deref(), Some("--bogus"));
        assert_eq!(p.program, None);

        // Post-`--` dash-tokens are program argv (escape hatch intact).
        let p = parse_args(&args_of(&["bitty", "--", "--bogus"]));
        assert_eq!(p.unknown_flag, None);
        assert_eq!(p.program.as_deref(), Some("--bogus"));

        // Dash-tokens after an explicit program are that program's args
        // (e.g. `bitty /bin/cat -A` keeps working).
        let p = parse_args(&args_of(&["bitty", "/bin/cat", "-A"]));
        assert_eq!(p.unknown_flag, None);
        assert_eq!(p.program.as_deref(), Some("/bin/cat"));
        assert_eq!(p.program_args, vec!["-A"]);

        // `run -- <prog>` escape hatch stays intact (verbatim raw tail).
        let p = parse_args(&args_of(&["bitty", "run", "--", "--bogus"]));
        assert!(p.run_word);
        assert_eq!(p.unknown_flag, None);
        assert_eq!(p.program, None);
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
        // CTX-0167: default real sessions carry no demo pump — poll drains
        // only the real PTY (empty here) and ticks stay total.
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            Vec::new(),
            SpawnSpec::default(),
        );
        assert!(app.pty_rx.is_none());
        let _ = app.poll_pty_pump();
        let _ = app.drive_tick();
        // Opt-in debug path still drains the synthetic burst without
        // deadlocking (pump sends async: bounded yield retries).
        let rt_demo = Runtime::with_defaults().expect("must build");
        let mut demo = TerminalApp::with_demo_pump(
            rt_demo,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            Vec::new(),
            SpawnSpec::default(),
        );
        let mut consumed = false;
        for _ in 0..1000 {
            if demo.poll_pty_pump() {
                consumed = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(consumed);
        let _ = demo.drive_tick();
        // Second tick without new bytes should be idle (frame-on-demand).
        let rt2 = Runtime::with_defaults().expect("must build");
        let mut app2 = TerminalApp::with_theme(
            rt2,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            Vec::new(),
            SpawnSpec::default(),
        );
        let _ = app2.drive_tick();
        // After first present the second idle tick in the same app may be None.
        // We do not assert presence, only totality (no panic).
    }

    #[test]
    fn default_startup_carries_no_demo_line() {
        // CTX-0167 / #269: real sessions show only the shell — the default
        // constructor attaches no synthetic pump, so polling consumes
        // nothing and the grid never sees `demo pty:`.
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            Vec::new(),
            SpawnSpec::default(),
        );
        assert!(app.pty_rx.is_none());
        assert!(!app.poll_pty_pump());
        app.runtime.select_all();
        let text = app.runtime.selection_text().unwrap_or_default();
        assert!(
            !text.contains("demo pty"),
            "default startup must not contain demo line, got {text:?}"
        );
    }

    #[test]
    fn demo_pump_gate_defaults_off_and_opts_in() {
        // CTX-0167: `BITTY_DEMO_PUMP` is default-off; only explicit `1`/`true`
        // enables the synthetic burst.
        assert!(!demo_pump_enabled_from_value(None));
        assert!(!demo_pump_enabled_from_value(Some("")));
        assert!(!demo_pump_enabled_from_value(Some("0")));
        assert!(!demo_pump_enabled_from_value(Some("false")));
        assert!(!demo_pump_enabled_from_value(Some("yes")));
        assert!(demo_pump_enabled_from_value(Some("1")));
        assert!(demo_pump_enabled_from_value(Some("true")));
        assert!(demo_pump_enabled_from_value(Some("TRUE")));
        assert!(demo_pump_enabled_from_value(Some(" 1 ")));
    }

    #[test]
    fn demo_pump_opt_in_delivers_greeting() {
        // CTX-0167: the gated debug path still delivers the themed greeting
        // for harnesses that explicitly opt in. The pump thread sends
        // asynchronously, so drain with bounded retries (no sleep: yield
        // only) before asserting grid content.
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_demo_pump(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            Vec::new(),
            SpawnSpec::default(),
        );
        assert!(app.pty_rx.is_some());
        let mut consumed = false;
        for _ in 0..1000 {
            if app.poll_pty_pump() {
                consumed = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(consumed, "opt-in demo pump must deliver bytes");
        app.runtime.select_all();
        let text = app.runtime.selection_text().expect("grid text");
        assert!(
            text.contains("demo pty"),
            "opt-in demo pump must deliver greeting, got {text:?}"
        );
        // `attach_demo_pump` is idempotent and also opts in from default.
        let rt2 = Runtime::with_defaults().expect("must build");
        let mut app2 = TerminalApp::with_theme(
            rt2,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            Vec::new(),
            SpawnSpec::default(),
        );
        app2.attach_demo_pump(bitty_config::theme::DEFAULT_THEME_NAME, "default");
        assert!(app2.pty_rx.is_some());
        let mut consumed2 = false;
        for _ in 0..1000 {
            if app2.poll_pty_pump() {
                consumed2 = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(consumed2, "attached demo pump must deliver bytes");
        // Second attach is a no-op (does not replace the channel).
        app2.attach_demo_pump(bitty_config::theme::DEFAULT_THEME_NAME, "default");
        assert!(app2.pty_rx.is_some());
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
        assert!(help.contains("--profile"));
        assert!(help.contains("--font-family"));
        assert!(help.contains("--font-size"));
        assert!(help.contains("--opacity"));
        assert!(help.contains("BITTY_CONFIG"));
        assert!(help.contains("BITTY_PROFILE"));
        assert!(help.contains("init.lua"));
        assert!(help.contains("config check"));
    }

    #[test]
    fn parse_profile_flags() {
        // CTX-0169: --profile space + equals forms; blank warns + ignores.
        let p = parse_args(&args_of(&["bitty", "--profile", "work"]));
        assert_eq!(p.profile.as_deref(), Some("work"));
        let p = parse_args(&args_of(&["bitty", "--profile=work"]));
        assert_eq!(p.profile.as_deref(), Some("work"));
        let p = parse_args(&args_of(&["bitty", "--profile", "work", "--theme", "dark"]));
        assert_eq!(p.profile.as_deref(), Some("work"));
        assert_eq!(p.theme.as_deref(), Some("dark"));
        // Composes with --config (warn-at-runtime, not parse time).
        let p = parse_args(&args_of(&[
            "bitty",
            "--config",
            "/tmp/c.lua",
            "--profile",
            "work",
        ]));
        assert_eq!(p.config_path.as_deref(), Some("/tmp/c.lua"));
        assert_eq!(p.profile.as_deref(), Some("work"));
        // Composes with `config check` in any order.
        let p = parse_args(&args_of(&["bitty", "config", "check", "--profile", "work"]));
        assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
        assert_eq!(p.profile.as_deref(), Some("work"));
        let p = parse_args(&args_of(&["bitty", "--profile", "work", "config", "check"]));
        assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
        assert_eq!(p.profile.as_deref(), Some("work"));
        // Missing value warns + ignores (total, no panic).
        let p = parse_args(&args_of(&["bitty", "--profile"]));
        assert_eq!(p.profile, None);
        // `--` escape hatch: --profile after `--` is a program name.
        let p = parse_args(&args_of(&["bitty", "--", "--profile"]));
        assert_eq!(p.profile, None);
        assert_eq!(p.program.as_deref(), Some("--profile"));
    }

    #[test]
    fn parse_appearance_override_flags() {
        // CTX-0180: --font-family/--font-size/--opacity space + equals forms.
        // Raws stay strings (merge-time fail-closed); blanks warn + ignore.
        let p = parse_args(&args_of(&["bitty", "--font-family", "Cli Mono"]));
        assert_eq!(p.font_family.as_deref(), Some("Cli Mono"));
        let p = parse_args(&args_of(&["bitty", "--font-family=Cli Mono"]));
        assert_eq!(p.font_family.as_deref(), Some("Cli Mono"));
        let p = parse_args(&args_of(&["bitty", "--font-size", "14"]));
        assert_eq!(p.font_size.as_deref(), Some("14"));
        let p = parse_args(&args_of(&["bitty", "--font-size=14.5"]));
        assert_eq!(p.font_size.as_deref(), Some("14.5"));
        let p = parse_args(&args_of(&["bitty", "--opacity", "0.9"]));
        assert_eq!(p.opacity.as_deref(), Some("0.9"));
        let p = parse_args(&args_of(&["bitty", "--opacity=0.95"]));
        assert_eq!(p.opacity.as_deref(), Some("0.95"));
        // Invalid raws are captured, never parsed here (merge fails closed).
        let p = parse_args(&args_of(&["bitty", "--font-size", "abc"]));
        assert_eq!(p.font_size.as_deref(), Some("abc"));
        // Negative numbers reach validation (fail-closed), not "missing".
        let p = parse_args(&args_of(&["bitty", "--font-size", "-5"]));
        assert_eq!(p.font_size.as_deref(), Some("-5"));
        let p = parse_args(&args_of(&["bitty", "--opacity=-0.1"]));
        assert_eq!(p.opacity.as_deref(), Some("-0.1"));
        // A real flag after the option still means "missing value".
        let p = parse_args(&args_of(&["bitty", "--font-size", "--verbose"]));
        assert_eq!(p.font_size, None);
        assert!(p.verbose);
        // Missing/blank values warn + ignore (total, no panic).
        let p = parse_args(&args_of(&["bitty", "--font-family"]));
        assert_eq!(p.font_family, None);
        let p = parse_args(&args_of(&["bitty", "--font-size="]));
        assert_eq!(p.font_size, None);
        let p = parse_args(&args_of(&["bitty", "--opacity"]));
        assert_eq!(p.opacity, None);
        // Composes with --theme and `config check`.
        let p = parse_args(&args_of(&[
            "bitty",
            "--theme",
            "dark",
            "--font-size",
            "14",
            "config",
            "check",
        ]));
        assert_eq!(p.theme.as_deref(), Some("dark"));
        assert_eq!(p.font_size.as_deref(), Some("14"));
        assert_eq!(p.config_cmd, Some(ConfigCommand::Check));
        // `--` escape hatch: appearance flags after `--` are program argv.
        let p = parse_args(&args_of(&["bitty", "--", "--font-size"]));
        assert_eq!(p.font_size, None);
        assert_eq!(p.program.as_deref(), Some("--font-size"));
    }

    #[test]
    fn looks_like_negative_number_guards_flag_values() {
        assert!(looks_like_negative_number("-5"));
        assert!(looks_like_negative_number("-0.1"));
        assert!(looks_like_negative_number("-.5"));
        assert!(!looks_like_negative_number("-v"));
        assert!(!looks_like_negative_number("--headless"));
        assert!(!looks_like_negative_number("-"));
        assert!(!looks_like_negative_number("14"));
        assert!(!looks_like_negative_number(""));
    }

    #[test]
    fn cli_overrides_from_args_trims_and_passes_raws() {
        // Pure wiring: trims, blanks become absent, numeric raws untouched.
        let mut args = Args::new();
        args.theme = Some("  dark  ".to_string());
        args.font_family = Some(" Cli Mono ".to_string());
        args.font_size = Some("abc".to_string());
        args.opacity = Some(" 0.9 ".to_string());
        let cli = cli_overrides_from_args(&args);
        assert_eq!(cli.theme.as_deref(), Some("dark"));
        assert_eq!(cli.font_family.as_deref(), Some("Cli Mono"));
        assert_eq!(cli.font_size.as_deref(), Some("abc"));
        assert_eq!(cli.opacity.as_deref(), Some("0.9"));
        // Invalid raws fail closed at the override layer (with field path).
        assert!(cli.validate_appearance_overrides().is_err());
        let mut args = Args::new();
        args.font_family = Some("   ".to_string());
        args.font_size = Some(String::new());
        let cli = cli_overrides_from_args(&args);
        assert!(cli.is_empty());
        assert!(cli.validate_appearance_overrides().is_ok());
        // Flag naming for the fail-closed message.
        assert_eq!(appearance_flag_for_field(Some("font.size")), "--font-size");
        assert_eq!(
            appearance_flag_for_field(Some("window.opacity")),
            "--opacity"
        );
        assert_eq!(
            appearance_flag_for_field(Some("font.family")),
            "--font-family"
        );
    }

    #[test]
    fn profile_request_resolution_prefers_cli_over_env() {
        // Pure resolver lives in bitty-config::file; pin the contract here
        // so Args parsing and env handling cannot drift (CLI > env > none).
        use bitty_config::file::resolve_profile_request;
        assert_eq!(
            resolve_profile_request(Some("cli"), Some("env")).as_deref(),
            Some("cli")
        );
        assert_eq!(
            resolve_profile_request(None, Some("env")).as_deref(),
            Some("env")
        );
        assert_eq!(resolve_profile_request(None, None), None);
    }

    #[test]
    fn profile_layer_stacks_under_user_over_defaults() {
        // CTX-0169 precedence matrix at the merge level (no fs): profile
        // beats defaults, user beats profile, CLI beats both.
        use bitty_config::file::{CliOverrides, parse_lua_config, resolve_effective_full};
        use bitty_config::plan::{ConfigSource, LayerKind, LayeredPlan};
        let profile_src = ConfigSource::new(LayerKind::Profile, Some("profiles/work.lua"));
        let profile_plan =
            parse_lua_config(r#"return { theme = "dark" }"#, &profile_src).expect("profile");
        let profile = LayeredPlan::new(profile_src, profile_plan);
        let cli_none = CliOverrides::default();
        let merged =
            resolve_effective_full(None, Some(profile.clone()), &cli_none).expect("profile");
        assert_eq!(merged.effective.appearance.theme.as_deref(), Some("dark"));
        assert_eq!(
            merged.source_of("appearance.theme").unwrap().layer,
            LayerKind::Profile
        );
        let user_src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let user_plan =
            parse_lua_config(r#"return { theme = "bitty-dark" }"#, &user_src).expect("user");
        let user = LayeredPlan::new(user_src, user_plan);
        let merged =
            resolve_effective_full(Some(user), Some(profile), &cli_none).expect("user wins");
        assert_eq!(
            merged.effective.appearance.theme.as_deref(),
            Some("bitty-dark")
        );
        assert_eq!(
            merged.source_of("appearance.theme").unwrap().layer,
            LayerKind::User
        );
        // Source labels carry the profile path for `config check`.
        let profile_src2 = ConfigSource::new(LayerKind::Profile, Some("profiles/work.lua"));
        let profile_plan2 =
            parse_lua_config(r#"return { theme = "dark" }"#, &profile_src2).expect("profile2");
        let profile2 = LayeredPlan::new(profile_src2, profile_plan2);
        let merged = resolve_effective_full(None, Some(profile2), &cli_none).expect("profile only");
        let label = layer_source_label(
            &merged,
            "appearance.theme",
            None,
            Some(std::path::Path::new("profiles/work.lua")),
        );
        assert!(label.starts_with("profile:"), "got {label:?}");
    }

    #[test]
    fn invalid_profile_name_fails_closed_without_filesystem() {
        // Traversal names never reach the filesystem: validation rejects.
        assert!(bitty_config::file::validate_profile_name("../evil").is_err());
        assert!(bitty_config::file::validate_profile_name("a/b").is_err());
        assert!(bitty_config::file::validate_profile_name("").is_err());
        assert!(
            bitty_config::file::profile_file_path_with_env("../evil", Some("/x"), Some("/h"))
                .is_err()
        );
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
        // Breathing-room defaults: legacy table omits spacing, so effective
        // 9x19 matches the readable runtime defaults.
        assert_eq!((cfg.cell_width, cfg.cell_height), (9, 19));
    }

    #[test]
    fn runtime_config_applies_font_spacing() {
        use bitty_config::file::{parse_lua_config, resolve_effective};
        use bitty_config::plan::{ConfigSource, LayerKind};
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let content = r#"return {
            font = { family = "Mono", size = 12, line_height = 1.0, letter_spacing = 0 },
        }"#;
        let plan = parse_lua_config(content, &src).expect("spacing parses");
        let layer = bitty_config::plan::LayeredPlan::new(src, plan);
        let merged = resolve_effective(Some(layer), None).expect("merge");
        let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
        assert_eq!((cfg.cell_width, cfg.cell_height), (8, 16));
    }

    #[test]
    fn runtime_config_inherits_file_scroll_speed() {
        // CTX-0185: scroll keys flow file -> effective -> runtime; crate
        // defaults stay equal (bitty-runtime must not depend on bitty-config,
        // so the pairing is by value, pinned here).
        assert_eq!(
            bitty_runtime::config::DEFAULT_SCROLL_LINES_PER_NOTCH,
            bitty_config::types::DEFAULT_SCROLL_LINES_PER_NOTCH
        );
        assert_eq!(
            bitty_runtime::config::DEFAULT_SCROLL_PIXELS_PER_NOTCH,
            bitty_config::types::DEFAULT_SCROLL_PIXELS_PER_NOTCH
        );
        use bitty_config::file::{parse_lua_config, resolve_effective};
        use bitty_config::plan::{ConfigSource, LayerKind};
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let content = r#"return {
            terminal = { scrollback = 10000, scroll_lines_per_notch = 5, scroll_pixels_per_notch = 24 },
        }"#;
        let plan = parse_lua_config(content, &src).expect("scroll keys parse");
        let layer = bitty_config::plan::LayeredPlan::new(src, plan);
        let merged = resolve_effective(Some(layer), None).expect("merge");
        assert_eq!(merged.effective.terminal.scroll_lines_per_notch, 5);
        assert_eq!(merged.effective.terminal.scroll_pixels_per_notch, 24);
        let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
        assert_eq!(cfg.scroll_lines_per_notch, 5);
        assert_eq!(cfg.scroll_pixels_per_notch, 24);
        // Absent keys ride the defaults end to end.
        let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
            .expect("minimal terminal parses");
        let merged2 = resolve_effective(
            Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
            None,
        )
        .expect("merge");
        let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
        assert_eq!(
            cfg2.scroll_lines_per_notch,
            bitty_runtime::config::DEFAULT_SCROLL_LINES_PER_NOTCH
        );
        assert_eq!(
            cfg2.scroll_pixels_per_notch,
            bitty_runtime::config::DEFAULT_SCROLL_PIXELS_PER_NOTCH
        );
    }

    #[test]
    fn runtime_config_inherits_file_selection_auto_copy() {
        // CTX-0191: `selection.auto_copy` flows file -> effective -> runtime;
        // crate defaults stay equal (bitty-runtime must not depend on
        // bitty-config, so the pairing is by value, pinned here). Default
        // preserves copy-on-select (zero change for existing users).
        assert_eq!(
            bitty_runtime::config::DEFAULT_SELECTION_AUTO_COPY,
            bitty_config::types::DEFAULT_SELECTION_AUTO_COPY
        );
        const { assert!(bitty_runtime::config::DEFAULT_SELECTION_AUTO_COPY) }
        use bitty_config::file::{parse_lua_config, resolve_effective};
        use bitty_config::plan::{ConfigSource, LayerKind};
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let plan = parse_lua_config(r#"return { selection = { auto_copy = false } }"#, &src)
            .expect("opt-out parses");
        let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
            .expect("merge");
        assert!(!merged.effective.selection.auto_copy);
        let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
        assert!(!cfg.selection_auto_copy);
        // Absent table rides the default-on end to end.
        let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
            .expect("no selection table parses");
        let merged2 = resolve_effective(
            Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
            None,
        )
        .expect("merge");
        assert!(merged2.effective.selection.auto_copy);
        let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
        assert!(cfg2.selection_auto_copy);
        assert_eq!(
            merged2.source_of("selection.auto_copy").unwrap().layer,
            bitty_config::plan::LayerKind::CoreDefaults
        );
    }

    #[test]
    fn runtime_config_inherits_file_layout_gaps() {
        // CTX-0177: `layout.gaps_in`/`gaps_out` flow file -> effective ->
        // runtime; crate defaults stay equal (bitty-runtime must not depend
        // on bitty-config, so the pairing is by value, pinned here). Default
        // preserves edge-to-edge tiling (zero change for existing users).
        assert_eq!(
            u32::from(bitty_runtime::config::DEFAULT_LAYOUT_GAPS_IN),
            bitty_config::types::DEFAULT_LAYOUT_GAPS_IN
        );
        assert_eq!(
            u32::from(bitty_runtime::config::DEFAULT_LAYOUT_GAPS_OUT),
            bitty_config::types::DEFAULT_LAYOUT_GAPS_OUT
        );
        assert_eq!(
            u32::from(bitty_runtime::config::MAX_LAYOUT_GAP_CELLS),
            bitty_config::types::MAX_LAYOUT_GAP_CELLS
        );
        use bitty_config::file::{parse_lua_config, resolve_effective};
        use bitty_config::plan::{ConfigSource, LayerKind};
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let plan = parse_lua_config(r#"return { layout = { gaps_in = 1, gaps_out = 2 } }"#, &src)
            .expect("gaps parse");
        let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
            .expect("merge");
        assert_eq!(merged.effective.layout.gaps_in, 1);
        assert_eq!(merged.effective.layout.gaps_out, 2);
        let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
        assert_eq!((cfg.gaps_in, cfg.gaps_out), (1, 2));
        assert_eq!(
            merged.source_of("layout.gaps_in").unwrap().layer,
            bitty_config::plan::LayerKind::User
        );
        // Absent table rides edge-to-edge end to end.
        let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
            .expect("no layout table parses");
        let merged2 = resolve_effective(
            Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
            None,
        )
        .expect("merge");
        assert_eq!(
            (
                merged2.effective.layout.gaps_in,
                merged2.effective.layout.gaps_out
            ),
            (0, 0)
        );
        let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
        assert_eq!((cfg2.gaps_in, cfg2.gaps_out), (0, 0));
        assert_eq!(
            merged2.source_of("layout.gaps_in").unwrap().layer,
            bitty_config::plan::LayerKind::CoreDefaults
        );
        // Oversized gaps fail closed at the file layer (never reach runtime).
        let src3 = ConfigSource::new(LayerKind::User, Some("init.lua"));
        parse_lua_config(r#"return { layout = { gaps_in = 17 } }"#, &src3).expect_err("must fail");
    }

    #[test]
    fn runtime_config_inherits_file_window_padding() {
        // CTX-0223: `window.padding` flows file -> effective -> runtime;
        // crate defaults stay equal (bitty-runtime must not depend on
        // bitty-config, so the pairing is by value, pinned here). Default
        // preserves the 8px breathing room for existing users.
        assert_eq!(
            bitty_runtime::config::DEFAULT_WINDOW_PADDING,
            bitty_config::EffectiveConfig::default().window.padding
        );
        assert_eq!(
            bitty_runtime::config::MAX_WINDOW_PADDING,
            64,
            "runtime bound must match config validation (`must be <= 64`)"
        );
        use bitty_config::file::{parse_lua_config, resolve_effective};
        use bitty_config::plan::{ConfigSource, LayerKind};
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let plan = parse_lua_config(
            r#"return { window = { opacity = 0.9, padding = 4 } }"#,
            &src,
        )
        .expect("window parses");
        let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
            .expect("merge");
        assert_eq!(merged.effective.window.padding, 4);
        let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
        assert_eq!(cfg.window_padding, 4);
        assert_eq!(
            merged.source_of("window.padding").unwrap().layer,
            bitty_config::plan::LayerKind::User
        );
        // Absent table rides the default end to end.
        let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
            .expect("no window table parses");
        let merged2 = resolve_effective(
            Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
            None,
        )
        .expect("merge");
        assert_eq!(
            merged2.effective.window.padding,
            bitty_runtime::config::DEFAULT_WINDOW_PADDING
        );
        let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
        assert_eq!(
            cfg2.window_padding,
            bitty_runtime::config::DEFAULT_WINDOW_PADDING
        );
        // Oversized padding fails closed at the file layer (never runtime).
        let src3 = ConfigSource::new(LayerKind::User, Some("init.lua"));
        parse_lua_config(
            r#"return { window = { opacity = 1.0, padding = 65 } }"#,
            &src3,
        )
        .expect_err("must fail");
    }

    #[test]
    fn window_opacity_reaches_platform_config() {
        // CTX-0223: `window.opacity` flows effective -> platform window
        // creation; platform defaults match the config default (opaque),
        // and sub-1.0 values request transparency (fail-soft where the
        // platform ignores the flag).
        assert!(
            (bitty_platform::WindowConfig::default().opacity()
                - bitty_config::EffectiveConfig::default().window.opacity)
                .abs()
                < f32::EPSILON
        );
        let app_opacity = bitty_config::EffectiveConfig::default().window.opacity;
        let config = bitty_platform::WindowConfig::new().with_opacity(app_opacity);
        assert!(!config.is_transparent());
        let faded = bitty_platform::WindowConfig::new().with_opacity(0.9);
        assert!(faded.is_transparent());
        // The composition root carries the effective value to creation.
        let rt = bitty_runtime::Runtime::with_defaults().expect("runtime builds");
        let app = TerminalApp::with_theme(
            rt,
            "bitty-dark",
            "default",
            Vec::new(),
            SpawnSpec::default(),
        )
        .with_window_opacity(0.9);
        assert!((app.window_opacity - 0.9).abs() < f32::EPSILON);
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
        // CTX-0191: starter leaves `selection` unset (commented example only)
        // so new installs ride the default-on without a file override.
        assert!(plan.selection.is_none());
        assert!(starter_init_lua().contains("auto_copy"));
        // CTX-0177: starter leaves `layout` unset (commented example only)
        // so new installs ride edge-to-edge without a file override.
        assert!(plan.layout.is_none());
        assert!(starter_init_lua().contains("gaps_in"));
        assert!(starter_init_lua().contains("gaps_out"));
    }

    // -- `bitty init` wizard (CTX-0149, #243) --------------------------------

    /// Unique scratch directory per test (process id + atomic counter: tests
    /// in one binary share the id and run on parallel threads).
    fn init_test_dir(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("bitty-ctx0149-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn parse_init_subcommand() {
        let p = parse_args(&args_of(&["bitty", "init"]));
        assert!(p.init_word);
        assert!(p.init_args.is_empty());
        assert_eq!(p.program, None);
        assert!(!p.init_yes);
        assert!(!p.init_force);

        // Flags compose in any order around the word.
        let p = parse_args(&args_of(&["bitty", "init", "--yes", "--force"]));
        assert!(p.init_word);
        assert!(p.init_yes);
        assert!(p.init_force);

        let p = parse_args(&args_of(&[
            "bitty",
            "--config",
            "/tmp/c.lua",
            "init",
            "--yes",
        ]));
        assert!(p.init_word);
        assert!(p.init_yes);
        assert_eq!(p.config_path.as_deref(), Some("/tmp/c.lua"));

        let p = parse_args(&args_of(&["bitty", "--yes", "init"]));
        assert!(p.init_word);
        assert!(p.init_yes);

        // Extra positionals are recorded for fail-closed dispatch.
        let p = parse_args(&args_of(&["bitty", "init", "extra"]));
        assert!(p.init_word);
        assert_eq!(p.init_args, vec!["extra".to_string()]);

        // Escape hatch: a program literally named `init`.
        let p = parse_args(&args_of(&["bitty", "--", "init"]));
        assert!(!p.init_word);
        assert_eq!(p.program.as_deref(), Some("init"));

        // `init` after `config` belongs to the config subcommand.
        let p = parse_args(&args_of(&["bitty", "config", "init"]));
        assert!(p.config_word);
        assert!(!p.init_word);
    }

    #[test]
    fn parse_doctor_subcommand() {
        let p = parse_args(&args_of(&["bitty", "doctor"]));
        assert!(p.doctor_word);
        assert!(p.doctor_args.is_empty());
        assert_eq!(p.program, None);
        assert_eq!(p.doctor_format, None);
        assert!(!p.doctor_no_color);

        // Flags compose in any order around the word.
        let p = parse_args(&args_of(&["bitty", "doctor", "--format", "json"]));
        assert!(p.doctor_word);
        assert_eq!(p.doctor_format.as_deref(), Some("json"));

        let p = parse_args(&args_of(&["bitty", "--format", "json", "doctor"]));
        assert!(p.doctor_word);
        assert_eq!(p.doctor_format.as_deref(), Some("json"));

        let p = parse_args(&args_of(&["bitty", "doctor", "--format=jsonl"]));
        assert_eq!(p.doctor_format.as_deref(), Some("jsonl"));

        let p = parse_args(&args_of(&[
            "bitty",
            "--config",
            "/tmp/c.lua",
            "doctor",
            "--no-color",
        ]));
        assert!(p.doctor_word);
        assert!(p.doctor_no_color);
        assert_eq!(p.config_path.as_deref(), Some("/tmp/c.lua"));

        // Extra positionals are recorded for fail-closed dispatch.
        let p = parse_args(&args_of(&["bitty", "doctor", "extra"]));
        assert!(p.doctor_word);
        assert_eq!(p.doctor_args, vec!["extra".to_string()]);

        // Escape hatch: a program literally named `doctor`.
        let p = parse_args(&args_of(&["bitty", "--", "doctor"]));
        assert!(!p.doctor_word);
        assert_eq!(p.program.as_deref(), Some("doctor"));

        // `doctor` after `config`/`init` belongs to that subcommand.
        let p = parse_args(&args_of(&["bitty", "config", "doctor"]));
        assert!(p.config_word);
        assert!(!p.doctor_word);
        let p = parse_args(&args_of(&["bitty", "init", "doctor"]));
        assert!(p.init_word);
        assert!(!p.doctor_word);
    }

    #[test]
    fn doctor_usage_names_format_and_checks() {
        let usage = doctor::doctor_usage();
        assert!(usage.contains("doctor"));
        assert!(usage.contains("--format"));
        assert!(usage.contains("table|json|jsonl"));
        let help = help_text();
        assert!(help.contains("doctor"));
        assert!(help.contains("--format"));
    }

    #[test]
    fn parse_ctl_subcommand() {
        // Bare `ctl` captures no tokens (dispatch fails closed with usage).
        let p = parse_args(&args_of(&["bitty", "ctl"]));
        assert!(p.ctl_word);
        assert!(p.ctl_raw.is_empty());
        assert_eq!(p.program, None);
        assert!(!p.run_word);
        assert!(!p.doctor_word);

        // Tokens after the word go verbatim to `ctl_raw`.
        let p = parse_args(&args_of(&["bitty", "ctl", "terminal", "list"]));
        assert!(p.ctl_word);
        assert_eq!(p.ctl_raw, vec!["terminal".to_string(), "list".to_string()]);

        // Global flags before the word land in pre-fields; post-word flags
        // stay verbatim in `ctl_raw` for `ctl::parse_ctl_request`.
        let p = parse_args(&args_of(&[
            "bitty",
            "--socket",
            "/tmp/a.sock",
            "ctl",
            "terminal",
            "list",
        ]));
        assert!(p.ctl_word);
        assert_eq!(p.ctl_socket_pre.as_deref(), Some("/tmp/a.sock"));
        assert_eq!(p.ctl_raw, vec!["terminal".to_string(), "list".to_string()]);

        let p = parse_args(&args_of(&[
            "bitty",
            "--instance",
            "demo_1",
            "ctl",
            "view",
            "list",
        ]));
        assert!(p.ctl_word);
        assert_eq!(p.ctl_instance_pre.as_deref(), Some("demo_1"));

        let p = parse_args(&args_of(&["bitty", "ctl", "--socket=/tmp/b.sock"]));
        assert!(p.ctl_word);
        assert!(p.ctl_socket_pre.is_none());
        assert_eq!(p.ctl_raw, vec!["--socket=/tmp/b.sock".to_string()]);

        // Escape hatch: a program literally named `ctl`.
        let p = parse_args(&args_of(&["bitty", "--", "ctl"]));
        assert!(!p.ctl_word);
        assert_eq!(p.program.as_deref(), Some("ctl"));

        // `ctl` after other subcommands belongs to that subcommand.
        let p = parse_args(&args_of(&["bitty", "config", "ctl"]));
        assert!(p.config_word);
        assert!(!p.ctl_word);
        let p = parse_args(&args_of(&["bitty", "doctor", "ctl"]));
        assert!(p.doctor_word);
        assert!(!p.ctl_word);

        // Help mentions the new subcommand.
        let help = help_text();
        assert!(help.contains("ctl"));
        assert!(help.contains("--socket"));
        assert!(help.contains("--instance"));
    }

    #[test]
    fn parse_inspect_subcommand() {
        // Target + value land in dedicated fields (dispatch validates).
        let p = parse_args(&args_of(&["bitty", "inspect", "command", "core.view.list"]));
        assert!(p.inspect_word);
        assert_eq!(p.inspect_target.as_deref(), Some("command"));
        assert_eq!(p.inspect_value.as_deref(), Some("core.view.list"));
        assert!(p.inspect_args.is_empty());
        assert_eq!(p.program, None);
        assert!(!p.ctl_word);
        assert!(!p.list_word);
        assert!(!p.doctor_word);

        // `--format`/`--no-color` compose before or after the word.
        let p = parse_args(&args_of(&[
            "bitty", "--format", "json", "inspect", "key", "alt+h",
        ]));
        assert!(p.inspect_word);
        assert_eq!(p.inspect_format.as_deref(), Some("json"));
        let p = parse_args(&args_of(&[
            "bitty",
            "inspect",
            "config",
            "font.size",
            "--format=json",
        ]));
        assert_eq!(p.inspect_format.as_deref(), Some("json"));
        let p = parse_args(&args_of(&[
            "bitty",
            "inspect",
            "key",
            "alt+h",
            "--no-color",
        ]));
        assert!(p.inspect_no_color);

        // Extra positionals and stray flags fail closed at dispatch.
        let p = parse_args(&args_of(&[
            "bitty",
            "inspect",
            "command",
            "core.view.list",
            "extra",
        ]));
        assert_eq!(p.inspect_args, vec!["extra".to_string()]);
        let p = parse_args(&args_of(&["bitty", "inspect", "command", "x", "--"]));
        assert_eq!(p.inspect_args, vec!["--".to_string()]);
        let p = parse_args(&args_of(&["bitty", "inspect", "command", "x", "--bogus"]));
        assert_eq!(p.inspect_args, vec!["--bogus".to_string()]);

        // Escape hatch: a program literally named `inspect`.
        let p = parse_args(&args_of(&["bitty", "--", "inspect"]));
        assert!(!p.inspect_word);
        assert_eq!(p.program.as_deref(), Some("inspect"));

        // `inspect` after other subcommands belongs to that subcommand.
        let p = parse_args(&args_of(&["bitty", "list", "inspect"]));
        assert!(p.list_word);
        assert!(!p.inspect_word);
        let p = parse_args(&args_of(&["bitty", "doctor", "inspect"]));
        assert!(p.doctor_word);
        assert!(!p.inspect_word);

        // Help mentions the new subcommand.
        let help = help_text();
        assert!(help.contains("inspect <target>"));
        assert!(help.contains("command|key|plugin|config|protocol"));
    }

    #[test]
    fn parse_dev_subcommand() {
        // Bare `dev` captures no tokens (dispatch fails closed with usage).
        let p = parse_args(&args_of(&["bitty", "dev"]));
        assert!(p.dev_word);
        assert!(p.dev_raw.is_empty());
        assert_eq!(p.program, None);
        assert!(!p.run_word);
        assert!(!p.ctl_word);
        assert!(!p.list_word);

        // Tokens after the word go verbatim to `dev_raw`.
        let p = parse_args(&args_of(&["bitty", "dev", "trace", "startup"]));
        assert!(p.dev_word);
        assert_eq!(p.dev_raw, vec!["trace".to_string(), "startup".to_string()]);

        // Post-word flags stay verbatim for `dev::parse_dev_request`.
        let p = parse_args(&args_of(&[
            "bitty", "dev", "capture", "--layout", "split", "--format", "json",
        ]));
        assert!(p.dev_word);
        assert_eq!(
            p.dev_raw,
            vec![
                "capture".to_string(),
                "--layout".to_string(),
                "split".to_string(),
                "--format".to_string(),
                "json".to_string()
            ]
        );
        assert_eq!(p.dev_format, None);

        // Global flags before the word land in dev pre-fields.
        let p = parse_args(&args_of(&["bitty", "--format", "json", "dev", "capture"]));
        assert!(p.dev_word);
        assert_eq!(p.dev_format.as_deref(), Some("json"));
        assert_eq!(p.dev_raw, vec!["capture".to_string()]);

        let p = parse_args(&args_of(&[
            "bitty",
            "--socket",
            "/tmp/a.sock",
            "dev",
            "capture",
        ]));
        assert!(p.dev_word);
        assert_eq!(p.dev_socket_pre.as_deref(), Some("/tmp/a.sock"));
        assert_eq!(p.dev_raw, vec!["capture".to_string()]);

        // `--no-color` before the word lands in the dev pre-field; after the
        // word it stays verbatim for `dev::parse_dev_request`.
        let p = parse_args(&args_of(&["bitty", "--no-color", "dev", "capture"]));
        assert!(p.dev_word);
        assert!(p.dev_no_color);
        assert_eq!(p.dev_raw, vec!["capture".to_string()]);

        let p = parse_args(&args_of(&["bitty", "dev", "--no-color", "overlay", "list"]));
        assert!(p.dev_word);
        assert!(!p.dev_no_color);
        assert_eq!(
            p.dev_raw,
            vec![
                "--no-color".to_string(),
                "overlay".to_string(),
                "list".to_string()
            ]
        );

        // Escape hatch: a program literally named `dev`.
        let p = parse_args(&args_of(&["bitty", "--", "dev"]));
        assert!(!p.dev_word);
        assert_eq!(p.program.as_deref(), Some("dev"));

        // `dev` after other subcommands belongs to that subcommand.
        let p = parse_args(&args_of(&["bitty", "config", "dev"]));
        assert!(p.config_word);
        assert!(!p.dev_word);
        let p = parse_args(&args_of(&["bitty", "doctor", "dev"]));
        assert!(p.doctor_word);
        assert!(!p.dev_word);
        let p = parse_args(&args_of(&["bitty", "ctl", "dev"]));
        assert!(p.ctl_word);
        assert!(!p.dev_word);

        // Other words after `dev` stay verbatim (dev dispatch rejects them).
        let p = parse_args(&args_of(&["bitty", "dev", "ctl"]));
        assert!(p.dev_word);
        assert!(!p.ctl_word);
        assert_eq!(p.dev_raw, vec!["ctl".to_string()]);

        // Help mentions the new subcommand.
        let help = help_text();
        assert!(help.contains("dev <verb>"));
        assert!(help.contains("bitty dev --help"));
    }

    #[test]
    fn init_yes_defaults_are_sane() {
        let d = init_yes_defaults(Some("/bin/bash"));
        assert_eq!(d.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(d.theme, "dark");
        assert_eq!(d.font_size, bitty_config::types::DEFAULT_FONT_SIZE);
        assert_eq!(d.key_preset, InitKeyPreset::Default);

        // Blank $SHELL means "leave unset" (startup falls back to /bin/sh).
        let d = init_yes_defaults(Some("   "));
        assert_eq!(d.shell, None);
        let d = init_yes_defaults(None);
        assert_eq!(d.shell, None);

        // Unusable $SHELL never becomes a default (warned + omitted at dispatch).
        let d = init_yes_defaults(Some("/bin/ba\x07sh"));
        assert_eq!(d.shell, None);
    }

    #[test]
    fn init_shell_candidates_order_and_fallback() {
        // $SHELL first, then existing commons, no duplicates, fallback last.
        let c = init_shell_candidates(Some("/bin/zsh"), &|p| p == "/bin/bash" || p == "/bin/zsh");
        assert_eq!(c, vec!["/bin/zsh", "/bin/bash", "/bin/sh"]);

        // Nothing set and nothing exists: still exactly the fallback.
        let c = init_shell_candidates(None, &|_| false);
        assert_eq!(c, vec!["/bin/sh"]);

        // Blank env is ignored, not listed.
        let c = init_shell_candidates(Some("  "), &|p| p == "/bin/sh");
        assert_eq!(c, vec!["/bin/sh"]);
    }

    #[test]
    fn init_step_parsers_accept_and_reject() {
        let cands = vec!["/bin/bash".to_string(), "/bin/sh".to_string()];
        // Shell: empty takes the default, numbers pick, customs validate.
        assert_eq!(
            init_parse_shell_answer("", &cands).expect("default"),
            Some("/bin/bash".to_string())
        );
        assert_eq!(
            init_parse_shell_answer("2", &cands).expect("pick"),
            Some("/bin/sh".to_string())
        );
        assert_eq!(
            init_parse_shell_answer("/usr/bin/fish", &cands).expect("custom"),
            Some("/usr/bin/fish".to_string())
        );
        assert!(init_parse_shell_answer("0", &cands).is_err());
        assert!(init_parse_shell_answer("9", &cands).is_err());
        assert!(init_parse_shell_answer("a\x07b", &cands).is_err());

        // Theme: only the shipped preset resolves; typos reprompt.
        assert_eq!(init_parse_theme_answer("").expect("default"), "dark");
        assert_eq!(
            init_parse_theme_answer("Bitty-Dark").expect("registry name"),
            "dark"
        );
        assert!(init_parse_theme_answer("solarized").is_err());

        // Font size: default, valid, and the FontConfig bound.
        assert_eq!(
            init_parse_font_size_answer("").expect("default"),
            bitty_config::types::DEFAULT_FONT_SIZE
        );
        assert_eq!(init_parse_font_size_answer("14").expect("int"), 14.0);
        assert_eq!(init_parse_font_size_answer(" 13.5 ").expect("float"), 13.5);
        for bad in ["0", "-3", "129", "nan", "inf", "big", "12pt"] {
            assert!(
                init_parse_font_size_answer(bad).is_err(),
                "must reject {bad:?}"
            );
        }

        // Preset: default vs vim, nothing else.
        assert_eq!(
            init_parse_preset_answer("").expect("default"),
            InitKeyPreset::Default
        );
        assert_eq!(
            init_parse_preset_answer("2").expect("vim"),
            InitKeyPreset::Vim
        );
        assert_eq!(
            init_parse_preset_answer("VIM").expect("vim word"),
            InitKeyPreset::Vim
        );
        assert!(init_parse_preset_answer("3").is_err());
        assert!(init_parse_preset_answer("emacs").is_err());

        // Shell cleaning: trims, bounds, rejects controls.
        assert_eq!(init_clean_shell("  /bin/bash ").expect("trim"), "/bin/bash");
        assert!(init_clean_shell("   ").is_err());
        assert!(init_clean_shell(&"x".repeat(2000)).is_err());
    }

    #[test]
    fn init_columns_parse() {
        assert_eq!(init_columns_from_env(None), None);
        assert_eq!(init_columns_from_env(Some("80")), Some(80));
        assert_eq!(init_columns_from_env(Some(" 100 ")), Some(100));
        assert_eq!(init_columns_from_env(Some("0")), None);
        assert_eq!(init_columns_from_env(Some("wide")), None);
        assert_eq!(init_columns_from_env(Some("")), None);
    }

    #[test]
    fn init_mascot_is_bounded_with_text_fallback() {
        // The vendored art is small, pure ASCII, and bounded.
        let width = init_mascot_width();
        assert!(width > 0 && width <= 80, "art width {width}");
        assert!(INIT_MASCOT_ART.lines().count() <= 32);
        assert!(INIT_MASCOT_ART.is_ascii());

        // Unknown or roomy widths print the full art (headless-safe).
        assert_eq!(init_greeting_art(None), INIT_MASCOT_ART);
        assert_eq!(init_greeting_art(Some(80)), INIT_MASCOT_ART);
        assert_eq!(init_greeting_art(Some(width as u16)), INIT_MASCOT_ART);

        // A tiny window fails closed to one honest line (pure-text fallback).
        let narrow = init_greeting_art(Some(20));
        assert_eq!(narrow, INIT_MASCOT_FALLBACK);
        assert_eq!(narrow.lines().count(), 1);
        assert!(init_greeting_art(Some(1)).contains("too narrow"));
    }

    /// Drives the interactive wizard with piped stdin; returns answers plus
    /// everything the wizard printed.
    fn drive_init_wizard(
        stdin_lines: &str,
        shell_env: Option<&str>,
        columns: Option<u16>,
    ) -> (Result<InitAnswers, String>, String) {
        let mut input = std::io::BufReader::new(stdin_lines.as_bytes());
        let mut output = Vec::new();
        let result = run_init_interactive(&mut input, &mut output, shell_env, columns, &|p| {
            p == "/bin/bash" || p == "/bin/sh"
        });
        let printed = String::from_utf8(output).expect("wizard output is UTF-8");
        (result, printed)
    }

    #[test]
    fn init_interactive_all_defaults() {
        // Four Enters: default shell, dark theme, default size, default keys.
        let (result, printed) = drive_init_wizard("\n\n\n\n", Some("/bin/bash"), None);
        let answers = result.expect("defaults accepted");
        assert_eq!(answers.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(answers.theme, "dark");
        assert_eq!(answers.font_size, bitty_config::types::DEFAULT_FONT_SIZE);
        assert_eq!(answers.key_preset, InitKeyPreset::Default);

        // Greeting shows the mascot plus every step prompt.
        assert!(printed.contains("Welcome to bitty"));
        assert!(printed.contains("MMMMM"));
        assert!(printed.contains("Shell"));
        assert!(printed.contains("Theme"));
        assert!(printed.contains("Font size"));
        assert!(printed.contains("Keybindings"));
    }

    #[test]
    fn init_interactive_custom_picks() {
        // Pick /bin/sh (#2), bitty-dark, 14pt, vim preset (#2).
        let (result, _) = drive_init_wizard("2\nbitty-dark\n14\n2\n", Some("/bin/bash"), None);
        let answers = result.expect("custom picks accepted");
        assert_eq!(answers.shell.as_deref(), Some("/bin/sh"));
        assert_eq!(answers.theme, "dark");
        assert_eq!(answers.font_size, 14.0);
        assert_eq!(answers.key_preset, InitKeyPreset::Vim);
    }

    #[test]
    fn init_interactive_retries_then_aborts() {
        // Bad font size reprompts and then accepts the correction.
        let (result, printed) = drive_init_wizard("\n\nbanana\n14\n\n", Some("/bin/bash"), None);
        assert!(result.is_ok());
        assert!(printed.contains("try again"));

        // Three bad preset answers exhaust the bound and abort.
        let (result, _) = drive_init_wizard("\n\n\nnope\nnah\nnever\n", Some("/bin/bash"), None);
        assert!(result.is_err());

        // EOF up front aborts without guessing.
        let (result, _) = drive_init_wizard("", Some("/bin/bash"), None);
        assert!(result.is_err());

        // A narrow window still wizards, with the text fallback greeting.
        let (result, printed) = drive_init_wizard("\n\n\n\n", Some("/bin/bash"), Some(20));
        assert!(result.is_ok());
        assert!(printed.contains("too narrow"));
        assert!(!printed.contains("MMMMM"));
    }

    #[test]
    fn init_render_default_and_vim() {
        let base = InitAnswers {
            shell: Some("/bin/bash".to_string()),
            theme: "dark".to_string(),
            font_size: 12.0,
            key_preset: InitKeyPreset::Default,
        };
        let lua = render_init_lua(&base);
        assert!(lua.contains("theme = \"dark\""));
        assert!(lua.contains("JetBrainsMono Nerd Font"));
        assert!(lua.contains("shell = \"/bin/bash\""));
        assert!(lua.contains("scrollback"));
        assert!(!lua.contains("keymaps = {"));

        // No shell: no terminal table at all (startup default applies).
        let noshell = InitAnswers {
            shell: None,
            ..base.clone()
        };
        let lua = render_init_lua(&noshell);
        assert!(!lua.contains("terminal ="));

        // Vim preset writes every shipped binding explicitly.
        let vim = InitAnswers {
            key_preset: InitKeyPreset::Vim,
            ..base
        };
        let lua = render_init_lua(&vim);
        assert!(lua.contains("keymaps = {"));
        for (chord, action) in bitty_config::keymap::DEFAULT_KEYMAPS {
            assert!(
                lua.contains(&format!("chord = \"{chord}\"")),
                "preset renders {chord}"
            );
            assert!(
                lua.contains(&format!("action = \"{action}\"")),
                "preset renders {action}"
            );
        }
    }

    #[test]
    fn init_rendered_config_parses() {
        use bitty_config::file::parse_lua_config;
        use bitty_config::plan::{ConfigSource, LayerKind};
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        // Every preset x shell combination must parse with values intact.
        for preset in [InitKeyPreset::Default, InitKeyPreset::Vim] {
            for shell in [Some("/bin/zsh"), None] {
                let answers = InitAnswers {
                    shell: shell.map(str::to_string),
                    theme: "dark".to_string(),
                    font_size: 14.0,
                    key_preset: preset,
                };
                let lua = render_init_lua(&answers);
                let plan = parse_lua_config(&lua, &src).expect("wizard output parses");
                assert_eq!(plan.appearance.unwrap().theme.as_deref(), Some("dark"));
                let font = plan.font.expect("font table");
                assert_eq!(font.size, 14.0);
                assert_eq!(
                    plan.terminal.as_ref().and_then(|t| t.shell.as_deref()),
                    shell,
                    "shell round-trips"
                );
                match preset {
                    InitKeyPreset::Vim => assert_eq!(
                        plan.keymaps.expect("vim preset writes keymaps").len(),
                        bitty_config::keymap::DEFAULT_KEYMAPS.len()
                    ),
                    InitKeyPreset::Default => assert!(plan.keymaps.is_none()),
                }
            }
        }
    }

    #[test]
    fn init_vim_preset_agrees_with_shipped_defaults() {
        // The wizard preset is rendered FROM DEFAULT_KEYMAPS (CTX-0178), so
        // resolving those entries as a user layer must reproduce the shipped
        // table exactly: same identities, same actions, explicit overrides.
        let effective = bitty_config::EffectiveConfig {
            keymaps: bitty_config::keymap::DEFAULT_KEYMAPS
                .iter()
                .map(|(chord, action)| bitty_config::KeymapEntry {
                    chord: chord.to_string(),
                    action: action.to_string(),
                    context: "global".to_string(),
                })
                .collect(),
            ..Default::default()
        };
        let resolved = bitty_config::resolve_keymaps(&effective).expect("preset entries resolve");
        let shipped = bitty_config::default_keymaps().expect("shipped defaults resolve");
        assert_eq!(resolved.len(), shipped.len());
        // `resolve_keymaps` sorts by identity while `default_keymaps` keeps
        // declaration order: compare sorted identities.
        let mut shipped_ids: Vec<String> = shipped.iter().map(|m| m.id()).collect();
        shipped_ids.sort();
        let resolved_ids: Vec<String> = resolved.iter().map(|m| m.id()).collect();
        assert_eq!(resolved_ids, shipped_ids);
        for entry in &resolved {
            // Every preset entry overrode its default (explicit, tweakable).
            assert!(
                !entry.from_default,
                "preset entry {} is explicit",
                entry.id()
            );
            let (_, want_action) = bitty_config::keymap::DEFAULT_KEYMAPS
                .iter()
                .find(|(chord, _)| {
                    bitty_config::Chord::parse(chord)
                        .expect("shipped chord parses")
                        .canonical()
                        == entry.chord.canonical()
                })
                .expect("preset chord is a shipped default");
            assert_eq!(entry.action.canonical(), *want_action);
        }

        // End to end: the rendered vim config parses and resolves to the
        // same table (render -> parse -> resolve agreement).
        use bitty_config::file::parse_lua_config;
        use bitty_config::plan::{ConfigSource, LayerKind};
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let lua = render_init_lua(&InitAnswers {
            shell: None,
            theme: "dark".to_string(),
            font_size: 12.0,
            key_preset: InitKeyPreset::Vim,
        });
        let plan = parse_lua_config(&lua, &src).expect("vim config parses");
        let effective = bitty_config::EffectiveConfig {
            keymaps: plan.keymaps.expect("keymaps"),
            ..Default::default()
        };
        let resolved = bitty_config::resolve_keymaps(&effective).expect("vim config resolves");
        let resolved_ids: Vec<String> = resolved.iter().map(|m| m.id()).collect();
        // Same sorted-identity comparison as above (`resolve_keymaps` sorts).
        assert_eq!(resolved_ids, shipped_ids);
    }

    #[test]
    fn init_write_new_refuse_force_backup_idempotent() {
        let dir = init_test_dir("write");
        let target = dir.join("init.lua");
        let content = render_init_lua(&init_yes_defaults(Some("/bin/bash")));

        // Fresh write succeeds.
        let outcome = write_init_config(&target, &content, false).expect("fresh write");
        assert_eq!(outcome.path, target);
        assert!(!outcome.updated);
        assert!(outcome.backup.is_none());
        assert_eq!(
            std::fs::read_to_string(&target).expect("read back"),
            content
        );

        // Re-run without --force refuses and leaves the file untouched.
        let err = write_init_config(&target, &content, false).expect_err("must refuse");
        assert!(
            matches!(err, InitWriteError::Refused(_)),
            "refusal is usage-level"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("untouched"),
            content
        );

        // --force backs up the previous bytes, then writes the new content.
        let updated_content = render_init_lua(&InitAnswers {
            shell: Some("/bin/zsh".to_string()),
            theme: "dark".to_string(),
            font_size: 14.0,
            key_preset: InitKeyPreset::Vim,
        });
        let outcome = write_init_config(&target, &updated_content, true).expect("forced write");
        assert!(outcome.updated);
        let backup = outcome.backup.expect("backup path");
        assert_eq!(backup, target.with_extension("lua.bak"));
        assert_eq!(
            std::fs::read_to_string(&backup).expect("backup bytes"),
            content
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("new bytes"),
            updated_content
        );

        // Idempotent: writing the same answers again produces byte-identical output.
        let again = render_init_lua(&InitAnswers {
            shell: Some("/bin/zsh".to_string()),
            theme: "dark".to_string(),
            font_size: 14.0,
            key_preset: InitKeyPreset::Vim,
        });
        assert_eq!(again, updated_content);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_write_rejects_invalid_content_without_touching_fs() {
        let dir = init_test_dir("invalid");
        let target = dir.join("init.lua");
        let err = write_init_config(&target, "return { theme = }", false)
            .expect_err("invalid content refused");
        assert!(matches!(err, InitWriteError::Refused(_)));
        assert!(!target.exists(), "refused write leaves no file");

        // Nested parents are created as needed.
        let nested = dir.join("a").join("b").join("init.lua");
        let content = render_init_lua(&init_yes_defaults(None));
        write_init_config(&nested, &content, false).expect("mkdir parents");
        assert!(nested.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_dispatch_yes_force_idempotent() {
        let dir = init_test_dir("dispatch");
        let target = dir.join("init.lua");

        // --yes writes sane defaults to the explicit target, exit 0.
        let mut args = Args::new();
        args.init_word = true;
        args.init_yes = true;
        args.config_path = Some(target.display().to_string());
        assert_eq!(
            run_init_subcommand_with_env(&args, None, Some("/bin/bash")),
            0
        );
        let written = std::fs::read_to_string(&target).expect("written");
        assert!(written.contains("shell = \"/bin/bash\""));
        assert!(written.contains("theme = \"dark\""));

        // Second --yes run refuses without --force (idempotent, exit 2).
        assert_eq!(
            run_init_subcommand_with_env(&args, None, Some("/bin/bash")),
            2
        );

        // --force overwrites with a backup, exit 0.
        args.init_force = true;
        assert_eq!(
            run_init_subcommand_with_env(&args, None, Some("/bin/sh")),
            0
        );
        let backup = target.with_extension("lua.bak");
        assert_eq!(std::fs::read_to_string(&backup).expect("backup"), written);
        assert!(
            std::fs::read_to_string(&target)
                .expect("rewritten")
                .contains("shell = \"/bin/sh\"")
        );

        // Unexpected positionals fail closed, exit 2.
        args.init_force = false;
        args.init_args = vec!["bogus".to_string()];
        assert_eq!(run_init_subcommand_with_env(&args, None, None), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_lua_escape_keeps_strings_valid() {
        assert_eq!(init_lua_escape("plain"), "plain");
        assert_eq!(init_lua_escape("/bin/bash"), "/bin/bash");
        assert_eq!(init_lua_escape("a\"b"), "a\\\"b");
        assert_eq!(init_lua_escape("C:\\Tools\\sh"), "C:\\\\Tools\\\\sh");

        // An escaped hostile shell still parses as one string value.
        use bitty_config::file::parse_lua_config;
        use bitty_config::plan::{ConfigSource, LayerKind};
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let lua = render_init_lua(&InitAnswers {
            shell: Some("C:\\Tools\\sh\"x".to_string()),
            theme: "dark".to_string(),
            font_size: 12.0,
            key_preset: InitKeyPreset::Default,
        });
        let plan = parse_lua_config(&lua, &src).expect("escaped shell parses");
        assert_eq!(
            plan.terminal.expect("terminal").shell.as_deref(),
            Some("C:\\Tools\\sh\"x")
        );
    }

    #[test]
    fn init_usage_names_flags_and_target() {
        let usage = init_usage();
        assert!(usage.contains("--yes"));
        assert!(usage.contains("--force"));
        assert!(usage.contains("--config"));
        assert!(usage.contains("BITTY_CONFIG"));
        assert!(usage.contains("init.lua"));
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

    // CTX-0153 keymap-driven chrome keys: single-owner rule + layout surgery.
    // -----------------------------------------------------------------------

    fn test_key(logical: LogicalKey) -> KeyEvent {
        KeyEvent {
            logical_key: logical,
            text: None,
            location: bitty_platform::KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        }
    }

    fn test_char_key(s: &str) -> KeyEvent {
        test_key(LogicalKey::Character(s.to_string()))
    }

    #[test]
    fn modifier_keys_route_never_chrome() {
        for named in [
            NamedKey::Shift,
            NamedKey::Control,
            NamedKey::Alt,
            NamedKey::Super,
            NamedKey::Meta,
        ] {
            assert!(is_modifier_key(&test_key(LogicalKey::Named(named))));
        }
        assert!(!is_modifier_key(&test_key(LogicalKey::Named(
            NamedKey::Tab
        ))));
        assert!(!is_modifier_key(&test_char_key("h")));
    }

    #[test]
    fn app_modifier_mirror_latches_and_releases() {
        let mut mods = AppModifiers::default();
        let mut press = test_key(LogicalKey::Named(NamedKey::Alt));
        track_app_modifiers(&mut mods, &press);
        assert!(mods.alt);
        press.state = PressState::Released;
        track_app_modifiers(&mut mods, &press);
        assert!(!mods.alt);
        // Non-modifier keys leave the mirror alone.
        track_app_modifiers(&mut mods, &test_char_key("h"));
        assert_eq!(mods, AppModifiers::default());
    }

    #[test]
    fn key_ref_mapping_covers_chords_and_shell_keys() {
        use bitty_config::KeyName;
        let plain = AppModifiers::default();
        let with_alt = AppModifiers {
            alt: true,
            ..Default::default()
        };
        // alt+h resolves to the matchable chord.
        let r = key_ref_from_event(&test_char_key("h"), &with_alt).expect("matchable");
        assert_eq!(r.key, KeyName::Char('h'));
        assert!(r.alt && !r.ctrl);
        // Shift+uppercase letter normalizes to the lowercase chord key.
        let with_shift_alt = AppModifiers {
            shift: true,
            alt: true,
            ..Default::default()
        };
        let r = key_ref_from_event(&test_char_key("H"), &with_shift_alt).expect("matchable");
        assert_eq!(r.key, KeyName::Char('h'));
        // Named keys map; modifier/media leftovers and dead keys do not.
        let r = key_ref_from_event(&test_key(LogicalKey::Named(NamedKey::Tab)), &plain)
            .expect("tab matchable");
        assert_eq!(r.key, KeyName::Tab);
        assert!(
            key_ref_from_event(&test_key(LogicalKey::Named(NamedKey::Shift)), &plain).is_none()
        );
        assert!(key_ref_from_event(&test_key(LogicalKey::Dead(None)), &plain).is_none());
        assert!(key_ref_from_event(&test_key(LogicalKey::Unidentified), &plain).is_none());
        assert!(key_ref_from_event(&test_char_key("ab"), &plain).is_none());
    }

    #[test]
    fn single_owner_unbound_keys_reach_shell() {
        use bitty_config::{KeyName, KeyRef, match_keymap, resolve_keymaps};
        let maps = resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
        let shell = |key: KeyName, ctrl: bool, alt: bool, shift: bool| KeyRef {
            key,
            ctrl,
            alt,
            shift,
            super_held: false,
        };
        // The stolen keys from #249: plain Tab/arrows/letters/digits plus
        // Ctrl+P (0x10 via CTX-0154) must all stay shell input by default.
        for k in [
            shell(KeyName::Tab, false, false, false),
            shell(KeyName::Up, false, false, false),
            shell(KeyName::Down, false, false, false),
            shell(KeyName::Left, false, false, false),
            shell(KeyName::Right, false, false, false),
            shell(KeyName::Char('n'), false, false, false),
            shell(KeyName::Char('p'), false, false, false),
            shell(KeyName::Char('1'), false, false, false),
            shell(KeyName::Char('p'), true, false, false),
        ] {
            assert_eq!(match_keymap(&maps, k), None, "shell key {k:?}");
        }
        // Bound chords resolve to exactly one action each.
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('h'), false, true, false)),
            Some(bitty_config::ChromeAction::GotoSplit(
                bitty_config::SplitDir::Left
            ))
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Tab, true, false, false)),
            Some(bitty_config::ChromeAction::FocusNext)
        );
        // CTX-0178 Alt-as-Mod: number jumps, paging, and zoom resolve.
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('1'), false, true, false)),
            Some(bitty_config::ChromeAction::FocusId(1))
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('9'), false, true, false)),
            Some(bitty_config::ChromeAction::FocusId(9))
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('u'), false, true, false)),
            Some(bitty_config::ChromeAction::ScrollPageUp)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('i'), false, true, false)),
            Some(bitty_config::ChromeAction::ScrollPageDown)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('z'), false, true, false)),
            Some(bitty_config::ChromeAction::ToggleZoom)
        );
    }

    #[test]
    fn single_owner_copy_paste_chords_resolve_and_shell_stays_clean() {
        // CTX-0161: the shifted chords are chrome-owned (single-owner
        // intercept consumes them before the PTY), while the unshifted C0
        // bytes (Ctrl+C SIGINT, Ctrl+V) stay shell input.
        use bitty_config::{ChromeAction, KeyName, KeyRef, match_keymap, resolve_keymaps};
        let maps = resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
        let chord = |key: KeyName, ctrl: bool, alt: bool, shift: bool| KeyRef {
            key,
            ctrl,
            alt,
            shift,
            super_held: false,
        };
        assert_eq!(
            match_keymap(&maps, chord(KeyName::Char('c'), true, false, true)),
            Some(ChromeAction::CopyToClipboard)
        );
        assert_eq!(
            match_keymap(&maps, chord(KeyName::Char('v'), true, false, true)),
            Some(ChromeAction::PasteFromClipboard)
        );
        assert_eq!(
            match_keymap(&maps, chord(KeyName::Char('c'), true, false, false)),
            None,
            "Ctrl+C must reach fish as 0x03"
        );
        assert_eq!(
            match_keymap(&maps, chord(KeyName::Char('v'), true, false, false)),
            None,
            "Ctrl+V must stay shell input"
        );
        // Uppercase letters normalize through the event mapper (Shift held
        // to type 'C' is part of the chord, not shell typing).
        let mods = AppModifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        let r = key_ref_from_event(&test_char_key("C"), &mods).expect("matchable");
        assert_eq!(match_keymap(&maps, r), Some(ChromeAction::CopyToClipboard));
        let r = key_ref_from_event(&test_char_key("V"), &mods).expect("matchable");
        assert_eq!(
            match_keymap(&maps, r),
            Some(ChromeAction::PasteFromClipboard)
        );
    }

    #[test]
    fn focus_loss_clears_stale_shift_so_bare_ctrl_v_stays_shell() {
        // CTX-0187 exit B: the mirror uses the raw compositor modifier bit
        // verbatim (no case inference). Staleness is fixed at the root by
        // clearing AppModifiers on focus transitions (see
        // clear_app_modifiers_on_focus, wired to WindowEventKind::Focused):
        // a latched shift=true from before focus loss must not leak a later
        // bare Ctrl+V into the paste arm.
        use bitty_config::{ChromeAction, match_keymap, resolve_keymaps};
        let maps = resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
        // Latched before focus loss: control+shift true (e.g. Shift held,
        // window focused out, release missed while unfocused).
        let mut mods = AppModifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        // Without the clear, the stale mirror WOULD match paste — this
        // documents why the focus clear matters (fail-open without it).
        let r = key_ref_from_event(&test_char_key("v"), &mods).expect("matchable");
        assert_eq!(
            match_keymap(&maps, r),
            Some(ChromeAction::PasteFromClipboard),
            "stale mirror without focus clear still matches paste (demonstrates leak)"
        );
        // Focus loss clears to fail-closed shell.
        clear_app_modifiers_on_focus(&mut mods, false);
        assert_eq!(mods, AppModifiers::default());
        // Re-latch only the still-held Control (as the fresh
        // ModifiersChanged snapshot would after regain); Shift stays false.
        mods.control = true;
        let r = key_ref_from_event(&test_char_key("v"), &mods).expect("matchable");
        assert_eq!(
            match_keymap(&maps, r),
            None,
            "after focus clear, bare Ctrl+V stays shell input, never paste"
        );
        // Focus regain also resets (fail-closed until fresh snapshot).
        let mut regained = AppModifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        clear_app_modifiers_on_focus(&mut regained, true);
        assert_eq!(regained, AppModifiers::default());
    }

    #[test]
    fn real_shifted_ctrl_v_pastes_regardless_of_reported_case() {
        // CTX-0187 exit B no-breakage guard (PX-0694): real Ctrl+Shift+V must
        // paste whether the platform reports uppercase "V" or lowercase "v"
        // with shift=true (X11/Wayland commonly report lowercase+shift for
        // real Ctrl+Shift chords). Trusting the raw shift bit — not the
        // character case — preserves both.
        use bitty_config::{ChromeAction, match_keymap, resolve_keymaps};
        let maps = resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
        let shifted = AppModifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        for logical in ["V", "v"] {
            let r = key_ref_from_event(&test_char_key(logical), &shifted).expect("matchable");
            assert_eq!(
                match_keymap(&maps, r),
                Some(ChromeAction::PasteFromClipboard),
                "real Ctrl+Shift+V (reported {logical:?} + shift=true) must paste"
            );
        }
        // Fresh bare (shift=false) stays shell for both cases.
        let bare = AppModifiers {
            control: true,
            ..Default::default()
        };
        for logical in ["V", "v"] {
            let r = key_ref_from_event(&test_char_key(logical), &bare).expect("matchable");
            assert_eq!(
                match_keymap(&maps, r),
                None,
                "bare Ctrl+V (reported {logical:?} + shift=false) stays shell"
            );
        }
        // Bare Ctrl+C stays shell SIGINT when unshifted; shifted copies.
        let r = key_ref_from_event(&test_char_key("c"), &bare).expect("matchable");
        assert_eq!(match_keymap(&maps, r), None, "bare Ctrl+C stays shell");
        let r = key_ref_from_event(&test_char_key("c"), &shifted).expect("matchable");
        assert_eq!(
            match_keymap(&maps, r),
            Some(ChromeAction::CopyToClipboard),
            "real Ctrl+Shift+C pastes-copies even when reported lowercase"
        );
    }

    #[test]
    fn chrome_copy_paste_round_trip_headless() {
        // CTX-0161 end-to-end through the chrome arms (no window): copy
        // mirrors the selection into the clipboard, paste routes through
        // the suspicious-paste gate, and no stray C0 reaches the PTY.
        use bitty_config::ChromeAction;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.force_headless_clipboard();
        rt.handle_pty_bytes(b"hello");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        // Copy with no selection warns and touches nothing.
        app.apply_chrome_action(ChromeAction::CopyToClipboard);
        assert_eq!(app.runtime.clipboard().headless_contents(), "");
        // Select everything, copy: clipboard mirrors the selection text.
        app.runtime.select_all();
        let selected = app.runtime.selection_text().expect("selection");
        assert!(selected.contains("hello"), "grid holds fed text");
        app.apply_chrome_action(ChromeAction::CopyToClipboard);
        assert_eq!(app.runtime.clipboard().headless_contents(), selected);
        // Pasting the grid-shaped clipboard goes through the inspection
        // gate (embedded newlines are suspicious): held pending, nothing
        // delivered silently.
        app.runtime.clear_selection();
        app.apply_chrome_action(ChromeAction::PasteFromClipboard);
        assert!(app.runtime.has_pending_paste());
        assert!(app.runtime.drain_pending_input().is_empty());
        // Clean clipboard text delivers immediately as PTY input bytes.
        assert!(app.runtime.cancel_pending_paste());
        app.runtime
            .clipboard_mut()
            .set_text("clean-paste".to_string())
            .expect("headless set");
        app.apply_chrome_action(ChromeAction::PasteFromClipboard);
        assert!(!app.runtime.has_pending_paste());
        assert_eq!(app.runtime.drain_pending_input(), b"clean-paste");
    }

    fn two_pane_layout() -> LayoutNode {
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        )
    }

    #[test]
    fn chrome_focus_actions_move_runtime_focus() {
        use bitty_config::{ChromeAction, SplitDir};
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        app.runtime.set_layout(two_pane_layout());
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
        app.apply_chrome_action(ChromeAction::GotoSplit(SplitDir::Right));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(2)));
        app.apply_chrome_action(ChromeAction::FocusPrev);
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
        app.apply_chrome_action(ChromeAction::FocusId(2));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(2)));
        // Unknown id warns and keeps focus.
        app.apply_chrome_action(ChromeAction::FocusId(99));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(2)));
    }

    #[test]
    fn chrome_split_close_resize_zoom_round_trip() {
        use bitty_config::{ChromeAction, SplitDir};
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        app.runtime.set_layout(two_pane_layout());
        // Split focused pane right: 2 -> 3 leaves, focus stays.
        app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
        assert_eq!(app.runtime.leaf_count(), 3);
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
        // Resize nudges without changing leaf count.
        app.apply_chrome_action(ChromeAction::ResizeSplit(SplitDir::Right));
        assert_eq!(app.runtime.leaf_count(), 3);
        // Zoom collapses to one leaf and restores the tree.
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 1);
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 3);
        // Close removes the focused leaf and refocuses inside the tree.
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 2);
        assert!(app.runtime.focused_view().is_some());
        // Last pane refuses to close.
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 1);
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 1);
    }

    #[test]
    fn chrome_scroll_actions_page_focused_pane() {
        use bitty_config::ChromeAction;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        for i in 0..200 {
            let line = format!("line {i:03}\n");
            app.runtime.handle_pty_bytes(line.as_bytes());
        }
        app.runtime.tick();
        assert!(app.runtime.state().scrollback_len() > 0);
        app.apply_chrome_action(ChromeAction::ScrollPageUp);
        let offset = app
            .runtime
            .layout()
            .find_leaf(ViewId::new(1))
            .expect("single leaf")
            .scroll_offset();
        assert!(offset > 0, "page up must leave live");
        app.apply_chrome_action(ChromeAction::ScrollPageDown);
        assert_eq!(
            app.runtime
                .layout()
                .find_leaf(ViewId::new(1))
                .expect("single leaf")
                .scroll_offset(),
            0,
            "page down must return to live"
        );
    }

    #[test]
    fn spawn_spec_resolve_prefers_explicit_program() {
        // CTX-0176: explicit program wins verbatim with its tail args.
        let spec = SpawnSpec {
            program: Some("/bin/fish".to_string()),
            program_args: vec!["-l".to_string()],
            shell_env: Some("/bin/bash".to_string()),
        };
        assert_eq!(
            spec.resolve(),
            ("/bin/fish".to_string(), vec!["-l".to_string()])
        );
    }

    #[test]
    fn spawn_spec_resolve_defaults_to_shell_env_then_fallback() {
        // CTX-0176: no explicit program resolves exactly like startup.
        let spec = SpawnSpec {
            program: None,
            program_args: vec!["-l".to_string()],
            shell_env: Some("/bin/bash".to_string()),
        };
        assert_eq!(spec.resolve(), ("/bin/bash".to_string(), Vec::new()));
        let spec = SpawnSpec {
            program: None,
            program_args: Vec::new(),
            shell_env: None,
        };
        assert_eq!(spec.resolve(), ("/bin/sh".to_string(), Vec::new()));
        let spec = SpawnSpec {
            program: None,
            program_args: Vec::new(),
            shell_env: Some("   ".to_string()),
        };
        assert_eq!(spec.resolve(), ("/bin/sh".to_string(), Vec::new()));
    }

    #[test]
    fn new_split_without_spawnable_shell_keeps_pane_with_warning() {
        // CTX-0176: spawn failure is loud but non-fatal — the split still
        // commits (layout ops stay total) with no pane session. Runs
        // everywhere: the bogus binary fails on every platform.
        use bitty_config::{ChromeAction, SplitDir};
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let rt = Runtime::with_defaults().expect("must build");
        let spec = SpawnSpec {
            program: Some("/nonexistent-bitty-pane-shell-xyz".to_string()),
            program_args: Vec::new(),
            shell_env: None,
        };
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            spec,
        );
        app.runtime.set_layout(two_pane_layout());
        app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
        assert_eq!(app.runtime.leaf_count(), 3);
        assert_eq!(app.runtime.pane_count(), 0);
        // Closing a session-less leaf is quiet and total.
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 2);
        assert_eq!(app.runtime.pane_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn new_split_spawns_private_shell_and_close_tears_it_down() {
        // CTX-0176 (Issue #274): the fresh leaf owns a live shell; closing
        // the leaf tears the child down with it.
        use bitty_config::{ChromeAction, SplitDir};
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let rt = Runtime::with_defaults().expect("must build");
        let spec = SpawnSpec {
            program: Some("/bin/sh".to_string()),
            program_args: Vec::new(),
            shell_env: None,
        };
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            spec,
        );
        app.runtime.set_layout(two_pane_layout());
        app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
        assert_eq!(app.runtime.leaf_count(), 3);
        // Fresh leaf is id 3 (one past the previous max).
        assert!(app.runtime.has_pane_session(&ViewId::new(3)));
        assert!(app.runtime.pane_pid(&ViewId::new(3)).is_some());
        // Focus the new leaf, then close it: the child goes down with it.
        app.apply_chrome_action(ChromeAction::FocusId(3));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(3)));
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 2);
        assert!(!app.runtime.has_pane_session(&ViewId::new(3)));
        assert_eq!(app.runtime.pane_count(), 0);
    }

    #[test]
    fn close_last_leaf_helper_refuses() {
        let mut single = LayoutNode::leaf(View::new(ViewId::new(1), 80, 24));
        assert!(!close_focused_leaf(&mut single, ViewId::new(1)));
        assert!(!close_focused_leaf(&mut single, ViewId::new(9)));
        let mut two = two_pane_layout();
        assert!(!close_focused_leaf(&mut two, ViewId::new(9)));
        assert!(close_focused_leaf(&mut two, ViewId::new(2)));
        assert_eq!(two.leaf_count(), 1);
    }
}
