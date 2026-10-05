#![forbid(unsafe_code)]
//! CTX-0979: Core draws no workspace display (Hyprland-style).
//!
//! The former workspace bar chrome band tests (issue #1349 drawn row;
//! CTX-0873 / #1431 Core-reserved band) are deleted. Core reserves no bar
//! band: the layout container is the full window grid with one workspace,
//! with many workspaces, and across resizes. Workspace state is
//! memory-only; the bar plugin owns presentation via the query commands
//! (`workspace_names`, `workspace_summaries`, `workspace_count`).

use bitty_runtime::{Runtime, RuntimeConfig};

fn runtime_with_workspaces(count: usize) -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    for _ in 1..count.max(1) {
        rt.workspace_new().expect("new workspace");
    }
    rt
}

#[test]
fn core_reserves_no_bar_band_with_one_workspace() {
    let rt = runtime_with_workspaces(1);
    assert_eq!(rt.workspace_count(), 1);
    assert_eq!(rt.workspace_names(), vec![String::from("ws1")]);
    // No Core bar: the layout container is the full window grid.
    assert_eq!(rt.container(), rt.window_cells());
}

#[test]
fn core_reserves_no_bar_band_with_many_workspaces() {
    let rt = runtime_with_workspaces(3);
    assert_eq!(rt.workspace_count(), 3);
    assert_eq!(rt.active_workspace_index(), 2);
    assert_eq!(rt.container(), rt.window_cells());
}

#[test]
fn workspace_queries_serve_the_bar_plugin() {
    let mut rt = runtime_with_workspaces(1);
    rt.workspace_new().expect("ws2");
    assert_eq!(
        rt.workspace_names(),
        vec![String::from("ws1"), String::from("ws2")]
    );
    let summaries = rt.workspace_summaries();
    assert_eq!(summaries.len(), 2);
    assert!(!summaries[0].active);
    assert!(summaries[1].active);
}
