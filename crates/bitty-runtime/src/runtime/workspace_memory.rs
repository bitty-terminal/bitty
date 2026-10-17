//! Bounded, read-only memory accounting for retained workspace layouts.
//!
//! This report is deliberately separate from terminal/panel list queries. A
//! row counts layout leaves, then joins those views with the runtime-global
//! pane-session map and primary-state owner. A pane state is charged once by
//! its `ViewId`; the singleton primary state is charged once by the primary
//! owner marker. If a malformed or transitional layout references the same
//! owner more than once, the first workspace in slot order receives its bytes
//! and the aggregate still counts that owner once.

use std::collections::BTreeSet;
use std::fmt::Write;

use bitty_ui::{FloatStore, LayoutNode, ViewId};

use super::{Runtime, workspaces::MAX_WORKSPACES};

/// Maximum layout leaves examined for one workspace report row.
pub const MAX_WORKSPACE_MEMORY_LEAVES: usize = crate::registry::MAX_VIEWS_PER_WORKSPACE;

/// Maximum pane-session owners examined by one report. Runtime creation paths
/// enforce this same bound; the cap also keeps malformed test/embedder state
/// from making diagnostics traversal proportional to an untrusted map.
pub const MAX_WORKSPACE_MEMORY_PANE_SESSIONS: usize = super::session::MAX_SESSION_PANES_TOTAL;

/// Conservative bound for the JSON representation produced by
/// [`WorkspaceMemoryReport::to_json`]. It is independent of names, paths, and
/// terminal text and remains below the IPC frame ceiling.
pub const WORKSPACE_MEMORY_REPORT_JSON_MAX_BYTES: usize = MAX_WORKSPACES * 512 + 1024;

/// Memory accounting for one retained workspace slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMemoryRow {
    /// Stable workspace creation sequence.
    pub workspace_seq: u64,
    /// Whether this row is the active slot.
    pub active: bool,
    /// Number of reported layout and detached-float views, including duplicate
    /// references if present.
    pub leaf_views: usize,
    /// Whether the bounded layout walk stopped before visiting every layout
    /// leaf.
    pub layout_truncated: bool,
    /// Total detached floats in this workspace store. This count is exact and
    /// does not require materializing their ids.
    pub detached_floats_total: usize,
    /// Detached float ids included in the bounded leaf accounting.
    pub detached_floats_reported: usize,
    /// Detached floats omitted because the bounded leaf budget was exhausted.
    pub detached_floats_omitted: usize,
    /// Whether at least one detached float was omitted by the bounded walk.
    pub detached_floats_truncated: bool,
    /// Number of distinct owners charged to this row. A shared owner is
    /// charged only in the first retained workspace row that references it.
    pub owned_views: usize,
    /// Pane sessions whose owner was first encountered in this row.
    pub pane_sessions: usize,
    /// Whether the primary state owner was first encountered in this row.
    pub primary_state: bool,
    /// Unique state bytes charged to this row.
    pub owned_bytes: usize,
}

/// Bounded aggregate over all retained workspace slots and runtime-global
/// state owners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMemoryAggregate {
    /// Number of retained workspace rows.
    pub workspaces: usize,
    /// Sum of row leaf counts. Duplicate layout references are intentionally
    /// visible here because this is a view count, not an ownership count.
    pub leaf_views: usize,
    /// Number of distinct pane-session owners in the runtime-global map.
    pub pane_sessions: usize,
    /// Number of pane sessions not referenced by a retained workspace layout.
    pub orphaned_pane_sessions: usize,
    /// Number of primary state owners (zero or one).
    pub primary_states: usize,
    /// Number of distinct state owners with bytes charged.
    pub owned_views: usize,
    /// Sum of every distinct pane/primary state owner's retained bytes.
    pub unique_owned_bytes: usize,
    /// Bytes for owners that could not be attributed to a workspace row.
    pub unattributed_bytes: usize,
}

/// Read-only bounded report for diagnosing hidden/background workspace memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMemoryReport {
    /// Per-workspace rows in retained slot order.
    pub workspaces: Vec<WorkspaceMemoryRow>,
    /// Stable aggregate across all rows and global owners.
    pub aggregate: WorkspaceMemoryAggregate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Owner {
    Pane(ViewId),
    Primary,
}

