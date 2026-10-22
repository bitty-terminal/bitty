//! Activation budget dimensions (bitty#2432).
//!
//! A plugin VM has one wall slice: the activation/callback wall budget and
//! the per-call host deadline are the same number, so a harness (or any
//! caller) that configures a different slice moves both. Pinning the host
//! deadline to the production 50 ms while a VM runs on a widened slice made
//! a loaded runner fail activation with
//! `E_TIMEOUT: host call exceeded its deadline` even though the callback's
//! own wall budget still had headroom (`plugin_audio`, bitty#2432).
//!
//! The probe below performs one blocking host read during `init.lua`, which
//! pins both sides of the contract deterministically:
//!
//! - on the production budget the read is refused (the deadline still bites),
//! - on the harness budget the same read completes (the deadline follows the
//!   configured slice rather than the production constant).
//!
//! The blocking read is a `std::thread::sleep`, so the outcome does not
//! depend on how fast the host schedules the probe: three slices always
//! exceed the production deadline, and one third of the harness headroom
//! always fits it.
//!
//! Headless and Windows-safe (temp dirs only, no processes or sockets).

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use bitty_lua::{BridgeError, LuaValue, RC1_WALL_CLOCK_BUDGET_MS};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, LifecycleState, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    WorkspaceAttention, WorkspaceInfo, WorkspaceSource,
};

/// Harness wall budget for the widened case.
///
/// The same 10x headroom the other runtime harnesses use for scheduler stalls
/// (bitty#1744/#1727 treatment); production keeps
/// `RC1_WALL_CLOCK_BUDGET_MS`.
const HARNESS_WALL_BUDGET_MS: u64 = 10 * RC1_WALL_CLOCK_BUDGET_MS;

/// How long the probe's host read blocks.
///
/// Three production slices: unambiguously past the default host deadline on
/// any host, and one third of [`HARNESS_WALL_BUDGET_MS`], so the widened case
/// keeps a 3x scheduling margin.
const HOST_READ_STALL_MS: u64 = 3 * RC1_WALL_CLOCK_BUDGET_MS;

const PROBE_ID: &str = "xuepoo.slowread";

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

/// Workspace source whose read blocks like a slow host backend.
struct StalledWorkspaces;

impl WorkspaceSource for StalledWorkspaces {
    fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, BridgeError> {
        std::thread::sleep(Duration::from_millis(HOST_READ_STALL_MS));
        Ok(vec![WorkspaceInfo {
            id: 1,
            name: "ws1".to_string(),
            active: true,
            panel_count: 1,
            scratchpad_count: 0,
            scratchpad_occupied: false,
            attention: WorkspaceAttention::default(),
        }])
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-activation-budget-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Write one package that reads `bitty.workspace.list()` at activation.
fn write_probe(root: &Path) {
    let plugin = root.join(PROBE_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{PROBE_ID}"
name = "Slow read"
version = "0.1.0"
description = "test"

[compat]
plugin-api = "^1.0"

[capabilities]
workspace.read = true

[lazy]
commands = ["{PROBE_ID}:probe"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        r#"
        local rows = bitty.workspace.list()
        bitty.commands.register({
          id = "probe",
          title = "Probe",
          run = function() return tostring(#rows) end,
        })
        return {}
        "#,
    )
    .expect("init");
}

fn probe_runtime(tag: &str, wall_budget_ms: Option<u64>) -> (PluginRuntime, PathBuf) {
    let root = temp_dir(tag);
    write_probe(&root);
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(NoSnapshot),
    });
    rt.set_workspace_source(Rc::new(StalledWorkspaces));
    rt.set_vm_wall_budget_ms(wall_budget_ms);
    (rt, root)
}

#[test]
fn production_budget_still_refuses_a_host_call_past_one_slice() {
    // No harness override: the production RC-1 slice applies to both the
    // activation wall and the host call, so the 3x-slice read fails closed.
    // This keeps the widened harness from hiding a real deadline removal.
    let (mut rt, root) = probe_runtime("default", None);
    assert_eq!(rt.discover().len(), 1, "probe discovers");
    let id = PluginId::new(PROBE_ID).expect("id");
    let error = rt
        .activate(&id)
        .expect_err("a host call past the slice fails");
    let text = format!("{error:?}");
    assert!(
        text.contains("E_TIMEOUT") && text.contains("host call exceeded its deadline"),
        "the production host deadline still bites, got {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn harness_wall_budget_carries_the_host_call_deadline() {
    // Same read, same host, widened slice: the host deadline is the VM's
    // configured slice, so activation completes instead of failing at the
    // production 50 ms while the callback still fits its own budget.
    let (mut rt, root) = probe_runtime("harness", Some(HARNESS_WALL_BUDGET_MS));
    assert_eq!(rt.discover().len(), 1, "probe discovers");
    let id = PluginId::new(PROBE_ID).expect("id");
    let report = rt
        .activate(&id)
        .expect("the widened slice carries the host-call deadline");
    assert_eq!(report.state, LifecycleState::Active);
    let _ = std::fs::remove_dir_all(&root);
}
