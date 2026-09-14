#![cfg(unix)]
#![forbid(unsafe_code)]
//! Env-gated live compatibility probes (CTX-0404).
//!
//! These tests exercise real local tools through the owned PTY
//! (`bitty-pty`), capture the emitted bytes, and replay them through the
//! compat-lab harness (`parse_bounded` -> `State::check_invariants`) so the
//! M1/M2 matrix can record *verified locally* rows honestly.
//!
//! - **Disabled by default.** CI runs this file as a no-op: without
//!   `BITTY_COMPAT_LIVE=1` every test prints a skip JSON line and returns.
//! - **Bounded and safe.** Every child runs under one PTY with a hard
//!   `SCENARIO_DEADLINE`; the tmux probe uses a private `-L` socket and always
//!   kills its own server via a drop guard. No network dialling, no display,
//!   no agent spawning, no unbounded sleep.
//! - **Machine-readable.** Each scenario prints one JSON line to stdout:
//!   `{"scenario": ..., "status": "verified-local"|"skipped", ...}`. Run with
//!   `--nocapture` to collect them (see `scripts/compat-local.sh`).
//!
//! Enable:
//! ```text
//! BITTY_COMPAT_LIVE=1 cargo test -p bitty-compat-lab --test live_compat -- --nocapture --test-threads=1
//! ```

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use bitty_compat_lab::report::tool_path;
use bitty_pty::PtyBuilder;

/// Idle poll between bounded reads.
const IDLE_READ: Duration = Duration::from_millis(250);
/// Hard deadline for one scenario, from spawn to reap.
const SCENARIO_DEADLINE: Duration = Duration::from_secs(20);
/// Capture cap; the PTY pump itself is bounded at 128 KiB.
const MAX_CAPTURE_BYTES: usize = 512 * 1024;
/// Bytes of a capture replayed through the compat-lab harness (< 8 KiB bound).
const REPLAY_BYTES: usize = 8 * 1024;

fn live_enabled() -> bool {
    matches!(std::env::var("BITTY_COMPAT_LIVE").as_deref(), Ok("1"))
}

fn emit_skip(scenario: &str, reason: &str) {
    println!("{{\"scenario\": \"{scenario}\", \"status\": \"skipped\", \"reason\": \"{reason}\"}}");
    eprintln!("SKIP live_compat/{scenario}: {reason}");
}

/// One staged interaction step.
enum Stage {
    /// Write bytes after the given delay from spawn.
    Write(Duration, &'static [u8]),
    /// Resize the PTY after the given delay from spawn.
    Resize(Duration, u16, u16),
}

struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
    exited: bool,
    exit_code: Option<u32>,
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// Spawn `program` under an 80x24 PTY, apply `stages`, read until exit or the
/// scenario deadline, and always reap the child.
fn capture(program: &Path, args: &[&str], stages: &[Stage]) -> Capture {
    let mut pty = PtyBuilder::new(program.as_os_str())
        .args(args.iter().map(|arg| arg.to_string()))
        .size(80, 24)
        .spawn()
        .unwrap_or_else(|err| panic!("spawn {}: {err}", program.display()));
    let mut writer = pty.take_writer().expect("pty writer half");
    let reader = pty.take_reader().expect("pty reader half");

    let start = Instant::now();
    let deadline = start + SCENARIO_DEADLINE;
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut exited = false;
    let mut next_stage = 0usize;

    let append = |bytes: &mut Vec<u8>, truncated: &mut bool, chunk: &[u8]| {
        if bytes.len() >= MAX_CAPTURE_BYTES {
            *truncated = true;
            return;
        }
        let room = MAX_CAPTURE_BYTES - bytes.len();
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            *truncated = true;
        } else {
            bytes.extend_from_slice(chunk);
        }
    };

    while Instant::now() < deadline {
        while next_stage < stages.len() {
            let due = match &stages[next_stage] {
                Stage::Write(delay, _) | Stage::Resize(delay, _, _) => *delay,
            };
            if start.elapsed() < due {
                break;
            }
            match &stages[next_stage] {
                Stage::Write(_, payload) => {
                    let _ = writer.write_all(payload);
                    let _ = writer.flush();
                }
                Stage::Resize(_, cols, rows) => {
                    let _ = pty.resize(*cols, *rows);
                }
            }
            next_stage += 1;
        }
        match reader.recv_timeout(IDLE_READ) {
            Ok(Some(chunk)) => append(&mut bytes, &mut truncated, &chunk),
            Ok(None) => {
                exited = true;
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                exited = true;
                break;
            }
        }
    }

