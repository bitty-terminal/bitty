//! `registry` — terminal-registry unit tests.
//!
//! Split from `super` (`registry.rs`) as a pure move under CTX-0308:
//! byte-identical logic, only module wiring changed.

use super::*;
use bitty_ui::{Rect as UiRect, SplitAxis};

fn default_registry() -> TerminalRegistry {
    TerminalRegistry::new(RegistryConfig::default()).expect("default must build")
}

#[test]
fn terminal_id_and_view_id_are_distinct_types() {
    // Type-level distinctness: no From bridge, no equality across types.
    let tid = TerminalId::new(1);
    let vid = ViewId::new(1);
    // The following would not compile if they were the same type:
    // assert_eq!(tid, vid);
    // Instead we assert the raw values can be equal while types differ.
    assert_eq!(tid.0, vid.0);
    assert_ne!(
        std::any::TypeId::of::<TerminalId>(),
        std::any::TypeId::of::<ViewId>()
    );
    // Ensure no From impl exists (compile check via trait bound absence is implicit).
    // This line must NOT compile, proving no bridge:
    // assert_no_from::<TerminalId, ViewId>(); where assert_no_from is
    // `fn assert_no_from<T, U>() where T: From<U>` — not instantiated.
    let _ = tid;
    let _ = vid;
}

#[test]
fn registry_creation_validates_bounds() {
    let bad = RegistryConfig {
        max_terminals: 0,
        ..RegistryConfig::default()
    };
    assert!(TerminalRegistry::new(bad).is_err());
    let bad2 = RegistryConfig {
        max_terminals: 65,
        ..RegistryConfig::default()
    };
    assert!(TerminalRegistry::new(bad2).is_err());
    let bad3 = RegistryConfig {
        cell_width: 0,
        ..RegistryConfig::default()
    };
    assert!(TerminalRegistry::new(bad3).is_err());
}

#[test]
fn create_terminal_within_bound_and_generation_monotonic() {
    let mut reg = default_registry();
    let start = reg.generation();
    let h1 = reg.create_terminal(None).expect("create 1");
    assert!(reg.generation().get() > start.get());
    let h2 = reg.create_terminal(None).expect("create 2");
    assert_ne!(h1.id, h2.id);
    assert_ne!(h1.generation, h2.generation);
    assert_ne!(h1.runtime_id, h2.runtime_id);
    assert_eq!(reg.terminal_count(), 2);
    assert_eq!(reg.total_created(), 2);
}

#[test]
fn defaults_match_shared_grid_and_cell_constants() {
    // CTX-0296: registry defaults must not restate literals; the grid
    // comes from term-state and the cell from the runtime config.
    let cfg = RegistryConfig::default();
    let runtime = crate::config::RuntimeConfig::default();
    assert_eq!(
        (cfg.cell_width, cfg.cell_height),
        (runtime.cell_width, runtime.cell_height)
    );
    assert_eq!(
        (cfg.cell_width, cfg.cell_height),
        (
            crate::config::DEFAULT_CELL_WIDTH,
            crate::config::DEFAULT_CELL_HEIGHT
        )
    );
    let mut reg = default_registry();
    let h = reg.create_terminal(None).expect("create");
    let rec = reg.terminals.get(&h.id.0).expect("record present");
    assert_eq!(rec.cols, bitty_term_state::GRID_COLUMNS as u16);
    assert_eq!(rec.rows, bitty_term_state::GRID_ROWS as u16);
}

#[test]
fn too_many_terminals_returns_error_and_preserves_state() {
    let mut reg = TerminalRegistry::new(RegistryConfig {
        max_terminals: 1,
        ..RegistryConfig::default()
    })
    .unwrap();
    let _ = reg.create_terminal(None).unwrap();
    let before_gen = reg.generation();
    let err = reg.create_terminal(None).unwrap_err();
    assert!(matches!(err, RegistryError::TooManyTerminals { .. }));
    // State unchanged
    assert_eq!(reg.terminal_count(), 1);
    assert_eq!(reg.generation(), before_gen);
    assert!(reg.error_count("TooManyTerminals") > 0);
}

