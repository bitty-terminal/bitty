//! Chrome-key intercept for the `bitty-app` composition root (CTX-0233).
//!
//! Pure-move extraction from `main.rs`: the keymap-driven single-owner
//! rule (CTX-0153), press-to-release `chrome_held` ownership (CTX-0229),
//! chord validation/routing helpers, layout-surgery helpers behind chrome
//! actions, and their unit tests. No behavior change: resolution order,
//! ownership bounds, and refusal warnings are byte-identical to `main.rs`.
//!
//! `TerminalApp` itself stays in `main.rs`; this module implements its
//! chrome methods via `impl TerminalApp` and shares `AppModifiers`
//! as `pub(crate)`.

#![forbid(unsafe_code)]

use bitty_platform::{KeyEvent, LogicalKey, NamedKey, PressState, WindowEventKind};
use bitty_runtime::{FocusDirection, LayoutNode, SplitAxis, View, ViewId, WsCloseRequest};

use super::{TerminalApp, spawn_pane_shell};

/// App-side modifier mirror for keymap matching (CTX-0153).
///
/// `KeyEvent` carries no modifier field (modifiers arrive as separate
/// `ModifiersChanged` events plus modifier key presses), and `Runtime` keeps
/// its own tracker for PTY encoding. The app mirrors the same stream so a
/// bound chord (`alt+h`, `ctrl+tab`, ...) resolves before routing; both
/// trackers stay in sync because modifier-only keys and `ModifiersChanged`
/// are always routed to `Runtime` and never consumed as chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct AppModifiers {
    /// Shift held.
    shift: bool,
    /// Control held.
    control: bool,
    /// Alt held.
    alt: bool,
    /// Super held.
    super_held: bool,
}

// ---------------------------------------------------------------------------
// Keymap-driven chrome keys (CTX-0153 single-owner rule)
// ---------------------------------------------------------------------------

/// True for modifier-only keys: always routed to `Runtime` (its modifier
/// tracker needs them) and never treated as chrome.
pub(crate) fn is_modifier_key(key: &KeyEvent) -> bool {
    matches!(
        &key.logical_key,
        LogicalKey::Named(
            NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::Super
                | NamedKey::Meta
        )
    )
}

/// Mirror the modifier stream into the app snapshot (same rules as
/// `Runtime::track_modifiers_from_key`): modifier key presses latch, releases
/// unlatch. Pure over the event; total.
pub(crate) fn track_app_modifiers(mods: &mut AppModifiers, key: &KeyEvent) {
    if let LogicalKey::Named(named) = &key.logical_key {
        let pressed = key.state == PressState::Pressed;
        match named {
            NamedKey::Shift => mods.shift = pressed,
            NamedKey::Control => mods.control = pressed,
            NamedKey::Alt | NamedKey::AltGraph => mods.alt = pressed,
            NamedKey::Super | NamedKey::Meta | NamedKey::Hyper => mods.super_held = pressed,
            _ => {}
        }
    }
}

/// Clear the app modifier mirror on window focus transitions (CTX-0187
/// exit B root-cause fix).
///
/// The mirror latches `Shift`/`Control`/`Alt` from modifier key presses and
/// `ModifiersChanged` snapshots. When the window loses focus, key releases
/// that happen while unfocused are never delivered, so a latched `true` goes
/// stale and a later bare `Ctrl+V` would falsely match the `Ctrl+Shift+V`
/// paste chord (single-owner leak). Resetting to a clean slate on both loss
/// (`focused=false`) and regain (`focused=true`) fails closed to shell input:
/// the authoritative `ModifiersChanged` stream re-latches the true physical
/// state before the next chord on Wayland/winit, and until then an unshifted
/// `Ctrl+V` correctly reaches the shell instead of stealing paste. The worst
/// case without a fresh snapshot is a missed paste (retryable), never stolen
/// shell bytes.
pub(crate) fn clear_app_modifiers_on_focus(mods: &mut AppModifiers, _focused: bool) {
    *mods = AppModifiers::default();
}

/// Convert a key press plus the app modifier mirror into a matchable
/// [`bitty_config::KeyRef`]. Returns `None` for keys with no chord identity
/// (dead keys, unidentified, media/modifier leftovers, non-ASCII text), which
/// always route to the PTY. Single characters are lowercased so `Shift+Alt+H`
/// matches the `shift+alt+h` chord.
///
/// CTX-0187 exit B: the `shift` bit comes verbatim from the compositor-fed
/// mirror (`ModifiersChanged` physical state plus modifier key presses,
/// cleared on focus transitions above) — never inferred from character case.
/// A real `Ctrl+Shift+V` therefore pastes whether the platform reports it as
/// uppercase `"V"` or lowercase `"v"` with `shift=true`; only a physically
/// unshifted `Ctrl+V` (`shift=false`) stays shell input as `0x16`. This has
/// no silent-breakage mode: trusting the raw modifier bit preserves every
/// real chord, and staleness is handled by the focus clear, not by guessing
/// from case.
pub(crate) fn key_ref_from_event(
    key: &KeyEvent,
    mods: &AppModifiers,
) -> Option<bitty_config::KeyRef> {
    use bitty_config::{KeyName, KeyRef};
    let name = match &key.logical_key {
        LogicalKey::Character(s) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_graphic() => KeyName::Char(c.to_ascii_lowercase()),
                _ => return None,
            }
        }
        LogicalKey::Named(named) => match named {
            NamedKey::Tab => KeyName::Tab,
            NamedKey::Enter => KeyName::Enter,
            NamedKey::Escape => KeyName::Escape,
            NamedKey::Space => KeyName::Space,
            NamedKey::Backspace => KeyName::Backspace,
            NamedKey::Delete => KeyName::Delete,
            NamedKey::Insert => KeyName::Insert,
            NamedKey::Home => KeyName::Home,
            NamedKey::End => KeyName::End,
            NamedKey::PageUp => KeyName::PageUp,
            NamedKey::PageDown => KeyName::PageDown,
            NamedKey::ArrowUp => KeyName::Up,
            NamedKey::ArrowDown => KeyName::Down,
            NamedKey::ArrowLeft => KeyName::Left,
            NamedKey::ArrowRight => KeyName::Right,
            _ => {
                // Function keys `F1`..=`F35` share the `F<n>` debug spelling;
                // everything else (modifiers, media, `Other`) has no chord
                // identity and routes to the PTY.
                let spelled = format!("{named:?}");
                let n = spelled.strip_prefix('F')?;
                match n.parse::<u8>() {
                    Ok(num) if (1..=35).contains(&num) => KeyName::F(num),
                    _ => return None,
                }
            }
        },
        LogicalKey::Dead(_) | LogicalKey::Unidentified => return None,
    };
    Some(KeyRef {
        key: name,
        ctrl: mods.control,
        alt: mods.alt,
        shift: mods.shift,
        super_held: mods.super_held,
    })
}

/// Map a split direction onto focus movement.
pub(crate) fn split_dir_to_focus(dir: bitty_config::SplitDir) -> FocusDirection {
    match dir {
        bitty_config::SplitDir::Left => FocusDirection::Left,
        bitty_config::SplitDir::Right => FocusDirection::Right,
        bitty_config::SplitDir::Up => FocusDirection::Up,
        bitty_config::SplitDir::Down => FocusDirection::Down,
    }
}

