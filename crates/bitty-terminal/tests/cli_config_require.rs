//! Restricted configuration module loader: the four configuration paths
//! (`bitty#2869`, `config-lua-require-contract.md`).
//!
//! Binary-level proof that one loader serves every path, with the behavior the
//! contract names for each: startup drops the whole external layer and keeps
//! the rest, `config check` and `doctor` are strict and non-zero, and `--safe`
//! never reads external configuration (proved with a FIFO parked where the
//! read would land, so an accidental read cannot complete).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-2869-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("bitty").join("lua")).expect("module root");
    dir
}

fn write_init(root: &Path, body: &str) {
    std::fs::write(root.join("bitty").join("init.lua"), body).expect("write init.lua");
}

fn write_module(root: &Path, relative: &str, body: &str) {
    let path = root.join("bitty").join("lua").join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("module parent");
    }
    std::fs::write(path, body).expect("write module");
}

fn base_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(BITTY_BIN);
    command
        .args(args)
        .env("XDG_CONFIG_HOME", root)
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .env_remove("BITTY_LOG");
    command
}

fn run_bitty(root: &Path, args: &[&str]) -> Output {
    base_command(root, args)
        .output()
        .unwrap_or_else(|err| panic!("spawn bitty {args:?}: {err}"))
}

/// Run with a hard deadline, so a read that would block is a failure instead
/// of a hung test (the contract: "a test that hangs is a defect, not a pass").
fn run_bitty_within(root: &Path, args: &[&str], limit: Duration) -> Output {
    let mut child = base_command(root, args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("spawn bitty {args:?}: {err}"));
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("bitty {args:?} did not finish inside {limit:?}; a read blocked");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => panic!("wait on bitty {args:?}: {err}"),
        }
    }
    child.wait_with_output().expect("collect output")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_reached_a_tick(output: &Output, context: &str) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{context} must exit 0, stdout={:?} stderr={:?}",
        stdout(output),
        stderr(output)
    );
    assert!(
        stdout(output).contains("tick presented"),
        "{context} must reach a presented tick, stdout={:?} stderr={:?}",
        stdout(output),
        stderr(output)
    );
}

// ── startup: whole-layer salvage ────────────────────────────────────────────