    // Send any not-yet-due stages and give the child a short grace window.
    while next_stage < stages.len() {
        match &stages[next_stage] {
            Stage::Write(_, payload) => {
                let _ = writer.write_all(payload);
                let _ = writer.flush();
            }
            Stage::Resize(_, cols, rows) => {
                let _ = pty.resize(*cols, *rows);
            }
        }
        next_stage += 1;
    }
    let grace = Instant::now() + Duration::from_secs(2);
    while Instant::now() < grace && !exited {
        match reader.recv_timeout(IDLE_READ) {
            Ok(Some(chunk)) => append(&mut bytes, &mut truncated, &chunk),
            Ok(None) | Err(RecvTimeoutError::Disconnected) => exited = true,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }

    if !exited {
        // The scenario did not finish in time: kill and reap the child. The
        // reader pump ends at PTY EOF; never leave a live child behind.
        let _ = pty.kill();
    }
    let _ = writer.flush();
    let status = pty.wait_timeout(Duration::from_secs(2)).ok().flatten();
    let _ = reader.join();
    Capture {
        bytes,
        truncated,
        exited,
        exit_code: status.map(|s| s.code()),
    }
}

/// Replay a bounded slice of `capture` through the compat-lab harness and
/// print the verified-local JSON line. Panics on invariant violations.
fn emit_verified(scenario: &str, tool: &Path, capture: &Capture, extra: &str) -> usize {
    let replay_len = capture.bytes.len().min(REPLAY_BYTES);
    let actions = bitty_compat_lab::parse_bounded(&capture.bytes[..replay_len]);
    let mut state = bitty_term_state::State::new();
    for action in &actions {
        state.apply(action);
    }
    state
        .check_invariants()
        .unwrap_or_else(|err| panic!("live {scenario}: invariant violation: {err:?}"));
    let hash = state.state_hash();
    let tool = tool
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let exit_code = match capture.exit_code {
        Some(code) => code.to_string(),
        None => "null".to_string(),
    };
    println!(
        "{{\"scenario\": \"{scenario}\", \"status\": \"verified-local\", \"tool\": \"{tool}\", \
         \"captured_bytes\": {}, \"replayed_bytes\": {replay_len}, \"actions\": {}, \
         \"state_hash\": \"{hash:016x}\", \"truncated\": {}, \"exited\": {}, \
         \"exit_code\": {exit_code}, {extra}}}",
        capture.bytes.len(),
        actions.len(),
        capture.truncated,
        capture.exited
    );
    actions.len()
}

/// Private tmux server socket; the guard kills only this server on drop.
struct TmuxServer {
    socket: String,
}

impl TmuxServer {
    fn new() -> Self {
        Self {
            socket: format!("bitty-compat-{}", std::process::id()),
        }
    }

