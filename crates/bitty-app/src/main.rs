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
//!    for the explicit program argument, or via the default shell chain
//!    (configured `terminal.shell` > `$SHELL` > `/bin/sh`, see
//!    [`resolve_default_shell`]) when no program is given. The program is
//!    taken as a direct
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

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::JoinHandle;

use bitty_platform::{
    App, AppHandler, EventContext, EventWaker, LogicalKey, LogicalSize, MouseButton, NamedKey,
    PhysicalSize, PlatformError, PlatformEvent, PressState, WindowConfig, WindowEventKind,
    WindowHandle, WindowId,
};
use bitty_render::gpu::GpuContext;
use bitty_runtime::{LayoutNode, Runtime, SplitAxis};

mod chrome_keys;
mod config_cli;
mod ctl;
mod dev;
mod doctor;
mod init;
mod inspect;
mod ipc_serve;
mod layout_cmd;
mod logging;

mod list;
mod plugin;
mod run;
mod spawn;

use chrome_keys::AppModifiers;
use config_cli::{
    config_usage, load_app_config, load_merged_config, run_config_subcommand,
    runtime_config_from_effective,
};
use init::run_init_subcommand;
use layout_cmd::{
    apply_focus, build_layout, demo_pump_enabled_from_env, run_headless_smoke,
    spawn_demo_pty_pump_with_theme,
};
use logging::{LogLevel, effective_log_level};
use spawn::{
    SpawnSpec, looks_like_negative_number, parse_split_token, resolve_spawn_program,
    spawn_default_shell, spawn_pane_shell, spawn_startup_pane_shells,
};

#[cfg(test)]
use bitty_runtime::{UiRect, View, ViewId};
#[cfg(test)]
use logging::log_level_from_env_value;
#[cfg(test)]
use spawn::{configured_shell_argv0, resolve_default_shell};

#[cfg(test)]
use layout_cmd::{demo_pump_enabled_from_value, run_layout_proof};

#[cfg(test)]
use init::{
    INIT_MASCOT_ART, INIT_MASCOT_FALLBACK, InitAnswers, InitKeyPreset, InitWriteError,
    init_clean_shell, init_columns_from_env, init_greeting_art, init_lua_escape, init_mascot_width,
    init_parse_font_size_answer, init_parse_preset_answer, init_parse_shell_answer,
    init_parse_theme_answer, init_shell_candidates, init_usage, init_yes_defaults, render_init_lua,
    run_init_interactive, run_init_subcommand_with_env, write_init_config,
};

