//! CTX-1715 / bitty#2909: host-derived, confined `process.spawn { cwd =
//! "pane" }` through the real plugin runtime.
//!
//! These probes activate a real local-path package, dispatch a registered
//! command, and run the real `[tools.git]` backend (subject to the same
//! Landlock confinement as production on Linux). The pane directory is
//! supplied by a [`PaneCwdSource`]: most tests control it directly, while the
//! security-review probes feed the real host-observed model — raw `OSC 7`
//! pane bytes and a focused session-less view — so the untrusted channel and
//! the fail-closed focus case are covered without a test seam substituting a
//! path.
//!
//! The acceptance shape is the git-panel one: the child's working directory
//! is the pane's, the host process cwd is not consulted, a plugin path is
//! refused before spawn, and a missing binding/capability is a typed refusal.

#![forbid(unsafe_code)]

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, LifecycleState, PaneCwdSource, PluginRuntime, PluginRuntimeConfig,
    SnapshotSource, UnavailableSnapshot,
};
use bitty_runtime::{LayoutNode, Runtime, SplitAxis, View, ViewId};

const PROBE_ID: &str = "bitty-featured.pane-cwd-probe";

/// Source the test controls, mirroring the application's published model.
#[derive(Default)]
struct SharedPaneCwd {
    path: RefCell<Option<PathBuf>>,
}

impl SharedPaneCwd {
    fn set(&self, path: PathBuf) {
        *self.path.borrow_mut() = Some(path);
    }
}

impl PaneCwdSource for SharedPaneCwd {
    fn pane_cwd(&self) -> Option<PathBuf> {
        self.path.borrow().clone()
    }
}

/// Adapter over the real host-observed terminal model, so a test can prove
/// the resolver reads exactly what a pane program reported over `OSC 7`.
struct RuntimePaneCwd {
    runtime: Rc<RefCell<Runtime>>,
}

impl PaneCwdSource for RuntimePaneCwd {
    fn pane_cwd(&self) -> Option<PathBuf> {
        self.runtime.borrow().focused_pane_cwd()
    }
}

/// Raw `OSC 7` bytes, the untrusted channel a pane program drives.
fn osc7(dir: &Path) -> Vec<u8> {
    let url = dir.display().to_string().replace(' ', "%20");
    format!("\x1b]7;file://{url}\x07").into_bytes()
}

/// One layout leaf for focus/panel probes.
fn leaf(id: u64) -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
}

/// Unique scratch root under the platform temp dir.
fn scratch(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "bitty-pane-cwd-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ))
}

/// Real git repository with one untracked file, returning its canonical path.
fn git_repo(root: &Path, tag: &str, untracked: &str) -> PathBuf {
    std::fs::create_dir_all(root).expect("repo dir");
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(root)
        .status()
        .expect("git must be available for this probe");
    assert!(status.success(), "git init must succeed");
    std::fs::write(root.join(untracked), b"fixture").expect("untracked fixture");
    std::fs::canonicalize(root).unwrap_or_else(|error| panic!("{tag}: canonical repo: {error}"))
}

/// Write a minimal package whose `run` command reports the pane git status.
fn write_probe(root: &Path, capabilities: &[&str]) {
    let plugin = root.join("pane-cwd-probe");
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    let mut caps = String::new();
    for capability in capabilities {
        caps.push_str(&format!("\"{capability}\" = true\n"));
    }
    let tools = if capabilities.contains(&"process.spawn:git") {
        "\n[tools.git]\nrequired = true\nversion = \">=2.0\"\n"
    } else {
        ""
    };
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{PROBE_ID}"
name = "Pane cwd probe"
version = "0.1.0"
description = "CTX-1715 probe"

[compat]
plugin-api = "^1.0"

[capabilities]
{caps}{tools}
[lazy]
commands = ["{PROBE_ID}:run"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        r#"
bitty.commands.register({
  id = "run",
  title = "Run git in the pane",
  run = function()
    local ok, top = pcall(bitty.process.spawn,
      { "rev-parse", "--show-toplevel" }, { cwd = "pane" })
    if not ok then return "ERR:" .. tostring(top.code) end
    local ok2, status = pcall(bitty.process.spawn,
      { "status", "--porcelain" }, { cwd = "pane" })
    if not ok2 then return "ERR:" .. tostring(status.code) end
    return top.output .. "|" .. status.output
  end,
})
"#,
    )
    .expect("init.lua");
}

