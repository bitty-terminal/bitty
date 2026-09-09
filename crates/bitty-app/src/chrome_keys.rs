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

// ---------------------------------------------------------------------------
// Explicit key-dispatch priority (CTX-0275; input-pointer RFC key dispatch
// priority candidate).
// ---------------------------------------------------------------------------

/// Explicit dispatch layer for one keypress, highest first.
///
/// Priority order (emergency/reserved > active overlay-modal > user-defined
/// > plugin > terminal encoding):
///
/// 1. [`DispatchPriority::Emergency`] — reserved Bitty bindings that must
///    work even when a modal dialog or a user remap says otherwise. Today
///    the only emergency gesture is `Esc` while a modal confirmation pends
///    (paste gate / workspace kill-confirm): it cancels, is consumed here,
///    and a user `escape` remap never steals it.
/// 2. [`DispatchPriority::Modal`] — an active modal captures command
///    dispatch: bound chrome chords that are NOT the modal's own confirm
///    gesture are swallowed (consumed, no action, no PTY bytes) so no state
///    mutates behind the dialog. Unbound keys still fall through to
///    [`DispatchPriority::Terminal`] (fall-through unchanged).
/// 3. [`DispatchPriority::User`] — the resolved keymap table (shipped
///    defaults plus user overrides via [`bitty_config::match_keymap`]).
///    The modal's own confirm gesture (repeat the arming chord) dispatches
///    here so confirmation reuses the normal action path.
/// 4. [`DispatchPriority::Plugin`] — plugin-suggested bindings (future).
///    No plugin-binding facility exists, so this layer is inert and denies
///    by default; see [`match_plugin_binding`].
/// 5. [`DispatchPriority::Terminal`] — terminal encoding fall-through: the
///    event is not consumed and routes to `Runtime` (PTY bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DispatchPriority {
    /// Reserved emergency gesture (`Esc` cancels a pending confirmation).
    Emergency,
    /// Active modal captures a bound non-confirm chord (swallowed).
    Modal,
    /// Resolved keymap action runs (single owner, CTX-0153).
    User,
    /// Plugin-suggested binding (inert: deny-by-default, CTX-0275 slot).
    Plugin,
    /// No layer claimed the key: route to terminal encoding.
    Terminal,
}

/// Plugin-suggested binding lookup (CTX-0275 future slot).
///
/// No plugin-binding facility exists today, so this always returns `None`
/// (deny-by-default): an explicit user mapping can never lose to a plugin,
/// and unbound keys keep falling through to terminal encoding. When a
/// plugin-binding facility lands, its lookup must sit exactly here — after
/// [`DispatchPriority::User`], before [`DispatchPriority::Terminal`] — and
/// this stub is the single wiring point.
fn match_plugin_binding(_keyref: bitty_config::KeyRef) -> Option<bitty_config::ChromeAction> {
    None
}