#[test]
fn startup_drops_the_layer_when_a_module_is_missing_and_keeps_the_rest() {
    let root = scratch_root("startup-drop");
    write_module(&root, "options.lua", "return { theme = 'bitty-dark' }");
    write_init(
        &root,
        "local opts = require('options')\nreturn { theme = opts.theme, window = { padding = require('absent').padding } }\n",
    );

    let startup = run_bitty(&root, &["--headless"]);
    assert_reached_a_tick(&startup, "startup with a missing module");
    let text = stderr(&startup);
    assert!(
        text.contains("E_CONFIG_MODULE_NOT_FOUND"),
        "startup must name the typed code: {text}"
    );
    assert!(
        text.contains("module_spec=\"absent\""),
        "startup must name the refused spec: {text}"
    );
    assert!(
        text.contains("ignored; using defaults and remaining layers"),
        "startup must state the whole-layer fallback: {text}"
    );
    // Redaction: the *entry* may be named (every shipped configuration
    // diagnostic names it), but never a resolved module path, the canonical
    // root, or an identity tuple.
    assert!(!text.contains("lua/absent.lua"), "{text}");
    assert!(!text.contains("dev="), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn startup_loads_values_from_a_module_graph() {
    let root = scratch_root("startup-load");
    write_module(
        &root,
        "config/options.lua",
        "return { theme = 'tokyo-night' }",
    );
    write_init(
        &root,
        "return { theme = require('config.options').theme }\n",
    );

    let startup = run_bitty(&root, &["--headless"]);
    assert_reached_a_tick(&startup, "startup from a module graph");
    let text = stderr(&startup);
    assert!(
        !text.contains("E_CONFIG_MODULE_"),
        "a clean module graph must not report a refusal: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ── strict `config check` / `doctor` ────────────────────────────────────────

#[test]
fn config_check_is_strict_and_non_zero_for_a_module_failure() {
    let root = scratch_root("check");
    write_module(&root, "ok.lua", "return { theme = 'bitty-dark' }");
    write_init(&root, "require('ok')\nreturn require('missing.module')\n");

    let check = run_bitty(&root, &["config", "check"]);
    assert_eq!(
        check.status.code(),
        Some(2),
        "config check must stay strict, stdout={:?} stderr={:?}",
        stdout(&check),
        stderr(&check)
    );
    let text = format!("{}{}", stdout(&check), stderr(&check));
    assert!(text.contains("E_CONFIG_MODULE_NOT_FOUND"), "{text}");
    assert!(text.contains("module_spec=\"missing.module\""), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn config_check_reports_a_cycle_with_the_chain() {
    let root = scratch_root("check-cycle");
    write_module(&root, "a.lua", "return require('b')");
    write_module(&root, "b.lua", "return require('a')");
    write_init(&root, "return require('a')\n");

    let check = run_bitty(&root, &["config", "check"]);
    assert_eq!(check.status.code(), Some(2), "{}", stdout(&check));
    let text = format!("{}{}", stdout(&check), stderr(&check));
    assert!(text.contains("E_CONFIG_MODULE_CYCLE"), "{text}");
    assert!(text.contains("a -> b -> a"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn doctor_reports_a_module_failure_and_exits_non_zero() {
    let root = scratch_root("doctor");
    write_init(&root, "return require('nope')\n");

    let doctor = run_bitty(&root, &["doctor"]);
    assert_ne!(
        doctor.status.code(),
        Some(0),
        "doctor must exit non-zero for a module failure, stdout={:?} stderr={:?}",
        stdout(&doctor),
        stderr(&doctor)
    );
    let text = format!("{}{}", stdout(&doctor), stderr(&doctor));
    assert!(text.contains("E_CONFIG_MODULE_NOT_FOUND"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

// ── `--safe`: no external read ──────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn safe_mode_never_reads_external_configuration() {
    let root = scratch_root("safe");
    // A hostile tree, and a FIFO parked where the entry read would land: a
    // safe-mode path that touched external configuration could not finish.
    write_module(&root, "hostile.lua", "return require('also-hostile')");
    write_init(&root, "return require('hostile')\n");
    let fifo = root.join("bitty").join("fifo.lua");
    //  is already the crate's safe Unix wrapper dependency.
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).expect("mkfifo");

    // `--config <fifo>` is the most direct read the loader could attempt.
    let fifo_text = fifo.to_string_lossy().into_owned();
    let startup = run_bitty_within(
        &root,
        &["--headless", "--safe", "--config", &fifo_text],
        Duration::from_secs(60),
    );
    assert_reached_a_tick(&startup, "--safe startup with a FIFO entry");
    let text = stderr(&startup);
    assert!(
        !text.contains("E_CONFIG_MODULE_"),
        "--safe must not reach a loader: {text}"
    );

    // Same for the strict diagnostics path: it uses the built-in configuration
    // and reports nothing from the hostile tree.
    let check = run_bitty_within(
        &root,
        &["config", "check", "--safe", "--config", &fifo_text],
        Duration::from_secs(60),
    );
    assert_eq!(
        check.status.code(),
        Some(0),
        "config check --safe must accept the built-in config, stdout={:?} stderr={:?}",
        stdout(&check),
        stderr(&check)
    );
    let check_text = format!("{}{}", stdout(&check), stderr(&check));
    assert!(
        !check_text.contains("hostile") && !check_text.contains("E_CONFIG_MODULE_"),
        "--safe must not consult the hostile tree: {check_text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ── generated templates stay loader-free ────────────────────────────────────

#[test]
fn init_templates_stay_self_contained_data() {
    let root = scratch_root("init");
    let init = run_bitty(&root, &["init", "--yes", "--force"]);
    assert_eq!(
        init.status.code(),
        Some(0),
        "bitty init must succeed, stdout={:?} stderr={:?}",
        stdout(&init),
        stderr(&init)
    );
    let body = std::fs::read_to_string(root.join("bitty").join("init.lua")).expect("init.lua");
    assert!(
        !body.contains("require("),
        "a generated template must not depend on the module loader"
    );
    let _ = std::fs::remove_dir_all(&root);
}