/// Activated runtime over a dev-root package.
fn runtime_with_source(root: &Path, pane: Option<Rc<dyn PaneCwdSource>>) -> PluginRuntime {
    let snapshot: Rc<dyn SnapshotSource> = Rc::new(UnavailableSnapshot);
    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.to_path_buf()],
        settings: Rc::new(EmptySettings),
        snapshot,
    });
    if let Some(source) = pane {
        runtime.set_pane_cwd_source(source);
    }
    common::install_harness_seams(&mut runtime);
    runtime.discover();
    for (id, result) in runtime.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    let id = PluginId::new(PROBE_ID).expect("valid id");
    assert_eq!(
        runtime.state(&id),
        Some(&LifecycleState::Active),
        "probe package activates"
    );
    runtime
}

/// Activated runtime with the test-controlled pane source.
fn runtime(root: &Path, pane: Option<Rc<SharedPaneCwd>>) -> PluginRuntime {
    runtime_with_source(root, pane.map(|pane| pane as Rc<dyn PaneCwdSource>))
}

fn dispatch(runtime: &mut PluginRuntime) -> common::SettledDispatch {
    let id = PluginId::new(PROBE_ID).expect("valid id");
    common::settle_dispatch(runtime, &id, "run", &[])
}

#[test]
#[cfg(target_os = "linux")]
fn pane_cwd_runs_git_in_the_pane_repository() {
    let root = scratch("repo");
    let repo = git_repo(&root.join("pane"), "repo", "untracked.txt");
    let host_cwd = std::fs::canonicalize(std::env::current_dir().expect("host cwd"))
        .unwrap_or_else(|error| panic!("canonical host cwd: {error}"));
    assert_ne!(
        repo, host_cwd,
        "the fixture must differ from the host process cwd"
    );
    write_probe(&root, &["process.spawn:git"]);
    let pane = Rc::new(SharedPaneCwd::default());
    pane.set(repo.clone());
    let mut runtime = runtime(&root, Some(pane));

    let output = dispatch(&mut runtime).expect_string("run");
    let (toplevel, status) = output.split_once('|').expect("two sections");
    assert_eq!(
        Path::new(toplevel.trim()),
        repo.as_path(),
        "git must report the pane repository as its toplevel"
    );
    assert!(
        status.contains("untracked.txt"),
        "git status must observe the pane repository's file: {status:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[cfg(target_os = "linux")]
fn pane_cwd_change_is_observed_without_a_plugin_reload() {
    let root = scratch("change");
    let first = git_repo(&root.join("first"), "first", "alpha.txt");
    let second = git_repo(&root.join("second"), "second", "beta.txt");
    write_probe(&root, &["process.spawn:git"]);
    let pane = Rc::new(SharedPaneCwd::default());
    pane.set(first);
    let mut runtime = runtime(&root, Some(pane.clone()));

    let one = dispatch(&mut runtime).expect_string("run");
    assert!(one.contains("alpha.txt"), "{one:?}");
    // No reload: only the source's observation changes.
    pane.set(second);
    let two = dispatch(&mut runtime).expect_string("run");
    assert!(two.contains("beta.txt"), "{two:?}");
    assert!(!two.contains("alpha.txt"), "{two:?}");
    let id = PluginId::new(PROBE_ID).expect("valid id");
    assert_eq!(
        runtime.state(&id),
        Some(&LifecycleState::Active),
        "the generation survives the pane change"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[cfg(target_os = "linux")]
fn dispose_after_a_pane_spawn_leaves_the_generation_and_child_settled() {
    let root = scratch("dispose");
    let repo = git_repo(&root.join("pane"), "dispose", "untracked.txt");
    write_probe(&root, &["process.spawn:git"]);
    let pane = Rc::new(SharedPaneCwd::default());
    pane.set(repo);
    let mut runtime = runtime(&root, Some(pane));
    let id = PluginId::new(PROBE_ID).expect("valid id");

    // `settle_dispatch` returns only after the supervisor observed the
    // terminal outcome and reaped the child; a live child would not have a
    // `Completed` result with exit 0 here.
    let output = dispatch(&mut runtime).expect_string("run");
    assert!(output.contains("untracked.txt"), "{output:?}");

    runtime.dispose(&id).expect("dispose");
    assert_eq!(runtime.state(&id), Some(&LifecycleState::Disposed));
    let after = runtime.dispatch_command(&id, "run", &[]);
    assert!(
        after.is_err(),
        "a disposed generation cannot start another child: {after:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn plugin_without_a_pane_binding_gets_a_typed_refusal() {
    let root = scratch("no-binding");
    std::fs::create_dir_all(root.join("pane")).expect("pane dir");
    write_probe(&root, &["process.spawn:git"]);
    let mut runtime = runtime(&root, None);

    let output = dispatch(&mut runtime).expect_string("run");
    assert_eq!(
        output, "ERR:E_SPAWN_CWD_UNAVAILABLE",
        "no binding must never fall back to the host cwd"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn plugin_without_the_spawn_capability_gets_a_capability_denial() {
    let root = scratch("no-capability");
    std::fs::create_dir_all(root.join("pane")).expect("pane dir");
    write_probe(&root, &[]);
    let pane = Rc::new(SharedPaneCwd::default());
    pane.set(root.join("pane"));
    let mut runtime = runtime(&root, Some(pane));

    let output = dispatch(&mut runtime).expect_string("run");
    assert_eq!(
        output, "ERR:E_CAPABILITY_DENIED",
        "no spawn capability means no pane probe"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[cfg(target_os = "linux")]
fn raw_osc7_pane_report_is_the_resolution_source() {
    // CTX-1715 security review: the resolver's input is the untrusted pane
    // `OSC 7` channel, fed end to end from raw terminal bytes through the
    // host-observed model into the real spawn backend. No test seam supplies
    // the path directly: the bytes a pane program emits select the directory.
    let root = scratch("osc7-source");
    let repo = git_repo(&root.join("pane"), "osc7-source", "osc7.txt");
    write_probe(&root, &["process.spawn:git"]);

    let terminal = Rc::new(RefCell::new(
        Runtime::with_defaults().expect("headless terminal"),
    ));
    terminal.borrow_mut().handle_pty_bytes(&osc7(&repo));
    assert_eq!(
        terminal.borrow().focused_pane_cwd().as_deref(),
        Some(repo.as_path()),
        "the raw OSC 7 report is the decoded resolution source"
    );

    let mut runtime =
        runtime_with_source(&root, Some(Rc::new(RuntimePaneCwd { runtime: terminal })));
    let output = dispatch(&mut runtime).expect_string("run");
    let (toplevel, status) = output.split_once('|').expect("two sections");
    assert_eq!(
        Path::new(toplevel.trim()),
        repo.as_path(),
        "the child must run in the directory the raw OSC 7 report named"
    );
    assert!(status.contains("osc7.txt"), "{status:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn focused_view_without_a_terminal_session_refuses_pane_cwd() {
    // CTX-1715 security review: a focused view with no terminal session (a
    // panel-like surface) has no pane report. The resolver must refuse rather
    // than fall back to the primary grid's report or the host process cwd.
    let root = scratch("no-session");
    let primary = root.join("primary");
    std::fs::create_dir_all(&primary).expect("primary dir");
    write_probe(&root, &["process.spawn:git"]);

    let terminal = Rc::new(RefCell::new(
        Runtime::with_defaults().expect("headless terminal"),
    ));
    {
        let mut rt = terminal.borrow_mut();
        // The primary grid reports a usable directory...
        rt.handle_pty_bytes(&osc7(&primary));
        assert!(rt.primary_cwd().is_some(), "primary report is present");
        // ...but focus moves to a session-less non-primary view: no private
        // pane session, so there is no report for the resolver to use.
        rt.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            leaf(1),
            leaf(2),
        ));
        assert!(rt.set_focus(ViewId::new(2)), "focus the session-less view");
        assert_eq!(
            rt.focused_pane_cwd(),
            None,
            "a session-less focused view has no pane cwd"
        );
    }

    let mut runtime =
        runtime_with_source(&root, Some(Rc::new(RuntimePaneCwd { runtime: terminal })));
    let output = dispatch(&mut runtime).expect_string("run");
    assert_eq!(
        output, "ERR:E_SPAWN_CWD_UNAVAILABLE",
        "no session means a typed refusal, never the primary or host cwd"
    );
    let _ = std::fs::remove_dir_all(&root);
}