fn bounded_leaf_ids(
    node: &LayoutNode,
    out: &mut Vec<ViewId>,
    depth: usize,
    limit: usize,
    truncated: &mut bool,
) {
    if out.len() >= limit {
        *truncated = true;
        return;
    }
    if depth >= super::session::MAX_SESSION_LAYOUT_DEPTH {
        *truncated = true;
        return;
    }
    match node {
        LayoutNode::Leaf(view) => out.push(view.id()),
        LayoutNode::Split { first, second, .. } => {
            bounded_leaf_ids(first, out, depth + 1, limit, truncated);
            bounded_leaf_ids(second, out, depth + 1, limit, truncated);
        }
        LayoutNode::Stack(children) => {
            for child in children {
                bounded_leaf_ids(child, out, depth + 1, limit, truncated);
                if *truncated {
                    break;
                }
            }
        }
        LayoutNode::Overlay { base, overlay, .. } => {
            bounded_leaf_ids(base, out, depth + 1, limit, truncated);
            bounded_leaf_ids(overlay, out, depth + 1, limit, truncated);
        }
    }
}

struct BoundedWorkspaceViews {
    views: Vec<ViewId>,
    layout_truncated: bool,
    detached_floats_total: usize,
    detached_floats_reported: usize,
    detached_floats_omitted: usize,
    detached_floats_truncated: bool,
}

fn bounded_workspace_views(layout: &LayoutNode, floats: &FloatStore) -> BoundedWorkspaceViews {
    let (mut views, layout_truncated) =
        bounded_layout_leaf_ids(layout, MAX_WORKSPACE_MEMORY_LEAVES);
    let layout_views_reported = views.len();
    let remaining = MAX_WORKSPACE_MEMORY_LEAVES.saturating_sub(views.len());
    let detached_floats_total = floats.len();
    // `FloatStore::ids` is a convenience allocation intended for paint order.
    // Only call it after the O(1) length check, so malformed state cannot make
    // diagnostics allocate an unbounded detached-view vector.
    if remaining != 0 && floats.len() <= remaining {
        views.extend(floats.ids());
    }
    let detached_floats_reported = views.len().saturating_sub(layout_views_reported);
    BoundedWorkspaceViews {
        views,
        layout_truncated,
        detached_floats_total,
        detached_floats_reported,
        detached_floats_omitted: detached_floats_total.saturating_sub(detached_floats_reported),
        detached_floats_truncated: detached_floats_total > detached_floats_reported,
    }
}

/// Collects a bounded prefix of layout leaves without materializing the full
/// tree. The caller's limit is capped at the terminal registry bound, and
/// `truncated` is true when the prefix does not describe every leaf.
#[must_use]
pub fn bounded_layout_leaf_ids(layout: &LayoutNode, limit: usize) -> (Vec<ViewId>, bool) {
    let limit = limit.min(crate::registry::MAX_TERMINALS);
    let mut views = Vec::with_capacity(limit);
    let mut truncated = false;
    if limit != 0 {
        bounded_leaf_ids(layout, &mut views, 0, limit, &mut truncated);
    }
    (views, truncated)
}

