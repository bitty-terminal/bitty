//! New-pane cwd inheritance (CTX-0357).
//!
//! A shell spawned for a new pane/split starts in the working directory most
//! recently reported by the previously focused (source) pane over `OSC 7`,
//! mirroring kitty `launch --cwd=current` and ghostty
//! `split-inherit-working-directory=true`. Resolution is bounded and
//! fail-open: a missing report, a non-`file://` report, a malformed URL, or a
//! path that is no longer an existing directory leaves the child at the
//! default (process) cwd and never errors.
//!
//! Unix-only: spawning needs a POSIX shell/PTY (mirrors `pane_sessions.rs`).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

const TIMEOUT: Duration = Duration::from_secs(10);
const SHELL: &str = "/bin/sh";

fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("headless build")
}

fn leaf(id: u64) -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(id), 120, 24))
}

/// Two leaves: v1 (primary/focused) and v2.
fn two_pane_runtime() -> Runtime {
    let mut rt = runtime();
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        leaf(1),
        leaf(2),
    ));
    rt
}

/// Three leaves: v1 (primary/focused), v2 (source), v3 (new pane target).
fn three_pane_runtime() -> Runtime {
    let mut rt = runtime();
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        leaf(1),
        LayoutNode::split(SplitAxis::Vertical, 0.5, leaf(2), leaf(3)),
    ));
    rt
}

/// Four leaves: v1 primary, v2/v3/v4 split targets.
fn four_pane_runtime() -> Runtime {
    let mut rt = runtime();
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        leaf(1),
        LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            leaf(2),
            LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(3), leaf(4)),
        ),
    ));
    rt
}

fn pane_text(rt: &Runtime, view: ViewId) -> String {
    match rt.pane_snapshot(&view) {
        Some(snap) => snap.cells.iter().map(|c| c.glyph).collect(),
        None => String::new(),
    }
}

/// Polls (primary + pane pumps) and ticks until the pane grid shows
/// `needle` or the timeout expires.
fn wait_for_pane_text(rt: &mut Runtime, view: ViewId, needle: &str) -> bool {
    let deadline = std::time::Instant::now() + TIMEOUT;
    while std::time::Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        if pane_text(rt, view).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// The physical default cwd a child gets when nothing is inherited:
/// with no explicit cwd the PTY layer falls back to `$HOME`
/// (ghostty `working-directory = home` parity), else the process cwd.
fn default_cwd() -> String {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    let cwd = home
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .expect("home or process cwd");
    std::fs::canonicalize(&cwd)
        .unwrap_or(cwd)
        .display()
        .to_string()
}

/// Unique scratch directory under the platform temp dir. The name embeds the
/// test process id so parallel test binaries never collide.
fn scratch_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("bitty-ctx0357 {tag} {}", std::process::id()))
}

/// `OSC 7` bytes for `dir`, percent-encoding spaces like real shell
/// integration reports do.
fn osc7(dir: &Path) -> Vec<u8> {
    let url = dir.display().to_string().replace(' ', "%20");
    format!("\x1b]7;file://{url}\x07").into_bytes()
}

/// Spawns a shell as leaf `view`'s private shell that prints its physical
/// cwd and then stays alive, and returns the grid text once it contains
/// `needle`.
///
/// The child must outlive the drain. On macOS the kernel discards unread PTY
/// slave output when the child exits before the parent reads it (XNU
/// `S_CTTYREF`; Apple Developer Forums thread 663632, pexpect#662, Ruby bug
/// #20682), so a one-shot `/bin/pwd -P` can race the pump and leave the grid
/// blank forever — no wait window fixes that. Holding the slave open with a
/// trailing sleep makes the wait deterministic without weakening the
/// assertion: the inherited cwd is still proven by the spawned child's own
/// `pwd -P` output.
fn spawn_pwd_and_wait(rt: &mut Runtime, view: ViewId, needle: &str) -> String {
    rt.spawn_shell_for_view(view, SHELL, &["-c", "pwd -P; exec sleep 30"], 120, 24)
        .expect("spawn pwd shell for pane");
    assert!(
        wait_for_pane_text(rt, view, needle),
        "pane {view:?} never showed {needle:?}; grid={:?}",
        pane_text(rt, view)
    );
    pane_text(rt, view)
}