/// Pure dispatch classifier over one matchable keypress (CTX-0275).
///
/// Inputs are plain data so the priority table is headless-testable without
/// a display server or a live `Runtime`: `matched` is the
/// [`bitty_config::match_keymap`] result for `keyref`, and the two pending
/// flags are the app's modal surface (`Runtime::has_pending_paste` /
/// `Runtime::has_pending_ws_close`). Returns the winning layer plus the
/// action to run when the layer dispatches one (`User` only; `Modal`
/// swallows, `Plugin` is inert, `Terminal` routes).
///
/// Table (first match wins):
/// - `Esc` with any modal pending -> `(Emergency, None)`.
/// - modal pending + bound chord that confirms THAT modal (repeat the
///   arming chord: `paste_from_clipboard` while a paste pends,
///   `workspace_close` while a close pends) -> `(User, action)`.
/// - modal pending + any other bound chord -> `(Modal, None)` (captured).
/// - bound chord, no modal -> `(User, action)`.
/// - unbound key while a modal pends -> `(Terminal, None)` (fall-through
///   unchanged: the dialog captures commands, not typing).
/// - unbound key, no modal -> `(Terminal, None)` (existing fall-through).
fn resolve_priority_for(
    keyref: bitty_config::KeyRef,
    matched: Option<bitty_config::ChromeAction>,
    paste_pending: bool,
    ws_close_pending: bool,
) -> (DispatchPriority, Option<bitty_config::ChromeAction>) {
    use bitty_config::{ChromeAction as A, KeyName};
    let modal_active = paste_pending || ws_close_pending;
    // Emergency/reserved first: Esc cancels any pending confirmation even
    // when the user remapped `escape` (the remap only applies with no
    // modal active, where this arm never fires).
    if modal_active && keyref.key == KeyName::Escape {
        return (DispatchPriority::Emergency, None);
    }
    match matched {
        Some(action) if modal_active => {
            let confirms_paste = paste_pending && matches!(action, A::PasteFromClipboard);
            let confirms_close = ws_close_pending && matches!(action, A::WorkspaceClose);
            if confirms_paste || confirms_close {
                // The modal's own confirm gesture reuses the normal user
                // action path (identical re-paste delivers, repeat Alt+W
                // kills); every other bound chord is captured below.
                (DispatchPriority::User, Some(action))
            } else {
                (DispatchPriority::Modal, None)
            }
        }
        Some(action) => (DispatchPriority::User, Some(action)),
        None => match match_plugin_binding(keyref) {
            // Future wiring point: plugin suggestions sit after the user
            // table (an explicit mapping always wins) and before terminal
            // encoding. Inert today (`match_plugin_binding` denies by
            // default), so this arm falls through to `Terminal`.
            Some(action) => (DispatchPriority::Plugin, Some(action)),
            None => (DispatchPriority::Terminal, None),
        },
    }
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
            A::WorkspaceMove(n) => {
                // CTX-0259 (DEC-0034 follow-through): reparent the focused
                // leaf into workspace N. Never kills, never removes a slot;
                // invalid N warns fail-closed. Zoom restores first so the
                // move operates on the real tiled tree, not the zoom proxy.
                self.restore_zoom();
                match self.runtime.workspace_move_focused_to_one_based(n) {
                    Ok((moved, from, to)) => eprintln!(
                        "bitty: keymap workspace_move:{n} -> moved {moved:?} ws:{from} -> ws:{to} ({})",
                        self.runtime.workspaceline_text()
                    ),
                    Err(err) => eprintln!(
                        "warning: keymap workspace_move:{n} refused ({err}) ({}) — ignoring",
                        self.runtime.workspaceline_text()
                    ),
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
            A::IncreaseFontSize => {
                // CTX-0263 per-window font zoom: mutates only this window's
                // live `RuntimeConfig.font_size` (never the config file),
                // re-derives the renderer at the live DPI scale, and reflows
                // the grid from the current surface extent. Bounded and
                // fail-closed at the ends (warn + keep current size).
                match self.runtime.zoom_in() {
                    Ok(()) => eprintln!(
                        "bitty: keymap increase_font_size -> {:.1}pt",
                        self.runtime.font_size()
                    ),
                    Err(err) => eprintln!("warning: keymap increase_font_size refused ({err})"),
                }
            }
            A::DecreaseFontSize => match self.runtime.zoom_out() {
                Ok(()) => eprintln!(
                    "bitty: keymap decrease_font_size -> {:.1}pt",
                    self.runtime.font_size()
                ),
                Err(err) => eprintln!("warning: keymap decrease_font_size refused ({err})"),
            },
            A::ResetFontSize => {
                self.runtime.reset_zoom();
                eprintln!(
                    "bitty: keymap reset_font_size -> {:.1}pt",
                    self.runtime.font_size()
                );
            }
        }
    }
}

impl TerminalApp {
    /// True while a modal confirmation captures command dispatch (CTX-0275).
    ///
    /// Today's app-level modal surface is the two pending-confirm gates:
    /// the suspicious-paste confirmation (`Runtime::has_pending_paste`) and
    /// the workspace kill-confirm (`Runtime::has_pending_ws_close`). Panel
    /// overlays already enforce creation-exclusivity
    /// (`OverlayManager::modal_active`); when they gain key dispatch, their
    /// active-modal bit must feed this same predicate so one capture rule
    /// covers every modal kind.
    pub(crate) fn modal_capture_active(&self) -> bool {
        self.runtime.has_pending_paste() || self.runtime.has_pending_ws_close()
    }

