//! Bounded hidden-workspace memory diagnostics (CTX-1273).

use bitty_ipc::frame::MAX_FRAME_BYTES;
use bitty_runtime::{
    LayoutNode, MAX_WORKSPACE_MEMORY_LEAVES, MAX_WORKSPACES, Runtime, SplitAxis, View, ViewId,
    WorkspaceMemoryReport,
};

fn leaf(id: u64) -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
}

fn split(left: LayoutNode, right: LayoutNode) -> LayoutNode {
    LayoutNode::split(SplitAxis::Horizontal, 0.5, left, right)
}

fn many_leaves(count: usize) -> LayoutNode {
    (1..count as u64)
        .map(leaf)
        .reduce(split)
        .expect("test layout must contain a leaf")
}

#[test]
fn report_includes_hidden_workspace_and_is_stable() {
    let mut runtime = Runtime::with_defaults().expect("runtime");
    runtime.set_layout(split(leaf(1), leaf(2)));
    let hidden = runtime.workspace_new().expect("workspace");
    assert_eq!(hidden, 1);

    let first = runtime.workspace_memory_report();
    let second = runtime.workspace_memory_report();
    assert_eq!(first, second);
    assert_eq!(first.workspaces.len(), 2);
    assert_eq!(first.workspaces[0].leaf_views, 2);
    assert_eq!(first.workspaces[1].leaf_views, 1);
    assert_eq!(first.workspaces[0].detached_floats_total, 0);
    assert_eq!(first.workspaces[0].detached_floats_reported, 0);
    assert_eq!(first.workspaces[0].detached_floats_omitted, 0);
    assert!(!first.workspaces[0].detached_floats_truncated);
    assert_eq!(first.aggregate.workspaces, 2);
    assert_eq!(
        first.aggregate.unique_owned_bytes,
        first
            .workspaces
            .iter()
            .map(|row| row.owned_bytes)
            .sum::<usize>()
            + first.aggregate.unattributed_bytes
    );
}

#[test]
fn report_caps_workspace_and_layout_bounds() {
    let mut runtime = Runtime::with_defaults().expect("runtime");
    runtime.set_layout(many_leaves(MAX_WORKSPACE_MEMORY_LEAVES + 8));
    for _ in 1..MAX_WORKSPACES {
        runtime.workspace_new().expect("workspace within cap");
    }

    let report = runtime.workspace_memory_report();
    assert_eq!(report.workspaces.len(), MAX_WORKSPACES);
    assert!(
        report
            .workspaces
            .iter()
            .all(|row| row.leaf_views <= MAX_WORKSPACE_MEMORY_LEAVES)
    );
    assert!(report.workspaces.iter().all(|row| {
        row.detached_floats_reported + row.detached_floats_omitted == row.detached_floats_total
            && row.detached_floats_truncated == (row.detached_floats_omitted != 0)
    }));
    assert!(report.to_json().len() <= MAX_FRAME_BYTES);
}

#[test]
fn report_json_is_complete_and_contains_no_sensitive_runtime_values() {
    let runtime = Runtime::with_defaults().expect("runtime");
    let json = runtime.workspace_memory_report().to_json();
    assert!(json.starts_with('{') && json.ends_with('}'));
    assert!(json.len() <= MAX_FRAME_BYTES);
    assert!(!json.contains("cwd"));
    assert!(!json.contains("pid"));
    assert!(!json.contains('/'));
    assert!(json.contains("\"aggregate\""));
}

#[cfg(unix)]
#[test]
fn duplicate_layout_views_share_owner_accounting() {
    let mut runtime = Runtime::with_defaults().expect("runtime");
    runtime.set_layout(split(leaf(1), split(leaf(2), leaf(2))));
    runtime
        .spawn_shell_for_view(ViewId::new(2), "sh", &["-c", "sleep 30"], 80, 24)
        .expect("pane session");

    let report = runtime.workspace_memory_report();
    let row = &report.workspaces[0];
    assert_eq!(row.leaf_views, 3);
    assert_eq!(
        row.pane_sessions, 1,
        "pane owner is counted once in the row"
    );
    assert!(row.primary_state, "primary owner is attributed to its row");
    assert_eq!(report.aggregate.pane_sessions, 1);
    assert_eq!(report.aggregate.orphaned_pane_sessions, 0);
    assert_eq!(report.aggregate.primary_states, 1);
    assert_eq!(report.aggregate.owned_views, 2);

    assert!(runtime.close_pane_session(&ViewId::new(2)));
}