#[cfg(test)]
use config_cli::{
    appearance_flag_for_field, cli_overrides_from_args, layer_source_label,
    resolve_editor_with_env, starter_init_lua,
};

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
    /// When `None`, the spawn layer falls back to the default shell chain
    /// ([`resolve_default_shell`]: configured `terminal.shell` > `$SHELL` >
    /// `/bin/sh`); explicit values are used verbatim.
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
    /// `bitty plugin` CLI-first management (CTX-0150, DEC-0007). True once
    /// the first positional `plugin` word is seen; a program literally named
    /// `plugin` needs `bitty -- plugin ...` (or `bitty run -- plugin ...`).
    /// Tokens after the word land verbatim in `plugin_raw` for
    /// [`plugin::parse_plugin_request`].
    plugin_word: bool,
    /// Raw tokens after the `plugin` word (verb, id, flags) for
    /// [`plugin::parse_plugin_request`]. Empty until `plugin_word` is set.
    plugin_raw: Vec<String>,
    /// Global `--format` before the `plugin` word (fallback merged at
    /// dispatch; a post-word `--format` wins).
    plugin_format: Option<String>,
    /// `--no-color` for plugin table output (global or post-word).
    plugin_no_color: bool,
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
            plugin_word: false,
            plugin_raw: Vec::new(),
            plugin_format: None,
            plugin_no_color: false,
            verbose: false,
            log_level: None,
        }
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
            out.plugin_format = Some(val.to_string());
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
                    out.plugin_format = Some(raw[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("warning: --format needs a value (table|json|jsonl) — ignoring");
                    out.list_format = Some(String::new());
                    out.inspect_format = Some(String::new());
                    out.dev_format = Some(String::new());
                    out.plugin_format = Some(String::new());
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
                out.plugin_no_color = true;
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
                // `bitty plugin` CLI-first management (CTX-0150, DEC-0007).
                // The word `plugin` is always this subcommand, never a
                // program named `plugin`: use `bitty run -- plugin ...` (or
                // legacy `bitty -- plugin ...`) for that program. Tokens
                // after the word are kept verbatim for
                // `plugin::parse_plugin_request`.
                if !program_set
                    && !out.config_word
                    && !out.inspect_word
                    && !out.init_word
                    && !out.doctor_word
                    && !out.run_word
                    && !out.ctl_word
                    && !out.list_word
                    && !out.dev_word
                    && !out.plugin_word
                    && token == "plugin"
                {
                    out.plugin_word = true;
                    out.plugin_raw.extend_from_slice(&raw[i + 1..]);
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
                 --format SHAPE  Doctor/ctl/list/inspect/plugin output shape:\n  \
                              table|json|jsonl (default table; parsed globally,\n  \
                              consumed by `bitty doctor`, `bitty ctl`,\n  \
                              `bitty list`, `bitty inspect`, and\n  \
                              `bitty plugin list|info`; ignored by startup).\n  \
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
           plugin <verb>     CLI-first plugin management (local, no VM):\n  \
                             list|install|remove|enable|disable|info over the\n  \
                             managed manifest (bitty-plugins.toml); install\n  \
                             requires capability consent; remove requires\n  \
                             --force; `bitty plugin --help` for detail\n  \
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
           bitty doctor --format json\n  \
           bitty plugin list\n  \
           bitty plugin install bitty-terminal.tabs --yes\n",
        version_text()
    )
}

fn version_text() -> String {
    env!("CARGO_PKG_VERSION").to_string()
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

/// Window title carrying the resolved theme preset and its source layer.
///
/// Visible via `hyprctl clients` and (where decorations show) the title bar,
/// so screenshots plus class-check prove which config path the window took:
/// `... — bitty-dark (default)` vs `... — bitty-dark (file)`.
fn window_title_for_theme(theme_name: &str, source: &str) -> String {
    format!("bitty \u{2014} Correct Terminal \u{2014} {theme_name} ({source})")
}

// ---------------------------------------------------------------------------
// App handler
// ---------------------------------------------------------------------------

/// `AppModifiers` lives in [`chrome_keys`] (CTX-0233 pure move).
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
    /// [`WindowConfig`](bitty_platform::WindowConfig) at creation and to the
    /// renderer at GPU attach (CTX-0290): the platform requests compositor
    /// blending where supported, and the renderer scales its premultiplied
    /// output so the value has a visible effect. Platforms without
    /// premultiplied compositing stay opaque with a loud warning.
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
    /// Chrome-owned keys with an unreleased press (CTX-0229 press-to-release
    /// ownership). A consumed chord owns its key until the physical release:
    /// repeats/duplicates arriving after modifier decay stay swallowed
    /// instead of leaking shell bytes (e.g. `Ctrl+Shift+V` paste followed by
    /// a `V` repeat with `Ctrl` already released must not type `V`).
    /// Releases and focus transitions clear entries (same staleness bound as
    /// the CTX-0187 mirror clear); bounded by simultaneously held keys.
    chrome_held: HashSet<bitty_config::KeyName>,
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
            chrome_held: HashSet::new(),
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
            chrome_held: HashSet::new(),
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

    /// Sets the window opacity applied at creation and GPU attach
    /// (CTX-0223/CTX-0290). Call once at startup from the effective config;
    /// the value is sanitized by the platform
    /// [`WindowConfig`](bitty_platform::WindowConfig) and the renderer
    /// surface, so out-of-range inputs degrade instead of failing creation.
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
            "bitty tick: frame={} fills={} glyphs={} headless={} gen={} presented_frames={} focused={:?} leafs={} gpu={} crossfont={} images={} images_skipped={}",
            present.frame,
            present.fills,
            present.glyphs,
            present.headless,
            present.generation,
            presented_frames,
            focused,
            leafs,
            gpu,
            crossfont,
            present.images,
            present.images_skipped
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
            // CTX-0230: same post-tick flush for every pane session, so a
            // split shell's query answers never wait for the next pump.
            // Bounded per pane (reply cap, fail-closed); no-op when quiet.
            let mut pane_written = 0usize;
            for id in self.runtime.pane_session_ids() {
                pane_written += self.runtime.write_pane_replies(id);
            }
            if pane_written > 0 && self.tick_logging_enabled() {
                eprintln!(
                    "bitty: {pane_written} pane reply bytes written to PTY masters (post-tick)"
                );
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
                    // and the effective window opacity (CTX-0290): the
                    // renderer scales its premultiplied output so the
                    // compositor can blend the window at `window.opacity`.
                    match surface.configure_with_opacity(&gpu, extent, self.window_opacity) {
                        Ok(()) => {
                            if self.window_opacity < 1.0 && !surface.opacity_alpha_supported() {
                                eprintln!(
                                    "bitty: window.opacity={:.3} unsupported on this GPU surface (no premultiplied alpha mode) — staying opaque",
                                    bitty_platform::sanitize_opacity(self.window_opacity)
                                );
                            }
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

// Chrome-key cluster lives in `chrome_keys` (CTX-0233 pure move:
// `is_modifier_key`, `track_app_modifiers`, `clear_app_modifiers_on_focus`,
// `key_ref_from_event`, split/layout-surgery helpers).
/// Chrome actions live in [`chrome_keys`] (CTX-0233 pure move).
/// Chrome intercept lives in [`chrome_keys`] (CTX-0233 pure move).
impl AppHandler for TerminalApp {
    fn set_event_waker(&mut self, waker: EventWaker) {
        // Bridge the platform proxy into the runtime's bounded wakeup pump:
        // the forwarder thread owns its clone and wakes once per readability
        // signal (plus once on EOF). `Mutex` keeps the closure `Send + Sync`
        // even if the proxy is only `Send`.
        let shared = std::sync::Arc::new(std::sync::Mutex::new(waker));
        let shared_pty = std::sync::Arc::clone(&shared);
        let pty_waker: bitty_runtime::PtyWaker = std::sync::Arc::new(move || {
            if let Ok(w) = shared_pty.lock() {
                w.wake_pty();
            }
        });
        self.runtime.set_pty_waker(pty_waker);
        // CTX-0235: wake the same event loop when an IPC control action is
        // enqueued, so an idle window (`ControlFlow::Wait`, no PTY damage)
        // drains the control queue promptly instead of letting every verb
        // time out. The wakeup reuses the PTY-readability signal because
        // that arm already drains the control queue first via `drive_tick`;
        // it grants nothing — `drain_global_control_queue` re-authorizes
        // every action against the servo scopes before applying.
        if let Ok(w) = shared.lock() {
            let control_proxy = w.clone();
            bitty_ipc::ctl::set_control_waker(Some(std::sync::Arc::new(move || {
                control_proxy.wake_pty();
            })));
        }
        eprintln!("bitty: pty wakeup armed (event-loop proxy)");
    }

    fn handle_event(&mut self, ctx: &mut EventContext<'_>, event: PlatformEvent) {
        // Bounded PTY pump: drain before handling the event so fresh bytes are
        // visible to the state machine before the tick.
        self.poll_pty_pump();

        // CTX-0153 single-owner intercept with CTX-0275 explicit dispatch
        // priority (see `intercept_chrome_key` / `DispatchPriority`):
        // emergency/reserved > active overlay-modal > user-defined keymap >
        // plugin (inert, deny-by-default) > terminal encoding. A consumed
        // event never reaches `Runtime`.
        if let PlatformEvent::Window { window_id: _, kind } = &event {
            if self.intercept_chrome_key(kind) {
                return;
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
    // `bitty plugin --help` shows plugin help (never needs an instance, a
    // config file, or a plugin VM).
    if args.help && args.plugin_word {
        println!("{}", plugin::plugin_help_text());
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

    // `bitty plugin` CLI-first management (CTX-0150, DEC-0007). Local class:
    // static bundled manifests plus the managed manifest only — no instance,
    // no IPC, no plugin VM and no plugin code ever loaded. Capability
    // consent prompts read stdin and fail closed on EOF.
    if args.plugin_word {
        let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
        let xdg_config_home = std::env::var("XDG_CONFIG_HOME").ok();
        let home = std::env::var("HOME").ok();
        let context = plugin::PluginContext {
            config_path: args.config_path.as_deref(),
            bitty_config_env: bitty_config_env.as_deref(),
            xdg_config_home: xdg_config_home.as_deref(),
            home: home.as_deref(),
            pre_format: args.plugin_format.as_deref(),
            pre_no_color: args.plugin_no_color,
        };
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        let stdout = std::io::stdout();
        let mut output = stdout.lock();
        std::process::exit(plugin::run_plugin_subcommand(
            &args.plugin_raw,
            &context,
            &mut input,
            &mut output,
        ));
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
    // bare invocation resolves to the default shell chain (configured
    // `terminal.shell` > $SHELL > /bin/sh, CTX-0298).
    // Headless CI still succeeds even if spawn fails (bounded synthetic smoke).
    // `$SHELL` and the effective configured shell are read once here and
    // injected into the pure resolver so arg handling stays testable; both are
    // trusted only as binary paths, never split.
    let shell_env = std::env::var("SHELL").ok();
    let config_shell = app_config.effective.terminal.shell.clone();
    // CTX-0176: frozen once so every split leaf replays this resolution.
    let spawn_spec = SpawnSpec {
        program: args.program.clone(),
        program_args: args.program_args.clone(),
        shell_env: shell_env.clone(),
        config_shell: config_shell.clone(),
    };
    let effective = resolve_spawn_program(&args, config_shell.as_deref(), shell_env.as_deref());
    eprintln!(
        "bitty: effective program {effective:?} (explicit={}, configured_shell={})",
        args.program.is_some(),
        config_shell.is_some()
    );
    let spawn_result = if let Some(program) = args.program.as_deref() {
        let tail: Vec<&str> = args.program_args.iter().map(|s| s.as_str()).collect();
        if tail.is_empty() {
            runtime.spawn_shell(program)
        } else {
            runtime.spawn_shell_with_args(program, &tail)
        }
    } else {
        spawn_default_shell(&mut runtime, config_shell.as_deref(), shell_env.as_deref())
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
        // resolve the default shell chain (configured `terminal.shell` >
        // $SHELL > /bin/sh) for completeness.
        // CTX-0176: startup panes get their own shells here too (same rule as
        // the primary path above — panes only when the primary spawn worked).
        let fallback_spec = SpawnSpec {
            program: args.program.clone(),
            program_args: args.program_args.clone(),
            shell_env: std::env::var("SHELL").ok(),
            config_shell: app_config.effective.terminal.shell.clone(),
        };
        let fallback_primary_ok = if let Some(program) = args.program.as_deref() {
            let tail: Vec<&str> = args.program_args.iter().map(|s| s.as_str()).collect();
            if tail.is_empty() {
                rt.spawn_shell(program).is_ok()
            } else {
                rt.spawn_shell_with_args(program, &tail).is_ok()
            }
        } else {
            spawn_default_shell(
                &mut rt,
                fallback_spec.config_shell.as_deref(),
                fallback_spec.shell_env.as_deref(),
            )
            .is_ok()
        };
        if fallback_primary_ok {
            spawn_startup_pane_shells(&mut rt, &fallback_spec);
        }
        let code = run_headless_smoke(&mut rt);
        std::process::exit(code);
    }
}

#[cfg(test)]
mod tests;
