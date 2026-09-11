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

/// Granted scopes for the live servo: CLI default plus explicit elevation.
#[must_use]
pub fn granted_scopes_for_servo() -> bitty_ipc::ScopeSet {
    ipc_ctl::elevation_from_env(std::env::var("BITTY_CTL_ELEVATE").ok().as_deref())
}

/// Drain the global queue, applying each action to `runtime`.
///
/// Called on the main thread between ticks. Each action is re-authorized
/// against `granted` before any mutation (defense in depth: the enqueue
/// path already authorized). Returns the number drained.
pub fn drain_global_control_queue(
    runtime: &mut bitty_runtime::Runtime,
    granted: &bitty_ipc::ScopeSet,
) -> usize {
    let mut count = 0usize;
    loop {
        let Some(item) = ipc_ctl::pop_pending_control() else {
            break;
        };
        count += 1;
        let reply = apply_control_envelope(runtime, &item.method, item.params.as_deref(), granted);
        let _ = item.reply.send(reply);
    }
    count
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
    match err {
        bitty_ipc::IpcError::ScopeDenied { .. } => (
            "auth",
            "ScopeDenied",
            format!("permission denied: {err} (needs elevation via BITTY_CTL_ELEVATE)"),
        ),
        // Generic denials and unauthenticated peers are permission failures
        // (CLI exit 7), never transport timeouts: surface the denial with
        // the elevation hint instead of exit 6.
        bitty_ipc::IpcError::Denied { code, reason } => (
            "auth",
            "Denied",
            format!("permission denied: [{code}] {reason} (needs elevation via BITTY_CTL_ELEVATE)"),
        ),
        bitty_ipc::IpcError::Unauthenticated { .. } => (
            "auth",
            "Unauthenticated",
            format!("permission denied: {err}"),
        ),
        bitty_ipc::IpcError::NotFound { .. } => ("usage", "NotFound", format!("{err}")),
        bitty_ipc::IpcError::InvalidMethod { .. } => ("usage", "InvalidMethod", format!("{err}")),
        bitty_ipc::IpcError::InvalidRequest { .. } => ("usage", "InvalidParams", format!("{err}")),
        bitty_ipc::IpcError::LimitExceeded { .. } => {
            ("transport", "PayloadTooLarge", format!("{err}"))
        }
        _ => ("transport", "Transport", format!("{err}")),
    }
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
    if method == ipc_ctl::METHOD_LIST_TERMINALS {
        // 1:1 terminal:view mapping until the registry lands: each leaf is
        // one terminal `t:<view>`. CTX-0284: tiles are layout-derived and may
        // have no shell (ctl splits spawn none), so report live pane-session
        // presence per entry instead of implying one shell per tile.
        let ids = runtime.layout().leaf_ids();
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
        out.push_str("]}");
        return Ok(out);
    }
    if method == ipc_ctl::METHOD_SEND_INPUT {
        let (terminal_id, text) = ipc_ctl::parse_send_params(params)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let num = ipc_ctl::parse_terminal_id(&terminal_id)
            .map_err(|err| ("usage", "InvalidParams", format!("{err}")))?;
        let target_view = ViewId::new(u64::from(num));
        if !runtime.layout().leaf_ids().contains(&target_view) {
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
        runtime.push_input_bytes(text.as_bytes());
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
        if !runtime.layout().leaf_ids().contains(&target_view) {
            return Err((
                "usage",
                "NotFound",
                format!("no such terminal {terminal_id}"),
            ));
        }
        // Prefer the pane snapshot when present; fall back to the primary
        // snapshot for a session-less leaf that IS the focused view
        // (parity with the present.rs CTX-0234 focused-only branch, which
        // paints the shared primary grid exactly there). Unfocused
        // session-less leaves stay empty so one grid is never duplicated as
        // text across tiles — deliberately NOT mirroring the CTX-0255
        // co-paint arm (PX-1676), which is a separate present-layer concern.
        // Single-leaf keeps the legacy fallback (the sole leaf owns focus).
        let text = runtime
            .pane_snapshot(&target_view)
            .map(|snap| snapshot_text(&snap))
            .or_else(|| {
                if runtime.focused_view() == Some(target_view)
                    || runtime.layout().leaf_ids().len() == 1
                {
                    Some(snapshot_text(&runtime.snapshot()))
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
            if !std::path::Path::new(dir).is_dir() {
                return Err((
                    "transport",
                    "Transport",
                    format!("spawn --cwd {dir:?} is not a directory"),
                ));
            }
            // Accepted + validated, but the spawn below does not chdir yet
            // (Runtime::spawn_shell has no cwd seam); the client already
            // warned on stderr, and the result names the gap.
        }
        let shell = std::env::var("SHELL").ok().filter(|s| !s.trim().is_empty());
        let program = shell.as_deref().unwrap_or("/bin/sh");
        runtime
            .spawn_shell(program)
            .map_err(|err| ("transport", "Transport", format!("spawn failed: {err}")))?;
        return Ok(String::from("{\"spawned\":true}"));
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
        // No pane session: refuse the last leaf so the layout is never
        // stranded empty; otherwise report NotFound.
        if runtime.layout().leaf_ids().contains(&target_view) {
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
        let next_id = runtime
            .layout()
            .leaf_ids()
            .iter()
            .map(|id| id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let new_id = ViewId::new(next_id.max(1));
        let mut layout = runtime.layout().clone();
        if !split_leaf(&mut layout, focused, axis, new_id, place_new_first) {
            return Err((
                "usage",
                "Conflict",
                String::from("focused view is not a splittable leaf"),
            ));
        }
        runtime.set_layout(layout);
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
        // Tabline truth: names + indices + focused marker + count, the same
        // string the overlay renders (`workspaceline_text`).
        let names = workspace_names_json(runtime);
        let active = runtime.active_workspace_index() + 1;
        return Ok(format!(
            "{{\"workspaces\":{names},\"active\":{active},\"count\":{},\"tabline\":\"{}\"}}",
            runtime.workspace_count(),
            json_escape(&runtime.workspaceline_text()),
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
                return Ok(format!(
                    "{{\"created\":\"ws:{seq}\",\"tabline\":\"{}\"}}",
                    json_escape(&runtime.workspaceline_text()),
                ));
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
                    "{{\"closed\":\"{workspace_id}\",\"killed\":{killed},\"tabline\":\"{}\"}}",
                    json_escape(&runtime.workspaceline_text()),
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
                return Ok(format!(
                    "{{\"focused\":\"{workspace_id}\",\"tabline\":\"{}\"}}",
                    json_escape(&runtime.workspaceline_text()),
                ));
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
                    "{{\"moved\":\"v:{}\",\"from\":\"ws:{from_seq}\",\"to\":\"{workspace_id}\",\"tabline\":\"{}\"}}",
                    moved.0,
                    json_escape(&runtime.workspaceline_text()),
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
    if method == ipc_ctl::METHOD_RELOAD_CONFIG {
        // Validate the config file (same probe the startup path uses) and
        // report its path; live hot-swap is a documented follow-up.
        let probed = bitty_config::file::probe_config_path(None);
        let path = probed
            .as_ref()
            .map(|p| p.path.display().to_string())
            .unwrap_or_else(|| String::from("(defaults; no file)"));
        return Ok(format!(
            "{{\"reloaded\":true,\"path\":\"{}\",\"hot_swap\":\"follow-up\"}}",
            json_escape(&path)
        ));
    }
    Err((
        "usage",
        "UnknownMethod",
        format!("unknown control method {method}"),
    ))
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

/// Workspace names as a JSON string array for `workspace list`.
///
/// Bounded: at most 16 names, each JSON-escaped. Names are runtime-issued
/// (`ws{seq}`) until rename lands as a follow-up.
fn workspace_names_json(runtime: &bitty_runtime::Runtime) -> String {
    let mut out = String::from("[");
    for (idx, name) in runtime.workspace_names().iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&json_escape(name));
        out.push('"');
    }
    out.push(']');
    out
}

/// Extract printable text from a terminal snapshot (rows joined, bounded).
///
/// CTX-0321: `terminal text` must return the rendered grid, not a `Debug` dump
/// of the internal `Snapshot` struct. This reuses the canonical bounded
/// row-wise extraction the devtools `grid-text` snapshot exposes
/// ([`bitty_runtime::inspect::grid_text_from_snapshot`]): wide-char spacers are
/// skipped, trailing blanks are trimmed per row, and the rows are newline
/// joined. The joined result is bounded ([`bounded_text`]) and never panics on
/// an unexpected grid.
pub(super) fn snapshot_text(snapshot: &bitty_term_state::Snapshot) -> String {
    let grid = bitty_runtime::inspect::grid_text_from_snapshot(
        snapshot,
        bitty_runtime::inspect::INSPECT_MAX_ROWS,
        bitty_runtime::inspect::INSPECT_MAX_COLS,
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
