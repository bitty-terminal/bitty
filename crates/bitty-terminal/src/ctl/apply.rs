//! `bitty ctl` server-side apply against a live `Runtime` (split from `ctl.rs`,
//! CTX-0307).

use super::render::json_escape;
use bitty_ipc::ctl as ipc_ctl;

// ── server-side apply (owns &mut Runtime) ─────────────────────────────────
//
// The servo (background thread) never touches `Runtime` directly (`Runtime`
// is `!Send`): `bitty_ipc::ctl` validates + authorizes, enqueues to its
// global queue, and blocks on the reply; the main thread (which owns
// `Runtime`) drains via [`drain_global_control_queue`] and applies each
// action with [`apply_control`]. Tests drive [`apply_control`] directly
// (same thread, no queue).
//
// CTX-0792 (#1403): every served connection enqueues with its own
// authorization snapshot, re-validated against the connection authority at
// drain. The drain's `granted` argument is only the fallback for
// authority-less entries, and the live callers pass an empty set so such an
// entry can never borrow process-wide scopes.

/// `drain_global_control_queue` with a pre-mutation hook (CTX-0481, #762).
///
/// `before_mutation(runtime, method)` runs for an authorized
/// layout-mutating verb before it applies; the app uses it to restore an
/// active pane zoom so the verb lands on the real tiled tree instead of the
/// single-leaf zoom proxy (which the zoom restore would later discard,
/// dropping the new pane). Read-only verbs never invoke the hook.
pub fn drain_global_control_queue_with(
    runtime: &mut bitty_runtime::Runtime,
    granted: &bitty_ipc::ScopeSet,
    before_mutation: impl FnMut(&mut bitty_runtime::Runtime, &str),
) -> usize {
    drain_global_control_queue_inner(runtime, granted, before_mutation, |_| {})
}

/// Test-only drain variant that runs a hook between the claim and the apply.
///
/// The hook is what makes the claim/apply window observable (a cancel or a
/// deadline crossing there), and every test that uses it drives a Unix PTY, so
/// the variant is unix-only: keeping it alive on Windows would be dead code and
/// the Windows `-D warnings` cross-check would fail on it.
#[cfg(all(test, unix))]
pub(crate) fn drain_global_control_queue_with_barrier(
    runtime: &mut bitty_runtime::Runtime,
    granted: &bitty_ipc::ScopeSet,
    before_mutation: impl FnMut(&mut bitty_runtime::Runtime, &str),
    after_claim: impl FnMut(&ipc_ctl::PendingControl),
) -> usize {
    drain_global_control_queue_inner(runtime, granted, before_mutation, after_claim)
}

fn drain_global_control_queue_inner(
    runtime: &mut bitty_runtime::Runtime,
    granted: &bitty_ipc::ScopeSet,
    mut before_mutation: impl FnMut(&mut bitty_runtime::Runtime, &str),
    mut after_claim: impl FnMut(&ipc_ctl::PendingControl),
) -> usize {
    let mut count = 0usize;
    loop {
        let Some(item) = ipc_ctl::pop_pending_control() else {
            break;
        };
        count += 1;
        after_claim(&item);
        if !item.begin_apply() {
            // Withdrawn by the waiter, already applied, or too close to its
            // deadline: `begin_apply` re-checks the deadline under the phase
            // lock (and answers a late entry itself), so no effect can land
            // after the client-side timeout.
            continue;
        }
        // CTX-0792 (#1403): the single authorization recheck runs after the
        // apply phase is claimed and immediately before mutation, against
        // the live connection authority (session, consent generation, scopes,
        // terminal capability map).
        if let Err(err) = item.authorize_at_drain(granted) {
            item.finish_without_apply();
            let (category, code, message) = ipc_error_triple(&err);
            let _ = item.reply.send(ipc_ctl::ControlReply {
                ok: false,
                result_json: String::new(),
                category,
                code,
                message,
            });
            continue;
        }
        let effective_scopes = item.effective_scopes(granted);
        if method_mutates_layout(&item.method) {
            before_mutation(runtime, &item.method);
        }
        let reply = apply_control_envelope(
            runtime,
            &item.method,
            item.params.as_deref(),
            &effective_scopes,
        );
        item.finish_apply();
        let _ = item.reply.send(reply);
    }
    count
}

/// Whether `method` can mutate the layout tree (CTX-0481, #762).
///
/// Zoom restore must run before any of these so a ctl verb never applies to
/// the single-leaf zoom proxy. Read-only verbs and focus/send/text stay out:
/// they must not disturb an active zoom.
pub fn method_mutates_layout(method: &str) -> bool {
    matches!(
        method,
        ipc_ctl::METHOD_SPAWN_TERMINAL
            | ipc_ctl::METHOD_CLOSE_TERMINAL
            | ipc_ctl::METHOD_SPLIT_VIEW
            | ipc_ctl::METHOD_NEW_WORKSPACE
            | ipc_ctl::METHOD_CLOSE_WORKSPACE
            | ipc_ctl::METHOD_FOCUS_WORKSPACE
            | ipc_ctl::METHOD_MOVE_WORKSPACE
    )
}

/// Validate + authorize + apply one control envelope against `runtime`.
///
/// Total: every failure becomes an [`ipc_ctl::ControlReply`] error, never a panic.
pub fn apply_control_envelope(
    runtime: &mut bitty_runtime::Runtime,
    method: &str,
    params: Option<&str>,
    granted: &bitty_ipc::ScopeSet,
) -> ipc_ctl::ControlReply {
    // CTX-1117 (issue #1900): `panel list` is local to `bitty-terminal` (the
    // `bitty-ipc` pin predates it). Authorize as `view.inspect` (same as
    // `view list`): read-only, no elevation, no mutation.
    if method == super::request::METHOD_LIST_PANELS {
        if !granted.contains(bitty_ipc::Scope::ViewInspect) {
            let err = bitty_ipc::IpcError::ScopeDenied {
                scope: String::from("view.inspect"),
                action: String::from(method),
            };
            let (category, code, message) = ipc_error_triple(&err);
            return ipc_ctl::ControlReply {
                ok: false,
                result_json: String::new(),
                category,
                code,
                message,
            };
        }
        match apply_control(runtime, method, params) {
            Ok(result_json) => {
                return ipc_ctl::ControlReply {
                    ok: true,
                    result_json,
                    category: "",
                    code: "",
                    message: String::new(),
                };
            }
            Err((category, code, message)) => {
                return ipc_ctl::ControlReply {
                    ok: false,
                    result_json: String::new(),
                    category,
                    code,
                    message,
                };
            }
        }
    }
    if let Err(ipc_err) = ipc_ctl::authorize_ctl_method(method, granted) {
        let (category, code, message) = ipc_error_triple(&ipc_err);
        return ipc_ctl::ControlReply {
            ok: false,
            result_json: String::new(),
            category,
            code,
            message,
        };
    }
    match apply_control(runtime, method, params) {
        Ok(result_json) => ipc_ctl::ControlReply {
            ok: true,
            result_json,
            category: "",
            code: "",
            message: String::new(),
        },
        Err((category, code, message)) => ipc_ctl::ControlReply {
            ok: false,
            result_json: String::new(),
            category,
            code,
            message,
        },
    }
}

fn ipc_error_triple(err: &bitty_ipc::IpcError) -> (&'static str, &'static str, String) {
    // One class table for enqueue and drain (CTX-0792): the category and code
    // come from `bitty_ipc::ctl::control_error_class`; only the message is
    // drain-specific (it may name the rejected id, never params text).
    let (category, code) = ipc_ctl::control_error_class(err);
    let message = match err {
        bitty_ipc::IpcError::ScopeDenied { .. } => {
            format!("permission denied: {err} (needs elevation via BITTY_CTL_ELEVATE)")
        }
        // Generic denials and unauthenticated peers are permission failures
        // (CLI exit 7), never transport timeouts: surface the denial with
        // the elevation hint instead of exit 6.
        bitty_ipc::IpcError::Denied { code, reason } if category == "auth" => {
            format!("permission denied: [{code}] {reason} (needs elevation via BITTY_CTL_ELEVATE)")
        }
        bitty_ipc::IpcError::Unauthenticated { .. } => format!("permission denied: {err}"),
        _ => format!("{err}"),
    };
    (category, code, message)
}