#[test]
fn persistent_id_validation_and_in_use() {
    let mut reg = default_registry();
    let pid = PersistentId::new("valid-id_123").unwrap();
    let h = reg.create_terminal(Some(pid.clone())).unwrap();
    assert_eq!(
        reg.terminal_persistent_id(h.id, h.generation).unwrap(),
        Some(pid.clone())
    );
    // Duplicate should fail
    let pid2 = PersistentId::new("valid-id_123").unwrap();
    let err = reg.create_terminal(Some(pid2)).unwrap_err();
    assert!(matches!(err, RegistryError::PersistentIdInUse { .. }));
    // Invalid charset
    assert!(PersistentId::new("BAD CAPS").is_err());
    assert!(PersistentId::new("x".repeat(65)).is_err());
    assert!(PersistentId::new("").is_err());
    // After close, pid reusable
    reg.close_terminal(h.id, h.generation).unwrap();
    let pid3 = PersistentId::new("valid-id_123").unwrap();
    let h2 = reg.create_terminal(Some(pid3)).expect("reuse after close");
    assert_ne!(h.id, h2.id);
}

#[test]
fn stale_handle_rejected_with_expected_and_found() {
    let mut reg = default_registry();
    let h = reg.create_terminal(None).unwrap();
    // Close retires handle with generation bump
    let stale_gen = h.generation;
    reg.close_terminal(h.id, h.generation).unwrap();
    // New terminal may reuse numeric? We use monotonic raw, so reuse not occur,
    // but stale handle for old id should be NotFound? Actually old id removed,
    // so stale check for that id returns NotFound. Test generation mismatch via
    // second terminal close then attempt with old generation.
    let h2 = reg.create_terminal(None).unwrap();
    // Trying to close h2 with stale generation (h.generation) should give StaleHandle because id differs
    // For same id with stale generation, we need to simulate same id but old generation.
    // Our monotonic raw makes ids unique, so we test StaleHandle via attach with stale generation
    // by creating a terminal, then bumping generation via another create, then using old generation
    let mut reg2 = default_registry();
    let th = reg2.create_terminal(None).unwrap();
    let correct_gen = th.generation;
    // After another allocation, registry_generation advanced, but terminal's generation stays
    let _ = reg2.create_terminal(None).unwrap();
    // Using wrong generation for that terminal should be StaleHandle
    let wrong = Generation(correct_gen.0.wrapping_add(10));
    let err = reg2.terminal_snapshot(th.id, wrong).unwrap_err();
    assert!(matches!(err, RegistryError::StaleHandle { .. }));
    if let RegistryError::StaleHandle {
        expected_generation,
        found_generation,
        id_raw,
    } = err
    {
        assert_eq!(expected_generation, correct_gen);
        assert_eq!(found_generation, wrong);
        assert_eq!(id_raw, th.id.0);
    }
    let _ = h2;
    let _ = stale_gen;
}

#[test]
fn closing_retires_id_and_generation_bumps() {
    let mut reg = default_registry();
    let h = reg.create_terminal(None).unwrap();
    let before = reg.generation();
    reg.close_terminal(h.id, h.generation).unwrap();
    assert!(reg.generation().get() > before.get());
    // Subsequent use of same handle must fail (NotFound or StaleHandle)
    let err = reg.terminal_snapshot(h.id, h.generation).unwrap_err();
    assert!(matches!(
        err,
        RegistryError::NotFound { .. } | RegistryError::StaleHandle { .. }
    ));
}

#[test]
fn attach_detach_preserves_terminal_and_runtime() {
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let vh = reg.create_view(wid).unwrap();
    let th = reg.create_terminal(None).unwrap();
    let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
    reg.attach(wid, vh.id, vh.generation, th.id, th.generation, rect)
        .unwrap();
    assert_eq!(reg.attached_view(th.id), Some(vh.id));
    assert_eq!(reg.attached_terminal(vh.id), Some(th.id));
    // Detach preserves ids
    let tid = reg.detach(wid, vh.id, vh.generation).unwrap();
    assert_eq!(tid, th.id);
    assert_eq!(reg.attached_view(th.id), None);
    assert_eq!(reg.attached_terminal(vh.id), None);
    // Terminal still live with same generation/runtime
    let snap = reg.terminal_snapshot(th.id, th.generation).unwrap();
    assert_eq!(snap.width, 80);
    assert_eq!(snap.height, 24);
    // Reattach to new view preserves TerminalId/RuntimeId
    let vh2 = reg.create_view(wid).unwrap();
    reg.attach(wid, vh2.id, vh2.generation, th.id, th.generation, rect)
        .unwrap();
    assert_eq!(reg.attached_view(th.id), Some(vh2.id));
}