    fn run(&self, args: &[&str]) {
        let _ = std::process::Command::new("tmux")
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        self.run(&["kill-server"]);
    }
}

#[test]
fn live_shell_probe() {
    if !live_enabled() {
        return emit_skip("shell", "BITTY_COMPAT_LIVE is not 1");
    }
    let Some(shell) = tool_path("bash") else {
        return emit_skip("shell", "bash not found on PATH");
    };
    let capture = capture(
        &shell,
        &["--noprofile", "--norc", "-i"],
        &[
            Stage::Write(Duration::from_millis(400), b"printf 'bitty-live\\n'\r"),
            Stage::Write(Duration::from_millis(1200), b"exit 7\r"),
        ],
    );
    let text = capture.text();
    assert!(
        text.contains("bitty-live"),
        "shell echo missing from capture ({} bytes)",
        capture.bytes.len()
    );
    assert_eq!(
        capture.exit_code,
        Some(7),
        "interactive shell exit status not propagated"
    );
    emit_verified(
        "shell",
        &shell,
        &capture,
        "\"scope\": \"startup-echo-exit\"",
    );
}

#[test]
fn live_tmux_probe() {
    if !live_enabled() {
        return emit_skip("tmux", "BITTY_COMPAT_LIVE is not 1");
    }
    let Some(tmux) = tool_path("tmux") else {
        return emit_skip("tmux", "tmux not found on PATH");
    };
    let server = TmuxServer::new();
    // Prime the server so the attach client starts on a ready session.
    server.run(&["new-session", "-d"]);
    let socket = server.socket.clone();
    let capture = capture(
        &tmux,
        &["-L", &socket, "-f", "/dev/null", "attach-session"],
        &[
            Stage::Write(Duration::from_millis(800), b"printf 'bitty-tmux-live'\r"),
            Stage::Write(Duration::from_millis(2500), b"\x02d"),
        ],
    );
    let text = capture.text();
    assert!(
        text.contains("bitty-tmux-live"),
        "tmux capture missing typed output ({} bytes)",
        capture.bytes.len()
    );
    emit_verified("tmux", &tmux, &capture, "\"scope\": \"render-capture\"");
}

#[test]
fn live_nvim_probe() {
    if !live_enabled() {
        return emit_skip("nvim", "BITTY_COMPAT_LIVE is not 1");
    }
    let Some(nvim) = tool_path("nvim") else {
        return emit_skip("nvim", "nvim not found on PATH");
    };
    let capture = capture(
        &nvim,
        &["--clean"],
        &[Stage::Write(
            Duration::from_millis(600),
            b"ihello from bitty\x1b:qa!\r",
        )],
    );
    let text = capture.text();
    assert!(
        text.contains("hello from bitty"),
        "nvim capture missing typed text ({} bytes)",
        capture.bytes.len()
    );
    emit_verified("nvim", &nvim, &capture, "\"scope\": \"render-capture\"");
}

#[test]
fn live_fzf_probe() {
    if !live_enabled() {
        return emit_skip("fzf", "BITTY_COMPAT_LIVE is not 1");
    }
    let Some(fzf) = tool_path("fzf") else {
        return emit_skip("fzf", "fzf not found on PATH");
    };
    let Some(sh) = tool_path("sh") else {
        return emit_skip("fzf", "sh not found on PATH");
    };
    let capture = capture(
        &sh,
        &[
            "-c",
            "printf 'alpha\\nbeta\\ngamma\\n' | fzf --no-mouse --no-color",
        ],
        &[Stage::Write(Duration::from_millis(1500), b"\x1b")],
    );
    let text = capture.text();
    assert!(
        text.contains("alpha") && text.contains("gamma"),
        "fzf capture missing candidates ({} bytes)",
        capture.bytes.len()
    );
    emit_verified(
        "fzf",
        &fzf,
        &capture,
        "\"scope\": \"render-capture\", \"driver\": \"sh\"",
    );
}

#[test]
fn live_htop_probe() {
    if !live_enabled() {
        return emit_skip("htop", "BITTY_COMPAT_LIVE is not 1");
    }
    let Some(htop) = tool_path("htop") else {
        return emit_skip("htop", "htop not found on PATH");
    };
    let capture = capture(
        &htop,
        &["--no-color"],
        &[Stage::Write(Duration::from_millis(1500), b"q")],
    );
    let text = capture.text();
    assert!(
        text.contains("CPU") || text.contains("Mem") || text.contains("Tasks"),
        "htop capture missing process table ({} bytes)",
        capture.bytes.len()
    );
    emit_verified("htop", &htop, &capture, "\"scope\": \"render-capture\"");
}

/// 2x2 RGBA PNG (red/green/blue/yellow) used as the chafa input.
const PNG_2X2: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xb6, 0x0d,
    0x24, 0x00, 0x00, 0x00, 0x14, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x0c, 0x81, 0x34, 0x10, 0x30, 0xfc, 0x07, 0x00, 0x47, 0xca, 0x08, 0xf8, 0x5b, 0x9a, 0xa4,
    0xbe, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[test]
fn live_chafa_probe() {
    if !live_enabled() {
        return emit_skip("chafa", "BITTY_COMPAT_LIVE is not 1");
    }
    let Some(chafa) = tool_path("chafa") else {
        return emit_skip("chafa", "chafa not found on PATH");
    };
    let image = std::env::temp_dir().join(format!("bitty-compat-{}.png", std::process::id()));
    std::fs::write(&image, PNG_2X2).expect("write temp png");
    let image_arg = image.to_string_lossy().into_owned();
    let capture = capture(
        &chafa,
        &["--format", "kitty", "--size", "8x8", &image_arg],
        &[],
    );
    let _ = std::fs::remove_file(&image);
    assert!(
        capture.bytes.windows(3).any(|window| window == b"\x1b_G"),
        "chafa capture missing kitty APC G ({} bytes)",
        capture.bytes.len()
    );
    let replay_len = capture.bytes.len().min(REPLAY_BYTES);
    let actions = bitty_compat_lab::parse_bounded(&capture.bytes[..replay_len]);
    let graphics = actions
        .iter()
        .filter(|action| matches!(action, bitty_vt::TerminalAction::KittyGraphics { .. }))
        .count();
    assert!(
        graphics >= 1,
        "chafa capture produced no KittyGraphics action ({} actions)",
        actions.len()
    );
    let actions_len = emit_verified(
        "chafa",
        &chafa,
        &capture,
        "\"scope\": \"graphics-admission\"",
    );
    assert_eq!(actions_len, actions.len());
}

#[test]
fn live_resize_probe() {
    if !live_enabled() {
        return emit_skip("resize", "BITTY_COMPAT_LIVE is not 1");
    }
    let Some(shell) = tool_path("bash") else {
        return emit_skip("resize", "bash not found on PATH");
    };
    let capture = capture(
        &shell,
        &["--noprofile", "--norc", "-i"],
        &[
            Stage::Write(Duration::from_millis(400), b"stty size\r"),
            Stage::Resize(Duration::from_millis(1200), 120, 40),
            Stage::Write(Duration::from_millis(1600), b"stty size\r"),
            Stage::Write(Duration::from_millis(2600), b"exit\r"),
        ],
    );
    let text = capture.text();
    assert!(
        text.contains("24 80"),
        "initial PTY size not observed ({} bytes)",
        capture.bytes.len()
    );
    assert!(
        text.contains("40 120"),
        "resized PTY size not observed ({} bytes)",
        capture.bytes.len()
    );
    emit_verified("resize", &shell, &capture, "\"scope\": \"sigwinch-resize\"");
}
