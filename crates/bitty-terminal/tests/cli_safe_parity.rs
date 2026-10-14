//! Headless `--safe` acceptance suite for the Small-Core gate (issue #2059,
//! CTX-1209).
//!
//! Scope: the headless-able third of the safe-mode parity gate. Everything
//! here runs without a display, instance, or GPU: a hostile `--safe` startup
//! never loads third-party code (fail closed with evidence), the core
//! command set stays present and functional under `--safe`, and headless
//! startup/frame proxies anchor the PB-1/PB-2 budgets. Headed remainder
//! (render/input parity) is STAGED as data below, never executed while the
//! owner is active.
//!
//! Fail-closed contract: a safe-mode escape is a failing assertion, never a
//! skip. No test returns early, no `#[ignore]`, no platform-conditional
//! pass-through: every assertion names the evidence it requires.
//!
//! Budget anchors (accepted targets, both currently `ABOVE_BUDGET` on real
//! hardware — hence headless proxies here, headed evidence staged):
//! - PB-1 cold startup: `<= 100 ms` p50, `<= 200 ms` p99 (default config,
//!   warm cache). Committed real-window re-measurement (`bitty-perf`
//!   `pb-rss.json`, CTX-0694): p50 `218.1 ms`, p99 `245.9 ms`.
//! - PB-2 idle memory: `<= 80 MB` RSS p50 (one window, 60 s idle, plugins
//!   disabled beyond the bundled minimum). Same artifact: `426.7 MB`.
//!
//! The headless proxies below prove bounded startup completion and a bounded
//! headless frame; they do not claim the real-window budgets.
//!
//! Coverage map (see the report for the full matrix):
//! - S1 zero third-party: hostile dev-root plugin skipped under
//!   `--safe --headless` with a skip trace and no VM activation; hostile
//!   service plugin listed non-safe (`count:1`) but absent under `--safe`
//!   (`count:0`).
//! - S2 core commands under `--safe`: doctor, list (themes/plugins/actions/
//!   keymaps/instances/services), inspect (command/config/protocol/plugin),
//!   config check/path, version, completion, shell-init, help, plugin,
//!   extension, ctl, dev, cmd, x, plus the headless smoke markers.
//! - S3 budget proxies: startup wall-clock bound, headless frame bound.
//! - S4 headed staging: the `STAGED_HEADED` table plus its guard test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Generous headless startup wall-clock bound (milliseconds).
///
/// Debug builds on small runners take seconds; the real PB-1 budget (100 ms
/// p50) is a headed real-window number owned by the staged follow-up, so
/// this proxy only fails closed on a hang or pathological slowdown.
const STARTUP_WALL_BOUND_MS: u128 = 30_000;

/// Generous headless frame bound (bytes): the software-seam RGBA buffer must
/// equal `width * height * 4` and stay far below real-window residency.
/// The default `816x544` frame is ~1.7 MB; 32 MB leaves headroom for HiDPI
/// growth while failing closed on an unbounded surface.
const FRAME_BYTES_BOUND: usize = 32 * 1024 * 1024;

/// Headed remainder, STAGED as data (never executed while the owner is
/// active). Each entry names the parity axis plus the guard that keeps it
/// out of this headless suite.
const STAGED_HEADED: &[(&str, &str)] = &[
    (
        "render parity (PTY ownership, damage, present)",
        "needs a real window/GPU; staged until a headed runner is scheduled",
    ),
    (
        "input/mouse/clipboard parity",
        "needs OS input injection; staged until a headed runner is scheduled",
    ),
    (
        "unicode/shaping parity",
        "needs font stack + GPU present; staged until a headed runner is scheduled",
    ),
    (
        "PB-1 real-window startup p50/p99",
        "needs Hyprland + release binary + 50 launches; owned by bitty-perf CTX-1205 line",
    ),
    (
        "PB-2 real-window idle RSS",
        "needs 60 s idle window + /proc sampling; owned by bitty-perf CTX-1205 line",
    ),
];

/// Fresh isolated scratch root (config + data + runtime + home).
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-safeparity-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    for sub in ["config", "data", "runtime"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch subdir");
    }
    dir
}