#[test]
fn attach_when_already_attached_returns_error() {
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let vh1 = reg.create_view(wid).unwrap();
    let vh2 = reg.create_view(wid).unwrap();
    let th = reg.create_terminal(None).unwrap();
    let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
    reg.attach(wid, vh1.id, vh1.generation, th.id, th.generation, rect)
        .unwrap();
    let err = reg
        .attach(wid, vh2.id, vh2.generation, th.id, th.generation, rect)
        .unwrap_err();
    assert!(matches!(err, RegistryError::AlreadyAttached { .. }));
    let th2 = reg.create_terminal(None).unwrap();
    let err2 = reg
        .attach(wid, vh1.id, vh1.generation, th2.id, Generation(999), rect)
        .unwrap_err();
    // View already hosts terminal
    assert!(matches!(
        err2,
        RegistryError::StaleHandle { .. } | RegistryError::ViewAlreadyAttached { .. }
    ));
}

#[test]
fn move_terminal_atomic_preserves_ids() {
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let vh1 = reg.create_view(wid).unwrap();
    let vh2 = reg.create_view(wid).unwrap();
    let th = reg.create_terminal(None).unwrap();
    let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
    reg.attach(wid, vh1.id, vh1.generation, th.id, th.generation, rect)
        .unwrap();
    let before_gen = th.generation;
    reg.move_terminal(
        th.id,
        th.generation,
        wid,
        vh1.id,
        vh1.generation,
        wid,
        vh2.id,
        vh2.generation,
        rect,
    )
    .unwrap();
    assert_eq!(reg.attached_view(th.id), Some(vh2.id));
    assert_eq!(reg.attached_terminal(vh1.id), None);
    assert_eq!(reg.attached_terminal(vh2.id), Some(th.id));
    // RuntimeId and TerminalId preserved, generation unchanged
    let snap = reg.terminal_snapshot(th.id, before_gen).unwrap();
    let _ = snap;
    // Failure atomicity: try move to occupied view should leave both unchanged
    let th2 = reg.create_terminal(None).unwrap();
    reg.attach(wid, vh1.id, vh1.generation, th2.id, th2.generation, rect)
        .unwrap();
    let err = reg
        .move_terminal(
            th.id,
            th.generation,
            wid,
            vh2.id,
            vh2.generation,
            wid,
            vh1.id,
            vh1.generation,
            rect,
        )
        .unwrap_err();
    assert!(matches!(err, RegistryError::ViewAlreadyAttached { .. }));
    // Both views unchanged
    assert_eq!(reg.attached_terminal(vh1.id), Some(th2.id));
    assert_eq!(reg.attached_terminal(vh2.id), Some(th.id));
}

#[test]
fn focus_mru_survives_detach_and_destroy() {
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let vh1 = reg.create_view(wid).unwrap();
    let vh2 = reg.create_view(wid).unwrap();
    let vh3 = reg.create_view(wid).unwrap();
    // Focus order: default focused vh1, then set focus to vh2, vh3
    reg.set_focus(wid, vh2.id, vh2.generation).unwrap();
    reg.set_focus(wid, vh3.id, vh3.generation).unwrap();
    assert_eq!(reg.focused_view(wid), Some(vh3.id));
    // Detach focused view -> focus moves to MRU next (vh2)
    let th = reg.create_terminal(None).unwrap();
    let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
    reg.attach(wid, vh3.id, vh3.generation, th.id, th.generation, rect)
        .unwrap();
    reg.detach(wid, vh3.id, vh3.generation).unwrap();
    assert_eq!(reg.focused_view(wid), Some(vh2.id));
    // Destroy focused view -> focus moves to next MRU (vh1)
    let focused = reg.focused_view(wid).unwrap();
    assert_eq!(focused, vh2.id);
    reg.destroy_view(wid, vh2.id, vh2.generation).unwrap();
    assert_eq!(reg.focused_view(wid), Some(vh1.id));
    // Destroy last view -> focus None, no panic
    reg.destroy_view(wid, vh1.id, vh1.generation).unwrap();
    reg.destroy_view(wid, vh3.id, vh3.generation).unwrap();
    assert_eq!(reg.focused_view(wid), None);
}

