// ---------------------------------------------------------------------------
// Tests (pure arg parsing + headless smoke totality)
// ---------------------------------------------------------------------------

use super::chrome_keys::two_pane_layout;
use super::*;
// Only the POSIX-shell live-spawn test below uses this (`#[cfg(unix)]`);
// without the gate the import is unused on Windows.
#[cfg(unix)]
use bitty_test_support::require_pty;

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
fn default_shell_prefers_shell_env_when_no_configured_shell() {
    assert_eq!(resolve_default_shell(None, None), "/bin/sh");
    assert_eq!(resolve_default_shell(None, Some("/bin/fish")), "/bin/fish");
    assert_eq!(resolve_default_shell(None, Some("/bin/bash")), "/bin/bash");
    assert_eq!(
        resolve_default_shell(None, Some("/usr/bin/zsh")),
        "/usr/bin/zsh"
    );
}

#[test]
fn default_shell_prefers_configured_terminal_shell_over_env() {
    // CTX-0298: the effective `terminal.shell` outranks $SHELL.
    assert_eq!(
        resolve_default_shell(Some("/bin/zsh"), Some("/bin/fish")),
        "/bin/zsh"
    );
    assert_eq!(
        resolve_default_shell(Some("  /bin/zsh  "), Some("/bin/fish")),
        "/bin/zsh"
    );
    assert_eq!(resolve_default_shell(Some("/bin/zsh"), None), "/bin/zsh");
}

#[test]
fn default_shell_fails_closed_on_unusable_configured_shell() {
    // CTX-0298: blank/oversized/control-laden configured values never
    // reach execve; they fall through to $SHELL, then /bin/sh.
    for bad in ["", "   ", "\t\n ", "/bin/z\nsh", "/bin/zsh\u{7}"] {
        assert_eq!(
            resolve_default_shell(Some(bad), Some("/bin/fish")),
            "/bin/fish",
            "configured {bad:?} fails closed to $SHELL"
        );
        assert_eq!(resolve_default_shell(Some(bad), None), "/bin/sh");
    }
    // Edge whitespace is trimmed, not rejected: the normalized path runs.
    assert_eq!(
        resolve_default_shell(Some("/bin/zsh\n"), Some("/bin/fish")),
        "/bin/zsh"
    );
    let overlong = "a".repeat(bitty_config::types::MAX_SHELL_LEN + 1);
    assert_eq!(
        resolve_default_shell(Some(&overlong), Some("/bin/fish")),
        "/bin/fish"
    );
}

#[test]
fn default_shell_falls_back_when_env_missing_or_blank() {
    assert_eq!(resolve_default_shell(None, Some("")), "/bin/sh");
    assert_eq!(resolve_default_shell(None, Some("   ")), "/bin/sh");
    assert_eq!(resolve_default_shell(None, Some("\t\n ")), "/bin/sh");
}

#[test]
fn default_shell_trims_surrounding_whitespace() {
    assert_eq!(
        resolve_default_shell(None, Some("  /bin/fish  ")),
        "/bin/fish"
    );
    assert_eq!(
        resolve_default_shell(Some("  /bin/fish  "), None),
        "/bin/fish"
    );
}

#[test]
fn configured_shell_argv0_rejects_unusable_values() {
    assert_eq!(configured_shell_argv0(None), None);
    assert_eq!(configured_shell_argv0(Some("")), None);
    assert_eq!(configured_shell_argv0(Some(" \t ")), None);
    assert_eq!(configured_shell_argv0(Some("/bin/zsh\u{7}")), None);
    assert_eq!(configured_shell_argv0(Some("/bin/fish")), Some("/bin/fish"));
    assert_eq!(
        configured_shell_argv0(Some("  /bin/fish  ")),
        Some("/bin/fish")
    );
}

#[test]
fn bare_args_resolve_to_default_shell_chain() {
    let parsed = parse_args(&args_of(&["bitty"]));
    assert_eq!(parsed.program, None);
    assert_eq!(
        resolve_spawn_program(&parsed, Some("/bin/zsh"), Some("/bin/fish")),
        "/bin/zsh"
    );
    assert_eq!(
        resolve_spawn_program(&parsed, None, Some("/bin/fish")),
        "/bin/fish"
    );
    assert_eq!(resolve_spawn_program(&parsed, None, None), "/bin/sh");
    assert_eq!(resolve_spawn_program(&parsed, None, Some("")), "/bin/sh");
}