/// Isolated env for a headless-safe run: every mutable root points inside
/// `root`, live-instance discovery is detached, and color is off so table
/// assertions stay plain.
fn run_isolated(root: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BITTY_BIN);
    cmd.args(args)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .env_remove("BITTY_SOCKET")
        .env_remove("BITTY_INSTANCE_ID");
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Writes one hostile dev-root package: `<root>/<id>/bitty-plugin.toml`
/// plus `<root>/<id>/lua/init.lua`. Dev roots resolve as third-party
/// `local-path` provenance, so `--safe` must skip them with no VM. The
/// payload drops a marker file if plugin code ever executes: its presence
/// is a safe-mode escape and fails the test.
fn write_hostile_dev_plugin(root: &Path, id: &str, marker: &str) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"Hostile Safe Probe\"\nversion = \"0.0.1\"\n\
             description = \"safe-mode skip probe\"\n\n\
             [compat]\nbitty = \">=0.0.1\"\nplugin-api = \"^1.0\"\n\n\
             [capabilities]\nui.overlay.focus = true\n\n\
             [lazy]\ncommands = []\nevents = []\n",
        ),
    )
    .expect("write manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        format!("local f = io.open({marker:?}, 'w'); if f then f:write('pwned') f:close() end\nreturn {{}}\n"),
    )
    .expect("write init.lua");
}

/// Writes one service-provision dev-root plugin whose `init.lua` errors if a
/// VM ever loads it (discovery must stay VM-less on both paths).
fn write_service_plugin(root: &Path, id: &str) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"Test\"\nversion = \"0.1.0\"\ndescription = \"test\"\n\n\
             [compat]\nplugin-api = \"^1.0\"\n\n\
             [services.provided]\n\"calc.add\" = {{ version = \"1.0.0\", heads = [\"runtime.inspect\"] }}\n\n\
             [capabilities]\nruntime.inspect = true\n[lazy]\ncommands = [\"{id}:ping\"]\nevents = []\n",
        ),
    )
    .expect("write manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        "error(\"sentinel: safe parity must not execute plugin VM\")\nreturn {}\n",
    )
    .expect("write init.lua");
}

// ── S1: zero third-party (fail closed with evidence) ────────────────────────

#[test]
fn safe_headless_skips_hostile_dev_plugin_with_no_vm() {
    // The escape shape: a hostile `BITTY_PLUGIN_DIR` package whose payload
    // writes a marker file. Safe startup must exit 0, log the skip trace,
    // activate no VM, and leave no marker. Any deviation fails (never skips).
    let root = scratch_root("escape");
    let plugin_dir = root.join("plugdir");
    std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
    let marker = root.join("pwned-by-safe-parity");
    write_hostile_dev_plugin(
        &plugin_dir,
        "evil-safe-parity.example",
        &marker.to_string_lossy(),
    );

    let plugin_dir_str = plugin_dir.to_string_lossy().to_string();
    let output = run_isolated(
        &root,
        &["--safe", "--headless", "--log-level", "info"],
        &[("BITTY_PLUGIN_DIR", plugin_dir_str.as_str())],
    );
    let err = stderr(&output);
    let payload_ran = marker.exists();
    let _ = std::fs::remove_dir_all(&root);

    assert_eq!(
        output.status.code(),
        Some(0),
        "safe headless startup must exit 0, stderr={err:?}"
    );
    assert!(
        err.contains("skipped (--safe"),
        "safe startup must log the plugin skip trace, stderr={err:?}"
    );
    assert!(
        !err.contains(" active ("),
        "safe startup must activate no plugin VM, stderr={err:?}"
    );
    assert!(
        !payload_ran,
        "hostile plugin payload must never execute under --safe"
    );
}