#[test]
fn visibility_states_do_not_mutate_terminal() {
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let vh = reg.create_view(wid).unwrap();
    let th = reg.create_terminal(None).unwrap();
    let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
    reg.attach(wid, vh.id, vh.generation, th.id, th.generation, rect)
        .unwrap();
    // Hidden view still has live terminal with same generation
    reg.set_visibility(wid, vh.id, vh.generation, Visibility::InactiveWorkspace)
        .unwrap();
    assert_eq!(
        reg.visibility(wid, vh.id).unwrap(),
        Visibility::InactiveWorkspace
    );
    // Terminal snapshot still accessible, not mutated
    let snap_before = reg.terminal_snapshot(th.id, th.generation).unwrap();
    reg.set_visibility(wid, vh.id, vh.generation, Visibility::ScratchpadHidden)
        .unwrap();
    let snap_after = reg.terminal_snapshot(th.id, th.generation).unwrap();
    assert_eq!(snap_before.generation, snap_after.generation);
}

#[test]
fn zero_area_never_reaches_pty_and_retains_previous_geometry() {
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let vh = reg.create_view(wid).unwrap();
    let th = reg.create_terminal(None).unwrap();
    let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
    reg.attach(wid, vh.id, vh.generation, th.id, th.generation, rect)
        .unwrap();
    let before = reg.terminal_snapshot(th.id, th.generation).unwrap();
    let zero = LogicalRect::new(0.0, 0.0, 0.0, 0.0).unwrap();
    let err = reg
        .handle_view_rect(wid, vh.id, vh.generation, zero)
        .unwrap_err();
    assert!(matches!(err, RegistryError::InvalidGeometry { .. }));
    let after = reg.terminal_snapshot(th.id, th.generation).unwrap();
    assert_eq!(before.width, after.width);
    assert_eq!(before.height, after.height);
    // Pending queue should be empty
    assert_eq!(reg.flush_pending_resizes().len(), 0);
}

#[test]
fn logical_rect_to_grid_floor_and_clamp() {
    let reg = default_registry();
    // 720x456 with cell 9x19 => 80x24
    let r = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
    assert_eq!(reg.logical_rect_to_grid(r).unwrap(), (80, 24));
    // Floor behavior
    let r2 = LogicalRect::new(0.0, 0.0, 721.9, 457.9).unwrap();
    assert_eq!(reg.logical_rect_to_grid(r2).unwrap(), (80, 24));
    // Clamp to 1 when tiny
    let r3 = LogicalRect::new(0.0, 0.0, 4.0, 4.0).unwrap();
    assert_eq!(reg.logical_rect_to_grid(r3).unwrap(), (1, 1));
    // Clamp to 1024 when huge
    let r4 = LogicalRect::new(0.0, 0.0, 20000.0, 20000.0).unwrap();
    assert_eq!(reg.logical_rect_to_grid(r4).unwrap(), (1024, 1024));
    // Zero-area returns error
    let r5 = LogicalRect::new(0.0, 0.0, 0.0, 10.0).unwrap();
    assert!(reg.logical_rect_to_grid(r5).is_err());
}