#[test]
fn explicit_program_arg_stays_identical() {
    let parsed = parse_args(&args_of(&["bitty", "--", "fish", "-l"]));
    assert_eq!(parsed.program.as_deref(), Some("fish"));
    assert_eq!(parsed.program_args, vec!["-l"]);
    // Explicit program wins over any injected configured shell + $SHELL.
    assert_eq!(
        resolve_spawn_program(&parsed, Some("/bin/zsh"), Some("/bin/bash")),
        "fish"
    );
    assert_eq!(resolve_spawn_program(&parsed, None, None), "fish");

    let parsed = parse_args(&args_of(&["bitty", "/bin/bash"]));
    assert_eq!(
        resolve_spawn_program(&parsed, Some("/bin/zsh"), Some("/bin/fish")),
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
        images: 0,
        images_skipped: 0,
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
        images: 1,
        images_skipped: 0,
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
        images: 0,
        images_skipped: 0,
    };
    let line = TerminalApp::format_tick_line(&present, 1, None, 1, false, false);
    assert!(line.starts_with("bitty tick:"));
    assert!(line.contains("frame=2"));
    assert!(line.contains("fills=1921"));
    assert!(line.contains("glyphs=21"));
    assert!(line.contains("headless=true"));
    assert!(line.contains("gen=30"));
    // CTX-0253 F3: the image gate counters ride the verbose tick line
    // so a real-GPU skip is user-visible in logs, never silent.
    assert!(line.contains("images=0"));
    assert!(line.contains("images_skipped=0"));
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
    // CTX-0234: session-less unfocused tiles present erased (no primary
    // duplication), so sparse bytes render identically across
    // compositions. Full-width marker rows 1..=10 keep split / stack /
    // overlay pairwise distinct: the overlay leaf blanks rows 5..=10 at
    // cols 5..=24, the split right tile stays blank, the stack covers
    // the grid — while same-layout replays stay bit-identical.
    let mut synthetic = Vec::new();
    for row in 1..=10u32 {
        synthetic.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
        synthetic.extend(std::iter::repeat_n(b'A'.wrapping_add((row % 26) as u8), 80));
    }
    let code = run_layout_proof(&synthetic);
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
    let merged = resolve_effective_full(None, Some(profile.clone()), &cli_none).expect("profile");
    assert_eq!(merged.effective.appearance.theme.as_deref(), Some("dark"));
    assert_eq!(
        merged.source_of("appearance.theme").unwrap().layer,
        LayerKind::Profile
    );
    let user_src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let user_plan =
        parse_lua_config(r#"return { theme = "bitty-dark" }"#, &user_src).expect("user");
    let user = LayeredPlan::new(user_src, user_plan);
    let merged = resolve_effective_full(Some(user), Some(profile), &cli_none).expect("user wins");
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
        bitty_config::file::profile_file_path_with_env("../evil", Some("/x"), Some("/h")).is_err()
    );
}