    /// Classify one matchable keypress into its dispatch layer (CTX-0275).
    ///
    /// Thin wrapper over the pure [`resolve_priority_for`] table using live
    /// modal state; see that function for the layer order and the
    /// confirm-gesture rule.
    pub(crate) fn resolve_dispatch(
        &self,
        keyref: bitty_config::KeyRef,
        matched: Option<bitty_config::ChromeAction>,
    ) -> (DispatchPriority, Option<bitty_config::ChromeAction>) {
        // Single capture predicate first (panel-modal bits feed
        // `modal_capture_active` itself when overlay dispatch lands); the
        // per-gate reads below only pick the confirm gesture.
        let (paste_pending, ws_close_pending) = if self.modal_capture_active() {
            (
                self.runtime.has_pending_paste(),
                self.runtime.has_pending_ws_close(),
            )
        } else {
            (false, false)
        };
        resolve_priority_for(keyref, matched, paste_pending, ws_close_pending)
    }

    /// Run the emergency `Esc`-cancels-modal gesture (CTX-0275).
    ///
    /// Routes the press through `Runtime::handle_key_event` — the same
    /// cancel path a routed `Esc` takes today
    /// (`cancel_pending_on_escape`: drops the pending paste and/or the
    /// workspace-close arm, consumes the key so it never reaches the PTY)
    /// — then consumes it here so a user `escape` remap cannot steal the
    /// cancel. Loud paste reporting mirrors `handle_event`'s CTX-0186 probe
    /// byte-for-byte (a gated paste is never silent); workspace-close
    /// cancellation stays silent exactly as today. Always returns `true`.
    pub(crate) fn handle_emergency_escape(&mut self, key: &KeyEvent) -> bool {
        let had_pending = self.runtime.has_pending_paste();
        let before_len = self.runtime.pending_input_len();
        self.runtime.handle_key_event(key.clone());
        let has_pending = self.runtime.has_pending_paste();
        let after_len = self.runtime.pending_input_len();
        if has_pending {
            if let Some(summary) = self.runtime.pending_paste_summary() {
                eprintln!("bitty: paste -> {summary}");
            }
        } else if had_pending && after_len > before_len {
            eprintln!("bitty: paste confirmed -> delivered");
        } else if had_pending {
            eprintln!("bitty: paste confirmation cancelled (Esc)");
        }
        if let Some(win) = self.window.as_ref() {
            win.request_redraw();
        }
        true
    }