/// Map a split direction onto the axis a new split divides.
pub(crate) fn split_dir_to_axis(dir: bitty_config::SplitDir) -> SplitAxis {
    match dir {
        bitty_config::SplitDir::Left | bitty_config::SplitDir::Right => SplitAxis::Horizontal,
        bitty_config::SplitDir::Up | bitty_config::SplitDir::Down => SplitAxis::Vertical,
    }
}

/// Fresh view id: one past the current maximum (total; empty layouts yield 1).
pub(crate) fn next_view_id(layout: &LayoutNode) -> ViewId {
    let max = layout.leaf_ids().iter().map(|id| id.0).max().unwrap_or(0);
    ViewId::new(max.saturating_add(1).max(1))
}

/// Split the focused leaf along `axis`, keeping the focused view and adding a
/// fresh sibling. The new pane goes first for `Left`/`Up`, second otherwise.
/// Returns false when the focused id is not in the tree.
pub(crate) fn split_focused_leaf(
    layout: &mut LayoutNode,
    focused: ViewId,
    axis: SplitAxis,
    new_id: ViewId,
    place_new_first: bool,
) -> bool {
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
            split_focused_leaf(first, focused, axis, new_id, place_new_first)
                || split_focused_leaf(second, focused, axis, new_id, place_new_first)
        }
        LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|c| split_focused_leaf(c, focused, axis, new_id, place_new_first)),
        LayoutNode::Overlay { base, overlay, .. } => {
            split_focused_leaf(base, focused, axis, new_id, place_new_first)
                || split_focused_leaf(overlay, focused, axis, new_id, place_new_first)
        }
    }
}

/// Remove the focused leaf, promoting its sibling. Refuses the last leaf so
/// the layout is never stranded empty. Returns false when refused or missing.
pub(crate) fn close_focused_leaf(layout: &mut LayoutNode, focused: ViewId) -> bool {
    match layout {
        LayoutNode::Leaf(_) => false,
        LayoutNode::Split { first, second, .. } => {
            if matches!(first.as_ref(), LayoutNode::Leaf(v) if v.id() == focused) {
                let sibling = (**second).clone();
                *layout = sibling;
                true
            } else if matches!(second.as_ref(), LayoutNode::Leaf(v) if v.id() == focused) {
                let sibling = (**first).clone();
                *layout = sibling;
                true
            } else if close_focused_leaf(first, focused) {
                true
            } else {
                close_focused_leaf(second, focused)
            }
        }
        LayoutNode::Stack(children) => {
            if let Some(pos) = children
                .iter()
                .position(|c| matches!(c, LayoutNode::Leaf(v) if v.id() == focused))
            {
                if children.len() <= 1 {
                    return false;
                }
                children.remove(pos);
                true
            } else {
                children.iter_mut().any(|c| close_focused_leaf(c, focused))
            }
        }
        LayoutNode::Overlay { base, overlay, .. } => {
            close_focused_leaf(base, focused) || close_focused_leaf(overlay, focused)
        }
    }
}

/// Record a candidate resize target: path to the deepest split whose axis
/// matches the resize direction and whose subtree holds focus, plus its ratio
/// and whether focus sits in its first child.
pub(crate) fn find_resize_target(
    node: &LayoutNode,
    focused: ViewId,
    horizontal: bool,
    path: &mut Vec<usize>,
    out: &mut Option<(Vec<usize>, f32, bool)>,
) {
    match node {
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let axis_matches = (*axis == SplitAxis::Horizontal) == horizontal;
            if first.leaf_ids().contains(&focused) {
                if axis_matches {
                    *out = Some((path.clone(), *ratio, true));
                }
                path.push(0);
                find_resize_target(first, focused, horizontal, path, out);
                path.pop();
            } else if second.leaf_ids().contains(&focused) {
                if axis_matches {
                    *out = Some((path.clone(), *ratio, false));
                }
                path.push(1);
                find_resize_target(second, focused, horizontal, path, out);
                path.pop();
            }
        }
        LayoutNode::Stack(children) => {
            for (i, child) in children.iter().enumerate() {
                if child.leaf_ids().contains(&focused) {
                    path.push(i);
                    find_resize_target(child, focused, horizontal, path, out);
                    path.pop();
                    break;
                }
            }
        }
        LayoutNode::Overlay { base, overlay, .. } => {
            if base.leaf_ids().contains(&focused) {
                path.push(0);
                find_resize_target(base, focused, horizontal, path, out);
                path.pop();
            } else if overlay.leaf_ids().contains(&focused) {
                path.push(1);
                find_resize_target(overlay, focused, horizontal, path, out);
                path.pop();
            }
        }
        LayoutNode::Leaf(_) => {}
    }
}

/// Nudge the enclosing split ratio 0.1 toward the given direction so the
/// focused pane grows that way (`set_split_ratio_at` clamps to
/// `0.10..=0.90`). Returns false when no matching split holds focus.
pub(crate) fn resize_focused_pane(
    layout: &mut LayoutNode,
    focused: ViewId,
    dir: bitty_config::SplitDir,
) -> bool {
    use bitty_config::SplitDir as D;
    let horizontal = matches!(dir, D::Left | D::Right);
    let mut out: Option<(Vec<usize>, f32, bool)> = None;
    let mut path = Vec::new();
    find_resize_target(layout, focused, horizontal, &mut path, &mut out);
    let (target, ratio, focus_in_first) = match out {
        Some(t) => t,
        None => return false,
    };
    let positive = matches!(dir, D::Right | D::Down);
    let delta = if focus_in_first == positive {
        0.1
    } else {
        -0.1
    };
    layout.set_split_ratio_at(&target, ratio + delta)
}

#[cfg(test)]
pub(crate) fn two_pane_layout() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

impl TerminalApp {
    /// Restore a zoomed layout before a tree-mutating action so the mutation
    /// applies to the real tree instead of the single-leaf zoom view.
    pub(crate) fn restore_zoom(&mut self) -> bool {
        if let Some(backup) = self.zoom_backup.take() {
            self.runtime.set_layout(backup);
            eprintln!("bitty: zoom restored for layout mutation");
            true
        } else {
            false
        }
    }