#[test]
fn new_pane_inherits_focused_pane_osc7_cwd() {
    let dir = scratch_dir("inherit");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let mut rt = three_pane_runtime();
    // v2 is the previously focused pane: give it a private session, then a
    // report, then focus it as the source of the next split.
    rt.spawn_shell_for_view(ViewId::new(2), SHELL, &["-c", "sleep 30"], 120, 24)
        .expect("spawn source pane shell");
    rt.handle_pane_bytes(ViewId::new(2), &osc7(&dir));
    assert!(rt.set_focus(ViewId::new(2)), "focus source pane");
    // The new pane's shell must start in the reported directory.
    let text = spawn_pwd_and_wait(&mut rt, ViewId::new(3), "bitty-ctx0357");
    assert!(
        text.contains(dir.file_name().unwrap().to_str().unwrap()),
        "new pane did not inherit the focused pane's OSC 7 cwd: {text:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_report_falls_back_to_default_cwd() {
    let mut rt = two_pane_runtime();
    assert!(rt.set_focus(ViewId::new(1)), "focus primary");
    // No OSC 7 was ever reported: the child keeps the default cwd.
    let text = spawn_pwd_and_wait(&mut rt, ViewId::new(2), "/");
    assert!(
        text.contains(&default_cwd()),
        "pane without a report must fall back to the default cwd: {text:?}"
    );
}

#[test]
fn deleted_report_directory_falls_back_without_error() {
    let dir = scratch_dir("deleted");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let mut rt = three_pane_runtime();
    rt.spawn_shell_for_view(ViewId::new(2), SHELL, &["-c", "sleep 30"], 120, 24)
        .expect("spawn source pane shell");
    rt.handle_pane_bytes(ViewId::new(2), &osc7(&dir));
    assert!(rt.set_focus(ViewId::new(2)), "focus source pane");
    std::fs::remove_dir_all(&dir).expect("delete scratch dir");
    // Stale report: spawn still succeeds, child lands in the default cwd.
    let text = spawn_pwd_and_wait(&mut rt, ViewId::new(3), "/");
    assert!(
        text.contains(&default_cwd()),
        "deleted report path must fall back to the default cwd: {text:?}"
    );
}

#[test]
fn report_pointing_at_file_falls_back_without_error() {
    let file = scratch_dir("file");
    std::fs::write(&file, b"not a directory").expect("create scratch file");
    let mut rt = three_pane_runtime();
    rt.spawn_shell_for_view(ViewId::new(2), SHELL, &["-c", "sleep 30"], 120, 24)
        .expect("spawn source pane shell");
    rt.handle_pane_bytes(ViewId::new(2), &osc7(&file));
    assert!(rt.set_focus(ViewId::new(2)), "focus source pane");
    let text = spawn_pwd_and_wait(&mut rt, ViewId::new(3), "/");
    assert!(
        text.contains(&default_cwd()),
        "non-directory report path must fall back to the default cwd: {text:?}"
    );
    let _ = std::fs::remove_file(&file);
}

#[test]
fn non_file_url_report_falls_back_without_error() {
    let mut rt = three_pane_runtime();
    rt.spawn_shell_for_view(ViewId::new(2), SHELL, &["-c", "sleep 30"], 120, 24)
        .expect("spawn source pane shell");
    // Not a `file://` URL: never trusted as a filesystem path.
    rt.handle_pane_bytes(ViewId::new(2), b"\x1b]7;kitty-shell-cwd://host/etc\x07");
    assert!(rt.set_focus(ViewId::new(2)), "focus source pane");
    let text = spawn_pwd_and_wait(&mut rt, ViewId::new(3), "/");
    assert!(
        text.contains(&default_cwd()),
        "non-file OSC 7 URL must fall back to the default cwd: {text:?}"
    );
}

#[test]
fn focused_pane_selects_the_inherited_cwd() {
    let focused_dir = scratch_dir("focused-src");
    let other_dir = scratch_dir("other-src");
    std::fs::create_dir_all(&focused_dir).expect("create focused dir");
    std::fs::create_dir_all(&other_dir).expect("create other dir");
    let mut rt = four_pane_runtime();
    // v2 reports the other dir; the primary v1 reports the focused dir.
    rt.spawn_shell_for_view(ViewId::new(2), SHELL, &["-c", "sleep 30"], 120, 24)
        .expect("spawn source pane shell");
    rt.handle_pane_bytes(ViewId::new(2), &osc7(&other_dir));
    rt.handle_pty_bytes(&osc7(&focused_dir));

    // Non-focused v2 report must NOT be picked while v1 is focused.
    assert!(rt.set_focus(ViewId::new(1)), "focus primary");
    let text = spawn_pwd_and_wait(&mut rt, ViewId::new(3), "bitty-ctx0357");
    assert!(
        text.contains(focused_dir.file_name().unwrap().to_str().unwrap()),
        "focused pane report must win over the non-focused pane: {text:?}"
    );
    assert!(
        !text.contains(other_dir.file_name().unwrap().to_str().unwrap()),
        "non-focused pane report leaked into the new pane: {text:?}"
    );

    // Focus the reporting pane: its report now wins for the next new pane.
    assert!(rt.set_focus(ViewId::new(2)), "focus reporting pane");
    let text = spawn_pwd_and_wait(&mut rt, ViewId::new(4), "bitty-ctx0357");
    assert!(
        text.contains(other_dir.file_name().unwrap().to_str().unwrap()),
        "newly focused pane report must win: {text:?}"
    );
    assert!(
        !text.contains(focused_dir.file_name().unwrap().to_str().unwrap()),
        "stale focused-source report leaked into the new pane: {text:?}"
    );
    let _ = std::fs::remove_dir_all(&focused_dir);
    let _ = std::fs::remove_dir_all(&other_dir);
}

#[test]
fn spawn_for_focused_leaf_itself_does_not_self_inherit() {
    let dir = scratch_dir("self");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let mut rt = two_pane_runtime();
    rt.spawn_shell_for_view(ViewId::new(2), SHELL, &["-c", "sleep 30"], 120, 24)
        .expect("spawn pane shell");
    rt.handle_pane_bytes(ViewId::new(2), &osc7(&dir));
    assert!(rt.set_focus(ViewId::new(2)), "focus the pane");
    // Respawning the focused leaf replaces its own session: there is no other
    // source surface, so the fresh shell starts at the default cwd.
    let text = spawn_pwd_and_wait(&mut rt, ViewId::new(2), "/");
    assert!(
        text.contains(&default_cwd()),
        "a leaf must not inherit its own replaced session report: {text:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