    /// Single-owner chrome intercept (CTX-0153) with explicit dispatch
    /// priority (CTX-0275): emergency/reserved > active overlay-modal >
    /// user-defined keymap > plugin (inert, deny-by-default) > terminal
    /// encoding. Returns `true` when the event was consumed (emergency
    /// cancel ran, a modal captured a bound chord, a bound chord ran, or a
    /// chrome-owned key repeat/duplicate was swallowed) and must NOT reach
    /// `Runtime`; `false` means route normally (unbound keys like Tab,
    /// arrows, plain letters fall through to shell input, modal or not).
    /// Modifier tracking stays in sync because modifier-only keys
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
    ///
    /// CTX-0187/0229 behavior is byte-identical with no modal active: the
    /// emergency arm never fires, the modal arm never fires, and the
    /// user/terminal arms run the exact pre-0275 match/own/fall-through.
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
                    // (CTX-0229 ownership sits above the modal arm: ownership
                    // is a physical invariant, and with no modal active the
                    // arms below are unreachable — byte-identical.)
                    if self.chrome_held.contains(&keyref.key) {
                        if let Some(win) = self.window.as_ref() {
                            win.request_redraw();
                        }
                        return true;
                    }
                    let matched = bitty_config::match_keymap(&self.keymaps, keyref);
                    let (priority, action) = self.resolve_dispatch(keyref, matched);
                    match priority {
                        DispatchPriority::Emergency => {
                            return self.handle_emergency_escape(key);
                        }
                        DispatchPriority::Modal => {
                            // Active modal captures the bound non-confirm
                            // chord: consumed, no action runs, no PTY bytes.
                            if let Some(win) = self.window.as_ref() {
                                win.request_redraw();
                            }
                            return true;
                        }
                        DispatchPriority::User => {
                            let Some(action) = action else {
                                // Unreachable by construction (`User` always
                                // carries the matched action); fail closed to
                                // terminal routing rather than panicking.
                                return false;
                            };
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
                        DispatchPriority::Plugin => {
                            // Inert today (`match_plugin_binding` denies by
                            // default); fail closed to terminal routing.
                            debug_assert!(
                                match_plugin_binding(keyref).is_none(),
                                "plugin layer must stay deny-by-default",
                            );
                            return false;
                        }
                        DispatchPriority::Terminal => {
                            return false;
                        }
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
    fn fkey_special_keys_map_with_mod_mirror_and_bare_falls_through() {
        // CTX-0264: F-keys plus INS/DEL/HM/END/PU/PD are first-class
        // matchable chord segments. Platform events map to the config
        // `KeyName` carrying the live modifier mirror (Alt and Super
        // variants), while bare presses produce `KeyRef`s that match nothing
        // in either default map — so `intercept_chrome_key` returns false
        // and they route to `Runtime` terminal encoding unchanged.
        use bitty_config::{EffectiveConfig, KeyName, ModKey, match_keymap, resolve_keymaps};
        let plain = AppModifiers::default();
        let with_alt = AppModifiers {
            alt: true,
            ..Default::default()
        };
        let with_super = AppModifiers {
            super_held: true,
            ..Default::default()
        };
        let cases: &[(NamedKey, KeyName)] = &[
            (NamedKey::F1, KeyName::F(1)),
            (NamedKey::F5, KeyName::F(5)),
            (NamedKey::F12, KeyName::F(12)),
            (NamedKey::F35, KeyName::F(35)),
            (NamedKey::Insert, KeyName::Insert),
            (NamedKey::Delete, KeyName::Delete),
            (NamedKey::Home, KeyName::Home),
            (NamedKey::End, KeyName::End),
            (NamedKey::PageUp, KeyName::PageUp),
            (NamedKey::PageDown, KeyName::PageDown),
        ];
        for (named, want) in cases {
            let event = test_key(LogicalKey::Named(*named));
            let r = key_ref_from_event(&event, &with_alt).expect("alt matchable");
            assert_eq!(r.key, *want, "alt+{named:?}");
            assert!(r.alt && !r.super_held, "alt mirror for {named:?}");
            let r = key_ref_from_event(&event, &with_super).expect("super matchable");
            assert_eq!(r.key, *want, "super+{named:?}");
            assert!(r.super_held && !r.alt, "super mirror for {named:?}");
            // A Mod-held press is matchable (bindable); bare falls through.
            let bare = key_ref_from_event(&event, &plain).expect("bare matchable");
            assert_eq!(bare.key, *want);
            assert!(!bare.ctrl && !bare.alt && !bare.shift && !bare.super_held);
        }
        // Bare presses match nothing under either default map (both mods),
        // so the intercept never consumes them: shell/PTY path unchanged.
        for mod_key in [ModKey::Alt, ModKey::Super] {
            let maps = resolve_keymaps(&EffectiveConfig {
                mod_key,
                ..Default::default()
            })
            .expect("defaults");
            for (named, _) in cases {
                let bare = key_ref_from_event(&test_key(LogicalKey::Named(*named)), &plain)
                    .expect("bare matchable");
                assert_eq!(
                    match_keymap(&maps, bare),
                    None,
                    "bare {named:?} is shell under mod {:?}",
                    mod_key
                );
            }
        }
        // Non-chord platform keys still have no chord identity (route to
        // the PTY): media leftovers collapse to `Other`, `Fn` is not a
        // bindable segment.
        assert!(
            key_ref_from_event(&test_key(LogicalKey::Named(NamedKey::Other)), &plain).is_none()
        );
        assert!(key_ref_from_event(&test_key(LogicalKey::Named(NamedKey::Fn)), &plain).is_none());
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

    fn esc_release() -> WindowEventKind {
        WindowEventKind::KeyboardInput(KeyEvent {
            logical_key: LogicalKey::Named(NamedKey::Escape),
            text: None,
            location: bitty_platform::KeyLocation::Standard,
            state: PressState::Released,
            repeat: false,
            is_synthetic: false,
        })
    }

    fn alt_mods_event() -> WindowEventKind {
        WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift: false,
            control: false,
            alt: true,
            super_pressed: false,
        })
    }

    fn clear_mods_event() -> WindowEventKind {
        WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift: false,
            control: false,
            alt: false,
            super_pressed: false,
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

    #[test]
    fn chrome_workspace_move_reparents_focused_leaf() {
        // CTX-0259: Mod+Shift+Number through the chrome arms reparents the
        // focused leaf (no kill, no switch, last-workspace guard holds).
        use bitty_config::ChromeAction;
        let mut app = workspace_test_app();
        app.apply_chrome_action(ChromeAction::WorkspaceNew);
        assert!(app.runtime.workspace_switch(0));
        // Split ws1 so the source has two leaves; focus the second.
        let moved_id = ViewId::new(9);
        let mut layout = app.runtime.layout().clone();
        let focused = app.runtime.focused_view().expect("focus");
        let old = layout.find_leaf(focused).cloned().expect("leaf");
        let fresh_leaf = View::new(moved_id, usize::from(old.cols()), usize::from(old.rows()));
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(fresh_leaf),
        );
        app.runtime.set_layout(layout);
        assert!(app.runtime.set_focus(moved_id));
        app.apply_chrome_action(ChromeAction::WorkspaceMove(2));
        assert_eq!(app.runtime.workspace_count(), 2);
        assert_eq!(app.runtime.active_workspace_index(), 0);
        assert_eq!(app.runtime.layout().leaf_count(), 1);
        assert!(!app.runtime.has_pending_ws_close());
        assert!(app.runtime.workspace_switch(1));
        assert!(app.runtime.layout().leaf_ids().contains(&moved_id));
        assert_eq!(app.runtime.focused_view(), Some(moved_id));
        // Invalid N warns fail-closed (state untouched).
        let tabline = app.runtime.workspaceline_text();
        app.apply_chrome_action(ChromeAction::WorkspaceMove(9));
        assert_eq!(app.runtime.workspaceline_text(), tabline);
        assert_eq!(app.runtime.layout().leaf_count(), 2);
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
        // Esc through the real intercept cancels via the CTX-0275 emergency
        // layer (consumed as chrome before any keymap match): the arm drops
        // with no kill.
        assert!(drive_chrome(&mut app, esc_press()));
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

    // CTX-0275 explicit DispatchPriority: headless regression suite.
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_priority_table_orders_emergency_modal_user_terminal() {
        // Pure classifier: emergency > modal > user > plugin (inert) >
        // terminal, over plain data (no display server, no live Runtime).
        use bitty_config::{ChromeAction as A, KeyName, KeyRef, SplitDir};
        let esc = KeyRef {
            key: KeyName::Escape,
            ctrl: false,
            alt: false,
            shift: false,
            super_held: false,
        };
        let alt_h = KeyRef {
            key: KeyName::Char('h'),
            ctrl: false,
            alt: true,
            shift: false,
            super_held: false,
        };
        let alt_w = KeyRef {
            key: KeyName::Char('w'),
            ctrl: false,
            alt: true,
            shift: false,
            super_held: false,
        };
        let paste_chord = KeyRef {
            key: KeyName::Char('v'),
            ctrl: true,
            alt: false,
            shift: true,
            super_held: false,
        };
        let bare_x = KeyRef {
            key: KeyName::Char('x'),
            ctrl: false,
            alt: false,
            shift: false,
            super_held: false,
        };
        // Emergency beats everything, even a user `escape` remap.
        assert_eq!(
            resolve_priority_for(esc, Some(A::CloseView), true, false),
            (DispatchPriority::Emergency, None)
        );
        assert_eq!(
            resolve_priority_for(esc, None, false, true),
            (DispatchPriority::Emergency, None)
        );
        // No modal: bound -> User, unbound -> Terminal (pre-0275 behavior).
        assert_eq!(
            resolve_priority_for(esc, Some(A::CloseView), false, false),
            (DispatchPriority::User, Some(A::CloseView))
        );
        assert_eq!(
            resolve_priority_for(esc, None, false, false),
            (DispatchPriority::Terminal, None)
        );
        // Unbound keys fall through even with a modal up (the dialog
        // captures commands, not typing).
        assert_eq!(
            resolve_priority_for(bare_x, None, true, false),
            (DispatchPriority::Terminal, None)
        );
        assert_eq!(
            resolve_priority_for(bare_x, None, true, true),
            (DispatchPriority::Terminal, None)
        );
        // Either gate captures a bound non-confirm chord...
        assert_eq!(
            resolve_priority_for(alt_h, Some(A::GotoSplit(SplitDir::Left)), true, false),
            (DispatchPriority::Modal, None)
        );
        assert_eq!(
            resolve_priority_for(alt_h, Some(A::GotoSplit(SplitDir::Left)), false, true),
            (DispatchPriority::Modal, None)
        );
        // ...but each modal's own repeat-confirm still dispatches as User...
        assert_eq!(
            resolve_priority_for(paste_chord, Some(A::PasteFromClipboard), true, false),
            (DispatchPriority::User, Some(A::PasteFromClipboard))
        );
        assert_eq!(
            resolve_priority_for(alt_w, Some(A::WorkspaceClose), false, true),
            (DispatchPriority::User, Some(A::WorkspaceClose))
        );
        // ...and crossed gestures stay captured (a close chord never
        // confirms a paste, a paste chord never confirms a close).
        assert_eq!(
            resolve_priority_for(alt_w, Some(A::WorkspaceClose), true, false),
            (DispatchPriority::Modal, None)
        );
        assert_eq!(
            resolve_priority_for(paste_chord, Some(A::PasteFromClipboard), false, true),
            (DispatchPriority::Modal, None)
        );
        // Plugin slot denies by default for every key shape.
        for keyref in [esc, alt_h, alt_w, paste_chord, bare_x] {
            assert!(
                match_plugin_binding(keyref).is_none(),
                "plugin layer must stay deny-by-default"
            );
        }
    }

    #[test]
    fn dispatch_priority_modal_captures_user_chords_but_not_typing() {
        // Suspicious clipboard (embedded newline) arms the paste-confirm
        // modal through the real chord; every later step drives the same
        // `drive_chrome` dispatch `handle_event` performs.
        let mut app = paste_test_app("line1\nline2");
        app.runtime.set_layout(two_pane_layout());
        app.runtime.set_focus(ViewId::new(1));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
        press_paste_chord(&mut app);
        assert!(app.runtime.has_pending_paste());
        assert!(app.modal_capture_active());
        // Bound focus chord behind the modal is captured: consumed, no
        // focus move, no PTY bytes, no press-to-release ownership taken.
        assert!(!drive_chrome(&mut app, alt_mods_event()));
        assert!(drive_chrome(&mut app, char_press("l", "l", false)));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
        assert!(app.runtime.drain_pending_input().is_empty());
        assert!(!app.chrome_held.contains(&bitty_config::KeyName::Char('l')));
        assert!(!drive_chrome(&mut app, char_release("l")));
        // Workspace close behind the modal is captured too: no arm, no kill.
        assert!(drive_chrome(&mut app, char_press("w", "w", false)));
        assert!(!app.runtime.has_pending_ws_close());
        assert_eq!(app.runtime.workspace_count(), 1);
        assert!(!drive_chrome(&mut app, char_release("w")));
        // Unbound typing still falls through to the terminal: unconsumed
        // with its byte delivered (fall-through unchanged). The alt latch
        // is cleared first so this proves plain typing, not Alt+X ESC-x.
        assert!(!drive_chrome(&mut app, clear_mods_event()));
        assert!(!drive_chrome(&mut app, char_press("x", "x", false)));
        assert_eq!(app.runtime.drain_pending_input(), b"x");
        assert!(!drive_chrome(&mut app, char_release("x")));
        // The modal pends throughout: nothing above resolved it early.
        assert!(app.runtime.has_pending_paste());
        // Release hygiene for the owned paste key and the alt latch.
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert!(!drive_chrome(&mut app, clear_mods_event()));
    }

    #[test]
    fn dispatch_priority_emergency_esc_overrides_user_remap() {
        // A user `escape` remap applies with no modal active but never
        // steals the emergency cancel while a confirmation pends.
        use bitty_config::{Chord, ChromeAction, ResolvedKeymap};
        let mut maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        maps.push(ResolvedKeymap {
            chord: Chord::parse("escape").expect("escape parses"),
            action: ChromeAction::CloseView,
            context: "global".to_string(),
            from_default: false,
        });
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.force_headless_clipboard();
        rt.clipboard_mut()
            .set_text("line1\nline2".to_string())
            .expect("headless set");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        app.runtime.set_layout(two_pane_layout());
        assert_eq!(app.runtime.leaf_count(), 2);
        press_paste_chord(&mut app);
        assert!(app.runtime.has_pending_paste());
        // Emergency: consumed here (not routed), pending dropped, layout
        // untouched — the `close_view` remap does NOT run.
        assert!(drive_chrome(&mut app, esc_press()));
        assert!(!app.runtime.has_pending_paste());
        assert_eq!(app.runtime.leaf_count(), 2);
        assert!(app.runtime.drain_pending_input().is_empty());
        // With no modal active the remap applies again: Esc closes a pane.
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert!(!drive_chrome(&mut app, clear_mods_event()));
        assert!(drive_chrome(&mut app, esc_press()));
        assert_eq!(app.runtime.leaf_count(), 1);
        assert!(app.runtime.drain_pending_input().is_empty());
        assert!(!drive_chrome(&mut app, esc_release()));
    }

    #[test]
    fn dispatch_priority_modal_confirm_gesture_still_dispatches() {
        // The modal's own confirm gesture (identical repeat of the arming
        // paste chord) dispatches as User and delivers exactly once.
        let mut app = paste_test_app("line1\nline2");
        press_paste_chord(&mut app);
        assert!(app.runtime.has_pending_paste());
        assert!(app.runtime.drain_pending_input().is_empty());
        // Release the key (ownership ends) while ctrl+shift stay latched,
        // then repeat the identical chord: confirm, not capture.
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert!(drive_chrome(&mut app, char_press("V", "V", false)));
        assert!(!app.runtime.has_pending_paste());
        assert_eq!(app.runtime.drain_pending_input(), b"line1\nline2");
    }

    #[test]
    fn dispatch_priority_fallthrough_unchanged_without_modal() {
        // No modal active: unbound keys route with their bytes (pre-0275
        // fall-through byte-identical), bound chords still dispatch.
        let mut app = paste_test_app("clean-paste");
        assert!(!app.modal_capture_active());
        assert!(!drive_chrome(&mut app, char_press("v", "v", false)));
        assert_eq!(app.runtime.drain_pending_input(), b"v");
        assert!(!drive_chrome(&mut app, char_release("v")));
        // Bound paste chord still dispatches as User and delivers clean
        // text immediately (no modal arms for clean input).
        press_paste_chord(&mut app);
        assert!(!app.runtime.has_pending_paste());
        assert_eq!(app.runtime.drain_pending_input(), b"clean-paste");
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert!(!drive_chrome(&mut app, clear_mods_event()));
    }

    #[test]
    fn chrome_font_zoom_round_trip_headless() {
        // CTX-0263: font zoom is per-window live size (not pane
        // `toggle_zoom`, not a config write). Chords resolve through the
        // single-owner intercept path and the layout is untouched.
        use bitty_config::{ChromeAction, KeyName, KeyRef, match_keymap};
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        // Every Ctrl spelling hits the zoom actions (US Shift+= covered).
        for (key, shift, action) in [
            ('=', false, ChromeAction::IncreaseFontSize),
            ('+', false, ChromeAction::IncreaseFontSize),
            ('=', true, ChromeAction::IncreaseFontSize),
            ('+', true, ChromeAction::IncreaseFontSize),
            ('-', false, ChromeAction::DecreaseFontSize),
            ('-', true, ChromeAction::DecreaseFontSize),
            ('0', false, ChromeAction::ResetFontSize),
        ] {
            let r = KeyRef {
                key: KeyName::Char(key),
                ctrl: true,
                alt: false,
                shift,
                super_held: false,
            };
            assert_eq!(
                match_keymap(&maps, r),
                Some(action),
                "chord {key:?}+shift={shift}"
            );
        }
        // Bare keys stay shell.
        for key in ['+', '-', '=', '0'] {
            let r = KeyRef {
                key: KeyName::Char(key),
                ctrl: false,
                alt: false,
                shift: false,
                super_held: false,
            };
            assert_eq!(match_keymap(&maps, r), None, "bare {key:?} stays shell");
        }
        // Actions drive the live runtime without touching the tree.
        let rt = Runtime::with_defaults().expect("must build");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        let leafs_before = app.runtime.leaf_count();
        app.apply_chrome_action(ChromeAction::IncreaseFontSize);
        assert!((app.runtime.font_size() - 13.0).abs() < f32::EPSILON);
        assert_eq!(app.runtime.leaf_count(), leafs_before);
        app.apply_chrome_action(ChromeAction::DecreaseFontSize);
        assert!((app.runtime.font_size() - 12.0).abs() < f32::EPSILON);
        app.apply_chrome_action(ChromeAction::IncreaseFontSize);
        app.apply_chrome_action(ChromeAction::ResetFontSize);
        assert!((app.runtime.font_size() - 12.0).abs() < f32::EPSILON);
        assert_eq!(app.runtime.leaf_count(), leafs_before);
    }
}