    /// Execute one bound chrome action (single owner: the PTY never sees the
    /// chord). All mutations go through existing `Runtime`/`LayoutNode` APIs;
    /// refusals warn and keep the current layout.
    pub(crate) fn apply_chrome_action(&mut self, action: bitty_config::ChromeAction) {
        use bitty_config::ChromeAction as A;
        match action {
            A::GotoSplit(dir) => {
                let focus = split_dir_to_focus(dir);
                let next = self.runtime.move_focus(focus);
                eprintln!(
                    "bitty: keymap goto_split:{} -> {:?} leafs={}",
                    dir.canonical(),
                    next,
                    self.runtime.leaf_count()
                );
            }
            A::FocusNext => {
                let next = self.runtime.move_focus(FocusDirection::Next);
                eprintln!(
                    "bitty: keymap focus_next -> {next:?} leafs={}",
                    self.runtime.leaf_count()
                );
            }
            A::FocusPrev => {
                let next = self.runtime.move_focus(FocusDirection::Prev);
                eprintln!(
                    "bitty: keymap focus_prev -> {next:?} leafs={}",
                    self.runtime.leaf_count()
                );
            }
            A::FocusId(n) => {
                let ok = self.runtime.set_focus(ViewId::new(n));
                if ok {
                    eprintln!("bitty: keymap focus:{n} -> focused");
                } else {
                    eprintln!(
                        "warning: keymap focus:{n} not in layout (leaf ids {:?}) — ignoring",
                        self.runtime.layout().leaf_ids()
                    );
                }
            }
            A::ScrollPageUp => {
                if self.runtime.scroll_focused_page(true) {
                    eprintln!("bitty: keymap scroll_page_up -> paged");
                } else {
                    eprintln!("warning: keymap scroll_page_up has no focused pane — ignoring");
                }
            }
            A::ScrollPageDown => {
                if self.runtime.scroll_focused_page(false) {
                    eprintln!("bitty: keymap scroll_page_down -> paged");
                } else {
                    eprintln!("warning: keymap scroll_page_down has no focused pane — ignoring");
                }
            }
            A::OpenComposer => {
                // CTX-0227 (008 route P4): manual composer open through the
                // single-owner keymap (suggested chord `alt+e`). The chord
                // is consumed here so its bytes never reach the PTY; the
                // composer session itself lives in `bitty-rich` (headless,
                // tested there) and the overlay/panel presentation is a
                // follow-up — until then the open signal is logged and no
                // input routing changes (Normal Mode stays byte-identical).
                eprintln!(
                    "bitty: keymap open_composer -> composer open requested (manual open only; Normal Mode input still goes to the PTY)"
                );
            }
            A::NewSplit(dir) => {
                self.restore_zoom();
                let focused = match self.runtime.focused_view() {
                    Some(id) => id,
                    None => {
                        eprintln!("warning: keymap new_split has no focused pane — ignoring");
                        return;
                    }
                };
                let mut layout = self.runtime.layout().clone();
                let new_id = next_view_id(&layout);
                let place_new_first = matches!(
                    dir,
                    bitty_config::SplitDir::Left | bitty_config::SplitDir::Up
                );
                if split_focused_leaf(
                    &mut layout,
                    focused,
                    split_dir_to_axis(dir),
                    new_id,
                    place_new_first,
                ) {
                    self.runtime.set_layout(layout);
                    // CTX-0176: the fresh leaf gets its own shell/PTY sized
                    // to its allocation — best-effort (startup parity). On
                    // failure the pane shares the primary grid with a loud
                    // warning instead of silently mirroring.
                    let (cols, rows) = self
                        .runtime
                        .layout_allocations()
                        .iter()
                        .find(|(id, _)| *id == new_id)
                        .map(|(_, r)| (r.width.max(1), r.height.max(1)))
                        .unwrap_or((80, 24));
                    match spawn_pane_shell(&mut self.runtime, &self.spawn_spec, new_id, cols, rows)
                    {
                        Ok(()) => eprintln!(
                            "bitty: keymap new_split:{} -> leafs={} focused={:?} pane_shell={new_id:?} pid={:?}",
                            dir.canonical(),
                            self.runtime.leaf_count(),
                            self.runtime.focused_view(),
                            self.runtime.pane_pid(&new_id),
                        ),
                        Err(err) => eprintln!(
                            "warning: keymap new_split:{} pane shell spawn failed ({err}) — pane {new_id:?} shares the primary grid",
                            dir.canonical(),
                        ),
                    }
                } else {
                    eprintln!("warning: keymap new_split found no focused pane — ignoring");
                }
            }
            A::CloseView => {
                self.restore_zoom();
                let focused = match self.runtime.focused_view() {
                    Some(id) => id,
                    None => {
                        eprintln!("warning: keymap close_view has no focused pane — ignoring");
                        return;
                    }
                };
                if self.runtime.leaf_count() <= 1 {
                    eprintln!("warning: keymap close_view refused (last pane) — ignoring");
                    return;
                }
                let mut layout = self.runtime.layout().clone();
                if close_focused_leaf(&mut layout, focused) {
                    self.runtime.set_layout(layout);
                    // CTX-0176: tear down the closed leaf's shell (drop kills
                    // + reaps the child; no-op when it never owned one).
                    if self.runtime.close_pane_session(&focused) {
                        eprintln!("bitty: keymap close_view tore down pane shell {focused:?}");
                    }
                    eprintln!(
                        "bitty: keymap close_view -> leafs={} focused={:?}",
                        self.runtime.leaf_count(),
                        self.runtime.focused_view()
                    );
                } else {
                    eprintln!("warning: keymap close_view found no focused pane — ignoring");
                }
            }
            A::ResizeSplit(dir) => {
                self.restore_zoom();
                let focused = match self.runtime.focused_view() {
                    Some(id) => id,
                    None => {
                        eprintln!("warning: keymap resize_split has no focused pane — ignoring");
                        return;
                    }
                };
                let mut layout = self.runtime.layout().clone();
                if resize_focused_pane(&mut layout, focused, dir) {
                    self.runtime.set_layout(layout);
                    eprintln!("bitty: keymap resize_split:{} applied", dir.canonical());
                } else {
                    eprintln!(
                        "warning: keymap resize_split:{} found no matching split — ignoring",
                        dir.canonical()
                    );
                }
            }
            A::CopyToClipboard => {
                // CTX-0161: explicit single-owner copy chord (ctrl+shift+c).
                // Before this binding the chord fell through to the PTY as
                // 0x03 (SIGINT); now chrome owns it and fish never sees the
                // byte. Reuses the Wayland-first clipboard path (CTX-0160)
                // with headless fallback; refusals warn like other chrome.
                match self.runtime.copy_selection_to_clipboard() {
                    Ok(Some(text)) => {
                        eprintln!("bitty: keymap copy_to_clipboard -> {} bytes", text.len())
                    }
                    Ok(None) => {
                        eprintln!("warning: keymap copy_to_clipboard has no selection — ignoring")
                    }
                    Err(err) => eprintln!(
                        "warning: keymap copy_to_clipboard clipboard error ({err}) — ignoring"
                    ),
                }
            }
            A::PasteFromClipboard => {
                // CTX-0161: explicit single-owner paste chord (ctrl+shift+v).
                // Before this binding the chord fell through to the PTY as
                // 0x16; now chrome owns it. Routes through the
                // suspicious-paste inspection gate (P0-AC-008): clean text
                // delivers immediately, suspicious text waits on the pending
                // confirmation path, clipboard errors warn.
                //
                // CTX-0186: a gated paste is never silent. The pending summary
                // (line count, byte size, reasons, preview) is logged loudly
                // with confirm/cancel instructions; repeating the identical
                // chord with an unchanged clipboard confirms delivery, Esc
                // cancels.
                match self.runtime.paste_from_clipboard() {
                    Ok(Some(true)) => {
                        let summary = self
                            .runtime
                            .pending_paste_summary()
                            .unwrap_or_else(|| "pending confirmation".to_string());
                        eprintln!("bitty: keymap paste_from_clipboard -> {summary}");
                    }
                    Ok(Some(false)) => eprintln!("bitty: keymap paste_from_clipboard delivered"),
                    Ok(None) => {
                        eprintln!("warning: keymap paste_from_clipboard clipboard empty — ignoring")
                    }
                    Err(err) => eprintln!(
                        "warning: keymap paste_from_clipboard clipboard error ({err}) — ignoring"
                    ),
                }
            }
            A::WorkspaceNew => {
                // CTX-0257 (DEC-0034 entry): fresh workspace, switched to it.
                match self.runtime.workspace_new() {
                    Ok(index) => eprintln!(
                        "bitty: keymap workspace_new -> workspace {} ({})",
                        index + 1,
                        self.runtime.workspaceline_text()
                    ),
                    Err(err) => {
                        eprintln!("warning: keymap workspace_new refused ({err}) — ignoring")
                    }
                }
            }
            A::WorkspaceClose => {
                // CTX-0257: idle closes immediately; live arms a pending
                // confirm (loud banner), repeat confirms the kill, Esc
                // cancels via the runtime key path. Never a silent kill.
                match self.runtime.workspace_close_request() {
                    WsCloseRequest::Closed { killed } => eprintln!(
                        "bitty: keymap workspace_close -> ({}) killed={killed}",
                        self.runtime.workspaceline_text()
                    ),
                    WsCloseRequest::Pending { summary } => {
                        eprintln!("bitty: keymap workspace_close PENDING -> {summary}")
                    }
                }
            }
            A::WorkspacePrev => {
                let index = self.runtime.workspace_prev();
                eprintln!(
                    "bitty: keymap workspace_prev -> workspace {} ({})",
                    index + 1,
                    self.runtime.workspaceline_text()
                );
            }
            A::WorkspaceNext => {
                let index = self.runtime.workspace_next();
                eprintln!(
                    "bitty: keymap workspace_next -> workspace {} ({})",
                    index + 1,
                    self.runtime.workspaceline_text()
                );
            }
            A::WorkspaceLast => {
                let index = self.runtime.workspace_last();
                eprintln!(
                    "bitty: keymap workspace_last -> workspace {} ({})",
                    index + 1,
                    self.runtime.workspaceline_text()
                );
            }
            A::WorkspaceFocus(n) => {
                let index = n.saturating_sub(1) as usize;
                if self.runtime.workspace_switch(index) {
                    eprintln!(
                        "bitty: keymap workspace_focus:{n} -> workspace {} ({})",
                        index + 1,
                        self.runtime.workspaceline_text()
                    );
                } else {
                    eprintln!(
                        "warning: keymap workspace_focus:{n} has no such workspace ({}) — ignoring",
                        self.runtime.workspaceline_text()
                    );
                }
            }
            A::ToggleZoom => {
                if let Some(backup) = self.zoom_backup.take() {
                    self.runtime.set_layout(backup);
                    eprintln!(
                        "bitty: keymap toggle_zoom off -> leafs={} focused={:?}",
                        self.runtime.leaf_count(),
                        self.runtime.focused_view()
                    );
                } else {
                    let focused = match self.runtime.focused_view() {
                        Some(id) => id,
                        None => {
                            eprintln!("warning: keymap toggle_zoom has no focused pane — ignoring");
                            return;
                        }
                    };
                    match self.runtime.layout().find_leaf(focused).cloned() {
                        Some(view) => {
                            let backup = self.runtime.layout().clone();
                            self.runtime.set_layout(LayoutNode::leaf(view));
                            self.zoom_backup = Some(backup);
                            eprintln!("bitty: keymap toggle_zoom on -> {focused:?}");
                        }
                        None => {
                            eprintln!(
                                "warning: keymap toggle_zoom found no focused pane — ignoring"
                            );
                        }
                    }
                }
            }
        }
    }
}