/// Apply an authorized control method to `runtime`.
///
/// The caller must have authorized via [`ipc_ctl::authorize_ctl_method`]
/// first ([`apply_control_envelope`] does this); this function re-validates
/// params defensively so direct callers cannot bypass bounds.
#[allow(clippy::too_many_lines)]
pub fn apply_control(
    runtime: &mut bitty_runtime::Runtime,
    method: &str,
    params: Option<&str>,
) -> Result<String, (&'static str, &'static str, String)> {
    use bitty_runtime::{SplitAxis, ViewId};

    if method == ipc_ctl::METHOD_LIST_WINDOWS {
        // Single-window vertical slice: exactly one window today.
        return Ok(String::from("{\"windows\":[{\"id\":\"w:1\"}]}"));
    }
    if method == ipc_ctl::METHOD_LIST_VIEWS {
        let ids = runtime.layout().leaf_ids();
        let focused = runtime.focused_view();
        let mut out = String::from("{\"views\":[");
        for (idx, id) in ids.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"id\":\"v:{}\",\"focused\":{}}}",
                id.0,
                focused.is_some_and(|f| f == *id)
            ));
        }
        out.push_str("]}");
        return Ok(out);
    }
    if method == super::request::METHOD_LIST_PANELS {
        // CTX-1117 (issue #1900): tiled panels are layout leaves (Panel ==
        // leaf View, 1:1 with `view list` ids as `p:N`/`v:N`). Minimal
        // read-only query (no mutation): id, bounds from the solver
        // allocations, role from the leaf presentation mode, focused marker
        // mirroring `view list`. Pinned/scratchpad leaves outside the live
        // layout are not listed (same scope as `view list`). Like `view
        // list`, wire params are ignored (lists take none; `None` from the
        // typed client).
        let _ = params;
        let ids = runtime.layout().leaf_ids();
        let focused = runtime.focused_view();
        let allocations = runtime.layout_allocations();
        let mut out = String::from("{\"panels\":[");
        for (idx, id) in ids.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            let bounds = allocations
                .iter()
                .find(|(alloc_id, _)| *alloc_id == *id)
                .map(|(_, rect)| *rect);
            let role = runtime
                .layout()
                .find_leaf(*id)
                .map(|view| view.presentation().as_str())
                .unwrap_or("tiled");
            let (x, y, width, height) = bounds
                .map(|rect| (rect.x, rect.y, rect.width, rect.height))
                .unwrap_or((0, 0, 0, 0));
            out.push_str(&format!(
                "{{\"id\":\"p:{}\",\"view\":\"v:{}\",\"bounds\":{{\"x\":{x},\"y\":{y},\"width\":{width},\"height\":{height}}},\"role\":\"{role}\",\"focused\":{}}}",
                id.0,
                id.0,
                focused.is_some_and(|f| f == *id)
            ));
        }
        out.push_str("]}");
        return Ok(out);
    }
    if method == ipc_ctl::METHOD_LIST_TERMINALS {
        // 1:1 terminal:view mapping until the registry lands: each leaf is
        // one terminal `t:<view>`. CTX-0284/CTX-0387: tiles are layout-derived
        // and a leaf may still have no shell (spawn failure), so report live
        // pane-session presence per entry instead of implying one shell per
        // tile.
        // Keep both arrays aligned and bound the complete terminal diagnostics
        // envelope even if a test or embedder supplies a larger raw layout.
        // `total_lower_bound` and `truncated` make the bounded prefix explicit
        // to callers without presenting a lower-bound sentinel as an exact
        // total.
        let (ids, truncated) =
            bitty_runtime::bounded_layout_leaf_ids(runtime.layout(), bitty_runtime::MAX_TERMINALS);
        let mut out = String::from("{\"terminals\":[");
        for (idx, id) in ids.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"id\":\"t:{}\",\"has_pane_session\":{}}}",
                id.0,
                runtime.has_pane_session(id)
            ));
        }
        out.push_str("],\"diagnostics\":[");
        for (idx, id) in ids.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            let diagnostics = runtime.terminal_diagnostics(*id);
            let child_pid = diagnostics
                .child_pid
                .map_or_else(|| String::from("null"), |pid| pid.to_string());
            out.push_str(&format!(
                "{{\"terminal_id\":\"t:{}\",\"view_id\":\"v:{}\",\"has_session\":{},\"scrollback_lines\":{},\"retained_heap_bytes\":{},\"child_pid\":{},\"child_state\":\"{}\"}}",
                diagnostics.terminal_id,
                diagnostics.view_id,
                diagnostics.has_session,
                diagnostics.scrollback_lines,
                diagnostics.retained_heap_bytes,
                child_pid,
                diagnostics.child_state,
            ));
        }
        out.push_str(&format!(
            "],\"total_lower_bound\":{},\"truncated\":{truncated}}}",
            ids.len()
        ));
        return Ok(out);
    }
    if method == ipc_ctl::METHOD_SEND_INPUT {
        let (terminal_id, text) = ipc_ctl::parse_send_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_terminal_id(&terminal_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let target_view = ViewId::new(u64::from(num));
        let is_live = runtime.layout().leaf_ids().contains(&target_view)
            || runtime.floated_views().contains(&target_view)
            || runtime.pinned_views().contains(&target_view);
        if !is_live {
            return Err((
                "usage",
                "NotFound",
                format!("no such terminal {terminal_id}"),
            ));
        }
        // Focused-leaf routing only (runtime rule): sending to a
        // non-focused leaf would silently retarget input, so fail closed
        // with Conflict naming the focus verb first.
        if runtime.focused_view() != Some(target_view) {
            return Err((
                "usage",
                "Conflict",
                format!(
                    "terminal {terminal_id} is not focused; run `bitty ctl view focus v:{num}` first"
                ),
            ));
        }
        // A panel that presents a GUI content layer owns its input: the write is
        // dropped (and counted) at the router's content gate, so report the drop
        // as a drop instead of answering `sent_to`/`bytes` for bytes no shell
        // received (bitty#3250 review). The router stays the only place that
        // drops and counts, so this verb cannot disagree with it.
        let dropped_by_content = runtime.content_layer_presented(target_view);
        let delivered = runtime.push_input_bytes(text.as_bytes());
        if !delivered && dropped_by_content {
            return Ok(format!(
                "{{\"sent_to\":\"{terminal_id}\",\"bytes\":0,\"dropped\":\"content-layer\"}}"
            ));
        }
        return Ok(format!(
            "{{\"sent_to\":\"{terminal_id}\",\"bytes\":{}}}",
            text.len()
        ));
    }
    if method == ipc_ctl::METHOD_GET_TERMINAL_TEXT {
        let terminal_id = ipc_ctl::parse_terminal_id_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_terminal_id(&terminal_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let target_view = ViewId::new(u64::from(num));
        let is_live = runtime.layout().leaf_ids().contains(&target_view)
            || runtime.floated_views().contains(&target_view)
            || runtime.pinned_views().contains(&target_view);
        if !is_live {
            return Err((
                "usage",
                "NotFound",
                format!("no such terminal {terminal_id}"),
            ));
        }
        // Prefer the pane snapshot when present; the primary snapshot is
        // returned only for the primary owner leaf (CTX-0359: the leaf
        // focused when the primary shell attached). A session-less leaf that
        // is not the owner has no shell of its own; its text is empty so one
        // grid is never duplicated as text across tiles (pre-CTX-0359 the
        // focused session-less leaf mirrored primary, which let a fresh
        // workspace leaf read the previous workspace's terminal).
        //
        // Issue #2875 scope 6: a painted image that owns no grid cell (a
        // direct `a=p`/`a=T` placement) must not read as an empty hole, so
        // the projection carries the view's painted image footprints.
        let images = runtime.kitty_image_cell_spans_for_view(target_view);
        // bitty#2388: a presented GUI content layer displaces the grid, so
        // the honest projection of "what this terminal shows" is the layer's
        // own text — the displaced grid is frozen under the layer and paints
        // nothing. The reply names the mode with `"content":"layer"` so a
        // script can tell the layer from the grid without parsing text, and
        // a release returns the grid projection byte-identically.
        if let Some(layer_text) = runtime.presented_panel_content_text(target_view) {
            let mut out = String::from("{\"terminal_id\":\"");
            out.push_str(&terminal_id);
            out.push_str("\",\"content\":\"layer\",\"text\":\"");
            append_json_escaped(&mut out, &bounded_text(&layer_text));
            out.push_str("\"}");
            return Ok(out);
        }
        let text = runtime
            .pane_snapshot(&target_view)
            .map(|snap| snapshot_text_with_images(&snap, &images))
            .or_else(|| {
                if runtime.is_primary_view(&target_view) {
                    Some(snapshot_text_with_images(&runtime.snapshot(), &images))
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let mut out = String::from("{\"terminal_id\":\"");
        out.push_str(&terminal_id);
        out.push_str("\",\"text\":\"");
        append_json_escaped(&mut out, &bounded_text(&text));
        out.push_str("\"}");
        return Ok(out);
    }
    if method == ipc_ctl::METHOD_SPAWN_TERMINAL {
        let cwd = ipc_ctl::parse_spawn_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        if let Some(dir) = cwd.as_deref() {
            // #1528: the directory is resolved by the requesting client, so
            // it must arrive absolute; a relative path would silently
            // resolve against this terminal process's own cwd instead.
            if !std::path::Path::new(dir).is_absolute() {
                return Err((
                    "usage",
                    "InvalidParams",
                    format!("spawn --cwd {dir:?} must be an absolute path"),
                ));
            }
            if !std::path::Path::new(dir).is_dir() {
                return Err((
                    "transport",
                    "Transport",
                    format!("spawn --cwd {dir:?} is not a directory"),
                ));
            }
        }
        // CTX-0323 (D3): a spawn must be representable in the ctl model. The
        // old path replaced the primary shell, which `terminal list` (layout
        // leaves + pane sessions) cannot observe, so it reported a no-op.
        // Create a fresh leaf (default right split) and give it a private
        // shell session through the shared CTX-0387 creation path:
        // `view list` gains `v:N`, `terminal list` gains `t:N` with
        // `has_pane_session:true`, and the caller can address it.
        // Fail-closed: if the shell cannot start, the pre-spawn layout is
        // restored, so success is never reported without a live session.
        let Some(focused) = runtime.focused_view() else {
            return Err((
                "usage",
                "Conflict",
                String::from("no focused view to spawn into"),
            ));
        };
        let previous = runtime.layout().clone();
        let (new_id, cols, rows) =
            create_split_leaf(runtime, focused, SplitAxis::Horizontal, false)?;
        if let Err(err) = spawn_leaf_shell(
            runtime,
            new_id,
            cols,
            rows,
            cwd.as_deref().map(std::path::Path::new),
        ) {
            // Fail-closed: no observable terminal means no success.
            runtime.set_layout(previous);
            return Err(("transport", "Transport", format!("spawn failed: {err}")));
        }
        // CTX-0364: focus follows the new panel. Set after the spawn so
        // CTX-0357 cwd inheritance still reads the source pane as focused.
        runtime.set_focus(new_id);
        // #1528: the receipt names the directory the shell started in when
        // the caller chose one (absent means `OSC 7` inheritance applied).
        let cwd_field = cwd
            .as_deref()
            .map(|dir| format!(",\"cwd\":\"{}\"", json_escape(dir)))
            .unwrap_or_default();
        return Ok(format!(
            "{{\"spawned\":true,\"terminal_id\":\"t:{}\",\"view_id\":\"v:{}\"{cwd_field}}}",
            new_id.0, new_id.0
        ));
    }
    if method == ipc_ctl::METHOD_CLOSE_TERMINAL {
        let terminal_id = ipc_ctl::parse_terminal_id_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_terminal_id(&terminal_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let target_view = ViewId::new(u64::from(num));
        if runtime.close_pane_session(&target_view) {
            return Ok(format!("{{\"closed\":\"{terminal_id}\"}}"));
        }
        // No pane session: refuse a live session-less view so the layout is
        // never stranded empty; otherwise report NotFound.
        let is_live = runtime.layout().leaf_ids().contains(&target_view)
            || runtime.floated_views().contains(&target_view)
            || runtime.pinned_views().contains(&target_view);
        if is_live {
            return Err((
                "usage",
                "Conflict",
                format!("terminal {terminal_id} has no live session to close"),
            ));
        }
        return Err((
            "usage",
            "NotFound",
            format!("no such terminal {terminal_id}"),
        ));
    }
    if method == ipc_ctl::METHOD_SPLIT_VIEW {
        let direction = ipc_ctl::parse_split_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let Some(focused) = runtime.focused_view() else {
            return Err((
                "usage",
                "Conflict",
                String::from("no focused view to split"),
            ));
        };
        // Canonical axis semantics (CTX-0224): `SplitAxis::Horizontal` is
        // left/right (side-by-side, vertical divider) and
        // `SplitAxis::Vertical` is top/bottom (stacked, horizontal
        // divider), matching `geometry.rs`, the layout solver, CTX-0209
        // `smart_split_axis`, and the keymap path (`split_dir_to_axis` in
        // `main.rs`: Left/Right -> Horizontal, Up/Down -> Vertical).
        let (axis, place_new_first) = match direction {
            ipc_ctl::SplitDirection::Left => (SplitAxis::Horizontal, true),
            ipc_ctl::SplitDirection::Right => (SplitAxis::Horizontal, false),
            ipc_ctl::SplitDirection::Up => (SplitAxis::Vertical, true),
            ipc_ctl::SplitDirection::Down => (SplitAxis::Vertical, false),
        };
        // CTX-0387: the fresh pane gets its own shell through the same
        // creation path as the keymap `new_split` action (`spawn_pane_shell`,
        // replaying the recorded primary recipe). Best-effort, matching the
        // keymap and `workspace_new`: a failed spawn leaves the pane empty
        // with a loud warning instead of failing the layout verb.
        let (new_id, cols, rows) = create_split_leaf(runtime, focused, axis, place_new_first)?;
        if let Err(err) = spawn_leaf_shell(runtime, new_id, cols, rows, None) {
            eprintln!(
                "warning: ctl view split pane {new_id:?} shell spawn failed ({err}) — pane stays empty"
            );
        }
        // CTX-0364: focus follows the freshly created panel (kitty/ghostty
        // parity): the new view becomes the input/cursor target immediately.
        runtime.set_focus(new_id);
        return Ok(format!(
            "{{\"split\":\"{}\",\"new_view\":\"v:{}\"}}",
            direction.as_str(),
            new_id.0
        ));
    }
    if method == ipc_ctl::METHOD_FOCUS_VIEW {
        let view_id = ipc_ctl::parse_focus_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_view_id(&view_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        if runtime.set_focus(ViewId::new(u64::from(num))) {
            return Ok(format!("{{\"focused\":\"{view_id}\"}}"));
        }
        return Err(("usage", "NotFound", format!("no such view {view_id}")));
    }
    if method == ipc_ctl::METHOD_LIST_WORKSPACES {
        // CTX-0338 (D2 residual): `workspaces` carries the canonical
        // `ws:{seq}` identity the write verbs (`focus`/`close`/`move`) accept,
        // so a client can feed list output straight back. Display labels stay
        // available under `names`; `active` keeps the 1-based positional index
        // and `active_id` names the focused workspace canonically.
        // CTX-0979: Core draws no display; no `tabline` string is returned.
        let ids: Vec<String> = (0..runtime.workspace_count())
            .filter_map(|idx| runtime.workspace_seq_at(idx).map(|seq| format!("ws:{seq}")))
            .collect();
        let active_id = runtime
            .workspace_seq_at(runtime.active_workspace_index())
            .map_or_else(String::new, |seq| format!("ws:{seq}"));
        let memory = runtime.workspace_memory_report().to_json();
        return Ok(format!(
            "{{\"workspaces\":{},\"names\":{},\"active\":{},\"active_id\":\"{}\",\"count\":{},\"memory\":{memory}}}",
            json_string_array(&ids),
            json_string_array(&runtime.workspace_names()),
            runtime.active_workspace_index() + 1,
            json_escape(&active_id),
            runtime.workspace_count(),
        ));
    }
    if method == ipc_ctl::METHOD_NEW_WORKSPACE {
        if params.is_some() {
            // Strict arity: `workspace new` takes no params object (a
            // misplaced `{"cwd":...}` or split direction fails closed here,
            // not as a silent ignore).
            return Err((
                "usage",
                "InvalidParams",
                String::from("workspace new takes no params"),
            ));
        }
        match runtime.workspace_new() {
            Ok(index) => {
                // CTX-0322: report the stable creation sequence (`ws{seq}`),
                // the same identity `workspace list` names and `focus`/`close`
                // accept — never the positional display index.
                let Some(seq) = runtime.workspace_seq_at(index) else {
                    return Err((
                        "transport",
                        "Transport",
                        String::from("workspace slot vanished after create"),
                    ));
                };
                return Ok(format!("{{\"created\":\"ws:{seq}\"}}"));
            }
            Err(message) => {
                return Err((
                    "usage",
                    "Conflict",
                    format!("workspace new refused: {message}"),
                ));
            }
        }
    }
    if method == ipc_ctl::METHOD_CLOSE_WORKSPACE {
        let workspace_id = ipc_ctl::parse_workspace_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_workspace_id(&workspace_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        // CTX-0322: `ws:N` is the stable creation sequence, not a position.
        let Some(index) = runtime.workspace_index_by_seq(u64::from(num)) else {
            return Err((
                "usage",
                "NotFound",
                format!("no such workspace {workspace_id}"),
            ));
        };
        // Non-interactive path: elevation (terminal.manage, authorized
        // upstream) is the gate, so close is immediate — the pending-confirm
        // gate is the interactive key-chord UX only.
        match runtime.workspace_close_at(index) {
            Ok(killed) => {
                return Ok(format!(
                    "{{\"closed\":\"{workspace_id}\",\"killed\":{killed}}}"
                ));
            }
            Err(message) => {
                return Err(("usage", "NotFound", message));
            }
        }
    }
    if method == ipc_ctl::METHOD_FOCUS_WORKSPACE {
        let workspace_id = ipc_ctl::parse_workspace_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_workspace_id(&workspace_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        // CTX-0322: resolve the stable sequence id `ws:N` to the current slot.
        if let Some(index) = runtime.workspace_index_by_seq(u64::from(num)) {
            if runtime.workspace_switch(index) {
                return Ok(format!("{{\"focused\":\"{workspace_id}\"}}"));
            }
        }
        return Err((
            "usage",
            "NotFound",
            format!("no such workspace {workspace_id}"),
        ));
    }
    if method == ipc_ctl::METHOD_MOVE_WORKSPACE {
        // CTX-0259: non-interactive move of the focused window to `ws:N`.
        // No kill, no elevation beyond view.manage (authorized upstream);
        // unknown targets are NotFound with no partial state.
        let workspace_id = ipc_ctl::parse_workspace_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_workspace_id(&workspace_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        // CTX-0322: resolve the stable sequence id `ws:N` to the current slot.
        let Some(index) = runtime.workspace_index_by_seq(u64::from(num)) else {
            return Err((
                "usage",
                "NotFound",
                format!("no such workspace {workspace_id}"),
            ));
        };
        let from_seq = runtime
            .workspace_seq_at(runtime.active_workspace_index())
            .unwrap_or(u64::from(num));
        match runtime.workspace_move_focused_to(index) {
            Ok(moved) => {
                return Ok(format!(
                    "{{\"moved\":\"v:{}\",\"from\":\"ws:{from_seq}\",\"to\":\"{workspace_id}\"}}",
                    moved.0,
                ));
            }
            Err(message) => {
                // `from==to` no-ops succeed inside the runtime; every Err
                // here is a missing focus or an unsplittable source (no
                // partial state).
                return Err(("usage", "Conflict", message));
            }
        }
    }
    if method == ipc_ctl::METHOD_RENAME_WORKSPACE {
        // Issue #1333: rename a workspace display name. Display state only
        // (no session touched); unknown ids are NotFound with no partial
        // state, blank names are InvalidParams.
        let (workspace_id, name) = ipc_ctl::parse_workspace_rename_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_workspace_id(&workspace_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        // CTX-0322: resolve the stable sequence id `ws:N` to the current slot.
        let Some(index) = runtime.workspace_index_by_seq(u64::from(num)) else {
            return Err((
                "usage",
                "NotFound",
                format!("no such workspace {workspace_id}"),
            ));
        };
        match runtime.workspace_rename(index, &name) {
            Ok(()) => {
                // CTX-0979 CodeRabbit: report the stored name so trimming or
                // truncation applied by workspace_rename is visible to callers.
                let stored = runtime
                    .workspace_names()
                    .get(index)
                    .cloned()
                    .unwrap_or(name);
                return Ok(format!(
                    "{{\"renamed\":\"{workspace_id}\",\"name\":\"{}\"}}",
                    json_escape(&stored),
                ));
            }
            Err(message) => {
                return Err(("usage", "InvalidParams", message));
            }
        }
    }
    if method == ipc_ctl::METHOD_MOVE_PANEL {
        // Issue #1333: reposition the focused panel at a 1-based leaf
        // position within the active workspace. Never kills (sessions move
        // with the leaf); unknown positions are Conflict with no partial
        // state.
        let position = ipc_ctl::parse_move_panel_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let index = (position - 1) as usize;
        match runtime.workspace_move_focused_to_position(index) {
            Ok(moved) => {
                return Ok(format!(
                    "{{\"moved\":\"v:{}\",\"position\":{position}}}",
                    moved.0,
                ));
            }
            Err(message) => {
                return Err(("usage", "Conflict", message));
            }
        }
    }
    if method == super::request::METHOD_PANEL_CONTENT_PRESENT {
        // bitty#2388 (CTX-1913): the Core-owned debug producer. Core authors
        // the scene, so the call grants no `ui.panel` capability and no
        // plugin is involved; the receipt names the release token so a
        // script can restore the displaced terminal. The runtime keeps every
        // guarantee (frozen geometry, swallowed input, exact restore).
        let panel_id = super::request::parse_tab_params(params)
            .map_err(|err| ("usage", "InvalidParams", err))?;
        let num = super::request::parse_panel_id(&panel_id)
            .map_err(|err| ("usage", "InvalidParams", err))?;
        let view = ViewId::new(num);
        if !runtime.layout().leaf_ids().contains(&view) {
            // Same scope as `panel list`: a `p:N` outside the live layout is
            // not a listed panel, so it is NotFound with no partial state.
            return Err(("usage", "NotFound", format!("no such panel {panel_id}")));
        }
        let scene = debug_content_layer_scene(&panel_id)?;
        match runtime.present_panel_content(view, scene) {
            Ok(presented) => {
                return Ok(format!(
                    "{{\"presented\":true,\"panel_id\":\"{panel_id}\",\"view_id\":\"v:{}\",\"token\":\"{}\",\"mode\":\"{}\"}}",
                    presented.view.0,
                    presented.token.get(),
                    presented.presentation.as_str(),
                ));
            }
            Err(err) => {
                return Err(panel_content_refusal(&panel_id, &err));
            }
        }
    }
    if method == super::request::METHOD_PANEL_CONTENT_RELEASE {
        // bitty#2388 (CTX-1913): the release half of the debug pair. The
        // token is the present receipt's value, so the verb releases only a
        // layer its caller can name — never a live plugin's layer by mistake
        // (a plugin-owned row still refuses with a token mismatch).
        let (panel_id, token) = super::request::parse_panel_content_release_params(params)
            .map_err(|err| ("usage", "InvalidParams", err))?;
        let num = super::request::parse_panel_id(&panel_id)
            .map_err(|err| ("usage", "InvalidParams", err))?;
        let view = ViewId::new(num);
        let restores_before = runtime.content_restore_resizes();
        match runtime.release_panel_content(
            view,
            bitty_runtime::registry::PanelContentToken::from_ordinal(token),
        ) {
            Ok(restored) => {
                return Ok(format!(
                    "{{\"released\":true,\"panel_id\":\"{panel_id}\",\"view_id\":\"v:{}\",\"mode\":\"{}\",\"restore_resizes\":{}}}",
                    restored.view.0,
                    restored.prior_presentation.as_str(),
                    runtime
                        .content_restore_resizes()
                        .saturating_sub(restores_before),
                ));
            }
            Err(err) => {
                return Err(panel_content_refusal(&panel_id, &err));
            }
        }
    }
    if method == super::request::METHOD_PANEL_CONTENT_MODE {
        // bitty#2388 residue 3 (CTX-1931): the mode half of the debug pair.
        // Core owns the gesture, so the token-free form is fenced on the
        // layer's own token (the app thread is the only host mutator on this
        // drain); an explicit present-receipt token keeps the release-shaped
        // fence for a script that captured one. The change runs the runtime's
        // single mode gate, which stamps through the command registry, records
        // the mode, and refuses a placement-less mode typed.
        let (panel_id, mode_word, token) = super::request::parse_panel_content_mode_params(params)
            .map_err(|err| ("usage", "InvalidParams", err))?;
        let num = super::request::parse_panel_id(&panel_id)
            .map_err(|err| ("usage", "InvalidParams", err))?;
        let view = ViewId::new(num);
        if !runtime.layout().leaf_ids().contains(&view) {
            // Same scope as `panel list`: a `p:N` outside the live layout is
            // not a listed panel, so it is NotFound with no partial state.
            return Err(("usage", "NotFound", format!("no such panel {panel_id}")));
        }
        let Some(mode) = bitty_ui::PresentationMode::parse(&mode_word) else {
            // The params parser validated the word; a hand-built request that
            // reaches here still answers typed, never a panic.
            return Err((
                "usage",
                "InvalidParams",
                format!("unknown mode word {mode_word:?}"),
            ));
        };
        // The layer read doubles as the `changed` marker: the record's mode is
        // what the leaf carried before the gate runs, and the gate re-validates
        // everything against the live layer.
        let layer = runtime.panel_content_layer(view);
        let (layer_token, changed) = match &layer {
            Some(layer) => (layer.token, layer.presentation != mode),
            None => {
                // No layer is up for this panel: the same typed refusal the
                // runtime's gate produces, reported once here rather than
                // minting a dummy token for the gate to refuse.
                return Err((
                    "usage",
                    "NotFound",
                    format!("panel {panel_id} presents no content layer"),
                ));
            }
        };
        let token = match token {
            Some(explicit) => bitty_runtime::registry::PanelContentToken::from_ordinal(explicit),
            None => layer_token,
        };
        match runtime.set_panel_content_presentation(view, token, mode) {
            Ok(record) => {
                return Ok(format!(
                    "{{\"changed\":{changed},\"panel_id\":\"{panel_id}\",\"view_id\":\"v:{}\",\"mode\":\"{}\"}}",
                    record.view.0,
                    record.presentation.as_str(),
                ));
            }
            Err(err) => {
                return Err(panel_content_refusal(&panel_id, &err));
            }
        }
    }
    if method == ipc_ctl::METHOD_RELOAD_CONFIG {
        // CTX-0814 (#1397): when the app installed a live-reload context, an
        // authorized reload re-reads the sources, classifies the change
        // (live / restart-required / rejected), and adopts the live
        // presentation subset. Without a context (headless tests, `--safe`)
        // the CTX-0537 probe-only reply stands, so a client can never read a
        // probe as an applied reload.
        //
        // bitty#2927: `{"dry_run":true}` (`bitty reload --dry-run`) reports
        // the same change list without committing the plan or touching the
        // runtime. Params are bounded and strict.
        let dry_run = crate::config_reload::parse_reload_params(params)
            .map_err(|err| ("usage", "InvalidParams", err))?;
        if let Some(info) = crate::config_reload::reload_requested_with(runtime, dry_run) {
            return Ok(reload_config_json(&info));
        }
        // CTX-0537 (APP-001): probe-only until live hot-swap lands. The
        // response reports exactly what happened — `probed` true and
        // `applied` false — so a client can never read it as an applied
        // reload. `path` is the resolved config file (or a defaults marker)
        // and `hot_swap` names the follow-up.
        let probed = bitty_config::file::probe_config_path(None);
        let path = probed
            .as_ref()
            .map(|p| p.path.display().to_string())
            .unwrap_or_else(|| String::from("(defaults; no file)"));
        return Ok(format!(
            "{{\"probed\":true,\"applied\":false,\"path\":\"{}\",\"hot_swap\":\"follow-up\"}}",
            json_escape(&path)
        ));
    }
    if method == bitty_ipc::devtools::METHOD_TEST_EXIT {
        // CTX-0506: deterministic teardown for `bitty --test-mode`. The
        // dispatcher registers this verb only while test mode serves, and
        // authorization already required the elevated `debug.control`
        // scope; the loop polls the flag on the main thread and exits 0.
        crate::test_mode::request_exit();
        return Ok(String::from("{\"exiting\":true}"));
    }
    Err((
        "usage",
        "UnknownMethod",
        format!("unknown control method {method}"),
    ))
}

/// Encode a live-reload outcome as the `bitty.debug/reloadConfig` reply
/// (CTX-0814, #1397; result fields for `bitty reload` added by bitty#2927).
/// `reloaded` marks that the reload path ran; `applied` is false for a
/// restart-required, rejected, load, apply, or dry-run attempt, with `kind`
/// naming which. `changed` lists the conflicting field paths when a
/// classify-level report exists; `rejected` lists the dotted paths validation
/// refused (a non-empty list means the running plan is untouched);
/// `restart_required` lists the changed fields the runtime cannot adopt live
/// yet; `plugins_restart_required` lists the plugin ids whose enabled state in
/// the installed store differs from the launch set (bitty#2969; the plugin set
/// is not part of the config plan and applies only on restart);
/// `plan_version` is the running plan's monotonic version. A module
/// refusal adds the typed `module` object.
///
/// `pub(super)` so `ctl::tests` can pin the encoding without a runtime.
pub(super) fn reload_config_json(info: &crate::config_reload::ReloadOutcomeInfo) -> String {
    let mut changed = String::new();
    for (index, field) in info.changed.iter().enumerate() {
        if index > 0 {
            changed.push(',');
        }
        changed.push('"');
        changed.push_str(&json_escape(field));
        changed.push('"');
    }
    let mut rejected = String::new();
    for (index, field) in info.rejected.iter().enumerate() {
        if index > 0 {
            rejected.push(',');
        }
        rejected.push('"');
        rejected.push_str(&json_escape(field));
        rejected.push('"');
    }
    let mut restart_required = String::new();
    for (index, field) in info.restart_required.iter().enumerate() {
        if index > 0 {
            restart_required.push(',');
        }
        restart_required.push('"');
        restart_required.push_str(&json_escape(field));
        restart_required.push('"');
    }
    let mut plugins_restart_required = String::new();
    for (index, id) in info.plugins_restart_required.iter().enumerate() {
        if index > 0 {
            plugins_restart_required.push(',');
        }
        plugins_restart_required.push('"');
        plugins_restart_required.push_str(&json_escape(id));
        plugins_restart_required.push('"');
    }
    let message = info
        .message
        .as_ref()
        .map(|raw| format!(",\"message\":\"{}\"", json_escape(raw)))
        .unwrap_or_default();
    // Typed-outcome parity for the reload path: a module refusal carries its
    // contract record (code, phase, and the already allowlisted render). The
    // record is the part held to contract `N-6`: no host path, no canonical
    // root, no identity tuple. The envelope's own `path` field is unchanged
    // pre-existing surface and still emits the resolved entry file's absolute
    // path, so `N-6` scopes to `module.record`, not to the whole reply.
    let module = info
        .module
        .as_ref()
        .map(|diagnostic| {
            format!(
                ",\"module\":{{\"code\":\"{}\",\"phase\":\"{}\",\"record\":\"{}\"}}",
                json_escape(diagnostic.code().as_str()),
                json_escape(diagnostic.phase.as_str()),
                json_escape(&diagnostic.render())
            )
        })
        .unwrap_or_default();
    format!(
        "{{\"reloaded\":true,\"applied\":{},\"dry_run\":{},\"kind\":\"{}\",\"path\":\"{}\",\"changed\":[{}],\"rejected\":[{}],\"restart_required\":[{}],\"plugins_restart_required\":[{}],\"plan_version\":{}{message}{module}}}",
        info.applied,
        info.dry_run,
        json_escape(info.kind),
        json_escape(&info.path),
        changed,
        rejected,
        restart_required,
        plugins_restart_required,
        info.plan_version,
    )
}

/// Shared ctl creation core (CTX-0387): allocate a globally unique leaf id,
/// split `focused` along `axis`, validate the fresh view's appearance, and
/// commit the layout. Fail-closed: a refused split or appearance conflict
/// leaves the layout untouched. Returns the new leaf id plus its painted
/// content-frame size in cells (the allocation only when the fresh view has no
/// present frame yet).
fn create_split_leaf(
    runtime: &mut bitty_runtime::Runtime,
    focused: bitty_runtime::ViewId,
    axis: bitty_runtime::SplitAxis,
    place_new_first: bool,
) -> Result<(bitty_runtime::ViewId, u16, u16), (&'static str, &'static str, String)> {
    // CTX-0378: the id must be globally unique across every workspace slot,
    // not one past the active layout's max — `pane_sessions` is keyed
    // globally by `ViewId`, so a local scan aliases another workspace's shell.
    let new_id = runtime.next_view_id_global();
    let mut layout = runtime.layout().clone();
    let target = if layout.find_leaf(focused).is_some() {
        focused
    } else if let Some(plan) = runtime
        .panel_split_plan()
        .filter(|p| layout.find_leaf(p.target).is_some())
    {
        plan.target
    } else if let Some(first) = layout.leaf_ids().into_iter().next() {
        first
    } else {
        focused
    };
    if !split_leaf(&mut layout, target, axis, new_id, place_new_first) {
        return Err((
            "usage",
            "Conflict",
            String::from("focused view is not a splittable leaf"),
        ));
    }
    // CTX-0343 first match: a previously inert `ws:`/`view:` selector can
    // match the fresh `View` as `empty` content; fail the creation closed
    // before the layout commits it (the spawn helper re-checks the
    // `terminal` bind).
    if let Err(err) = runtime.validate_new_view_appearance(new_id) {
        return Err(("usage", "Conflict", format!("spawn refused: {err}")));
    }
    runtime.set_layout(layout);
    // CTX-1707 (#2896): size the shell to the decorated content frame the
    // pane actually paints, never the larger raw slot allocation. Spawning
    // at the allocation makes the PTY advertise rows the painter windows
    // away, so a full-screen TUI loses its top lines until an unrelated
    // geometry change re-syncs the pane. Same rule as the keymap
    // `new_split` path (`commit_new_leaf`); the allocation is a
    // degenerate-container fallback only.
    let (cols, rows) = runtime
        .presented_frame_cells(new_id)
        .or_else(|| {
            runtime
                .layout_allocations()
                .iter()
                .find(|(id, _)| *id == new_id)
                .map(|(_, rect)| (rect.width.max(1), rect.height.max(1)))
        })
        .unwrap_or((80, 24));
    Ok((new_id, cols, rows))
}

/// Spawns the private shell of ctl-created leaf `view` through the same
/// app-level path as the keymap `new_split` action (CTX-0387).
///
/// Replays the runtime's recorded primary attach recipe when a primary shell
/// attached (CTX-0359; keymap/`workspace_new` parity, explicit `--program`
/// tail included), otherwise resolves `$SHELL` > `/bin/sh` exactly like the
/// prior ctl spawn arm. The call itself is fail-closed; each verb chooses its
/// own failure policy (`terminal spawn` rolls the layout back, `view split`
/// keeps the empty pane best-effort like the keymap).
fn spawn_leaf_shell(
    runtime: &mut bitty_runtime::Runtime,
    view: bitty_runtime::ViewId,
    cols: u16,
    rows: u16,
    cwd: Option<&std::path::Path>,
) -> Result<(), bitty_runtime::RuntimeError> {
    let (program, program_args) = match runtime.primary_spawn_recipe() {
        Some((program, args)) => (Some(program.to_owned()), args.to_vec()),
        None => (None, Vec::new()),
    };
    let spec = crate::spawn::SpawnSpec {
        program,
        program_args,
        shell_env: std::env::var("SHELL").ok(),
        config_shell: None,
        config_shell_args: Vec::new(),
        startup_cwd: None,
    };
    crate::spawn::spawn_pane_shell_in(runtime, &spec, view, cols, rows, cwd)
}

/// Split the focused leaf (mirrors the composition-root helper).
fn split_leaf(
    layout: &mut bitty_runtime::LayoutNode,
    focused: bitty_runtime::ViewId,
    axis: bitty_runtime::SplitAxis,
    new_id: bitty_runtime::ViewId,
    place_new_first: bool,
) -> bool {
    use bitty_runtime::{LayoutNode, View};
    match layout {
        LayoutNode::Leaf(v) => {
            if v.id() != focused {
                return false;
            }
            let old = v.clone();
            let fresh = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
            let (first, second) = if place_new_first {
                (LayoutNode::leaf(fresh), LayoutNode::leaf(old))
            } else {
                (LayoutNode::leaf(old), LayoutNode::leaf(fresh))
            };
            *layout = LayoutNode::split(axis, 0.5, first, second);
            true
        }
        LayoutNode::Split { first, second, .. } => {
            split_leaf(first, focused, axis, new_id, place_new_first)
                || split_leaf(second, focused, axis, new_id, place_new_first)
        }
        LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|c| split_leaf(c, focused, axis, new_id, place_new_first)),
        LayoutNode::Overlay { base, overlay, .. } => {
            split_leaf(base, focused, axis, new_id, place_new_first)
                || split_leaf(overlay, focused, axis, new_id, place_new_first)
        }
    }
}

/// Serialize a slice of strings as a JSON string array for the control
/// surface.
///
/// Bounded by the caller (workspace slots are capped at 16); each item is
/// JSON-escaped and order is preserved.
fn json_string_array(items: &[String]) -> String {
    let mut out = String::from("[");
    for (idx, item) in items.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&json_escape(item));
        out.push('"');
    }
    out.push(']');
    out
}

/// The Core-authored debug scene for `ctl panel present` (bitty#2388,
/// CTX-1913).
///
/// One bounded text line naming the surface, so the layer is identifiable in
/// `terminal text` and by eye. Core owns the block row, no plugin identity or
/// generation is implicated, and the scene reuses the same SCN bounds every
/// other scene passes.
fn debug_content_layer_scene(
    panel_id: &str,
) -> Result<bitty_rich::scene::Scene, (&'static str, &'static str, String)> {
    let mut scene = bitty_rich::scene::Scene::new();
    let text = format!("bitty debug content layer (panel {panel_id})");
    let block = bitty_rich::scene::RichBlock::new(
        bitty_rich::scene::BlockId(1),
        bitty_rich::scene::BlockAnchor::Zone(1),
        bitty_rich::scene::SceneNode::Text(bitty_rich::scene::StyledSpan {
            text,
            bold: false,
            italic: false,
        }),
        bitty_rich::scene::ScrollBehavior::Inline,
        1,
        1,
        1,
    )
    .map_err(|err| {
        (
            "usage",
            "InvalidParams",
            format!("debug layer refused: {err}"),
        )
    })?;
    scene.insert(block).map_err(|err| {
        (
            "usage",
            "InvalidParams",
            format!("debug layer refused: {err}"),
        )
    })?;
    Ok(scene)
}

/// Map one typed `bitty_runtime::registry::PanelError` from the debug pair
/// onto the ctl error class (bitty#2388, CTX-1913).
///
/// "The caller named something that is not there" is `NotFound`, "the current
/// state forbids this" is `Conflict`, and both name the panel so a script can
/// tell its own bad target from a live layer it cannot touch. Never a silent
/// success.
fn panel_content_refusal(
    panel_id: &str,
    err: &bitty_runtime::registry::PanelError,
) -> (&'static str, &'static str, String) {
    let (code, detail) = match err {
        bitty_runtime::registry::PanelError::ContentUnavailable { reason, .. } => (
            "NotFound",
            format!("panel {panel_id} cannot change a content layer: {reason}"),
        ),
        bitty_runtime::registry::PanelError::ContentBusy { .. } => (
            "Conflict",
            format!("panel {panel_id} already presents a content layer"),
        ),
        bitty_runtime::registry::PanelError::ContentNotPresented { .. } => (
            "NotFound",
            format!("panel {panel_id} presents no content layer"),
        ),
        bitty_runtime::registry::PanelError::ContentTokenMismatch { token, .. } => (
            "Conflict",
            format!(
                "content token {token} does not name panel {panel_id}'s live layer \
                 (re-run `bitty ctl panel present` and pass its receipt token)"
            ),
        ),
        bitty_runtime::registry::PanelError::ContentPresentationUnsupported { mode, .. } => (
            "Conflict",
            format!(
                "panel {panel_id} cannot take mode {mode}: no in-tree placement for a \
                 presented layer"
            ),
        ),
        other => ("Conflict", format!("panel {panel_id} refused: {other}")),
    };
    ("usage", code, detail)
}

/// Extract printable text from a terminal snapshot (rows joined, bounded).
///
/// CTX-0321: `terminal text` must return the rendered grid, not a `Debug` dump
/// of the internal `Snapshot` struct. This reuses the canonical bounded
/// row-wise extraction the devtools `grid-text` snapshot exposes
/// ([`bitty_runtime::inspect::grid_text_from_snapshot_with_images`]):
/// wide-char spacers are skipped, trailing blanks are trimmed per row, and the
/// rows are newline joined. The joined result is bounded ([`bounded_text`])
/// and never panics on an unexpected grid.
///
/// Issue #2875 scope 6 (a11y/scene contract): a direct `a=p`/`a=T`
/// placement owns no grid cell, so its cells stay blank. Passing the
/// [footprints](bitty_runtime::inspect::ImageCellSpan) lets the shared
/// projection substitute the object-replacement character for exactly those
/// blank cells, so `bitty ctl terminal text` reports an image instead of an
/// empty hole. Grid text under an image is preserved. Callers with no image
/// plane pass an empty slice.
pub(super) fn snapshot_text_with_images(
    snapshot: &bitty_term_state::Snapshot,
    images: &[bitty_runtime::inspect::ImageCellSpan],
) -> String {
    let grid = bitty_runtime::inspect::grid_text_from_snapshot_with_images(
        snapshot,
        bitty_runtime::inspect::INSPECT_MAX_ROWS,
        bitty_runtime::inspect::INSPECT_MAX_COLS,
        images,
    );
    bounded_text(&grid.lines.join("\n"))
}

/// Bound terminal text for the response (16 KiB, char-boundary safe).
fn bounded_text(text: &str) -> String {
    const MAX: usize = 16 * 1024;
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut end = MAX;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Append JSON-escaped text (no surrounding quotes).
fn append_json_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_runtime::registry::{PanelRegistryConfig, PanelRuntime, PanelType, WorkspaceId};
    use bitty_runtime::{LayoutNode, Runtime, SplitAxis, UiRect, View, ViewId};

    /// Extract the `"token"` ordinal from a present receipt.
    ///
    /// The receipt is built by hand in this module, so the marker parse is
    /// exact; a missing or malformed token fails the test that asked.
    fn receipt_token(receipt: &str) -> u64 {
        const MARKER: &str = "\"token\":\"";
        let start = receipt
            .find(MARKER)
            .expect("the present receipt names its token")
            + MARKER.len();
        let end = start + receipt[start..].find('"').expect("the token is quoted");
        receipt[start..end]
            .parse::<u64>()
            .expect("the token is an ordinal")
    }

    /// One headless leaf with a mounted terminal panel and a grid line the
    /// displaced terminal owns.
    fn content_layer_fixture() -> (Runtime, ViewId) {
        let mut rt = Runtime::with_defaults().expect("runtime builds");
        let view = ViewId::new(1);
        rt.set_layout(LayoutNode::leaf(View::new(view, 80, 24)));
        rt.set_container(UiRect::new(0, 0, 80, 24));
        assert!(rt.set_focus(view), "the single leaf holds focus");
        rt.handle_pty_bytes(b"grid-line-one\r\n");
        let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host");
        let handle = host
            .create_panel(PanelType::Terminal, Some(WorkspaceId::new(1)))
            .expect("terminal panel");
        host.mount_panel(handle.id, handle.generation, view)
            .expect("panel mounts at the leaf");
        rt.install_scene_host(host);
        (rt, view)
    }

    /// bitty#2388 (CTX-1913): the Core-owned ctl debug pair, end to end
    /// through `apply_control` — the same dispatch the app-turn lane service
    /// runs.
    ///
    /// `panel present` swaps the leaf's terminal for the Core-authored debug
    /// layer, so `terminal text` reports the layer (not the frozen grid) and
    /// names the mode, `terminal send` answers the router's content-gate
    /// drop, the mode change to `floating` is observable through
    /// `panel list`, and `panel release` restores the displaced grid and the
    /// mode the layer displaced.
    #[test]
    fn ctl_debug_pair_presents_text_drops_input_and_releases_back() {
        let (mut rt, view) = content_layer_fixture();

        // 1. Present through the debug verb; the receipt names the token.
        let present = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_PRESENT,
            Some(&super::super::request::tab_params("p:1")),
        )
        .expect("panel present applies");
        assert!(present.contains("\"presented\":true"), "{present}");
        assert!(present.contains("\"panel_id\":\"p:1\""), "{present}");
        assert!(present.contains("\"mode\":\"tiled\""), "{present}");
        let token = receipt_token(&present);

        // 2. `terminal text` shows the layer, and says so.
        let text = apply_control(
            &mut rt,
            ipc_ctl::METHOD_GET_TERMINAL_TEXT,
            Some(&ipc_ctl::params_terminal_id("t:1")),
        )
        .expect("terminal text applies");
        assert!(
            text.contains("\"content\":\"layer\""),
            "the reply names the layer mode: {text}"
        );
        assert!(
            text.contains("bitty debug content layer (panel p:1)"),
            "the layer's own text is what the panel shows: {text}"
        );
        assert!(
            !text.contains("grid-line-one"),
            "the hidden grid is not what the panel shows: {text}"
        );

        // 3. `terminal send` answers the content-gate drop, and the dropped
        // bytes mirror into the process telemetry the stall/crash reports
        // read (bitty#2388 observability half).
        let before = bitty_render::telemetry::snapshot();
        let send = apply_control(
            &mut rt,
            ipc_ctl::METHOD_SEND_INPUT,
            Some(&ipc_ctl::params_send_input("t:1", "x")),
        )
        .expect("terminal send applies");
        assert!(
            send.contains("\"dropped\":\"content-layer\""),
            "the drop is named, not reported as a send: {send}"
        );
        assert!(send.contains("\"bytes\":0"), "{send}");
        let after = bitty_render::telemetry::snapshot();
        assert!(
            after.content_input_dropped_bytes > before.content_input_dropped_bytes,
            "the dropped bytes are mirrored into the report telemetry"
        );

        // 4. Mode change to `floating` through the Core-owned verb, observed
        // through `panel list`'s role row.
        rt.set_panel_content_presentation(
            view,
            bitty_runtime::registry::PanelContentToken::from_ordinal(token),
            bitty_ui::PresentationMode::Floating,
        )
        .expect("a presented panel may float");
        let panels = apply_control(&mut rt, super::super::request::METHOD_LIST_PANELS, None)
            .expect("panel list applies");
        assert!(
            panels.contains("\"role\":\"floating\""),
            "the floated layer is observable through ctl: {panels}"
        );

        // 5. Release through the debug verb: the grid and the mode it
        // started in come back.
        let release = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_RELEASE,
            Some(&super::super::request::panel_content_release_params(
                "p:1", token,
            )),
        )
        .expect("panel release applies");
        assert!(release.contains("\"released\":true"), "{release}");
        assert!(
            release.contains("\"mode\":\"tiled\""),
            "release returns the mode the layer displaced: {release}"
        );
        assert!(
            release.contains("\"restore_resizes\":"),
            "the receipt surfaces the restore-resize counter: {release}"
        );
        assert!(!rt.content_layer_presented(view), "no layer is left up");
        let text_after = apply_control(
            &mut rt,
            ipc_ctl::METHOD_GET_TERMINAL_TEXT,
            Some(&ipc_ctl::params_terminal_id("t:1")),
        )
        .expect("terminal text applies after release");
        assert!(
            text_after.contains("grid-line-one"),
            "release restores the displaced grid: {text_after}"
        );
        assert!(
            !text_after.contains("\"content\":\"layer\""),
            "the grid projection is back: {text_after}"
        );
        let panels_after = apply_control(&mut rt, super::super::request::METHOD_LIST_PANELS, None)
            .expect("panel list applies after release");
        assert!(
            panels_after.contains("\"role\":\"tiled\""),
            "the leaf is back in its own mode: {panels_after}"
        );
    }

    /// bitty#2388 (CTX-1913): the pair's typed refusals — an unknown panel,
    /// a second present, a mismatched token, and a release with nothing
    /// presented — all fail closed with the class the receipt documents and
    /// never a silent success.
    #[test]
    fn ctl_debug_pair_answers_typed_refusals() {
        let (mut rt, _view) = content_layer_fixture();

        // Unknown panel: same scope as `panel list`, so it is NotFound.
        let missing = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_PRESENT,
            Some(&super::super::request::tab_params("p:9")),
        )
        .expect_err("an unknown panel must refuse");
        assert_eq!(missing.1, "NotFound", "{missing:?}");

        // Malformed params are InvalidParams, never a defaulted target.
        let malformed = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_PRESENT,
            Some("{\"panel_id\":\"v:1\"}"),
        )
        .expect_err("a non-prefixed id must refuse");
        assert_eq!(malformed.1, "InvalidParams", "{malformed:?}");

        // Present once, then a second present is Conflict (ContentBusy).
        let present = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_PRESENT,
            Some(&super::super::request::tab_params("p:1")),
        )
        .expect("panel present applies");
        let token = receipt_token(&present);
        let busy = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_PRESENT,
            Some(&super::super::request::tab_params("p:1")),
        )
        .expect_err("one layer per panel");
        assert_eq!(busy.1, "Conflict", "{busy:?}");
        assert!(
            busy.2.contains("already presents"),
            "the refusal names the busy layer: {busy:?}"
        );

        // A mismatched token is Conflict and leaves the layer presented.
        let mismatch = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_RELEASE,
            Some(&super::super::request::panel_content_release_params(
                "p:1",
                token + 1,
            )),
        )
        .expect_err("a stale token must refuse");
        assert_eq!(mismatch.1, "Conflict", "{mismatch:?}");
        assert!(
            mismatch.2.contains("does not name"),
            "the refusal explains the token fence: {mismatch:?}"
        );
        assert!(rt.content_layer_presented(ViewId::new(1)));

        // The matching token still releases, and a release after the release
        // is NotFound (nothing is presented).
        let release = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_RELEASE,
            Some(&super::super::request::panel_content_release_params(
                "p:1", token,
            )),
        )
        .expect("the matching token releases");
        assert!(release.contains("\"released\":true"), "{release}");
        let gone = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_RELEASE,
            Some(&super::super::request::panel_content_release_params(
                "p:1", token,
            )),
        )
        .expect_err("a released layer is gone");
        assert_eq!(gone.1, "NotFound", "{gone:?}");
    }

    /// bitty#2388 residue 3 (CTX-1931): the mode half of the debug pair —
    /// `panel mode` moves a presented layer between tiled and floating
    /// through `apply_control` (the same dispatch the app-turn lane service
    /// runs), answers every refusal typed, and a redundant request is a
    /// no-op receipt.
    #[test]
    fn ctl_mode_verb_flips_the_layer_mode_and_types_every_refusal() {
        let (mut rt, view) = content_layer_fixture();

        // Present through the debug verb; the receipt names the token.
        let present = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_PRESENT,
            Some(&super::super::request::tab_params("p:1")),
        )
        .expect("panel present applies");
        let token = receipt_token(&present);

        // Token-free: the verb is fenced on the layer's own token, and the
        // receipt names the mode it moved the layer to.
        let mode = apply_control(
            &mut rt,
            "bitty.debug/panelContentMode",
            Some("{\"panel_id\":\"p:1\",\"mode\":\"floating\"}"),
        )
        .expect("panel mode applies");
        assert!(mode.contains("\"changed\":true"), "{mode}");
        assert!(mode.contains("\"panel_id\":\"p:1\""), "{mode}");
        assert!(mode.contains("\"view_id\":\"v:1\""), "{mode}");
        assert!(mode.contains("\"mode\":\"floating\""), "{mode}");

        // `panel list`'s role row flips with the leaf.
        let panels = apply_control(&mut rt, super::super::request::METHOD_LIST_PANELS, None)
            .expect("panel list applies");
        assert!(
            panels.contains("\"role\":\"floating\""),
            "the floated layer is observable through ctl: {panels}"
        );

        // The layer is still up, so no restore ran and no resize was owed:
        // the displaced terminal keeps the geometry the freeze owes it.
        assert_eq!(
            rt.content_restore_resizes(),
            0,
            "a mode change never pays a restore resize"
        );

        // A redundant same-mode request is a no-op receipt.
        let again = apply_control(
            &mut rt,
            "bitty.debug/panelContentMode",
            Some("{\"panel_id\":\"p:1\",\"mode\":\"floating\"}"),
        )
        .expect("a redundant request is admitted as a no-op");
        assert!(again.contains("\"changed\":false"), "{again}");
        assert!(again.contains("\"mode\":\"floating\""), "{again}");

        // A wrong token is a typed refusal, and nothing moved.
        let mismatch = apply_control(
            &mut rt,
            "bitty.debug/panelContentMode",
            Some("{\"panel_id\":\"p:1\",\"mode\":\"floating\",\"token\":\"999\"}"),
        )
        .expect_err("a stale token must refuse");
        assert_eq!(mismatch.1, "Conflict", "{mismatch:?}");
        assert!(
            mismatch.2.contains("does not name"),
            "the refusal explains the token fence: {mismatch:?}"
        );

        // The fenced form with the present receipt's token flips the layer
        // back, proving the explicit-token path runs the same single gate.
        let back = apply_control(
            &mut rt,
            "bitty.debug/panelContentMode",
            Some(&format!(
                "{{\"panel_id\":\"p:1\",\"mode\":\"tiled\",\"token\":\"{token}\"}}"
            )),
        )
        .expect("the fenced form applies");
        assert!(back.contains("\"changed\":true"), "{back}");
        assert!(back.contains("\"mode\":\"tiled\""), "{back}");

        // A mode with no in-tree placement is an honest typed refusal —
        // never a panic, never a silent no-op — and it names the mode.
        let unsupported = apply_control(
            &mut rt,
            "bitty.debug/panelContentMode",
            Some("{\"panel_id\":\"p:1\",\"mode\":\"fullscreen\"}"),
        )
        .expect_err("fullscreen has no in-tree placement");
        assert_eq!(unsupported.1, "Conflict", "{unsupported:?}");
        assert!(
            unsupported.2.contains("fullscreen"),
            "the refusal names the refused mode: {unsupported:?}"
        );
        assert_eq!(
            rt.view_presentation(view),
            Some(bitty_ui::PresentationMode::Tiled),
            "the refused request moved nothing"
        );

        // An unknown panel is NotFound with no partial state.
        let missing = apply_control(
            &mut rt,
            "bitty.debug/panelContentMode",
            Some("{\"panel_id\":\"p:9\",\"mode\":\"floating\"}"),
        )
        .expect_err("an unknown panel must refuse");
        assert_eq!(missing.1, "NotFound", "{missing:?}");
        assert!(missing.2.contains("no such panel p:9"), "{missing:?}");

        // After the release there is no layer to move, so the verb answers
        // the typed no-layer refusal instead of a fall-through mutation.
        let release = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_RELEASE,
            Some(&super::super::request::panel_content_release_params(
                "p:1", token,
            )),
        )
        .expect("panel release applies");
        assert!(release.contains("\"released\":true"), "{release}");
        let no_layer = apply_control(
            &mut rt,
            "bitty.debug/panelContentMode",
            Some("{\"panel_id\":\"p:1\",\"mode\":\"floating\"}"),
        )
        .expect_err("a panel with no layer must refuse");
        assert_eq!(no_layer.1, "NotFound", "{no_layer:?}");
        assert!(
            no_layer.2.contains("presents no content layer"),
            "{no_layer:?}"
        );
        assert_eq!(
            rt.view_presentation(view),
            Some(bitty_ui::PresentationMode::Tiled),
            "the released panel keeps the mode the layer gave back"
        );
    }

    /// bitty#2388 residue 3 (CTX-1931): the release-after-mode row — the
    /// release receipt returns the mode the layer DISPLACED (tiled), and the
    /// restore pays exactly one resize for the reflow the freeze deferred
    /// while the layer floated.
    #[test]
    #[cfg(unix)]
    fn ctl_mode_then_release_returns_the_displaced_mode_with_one_restore_resize() {
        bitty_test_support::require_pty!();
        let mut rt = Runtime::with_defaults().expect("runtime builds");
        let view = ViewId::new(1);
        rt.set_layout(LayoutNode::leaf(View::new(view, 80, 24)));
        rt.set_container(UiRect::new(0, 0, 80, 24));
        assert!(rt.set_focus(view), "the single leaf holds focus");
        rt.spawn_shell_for_view(view, "/bin/sh", &["-c", "sleep 30"], 40, 12)
            .expect("the leaf owns a shell");
        rt.handle_pane_bytes(view, b"grid-line-one\r\n");
        rt.tick().expect("the pane presents a frame");
        let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host");
        let handle = host
            .create_panel(PanelType::Terminal, Some(WorkspaceId::new(1)))
            .expect("terminal panel");
        host.mount_panel(handle.id, handle.generation, view)
            .expect("panel mounts at the leaf");
        rt.install_scene_host(host);

        // Present through the debug verb, then float the layer through the
        // mode verb: the hidden PTY cannot tell the mode moved.
        let present = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_PRESENT,
            Some(&super::super::request::tab_params("p:1")),
        )
        .expect("panel present applies");
        let token = receipt_token(&present);
        let frozen = rt.pane_pty_size(&view).expect("live winsize");

        let mode = apply_control(
            &mut rt,
            "bitty.debug/panelContentMode",
            Some("{\"panel_id\":\"p:1\",\"mode\":\"floating\"}"),
        )
        .expect("panel mode applies");
        assert!(mode.contains("\"changed\":true"), "{mode}");
        assert_eq!(
            rt.pane_pty_size(&view),
            Some(frozen),
            "the mode change never resizes the hidden PTY"
        );
        assert_eq!(
            rt.content_restore_resizes(),
            0,
            "no restore ran while the layer was up"
        );

        // The gesture the freeze exists for: the window moves, the hidden
        // PTY does not.
        rt.handle_resize(bitty_platform::PhysicalSize::new(1200, 700))
            .expect("window resize applies");
        let frame = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == view)
            .expect("the layer's frame");
        assert_ne!(
            (frame.cols, frame.rows),
            frozen,
            "the gesture must move the leaf, or the restore resize proves nothing"
        );
        assert_eq!(
            rt.pane_pty_size(&view),
            Some(frozen),
            "the frozen PTY keeps the winsize the layer took"
        );

        // Release: the receipt returns the mode the layer DISPLACED and
        // exactly one restore resize pays the deferred reflow.
        let release = apply_control(
            &mut rt,
            super::super::request::METHOD_PANEL_CONTENT_RELEASE,
            Some(&super::super::request::panel_content_release_params(
                "p:1", token,
            )),
        )
        .expect("panel release applies");
        assert!(
            release.contains("\"mode\":\"tiled\""),
            "the receipt returns the displaced mode: {release}"
        );
        assert!(
            release.contains("\"restore_resizes\":1"),
            "exactly one restore resize: {release}"
        );
        assert_eq!(
            rt.view_presentation(view),
            Some(bitty_ui::PresentationMode::Tiled),
            "release gives the displaced mode back"
        );
        let restored_frame = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == view)
            .expect("the restored frame");
        assert_eq!(
            rt.pane_pty_size(&view),
            Some((restored_frame.cols, restored_frame.rows)),
            "the PTY came back at the current content rect"
        );
    }

    /// bitty#3250 review: `terminal send` must not answer `sent_to`/`bytes` for
    /// a write the router's content gate drops. The panel in front of the
    /// target leaf presents a GUI content layer, so the verb reports the drop
    /// and no byte reaches the headless buffer.
    #[test]
    fn ctl_send_reports_a_content_layer_drop_instead_of_a_send() {
        let mut rt = Runtime::with_defaults().expect("runtime builds");
        let view = ViewId::new(1);
        rt.set_layout(LayoutNode::leaf(View::new(view, 80, 24)));
        rt.set_container(UiRect::new(0, 0, 80, 24));
        assert!(rt.set_focus(view), "the single leaf holds focus");
        let mut host = PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host");
        let handle = host
            .create_panel(PanelType::Terminal, Some(WorkspaceId::new(1)))
            .expect("terminal panel");
        host.mount_panel(handle.id, handle.generation, view)
            .expect("panel mounts at the leaf");
        rt.install_scene_host(host);
        let mut scene = bitty_rich::scene::Scene::new();
        scene
            .insert(
                bitty_rich::scene::RichBlock::new(
                    bitty_rich::scene::BlockId(1),
                    bitty_rich::scene::BlockAnchor::Zone(1),
                    bitty_rich::scene::SceneNode::Text(bitty_rich::scene::StyledSpan {
                        text: "dashboard".to_string(),
                        bold: false,
                        italic: false,
                    }),
                    bitty_rich::scene::ScrollBehavior::Inline,
                    1,
                    1,
                    1,
                )
                .expect("block fits"),
            )
            .expect("scene admits the block");
        rt.present_panel_content(view, scene)
            .expect("the primary owner is a terminal-backed panel");

        let params = ipc_ctl::params_send_input("t:1", "hello\n");
        let body = apply_control(&mut rt, ipc_ctl::METHOD_SEND_INPUT, Some(&params))
            .expect("terminal send applies");
        assert!(
            body.contains("\"dropped\":\"content-layer\""),
            "the drop is named, not reported as a send: {body}"
        );
        assert!(
            body.contains("\"bytes\":0"),
            "a dropped write reports zero delivered bytes: {body}"
        );
        assert_eq!(
            rt.pending_input_len(),
            0,
            "a dropped write is not buffered headless"
        );
        assert_eq!(
            rt.content_input_dropped_bytes(),
            b"hello\n".len() as u64,
            "the dropped write is counted at the content gate"
        );
    }

    #[test]
    fn ctl_send_text_close_and_split_support_detached_float_and_pinned_views() {
        let mut rt = Runtime::with_defaults().expect("runtime builds");
        rt.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        ));
        let v1 = ViewId::new(1);
        rt.float_detach(v1).expect("detach v1 into float store");
        assert!(rt.set_focus(v1));
        rt.handle_pty_bytes(b"float-output-line\r\n");

        // 1. `terminal text` succeeds for a detached float.
        let text_params = ipc_ctl::params_terminal_id("t:1");
        let text_json = apply_control(
            &mut rt,
            ipc_ctl::METHOD_GET_TERMINAL_TEXT,
            Some(&text_params),
        )
        .expect("terminal text on detached float must succeed");
        assert!(text_json.contains("float-output-line"));

        // 2. `terminal send` succeeds when the detached float is focused.
        let send_params = ipc_ctl::params_send_input("t:1", "hello\n");
        let send_json = apply_control(&mut rt, ipc_ctl::METHOD_SEND_INPUT, Some(&send_params))
            .expect("terminal send on focused detached float must succeed");
        assert!(send_json.contains("\"sent_to\":\"t:1\""));

        // 3. `terminal close` on a session-less detached float returns Conflict (not NotFound).
        let close_err = apply_control(&mut rt, ipc_ctl::METHOD_CLOSE_TERMINAL, Some(&text_params))
            .expect_err("session-less detached float must return Conflict");
        assert_eq!(close_err.1, "Conflict");

        // 4. Restore and pin v1, then verify `terminal text`, `terminal send`, and `view split`.
        rt.float_restore(v1).expect("restore float");
        rt.pin_floating(v1).expect("pin v1");
        assert!(rt.set_focus(v1));
        let pinned_text = apply_control(
            &mut rt,
            ipc_ctl::METHOD_GET_TERMINAL_TEXT,
            Some(&text_params),
        )
        .expect("terminal text on pinned view must succeed");
        assert!(pinned_text.contains("\"terminal_id\":\"t:1\""));
        let pinned_send = apply_control(&mut rt, ipc_ctl::METHOD_SEND_INPUT, Some(&send_params))
            .expect("terminal send on focused pinned view must succeed");
        assert!(pinned_send.contains("\"sent_to\":\"t:1\""));

        let split_params = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
        let split_json = apply_control(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split_params))
            .expect("view split while pinned view is focused must split tiled base");
        assert!(split_json.contains("\"new_view\":\"v:3\""));
    }
}