#[test]
fn safe_list_services_skips_third_party_while_nonsafe_lists_it() {
    // Same third-party root through the VM-less `list services` catalog:
    // non-safe lists the granted provision (`count:1`), `--safe` reports
    // `count:0` with exit 0 and never names the provider. A safe listing
    // that names the provider is an escape and fails.
    let root = scratch_root("services");
    let plugin_dir = root.join("plugdir");
    std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
    write_service_plugin(&plugin_dir, "xuepoo.calc");
    let dev = plugin_dir.to_string_lossy().to_string();

    let listed = run_isolated(
        &root,
        &["list", "services", "--format", "json"],
        &[("BITTY_PLUGIN_DIR", &dev)],
    );
    assert_eq!(
        listed.status.code(),
        Some(0),
        "non-safe listing must exit 0, stderr={:?}",
        stderr(&listed)
    );
    let listed_text = stdout(&listed);
    assert!(
        listed_text.contains("\"count\":1"),
        "non-safe must list the granted provision, got {listed_text:?}"
    );
    assert!(
        listed_text.contains("xuepoo.calc"),
        "non-safe must name the provider, got {listed_text:?}"
    );

    let safe = run_isolated(
        &root,
        &["--safe", "list", "services", "--format", "json"],
        &[("BITTY_PLUGIN_DIR", &dev)],
    );
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        safe.status.code(),
        Some(0),
        "safe listing must exit 0, stderr={:?}",
        stderr(&safe)
    );
    let safe_text = stdout(&safe);
    assert!(
        safe_text.contains("\"count\":0"),
        "safe must skip third-party provisions, got {safe_text:?}"
    );
    assert!(
        !safe_text.contains("xuepoo.calc"),
        "safe listing must not name the skipped provider, got {safe_text:?}"
    );
}

// ── S2: core command set present and functional under --safe ────────────────

/// Runs `bitty --safe <args>` isolated and asserts exit 0 with non-empty
/// stdout containing `marker`. Failure names the args and stderr.
fn assert_safe_ok_contains(root: &Path, args: &[&str], marker: &str) {
    let output = run_isolated(root, args, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "bitty {args:?} under --safe must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains(marker),
        "bitty {args:?} must emit {marker:?}, got {text:?}"
    );
}