impl TerminalApp {
    /// Single-owner chrome intercept (CTX-0153): resolve bound chrome keys
    /// BEFORE `Runtime` routing. Returns `true` when the event was consumed
    /// (a bound chord ran, or a chrome-owned key repeat/duplicate was
    /// swallowed) and must NOT reach `Runtime`; `false` means route normally
    /// (unbound keys like Tab, arrows, plain letters fall through to shell
    /// input). Modifier tracking stays in sync because modifier-only keys
    /// and `ModifiersChanged` update the mirror here AND are always routed
    /// (this method returns `false` for them), never consumed.
    ///
    /// CTX-0229 press-to-release ownership: a consumed press owns its key
    /// until the physical release. Auto-repeat (or a duplicate press) that
    /// arrives after the chord decayed — e.g. `V` still held while `Ctrl` was
    /// already released after a `Ctrl+Shift+V` paste — stays swallowed
    /// instead of leaking a literal `V` into the PTY right after the pasted
    /// text. A genuine re-press is physically impossible without an
    /// intervening release, so swallowing duplicates cannot eat real typing;
    /// the next press after the release re-matches against the live mirror.
    /// Focus transitions clear ownership alongside the CTX-0187 mirror clear
    /// (missed releases while unfocused must not swallow future typing).
    pub(crate) fn intercept_chrome_key(&mut self, kind: &WindowEventKind) -> bool {
        match kind {
            WindowEventKind::KeyboardInput(key) => {
                track_app_modifiers(&mut self.app_mods, key);
                // Release ends press-to-release ownership; the key returns to
                // normal matching on its next press. Releases always route
                // (the runtime encodes no bytes for them).
                if key.state != PressState::Pressed {
                    if let Some(keyref) = key_ref_from_event(key, &self.app_mods) {
                        self.chrome_held.remove(&keyref.key);
                    }
                    return false;
                }
                if is_modifier_key(key) {
                    return false;
                }
                if let Some(keyref) = key_ref_from_event(key, &self.app_mods) {
                    // Still physically held from a consumed chord press: stay
                    // swallowed even when the mirror decayed (release cascade).
                    // No action re-runs; the PTY never sees the key.
                    if self.chrome_held.contains(&keyref.key) {
                        if let Some(win) = self.window.as_ref() {
                            win.request_redraw();
                        }
                        return true;
                    }
                    if let Some(action) = bitty_config::match_keymap(&self.keymaps, keyref) {
                        // The press is chrome-owned from here until release.
                        self.chrome_held.insert(keyref.key);
                        // Repeats of a bound chord stay owned by chrome
                        // (no action, no PTY bytes).
                        if !key.repeat {
                            self.apply_chrome_action(action);
                        }
                        if let Some(win) = self.window.as_ref() {
                            win.request_redraw();
                        }
                        return true;
                    }
                }
                false
            }
            WindowEventKind::ModifiersChanged(mods) => {
                self.app_mods = AppModifiers {
                    shift: mods.shift,
                    control: mods.control,
                    alt: mods.alt,
                    super_held: mods.super_pressed,
                };
                false
            }
            WindowEventKind::Focused(focused) => {
                // CTX-0187 exit B root-cause fix: focus transitions are where
                // the mirror goes stale (missed releases while unfocused), so
                // clear here before delegating to Runtime (which records focus
                // via set_focused). Fail-closed to shell until the
                // authoritative ModifiersChanged stream re-latches.
                clear_app_modifiers_on_focus(&mut self.app_mods, *focused);
                // CTX-0229: a missed key release while unfocused must not
                // leave a stale ownership entry swallowing future typing.
                self.chrome_held.clear();
                false
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{SpawnSpec, TerminalApp};
    use super::*;
    use bitty_platform::{PlatformEvent, WindowId};
    use bitty_runtime::Runtime;
    use bitty_test_support::require_pty;

    // CTX-0153 keymap-driven chrome keys: single-owner rule + layout surgery.
    // -----------------------------------------------------------------------

    fn test_key(logical: LogicalKey) -> KeyEvent {
        KeyEvent {
            logical_key: logical,
            text: None,
            location: bitty_platform::KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        }
    }

    fn test_char_key(s: &str) -> KeyEvent {
        test_key(LogicalKey::Character(s.to_string()))
    }

    #[test]
    fn modifier_keys_route_never_chrome() {
        for named in [
            NamedKey::Shift,
            NamedKey::Control,
            NamedKey::Alt,
            NamedKey::Super,
            NamedKey::Meta,
        ] {
            assert!(is_modifier_key(&test_key(LogicalKey::Named(named))));
        }
        assert!(!is_modifier_key(&test_key(LogicalKey::Named(
            NamedKey::Tab
        ))));
        assert!(!is_modifier_key(&test_char_key("h")));
    }

    #[test]
    fn app_modifier_mirror_latches_and_releases() {
        let mut mods = AppModifiers::default();
        let mut press = test_key(LogicalKey::Named(NamedKey::Alt));
        track_app_modifiers(&mut mods, &press);
        assert!(mods.alt);
        press.state = PressState::Released;
        track_app_modifiers(&mut mods, &press);
        assert!(!mods.alt);
        // Non-modifier keys leave the mirror alone.
        track_app_modifiers(&mut mods, &test_char_key("h"));
        assert_eq!(mods, AppModifiers::default());
    }

    #[test]
    fn key_ref_mapping_covers_chords_and_shell_keys() {
        use bitty_config::KeyName;
        let plain = AppModifiers::default();
        let with_alt = AppModifiers {
            alt: true,
            ..Default::default()
        };
        // alt+h resolves to the matchable chord.
        let r = key_ref_from_event(&test_char_key("h"), &with_alt).expect("matchable");
        assert_eq!(r.key, KeyName::Char('h'));
        assert!(r.alt && !r.ctrl);
        // Shift+uppercase letter normalizes to the lowercase chord key.
        let with_shift_alt = AppModifiers {
            shift: true,
            alt: true,
            ..Default::default()
        };
        let r = key_ref_from_event(&test_char_key("H"), &with_shift_alt).expect("matchable");
        assert_eq!(r.key, KeyName::Char('h'));
        // Named keys map; modifier/media leftovers and dead keys do not.
        let r = key_ref_from_event(&test_key(LogicalKey::Named(NamedKey::Tab)), &plain)
            .expect("tab matchable");
        assert_eq!(r.key, KeyName::Tab);
        assert!(
            key_ref_from_event(&test_key(LogicalKey::Named(NamedKey::Shift)), &plain).is_none()
        );
        assert!(key_ref_from_event(&test_key(LogicalKey::Dead(None)), &plain).is_none());
        assert!(key_ref_from_event(&test_key(LogicalKey::Unidentified), &plain).is_none());
        assert!(key_ref_from_event(&test_char_key("ab"), &plain).is_none());
    }

    #[test]
    fn single_owner_unbound_keys_reach_shell() {
        use bitty_config::{KeyName, KeyRef, match_keymap, resolve_keymaps};
        let maps = resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
        let shell = |key: KeyName, ctrl: bool, alt: bool, shift: bool| KeyRef {
            key,
            ctrl,
            alt,
            shift,
            super_held: false,
        };
        // The stolen keys from #249: plain Tab/arrows/letters/digits plus
        // Ctrl+P (0x10 via CTX-0154) must all stay shell input by default.
        for k in [
            shell(KeyName::Tab, false, false, false),
            shell(KeyName::Up, false, false, false),
            shell(KeyName::Down, false, false, false),
            shell(KeyName::Left, false, false, false),
            shell(KeyName::Right, false, false, false),
            shell(KeyName::Char('n'), false, false, false),
            shell(KeyName::Char('p'), false, false, false),
            shell(KeyName::Char('1'), false, false, false),
            shell(KeyName::Char('p'), true, false, false),
        ] {
            assert_eq!(match_keymap(&maps, k), None, "shell key {k:?}");
        }
        // Bound chords resolve to exactly one action each.
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('h'), false, true, false)),
            Some(bitty_config::ChromeAction::GotoSplit(
                bitty_config::SplitDir::Left
            ))
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Tab, true, false, false)),
            Some(bitty_config::ChromeAction::FocusNext)
        );
        // CTX-0178 Alt-as-Mod: number jumps, paging, and zoom resolve.
        // CTX-0257: alt+1..=9 jumps WORKSPACES (DEC-0034 entry).
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('1'), false, true, false)),
            Some(bitty_config::ChromeAction::WorkspaceFocus(1))
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('9'), false, true, false)),
            Some(bitty_config::ChromeAction::WorkspaceFocus(9))
        );
        // CTX-0257 DEC entry set: new/close/prev/next/last.
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('n'), false, true, false)),
            Some(bitty_config::ChromeAction::WorkspaceNew)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('w'), false, true, false)),
            Some(bitty_config::ChromeAction::WorkspaceClose)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('-'), false, true, false)),
            Some(bitty_config::ChromeAction::WorkspacePrev)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('='), false, true, false)),
            Some(bitty_config::ChromeAction::WorkspaceNext)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Tab, false, true, false)),
            Some(bitty_config::ChromeAction::WorkspaceLast)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('u'), false, true, false)),
            Some(bitty_config::ChromeAction::ScrollPageUp)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('i'), false, true, false)),
            Some(bitty_config::ChromeAction::ScrollPageDown)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('z'), false, true, false)),
            Some(bitty_config::ChromeAction::ToggleZoom)
        );
    }

    #[test]
    fn single_owner_copy_paste_chords_resolve_and_shell_stays_clean() {
        // CTX-0161: the shifted chords are chrome-owned (single-owner
        // intercept consumes them before the PTY), while the unshifted C0
        // bytes (Ctrl+C SIGINT, Ctrl+V) stay shell input.
        use bitty_config::{ChromeAction, KeyName, KeyRef, match_keymap, resolve_keymaps};
        let maps = resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
        let chord = |key: KeyName, ctrl: bool, alt: bool, shift: bool| KeyRef {
            key,
            ctrl,
            alt,
            shift,
            super_held: false,
        };
        assert_eq!(
            match_keymap(&maps, chord(KeyName::Char('c'), true, false, true)),
            Some(ChromeAction::CopyToClipboard)
        );
        assert_eq!(
            match_keymap(&maps, chord(KeyName::Char('v'), true, false, true)),
            Some(ChromeAction::PasteFromClipboard)
        );
        assert_eq!(
            match_keymap(&maps, chord(KeyName::Char('c'), true, false, false)),
            None,
            "Ctrl+C must reach fish as 0x03"
        );
        assert_eq!(
            match_keymap(&maps, chord(KeyName::Char('v'), true, false, false)),
            None,
            "Ctrl+V must stay shell input"
        );
        // Uppercase letters normalize through the event mapper (Shift held
        // to type 'C' is part of the chord, not shell typing).
        let mods = AppModifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        let r = key_ref_from_event(&test_char_key("C"), &mods).expect("matchable");
        assert_eq!(match_keymap(&maps, r), Some(ChromeAction::CopyToClipboard));
        let r = key_ref_from_event(&test_char_key("V"), &mods).expect("matchable");
        assert_eq!(
            match_keymap(&maps, r),
            Some(ChromeAction::PasteFromClipboard)
        );
    }

    #[test]
    fn focus_loss_clears_stale_shift_so_bare_ctrl_v_stays_shell() {
        // CTX-0187 exit B: the mirror uses the raw compositor modifier bit
        // verbatim (no case inference). Staleness is fixed at the root by
        // clearing AppModifiers on focus transitions (see
        // clear_app_modifiers_on_focus, wired to WindowEventKind::Focused):
        // a latched shift=true from before focus loss must not leak a later
        // bare Ctrl+V into the paste arm.
        use bitty_config::{ChromeAction, match_keymap, resolve_keymaps};
        let maps = resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
        // Latched before focus loss: control+shift true (e.g. Shift held,
        // window focused out, release missed while unfocused).
        let mut mods = AppModifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        // Without the clear, the stale mirror WOULD match paste — this
        // documents why the focus clear matters (fail-open without it).
        let r = key_ref_from_event(&test_char_key("v"), &mods).expect("matchable");
        assert_eq!(
            match_keymap(&maps, r),
            Some(ChromeAction::PasteFromClipboard),
            "stale mirror without focus clear still matches paste (demonstrates leak)"
        );
        // Focus loss clears to fail-closed shell.
        clear_app_modifiers_on_focus(&mut mods, false);
        assert_eq!(mods, AppModifiers::default());
        // Re-latch only the still-held Control (as the fresh
        // ModifiersChanged snapshot would after regain); Shift stays false.
        mods.control = true;
        let r = key_ref_from_event(&test_char_key("v"), &mods).expect("matchable");
        assert_eq!(
            match_keymap(&maps, r),
            None,
            "after focus clear, bare Ctrl+V stays shell input, never paste"
        );
        // Focus regain also resets (fail-closed until fresh snapshot).
        let mut regained = AppModifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        clear_app_modifiers_on_focus(&mut regained, true);
        assert_eq!(regained, AppModifiers::default());
    }

    #[test]
    fn real_shifted_ctrl_v_pastes_regardless_of_reported_case() {
        // CTX-0187 exit B no-breakage guard (PX-0694): real Ctrl+Shift+V must
        // paste whether the platform reports uppercase "V" or lowercase "v"
        // with shift=true (X11/Wayland commonly report lowercase+shift for
        // real Ctrl+Shift chords). Trusting the raw shift bit — not the
        // character case — preserves both.
        use bitty_config::{ChromeAction, match_keymap, resolve_keymaps};
        let maps = resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
        let shifted = AppModifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        for logical in ["V", "v"] {
            let r = key_ref_from_event(&test_char_key(logical), &shifted).expect("matchable");
            assert_eq!(
                match_keymap(&maps, r),
                Some(ChromeAction::PasteFromClipboard),
                "real Ctrl+Shift+V (reported {logical:?} + shift=true) must paste"
            );
        }
        // Fresh bare (shift=false) stays shell for both cases.
        let bare = AppModifiers {
            control: true,
            ..Default::default()
        };
        for logical in ["V", "v"] {
            let r = key_ref_from_event(&test_char_key(logical), &bare).expect("matchable");
            assert_eq!(
                match_keymap(&maps, r),
                None,
                "bare Ctrl+V (reported {logical:?} + shift=false) stays shell"
            );
        }
        // Bare Ctrl+C stays shell SIGINT when unshifted; shifted copies.
        let r = key_ref_from_event(&test_char_key("c"), &bare).expect("matchable");
        assert_eq!(match_keymap(&maps, r), None, "bare Ctrl+C stays shell");
        let r = key_ref_from_event(&test_char_key("c"), &shifted).expect("matchable");
        assert_eq!(
            match_keymap(&maps, r),
            Some(ChromeAction::CopyToClipboard),
            "real Ctrl+Shift+C pastes-copies even when reported lowercase"
        );
    }

    #[test]
    fn chrome_copy_paste_round_trip_headless() {
        // CTX-0161 end-to-end through the chrome arms (no window): copy
        // mirrors the selection into the clipboard, paste routes through
        // the suspicious-paste gate, and no stray C0 reaches the PTY.
        use bitty_config::ChromeAction;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.force_headless_clipboard();
        rt.handle_pty_bytes(b"hello");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        // Copy with no selection warns and touches nothing.
        app.apply_chrome_action(ChromeAction::CopyToClipboard);
        assert_eq!(app.runtime.clipboard().headless_contents(), "");
        // Select everything, copy: clipboard mirrors the selection text.
        app.runtime.select_all();
        let selected = app.runtime.selection_text().expect("selection");
        assert!(selected.contains("hello"), "grid holds fed text");
        app.apply_chrome_action(ChromeAction::CopyToClipboard);
        assert_eq!(app.runtime.clipboard().headless_contents(), selected);
        // Pasting the grid-shaped clipboard goes through the inspection
        // gate (embedded newlines are suspicious): held pending, nothing
        // delivered silently.
        app.runtime.clear_selection();
        app.apply_chrome_action(ChromeAction::PasteFromClipboard);
        assert!(app.runtime.has_pending_paste());
        assert!(app.runtime.drain_pending_input().is_empty());
        // Clean clipboard text delivers immediately as PTY input bytes.
        assert!(app.runtime.cancel_pending_paste());
        app.runtime
            .clipboard_mut()
            .set_text("clean-paste".to_string())
            .expect("headless set");
        app.apply_chrome_action(ChromeAction::PasteFromClipboard);
        assert!(!app.runtime.has_pending_paste());
        assert_eq!(app.runtime.drain_pending_input(), b"clean-paste");
    }

    // CTX-0229 headless chord driver: the same dispatch `handle_event`
    // performs (single-owner intercept first, `Runtime` routing only for
    // unconsumed events), minus the `EventContext` (redraw/exit plumbing
    // carries no bytes). Returns `true` when chrome consumed the event.
    fn drive_chrome(app: &mut TerminalApp, kind: WindowEventKind) -> bool {
        let consumed = app.intercept_chrome_key(&kind);
        if !consumed {
            match kind {
                WindowEventKind::KeyboardInput(key) => {
                    app.runtime.handle_key_event(key);
                }
                WindowEventKind::ModifiersChanged(_)
                | WindowEventKind::Focused(_)
                | WindowEventKind::Resized(_)
                | WindowEventKind::ScaleFactorChanged(_)
                | WindowEventKind::CloseRequested
                | WindowEventKind::Closed
                | WindowEventKind::RedrawRequested
                | WindowEventKind::MouseInput(_)
                | WindowEventKind::MouseWheel(_)
                | WindowEventKind::CursorMoved(_)
                | WindowEventKind::CursorLeft
                | WindowEventKind::Ime(_) => {
                    app.runtime.handle_platform_event(PlatformEvent::Window {
                        window_id: WindowId::from_raw_public(1),
                        kind,
                    });
                }
            }
        }
        consumed
    }

    fn mods_event(shift: bool, control: bool) -> WindowEventKind {
        WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift,
            control,
            alt: false,
            super_pressed: false,
        })
    }

    fn char_press(logical: &str, text: &str, repeat: bool) -> WindowEventKind {
        WindowEventKind::KeyboardInput(KeyEvent {
            logical_key: LogicalKey::Character(logical.to_string()),
            text: Some(text.to_string()),
            location: bitty_platform::KeyLocation::Standard,
            state: PressState::Pressed,
            repeat,
            is_synthetic: false,
        })
    }

    fn char_release(logical: &str) -> WindowEventKind {
        WindowEventKind::KeyboardInput(KeyEvent {
            logical_key: LogicalKey::Character(logical.to_string()),
            text: None,
            location: bitty_platform::KeyLocation::Standard,
            state: PressState::Released,
            repeat: false,
            is_synthetic: false,
        })
    }

    fn named_press(named: NamedKey) -> WindowEventKind {
        WindowEventKind::KeyboardInput(KeyEvent {
            logical_key: LogicalKey::Named(named),
            text: None,
            location: bitty_platform::KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        })
    }

    fn paste_test_app(clipboard_text: &str) -> TerminalApp {
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.force_headless_clipboard();
        rt.clipboard_mut()
            .set_text(clipboard_text.to_string())
            .expect("headless set");
        TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        )
    }

    /// Drives the live `Ctrl+Shift+V` shape (uppercase logical + text, as
    /// winit reports with Shift applied and Ctrl excluded) through the real
    /// intercept: consumed, clipboard bytes delivered, zero stray PTY bytes.
    fn press_paste_chord(app: &mut TerminalApp) {
        // Mirror updates and modifier presses route (never consumed)...
        assert!(!drive_chrome(app, mods_event(false, true)));
        assert!(!drive_chrome(app, named_press(NamedKey::Control)));
        assert!(!drive_chrome(app, mods_event(true, true)));
        assert!(!drive_chrome(app, named_press(NamedKey::Shift)));
        // ...while the chorded press itself is chrome-owned.
        assert!(drive_chrome(app, char_press("V", "V", false)));
    }

    #[test]
    fn paste_chord_delivers_clipboard_with_zero_pty_stray() {
        // CTX-0229 dogfood shape (PX-1233+, shot 07): the chord must deliver
        // exactly the clipboard bytes — never `PASTE-FROM-CLIPBOARD-OKV`.
        let mut app = paste_test_app("PASTE-FROM-CLIPBOARD-OK");
        press_paste_chord(&mut app);
        assert_eq!(
            app.runtime.drain_pending_input(),
            b"PASTE-FROM-CLIPBOARD-OK",
            "paste delivers clipboard bytes with zero trailing key bytes"
        );
        // Clean release cascade: nothing further reaches the PTY.
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert_eq!(
            app.runtime.drain_pending_input(),
            b"",
            "release cascade after paste stays silent"
        );
    }

    #[test]
    fn paste_chord_repeat_after_ctrl_release_stays_swallowed() {
        // CTX-0229 root-cause regression: the `V` press is chrome-owned until
        // its physical release. A repeat arriving after `Ctrl` was released
        // first (release cascade, `Shift` still held) no longer matches the
        // full chord — without press-to-release ownership it fell through to
        // the PTY as a literal `V` right after the pasted text.
        let mut app = paste_test_app("PASTE-FROM-CLIPBOARD-OK");
        press_paste_chord(&mut app);
        assert_eq!(
            app.runtime.drain_pending_input(),
            b"PASTE-FROM-CLIPBOARD-OK"
        );
        // Operator releases `Ctrl` while `V` is still held (auto-repeat
        // continues); the decayed repeat must stay chrome-owned.
        assert!(!drive_chrome(&mut app, mods_event(true, false)));
        assert!(
            drive_chrome(&mut app, char_press("V", "V", true)),
            "decayed repeat of a chrome-held key stays swallowed"
        );
        assert_eq!(
            app.runtime.drain_pending_input(),
            b"",
            "no trailing V may reach the PTY after the paste"
        );
        // The physical release ends ownership; later typing works again.
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert!(!drive_chrome(&mut app, char_press("v", "v", false)));
        assert_eq!(app.runtime.drain_pending_input(), b"v");
    }

    #[test]
    fn bare_v_still_reaches_shell() {
        // CTX-0229 no-breakage guard: unmatched keys never enter chrome
        // ownership — bare `v` (press and held repeat) always types.
        let mut app = paste_test_app("PASTE-FROM-CLIPBOARD-OK");
        assert!(!drive_chrome(&mut app, char_press("v", "v", false)));
        assert!(!drive_chrome(&mut app, char_press("v", "v", true)));
        assert_eq!(app.runtime.drain_pending_input(), b"vv");
        // Bare `Ctrl+V` (no shift) stays shell input as `0x16` (CTX-0187).
        assert!(!drive_chrome(&mut app, mods_event(false, true)));
        assert!(!drive_chrome(&mut app, char_press("v", "v", false)));
        assert_eq!(app.runtime.drain_pending_input(), b"\x16");
    }

    #[test]
    fn ctrl_shift_c_still_copies_unaffected() {
        // CTX-0229: the sibling chord keeps its behavior — consumed, no PTY
        // bytes — and its own release ends its ownership independently.
        let mut app = paste_test_app("PASTE-FROM-CLIPBOARD-OK");
        assert!(!drive_chrome(&mut app, mods_event(true, true)));
        assert!(drive_chrome(&mut app, char_press("C", "C", false)));
        assert_eq!(
            app.runtime.drain_pending_input(),
            b"",
            "copy chord never types into the PTY"
        );
        assert!(!drive_chrome(&mut app, char_release("C")));
        // Bare `c` afterwards is fresh shell input, not swallowed.
        assert!(!drive_chrome(&mut app, mods_event(false, false)));
        assert!(!drive_chrome(&mut app, char_press("c", "c", false)));
        assert_eq!(app.runtime.drain_pending_input(), b"c");
    }

    #[test]
    fn chrome_held_clears_on_focus() {
        // CTX-0229 staleness bound (mirrors the CTX-0187 mirror clear): a
        // missed release while unfocused must not swallow future typing.
        let mut app = paste_test_app("PASTE-FROM-CLIPBOARD-OK");
        press_paste_chord(&mut app);
        assert_eq!(
            app.runtime.drain_pending_input(),
            b"PASTE-FROM-CLIPBOARD-OK"
        );
        // Focus loss with `V` still "held" (release missed while unfocused).
        assert!(!drive_chrome(&mut app, WindowEventKind::Focused(false)));
        // A later `Shift+V` is fresh typing, not a chrome duplicate.
        assert!(!drive_chrome(&mut app, mods_event(true, false)));
        assert!(!drive_chrome(&mut app, char_press("V", "V", false)));
        assert_eq!(app.runtime.drain_pending_input(), b"V");
    }

    #[test]
    fn second_chord_press_after_release_pastes_again() {
        // CTX-0229 intent guard: ownership ends at release, so a deliberate
        // second chord press pastes again — no bytes lost, no stray added.
        let mut app = paste_test_app("PASTE-FROM-CLIPBOARD-OK");
        press_paste_chord(&mut app);
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert_eq!(
            app.runtime.drain_pending_input(),
            b"PASTE-FROM-CLIPBOARD-OK"
        );
        press_paste_chord(&mut app);
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert_eq!(
            app.runtime.drain_pending_input(),
            b"PASTE-FROM-CLIPBOARD-OK"
        );
    }

    #[test]
    fn chrome_focus_actions_move_runtime_focus() {
        use bitty_config::{ChromeAction, SplitDir};
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        app.runtime.set_layout(two_pane_layout());
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
        app.apply_chrome_action(ChromeAction::GotoSplit(SplitDir::Right));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(2)));
        app.apply_chrome_action(ChromeAction::FocusPrev);
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
        app.apply_chrome_action(ChromeAction::FocusId(2));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(2)));
        // Unknown id warns and keeps focus.
        app.apply_chrome_action(ChromeAction::FocusId(99));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(2)));
    }

    #[test]
    fn chrome_split_close_resize_zoom_round_trip() {
        use bitty_config::{ChromeAction, SplitDir};
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        app.runtime.set_layout(two_pane_layout());
        // Split focused pane right: 2 -> 3 leaves, focus stays.
        app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
        assert_eq!(app.runtime.leaf_count(), 3);
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
        // Resize nudges without changing leaf count.
        app.apply_chrome_action(ChromeAction::ResizeSplit(SplitDir::Right));
        assert_eq!(app.runtime.leaf_count(), 3);
        // Zoom collapses to one leaf and restores the tree.
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 1);
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 3);
        // Close removes the focused leaf and refocuses inside the tree.
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 2);
        assert!(app.runtime.focused_view().is_some());
        // Last pane refuses to close.
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 1);
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 1);
    }

    #[test]
    fn chrome_scroll_actions_page_focused_pane() {
        use bitty_config::ChromeAction;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        for i in 0..200 {
            let line = format!("line {i:03}\n");
            app.runtime.handle_pty_bytes(line.as_bytes());
        }
        app.runtime.tick();
        assert!(app.runtime.state().scrollback_len() > 0);
        app.apply_chrome_action(ChromeAction::ScrollPageUp);
        let offset = app
            .runtime
            .layout()
            .find_leaf(ViewId::new(1))
            .expect("single leaf")
            .scroll_offset();
        assert!(offset > 0, "page up must leave live");
        app.apply_chrome_action(ChromeAction::ScrollPageDown);
        assert_eq!(
            app.runtime
                .layout()
                .find_leaf(ViewId::new(1))
                .expect("single leaf")
                .scroll_offset(),
            0,
            "page down must return to live"
        );
    }

    #[test]
    fn close_last_leaf_helper_refuses() {
        let mut single = LayoutNode::leaf(View::new(ViewId::new(1), 80, 24));
        assert!(!close_focused_leaf(&mut single, ViewId::new(1)));
        assert!(!close_focused_leaf(&mut two_pane_layout(), ViewId::new(9)));
        let mut two = two_pane_layout();
        assert!(!close_focused_leaf(&mut two, ViewId::new(9)));
        assert!(close_focused_leaf(&mut two, ViewId::new(2)));
        assert_eq!(two.leaf_count(), 1);
    }

    // CTX-0257 workspace ops entry (DEC-0034): keys + tabline through the
    // chrome arms, headless (no window).
    // -----------------------------------------------------------------------

    fn workspace_test_app() -> TerminalApp {
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        TerminalApp::with_theme(
            Runtime::with_defaults().expect("must build"),
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        )
    }

    // Live-spawn helper: the live-close test below spawns /bin/sh. It calls
    // `require_pty!()` first and skips where no PTY backend exists
    // (Windows ConPTY unimplemented per ADR-0002; CTX-0267), so this helper
    // stays compiled on all platforms instead of hiding behind `#[cfg(unix)]`.
    fn esc_press() -> WindowEventKind {
        WindowEventKind::KeyboardInput(KeyEvent {
            logical_key: LogicalKey::Named(NamedKey::Escape),
            text: None,
            location: bitty_platform::KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        })
    }

    #[test]
    fn chrome_workspace_ops_move_active_and_tabline_follows() {
        use bitty_config::ChromeAction;
        let mut app = workspace_test_app();
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1* (1)");
        app.apply_chrome_action(ChromeAction::WorkspaceNew);
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1 2:ws2* (2)");
        app.apply_chrome_action(ChromeAction::WorkspaceFocus(1));
        assert_eq!(app.runtime.active_workspace_index(), 0);
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1* 2:ws2 (2)");
        app.apply_chrome_action(ChromeAction::WorkspaceNext);
        assert_eq!(app.runtime.active_workspace_index(), 1);
        app.apply_chrome_action(ChromeAction::WorkspacePrev);
        assert_eq!(app.runtime.active_workspace_index(), 0);
        app.apply_chrome_action(ChromeAction::WorkspaceLast);
        assert_eq!(app.runtime.active_workspace_index(), 1);
        // Unknown workspace warns and keeps state.
        app.apply_chrome_action(ChromeAction::WorkspaceFocus(9));
        assert_eq!(app.runtime.active_workspace_index(), 1);
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1 2:ws2* (2)");
        // Idle close is immediate (never pends, never kills).
        app.apply_chrome_action(ChromeAction::WorkspaceClose);
        assert!(!app.runtime.has_pending_ws_close());
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1* (1)");
    }

    // Live-spawn: runs a real shell; skips (not fails) where no PTY backend
    // exists (Windows; CTX-0267).
    #[test]
    fn chrome_workspace_close_live_pends_esc_cancels_repeat_kills() {
        require_pty!();
        use bitty_config::ChromeAction;
        let mut app = workspace_test_app();
        // Live session in the active workspace: manual split (headless) +
        // a real shell in the new leaf (pane_sessions.rs pattern).
        let new_id = ViewId::new(2);
        let mut layout = app.runtime.layout().clone();
        let focused = app.runtime.focused_view().expect("focus");
        let old = layout.find_leaf(focused).cloned().expect("leaf");
        let fresh_leaf = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(fresh_leaf),
        );
        app.runtime.set_layout(layout);
        app.runtime
            .spawn_shell_for_view(new_id, "/bin/sh", &[], 40, 12)
            .expect("pane shell must spawn headless");
        // First Alt+W arms pending (never silent kill).
        app.apply_chrome_action(ChromeAction::WorkspaceClose);
        assert!(app.runtime.has_pending_ws_close());
        assert!(app.runtime.has_pane_session(&new_id));
        // Esc through the real intercept cancels: unconsumed as chrome
        // (routes to Runtime) and the arm drops with no kill.
        assert!(!drive_chrome(&mut app, esc_press()));
        assert!(!app.runtime.has_pending_ws_close());
        assert!(app.runtime.has_pane_session(&new_id));
        // Re-arm, then repeat-to-confirm kills and closes.
        app.apply_chrome_action(ChromeAction::WorkspaceClose);
        assert!(app.runtime.has_pending_ws_close());
        app.apply_chrome_action(ChromeAction::WorkspaceClose);
        assert!(!app.runtime.has_pending_ws_close());
        assert!(!app.runtime.has_pane_session(&new_id));
        assert_eq!(app.runtime.workspace_count(), 1);
    }
}
