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
//! 4. **Build layout** via [`build_layout`] from the parsed [`crate::cli::Args`] (default
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
//!    [`crate::spawn::resolve_default_shell`]) when no program is given. The program is
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

use bitty_platform::{App, PlatformError};
use bitty_runtime::Runtime;

mod chrome_keys;
mod cli;
mod config_cli;
mod ctl;
mod dev;
mod doctor;
mod init;
mod inspect;
mod ipc_serve;
mod layout_cmd;
mod logging;
mod plugin_runtime;

mod list;
mod plugin;
mod run;
mod spawn;
mod terminal_app;

use cli::{help_text, parse_args, version_text};
use config_cli::{
    config_usage, load_app_config, run_config_subcommand, runtime_config_from_effective,
};
use init::run_init_subcommand;
use layout_cmd::{apply_focus, build_layout, demo_pump_enabled_from_env, run_headless_smoke};
use logging::effective_log_level;
use spawn::{SpawnSpec, resolve_spawn_program, spawn_default_shell, spawn_startup_pane_shells};
use terminal_app::TerminalApp;

#[cfg(test)]
use bitty_runtime::{LayoutNode, SplitAxis, UiRect, View, ViewId};
#[cfg(test)]
use logging::{LogLevel, log_level_from_env_value};
#[cfg(test)]
use spawn::{configured_shell_argv0, looks_like_negative_number, resolve_default_shell};

#[cfg(test)]
use cli::{Args, ConfigCommand};

#[cfg(test)]
use layout_cmd::{demo_pump_enabled_from_value, run_layout_proof, spawn_demo_pty_pump_with_theme};

#[cfg(test)]
use terminal_app::window_title_for_theme;

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
        std::process::exit(doctor::run_cli(&args));
    }

    // `bitty ctl` runtime control (CTX-0171, runtime class). Dispatched
    // before config load and GUI startup: `--help` never needs an instance;
    // other verbs resolve `--socket`/`--instance`/inherited/exactly-one
    // targeting and speak the versioned IPC protocol. Parse failures are
    // usage errors (exit 2).
    if args.ctl_word {
        std::process::exit(ctl::run_cli(&args));
    }

    // `bitty list <kind>` enumeration (CTX-0172). Local kinds never touch
    // config/instance; `instances` does its own socket discovery. Runs
    // before config loading so `list` works with a missing or invalid
    // config file (safe-mode clean, no plugin VM).
    if args.list_word {
        std::process::exit(list::run_cli(&args));
    }

    // `bitty inspect <target> <value>` state and ownership (CTX-0173, local
    // class, safe mode). Runs before config loading so `inspect` works with
    // a missing or invalid config file: every target resolves from built-in
    // defaults and static manifests (no file I/O, no instance, no VM).
    if args.inspect_word {
        std::process::exit(inspect::run_cli(&args));
    }

    // `bitty dev <verb>` tracing, captures, dumps, overlays (CTX-0174,
    // local class). Dispatched before config load and GUI startup: no
    // instance, no IPC, no plugin VM. Parse failures are usage errors
    // (exit 2); post-parse failures are generic errors (exit 1).
    if args.dev_word {
        std::process::exit(dev::run_cli(&args));
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

    // Gap A startup wiring (RFC plugin-host-runtime-rfc): discover bundled
    // plugin packages and activate each in its own VM on this thread. The
    // runtime is retained for the process lifetime so its registrations and
    // host-service state outlive startup; command/event delivery from the
    // event loop is a follow-up slice. `--safe` creates no third-party VM.
    let _plugin_runtime = plugin_runtime::discover_and_activate(
        args.safe,
        runtime.config().cols,
        runtime.config().rows,
    );

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