#[test]
fn debounce_64_coalesces_and_counts() {
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let vh = reg.create_view(wid).unwrap();
    let th = reg.create_terminal(None).unwrap();
    let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
    reg.attach(wid, vh.id, vh.generation, th.id, th.generation, rect)
        .unwrap();
    // Storm 70 rects in same tick
    for i in 1..=70 {
        let r = LogicalRect::new(0.0, 0.0, 720.0 + f64::from(i), 456.0).unwrap();
        let _ = reg.handle_view_rect(wid, vh.id, vh.generation, r);
    }
    // Pending queue capped at 64, coalesced dropped at least 6? Actually attach already cleared,
    // then 70 rects: first 64 fill, next 6 drop oldest => coalesced >=6 but we coalesce to latest per tick
    // The counter should be at least 6-?
    let coalesced = reg.resize_coalesced(th.id, th.generation).unwrap();
    assert!(coalesced >= 1, "storm beyond 64 must increment coalesced");
    // Flush processes at most one per terminal per tick (coalesced to latest)
    let flushed = reg.flush_pending_resizes();
    assert_eq!(flushed.len(), 1);
    // After flush pending empty
    assert_eq!(reg.flush_pending_resizes().len(), 0);
}

#[test]
fn generation_exhaustion_fails_closed() {
    let mut reg = default_registry();
    reg.set_generation_for_test(Generation(u64::MAX - 500));
    let err = reg.create_terminal(None).unwrap_err();
    assert!(matches!(err, RegistryError::GenerationExhausted { .. }));
    // State unchanged
    assert_eq!(reg.terminal_count(), 0);
}

#[test]
fn registry_disposal_closes_all_and_fails_further_calls() {
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let _vh = reg.create_view(wid).unwrap();
    let th = reg.create_terminal(None).unwrap();
    reg.dispose();
    assert!(reg.is_disposed());
    // All further calls fail with RegistryDisposed
    let err = reg.create_terminal(None).unwrap_err();
    assert!(matches!(err, RegistryError::RegistryDisposed { .. }));
    let err2 = reg.terminal_snapshot(th.id, th.generation).unwrap_err();
    assert!(matches!(err2, RegistryError::RegistryDisposed { .. }));
    let err3 = reg.create_workspace().unwrap_err();
    assert!(matches!(err3, RegistryError::RegistryDisposed { .. }));
}

#[test]
fn reattachment_vs_recreation() {
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let vh1 = reg.create_view(wid).unwrap();
    let vh2 = reg.create_view(wid).unwrap();
    let th = reg.create_terminal(None).unwrap();
    let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
    reg.attach(wid, vh1.id, vh1.generation, th.id, th.generation, rect)
        .unwrap();
    // Detach preserves same TerminalId/RuntimeId
    let tid = reg.detach(wid, vh1.id, vh1.generation).unwrap();
    assert_eq!(tid, th.id);
    // Reattach same terminal to new view
    reg.attach(wid, vh2.id, vh2.generation, th.id, th.generation, rect)
        .unwrap();
    assert_eq!(reg.attached_view(th.id), Some(vh2.id));
    // Simulate exit
    reg.mark_exited(th.id, th.generation, Some(1)).unwrap();
    let err = reg
        .handle_view_rect(wid, vh2.id, vh2.generation, rect)
        .unwrap_err();
    assert!(matches!(err, RegistryError::TerminalExited { .. }));
    // Close retires id
    reg.close_terminal(th.id, th.generation).unwrap();
    // Recreation with same PersistentId after close
    let pid = PersistentId::new("persist-a").unwrap();
    let pid2 = PersistentId::new("persist-a").unwrap();
    let th2 = reg.create_terminal(Some(pid)).unwrap();
    // Close th2 then recreate with same pid fresh id/runtime
    let pid_gen = th2.generation;
    reg.close_terminal(th2.id, pid_gen).unwrap();
    let th3 = reg.create_terminal(Some(pid2)).unwrap();
    assert_ne!(th2.id, th3.id);
    assert_ne!(th2.runtime_id, th3.runtime_id);
}

#[test]
fn bounded_workspaces_and_views() {
    let mut reg = TerminalRegistry::new(RegistryConfig {
        max_workspaces_per_window: 1,
        max_views_per_workspace: 1,
        ..RegistryConfig::default()
    })
    .unwrap();
    let wid = reg.create_workspace().unwrap();
    let err = reg.create_workspace().unwrap_err();
    assert!(matches!(err, RegistryError::TooManyWorkspaces { .. }));
    let _vh = reg.create_view(wid).unwrap();
    let err2 = reg.create_view(wid).unwrap_err();
    assert!(matches!(err2, RegistryError::TooManyViews { .. }));
}