#[cfg(unix)]
#[test]
fn shared_owner_is_charged_once_when_seen_in_two_retained_slots() {
    let mut runtime = Runtime::with_defaults().expect("runtime");
    runtime.set_layout(split(leaf(1), leaf(2)));
    runtime
        .spawn_shell_for_view(ViewId::new(2), "sh", &["-c", "sleep 30"], 80, 24)
        .expect("pane session");
    let second = runtime.workspace_new().expect("workspace");
    runtime.set_layout(leaf(2));
    assert!(runtime.workspace_switch(0));

    let report = runtime.workspace_memory_report();
    assert_eq!(report.workspaces[0].leaf_views, 2);
    assert_eq!(report.workspaces[1].leaf_views, 1);
    assert_eq!(report.aggregate.owned_views, 2);
    assert_eq!(report.workspaces[0].owned_views, 2);
    assert_eq!(report.workspaces[0].pane_sessions, 1);
    assert_eq!(report.workspaces[1].pane_sessions, 0);
    assert!(!report.workspaces[1].primary_state);
    assert_eq!(
        report.workspaces[0].owned_bytes,
        report.aggregate.unique_owned_bytes
    );
    assert_eq!(report.workspaces[1].owned_bytes, 0);
    assert_eq!(report.workspaces[1].workspace_seq, (second + 1) as u64);
    assert!(runtime.close_pane_session(&ViewId::new(2)));
}

#[test]
fn report_marks_detached_floats_omitted_by_leaf_bound() {
    let mut runtime = Runtime::with_defaults().expect("runtime");
    runtime.set_layout(many_leaves(MAX_WORKSPACE_MEMORY_LEAVES + 2));
    runtime.float_detach(ViewId::new(1)).expect("float detach");

    let row = &runtime.workspace_memory_report().workspaces[0];
    assert_eq!(row.detached_floats_total, 1);
    assert_eq!(row.detached_floats_reported, 0);
    assert_eq!(row.detached_floats_omitted, 1);
    assert!(row.detached_floats_truncated);
    assert!(!row.layout_truncated);
}

#[test]
fn existing_terminal_view_list_semantics_remain_active_layout_only() {
    let mut runtime = Runtime::with_defaults().expect("runtime");
    runtime.set_layout(split(leaf(1), leaf(2)));
    runtime.workspace_new().expect("workspace");
    let terminals_before = runtime.live_terminal_ids();
    let panels_before = runtime.workspace_summaries();
    let report = runtime.workspace_memory_report();
    assert_eq!(runtime.live_terminal_ids(), terminals_before);
    assert_eq!(runtime.workspace_summaries(), panels_before);
    assert_eq!(report.workspaces[0].leaf_views, 2);
    assert_eq!(report.workspaces[1].leaf_views, 1);
}

#[test]
fn report_type_is_constructible_for_serialization_tests() {
    let report = WorkspaceMemoryReport {
        workspaces: Vec::new(),
        aggregate: bitty_runtime::WorkspaceMemoryAggregate {
            workspaces: 0,
            leaf_views: 0,
            pane_sessions: 0,
            orphaned_pane_sessions: 0,
            primary_states: 0,
            owned_views: 0,
            unique_owned_bytes: 0,
            unattributed_bytes: 0,
        },
    };
    assert_eq!(
        report.to_json(),
        r#"{"v":1,"workspaces":[],"aggregate":{"workspaces":0,"leaf_views":0,"pane_sessions":0,"orphaned_pane_sessions":0,"primary_states":0,"owned_views":0,"unique_owned_bytes":0,"unattributed_bytes":0}}"#
    );
}