#[test]
fn safe_core_local_catalogs_stay_functional() {
    // Local-class enumeration: themes, plugins, actions, keymaps. Each runs
    // before config load with no VM, so `--safe` must keep them green with
    // their stable table markers.
    let root = scratch_root("catalogs");
    for (args, marker) in [
        (&["--safe", "list", "themes"][..], "bitty-dark"),
        (&["--safe", "list", "plugins"][..], "shell-integration"),
        (&["--safe", "list", "actions"][..], "core.terminal.text"),
        (&["--safe", "list", "keymaps"][..], "CHORD"),
    ] {
        assert_safe_ok_contains(&root, args, marker);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_list_instances_and_services_report_empty_success() {
    // Runtime-discovery `instances` with an isolated runtime dir (no live
    // sockets) and VM-less `services` with no fixtures: both exit 0 with
    // the frozen empty shape.
    let root = scratch_root("discovery");
    let instances = run_isolated(
        &root,
        &["--safe", "list", "instances", "--format", "json"],
        &[],
    );
    assert_eq!(
        instances.status.code(),
        Some(0),
        "safe list instances must exit 0, stderr={:?}",
        stderr(&instances)
    );
    assert!(
        stdout(&instances).contains("\"count\":0"),
        "isolated instances must be empty, got {:?}",
        stdout(&instances)
    );
    let services = run_isolated(
        &root,
        &["--safe", "list", "services", "--format", "json"],
        &[],
    );
    assert_eq!(
        services.status.code(),
        Some(0),
        "safe list services must exit 0, stderr={:?}",
        stderr(&services)
    );
    let text = stdout(&services);
    assert!(
        text.contains("\"kind\":\"services\"") && text.contains("\"count\":0"),
        "empty services envelope, got {text:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_inspect_covers_command_config_protocol_plugin() {
    // Static inspection targets resolve from built-ins and bundled
    // manifests only (no file I/O, no VM): all four stay green under
    // `--safe` with their frozen keys.
    let root = scratch_root("inspect");
    for (args, markers) in [
        (
            &[
                "--safe",
                "inspect",
                "command",
                "core.terminal.text",
                "--format",
                "json",
            ][..],
            vec!["\"id\":\"core.terminal.text\"", "\"owner\":\"core\""],
        ),
        (
            &[
                "--safe",
                "inspect",
                "config",
                "font.size",
                "--format",
                "json",
            ][..],
            vec!["\"key\":\"font.size\"", "\"source\":\"default\""],
        ),
        (
            &[
                "--safe",
                "inspect",
                "protocol",
                "kitty-graphics",
                "--format",
                "json",
            ][..],
            vec!["\"target\":\"protocol\"", "kitty-graphics"],
        ),
        (
            &[
                "--safe",
                "inspect",
                "plugin",
                "bitty-terminal.shell-integration",
                "--format",
                "json",
            ][..],
            vec!["bitty-terminal.shell-integration"],
        ),
    ] {
        let output = run_isolated(&root, args, &[]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "bitty {args:?} under --safe must exit 0, stderr={:?}",
            stderr(&output)
        );
        let text = stdout(&output);
        for marker in markers {
            assert!(
                text.contains(marker),
                "bitty {args:?} must emit {marker:?}, got {text:?}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_config_check_selects_builtin_safe_geometry() {
    // Spec rule 5: `--safe` ignores every external layer and forces the
    // `0/0/1/0/0` geometry with the opaque outline pair, sourced from
    // defaults even when a hostile explicit file is given.
    let root = scratch_root("config");
    let hostile = root.join("hostile.lua");
    std::fs::write(
        &hostile,
        "return { decoration = { gaps_in = 5, gaps_out = 7, border = 3, radius = 4 } }\n",
    )
    .expect("write hostile.lua");
    let hostile_str = hostile.to_string_lossy().to_string();
    let output = run_isolated(
        &root,
        &["--safe", "config", "check", "--config", &hostile_str],
        &[],
    );
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        output.status.code(),
        Some(0),
        "safe config check must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    for (key, value) in [
        ("decoration.gaps_in = ", "0 (default)"),
        ("decoration.gaps_out = ", "0 (default)"),
        ("decoration.border = ", "1 (default)"),
        ("decoration.radius = ", "0 (default)"),
    ] {
        let row = text
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap_or_else(|| panic!("missing row {key:?} in output:\n{text}"));
        assert_eq!(
            row.trim(),
            value,
            "--safe must force {key}, output:\n{text}"
        );
    }
}

#[test]
fn safe_config_path_stays_functional() {
    let root = scratch_root("configpath");
    assert_safe_ok_contains(&root, &["--safe", "config", "path"], "bitty");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_identity_and_shell_wiring_stay_functional() {
    // Identity (`version`), static scripts (`completion`, `shell-init`),
    // and top-level help: all local class, all safe-mode clean.
    let root = scratch_root("identity");
    assert_safe_ok_contains(&root, &["--safe", "version"], "bitty ");
    assert_safe_ok_contains(&root, &["--safe", "completion", "bash"], "bitty");
    assert_safe_ok_contains(&root, &["--safe", "shell-init", "bash"], "bitty");
    assert_safe_ok_contains(&root, &["--safe", "--help"], "Usage:");
    assert_safe_ok_contains(&root, &["--safe", "help"], "Usage:");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_management_helps_stay_functional_without_loading_code() {
    // Management surfaces never load plugin/extension code (`--help` only
    // here): the helps must print without touching a VM or instance.
    let root = scratch_root("helps");
    for (args, marker) in [
        (&["--safe", "plugin", "--help"][..], "bitty plugin"),
        (&["--safe", "extension", "--help"][..], "bitty extension"),
        (&["--safe", "ctl", "--help"][..], "bitty ctl"),
        (&["--safe", "dev", "--help"][..], "bitty dev"),
        (&["--safe", "cmd", "--help"][..], "bitty cmd"),
        (&["--safe", "x", "--help"][..], "bitty x"),
    ] {
        assert_safe_ok_contains(&root, args, marker);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_headless_smoke_reports_tick_and_layout_proof() {
    // The startup path itself: `--safe --headless` reaches the synthetic
    // smoke (tick presented), prints the surface line, and proves the
    // layout-aware composition. Missing markers fail (never skip).
    let root = scratch_root("smoke");
    let output = run_isolated(&root, &["--safe", "--headless"], &[]);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        output.status.code(),
        Some(0),
        "safe headless startup must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    for marker in [
        "headless smoke: ok",
        "surface: headless=true",
        "layout-proof: ok",
    ] {
        assert!(
            text.contains(marker),
            "safe headless must report {marker:?}, got {text:?}"
        );
    }
}

#[test]
fn safe_doctor_reports_safe_mode_posture() {
    // `doctor` never loads a plugin VM, so under `--safe` it must run to an
    // envelope and report the safe-mode plugin posture. Exit is 0 on a
    // healthy host or 1 when only host-dependent checks (fonts/PTY/GPU)
    // fail; a usage error (2) or a missing posture line fails.
    let root = scratch_root("doctor");
    let output = run_isolated(&root, &["--safe", "doctor", "--format", "json"], &[]);
    let code = output.status.code();
    assert!(
        code == Some(0) || code == Some(1),
        "safe doctor must run to completion (exit 0/1, never usage 2), got {code:?}, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("\"command\":\"doctor\""),
        "doctor must emit its envelope, got {text:?}"
    );
    assert!(
        text.contains("\"id\":\"plugins\""),
        "doctor must report the plugin check, got {text:?}"
    );
    assert!(
        text.contains("safe mode") || text.contains("no third-party plugins loaded"),
        "doctor must report the safe-mode posture, got {text:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ── S3: startup/memory budget proxies (headless) ────────────────────────────

#[test]
fn safe_headless_startup_completes_within_wall_bound() {
    // PB-1 proxy: the safe headless launch completes inside a generous
    // wall-clock bound. The accepted PB-1 budget (100 ms p50 / 200 ms p99,
    // real window) and its current measurement (p50 218.1 ms, ABOVE_BUDGET
    // per bitty-perf pb-rss.json CTX-0694) stay owned by the staged headed
    // follow-up; this proxy fails closed only on a hang or regression.
    let root = scratch_root("wall");
    let start = Instant::now();
    let output = run_isolated(&root, &["--safe", "--headless"], &[]);
    let elapsed_ms = start.elapsed().as_millis();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        output.status.code(),
        Some(0),
        "safe headless must exit 0, stderr={:?}",
        stderr(&output)
    );
    assert!(
        elapsed_ms < STARTUP_WALL_BOUND_MS,
        "safe headless startup took {elapsed_ms} ms, bound {STARTUP_WALL_BOUND_MS} ms"
    );
}

#[test]
fn safe_headless_frame_stays_within_memory_bound() {
    // PB-2 proxy: the headless software-seam frame (`extent` + `rgba_len`)
    // is exactly `w*h*4` and far below real-window residency. The accepted
    // PB-2 budget (80 MB RSS, one window, 60 s idle) and its measurement
    // (426.7 MB, ABOVE_BUDGET per bitty-perf pb-rss.json CTX-0694) stay
    // owned by the staged headed follow-up; this proxy fails closed on an
    // unbounded surface or a missing evidence line.
    let root = scratch_root("frame");
    let output = run_isolated(&root, &["--safe", "--headless"], &[]);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        output.status.code(),
        Some(0),
        "safe headless must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    let surface = text
        .lines()
        .find(|line| line.contains("surface: headless=true"))
        .unwrap_or_else(|| panic!("missing surface evidence line, got:\n{text}"));
    let extent: String = surface
        .split("extent=")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .unwrap_or_else(|| panic!("missing extent in {surface:?}"))
        .to_string();
    let (w, h) = extent
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?)))
        .unwrap_or_else(|| panic!("bad extent {extent:?}"));
    let rgba_len: usize = surface
        .split("rgba_len=")
        .nth(1)
        .and_then(|tail| tail.split([' ', ',']).next())
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or_else(|| panic!("missing rgba_len in {surface:?}"));
    assert_eq!(
        rgba_len,
        w * h * 4,
        "headless frame must be exactly w*h*4, surface={surface:?}"
    );
    assert!(
        rgba_len < FRAME_BYTES_BOUND,
        "headless frame {rgba_len} bytes exceeds bound {FRAME_BYTES_BOUND}"
    );
}

// ── S4: headed remainder staged (never run while owner active) ──────────────

#[test]
fn headed_remainder_is_staged_not_executed() {
    // Guard: the headed axes exist as staged follow-up notes with their
    // owner-active guard attached. This test runs headless; it proves the
    // staging is explicit, it never opens a window, GPU, or input path.
    assert!(
        STAGED_HEADED.len() >= 5,
        "staged headed remainder must list every parity axis"
    );
    for (axis, guard) in STAGED_HEADED {
        assert!(!axis.is_empty(), "staged axis must be named");
        assert!(
            !guard.is_empty(),
            "staged axis {axis:?} must carry its owner-active guard"
        );
    }
}