#[test]
fn cli_flag_wins_over_file_wins_over_default() {
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    // File layer from a Lua chunk (no fs): theme "dark".
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let file_plan = parse_lua_config(r#"return { theme = "dark" }"#, &src).expect("file parses");
    let file_layer = bitty_config::plan::LayeredPlan::new(src, file_plan);
    // CLI wins over file.
    let merged =
        resolve_effective(Some(file_layer.clone()), Some("bitty-dark")).expect("merge cli>file");
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
    // 10x22 covers the measured 12pt raster truth (CTX-0237: advance
    // 10, line 22). This intentionally differs from the headless
    // RuntimeConfig 9x19 compiled defaults.
    assert_eq!((cfg.cell_width, cfg.cell_height), (10, 22));
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
fn runtime_config_inherits_file_scrollback() {
    // CTX-0297: `terminal.scrollback` flows file -> effective -> runtime
    // and bounds retained history at terminal creation. The runtime
    // default mirrors the terminal-state default (pairing pinned here;
    // `bitty-runtime` must not depend on `bitty-config`).
    assert_eq!(
        bitty_runtime::config::DEFAULT_SCROLLBACK_LINES,
        bitty_term_state::SCROLLBACK_DEFAULT_LINES
    );
    assert_eq!(
        bitty_runtime::config::MAX_SCROLLBACK_LINES,
        bitty_term_state::SCROLLBACK_MAX_LINES
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(r#"return { terminal = { scrollback = 4242 } }"#, &src)
        .expect("scrollback parses");
    let layer = bitty_config::plan::LayeredPlan::new(src, plan);
    let merged = resolve_effective(Some(layer), None).expect("merge");
    assert_eq!(merged.effective.terminal.scrollback, 4242);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.scrollback, 4242);
    // The default value rides through unchanged.
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
        cfg2.scrollback,
        bitty_runtime::config::DEFAULT_SCROLLBACK_LINES
    );
}

#[test]
fn runtime_config_rejects_scrollback_bound_drift() {
    // CTX-0297: a future `bitty-config` bound raised past the runtime /
    // terminal-state hard cap must fail closed instead of silently
    // clamping the retention semantics.
    let mut effective = bitty_config::EffectiveConfig::default();
    effective.terminal.scrollback = 200_000;
    let err =
        runtime_config_from_effective(&effective).expect_err("above hard cap must fail closed");
    assert!(err.contains("terminal.scrollback"), "field named: {err}");
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
fn runtime_config_inherits_file_focus_follows_mouse() {
    // CTX-0260: `mouse.focus_follows_mouse` flows file -> effective ->
    // runtime; crate defaults stay equal (bitty-runtime must not depend
    // on bitty-config, so the pairing is by value, pinned here).
    // Default off preserves click-to-focus (zero change for existing
    // users).
    assert_eq!(
        bitty_runtime::config::DEFAULT_FOCUS_FOLLOWS_MOUSE,
        bitty_config::types::DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE
    );
    const { assert!(!bitty_runtime::config::DEFAULT_FOCUS_FOLLOWS_MOUSE) }
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(r#"return { mouse = { focus_follows_mouse = true } }"#, &src)
        .expect("opt-in parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert!(merged.effective.mouse.focus_follows_mouse);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert!(cfg.focus_follows_mouse);
    assert_eq!(
        merged.source_of("mouse.focus_follows_mouse").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    // Absent table rides the default-off end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("no mouse table parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert!(!merged2.effective.mouse.focus_follows_mouse);
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert!(!cfg2.focus_follows_mouse);
    assert_eq!(
        merged2
            .source_of("mouse.focus_follows_mouse")
            .unwrap()
            .layer,
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
fn runtime_config_inherits_file_decoration() {
    // CTX-0292 / accepted spec CTX-0118: `decoration.gaps_in`,
    // `decoration.gaps_out`, `decoration.border`, `decoration.radius`
    // flow file -> effective -> runtime; the crate constants stay equal
    // (bitty-runtime aliases bitty-ui, bitty-config owns its own copy;
    // the pairing is pinned here). Default is the accepted 4/6/2/6.
    assert_eq!(
        bitty_runtime::config::DEFAULT_DECORATION_GAPS_IN_PX,
        bitty_config::types::DEFAULT_DECORATION_GAPS_IN_PX as u16
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_DECORATION_GAPS_OUT_PX,
        bitty_config::types::DEFAULT_DECORATION_GAPS_OUT_PX as u16
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_DECORATION_BORDER_PX,
        bitty_config::types::DEFAULT_DECORATION_BORDER_PX as u16
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_DECORATION_RADIUS_PX,
        bitty_config::types::DEFAULT_DECORATION_RADIUS_PX as u16
    );
    assert_eq!(
        u32::from(bitty_runtime::config::MAX_DECORATION_GAP_PX),
        bitty_config::types::MAX_DECORATION_GAP_PX
    );
    assert_eq!(
        u32::from(bitty_runtime::config::MAX_DECORATION_BORDER_PX),
        bitty_config::types::MAX_DECORATION_BORDER_PX
    );
    assert_eq!(
        u32::from(bitty_runtime::config::MAX_DECORATION_RADIUS_PX),
        bitty_config::types::MAX_DECORATION_RADIUS_PX
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { decoration = { gaps_in = 0, gaps_out = 1, border = 1, radius = 0 } }"#,
        &src,
    )
    .expect("decoration parse");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(merged.effective.decoration.gaps_in, 0);
    assert_eq!(merged.effective.decoration.gaps_out, 1);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.decoration, bitty_runtime::Decoration::new(0, 1, 1, 0));
    assert_eq!(
        merged.source_of("decoration.gaps_in").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    // Absent table rides the accepted CTX-0118 defaults end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("no decoration table parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert_eq!(
        merged2.effective.decoration,
        bitty_config::types::DecorationConfig::default()
    );
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!(cfg2.decoration, bitty_runtime::Decoration::default());
    assert_eq!(cfg2.decoration, bitty_runtime::Decoration::new(4, 6, 2, 6));
    assert_eq!(
        merged2.source_of("decoration.gaps_in").unwrap().layer,
        bitty_config::plan::LayerKind::CoreDefaults
    );
    // Safe mode inverts to 0/0/1/0 regardless of user configuration.
    let safe = bitty_config::reload::fallback_builtin();
    let safe_cfg = runtime_config_from_effective(&safe).expect("safe builds");
    assert_eq!(safe_cfg.decoration, bitty_runtime::Decoration::SAFE);
    assert_eq!(
        safe_cfg.decoration,
        bitty_runtime::Decoration::new(0, 0, 1, 0)
    );
    // Out-of-range decoration fails closed at the file layer.
    for bad in [
        r#"return { decoration = { gaps_in = 33 } }"#,
        r#"return { decoration = { gaps_out = 33 } }"#,
        r#"return { decoration = { border = 9 } }"#,
        r#"return { decoration = { radius = 17 } }"#,
        r#"return { decoration = { gaps_in = 1, bogus = 2 } }"#,
    ] {
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        parse_lua_config(bad, &src).expect_err("must fail closed");
    }
}

#[test]
fn runtime_config_inherits_file_scrollbar() {
    // CTX-0181: `scrollbar.mode`/`scrollbar.width` flow file ->
    // effective -> runtime; crate defaults stay equal (bitty-runtime
    // must not depend on bitty-config, so the pairing is by value,
    // pinned here). Default preserves hidden (zero change for existing
    // users).
    assert_eq!(
        bitty_runtime::config::DEFAULT_SCROLLBAR_WIDTH,
        bitty_config::types::DEFAULT_SCROLLBAR_WIDTH
    );
    assert_eq!(
        bitty_runtime::config::MAX_SCROLLBAR_WIDTH_PX,
        bitty_config::types::MAX_SCROLLBAR_WIDTH_PX
    );
    assert_eq!(
        bitty_runtime::ScrollbarMode::Hidden.as_str(),
        bitty_config::ScrollbarMode::Hidden.as_str()
    );
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { scrollbar = { mode = "auto", width = 12 } }"#,
        &src,
    )
    .expect("scrollbar parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(
        merged.effective.scrollbar.mode,
        bitty_config::ScrollbarMode::Auto
    );
    assert_eq!(merged.effective.scrollbar.width, 12);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime cfg builds");
    assert_eq!(cfg.scrollbar_mode, bitty_runtime::ScrollbarMode::Auto);
    assert_eq!(cfg.scrollbar_width, 12);
    assert_eq!(
        merged.source_of("scrollbar.mode").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
    // Absent table rides hidden end to end.
    let src2 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan2 = parse_lua_config(r#"return { terminal = { scrollback = 10000 } }"#, &src2)
        .expect("no scrollbar table parses");
    let merged2 = resolve_effective(
        Some(bitty_config::plan::LayeredPlan::new(src2, plan2)),
        None,
    )
    .expect("merge");
    assert_eq!(
        merged2.effective.scrollbar.mode,
        bitty_config::ScrollbarMode::Hidden
    );
    let cfg2 = runtime_config_from_effective(&merged2.effective).expect("builds");
    assert_eq!(cfg2.scrollbar_mode, bitty_runtime::ScrollbarMode::Hidden);
    assert_eq!(
        merged2.source_of("scrollbar.mode").unwrap().layer,
        bitty_config::plan::LayerKind::CoreDefaults
    );
    // Unknown modes and oversized widths fail closed at the file layer
    // (never reach runtime).
    let src3 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    parse_lua_config(r#"return { scrollbar = { mode = "overlay" } }"#, &src3)
        .expect_err("must fail");
    let src4 = ConfigSource::new(LayerKind::User, Some("init.lua"));
    parse_lua_config(r#"return { scrollbar = { width = 33 } }"#, &src4).expect_err("must fail");
}

#[test]
fn runtime_config_inherits_file_window_padding() {
    // CTX-0223: `window.padding` flows file -> effective -> runtime;
    // crate defaults stay equal (bitty-runtime must not depend on
    // bitty-config, so the pairing is by value, pinned here). Default
    // preserves the 8px breathing room for existing users.
    // CTX-0241 S0: `window.radius_px` rides the same path as a parsed
    // no-op (default 0, zero render effect); existing `{ opacity,
    // padding }` tables default it to 0 end to end.
    assert_eq!(
        bitty_runtime::config::DEFAULT_WINDOW_PADDING,
        bitty_config::EffectiveConfig::default().window.padding
    );
    assert_eq!(
        bitty_runtime::config::DEFAULT_WINDOW_RADIUS_PX,
        bitty_config::EffectiveConfig::default().window.radius_px
    );
    assert_eq!(
        bitty_runtime::config::MAX_WINDOW_RADIUS_PX,
        bitty_config::types::MAX_WINDOW_RADIUS_PX,
        "runtime bound must match config validation (`must be <= 24`)"
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
    // CTX-0241 S0: legacy table without `radius_px` defaults to 0.
    assert_eq!(merged.effective.window.radius_px, 0);
    assert_eq!(cfg.window_radius_px, 0);
    assert_eq!(
        merged.source_of("window.radius_px").unwrap().layer,
        bitty_config::plan::LayerKind::User
    );
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
    assert_eq!(
        merged2.effective.window.radius_px,
        bitty_runtime::config::DEFAULT_WINDOW_RADIUS_PX
    );
    assert_eq!(
        cfg2.window_radius_px,
        bitty_runtime::config::DEFAULT_WINDOW_RADIUS_PX
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
fn runtime_config_inherits_file_window_radius_noop() {
    // CTX-0241 S0: `window.radius_px` (physical px, `0..=24`, default 0)
    // flows file -> effective -> runtime as a parsed no-op: accepted,
    // stored, reported, zero render effect (proved in
    // `bitty-runtime/tests/window_radius_noop.rs`).
    use bitty_config::file::{parse_lua_config, resolve_effective};
    use bitty_config::plan::{ConfigSource, LayerKind};
    let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
    let plan = parse_lua_config(
        r#"return { window = { opacity = 1.0, padding = 8, radius_px = 12 } }"#,
        &src,
    )
    .expect("window radius parses");
    let merged = resolve_effective(Some(bitty_config::plan::LayeredPlan::new(src, plan)), None)
        .expect("merge");
    assert_eq!(merged.effective.window.radius_px, 12);
    let cfg = runtime_config_from_effective(&merged.effective).expect("runtime builds");
    assert_eq!(cfg.window_radius_px, 12);
    assert_eq!(
        merged.source_of("window.radius_px").unwrap().layer,
        LayerKind::User
    );
    // Fail-closed validation: negative / oversized / float radius never
    // reaches runtime (file layer rejects with the field path).
    for bad in [
        r#"return { window = { opacity = 1.0, padding = 8, radius_px = -1 } }"#,
        r#"return { window = { opacity = 1.0, padding = 8, radius_px = 25 } }"#,
        r#"return { window = { opacity = 1.0, padding = 8, radius_px = 100 } }"#,
        r#"return { window = { opacity = 1.0, padding = 8, radius_px = 1.5 } }"#,
    ] {
        let src = ConfigSource::new(LayerKind::User, Some("init.lua"));
        let err = parse_lua_config(bad, &src).expect_err("must fail");
        assert!(
            err.to_string().contains("window.radius_px"),
            "bad radius must name field: {err}"
        );
    }
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
    // CTX-0181: starter leaves `scrollbar` unset (commented example
    // only) so new installs ride hidden without a file override.
    assert!(plan.scrollbar.is_none());
    assert!(starter_init_lua().contains("scrollbar"));
    // CTX-0260: starter leaves `mouse` unset (commented example only)
    // so new installs ride click-to-focus without a file override.
    assert!(plan.mouse.is_none());
    assert!(starter_init_lua().contains("focus_follows_mouse"));
}

// -- `bitty init` wizard (CTX-0149, #243) --------------------------------

/// Unique scratch directory per test (process id + atomic counter: tests
/// in one binary share the id and run on parallel threads).
fn init_test_dir(tag: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("bitty-ctx0149-{tag}-{}-{n}", std::process::id()));
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
fn parse_plugin_subcommand() {
    let p = parse_args(&args_of(&["bitty", "plugin", "list"]));
    assert!(p.plugin_word);
    assert_eq!(p.plugin_raw, vec!["list".to_string()]);
    assert_eq!(p.program, None);

    // Tokens after the word stay verbatim for the plugin parser.
    let p = parse_args(&args_of(&[
        "bitty",
        "plugin",
        "install",
        "bitty-terminal.tabs",
        "--yes",
    ]));
    assert!(p.plugin_word);
    assert_eq!(
        p.plugin_raw,
        vec![
            "install".to_string(),
            "bitty-terminal.tabs".to_string(),
            "--yes".to_string()
        ]
    );

    // Global --format before the word is stashed as the fallback.
    let p = parse_args(&args_of(&["bitty", "--format", "json", "plugin", "list"]));
    assert!(p.plugin_word);
    assert_eq!(p.plugin_format.as_deref(), Some("json"));

    let p = parse_args(&args_of(&["bitty", "--no-color", "plugin", "list"]));
    assert!(p.plugin_word);
    assert!(p.plugin_no_color);

    // Escape hatch: a program literally named `plugin`.
    let p = parse_args(&args_of(&["bitty", "--", "plugin", "list"]));
    assert!(!p.plugin_word);
    assert_eq!(p.program.as_deref(), Some("plugin"));

    // `plugin` after another word belongs to that word's args.
    let p = parse_args(&args_of(&["bitty", "list", "plugin"]));
    assert!(p.list_word);
    assert!(!p.plugin_word);

    // Help mentions the new subcommand.
    let help = help_text();
    assert!(help.contains("plugin <verb>"));
    assert!(help.contains("bitty plugin --help"));
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

// Chrome-key tests live in `chrome_keys::tests` (CTX-0233 pure move).
// Shared helper `two_pane_layout` is `chrome_keys::two_pane_layout`.
#[test]
fn spawn_spec_resolve_prefers_explicit_program() {
    // CTX-0176: explicit program wins verbatim with its tail args.
    let spec = SpawnSpec {
        program: Some("/bin/fish".to_string()),
        program_args: vec!["-l".to_string()],
        shell_env: Some("/bin/bash".to_string()),
        config_shell: Some("/bin/zsh".to_string()),
    };
    assert_eq!(
        spec.resolve(),
        ("/bin/fish".to_string(), vec!["-l".to_string()])
    );
}

#[test]
fn spawn_spec_resolve_defaults_to_configured_shell_env_then_fallback() {
    // CTX-0176/CTX-0298: no explicit program resolves exactly like
    // startup — configured `terminal.shell`, then `$SHELL`, then /bin/sh.
    let spec = SpawnSpec {
        program: None,
        program_args: vec!["-l".to_string()],
        shell_env: Some("/bin/bash".to_string()),
        config_shell: Some("/bin/zsh".to_string()),
    };
    assert_eq!(spec.resolve(), ("/bin/zsh".to_string(), Vec::new()));
    let spec = SpawnSpec {
        program: None,
        program_args: vec!["-l".to_string()],
        shell_env: Some("/bin/bash".to_string()),
        config_shell: None,
    };
    assert_eq!(spec.resolve(), ("/bin/bash".to_string(), Vec::new()));
    let spec = SpawnSpec {
        program: None,
        program_args: Vec::new(),
        shell_env: None,
        config_shell: None,
    };
    assert_eq!(spec.resolve(), ("/bin/sh".to_string(), Vec::new()));
    let spec = SpawnSpec {
        program: None,
        program_args: Vec::new(),
        shell_env: Some("   ".to_string()),
        config_shell: Some(" \n ".to_string()),
    };
    assert_eq!(spec.resolve(), ("/bin/sh".to_string(), Vec::new()));
}

#[test]
fn new_split_without_spawnable_shell_keeps_pane_with_warning() {
    // CTX-0176: spawn failure is loud but non-fatal — the split still
    // commits (layout ops stay total) with no pane session. Runs
    // everywhere: the bogus binary fails on every platform.
    use bitty_config::{ChromeAction, SplitDir};
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let spec = SpawnSpec {
        program: Some("/nonexistent-bitty-pane-shell-xyz".to_string()),
        program_args: Vec::new(),
        shell_env: None,
        config_shell: None,
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

// Live-spawn: the fresh leaf owns a real POSIX shell (`/bin/sh` has no
// Windows equivalent). `#[cfg(unix)]` keeps it off Windows CI;
// `require_pty!()` keeps the force-no-PTY simulation path. ConPTY
// coverage lives in bitty-pty/tests/spawn_windows.rs (CTX-0268);
// porting this test to a platform-neutral spawn is deferred follow-up.
#[test]
#[cfg(unix)]
fn new_split_spawns_private_shell_and_close_tears_it_down() {
    require_pty!();
    // CTX-0176 (Issue #274): the fresh leaf owns a live shell; closing
    // the leaf tears the child down with it.
    use bitty_config::{ChromeAction, SplitDir};
    let maps =
        bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
    let rt = Runtime::with_defaults().expect("must build");
    let spec = SpawnSpec {
        program: Some("/bin/sh".to_string()),
        program_args: Vec::new(),
        shell_env: None,
        config_shell: None,
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

/// Writes an executable fake shell that records its own execution in
/// `marker` and exits. Tiny and self-contained so the PTY child is
/// short-lived; used by the CTX-0298 effect tests.
#[cfg(unix)]
fn write_marker_shell(
    dir: &std::path::Path,
    name: &str,
    marker: &std::path::Path,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let body = format!(
        "#!/bin/sh\nprintf 'configured-shell-ran' > '{}'\nexit 0\n",
        marker.display()
    );
    std::fs::write(&path, body).expect("write fake shell");
    let mut perms = std::fs::metadata(&path)
        .expect("stat fake shell")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod fake shell");
    path
}

/// Bounded wait for the fake shell to write its marker.
#[cfg(unix)]
fn wait_for_marker(marker: &std::path::Path) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    marker.exists()
}

#[test]
#[cfg(unix)]
fn spawn_default_shell_uses_configured_terminal_shell() {
    // CTX-0298 effect test: `effective.terminal.shell` outranks `$SHELL`
    // at startup, proven by actually spawning the configured argv[0] and
    // observing its side effect. The injected `$SHELL` is /bin/sh, which
    // would never write this marker.
    require_pty!();
    let base =
        std::env::temp_dir().join(format!("bitty-ctx0298-configured-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("temp dir");
    let marker = base.join("ran.marker");
    let fake = write_marker_shell(&base, "configured-shell", &marker);

    let mut rt = Runtime::with_defaults().expect("must build");
    let result = spawn_default_shell(
        &mut rt,
        Some(fake.to_str().expect("utf8 fake path")),
        Some("/bin/sh"),
    );
    assert!(result.is_ok(), "configured shell spawns: {result:?}");
    assert!(rt.has_pty(), "configured shell owns the primary PTY");
    assert!(
        wait_for_marker(&marker),
        "configured shell was executed as argv[0]"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
#[cfg(unix)]
fn spawn_default_shell_skips_blank_configured_shell_for_env() {
    // CTX-0298 fail-closed effect test: a blank configured value never
    // reaches execve; the marker proves the injected `$SHELL` ran instead.
    require_pty!();
    let base = std::env::temp_dir().join(format!("bitty-ctx0298-blank-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("temp dir");
    let marker = base.join("ran.marker");
    let env_shell = write_marker_shell(&base, "env-shell", &marker);

    let mut rt = Runtime::with_defaults().expect("must build");
    let result = spawn_default_shell(
        &mut rt,
        Some("   "),
        Some(env_shell.to_str().expect("utf8 fake path")),
    );
    assert!(result.is_ok(), "env shell spawns: {result:?}");
    assert!(rt.has_pty(), "env shell owns the primary PTY");
    assert!(
        wait_for_marker(&marker),
        "blank configured shell failed closed to the injected $SHELL"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
#[cfg(unix)]
fn spawn_default_shell_falls_back_when_configured_shell_missing() {
    // CTX-0298 safe-defaults effect test: a configured shell that cannot
    // spawn retries FALLBACK_SHELL exactly like a failing `$SHELL`.
    require_pty!();
    let mut rt = Runtime::with_defaults().expect("must build");
    let result = spawn_default_shell(
        &mut rt,
        Some("/nonexistent-bitty-0298-configured-shell"),
        Some("/bin/sh"),
    );
    assert!(result.is_ok(), "fallback /bin/sh spawns: {result:?}");
    assert!(rt.has_pty(), "fallback shell owns the primary PTY");
}

// `close_last_leaf_helper_refuses` lives in `chrome_keys::tests` (CTX-0233).