impl Runtime {
    /// Produces a bounded, read-only memory report over every retained
    /// workspace slot. This does not inspect PTYs, spawn/stop processes, or
    /// alter active-layout terminal/panel list semantics.
    #[must_use]
    pub fn workspace_memory_report(&self) -> WorkspaceMemoryReport {
        let mut rows = Vec::with_capacity(self.workspaces.len().min(MAX_WORKSPACES));
        let mut seen = BTreeSet::new();
        let mut row_leaf_total = 0usize;
        let mut row_owned_bytes = 0usize;

        for (index, slot) in self.workspaces.iter().take(MAX_WORKSPACES).enumerate() {
            let layout = if index == self.active_workspace {
                &self.layout
            } else {
                &slot.layout
            };
            let bounded = bounded_workspace_views(
                layout,
                if index == self.active_workspace {
                    &self.floats
                } else {
                    &slot.floats
                },
            );

            let mut pane_sessions = 0usize;
            let mut primary_state = false;
            let mut owned_bytes = 0usize;
            let mut owned_views = 0usize;

            for view in &bounded.views {
                row_leaf_total = row_leaf_total.saturating_add(1);
                let mut owners = [None, None];
                let mut owner_count = 0usize;
                if self.pane_sessions.contains_key(view) {
                    owners[owner_count] = Some(Owner::Pane(*view));
                    owner_count += 1;
                }
                if self.primary_view == Some(*view) {
                    owners[owner_count] = Some(Owner::Primary);
                    owner_count += 1;
                }
                for owner in owners.into_iter().take(owner_count).flatten() {
                    let first_owner = seen.insert(owner);
                    if first_owner {
                        owned_views = owned_views.saturating_add(1);
                        match owner {
                            Owner::Pane(_) => pane_sessions = pane_sessions.saturating_add(1),
                            Owner::Primary => primary_state = true,
                        }
                    }
                    if !first_owner {
                        continue;
                    }
                    let bytes = match owner {
                        Owner::Pane(id) => self
                            .pane_sessions
                            .get(&id)
                            .map_or(0, |session| session.state.retained_heap_bytes()),
                        Owner::Primary => self.state.retained_heap_bytes(),
                    };
                    owned_bytes = owned_bytes.saturating_add(bytes);
                    row_owned_bytes = row_owned_bytes.saturating_add(bytes);
                }
            }

            rows.push(WorkspaceMemoryRow {
                workspace_seq: slot.seq,
                active: index == self.active_workspace,
                leaf_views: bounded.views.len(),
                layout_truncated: bounded.layout_truncated,
                detached_floats_total: bounded.detached_floats_total,
                detached_floats_reported: bounded.detached_floats_reported,
                detached_floats_omitted: bounded.detached_floats_omitted,
                detached_floats_truncated: bounded.detached_floats_truncated,
                owned_views,
                pane_sessions,
                primary_state,
                owned_bytes,
            });
        }

        // Include global owners that are not currently reachable from a
        // retained layout. They are intentionally aggregate-only: no row can
        // truthfully claim ownership for an orphaned view.
        let mut unattributed_bytes = 0usize;
        let mut orphaned_pane_sessions = 0usize;
        for (view, session) in self
            .pane_sessions
            .iter()
            .take(MAX_WORKSPACE_MEMORY_PANE_SESSIONS)
        {
            let owner = Owner::Pane(*view);
            if seen.insert(owner) {
                orphaned_pane_sessions = orphaned_pane_sessions.saturating_add(1);
                let bytes = session.state.retained_heap_bytes();
                unattributed_bytes = unattributed_bytes.saturating_add(bytes);
            }
        }
        if self.primary_view.is_some() {
            let owner = Owner::Primary;
            if seen.insert(owner) {
                unattributed_bytes =
                    unattributed_bytes.saturating_add(self.state.retained_heap_bytes());
            }
        }

        WorkspaceMemoryReport {
            aggregate: WorkspaceMemoryAggregate {
                workspaces: rows.len(),
                leaf_views: row_leaf_total,
                pane_sessions: self
                    .pane_sessions
                    .len()
                    .min(MAX_WORKSPACE_MEMORY_PANE_SESSIONS),
                orphaned_pane_sessions,
                primary_states: usize::from(self.primary_view.is_some()),
                owned_views: seen.len(),
                unique_owned_bytes: row_owned_bytes.saturating_add(unattributed_bytes),
                unattributed_bytes,
            },
            workspaces: rows,
        }
    }
}

impl WorkspaceMemoryReport {
    /// Serializes the report without names, paths, cwd, terminal text, or PIDs.
    /// The row and field bounds make the complete result bounded.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(WORKSPACE_MEMORY_REPORT_JSON_MAX_BYTES);
        out.push_str(r#"{"v":1,"workspaces":["#);
        for (index, row) in self.workspaces.iter().take(MAX_WORKSPACES).enumerate() {
            if index != 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                r#"{{"seq":{},"active":{},"leaf_views":{},"layout_truncated":{},"detached_floats_total":{},"detached_floats_reported":{},"detached_floats_omitted":{},"detached_floats_truncated":{},"owned_views":{},"pane_sessions":{},"primary_state":{},"owned_bytes":{}}}"#,
                row.workspace_seq,
                row.active,
                row.leaf_views,
                row.layout_truncated,
                row.detached_floats_total,
                row.detached_floats_reported,
                row.detached_floats_omitted,
                row.detached_floats_truncated,
                row.owned_views,
                row.pane_sessions,
                row.primary_state,
                row.owned_bytes
            );
        }
        let a = &self.aggregate;
        let _ = write!(
            out,
            r#"],"aggregate":{{"workspaces":{},"leaf_views":{},"pane_sessions":{},"orphaned_pane_sessions":{},"primary_states":{},"owned_views":{},"unique_owned_bytes":{},"unattributed_bytes":{}}}}}"#,
            a.workspaces,
            a.leaf_views,
            a.pane_sessions,
            a.orphaned_pane_sessions,
            a.primary_states,
            a.owned_views,
            a.unique_owned_bytes,
            a.unattributed_bytes
        );
        debug_assert!(out.len() <= WORKSPACE_MEMORY_REPORT_JSON_MAX_BYTES);
        out
    }
}