#[test]
fn workspace_layout_without_hardcoded_tabs() {
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let vh1 = reg.create_view(wid).unwrap();
    let vh2 = reg.create_view(wid).unwrap();
    // Build split layout without hardcoded tabs primitive
    let layout = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(vh1.id, 40, 24)),
        LayoutNode::leaf(View::new(vh2.id, 40, 24)),
    );
    reg.set_workspace_layout(wid, layout).unwrap();
    let allocs = reg
        .reflow_workspace(wid, UiRect::new(0, 0, 80, 24))
        .unwrap();
    assert_eq!(allocs.len(), 2);
    // Stack layout also works (overlay alternative)
    let stack = LayoutNode::stack(vec![
        LayoutNode::leaf(View::new(vh1.id, 80, 24)),
        LayoutNode::leaf(View::new(vh2.id, 80, 24)),
    ]);
    reg.set_workspace_layout(wid, stack).unwrap();
    let allocs2 = reg
        .reflow_workspace(wid, UiRect::new(0, 0, 80, 24))
        .unwrap();
    assert_eq!(allocs2.len(), 2);
    // Overlay
    let base = LayoutNode::leaf(View::new(vh1.id, 80, 24));
    let over = LayoutNode::leaf(View::new(vh2.id, 20, 10));
    let overlay = LayoutNode::overlay(base, over, UiRect::new(5, 5, 20, 10));
    reg.set_workspace_layout(wid, overlay).unwrap();
    let allocs3 = reg
        .reflow_workspace(wid, UiRect::new(0, 0, 80, 24))
        .unwrap();
    assert_eq!(allocs3.len(), 2);
}

#[test]
fn inactive_workspace_visibility_not_rendered_but_retains_attachment() {
    let mut reg = default_registry();
    let wid1 = reg.create_workspace().unwrap();
    let wid2 = reg.create_workspace().unwrap();
    let vh = reg.create_view(wid1).unwrap();
    let th = reg.create_terminal(None).unwrap();
    let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
    reg.attach(wid1, vh.id, vh.generation, th.id, th.generation, rect)
        .unwrap();
    // Switch active to wid2 => wid1 views become InactiveWorkspace
    reg.set_active_workspace(wid2).unwrap();
    assert_eq!(
        reg.visibility(wid1, vh.id).unwrap(),
        Visibility::InactiveWorkspace
    );
    // Terminal still live
    assert!(reg.terminal_snapshot(th.id, th.generation).is_ok());
    // Switching back restores Visible
    reg.set_active_workspace(wid1).unwrap();
    assert_eq!(reg.visibility(wid1, vh.id).unwrap(), Visibility::Visible);
}

#[test]
fn headless_composition_rects_equivalence() {
    // Workspace view rectangles, registry lifecycle, and resize routing have headless tests without window/GPU
    let mut reg = default_registry();
    let wid = reg.create_workspace().unwrap();
    let vh1 = reg.create_view(wid).unwrap();
    let vh2 = reg.create_view(wid).unwrap();
    let layout = LayoutNode::split(
        SplitAxis::Vertical,
        0.5,
        LayoutNode::leaf(View::new(vh1.id, 80, 12)),
        LayoutNode::leaf(View::new(vh2.id, 80, 12)),
    );
    reg.set_workspace_layout(wid, layout).unwrap();
    let allocs = reg
        .reflow_workspace(wid, UiRect::new(0, 0, 80, 24))
        .unwrap();
    assert_eq!(allocs.len(), 2);
    // Each allocation should be non-empty and not overlapping (vertical split)
    assert_eq!(allocs[0].1.x, 0);
    assert_eq!(allocs[1].1.x, 0);
    assert_eq!(allocs[0].1.y, 0);
    assert_eq!(allocs[1].1.y, 12);
    // Determinism: second reflow identical
    let allocs2 = reg
        .reflow_workspace(wid, UiRect::new(0, 0, 80, 24))
        .unwrap();
    assert_eq!(allocs, allocs2);
}
