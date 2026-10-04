//! Chrome-key intercept for the `bitty-terminal` composition root (CTX-0233).
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

use std::collections::HashSet;

use bitty_platform::{KeyEvent, LogicalKey, NamedKey, PressState, WindowEventKind};
use bitty_runtime::{
    FocusDirection, LayoutNode, PanelLayoutMode, PresentationMode, Runtime, SplitAxis, View,
    ViewCloseRequest, ViewId, WsCloseRequest,
};

use crate::spawn::spawn_pane_shell;
use crate::terminal_app::TerminalApp;

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

/// Chrome-owned key state coalesced out of `TerminalApp` (CTX-0481 state
/// slimming): the keymap table, the modifier mirror, press-to-release
/// ownership, and pane zoom move together and share one lifetime.
pub(crate) struct ChromeState {
    /// Resolved keymap table (shipped defaults + user overrides).
    pub(crate) keymaps: Vec<bitty_config::ResolvedKeymap>,
    /// App-side modifier mirror for chord matching.
    pub(crate) app_mods: AppModifiers,
    /// Chrome-owned keys with an unreleased press (CTX-0229 press-to-release
    /// ownership). A consumed chord owns its key until the physical release:
    /// repeats/duplicates arriving after modifier decay stay swallowed
    /// instead of leaking shell bytes (e.g. `Ctrl+Shift+V` paste followed by
    /// a `V` repeat with `Ctrl` already released must not type `V`).
    /// Releases and focus transitions clear entries (same staleness bound as
    /// the CTX-0187 mirror clear); bounded by simultaneously held keys.
    pub(crate) held: HashSet<bitty_config::KeyName>,
    /// Pane zoom state (CTX-0481, #762).
    pub(crate) zoom: ZoomState,
    /// Effective Leader binding (CTX-0715 / OQ-088, consumed CTX-0723 #981):
    /// every chord that arms the hint session plus the armed-window budget.
    /// Resolved from the effective config at startup; tests keep the
    /// platform default.
    pub(crate) leader: bitty_config::ResolvedLeader,
    /// Leader arming window with a caller-owned clock (CTX-0715 timeout and
    /// cancel semantics, consumed CTX-0723 #981). The clock base sits next
    /// to the state so `now_ms` stays monotonic for one app lifetime.
    pub(crate) leader_state: bitty_config::LeaderState,
    pub(crate) leader_clock: std::time::Instant,
    /// Hint collection generation, bumped once per Leader arming (CTX-0723,
    /// #981). Fresh batches per arming keep stale labels unreachable.
    pub(crate) hint_generation: u64,
    /// Panel-hosted external-editor session (CTX-0731, #982): at most one
    /// pending `$EDITOR` round trip; empty otherwise.
    pub(crate) editor: crate::editor_host::ExternalEditorHost,
    /// Hint session kill switch (CTX-0735 / OQ-089 #981): resolved from the
    /// effective config at startup (default-on). While disabled the Leader
    /// never arms — presses keep their normal owner (fail-open routing).
    pub(crate) hints_enabled: bool,
}

impl ChromeState {
    pub(crate) fn new(keymaps: Vec<bitty_config::ResolvedKeymap>) -> Self {
        Self {
            keymaps,
            app_mods: AppModifiers::default(),
            held: HashSet::new(),
            zoom: ZoomState::new(),
            leader: bitty_config::resolve_leader(None, None, bitty_config::LeaderPlatform::host())
                .unwrap_or(bitty_config::ResolvedLeader {
                    // Fail-closed: internal defaults are `const` chord
                    // strings, so this arm is unreachable in practice; an
                    // empty chord set simply never arms the Leader and keys
                    // keep their normal owner (fail-open routing).
                    chords: Vec::new(),
                    timeout_ms: bitty_config::LEADER_TIMEOUT_MS_DEFAULT,
                    from_default: true,
                }),
            leader_state: bitty_config::LeaderState::Idle,
            leader_clock: std::time::Instant::now(),
            hint_generation: 0,
            editor: crate::editor_host::ExternalEditorHost::new(),
            hints_enabled: true,
        }
    }

    /// Injects the effective-config Leader binding (startup path).
    pub(crate) fn with_leader(mut self, leader: bitty_config::ResolvedLeader) -> Self {
        self.leader = leader;
        self
    }

    /// Injects the effective-config hint kill switch (startup path).
    pub(crate) fn with_hints_enabled(mut self, enabled: bool) -> Self {
        self.hints_enabled = enabled;
        self
    }

    /// Caller-clock milliseconds for [`Self::leader_state`].
    pub(crate) fn leader_now_ms(&self) -> u64 {
        self.leader_clock
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

/// Pane zoom state (CTX-0481, #762; per-workspace CTX-0538).
///
/// The real tiled layout (`backup`) is captured when zoom engages together
/// with the single-leaf proxy installed in its place (`proxy`). The backup
/// is only restored while the runtime still holds that exact proxy: a layout
/// mutation landing meanwhile (e.g. a ctl verb drained between key events)
/// makes the pair stale, and restoring the older backup would silently drop
/// the newer panes. A stale backup is discarded with a warning instead.
///
/// CTX-0538 (LIVE-APP-002): entries are keyed by the workspace's stable
/// sequence (`ws{seq}` identity, CTX-0322), never a single slot. Workspace
/// switches therefore never compare one workspace's backup against another
/// workspace's live layout: zoom engaged in A survives a trip through B and
/// restores A exactly, while a disengage in B is a clean no-op.
#[derive(Debug, Default)]
pub(crate) struct ZoomState {
    /// Per-workspace entries keyed by stable workspace sequence.
    entries: std::collections::BTreeMap<u64, ZoomEntry>,
}

/// One workspace's zoom pair (CTX-0538).
#[derive(Debug)]
struct ZoomEntry {
    /// Real tiled layout while zoomed.
    backup: LayoutNode,
    /// Proxy layout installed at engage time (staleness identity).
    proxy: LayoutNode,
}

/// Stable sequence of the active workspace, if one exists (CTX-0538).
fn active_workspace_seq(runtime: &Runtime) -> Option<u64> {
    runtime.workspace_seq_at(runtime.active_workspace_index())
}

impl ZoomState {
    pub(crate) const fn new() -> Self {
        Self {
            entries: std::collections::BTreeMap::new(),
        }
    }

    /// Whether the active workspace currently holds a zoom backup.
    pub(crate) fn is_zoomed(&self, runtime: &Runtime) -> bool {
        active_workspace_seq(runtime).is_some_and(|seq| self.entries.contains_key(&seq))
    }

    /// Leaf count of the active workspace's real tree while zoomed
    /// (diagnostics).
    pub(crate) fn backup_leaf_count(&self, runtime: &Runtime) -> Option<usize> {
        let seq = active_workspace_seq(runtime)?;
        self.entries
            .get(&seq)
            .map(|entry| entry.backup.leaf_ids().len())
    }

    /// Engages zoom on `view`: captures the real tree and installs the
    /// single-leaf proxy. Refuses when the active workspace is already
    /// zoomed or the view is not a leaf, leaving the layout untouched.
    pub(crate) fn engage(&mut self, runtime: &mut Runtime, view: ViewId) -> bool {
        self.prune(runtime);
        let Some(seq) = active_workspace_seq(runtime) else {
            return false;
        };
        if self.entries.contains_key(&seq) {
            return false;
        }
        let Some(leaf) = runtime.layout().find_leaf(view).cloned() else {
            return false;
        };
        let backup = runtime.layout().clone();
        let proxy = LayoutNode::leaf(leaf);
        runtime.set_layout_preserve_grid(proxy.clone());
        self.entries.insert(seq, ZoomEntry { backup, proxy });
        true
    }

    /// Disengages zoom in the active workspace. Restores the real tree only
    /// while the runtime still holds that workspace's recorded proxy; a
    /// stale proxy keeps the current layout and drops the entry with a
    /// warning. Returns whether a restore happened.
    pub(crate) fn disengage(&mut self, runtime: &mut Runtime) -> bool {
        let Some(seq) = active_workspace_seq(runtime) else {
            return false;
        };
        let Some(entry) = self.entries.remove(&seq) else {
            return false;
        };
        if same_layout_shape(runtime.layout(), &entry.proxy) {
            runtime.set_layout_preserve_grid(entry.backup);
            return true;
        }
        eprintln!(
            "warning: zoom backup stale (layout changed while zoomed) — keeping current layout"
        );
        false
    }

    /// Restores zoom before a layout mutation (keymap path and the ctl drain
    /// hook). Returns whether the real tree was restored.
    pub(crate) fn restore_for_mutation(&mut self, runtime: &mut Runtime) -> bool {
        self.prune(runtime);
        if !self.is_zoomed(runtime) {
            return false;
        }
        let restored = self.disengage(runtime);
        if restored {
            eprintln!("bitty: zoom restored for layout mutation");
        }
        restored
    }

    /// Drops entries whose workspace no longer exists (closed, or replaced
    /// by a session restore), so the map stays bounded by the live
    /// workspace count instead of the session's close history.
    pub(crate) fn prune(&mut self, runtime: &Runtime) {
        self.entries
            .retain(|seq, _| runtime.workspace_index_by_seq(*seq).is_some());
    }
}

/// Structural identity of two layout trees, ignoring per-leaf cell
/// dimensions and allocation origins (CTX-0481).
///
/// The runtime reflows every `set_layout` in place, so `View` dimensions and
/// origins differ from the just-installed proxy; leaf ids and split/stack/
/// overlay structure are what identify "the runtime still holds the zoom
/// proxy". Ratios compare by bit pattern (the tree is deterministic).
fn same_layout_shape(a: &LayoutNode, b: &LayoutNode) -> bool {
    match (a, b) {
        (LayoutNode::Leaf(x), LayoutNode::Leaf(y)) => x.id() == y.id(),
        (
            LayoutNode::Split {
                axis: ax,
                ratio: rx,
                first: f1,
                second: s1,
            },
            LayoutNode::Split {
                axis: ay,
                ratio: ry,
                first: f2,
                second: s2,
            },
        ) => {
            ax == ay
                && rx.to_bits() == ry.to_bits()
                && same_layout_shape(f1, f2)
                && same_layout_shape(s1, s2)
        }
        (LayoutNode::Stack(xs), LayoutNode::Stack(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| same_layout_shape(x, y))
        }
        (
            LayoutNode::Overlay {
                base: b1,
                overlay: o1,
                bounds: r1,
                tier: t1,
            },
            LayoutNode::Overlay {
                base: b2,
                overlay: o2,
                bounds: r2,
                tier: t2,
            },
        ) => r1 == r2 && t1 == t2 && same_layout_shape(b1, b2) && same_layout_shape(o1, o2),
        _ => false,
    }
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

/// Match one press against the resolved keymap table, with the physical
/// base-key fallback for shifted symbols (issue #1446).
///
/// The event's logical character already carries Shift and the layout, so a
/// physical `Mod+Shift+2` — the accepted DEC-0034 workspace-move gesture,
/// spelled `shift+alt+2` — arrives as `@` with `shift=true` and can never
/// equal the base-key chord by exact equality (single-owner matching, no
/// fuzzy spellings). Matching therefore tries the reported spelling first —
/// every exact binding, default or user, keeps its precedence — and falls
/// back to the physical base-key spelling
/// ([`bitty_config::KeyRef::unshifted_base`]) when that one missed. Ghostty
/// carries the same `unshifted_codepoint` on its key events and matches
/// character keybinds on it. Without the
/// fallback the move gesture was unreachable and the shifted symbol leaked
/// through the single-owner intercept into the PTY.
pub(crate) fn match_chrome_keymap(
    maps: &[bitty_config::ResolvedKeymap],
    keyref: bitty_config::KeyRef,
) -> Option<bitty_config::ChromeAction> {
    bitty_config::match_keymap(maps, keyref).or_else(|| {
        keyref
            .unshifted_base()
            .and_then(|base| bitty_config::match_keymap(maps, base))
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

/// Split the target leaf along `axis`, keeping the target view and adding a
/// fresh sibling. The new pane goes first for `Left`/`Up`, second otherwise.
/// Returns false when the target id is not in the tree.
///
/// The target is usually the focused leaf (explicit `new_split:<dir>`); the
/// adaptive `new_panel` path passes `Runtime::panel_bisect_target` in
/// `PanelLayoutMode::BisectLargest` (CTX-0964, #1698, DEC-0099).
pub(crate) fn split_focused_leaf(
    layout: &mut LayoutNode,
    target: ViewId,
    axis: SplitAxis,
    new_id: ViewId,
    place_new_first: bool,
) -> bool {
    match layout {
        LayoutNode::Leaf(v) => {
            if v.id() != target {
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
            split_focused_leaf(first, target, axis, new_id, place_new_first)
                || split_focused_leaf(second, target, axis, new_id, place_new_first)
        }
        LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|c| split_focused_leaf(c, target, axis, new_id, place_new_first)),
        LayoutNode::Overlay { base, overlay, .. } => {
            split_focused_leaf(base, target, axis, new_id, place_new_first)
                || split_focused_leaf(overlay, target, axis, new_id, place_new_first)
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

/// Nudge the enclosing split ratio 0.1 in the given direction (issue
/// #1445: the divider moves left/up (-) or right/down (+), matching the
/// border-drag geometry where a positive drag delta toward `second`
/// grows the first pane via [`LayoutNode::resize_split_by_drag`]).
/// `set_split_ratio_at` clamps to `0.10..=0.90`. Returns false when no
/// matching split holds focus.
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
    let (target, ratio, _focus_in_first) = match out {
        Some(t) => t,
        None => return false,
    };
    // Divider moves in the pressed direction regardless of which side
    // holds focus (Hyprland/Niri model): Right/Down grows the first pane
    // (+0.1), Left/Up shrinks it (-0.1). The previous focus-dependent
    // sign moved the same divider opposite ways for the same key
    // depending on focus, which read as swapped hjkl directions.
    let positive = matches!(dir, D::Right | D::Down);
    let delta = if positive { 0.1 } else { -0.1 };
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
/// 2. [`DispatchPriority::Modal`] — a capturing modal (close-confirm or
///    panel overlay) captures command dispatch: bound chrome chords that
///    are NOT the modal's own confirm gesture are swallowed (consumed, no
///    action, no PTY bytes) so no state mutates behind the dialog. A
///    pending paste alone never captures (issue #1336: the banner is
///    informational). Unbound keys still fall through to
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
/// [`bitty_config::match_keymap`] result for `keyref`, and the pending flags
/// are the app's modal surface (`Runtime::has_pending_paste` /
/// `Runtime::has_pending_ws_close` / `Runtime::has_pending_close_confirm`
/// plus the CTX-0482 panel-overlay bit fed by
/// `OverlayManager::modal_active`).
/// Returns the winning layer plus the action to run when the layer dispatches
/// one (`User` only; `Modal` swallows, `Plugin` is inert, `Terminal` routes).
///
/// `close_confirm_pending` is any CTX-0370 close arm (view or window);
/// `close_confirm_view` is only true when the arm targets the currently
/// focused view, so the `close_view` chord confirms exactly that arm.
///
/// Table (first match wins):
/// - `Esc` with a pending confirmation -> `(Emergency, None)` (the cancel
///   path; a panel overlay does not take Esc onto that path).
/// - `Esc` with only the panel-overlay modal -> `(Terminal, None)`: the
///   panel owns its own dismissal and a remapped `Esc` must not run.
/// - a capturing modal (workspace-close, view/window close, or panel
///   overlay) + the bound chord that confirms THAT modal (repeat the arming
///   chord: `workspace_close` while a workspace-close pends, `close_view`
///   while a view-close arm on the focused view pends) -> `(User, action)`.
/// - a capturing modal + any other bound chord -> `(Modal, None)`
///   (captured).
/// - a pending paste alone never captures (issue #1336): the banner is
///   informational, so every bound chord — zoom, focus, splits, and the
///   paste-again confirm — dispatches as `(User, action)` through the normal
///   path below.
/// - bound chord, no modal -> `(User, action)`.
/// - unbound key while a modal pends -> `(Terminal, None)` (fall-through
///   unchanged: the dialog captures commands, not typing).
/// - unbound key, no modal -> `(Terminal, None)` (existing fall-through).
fn resolve_priority_for(
    keyref: bitty_config::KeyRef,
    matched: Option<bitty_config::ChromeAction>,
    paste_pending: bool,
    ws_close_pending: bool,
    close_confirm_pending: bool,
    close_confirm_view: bool,
    panel_modal_active: bool,
) -> (DispatchPriority, Option<bitty_config::ChromeAction>) {
    use bitty_config::{ChromeAction as A, KeyName};
    let confirm_modal_active = paste_pending || ws_close_pending || close_confirm_pending;
    let modal_active = confirm_modal_active || panel_modal_active;
    // Issue #1336: the paste banner is informational, never capturing. While
    // only a paste pends, bound chords route normally (zoom, focus, splits
    // keep working) and paste-again-to-confirm reuses the plain `User` path.
    // The capturing set stays the close/panel modals, where running an
    // action behind the dialog would mutate state under a destructive
    // confirm.
    let capturing = ws_close_pending || close_confirm_pending || panel_modal_active;
    // Emergency/reserved first: Esc cancels any pending confirmation even
    // when the user remapped `escape` (the remap only applies with no
    // modal active, where this arm never fires).
    if confirm_modal_active && keyref.key == KeyName::Escape {
        return (DispatchPriority::Emergency, None);
    }
    // Panel-overlay modal: Esc is the overlay's own dismissal key and every
    // bound chord is captured below. Route it to the runtime (the panel
    // path) instead of the confirm-cancel emergency arm, and never let a
    // remapped `Esc` action run behind the modal.
    if modal_active && keyref.key == KeyName::Escape {
        return (DispatchPriority::Terminal, None);
    }
    match matched {
        Some(action) if capturing => {
            let confirms_paste = paste_pending && matches!(action, A::PasteFromClipboard);
            let confirms_ws = ws_close_pending && matches!(action, A::WorkspaceClose);
            // CTX-0370: repeating the pane-close chord confirms the pane
            // arm; a window arm has no chord (the repeated OS close request
            // is its confirm gesture, handled by the runtime).
            let confirms_view_close = close_confirm_view && matches!(action, A::CloseView);
            if confirms_paste || confirms_ws || confirms_view_close {
                // The modal's own confirm gesture reuses the normal user
                // action path (repeat Alt+W kills; a paste-again confirm
                // behind a close/panel modal delivers through the same
                // path); every other bound chord is captured below.
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
    /// applies to the real tree instead of the single-leaf zoom view
    /// (CTX-0481: staleness-checked inside [`ZoomState`]).
    pub(crate) fn restore_zoom(&mut self) -> bool {
        self.chrome.zoom.restore_for_mutation(&mut self.runtime)
    }

    /// Applies one `fold_toggle`/`fold_expand`/`fold_collapse` verb to the
    /// focused view's latest command block (CTX-0723 #980).
    ///
    /// `None` (no shell-integration marks yet) warns loudly instead of
    /// inventing a target; terminal truth is untouched either way.
    fn apply_fold_verb(&mut self, action: bitty_runtime::cw_present::CwFoldAction, name: &str) {
        match self.runtime.cw_fold_latest(action) {
            Some(folded) => {
                eprintln!(
                    "bitty: keymap {name} -> latest command {}",
                    if folded { "folded" } else { "unfolded" }
                );
            }
            None => {
                eprintln!(
                    "warning: keymap {name} has no command block yet (no shell integration marks) — ignoring"
                );
            }
        }
    }

    /// Shared new-leaf creation for `new_split:<dir>` and `new_panel`
    /// (CTX-0838 #1441).
    ///
    /// Fail-closed, headless-testable: no focused pane warns and keeps the
    /// layout; a refused appearance keeps the layout; a missing `target`
    /// leaf in the tree warns and keeps the layout. On success the fresh
    /// leaf gets a best-effort shell, focus follows it
    /// (Hyprland/kitty/ghostty parity), and `label` names the action in the
    /// loud log line.
    ///
    /// `target` is the leaf to split: explicit `new_split:<dir>` passes the
    /// focused leaf, while `new_panel` in `PanelLayoutMode::BisectLargest`
    /// (CTX-0964, #1698, DEC-0099) passes `Runtime::panel_bisect_target`
    /// (the largest-area leaf, not focus). Spawn sizing and cwd inheritance
    /// still read the focused pane, so the fresh shell matches its
    /// neighbors.
    fn apply_new_leaf(
        &mut self,
        target: ViewId,
        axis: SplitAxis,
        place_new_first: bool,
        label: &str,
    ) {
        self.restore_zoom();
        if self.runtime.focused_view().is_none() {
            eprintln!("warning: keymap {label} has no focused pane — ignoring");
            return;
        }
        let mut layout = self.runtime.layout().clone();
        // CTX-0378: the id comes from the runtime-wide allocator
        // (every slot + the live layout), never this layout's max + 1:
        // `pane_sessions` is keyed globally by `ViewId`, so a local
        // scan would alias another workspace's shell and the spawn
        // below would replace its session.
        let new_id = self.runtime.next_view_id_global();
        // CTX-0343 first match: a previously inert `ws:`/`view:`
        // selector can match the fresh `View`; fail the creation
        // closed before the layout commits it.
        if let Err(err) = self.runtime.validate_new_view_appearance(new_id) {
            eprintln!("warning: keymap {label} refused: {err}");
            return;
        }
        if split_focused_leaf(&mut layout, target, axis, new_id, place_new_first) {
            self.runtime.set_layout(layout);
            // CTX-0176: the fresh leaf gets its own shell/PTY sized
            // to its allocation — best-effort (startup parity). On
            // failure the pane stays empty (CTX-0359: it never
            // paints or feeds the primary grid) with a loud warning.
            let (cols, rows) = self
                .runtime
                .layout_allocations()
                .iter()
                .find(|(id, _)| *id == new_id)
                .map(|(_, r)| (r.width.max(1), r.height.max(1)))
                .unwrap_or((80, 24));
            let spawn_result =
                spawn_pane_shell(&mut self.runtime, &self.spawn_spec, new_id, cols, rows);
            // CTX-0364: focus follows the fresh pane (kitty/ghostty
            // parity). Set after the spawn so CTX-0357 cwd
            // inheritance still reads the source pane as focused.
            self.runtime.set_focus(new_id);
            match spawn_result {
                Ok(()) => eprintln!(
                    "bitty: keymap {label} -> leafs={} focused={:?} pane_shell={new_id:?} pid={:?}",
                    self.runtime.leaf_count(),
                    self.runtime.focused_view(),
                    self.runtime.pane_pid(&new_id),
                ),
                Err(err) => eprintln!(
                    "warning: keymap {label} pane shell spawn failed ({err}) — pane {new_id:?} stays empty",
                ),
            }
        } else {
            eprintln!("warning: keymap {label} found no target pane — ignoring");
        }
    }

    /// Execute one bound chrome action (single owner: the PTY never sees the
    /// chord). All mutations go through existing `Runtime`/`LayoutNode` APIs;
    /// refusals warn and keep the current layout.
    pub(crate) fn apply_chrome_action(&mut self, action: bitty_config::ChromeAction) {
        use bitty_config::ChromeAction as A;
        // CTX-0943: a user focus-switch ends the transient input capture
        // (contract `focus_switched`), wherever the action originates
        // (key chord, plugin workspace request, ctl verb). Idempotent
        // no-op with no capture active.
        if Self::is_overlay_focus_switch(action) {
            self.revoke_overlay_capture();
        }
        match action {
            A::GotoSplit(dir) => {
                self.restore_zoom();
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
                self.restore_zoom();
                let next = self.runtime.move_focus(FocusDirection::Next);
                eprintln!(
                    "bitty: keymap focus_next -> {next:?} leafs={}",
                    self.runtime.leaf_count()
                );
            }
            A::FocusPrev => {
                self.restore_zoom();
                let next = self.runtime.move_focus(FocusDirection::Prev);
                eprintln!(
                    "bitty: keymap focus_prev -> {next:?} leafs={}",
                    self.runtime.leaf_count()
                );
            }
            A::FocusId(n) => {
                self.restore_zoom();
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
                // W-103 S-5 cutover (CTX-0929) through E-CUT retirement
                // (CTX-0968): an ACTIVE composer plugin owns the editing UX
                // via overlay/capture/submit/editor, so the open verb
                // dispatches to the plugin (`<id>:open`). Every other state
                // (safe mode, zero-plugin startup, uninstalled,
                // version/capability mismatch) is retained-deleted:
                // fail-closed with a diagnostic, no Core session.
                use crate::composer_owner::{COMPOSER_COMMAND_OPEN, ComposerOwner};
                match self.composer_owner() {
                    ComposerOwner::Plugin => {
                        match self.dispatch_composer_command(COMPOSER_COMMAND_OPEN) {
                            Ok(()) => eprintln!(
                                "bitty: keymap open_composer -> composer plugin owns editing UX (overlay/capture)"
                            ),
                            Err(diagnostic) => {
                                eprintln!(
                                    "warning: composer plugin open failed ({diagnostic}) — no retained fallback (E-CUT retired)"
                                );
                                self.open_retained_composer();
                            }
                        }
                    }
                    ComposerOwner::RetainedCore(reason) => {
                        eprintln!(
                            "bitty: keymap open_composer -> no composer (retired Core, {}; plugin owns editing UX)",
                            reason.code()
                        );
                        self.open_retained_composer();
                    }
                }
            }
            A::FoldToggle => self.apply_fold_verb(
                bitty_runtime::cw_present::CwFoldAction::Toggle,
                "fold_toggle",
            ),
            A::FoldExpand => self.apply_fold_verb(
                bitty_runtime::cw_present::CwFoldAction::Expand,
                "fold_expand",
            ),
            A::FoldCollapse => self.apply_fold_verb(
                bitty_runtime::cw_present::CwFoldAction::Collapse,
                "fold_collapse",
            ),
            A::TogglePalette => {
                // CTX-0647 / GitHub #1003 (palette user entry): the palette
                // ships as the `palette` Lua plugin after the OQ-053 split
                // (the old bundled Rust helper was removed in CTX-0922) and
                // the SDK has no focusable overlay/input capture yet, so
                // there is no Core overlay to toggle. Until that host API
                // lands, `toggle_palette` parses for forward
                // compat but is never in defaults: unbound Ctrl+Shift+P
                // reaches the shell, and an explicitly bound chord is
                // consumed here as inert with a loud warning (no overlay,
                // no routing change, Normal Mode stays byte-identical).
                eprintln!(
                    "warning: keymap toggle_palette is not yet shipped (see #1003); chord ignored, no palette opened"
                );
            }
            A::NewSplit(dir) => {
                let place_new_first = matches!(
                    dir,
                    bitty_config::SplitDir::Left | bitty_config::SplitDir::Up
                );
                let label = format!("new_split:{}", dir.canonical());
                // Explicit directional splits always bisect the focused
                // leaf, independent of `PanelLayoutMode` (which only
                // controls the adaptive `NewPanel` path below).
                let Some(target) = self.runtime.focused_view() else {
                    eprintln!("warning: keymap {label} has no focused pane — ignoring");
                    return;
                };
                self.apply_new_leaf(target, split_dir_to_axis(dir), place_new_first, &label);
            }
            A::NewPanel => {
                // CTX-0838 (#1441) / CTX-0881: 4-way spiral panel creation. Axis
                // follows the focused leaf's cell allocation (wide splits
                // side-by-side, tall stacks, square ties break side-by-side);
                // placement follows the spiral cycle (Right -> Down -> Left -> Up)
                // and focus follows the fresh pane. Explicit `new_split:<dir>` above
                // keeps its fixed axis for directional splits.
                //
                // CTX-0964 (#1698, DEC-0099): in `BisectLargest` mode the
                // split lands on `panel_bisect_target` (largest-area leaf)
                // with its raw `width >= height` axis instead of the focused
                // leaf; `Spiral`/`Dwindle` keep the focused leaf. The axis
                // already comes from `panel_split_axis` (bisect-aware); only
                // the target needs the same switch here.
                let focused = match self.runtime.focused_view() {
                    Some(id) => id,
                    None => {
                        eprintln!("warning: keymap new_panel has no focused pane — ignoring");
                        return;
                    }
                };
                // Review (#1441): restore zoom before measuring — a zoomed
                // view collapses the live layout to one leaf, so the axis
                // must come from the restored layout that `apply_new_leaf`
                // will split. Idempotent: `apply_new_leaf` restores again.
                self.restore_zoom();
                let axis = self.runtime.panel_split_axis(focused);
                let place_new_first = self.runtime.panel_split_place_new_first();
                let target = if self.runtime.panel_layout_mode() == PanelLayoutMode::BisectLargest {
                    self.runtime.panel_bisect_target().unwrap_or(focused)
                } else {
                    focused
                };
                self.apply_new_leaf(target, axis, place_new_first, "new_panel");
            }
            A::CloseView => {
                let focused = match self.runtime.focused_view() {
                    Some(id) => id,
                    None => {
                        eprintln!("warning: keymap close_view has no focused pane — ignoring");
                        return;
                    }
                };
                // A zoomed view collapses the live layout to one leaf, so
                // consult the backup the restore would bring back before
                // refusing a real multi-pane close.
                let effective_leaf_count = self
                    .chrome
                    .zoom
                    .backup_leaf_count(&self.runtime)
                    .unwrap_or_else(|| self.runtime.leaf_count());
                if effective_leaf_count <= 1 {
                    eprintln!("warning: keymap close_view refused (last pane) — ignoring");
                    return;
                }
                // CTX-0370 close-confirm gate: a pane with a running
                // foreground job never closes on one gesture (repeat the
                // chord confirms, Esc cancels). The zoom restore and layout
                // surgery below run only on the proceed path, so a cancel
                // leaves the zoomed layout untouched.
                match self.runtime.view_close_request(focused) {
                    ViewCloseRequest::Pending { summary } => {
                        eprintln!("bitty: keymap close_view PENDING -> {summary}");
                        return;
                    }
                    ViewCloseRequest::Proceed => {}
                }
                self.restore_zoom();
                let mut layout = self.runtime.layout().clone();
                if close_focused_leaf(&mut layout, focused) {
                    // CTX-0359: an explicit close is the only layout change
                    // allowed to re-home the primary owner; a plain
                    // `set_layout` (zoom, restore) must preserve it.
                    self.runtime.set_layout_closing(layout, focused);
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
            A::JumpToPromptPrev => {
                // CTX-0952 (issue #1670): one prompt toward scrollback,
                // target at viewport top. No marks: loud no-op, shell
                // keeps the bytes (fail-closed, never a mis-jump).
                if self
                    .runtime
                    .jump_to_prompt(bitty_runtime::runtime::shell_jump::PromptDirection::Prev)
                {
                    eprintln!("bitty: keymap jump_to_prompt:prev -> jumped");
                } else {
                    eprintln!(
                        "warning: keymap jump_to_prompt:prev found no prompt above — ignoring"
                    );
                }
            }
            A::JumpToPromptNext => {
                // CTX-0952: mirror toward live.
                if self
                    .runtime
                    .jump_to_prompt(bitty_runtime::runtime::shell_jump::PromptDirection::Next)
                {
                    eprintln!("bitty: keymap jump_to_prompt:next -> jumped");
                } else {
                    eprintln!(
                        "warning: keymap jump_to_prompt:next found no prompt below — ignoring"
                    );
                }
            }
            A::SelectCommandOutput => {
                // CTX-0952: select exactly the last command's output rows
                // (ghostty `selectOutput` shape). Empty/absent marks: loud
                // no-op, live selection untouched.
                if self.runtime.select_command_output() {
                    eprintln!("bitty: keymap select_command_output -> selected");
                } else {
                    eprintln!(
                        "warning: keymap select_command_output found no command output — ignoring"
                    );
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
                // Issue #1365: Alt+N clamps to the last workspace when N
                // exceeds the live count; zero fails closed with no state
                // change. Reuses the workspace_switch path via
                // workspace_focus_clamped.
                match self.runtime.workspace_focus_clamped(n) {
                    Some(index) => eprintln!(
                        "bitty: keymap workspace_focus:{n} -> workspace {} ({})",
                        index + 1,
                        self.runtime.workspaceline_text()
                    ),
                    None => eprintln!(
                        "warning: keymap workspace_focus:{n} has no such workspace ({}) — ignoring",
                        self.runtime.workspaceline_text()
                    ),
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
            A::WorkspaceSwap(n) => {
                self.restore_zoom();
                match self.runtime.workspace_swap_current_with(n) {
                    Ok((from_seq, to_seq)) => eprintln!(
                        "bitty: keymap workspace_swap:{n} -> swapped ws:{from_seq} <-> ws:{to_seq} ({})",
                        self.runtime.workspaceline_text()
                    ),
                    Err(err) => eprintln!(
                        "warning: keymap workspace_swap:{n} refused ({err}) ({}) — ignoring",
                        self.runtime.workspaceline_text()
                    ),
                }
            }
            A::ToggleZoom => {
                // CTX-0538: zoom is scoped to the active workspace, so a
                // toggle here engages/disengages only this workspace's
                // entry (closes/restores prune the dead keys).
                self.chrome.zoom.prune(&self.runtime);
                if self.chrome.zoom.is_zoomed(&self.runtime) {
                    if self.chrome.zoom.disengage(&mut self.runtime) {
                        eprintln!(
                            "bitty: keymap toggle_zoom off -> leafs={} focused={:?}",
                            self.runtime.leaf_count(),
                            self.runtime.focused_view()
                        );
                    }
                } else {
                    let focused = match self.runtime.focused_view() {
                        Some(id) => id,
                        None => {
                            eprintln!("warning: keymap toggle_zoom has no focused pane — ignoring");
                            return;
                        }
                    };
                    if self.chrome.zoom.engage(&mut self.runtime, focused) {
                        eprintln!("bitty: keymap toggle_zoom on -> {focused:?}");
                    } else {
                        eprintln!("warning: keymap toggle_zoom found no focused pane — ignoring");
                    }
                }
            }
            A::ToggleFloating => {
                // CTX-0962 (#1695): toggle the focused leaf Tiled <-> Floating
                // through the `bitty.workspace:floating-toggle` primitive
                // semantics (`bitty_ui::presentation::toggle_floating`):
                // single command so the chord never desyncs from leaf state.
                // Validate on the live tree BEFORE touching zoom: the zoom
                // proxy clones the focused leaf, so its presentation is the
                // real one. Fullscreen/Scratchpad leaves therefore reject
                // here with a warning and zero state change (no zoom
                // restore, no layout write). For a valid toggle, zoom
                // restores next so the mutation lands on the real tiled
                // tree, not the zoom proxy; the layout solver ignores the
                // mode, so the round trip restores the exact prior
                // allocation with no anchor bookkeeping.
                let focused = match self.runtime.focused_view() {
                    Some(id) => id,
                    None => {
                        eprintln!("warning: keymap toggle_floating has no focused pane — ignoring");
                        return;
                    }
                };
                let current = match self.runtime.layout().find_leaf(focused) {
                    Some(leaf) => leaf.presentation(),
                    None => {
                        eprintln!(
                            "warning: keymap toggle_floating found no focused pane — ignoring"
                        );
                        return;
                    }
                };
                let next = match current {
                    PresentationMode::Tiled => PresentationMode::Floating,
                    PresentationMode::Floating => PresentationMode::Tiled,
                    other => {
                        eprintln!(
                            "warning: keymap toggle_floating needs a tiled or floating leaf, found {other} — ignoring"
                        );
                        return;
                    }
                };
                if !PresentationMode::can_transition(current, next) {
                    eprintln!(
                        "warning: keymap toggle_floating transition rejected: {current} -> {next} — ignoring"
                    );
                    return;
                }
                self.restore_zoom();
                let mut layout = self.runtime.layout().clone();
                match layout.find_leaf_mut(focused) {
                    Some(leaf) => {
                        if !PresentationMode::request_transition(leaf, next) {
                            eprintln!(
                                "warning: keymap toggle_floating transition rejected: {current} -> {next} — ignoring"
                            );
                            return;
                        }
                    }
                    None => {
                        eprintln!(
                            "warning: keymap toggle_floating found no focused pane — ignoring"
                        );
                        return;
                    }
                }
                self.runtime.set_layout(layout);
                eprintln!("bitty: keymap toggle_floating -> {focused:?} {current} -> {next}");
            }
            A::ToggleHelp => {
                // CTX-0265 which-key help popup: rows regenerate from the
                // live keymap table on EVERY show, so the overlay lists
                // exactly what is bound (user rebinds, added chords, and
                // the Super-flip spelling included) — never a hardcoded
                // copy. Repeating the chord hides it again; `Esc`
                // dismisses through the runtime key path; the overlay is
                // present-layer only (never grid truth).
                let rows = bitty_config::keymap::help_rows_from_keymaps(&self.chrome.keymaps);
                let bindings = rows.len();
                self.runtime.set_help_rows(rows);
                let visible = self.runtime.toggle_help();
                eprintln!("bitty: keymap toggle_help -> visible={visible} bindings={bindings}");
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
    /// One capture rule for every modal kind (CTX-0482, #763): the
    /// runtime-owned pending confirmations (suspicious paste, workspace
    /// kill, pane close) plus the panel-overlay modal bit
    /// ([`bitty_runtime::Runtime::overlay_modal_active`]), which the panel
    /// integration drives from `OverlayManager::modal_active()`. Copy and
    /// search modes keep their bespoke modal arms in
    /// [`Self::intercept_chrome_key`] (their confirm gestures differ), but
    /// every new modal surface feeds this predicate instead of growing a
    /// fourth gate.
    pub(crate) fn modal_capture_active(&self) -> bool {
        self.runtime.has_pending_paste()
            || self.runtime.has_pending_ws_close()
            || self.runtime.has_pending_close_confirm()
            || self.runtime.overlay_modal_active()
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
        // Single capture predicate first (the panel-overlay bit feeds
        // `modal_capture_active` itself); the per-gate reads below only
        // pick the confirm gesture.
        let (
            paste_pending,
            ws_close_pending,
            close_confirm_pending,
            close_confirm_view,
            panel_modal_active,
        ) = if self.modal_capture_active() {
            (
                self.runtime.has_pending_paste(),
                self.runtime.has_pending_ws_close(),
                self.runtime.has_pending_close_confirm(),
                // CTX-0370: only an arm on the currently focused view is
                // confirmed by the close-view chord.
                self.runtime
                    .pending_close_view()
                    .is_some_and(|view| self.runtime.focused_view() == Some(view)),
                self.runtime.overlay_modal_active(),
            )
        } else {
            (false, false, false, false, false)
        };
        resolve_priority_for(
            keyref,
            matched,
            paste_pending,
            ws_close_pending,
            close_confirm_pending,
            close_confirm_view,
            panel_modal_active,
        )
    }

    /// Run the emergency `Esc`-cancels-modal gesture (CTX-0275).
    ///
    /// Routes the press through `Runtime::handle_key_event` — the same
    /// cancel path a routed `Esc` takes today
    /// (`cancel_pending_on_escape`: drops the pending paste, the
    /// workspace-close arm, and/or the view/window close confirmation;
    /// consumes the key so it never reaches the PTY)
    /// — then consumes it here so a user `escape` remap cannot steal the
    /// cancel. Loud paste reporting mirrors `handle_event`'s CTX-0186 probe
    /// byte-for-byte (a gated paste is never silent); workspace-close and
    /// close-confirm cancellation stays silent exactly as today. Always
    /// returns `true`.
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
        if let Some(win) = self.window.handle.as_ref() {
            win.request_redraw();
        }
        true
    }

    /// Present-path modal routing (CTX-0723): Leader/hint arming,
    /// one tier below the copy/search modals and above the user keymap.
    ///
    /// Ordering inside this tier:
    /// 1. Reap an expired Leader window first (fail-open: the press keeps
    ///    its normal owner and routes to the shell).
    /// 2. An armed hint session owns letters (`operator + label` via
    ///    [`Runtime::cw_hint_push_key`](bitty_runtime::Runtime::cw_hint_push_key));
    ///    `Esc` cancels, the Leader re-arms, anything else disarms loudly
    ///    and falls through.
    /// 3. A bare Leader press arms a fresh session from the live engine.
    ///
    /// Returns `true` when the press is consumed (the caller redraws and
    /// returns); `false` lets normal dispatch run. Copy/search modals sit
    /// above: while either is active this tier stays out of the way.
    /// Composer modal routing is retired (E-CUT, CTX-0968): the plugin owns
    /// editing UX via overlay/capture; there is no Core session to route to.
    fn route_cw_modal(&mut self, key: &KeyEvent, keyref: &bitty_config::KeyRef) -> bool {
        if self.runtime.is_copy_mode() || self.runtime.is_search_mode() {
            return false;
        }
        let now_ms = self.chrome.leader_now_ms();
        match self.chrome.leader_state.poll(now_ms) {
            bitty_config::LeaderPoll::Expired => {
                self.disarm_hint_session();
                eprintln!("bitty: leader window expired — keys route to the shell");
                return false;
            }
            bitty_config::LeaderPoll::Idle | bitty_config::LeaderPoll::Armed => {}
        }
        if self.runtime.cw_hint_is_armed() {
            return self.route_hint_armed(key, keyref);
        }
        if self.chrome.leader.arms(*keyref) {
            if !self.chrome.hints_enabled {
                // CTX-0735 (#981): hints disabled — the Leader keeps its
                // normal owner (fail-open); the press is not consumed and no
                // session arms, so follow-up keys never route to hint code.
                return false;
            }
            if !key.repeat {
                self.arm_hint_session();
            }
            return true;
        }
        // E-CUT (CTX-0968): no Core composer session; clear a stale fallback
        // latch for observability. The next open retries the plugin.
        self.composer_core_fallback_latched = false;
        false
    }

    /// Disarms both halves of the hint interaction (Leader window plus live
    /// session, CTX-0723 #981). Idempotent; terminal truth untouched.
    fn disarm_hint_session(&mut self) {
        self.chrome.leader_state = bitty_config::LeaderState::Idle;
        self.runtime.cw_hint_disarm();
    }

    /// Arms a fresh hint session on a Leader press (CTX-0723 #981).
    ///
    /// Collects one batch against live terminal state through the single
    /// cross-panel engine and opens the Leader window. Empty batches and
    /// operator conflicts refuse loudly without arming (a dead session
    /// must never swallow keystrokes). Always consumes the Leader press.
    fn arm_hint_session(&mut self) {
        use bitty_runtime::HintScope;
        self.chrome.hint_generation = self.chrome.hint_generation.wrapping_add(1);
        let generation = self.chrome.hint_generation;
        let scope = self
            .runtime
            .focused_view()
            .map_or(HintScope::default(), |view| HintScope(view.0));
        let bound = self.bound_bare_keys();
        match self.runtime.cw_hint_arm(scope, generation, &bound) {
            Ok(0) => {
                self.disarm_hint_session();
                eprintln!("warning: leader armed an empty hint batch (no targets) — disarming");
            }
            Ok(count) => {
                let timeout_ms = self.chrome.leader.timeout_ms;
                self.chrome
                    .leader_state
                    .arm(self.chrome.leader_now_ms(), timeout_ms);
                eprintln!(
                    "bitty: hint armed ({count} labels, {timeout_ms}ms) — operator then label, Esc cancels"
                );
            }
            Err(conflict) => {
                self.disarm_hint_session();
                eprintln!(
                    "warning: leader refused — hint operators shadow bound keys {:?} — disarming",
                    conflict.keys
                );
            }
        }
    }

    /// Routes one press while the hint session is armed (CTX-0723 #981).
    ///
    /// `Esc` cancels, a repeated Leader press re-arms, bare letters feed
    /// the operator/label accumulator, and anything else disarms loudly and
    /// falls through so the press keeps its normal owner (fail-open).
    fn route_hint_armed(&mut self, key: &KeyEvent, keyref: &bitty_config::KeyRef) -> bool {
        use bitty_config::KeyName;
        if keyref.key == KeyName::Escape && !keyref.ctrl && !keyref.alt && !keyref.super_held {
            self.disarm_hint_session();
            eprintln!("bitty: hint session cancelled");
            return true;
        }
        if self.chrome.leader.arms(*keyref) {
            if !key.repeat {
                self.arm_hint_session();
            }
            return true;
        }
        match keyref.key {
            KeyName::Char(c) if !keyref.ctrl && !keyref.alt && !keyref.super_held => {
                if !key.repeat {
                    self.push_hint_letter(c);
                }
                true
            }
            _ => {
                eprintln!(
                    "warning: hint session ignoring non-letter press — disarming, keys route normally"
                );
                self.disarm_hint_session();
                false
            }
        }
    }

    /// Feeds one letter into the armed hint interaction and consumes the
    /// outcome loudly (CTX-0723 #981).
    fn push_hint_letter(&mut self, c: char) {
        use bitty_runtime::cw_present::HintKeyOutcome;
        match self.runtime.cw_hint_push_key(c) {
            HintKeyOutcome::NeedMore => {}
            HintKeyOutcome::Dispatched(outcome) => {
                self.chrome.leader_state = bitty_config::LeaderState::Idle;
                self.apply_hint_outcome(outcome);
            }
            HintKeyOutcome::Invalid { key } => {
                eprintln!(
                    "warning: hint session rejected '{key}' — retry the label within the leader window, or Esc to cancel"
                );
            }
        }
    }

    /// Applies one hint dispatch outcome at the app boundary (CTX-0723 #981).
    ///
    /// Fold verbs are complete (the live fold state already mutated);
    /// view-focus resolves through the live layout fail-closed. Jump
    /// reveals (unfolds) but scroll-to-target, panel-focus resolution, and
    /// target-byte clipboard extraction are loud follow-ups, each named so
    /// no outcome is ever silent.
    fn apply_hint_outcome(&mut self, outcome: bitty_runtime::DispatchOutcome) {
        use bitty_runtime::DispatchOutcome;
        match outcome {
            DispatchOutcome::FoldToggled { id, folded } => {
                eprintln!(
                    "bitty: hint -> {id} {}",
                    if folded { "folded" } else { "unfolded" }
                );
            }
            DispatchOutcome::Expanded { id } => {
                eprintln!("bitty: hint -> {id} unfolded");
            }
            DispatchOutcome::Collapsed { id } => {
                eprintln!("bitty: hint -> {id} folded");
            }
            DispatchOutcome::Jump { target } => {
                eprintln!(
                    "bitty: hint -> jumped to target {} (revealed; scroll-to-target follows in panel-scroll work)",
                    target.get()
                );
            }
            DispatchOutcome::FocusView { view } => {
                if self.runtime.set_focus(ViewId::new(view)) {
                    eprintln!("bitty: hint -> focused view {view}");
                } else {
                    eprintln!("warning: hint -> view {view} is not a live leaf — focus unchanged");
                }
            }
            DispatchOutcome::FocusPanel { panel } => {
                eprintln!(
                    "warning: hint -> panel {panel} focus needs the panel-host view map (OQ-051 placed-contract follow-up) — focus unchanged"
                );
            }
            DispatchOutcome::CopyRequested { target } => {
                eprintln!(
                    "warning: hint -> copy of target {} needs the target-byte extraction seam (follow-up) — clipboard untouched",
                    target.get()
                );
            }
            DispatchOutcome::LinkOpen { uri, target } => {
                eprintln!(
                    "bitty: hint -> link target {} selects {uri} (opening stays on the URL gate — click the link to authorize; keyboard-only open is a follow-up)",
                    target.get()
                );
            }
        }
    }

    /// Bare (modifier-free) single-char keys bound in the keymap table, for
    /// the hint operator-conflict gate (CTX-0723 #981).
    ///
    /// Operators are bare lowercase letters by construction; any keymap
    /// chord on the same bare letter would be shadowed while armed, so the
    /// gate refuses arming instead of hijacking the binding.
    fn bound_bare_keys(&self) -> Vec<char> {
        self.chrome
            .keymaps
            .iter()
            .filter_map(|entry| {
                if entry.chord.ctrl
                    || entry.chord.alt
                    || entry.chord.shift
                    || entry.chord.super_held
                {
                    return None;
                }
                match entry.chord.key {
                    bitty_config::KeyName::Char(c) => Some(c),
                    _ => None,
                }
            })
            .collect()
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
                track_app_modifiers(&mut self.chrome.app_mods, key);
                // Release ends press-to-release ownership; the key returns to
                // normal matching on its next press. Releases always route
                // (the runtime encodes no bytes for them).
                if key.state != PressState::Pressed {
                    if let Some(keyref) = key_ref_from_event(key, &self.chrome.app_mods) {
                        self.chrome.held.remove(&keyref.key);
                    }
                    return false;
                }
                if is_modifier_key(key) {
                    return false;
                }
                if let Some(keyref) = key_ref_from_event(key, &self.chrome.app_mods) {
                    // Still physically held from a consumed chord press: stay
                    // swallowed even when the mirror decayed (release cascade).
                    // No action re-runs; the PTY never sees the key.
                    // (CTX-0229 ownership sits above the modal arm: ownership
                    // is a physical invariant, and with no modal active the
                    // arms below are unreachable — byte-identical.)
                    if self.chrome.held.contains(&keyref.key) {
                        if let Some(win) = self.window.handle.as_ref() {
                            win.request_redraw();
                        }
                        return true;
                    }
                    let matched = match_chrome_keymap(&self.chrome.keymaps, keyref);
                    // CTX-0943: the transient input capture owns the
                    // keyboard while active — below the copy/search
                    // modals above, above the user keymap below. Bare
                    // `Esc` cancels (revoke + consume, never forwarded,
                    // so no captured event reaches the terminal);
                    // focus-move chords revoke and fall through so focus
                    // actually moves; every other press is queued and
                    // swallowed (no action runs, no PTY bytes).
                    if self.overlay_capture_active() {
                        use bitty_config::KeyName as OverlayKey;
                        if key.is_synthetic {
                            return false;
                        }
                        if keyref.key == OverlayKey::Escape
                            && !keyref.ctrl
                            && !keyref.alt
                            && !keyref.super_held
                        {
                            self.revoke_overlay_capture_with_reason("cancelled");
                            if let Some(win) = self.window.handle.as_ref() {
                                win.request_redraw();
                            }
                            return true;
                        }
                        match matched {
                            Some(action) if Self::is_overlay_focus_switch(action) => {
                                self.revoke_overlay_capture();
                            }
                            _ => {
                                let text = key
                                    .text
                                    .clone()
                                    .unwrap_or_else(|| super::terminal_app::overlay_key_text(key));
                                self.push_overlay_input("key", &text);
                                if let Some(win) = self.window.handle.as_ref() {
                                    win.request_redraw();
                                }
                                return true;
                            }
                        }
                        // Focus-switch falls through to normal dispatch
                        // below, which runs the action through
                        // `apply_chrome_action` (revoke is idempotent).
                    }
                    // W-144 (CTX-0937) parked copy containment (owner: W-01):
                    // copy-mode policy retired to the copy-mode plugin
                    // (copy-mode@7410a3e); while a modal is held every bound
                    // chord is captured without running so focus, splits,
                    // zoom, and paste never fire mid-copy. `Esc` routes to
                    // the runtime containment (never the paste/close
                    // emergency path while modal); unbound keys route to
                    // the runtime copy containment, which consumes them
                    // with no PTY bytes.
                    if self.runtime.is_copy_mode() {
                        use bitty_config::KeyName as CopyModeKey;
                        if keyref.key == CopyModeKey::Escape {
                            return false;
                        }
                        match matched {
                            Some(_) => {
                                if let Some(win) = self.window.handle.as_ref() {
                                    win.request_redraw();
                                }
                                return true;
                            }
                            None => return false,
                        }
                    }
                    // W-144 (CTX-0937) parked capture-dispatch (owner: W-01):
                    // search policy retired to the plugin; while a modal is
                    // held every bound chord is captured without running so
                    // focus, splits, zoom, and paste never fire mid-search.
                    // `Esc` routes to the runtime containment (never the
                    // paste/close emergency path while modal); unbound keys
                    // route to the runtime search containment, which consumes
                    // them with no PTY bytes.
                    if self.runtime.is_search_mode() {
                        use bitty_config::KeyName as SearchModeKey;
                        if keyref.key == SearchModeKey::Escape {
                            return false;
                        }
                        match matched {
                            Some(_) => {
                                if let Some(win) = self.window.handle.as_ref() {
                                    win.request_redraw();
                                }
                                return true;
                            }
                            None => return false,
                        }
                    }
                    let (priority, action) = {
                        // CTX-0723: Leader/hint present-path routing runs here
                        // — below the copy/search modals above, above the user
                        // keymap below. Modal-consumed presses intentionally
                        // skip the `held` set (the modal owns the keyboard,
                        // not the chord).
                        if self.route_cw_modal(key, &keyref) {
                            if let Some(win) = self.window.handle.as_ref() {
                                win.request_redraw();
                            }
                            return true;
                        }
                        self.resolve_dispatch(keyref, matched)
                    };
                    match priority {
                        DispatchPriority::Emergency => {
                            return self.handle_emergency_escape(key);
                        }
                        DispatchPriority::Modal => {
                            // Active modal captures the bound non-confirm
                            // chord: consumed, no action runs, no PTY bytes.
                            if let Some(win) = self.window.handle.as_ref() {
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
                            self.chrome.held.insert(keyref.key);
                            // Repeats of a bound chord stay owned by chrome
                            // (no action, no PTY bytes).
                            if !key.repeat {
                                self.apply_chrome_action(action);
                            }
                            if let Some(win) = self.window.handle.as_ref() {
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
                self.chrome.app_mods = AppModifiers {
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
                clear_app_modifiers_on_focus(&mut self.chrome.app_mods, *focused);
                // CTX-0229: a missed key release while unfocused must not
                // leave a stale ownership entry swallowing future typing.
                self.chrome.held.clear();
                // CTX-0943: losing window focus ends the transient capture
                // (contract focus-switch release). Idempotent no-op with no
                // capture; the event still routes so Runtime records focus.
                if !focused {
                    self.revoke_overlay_capture();
                }
                false
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::SpawnSpec;
    use crate::terminal_app::TerminalApp;
    use bitty_platform::{PlatformEvent, WindowId};
    use bitty_runtime::{CloseConfirmMode, Runtime, RuntimeConfig};
    // Only the POSIX-shell live-spawn test below uses this (`#[cfg(unix)]`);
    // without the gate the import is unused on Windows.
    #[cfg(unix)]
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
        // CTX-0257 DEC entry set: new/close/prev/next/last
        // (CTX-0766: new-workspace moved alt+n -> alt+t; alt+n is new panel).
        // CTX-0838 (#1441): alt+n is Hyprland-style `new_panel` (adaptive
        // axis), not fixed `new_split:right`.
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('n'), false, true, false)),
            Some(bitty_config::ChromeAction::NewPanel)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('t'), false, true, false)),
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
            Some(bitty_config::ChromeAction::ScrollPageDown)
        );
        assert_eq!(
            match_keymap(&maps, shell(KeyName::Char('i'), false, true, false)),
            Some(bitty_config::ChromeAction::ScrollPageUp)
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

    #[test]
    fn chrome_prompt_nav_and_select_output_headless() {
        // CTX-0952 end-to-end through the chrome arms (no window): full
        // OSC133 session traffic, then jump/select dispatch moves the
        // viewport and installs the output selection; empty marks warn and
        // touch nothing.
        use bitty_config::ChromeAction;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        // The shipped defaults bind all three verbs (single-owner audit in
        // bitty-config pins the count).
        for action in [
            ChromeAction::JumpToPromptPrev,
            ChromeAction::JumpToPromptNext,
            ChromeAction::SelectCommandOutput,
        ] {
            assert!(
                maps.iter().any(|m| m.action == action),
                "defaults must bind {}",
                action.canonical()
            );
        }
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.force_headless_clipboard();
        let h = rt.state().height();
        for i in 0..(h + 2) {
            rt.handle_pty_bytes(b"\x1b]133;A\x07prompt$ \x1b]133;B\x07");
            rt.handle_pty_bytes(format!("cmd{i:02}\r\n").as_bytes());
            rt.handle_pty_bytes(b"\x1b]133;C\x07");
            rt.handle_pty_bytes(format!("out{i:02}\r\n").as_bytes());
            rt.handle_pty_bytes(b"\x1b]133;D;0\x07");
        }
        rt.handle_pty_bytes(b"\x1b]133;A\x07prompt$ ");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        // Select covers exactly the last command's output line.
        app.apply_chrome_action(ChromeAction::SelectCommandOutput);
        let expected = format!("out{:02}", h + 1);
        assert_eq!(
            app.runtime.selection_text().as_deref(),
            Some(expected.as_str())
        );
        // Jump prev pages into history; jump next walks back toward live.
        app.apply_chrome_action(ChromeAction::JumpToPromptPrev);
        let vid = app.runtime.focused_view().expect("focused view");
        let up = app
            .runtime
            .layout()
            .find_leaf(vid)
            .expect("leaf")
            .scroll_offset();
        assert!(up > 0, "prev jump pages into history");
        app.apply_chrome_action(ChromeAction::JumpToPromptNext);
        let down = app
            .runtime
            .layout()
            .find_leaf(vid)
            .expect("leaf")
            .scroll_offset();
        assert!(down < up, "next jump walks back toward live");
        assert_eq!(app.runtime.pending_input_len(), 0, "no PTY bytes produced");
    }

    #[test]
    fn chrome_prompt_nav_empty_marks_warn_and_touch_nothing() {
        // CTX-0952 fail-closed dispatch: with no marks the arms warn and
        // leave the viewport, selection, and PTY input untouched.
        use bitty_config::ChromeAction;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.force_headless_clipboard();
        rt.handle_pty_bytes(b"plain output, no integration\r\n");
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        app.apply_chrome_action(ChromeAction::JumpToPromptPrev);
        app.apply_chrome_action(ChromeAction::JumpToPromptNext);
        app.apply_chrome_action(ChromeAction::SelectCommandOutput);
        let vid = app.runtime.focused_view().expect("focused view");
        assert_eq!(
            app.runtime
                .layout()
                .find_leaf(vid)
                .expect("leaf")
                .scroll_offset(),
            0
        );
        assert!(!app.runtime.has_selection());
        assert_eq!(app.runtime.pending_input_len(), 0);
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
        // Split focused pane right: 2 -> 3 leaves; focus follows the fresh
        // pane (CTX-0364).
        app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
        assert_eq!(app.runtime.leaf_count(), 3);
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(3)));
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
    fn chrome_zoomed_navigation_unzooms_and_moves_focus() {
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
        assert_eq!(app.runtime.leaf_count(), 2);
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));

        // Zoom pane 1: collapses to 1 leaf.
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 1);
        assert!(app.chrome.zoom.is_zoomed(&app.runtime));

        // Navigating with GotoSplit restores zoom immediately and moves focus.
        app.apply_chrome_action(ChromeAction::GotoSplit(SplitDir::Right));
        assert_eq!(app.runtime.leaf_count(), 2, "must unzoom on navigation");
        assert!(!app.chrome.zoom.is_zoomed(&app.runtime));
        assert_eq!(
            app.runtime.focused_view(),
            Some(ViewId::new(2)),
            "focus must move right"
        );

        // Zoom pane 2, then FocusPrev: unzooms and focuses pane 1.
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 1);
        app.apply_chrome_action(ChromeAction::FocusPrev);
        assert_eq!(app.runtime.leaf_count(), 2, "must unzoom on focus_prev");
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
    }

    #[test]
    fn chrome_toggle_zoom_on_non_owner_preserves_primary_owner() {
        // CTX-0359 review defect: `ToggleZoom` funnels through
        // `Runtime::set_layout`; treating an owner-excluding layout as a
        // close re-homed `primary_view` onto the zoomed non-owner pane and
        // blanked the original home for good. Pin the action path: zoom
        // round trips preserve the owner and only an explicit close
        // re-homes it.
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
        app.runtime.set_layout(two_pane_layout());
        assert_eq!(app.runtime.primary_view(), Some(ViewId::new(1)));
        // Zoom onto the non-owner pane v:2 and back: ownership is untouched.
        app.apply_chrome_action(ChromeAction::FocusId(2));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(2)));
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 1);
        assert_eq!(
            app.runtime.primary_view(),
            Some(ViewId::new(1)),
            "zoom on a non-owner must not re-home the primary"
        );
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 2);
        assert_eq!(
            app.runtime.primary_view(),
            Some(ViewId::new(1)),
            "zoom round trip must preserve the primary owner"
        );
        // An explicit close of the owner is the one path that re-homes it.
        app.apply_chrome_action(ChromeAction::FocusId(1));
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.primary_view(), Some(ViewId::new(2)));
    }

    #[test]
    fn zoom_backup_never_clobbers_layout_changed_while_zoomed() {
        // CTX-0481 (#762): the single-slot `zoom_backup` was restored
        // blindly. A layout mutation landing while zoomed (e.g. a ctl
        // `view split` drained between key events, CTX-0171) was silently
        // dropped when zoom toggled off because the backup predated it.
        // The backup is only valid while the runtime still holds the zoom
        // proxy; once the layout changed, zoom must not clobber the newer
        // tree.
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
        app.runtime.set_layout(two_pane_layout());
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 1, "zoom collapses to one leaf");
        // Out-of-band mutation while zoomed: a three-leaf tree installed by
        // a path that did not go through the zoom-aware action helpers.
        let newer = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            two_pane_layout(),
            LayoutNode::leaf(View::new(ViewId::new(3), 80, 24)),
        );
        app.runtime.set_layout(newer);
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(
            app.runtime.leaf_count(),
            3,
            "a stale zoom backup must not clobber the newer layout"
        );
    }

    /// CTX-0538 zoom test app: two-workspace runtime where workspace 1 holds
    /// the two-pane tree under test and workspace 2 is a fresh single leaf.
    fn workspace_zoom_test_app() -> TerminalApp {
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
        app.runtime.workspace_new().expect("second workspace");
        assert_eq!(app.runtime.active_workspace_index(), 1);
        // Back to workspace 1 and lay the split tree into it again (the new
        // workspace switch stashed the fresh leaf, so re-install the split).
        assert!(app.runtime.workspace_switch(0));
        app.runtime.set_layout(two_pane_layout());
        app
    }

    #[test]
    fn zoom_round_trip_is_scoped_to_its_workspace() {
        // CTX-0538 (LIVE-APP-002): zoom is per-workspace. Engage in A,
        // switch to B and back, then disengage: A's exact tree returns and
        // B never sees A's backup.
        use bitty_config::ChromeAction;
        let mut app = workspace_zoom_test_app();
        assert_eq!(app.runtime.leaf_count(), 2);
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 1, "A zoom collapses to one leaf");

        // Switch to B: the live layout is B's own (single leaf), and the
        // zoom state must not leak into it. Disengaging in B is a clean
        // no-op (B holds no zoom), never a comparison against A's backup.
        assert!(app.runtime.workspace_switch(1));
        assert_eq!(
            app.runtime.leaf_count(),
            1,
            "B keeps its own single-leaf layout"
        );
        assert!(!app.chrome.zoom.restore_for_mutation(&mut app.runtime));
        assert_eq!(
            app.runtime.leaf_count(),
            1,
            "restore in B must not touch B's layout"
        );

        // Back to A: the zoom is still engaged there and disengaging
        // restores A's exact tree.
        assert!(app.runtime.workspace_switch(0));
        assert_eq!(app.runtime.leaf_count(), 1, "A is still zoomed");
        assert!(app.chrome.zoom.restore_for_mutation(&mut app.runtime));
        assert_eq!(
            app.runtime.leaf_count(),
            2,
            "disengaging in A restores A's exact two-leaf tree"
        );
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
    }

    #[test]
    fn zoom_toggle_in_another_workspace_never_consumes_a_backup() {
        // CTX-0538: a toggle in workspace B must engage B's zoom, never
        // disengage A's leftover entry (the pre-fix single slot).
        use bitty_config::ChromeAction;
        let mut app = workspace_zoom_test_app();
        // Zoom A onto v:2 (one-leaf proxy on v:2).
        app.apply_chrome_action(ChromeAction::FocusId(2));
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 1);
        // Switch to B and toggle: B engages its own zoom (single leaf is
        // idempotently zoomed), and A's entry is untouched.
        assert!(app.runtime.workspace_switch(1));
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 1, "B toggles its own zoom");
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert_eq!(app.runtime.leaf_count(), 1, "B disengages its own zoom");
        // Back in A the zoom is still live and restores the real tree.
        assert!(app.runtime.workspace_switch(0));
        assert_eq!(app.runtime.leaf_count(), 1, "A is still zoomed");
        assert!(app.chrome.zoom.restore_for_mutation(&mut app.runtime));
        assert_eq!(
            app.runtime.leaf_count(),
            2,
            "A's backup must survive B's toggles"
        );
    }

    // CTX-0370 close-confirm app wiring: the close_view chord arms a bounded
    // confirmation for a busy pane, repeat confirms, Esc cancels, and the
    // default `when_busy` mode keeps the pre-0370 single-gesture close for
    // idle/session-less panes.
    // -----------------------------------------------------------------------

    fn close_confirm_test_app(mode: CloseConfirmMode) -> TerminalApp {
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let rt = Runtime::new(RuntimeConfig {
            close_confirm: mode,
            ..RuntimeConfig::default()
        })
        .expect("must build");
        TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        )
    }

    #[test]
    fn chrome_close_view_idle_pane_closes_without_gate() {
        // Default `when_busy`: a session-less pane keeps the pre-0370
        // single-gesture close (zero change for existing users).
        use bitty_config::ChromeAction;
        let mut app = close_confirm_test_app(CloseConfirmMode::WhenBusy);
        app.runtime.set_layout(two_pane_layout());
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 1);
        assert!(!app.runtime.has_pending_close_confirm());
    }

    #[test]
    fn chrome_close_view_always_arms_repeat_closes_and_esc_cancels() {
        // `always`: the first gesture arms without closing; Esc cancels and
        // leaves the layout untouched; repeating the chord confirms.
        use bitty_config::ChromeAction;
        let mut app = close_confirm_test_app(CloseConfirmMode::Always);
        app.runtime.set_layout(two_pane_layout());
        app.runtime.set_focus(ViewId::new(2));
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 2, "pending must not close");
        assert_eq!(app.runtime.pending_close_view(), Some(ViewId::new(2)));
        assert!(app.modal_capture_active());
        assert!(app.runtime.close_confirm_banner_text().is_some());
        // Esc cancels through the emergency dispatch (never reaches the PTY).
        assert!(drive_chrome(&mut app, esc_press()));
        assert!(!app.runtime.has_pending_close_confirm());
        assert_eq!(app.runtime.leaf_count(), 2, "cancel keeps the pane");
        // Re-arm, then repeat confirms the close.
        app.apply_chrome_action(ChromeAction::CloseView);
        assert!(app.runtime.has_pending_close_confirm());
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 1);
        assert!(!app.runtime.has_pending_close_confirm());
    }

    #[test]
    #[cfg(unix)]
    fn chrome_close_view_busy_pane_needs_repeat_confirm_and_kills() {
        // The user-report shape (m0466): a pane with a running foreground
        // job never closes on one gesture under the default `when_busy`;
        // repeat confirms and tears the session down.
        use bitty_config::ChromeAction;
        require_pty!();
        let mut app = close_confirm_test_app(CloseConfirmMode::WhenBusy);
        app.runtime.set_layout(two_pane_layout());
        let view = ViewId::new(2);
        app.runtime
            .spawn_shell_for_view(view, "/bin/sh", &[], 40, 12)
            .expect("pane shell must spawn headless");
        app.runtime.set_focus(view);
        app.runtime.write_input(b"sleep 30\n");
        // Poll until the foreground job is visible (bounded).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if app.runtime.pane_foreground_job(&view).is_some() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "job never started");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // First close arms; the job and its session stay alive.
        app.apply_chrome_action(ChromeAction::CloseView);
        assert!(app.runtime.has_pending_close_confirm());
        assert_eq!(app.runtime.leaf_count(), 2, "busy pane must not close yet");
        assert!(app.runtime.has_pane_session(&view));
        let banner = app
            .runtime
            .close_confirm_banner_text()
            .expect("busy arm must be visible");
        assert!(
            banner.contains("close again to confirm"),
            "names the confirm gesture: {banner}"
        );
        assert!(banner.contains("Esc cancels"), "names cancel: {banner}");
        // Repeat confirms: pane closes and its shell is torn down.
        app.apply_chrome_action(ChromeAction::CloseView);
        assert_eq!(app.runtime.leaf_count(), 1);
        assert!(!app.runtime.has_pane_session(&view));
        assert!(!app.runtime.has_pending_close_confirm());
    }

    #[test]
    fn chrome_new_split_focuses_new_pane() {
        // CTX-0364: the keymap `new_split` path (Shift+Alt+L) must focus the
        // fresh pane immediately, matching the ctl path and kitty/ghostty.
        use bitty_config::{ChromeAction, SplitDir};
        let mut app = workspace_test_app();
        app.runtime.set_layout(two_pane_layout());
        assert_eq!(
            app.runtime.focused_view(),
            Some(ViewId::new(1)),
            "seed focus on v:1"
        );
        app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
        assert_eq!(app.runtime.leaf_count(), 3);
        assert_eq!(
            app.runtime.focused_view(),
            Some(ViewId::new(3)),
            "new_split must focus the fresh pane v:3"
        );
    }

    #[test]
    fn chrome_new_panel_wide_splits_side_by_side_new_second_focus_follows() {
        // CTX-0838 (#1441): Mod+N `new_panel` uses Hyprland-dwindle axis.
        // Default headless container is wide, so the split is side-by-side
        // (Horizontal), the new panel goes second (right), focus follows it.
        use bitty_config::ChromeAction;
        let mut app = workspace_test_app();
        let focused_before = app.runtime.focused_view().expect("seed focus");
        app.apply_chrome_action(ChromeAction::NewPanel);
        assert_eq!(app.runtime.leaf_count(), 2);
        let focused_after = app.runtime.focused_view().expect("focus follows");
        assert_ne!(
            focused_before, focused_after,
            "focus must move to the fresh pane"
        );
        match app.runtime.layout() {
            LayoutNode::Split {
                axis,
                first,
                second,
                ..
            } => {
                assert_eq!(*axis, SplitAxis::Horizontal, "wide must split side-by-side");
                assert!(
                    matches!(first.as_ref(), LayoutNode::Leaf(v) if v.id() == focused_before),
                    "focused leaf stays first (left)"
                );
                assert!(
                    matches!(second.as_ref(), LayoutNode::Leaf(v) if v.id() == focused_after),
                    "fresh panel goes second (right)"
                );
            }
            other => panic!("single leaf new_panel must split, got {other:?}"),
        }
    }

    #[test]
    fn chrome_new_panel_tall_stacks_new_below_focus_follows() {
        // CTX-0838 (#1441): a tall focused leaf stacks (Vertical), new panel
        // goes second (below), focus follows it.
        use bitty_config::ChromeAction;
        let mut app = workspace_test_app();
        app.runtime
            .set_container(bitty_runtime::UiRect::new(0, 0, 24, 80));
        let focused_before = app.runtime.focused_view().expect("seed focus");
        app.apply_chrome_action(ChromeAction::NewPanel);
        assert_eq!(app.runtime.leaf_count(), 2);
        let focused_after = app.runtime.focused_view().expect("focus follows");
        assert_ne!(
            focused_before, focused_after,
            "focus must move to the fresh pane"
        );
        match app.runtime.layout() {
            LayoutNode::Split {
                axis,
                first,
                second,
                ..
            } => {
                assert_eq!(*axis, SplitAxis::Vertical, "tall must stack");
                assert!(
                    matches!(first.as_ref(), LayoutNode::Leaf(v) if v.id() == focused_before),
                    "focused leaf stays first (top)"
                );
                assert!(
                    matches!(second.as_ref(), LayoutNode::Leaf(v) if v.id() == focused_after),
                    "fresh panel goes second (below)"
                );
            }
            other => panic!("single leaf new_panel must split, got {other:?}"),
        }
    }

    #[test]
    fn chrome_new_panel_spirals_clockwise_across_splits() {
        use bitty_config::ChromeAction;
        let mut app = workspace_test_app();
        app.runtime
            .set_container(bitty_runtime::UiRect::new(0, 0, 160, 40));

        let v1 = app.runtime.focused_view().expect("seed focus");

        // Split 1 (1 -> 2): Step 0 -> Horizontal, Right (place_new_first = false).
        app.apply_chrome_action(ChromeAction::NewPanel);
        assert_eq!(app.runtime.leaf_count(), 2);
        let v2 = app.runtime.focused_view().expect("v2 focus");
        assert_ne!(v1, v2);

        // Split 2 (2 -> 3): Step 1 -> Vertical, Bottom (place_new_first = false).
        app.apply_chrome_action(ChromeAction::NewPanel);
        assert_eq!(app.runtime.leaf_count(), 3);
        let v3 = app.runtime.focused_view().expect("v3 focus");
        assert_ne!(v2, v3);

        // Split 3 (3 -> 4): Step 2 -> Horizontal, Left (place_new_first = true).
        app.apply_chrome_action(ChromeAction::NewPanel);
        assert_eq!(app.runtime.leaf_count(), 4);
        let v4 = app.runtime.focused_view().expect("v4 focus");
        assert_ne!(v3, v4);

        // Split 4 (4 -> 5): Step 3 -> Vertical, Top (place_new_first = true).
        app.apply_chrome_action(ChromeAction::NewPanel);
        assert_eq!(app.runtime.leaf_count(), 5);
        let v5 = app.runtime.focused_view().expect("v5 focus");
        assert_ne!(v4, v5);

        // Next split wraps back to Step 0 (Right).
        assert!(
            !app.runtime.panel_split_place_new_first(),
            "5 -> 6 wraps back to Step 0 (place_new_first = false)"
        );
    }

    #[test]
    fn chrome_floating_toggle_default_mod_a_resolves() {
        // CTX-0962 (#1695): Mod+a toggles floating. Both alt+a/alt+v were
        // free; fish reserves alt+v for `$EDITOR` so Mod+v stays shell.
        use bitty_config::{ChromeAction, EffectiveConfig, KeyName, KeyRef, ModKey};
        let alt_maps =
            bitty_config::resolve_keymaps(&EffectiveConfig::default()).expect("defaults");
        let alt = |key: KeyName, ctrl: bool, alt: bool, shift: bool| KeyRef {
            key,
            ctrl,
            alt,
            shift,
            super_held: false,
        };
        assert_eq!(
            bitty_config::match_keymap(&alt_maps, alt(KeyName::Char('a'), false, true, false)),
            Some(ChromeAction::ToggleFloating),
            "alt+a toggles floating"
        );
        assert_eq!(
            bitty_config::match_keymap(&alt_maps, alt(KeyName::Char('a'), false, false, false)),
            None,
            "bare a stays shell"
        );
        assert_eq!(
            bitty_config::match_keymap(&alt_maps, alt(KeyName::Char('v'), false, true, false)),
            None,
            "alt+v stays shell (fish $EDITOR conflict)"
        );
        // Super flip follows the Mod slot.
        let super_maps = bitty_config::resolve_keymaps(&EffectiveConfig {
            mod_key: ModKey::Super,
            ..Default::default()
        })
        .expect("super defaults");
        assert_eq!(
            bitty_config::match_keymap(
                &super_maps,
                KeyRef {
                    key: KeyName::Char('a'),
                    ctrl: false,
                    alt: false,
                    shift: false,
                    super_held: true,
                }
            ),
            Some(ChromeAction::ToggleFloating),
            "super+a toggles floating under super mod"
        );
        assert_eq!(
            bitty_config::match_keymap(&super_maps, alt(KeyName::Char('a'), false, true, false)),
            None,
            "alt+a unbound under super mod"
        );
    }

    #[test]
    fn chrome_toggle_floating_flips_focused_leaf_and_roundtrips() {
        // CTX-0962 (#1695): the apply path flips the focused leaf
        // Tiled <-> Floating with no layout surgery (leaf count and focus
        // stable) and round-trips back to the exact prior mode.
        use bitty_config::ChromeAction;
        let mut app = workspace_test_app();
        app.runtime.set_layout(two_pane_layout());
        let focused = app.runtime.focused_view().expect("seed focus");
        assert_eq!(
            app.runtime
                .layout()
                .find_leaf(focused)
                .expect("focused leaf")
                .presentation(),
            PresentationMode::Tiled
        );
        app.apply_chrome_action(ChromeAction::ToggleFloating);
        assert_eq!(app.runtime.leaf_count(), 2, "toggle keeps leaf count");
        assert_eq!(
            app.runtime.focused_view(),
            Some(focused),
            "toggle keeps focus"
        );
        assert_eq!(
            app.runtime
                .layout()
                .find_leaf(focused)
                .expect("focused leaf")
                .presentation(),
            PresentationMode::Floating,
            "first toggle floats"
        );
        app.apply_chrome_action(ChromeAction::ToggleFloating);
        assert_eq!(app.runtime.leaf_count(), 2, "untoggle keeps leaf count");
        assert_eq!(
            app.runtime.focused_view(),
            Some(focused),
            "untoggle keeps focus"
        );
        assert_eq!(
            app.runtime
                .layout()
                .find_leaf(focused)
                .expect("focused leaf")
                .presentation(),
            PresentationMode::Tiled,
            "second toggle restores tiled"
        );
    }

    #[test]
    fn chrome_toggle_floating_rejects_zoomed_fullscreen_without_state_change() {
        // CodeRabbit PR #1704 (CTX-0962 follow-up): the leaf validates
        // before zoom restores. A zoomed Fullscreen/Scratchpad leaf must
        // reject with zero state change (zoom backup intact, layout and
        // focus untouched). A zoomed Tiled leaf still toggles: restore
        // lands on the real tree, then the flip applies.
        use bitty_config::ChromeAction;
        for mode in [PresentationMode::Fullscreen, PresentationMode::Scratchpad] {
            let mut app = workspace_test_app();
            app.runtime.set_layout(two_pane_layout());
            let focused = app.runtime.focused_view().expect("seed focus");
            let mut seeded = app.runtime.layout().clone();
            let leaf = seeded.find_leaf_mut(focused).expect("focused leaf");
            assert!(PresentationMode::request_transition(leaf, mode));
            app.runtime.set_layout(seeded);
            app.apply_chrome_action(ChromeAction::ToggleZoom);
            assert!(app.chrome.zoom.is_zoomed(&app.runtime), "zoom engaged");
            let before = format!("{:?}", app.runtime.layout());
            app.apply_chrome_action(ChromeAction::ToggleFloating);
            assert_eq!(
                format!("{:?}", app.runtime.layout()),
                before,
                "rejected toggle on zoomed {mode} must not touch the layout"
            );
            assert!(
                app.chrome.zoom.is_zoomed(&app.runtime),
                "rejected toggle on zoomed {mode} must keep the zoom backup"
            );
            assert_eq!(
                app.runtime.focused_view(),
                Some(focused),
                "rejected toggle keeps focus"
            );
        }
        let mut app = workspace_test_app();
        app.runtime.set_layout(two_pane_layout());
        let focused = app.runtime.focused_view().expect("seed focus");
        app.apply_chrome_action(ChromeAction::ToggleZoom);
        assert!(app.chrome.zoom.is_zoomed(&app.runtime), "zoom engaged");
        app.apply_chrome_action(ChromeAction::ToggleFloating);
        assert!(
            !app.chrome.zoom.is_zoomed(&app.runtime),
            "valid toggle consumes the zoom restore"
        );
        assert_eq!(app.runtime.leaf_count(), 2, "toggle lands on the real tree");
        assert_eq!(
            app.runtime
                .layout()
                .find_leaf(focused)
                .expect("focused leaf")
                .presentation(),
            PresentationMode::Floating,
            "zoomed tiled leaf floats"
        );
    }

    #[test]
    fn chrome_toggle_floating_user_override_replaces_default() {
        // CTX-0962 (#1695): Lua-overridable through the existing
        // `context + chord` merge rule — a user `keymaps` entry with the
        // same chord replaces the shipped Mod+a, and the action rebinds
        // elsewhere by explicit chord.
        use bitty_config::{ChromeAction, EffectiveConfig, KeyName, KeyRef, KeymapEntry};
        let overridden = EffectiveConfig {
            keymaps: vec![KeymapEntry {
                chord: "alt+a".into(),
                action: "focus_next".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        let maps = bitty_config::resolve_keymaps(&overridden).expect("resolves");
        assert_eq!(
            bitty_config::match_keymap(
                &maps,
                KeyRef {
                    key: KeyName::Char('a'),
                    ctrl: false,
                    alt: true,
                    shift: false,
                    super_held: false,
                }
            ),
            Some(ChromeAction::FocusNext),
            "user alt+a replaces the shipped floating toggle"
        );
        let rebound = EffectiveConfig {
            keymaps: vec![KeymapEntry {
                chord: "alt+q".into(),
                action: "toggle_floating".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        let maps = bitty_config::resolve_keymaps(&rebound).expect("resolves");
        assert_eq!(
            bitty_config::match_keymap(
                &maps,
                KeyRef {
                    key: KeyName::Char('q'),
                    ctrl: false,
                    alt: true,
                    shift: false,
                    super_held: false,
                }
            ),
            Some(ChromeAction::ToggleFloating),
            "toggle_floating rebinds by explicit chord"
        );
    }

    #[test]
    fn chrome_new_panel_bisect_largest_splits_largest_not_focused() {
        // CTX-0964 (#1698, DEC-0099): `BisectLargest` `new_panel` splits
        // `panel_bisect_target` (largest-area leaf) with its raw axis, not
        // the focused leaf. Focus parks on the small 20-wide leaf; the
        // 60-wide largest leaf must split right into 30/30 and focus must
        // follow the fresh pane. Heights stay relative (the workspaceline
        // band may reserve a row), so only widths and adjacency are pinned.
        use bitty_config::ChromeAction;
        let mut app = workspace_test_app();
        let small = ViewId::new(100);
        let largest = ViewId::new(101);
        let seed = app.runtime.focused_view().expect("seed focus");
        let probe = app
            .runtime
            .layout()
            .find_leaf(seed)
            .cloned()
            .expect("seed leaf");
        app.runtime.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.25,
            LayoutNode::leaf(View::new(
                small,
                usize::from(probe.cols()),
                usize::from(probe.rows()),
            )),
            LayoutNode::leaf(View::new(
                largest,
                usize::from(probe.cols()),
                usize::from(probe.rows()),
            )),
        ));
        assert!(app.runtime.set_focus(small));
        app.runtime
            .set_panel_layout_mode(PanelLayoutMode::BisectLargest);
        assert_eq!(app.runtime.panel_bisect_target(), Some(largest));
        app.apply_chrome_action(ChromeAction::NewPanel);
        assert_eq!(app.runtime.leaf_count(), 3);
        let fresh = app.runtime.focused_view().expect("focus follows fresh");
        assert_ne!(fresh, small, "fresh pane takes focus");
        assert_ne!(fresh, largest, "fresh pane is new");
        let allocs = app.runtime.layout_allocations();
        let rect_of = |id: ViewId| {
            allocs
                .iter()
                .find(|(leaf, _)| *leaf == id)
                .map(|(_, r)| *r)
                .expect("allocation present")
        };
        let small_rect = rect_of(small);
        let largest_rect = rect_of(largest);
        let fresh_rect = rect_of(fresh);
        assert_eq!(small_rect.width, 20, "small leaf stays whole");
        assert_eq!(largest_rect.width, 30, "largest splits right");
        assert_eq!(fresh_rect.width, 30, "fresh pane is the other half");
        assert_eq!(small_rect.height, largest_rect.height);
        assert_eq!(largest_rect.height, fresh_rect.height);
        assert_eq!(largest_rect.y, fresh_rect.y);
        assert_eq!(largest_rect.x + largest_rect.width, fresh_rect.x);
    }

    #[test]
    fn chrome_new_split_explicit_stays_on_focused_under_bisect_mode() {
        // CTX-0964 (#1698): explicit `new_split:<dir>` keeps focused-leaf
        // semantics under every mode — only adaptive `NewPanel` follows the
        // bisect target. With `BisectLargest` on and focus on the small
        // leaf, `new_split:right` must still split it (10/10), leaving the
        // 60-wide largest leaf whole.
        use bitty_config::{ChromeAction, SplitDir};
        let mut app = workspace_test_app();
        let small = ViewId::new(100);
        let largest = ViewId::new(101);
        let seed = app.runtime.focused_view().expect("seed focus");
        let probe = app
            .runtime
            .layout()
            .find_leaf(seed)
            .cloned()
            .expect("seed leaf");
        app.runtime.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.25,
            LayoutNode::leaf(View::new(
                small,
                usize::from(probe.cols()),
                usize::from(probe.rows()),
            )),
            LayoutNode::leaf(View::new(
                largest,
                usize::from(probe.cols()),
                usize::from(probe.rows()),
            )),
        ));
        assert!(app.runtime.set_focus(small));
        app.runtime
            .set_panel_layout_mode(PanelLayoutMode::BisectLargest);
        app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
        assert_eq!(app.runtime.leaf_count(), 3);
        let fresh = app.runtime.focused_view().expect("focus follows fresh");
        assert_ne!(fresh, small, "fresh pane takes focus");
        assert_ne!(fresh, largest, "fresh pane is new");
        let allocs = app.runtime.layout_allocations();
        let rect_of = |id: ViewId| {
            allocs
                .iter()
                .find(|(leaf, _)| *leaf == id)
                .map(|(_, r)| *r)
                .expect("allocation present")
        };
        assert_eq!(rect_of(small).width, 10, "focused leaf splits");
        assert_eq!(rect_of(fresh).width, 10, "fresh pane is the other half");
        assert_eq!(rect_of(largest).width, 60, "largest leaf untouched");
    }

    #[test]
    #[cfg(unix)]
    fn chrome_new_split_after_workspace_new_does_not_clobber_other_shell() {
        // CTX-0378 / Issue #627: the keymap `new_split` path allocated from
        // the active layout's `max + 1`, then spawned a shell for that id.
        // After `WorkspaceNew` gives ws2 its own leaf + replayed shell, a
        // split in ws1 must allocate globally (v:4) and must never re-spawn
        // over ws2's live session (which would kill its shell).
        use bitty_config::{ChromeAction, SplitDir};
        bitty_test_support::require_pty!();
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.spawn_shell_with_args("/bin/sh", &[])
            .expect("primary shell must attach headless");
        let spec = SpawnSpec {
            program: Some("/bin/sh".to_string()),
            program_args: Vec::new(),
            shell_env: None,
            config_shell: None,
        };
        let mut app = TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            spec,
        );
        // ws1: split -> v:2 with a shell of its own.
        app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
        let ws1_pid = app
            .runtime
            .pane_pid(&ViewId::new(2))
            .expect("ws1 split shell must spawn");
        // ws2: fresh workspace owns v:3 and the replayed shell.
        app.apply_chrome_action(ChromeAction::WorkspaceNew);
        let ws2_view = app.runtime.focused_view().expect("ws2 focus");
        assert_eq!(ws2_view, ViewId::new(3));
        let ws2_pid = app.runtime.pane_pid(&ws2_view).expect("ws2 shell");
        // Back to ws1 and split: the global allocator must give v:4.
        app.apply_chrome_action(ChromeAction::WorkspaceFocus(1));
        app.apply_chrome_action(ChromeAction::NewSplit(SplitDir::Right));
        assert_eq!(
            app.runtime.focused_view(),
            Some(ViewId::new(4)),
            "keymap split must allocate the global next id, not ws1's v:3"
        );
        assert_eq!(
            app.runtime.pane_pid(&ws2_view),
            Some(ws2_pid),
            "ws2's shell must survive the ws1 split (no session replacement)"
        );
        assert_eq!(
            app.runtime.pane_pid(&ViewId::new(2)),
            Some(ws1_pid),
            "ws1's earlier split shell must stay live"
        );
        let new_pid = app
            .runtime
            .pane_pid(&ViewId::new(4))
            .expect("new split shell must spawn");
        assert_ne!(new_pid, ws2_pid, "distinct shells for distinct views");
        assert_ne!(new_pid, ws1_pid, "distinct shells for distinct views");
    }

    #[test]
    fn chrome_workspace_new_focuses_fresh_view() {
        // CTX-0364: new tab (workspace) focuses its fresh view immediately.
        use bitty_config::ChromeAction;
        let mut app = workspace_test_app();
        app.apply_chrome_action(ChromeAction::WorkspaceNew);
        assert_eq!(app.runtime.active_workspace_index(), 1);
        assert_eq!(
            app.runtime.focused_view(),
            Some(ViewId::new(2)),
            "workspace_new must focus the fresh workspace view"
        );
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

    // Live-spawn helper: the live-close test below spawns /bin/sh. The test
    // calls `require_pty!()` first and carries `#[cfg(unix)]` (POSIX program;
    // Windows ConPTY coverage lives in bitty-pty/tests/spawn_windows.rs),
    // so this helper stays compiled on all platforms.
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
        // Beyond-count N clamps to the last workspace (issue #1365); here
        // the clamp target is already active, so state holds.
        app.apply_chrome_action(ChromeAction::WorkspaceFocus(9));
        assert_eq!(app.runtime.active_workspace_index(), 1);
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1 2:ws2* (2)");
        // Idle close is immediate (never pends, never kills).
        app.apply_chrome_action(ChromeAction::WorkspaceClose);
        assert!(!app.runtime.has_pending_ws_close());
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1* (1)");
    }

    #[test]
    fn chrome_workspace_focus_clamps_to_max_and_zero_fails_closed() {
        // Issue #1365: Alt+N through the chrome arms jumps exactly within
        // range, clamps beyond-count N to the last workspace, and refuses
        // zero with state untouched — headless (no window).
        use bitty_config::ChromeAction;
        let mut app = workspace_test_app();
        app.apply_chrome_action(ChromeAction::WorkspaceNew);
        app.apply_chrome_action(ChromeAction::WorkspaceNew);
        app.apply_chrome_action(ChromeAction::WorkspaceFocus(1));
        assert_eq!(app.runtime.active_workspace_index(), 0);
        // Exact jump.
        app.apply_chrome_action(ChromeAction::WorkspaceFocus(2));
        assert_eq!(app.runtime.active_workspace_index(), 1);
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1 2:ws2* 3:ws3 (3)");
        // Clamp: Alt+6 / Alt+9 with 3 workspaces go to workspace 3.
        app.apply_chrome_action(ChromeAction::WorkspaceFocus(6));
        assert_eq!(app.runtime.active_workspace_index(), 2);
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1 2:ws2 3:ws3* (3)");
        app.apply_chrome_action(ChromeAction::WorkspaceFocus(1));
        app.apply_chrome_action(ChromeAction::WorkspaceFocus(9));
        assert_eq!(app.runtime.active_workspace_index(), 2);
        // Zero fails closed with state untouched.
        app.apply_chrome_action(ChromeAction::WorkspaceFocus(0));
        assert_eq!(app.runtime.active_workspace_index(), 2);
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1 2:ws2 3:ws3* (3)");
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
        // Auto-creates missing target workspace N (CTX-0945).
        app.apply_chrome_action(ChromeAction::WorkspaceMove(9));
        assert_eq!(app.runtime.workspace_count(), 3);
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1 2:ws2* 9:ws9 (3)");
        assert_eq!(app.runtime.layout().leaf_count(), 1);
    }

    // Live-spawn: runs a real POSIX shell (`/bin/sh` has no Windows
    // equivalent). `#[cfg(unix)]` keeps it off Windows CI; `require_pty!()`
    // keeps the force-no-PTY simulation path. ConPTY coverage lives in
    // bitty-pty/tests/spawn_windows.rs (CTX-0268); porting this test to a
    // platform-neutral spawn is deferred follow-up.
    #[test]
    #[cfg(unix)]
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
            resolve_priority_for(esc, Some(A::CloseView), true, false, false, false, false,),
            (DispatchPriority::Emergency, None)
        );
        assert_eq!(
            resolve_priority_for(esc, None, false, true, false, false, false,),
            (DispatchPriority::Emergency, None)
        );
        // No modal: bound -> User, unbound -> Terminal (pre-0275 behavior).
        assert_eq!(
            resolve_priority_for(esc, Some(A::CloseView), false, false, false, false, false,),
            (DispatchPriority::User, Some(A::CloseView))
        );
        assert_eq!(
            resolve_priority_for(esc, None, false, false, false, false, false,),
            (DispatchPriority::Terminal, None)
        );
        // Unbound keys fall through even with a modal up (the dialog
        // captures commands, not typing).
        assert_eq!(
            resolve_priority_for(bare_x, None, true, false, false, false, false,),
            (DispatchPriority::Terminal, None)
        );
        assert_eq!(
            resolve_priority_for(bare_x, None, true, true, false, false, false,),
            (DispatchPriority::Terminal, None)
        );
        // A close gate captures a bound non-confirm chord (a pending paste
        // alone never captures — issue #1336 — so the same chord behind
        // only a paste banner dispatches as User).
        assert_eq!(
            resolve_priority_for(
                alt_h,
                Some(A::GotoSplit(SplitDir::Left)),
                true,
                false,
                false,
                false,
                false,
            ),
            (DispatchPriority::User, Some(A::GotoSplit(SplitDir::Left))),
            "paste pending alone: bound chords route normally"
        );
        assert_eq!(
            resolve_priority_for(
                alt_h,
                Some(A::GotoSplit(SplitDir::Left)),
                false,
                true,
                false,
                false,
                false,
            ),
            (DispatchPriority::Modal, None)
        );
        // ...but each modal's own repeat-confirm still dispatches as User
        // (the paste-again confirm needs no exemption: nothing captures
        // behind a paste banner, so it takes the normal User path —
        // issue #1336)...
        assert_eq!(
            resolve_priority_for(
                paste_chord,
                Some(A::PasteFromClipboard),
                true,
                false,
                false,
                false,
                false,
            ),
            (DispatchPriority::User, Some(A::PasteFromClipboard))
        );
        assert_eq!(
            resolve_priority_for(
                alt_w,
                Some(A::WorkspaceClose),
                false,
                true,
                false,
                false,
                false,
            ),
            (DispatchPriority::User, Some(A::WorkspaceClose))
        );
        // ...and crossed gestures stay captured under a capturing modal (a
        // close chord never confirms a paste, a paste chord never confirms
        // a close). With only a paste pending there is no capture: the
        // close chord arms normally (issue #1336).
        assert_eq!(
            resolve_priority_for(
                alt_w,
                Some(A::WorkspaceClose),
                true,
                false,
                false,
                false,
                false,
            ),
            (DispatchPriority::User, Some(A::WorkspaceClose)),
            "paste pending alone: close chord arms normally"
        );
        assert_eq!(
            resolve_priority_for(
                paste_chord,
                Some(A::PasteFromClipboard),
                false,
                true,
                false,
                false,
                false,
            ),
            (DispatchPriority::Modal, None)
        );
        // CTX-0370: a close-confirm arm captures bound chords; the close
        // chord confirms only when the arm targets the focused view.
        assert_eq!(
            resolve_priority_for(
                alt_h,
                Some(A::GotoSplit(SplitDir::Left)),
                false,
                false,
                true,
                false,
                false,
            ),
            (DispatchPriority::Modal, None)
        );
        assert_eq!(
            resolve_priority_for(alt_w, Some(A::CloseView), false, false, true, true, false,),
            (DispatchPriority::User, Some(A::CloseView)),
            "arm on the focused view: repeat close confirms"
        );
        assert_eq!(
            resolve_priority_for(alt_w, Some(A::CloseView), false, false, true, false, false,),
            (DispatchPriority::Modal, None),
            "window arm (or another view): the close chord is captured"
        );
        assert_eq!(
            resolve_priority_for(esc, None, false, false, true, true, false),
            (DispatchPriority::Emergency, None),
            "Esc cancels a close-confirm arm"
        );
        // CTX-0482: the panel-overlay modal captures bound chords...
        assert_eq!(
            resolve_priority_for(
                alt_h,
                Some(A::GotoSplit(SplitDir::Left)),
                false,
                false,
                false,
                false,
                true
            ),
            (DispatchPriority::Modal, None),
            "a panel modal captures bound non-confirm chords"
        );
        // ...but `Esc` is the panel's own dismissal path: it routes to the
        // runtime (Terminal), never the confirm-cancel emergency arm, and a
        // remapped `Esc` action must not run behind the modal.
        assert_eq!(
            resolve_priority_for(esc, Some(A::CloseView), false, false, false, false, true),
            (DispatchPriority::Terminal, None)
        );
        // A pending confirmation still owns `Esc` even with the panel modal
        // up: cancelling the confirmation is the emergency gesture.
        assert_eq!(
            resolve_priority_for(esc, None, true, false, false, false, true),
            (DispatchPriority::Emergency, None)
        );
        // The panel modal's confirm gestures are not exempt: only the
        // pending-confirmation chords confirm (a paste chord neither
        // confirms nor runs while only the panel modal is active).
        assert_eq!(
            resolve_priority_for(
                paste_chord,
                Some(A::PasteFromClipboard),
                false,
                false,
                false,
                false,
                true
            ),
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
    fn dispatch_priority_paste_banner_routes_chords_normally() {
        // Issue #1336: a pending paste is an informational banner, never a
        // capturing modal. Suspicious clipboard (embedded newline) arms the
        // pending paste through the real chord; every later step drives the
        // same `drive_chrome` dispatch `handle_event` performs.
        let mut app = paste_test_app("line1\nline2");
        app.runtime.set_layout(two_pane_layout());
        app.runtime.set_focus(ViewId::new(1));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(1)));
        press_paste_chord(&mut app);
        assert!(app.runtime.has_pending_paste());
        assert!(app.modal_capture_active());
        // Bound focus chord behind the banner runs normally: consumed, focus
        // moves, no PTY bytes, press-to-release ownership taken and released.
        assert!(!drive_chrome(&mut app, alt_mods_event()));
        assert!(drive_chrome(&mut app, char_press("l", "l", false)));
        assert_eq!(app.runtime.focused_view(), Some(ViewId::new(2)));
        assert!(app.runtime.drain_pending_input().is_empty());
        assert!(app.chrome.held.contains(&bitty_config::KeyName::Char('l')));
        assert!(!drive_chrome(&mut app, char_release("l")));
        // Unbound typing still falls through to the terminal: unconsumed
        // with its byte delivered (fall-through unchanged). The alt latch
        // is cleared first so this proves plain typing, not Alt+X ESC-x.
        assert!(!drive_chrome(&mut app, clear_mods_event()));
        assert!(!drive_chrome(&mut app, char_press("x", "x", false)));
        assert_eq!(app.runtime.drain_pending_input(), b"x");
        assert!(!drive_chrome(&mut app, char_release("x")));
        // The paste pends throughout: nothing above resolved it early.
        assert!(app.runtime.has_pending_paste());
        // Esc dismisses the pending paste: consumed, dropped, no bytes, no
        // layout change.
        assert!(drive_chrome(&mut app, esc_press()));
        assert!(!app.runtime.has_pending_paste());
        assert!(app.runtime.drain_pending_input().is_empty());
        assert_eq!(app.runtime.leaf_count(), 2);
        assert!(!drive_chrome(&mut app, esc_release()));
        // Release hygiene for the owned paste key and the alt latch.
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert!(!drive_chrome(&mut app, clear_mods_event()));
    }

    #[test]
    fn paste_pending_zoom_toggle_routes_normally() {
        // Issue #1336 (Mod+F dead): Alt+F toggle_zoom behind a pending paste
        // engages/disengages zoom instead of being swallowed; the paste
        // pends throughout until Esc dismisses it.
        let mut app = paste_test_app("line1\nline2");
        app.runtime.set_layout(two_pane_layout());
        app.runtime.set_focus(ViewId::new(1));
        press_paste_chord(&mut app);
        assert!(app.runtime.has_pending_paste());
        // First Alt+F engages zoom on the focused leaf: consumed, layout
        // collapses to the single-leaf proxy, paste still pending, no bytes.
        assert!(!drive_chrome(&mut app, alt_mods_event()));
        assert!(drive_chrome(&mut app, char_press("f", "f", false)));
        assert!(app.chrome.zoom.is_zoomed(&app.runtime));
        assert_eq!(app.runtime.leaf_count(), 1);
        assert!(app.runtime.has_pending_paste());
        assert!(app.runtime.drain_pending_input().is_empty());
        assert!(!drive_chrome(&mut app, char_release("f")));
        // Second Alt+F disengages: real tree restored, paste still pending.
        assert!(drive_chrome(&mut app, char_press("f", "f", false)));
        assert!(!app.chrome.zoom.is_zoomed(&app.runtime));
        assert_eq!(app.runtime.leaf_count(), 2);
        assert!(app.runtime.has_pending_paste());
        assert!(app.runtime.drain_pending_input().is_empty());
        assert!(!drive_chrome(&mut app, char_release("f")));
        // Esc still dismisses the paste afterwards.
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert!(!drive_chrome(&mut app, clear_mods_event()));
        assert!(drive_chrome(&mut app, esc_press()));
        assert!(!app.runtime.has_pending_paste());
        assert!(!drive_chrome(&mut app, esc_release()));
    }

    #[test]
    fn paste_pending_ctrl_d_dismisses_without_bytes() {
        // Issue #1336: Ctrl+D while a paste pends drops the paste (no 0x04
        // to the shell, no exit) instead of delivering EOF behind the banner.
        let mut app = paste_test_app("line1\nline2");
        // Ctrl+D with no paste pending encodes EOF normally (0x04).
        assert!(!drive_chrome(&mut app, mods_event(false, true)));
        assert!(!drive_chrome(&mut app, char_press("d", "\u{4}", false)));
        assert_eq!(app.runtime.drain_pending_input(), b"\x04");
        assert!(!drive_chrome(&mut app, char_release("d")));
        assert!(!drive_chrome(&mut app, clear_mods_event()));
        // Arm the pending paste through the real chord.
        press_paste_chord(&mut app);
        assert!(app.runtime.has_pending_paste());
        // Ctrl+D behind the banner drops the paste: no bytes, no exit.
        assert!(!drive_chrome(&mut app, char_press("d", "\u{4}", false)));
        assert!(!app.runtime.has_pending_paste());
        assert!(app.runtime.drain_pending_input().is_empty());
        assert!(!drive_chrome(&mut app, char_release("d")));
        // Once the gate is gone Ctrl+D encodes EOF normally again.
        assert!(!drive_chrome(&mut app, char_press("d", "\u{4}", false)));
        assert_eq!(app.runtime.drain_pending_input(), b"\x04");
        assert!(!drive_chrome(&mut app, char_release("d")));
        // Release hygiene for the owned paste key and the modifier latch.
        assert!(!drive_chrome(&mut app, char_release("V")));
        assert!(!drive_chrome(&mut app, clear_mods_event()));
    }

    #[test]
    fn shell_control_bytes_pass_chrome_unchanged() {
        // Issue #1356: with no modal pending, shell control bytes are never
        // chrome — Ctrl+C reaches the PTY as `0x03` (SIGINT via the line
        // discipline), Ctrl+D as `0x04` (EOF), and plain typing plus Enter
        // as their bytes. The Wayland shape carries `text=None` for
        // control chords; the tracked modifier snapshot synthesizes C0.
        let mut app = workspace_test_app();
        // Ctrl+C: ModifiersChanged then the press, like winit delivers.
        assert!(!drive_chrome(&mut app, mods_event(false, true)));
        assert!(!drive_chrome(
            &mut app,
            WindowEventKind::KeyboardInput(KeyEvent {
                logical_key: LogicalKey::Character("c".to_string()),
                text: None,
                location: bitty_platform::KeyLocation::Standard,
                state: PressState::Pressed,
                repeat: false,
                is_synthetic: false,
            })
        ));
        assert_eq!(app.runtime.drain_pending_input(), b"\x03");
        assert!(!drive_chrome(&mut app, clear_mods_event()));
        // Typing `exit` plus Enter passes through byte-identical.
        for ch in ["e", "x", "i", "t"] {
            assert!(!drive_chrome(&mut app, char_press(ch, ch, false)));
        }
        assert!(!drive_chrome(&mut app, named_press(NamedKey::Enter)));
        assert_eq!(app.runtime.drain_pending_input(), b"exit\r");
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

    // CTX-0265 which-key help popup: toggle gestures, registry rows, Esc.
    // -------------------------------------------------------------------

    fn help_test_app(maps: Vec<bitty_config::ResolvedKeymap>) -> TerminalApp {
        let rt = Runtime::with_defaults().expect("must build");
        TerminalApp::with_theme(
            rt,
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        )
    }

    fn alt_mods() -> WindowEventKind {
        WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift: false,
            control: false,
            alt: true,
            super_pressed: false,
        })
    }

    fn alt_shift_mods() -> WindowEventKind {
        WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift: true,
            control: false,
            alt: true,
            super_pressed: false,
        })
    }

    fn no_mods() -> WindowEventKind {
        WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift: false,
            control: false,
            alt: false,
            super_pressed: false,
        })
    }

    #[test]
    fn help_toggle_action_shows_live_registry_rows() {
        // `toggle_help` regenerates rows from the live table on every
        // show: the default navigate row and the backtick row are listed,
        // and repeating the action hides the popup again.
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps);
        assert!(!app.runtime.help_visible());
        app.apply_chrome_action(bitty_config::ChromeAction::ToggleHelp);
        assert!(app.runtime.help_visible());
        let rows = app.runtime.help_rows();
        assert!(
            rows.iter().any(|r| r == "alt+h  goto_split:left"),
            "navigate row listed: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r == "alt+`  toggle_help"),
            "backtick row listed: {rows:?}"
        );
        app.apply_chrome_action(bitty_config::ChromeAction::ToggleHelp);
        assert!(!app.runtime.help_visible(), "same chord dismisses");
    }

    #[test]
    fn help_popup_lists_added_chord_by_construction() {
        // Registry-generated content proof through the app path: a user
        // chord appended to the live table appears in the popup with no
        // second source.
        let effective = bitty_config::EffectiveConfig {
            keymaps: vec![bitty_config::KeymapEntry {
                chord: "alt+e".into(),
                action: "open_composer".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        let maps = bitty_config::resolve_keymaps(&effective).expect("resolves");
        let mut app = help_test_app(maps);
        app.apply_chrome_action(bitty_config::ChromeAction::ToggleHelp);
        assert!(app.runtime.help_visible());
        assert!(
            app.runtime
                .help_rows()
                .iter()
                .any(|r| r == "alt+e  open_composer"),
            "added chord listed: {:?}",
            app.runtime.help_rows()
        );
    }

    #[test]
    fn open_composer_opens_session_but_stays_unbound_by_default() {
        // E-CUT (CTX-0968): the Core composer session is retired; defaults
        // stay unbound so Alt+E reaches the shell until the user opts in,
        // and an explicit open is fail-closed with no Core session (plugin
        // owns editing UX).
        use bitty_config::{ChromeAction, KeyName, KeyRef, match_keymap, resolve_keymaps};
        let maps = resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
        assert!(
            !maps.iter().any(|m| m.action == ChromeAction::OpenComposer),
            "defaults must not bind open_composer so Alt+E stays shell input"
        );
        let alt_e = KeyRef {
            key: KeyName::Char('e'),
            ctrl: false,
            alt: true,
            shift: false,
            super_held: false,
        };
        assert_eq!(
            match_keymap(&maps, alt_e),
            None,
            "unbound Alt+E must fall through to the PTY"
        );
        // Bound: retired — no Core session opens; layout/focus/help untouched.
        let mut app = help_test_app(maps);
        let leafs = app.runtime.leaf_count();
        let leaves = app.runtime.layout().leaf_ids();
        let focused = app.runtime.focused_view();
        let help = app.runtime.help_visible();
        app.apply_chrome_action(ChromeAction::OpenComposer);
        assert_eq!(app.runtime.leaf_count(), leafs, "no pane surgery");
        assert_eq!(app.runtime.layout().leaf_ids(), leaves, "layout unchanged");
        assert_eq!(app.runtime.focused_view(), focused, "focus unchanged");
        assert_eq!(app.runtime.help_visible(), help, "help unchanged");
    }

    #[test]
    fn composer_cutover_zero_plugin_startup_keeps_retained_open() {
        // E-CUT (CTX-0968): zero-plugin startup is retained-deleted: the
        // owner is retained with a diagnostic, and OpenComposer opens
        // nothing (no Core session; plugin owns editing UX).
        use crate::composer_owner::{ComposerOwner, RetainedReason};
        use bitty_config::ChromeAction;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps);
        assert_eq!(
            app.composer_owner(),
            ComposerOwner::RetainedCore(RetainedReason::NoPluginRuntime)
        );
        assert!(!app.composer_plugin_owns());
        app.apply_chrome_action(ChromeAction::OpenComposer);
    }

    #[test]
    fn composer_cutover_safe_mode_identical_to_before() {
        // E-CUT (CTX-0968): safe mode is retained-deleted, identical to the
        // zero-plugin path: the owner is retained without consulting a VM,
        // and OpenComposer opens nothing in both modes.
        use crate::composer_owner::{ComposerOwner, RetainedReason};
        use bitty_config::ChromeAction;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps).with_safe_mode(true);
        assert_eq!(
            app.composer_owner(),
            ComposerOwner::RetainedCore(RetainedReason::SafeMode)
        );
        app.apply_chrome_action(ChromeAction::OpenComposer);
    }

    #[test]
    fn composer_cutover_dispatch_fails_closed_without_owner() {
        // W-103 S-5 (CTX-0929) through E-CUT (CTX-0968): dispatch fails
        // closed with a diagnostic when no ACTIVE plugin owns the UX, and
        // unknown verbs never dispatch. No Core session exists to open.
        use crate::composer_owner::COMPOSER_COMMAND_OPEN;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps);
        let error = app
            .dispatch_composer_command(COMPOSER_COMMAND_OPEN)
            .expect_err("no owner must fail");
        assert!(error.contains("bitty-terminal.composer"), "got {error}");
        let error = app
            .dispatch_composer_command("definitely-not-a-verb")
            .expect_err("unknown verb must fail");
        assert!(
            error.contains("not a known composer command"),
            "got {error}"
        );
    }

    #[test]
    fn composer_rollback_drill_uninstall_and_safe_mode_keep_retained_behavior() {
        // E-CUT (CTX-0968): scripted rollback drill, retained-deleted. An
        // ACTIVE composer plugin owns the UX and answers the open verb;
        // uninstalling it (runtime detached) is fail-closed with a
        // diagnostic and no Core session; a safe-mode startup behaves
        // identically (no session in both).
        use crate::composer_owner::{
            COMPOSER_COMMAND_CLOSE, COMPOSER_COMMAND_OPEN, COMPOSER_PLUGIN_ID,
            COMPOSER_REQUIRED_CAPABILITIES, ComposerOwner, RetainedReason, fixture,
        };
        use bitty_config::ChromeAction;
        let root = fixture::temp_dir("rollback-drill");
        fixture::write_plugin(
            &root,
            COMPOSER_PLUGIN_ID,
            ">=0.0.1",
            COMPOSER_REQUIRED_CAPABILITIES,
            &[COMPOSER_COMMAND_OPEN, COMPOSER_COMMAND_CLOSE],
        );
        let mut plugin_runtime = fixture::runtime_for(vec![root.clone()]);
        plugin_runtime.discover();
        let id = bitty_plugin_host::manifest::PluginId::new(COMPOSER_PLUGIN_ID).expect("id");
        plugin_runtime.activate(&id).expect("activate");
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps).with_plugin_runtime(Some(plugin_runtime));
        assert_eq!(app.composer_owner(), ComposerOwner::Plugin);
        // The plugin path answers the open verb; no Core session exists to
        // shadow while the plugin owns UX.
        app.dispatch_composer_command(COMPOSER_COMMAND_OPEN)
            .expect("active plugin answers open");
        app.apply_chrome_action(ChromeAction::OpenComposer);
        // Rollback step 1: uninstall (runtime detached) is retained-deleted
        // with a diagnostic and no session.
        app = app.with_plugin_runtime(None);
        assert_eq!(
            app.composer_owner(),
            ComposerOwner::RetainedCore(RetainedReason::NoPluginRuntime)
        );
        app.apply_chrome_action(ChromeAction::OpenComposer);
        // Rollback step 2: safe-mode startup keeps the retired behavior (no
        // session).
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut safe_app = help_test_app(maps).with_safe_mode(true);
        assert_eq!(
            safe_app.composer_owner(),
            ComposerOwner::RetainedCore(RetainedReason::SafeMode)
        );
        safe_app.apply_chrome_action(ChromeAction::OpenComposer);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn composer_cutover_dispatch_error_keeps_fallback_session_routed() {
        // E-CUT (CTX-0968): when the ACTIVE plugin fails the open dispatch,
        // the retired path latches the fallback for observability but opens
        // no Core session. The latch stays set until cleared by the next
        // modal pass; the plugin still owns the next open.
        use crate::composer_owner::{
            COMPOSER_COMMAND_CLOSE, COMPOSER_PLUGIN_ID, COMPOSER_REQUIRED_CAPABILITIES,
            ComposerOwner, fixture,
        };
        use bitty_config::ChromeAction;
        let root = fixture::temp_dir("dispatch-error-fallback");
        // The plugin owns the UX (active, matching) but never registered
        // the open verb, so the open dispatch fails.
        fixture::write_plugin(
            &root,
            COMPOSER_PLUGIN_ID,
            ">=0.0.1",
            COMPOSER_REQUIRED_CAPABILITIES,
            &[COMPOSER_COMMAND_CLOSE],
        );
        let mut plugin_runtime = fixture::runtime_for(vec![root.clone()]);
        plugin_runtime.discover();
        let id = bitty_plugin_host::manifest::PluginId::new(COMPOSER_PLUGIN_ID).expect("id");
        plugin_runtime.activate(&id).expect("activate");
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps).with_plugin_runtime(Some(plugin_runtime));
        assert_eq!(app.composer_owner(), ComposerOwner::Plugin);
        app.apply_chrome_action(ChromeAction::OpenComposer);
        assert!(
            app.composer_core_fallback_latched,
            "fallback latch set on dispatch-error open (retired, no session)"
        );
        assert_eq!(
            app.composer_owner(),
            ComposerOwner::Plugin,
            "next open retries the plugin"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn composer_cutover_editor_round_trip_keeps_fallback_routed() {
        // E-CUT (CTX-0968): the PTY-leaf editor flow is retired with the
        // composer engine. Hosting an editor leaf still tears down safely;
        // there is no composer session to close/reopen and no draft to
        // assert. The leaf lifecycle (single session, temp cleanup) is
        // covered by the external-editor host tests in `tests.rs`.
        use crate::composer_owner::{
            COMPOSER_COMMAND_CLOSE, COMPOSER_PLUGIN_ID, COMPOSER_REQUIRED_CAPABILITIES,
            ComposerOwner, fixture,
        };
        use crate::editor_host::ExternalEditorSession;
        use bitty_config::{ChromeAction, KeyName, KeyRef};
        use bitty_rich::host::write_composer_temp;
        let root = fixture::temp_dir("editor-round-trip-fallback");
        fixture::write_plugin(
            &root,
            COMPOSER_PLUGIN_ID,
            ">=0.0.1",
            COMPOSER_REQUIRED_CAPABILITIES,
            &[COMPOSER_COMMAND_CLOSE],
        );
        let mut plugin_runtime = fixture::runtime_for(vec![root.clone()]);
        plugin_runtime.discover();
        let id = bitty_plugin_host::manifest::PluginId::new(COMPOSER_PLUGIN_ID).expect("id");
        plugin_runtime.activate(&id).expect("activate");
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps).with_plugin_runtime(Some(plugin_runtime));
        assert_eq!(app.composer_owner(), ComposerOwner::Plugin);
        // Dispatch-error fallback latches (retired, no session).
        app.apply_chrome_action(ChromeAction::OpenComposer);
        assert!(app.composer_core_fallback_latched);
        // Mimic editor hosting: record a session directly; a keypress while
        // hosting must not route to any composer session (none exists) and
        // clears the stale latch.
        let temp = write_composer_temp("draft", &std::env::temp_dir()).expect("temp");
        let home = app.runtime.focused_view().expect("focused leaf");
        assert!(app.chrome.editor.begin(ExternalEditorSession {
            view: ViewId::new(999),
            temp,
            return_focus: home,
        }));
        let keyref = |key: KeyName| KeyRef {
            key,
            ctrl: false,
            alt: false,
            shift: false,
            super_held: false,
        };
        assert!(
            !app.route_cw_modal(&test_char_key("x"), &keyref(KeyName::Char('x'))),
            "no session open while the editor hosts"
        );
        assert!(
            !app.composer_core_fallback_latched,
            "stale latch clears with no open session"
        );
        // The editor leaf vanishes: poll drops the session safely; the
        // retired reopen latches again for observability while the plugin
        // still owns the UX.
        app.poll_external_editor();
        assert!(
            app.composer_core_fallback_latched,
            "reopen latches fallback routing under plugin ownership (retired)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Hold or release the platform leader modifier for the hint live
    /// tests (#1307: Alt+Space everywhere except Windows, where the OS
    /// owns Alt+Space and the default falls back to Ctrl+Space).
    fn set_leader_mods(app: &mut TerminalApp, held: bool) {
        if cfg!(target_os = "windows") {
            app.chrome.app_mods.control = held;
        } else {
            app.chrome.app_mods.alt = held;
        }
    }

    /// One marked `OSC 133` command cycle for the fold/hint live tests.
    fn mark_test_command(app: &mut TerminalApp) {
        app.runtime
            .handle_pty_bytes(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07out\x1b]133;D;0\x07");
    }

    #[test]
    fn fold_verbs_target_latest_command_live() {
        // CTX-0723 (#980): bound fold verbs resolve the focused view's
        // latest command block; with no shell marks they refuse loudly
        // instead of inventing a target.
        use bitty_config::ChromeAction;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps);
        app.apply_chrome_action(ChromeAction::FoldToggle);
        assert_eq!(app.runtime.cw_latest_command_id(), None);
        mark_test_command(&mut app);
        mark_test_command(&mut app);
        let latest = app.runtime.cw_latest_command_id().expect("marked");
        app.apply_chrome_action(ChromeAction::FoldToggle);
        assert!(app.runtime.cw_fold_is_folded(latest));
        app.apply_chrome_action(ChromeAction::FoldExpand);
        assert!(!app.runtime.cw_fold_is_folded(latest));
        app.apply_chrome_action(ChromeAction::FoldCollapse);
        assert!(app.runtime.cw_fold_is_folded(latest));
    }

    #[test]
    fn leader_arms_hint_and_operator_label_dispatches() {
        // CTX-0723 (#981): Alt+Space arms a live batch from the engine,
        // `z` + `a` dispatches the fold toggle, and no PTY byte leaks.
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps);
        mark_test_command(&mut app);
        let latest = app.runtime.cw_latest_command_id().expect("marked");
        set_leader_mods(&mut app, true);
        let space = test_key(LogicalKey::Named(NamedKey::Space));
        assert!(
            drive_chrome(&mut app, WindowEventKind::KeyboardInput(space)),
            "leader press is chrome-owned"
        );
        assert!(app.runtime.cw_hint_is_armed());
        assert!(
            app.runtime.drain_pending_input().is_empty(),
            "leader types no shell bytes"
        );
        set_leader_mods(&mut app, false);
        assert!(drive_chrome(
            &mut app,
            WindowEventKind::KeyboardInput(test_char_key("z"))
        ));
        assert!(app.runtime.cw_hint_is_armed(), "operator waits for label");
        assert!(drive_chrome(
            &mut app,
            WindowEventKind::KeyboardInput(test_char_key("a"))
        ));
        assert!(!app.runtime.cw_hint_is_armed(), "dispatch disarms");
        assert!(app.runtime.cw_fold_is_folded(latest));
        assert!(
            app.runtime.drain_pending_input().is_empty(),
            "hint keystrokes never reach the PTY"
        );
    }

    #[test]
    fn hint_disabled_leader_keeps_normal_owner() {
        // CTX-0735 (#981): `hints_enabled = false` keeps the Leader from
        // arming — the press falls through to normal routing (fail-open)
        // and no hint session ever owns follow-up keys.
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps).with_hints_enabled(false);
        mark_test_command(&mut app);
        set_leader_mods(&mut app, true);
        let space = test_key(LogicalKey::Named(NamedKey::Space));
        assert!(
            !drive_chrome(&mut app, WindowEventKind::KeyboardInput(space)),
            "disabled hints leave the leader press unconsumed"
        );
        assert!(
            !app.runtime.cw_hint_is_armed(),
            "no session arms while disabled"
        );
        // Follow-up letters are not hint-owned either: unconsumed, shell-bound.
        set_leader_mods(&mut app, false);
        assert!(
            !drive_chrome(&mut app, WindowEventKind::KeyboardInput(test_char_key("z"))),
            "letters keep their normal owner while disabled"
        );
        assert!(
            !app.runtime.cw_hint_is_armed(),
            "letters never arm while disabled"
        );
    }

    #[test]
    fn hint_esc_cancels_and_expiry_fails_open() {
        // CTX-0723 (#981): Esc cancels an armed session (consumed); an
        // expired Leader window fail-opens the press to normal routing.
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps);
        mark_test_command(&mut app);
        set_leader_mods(&mut app, true);
        assert!(drive_chrome(
            &mut app,
            WindowEventKind::KeyboardInput(test_key(LogicalKey::Named(NamedKey::Space)))
        ));
        assert!(app.runtime.cw_hint_is_armed());
        set_leader_mods(&mut app, false);
        assert!(drive_chrome(
            &mut app,
            WindowEventKind::KeyboardInput(test_key(LogicalKey::Named(NamedKey::Escape)))
        ));
        assert!(!app.runtime.cw_hint_is_armed(), "esc cancelled");

        // Re-arm, then force the window into the past: the next press
        // expires the Leader and keeps its normal owner (shell bytes).
        set_leader_mods(&mut app, true);
        assert!(drive_chrome(
            &mut app,
            WindowEventKind::KeyboardInput(test_key(LogicalKey::Named(NamedKey::Space)))
        ));
        assert!(app.runtime.cw_hint_is_armed());
        set_leader_mods(&mut app, false);
        app.chrome.leader_state = bitty_config::LeaderState::Armed { deadline_ms: 0 };
        assert!(
            !drive_chrome(&mut app, WindowEventKind::KeyboardInput(test_char_key("x"))),
            "expired leader falls through"
        );
        assert!(!app.runtime.cw_hint_is_armed(), "expiry disarmed");
        assert_eq!(app.runtime.drain_pending_input(), b"x");
    }

    #[test]
    fn composer_open_routes_typing_and_submit_to_pty() {
        // E-CUT (CTX-0968): the Core composer session is retired; an
        // explicit open opens nothing and typing reaches the PTY directly
        // (no bracketed submit frame; the plugin owns submit via the
        // `terminal.submit` host operation).
        use bitty_config::ChromeAction;
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps);
        app.apply_chrome_action(ChromeAction::OpenComposer);
        let text_key = |s: &str| KeyEvent {
            logical_key: LogicalKey::Character(s.to_string()),
            text: Some(s.to_string()),
            location: bitty_platform::KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        };
        assert!(!drive_chrome(
            &mut app,
            WindowEventKind::KeyboardInput(text_key("h"))
        ));
        assert!(!drive_chrome(
            &mut app,
            WindowEventKind::KeyboardInput(text_key("i"))
        ));
        // Typing reached the PTY byte-identically (no composer framing).
        let pending = app.runtime.drain_pending_input();
        assert_eq!(pending, b"hi");
    }

    #[test]
    fn toggle_palette_is_inert_and_unbound_by_default() {
        // CTX-0647 / #1003 palette entry: the palette is the `palette` Lua
        // plugin after the OQ-053 split and the SDK has no focusable
        // overlay yet, so there is no Core overlay to toggle. Until that
        // host API lands, defaults stay unbound so Ctrl+Shift+P reaches the shell,
        // and an explicitly bound chord is inert with a warning.
        use bitty_config::{ChromeAction, KeyName, KeyRef, match_keymap, resolve_keymaps};
        let maps = resolve_keymaps(&bitty_config::EffectiveConfig::default()).expect("defaults");
        assert!(
            !maps.iter().any(|m| m.action == ChromeAction::TogglePalette),
            "defaults must not bind toggle_palette so Ctrl+Shift+P stays shell input"
        );
        let ctrl_shift_p = KeyRef {
            key: KeyName::Char('p'),
            ctrl: true,
            alt: false,
            shift: true,
            super_held: false,
        };
        assert_eq!(
            match_keymap(&maps, ctrl_shift_p),
            None,
            "unbound Ctrl+Shift+P must fall through to the PTY"
        );
        // Bound-but-inert: no runtime mutation, no overlay, no routing change.
        let mut app = help_test_app(maps);
        let leafs = app.runtime.leaf_count();
        let leaves = app.runtime.layout().leaf_ids();
        let focused = app.runtime.focused_view();
        let help = app.runtime.help_visible();
        app.apply_chrome_action(ChromeAction::TogglePalette);
        assert_eq!(app.runtime.leaf_count(), leafs, "no pane surgery");
        assert_eq!(app.runtime.layout().leaf_ids(), leaves, "layout unchanged");
        assert_eq!(app.runtime.focused_view(), focused, "focus unchanged");
        assert_eq!(app.runtime.help_visible(), help, "help unchanged");
    }

    #[test]
    fn help_toggle_gestures_drive_intercept_and_esc_dismisses() {
        // End-to-end through the real intercept: Mod+backtick shows,
        // `Esc` (unbound, routed to `Runtime`) dismisses and still delivers
        // the Esc to the PTY (the popup is informational, CTX-0475), and the
        // same chord re-arms afterwards.
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps);
        assert!(!drive_chrome(&mut app, alt_mods()));
        assert!(drive_chrome(&mut app, char_press("`", "`", false)));
        assert!(app.runtime.help_visible(), "Mod+backtick shows");
        assert!(!drive_chrome(&mut app, char_release("`")));
        // Release the Mod before dismissing (physical truth).
        assert!(!drive_chrome(&mut app, no_mods()));
        assert!(!drive_chrome(&mut app, named_press(NamedKey::Escape)));
        assert!(!app.runtime.help_visible(), "Esc dismisses");
        assert_eq!(
            app.runtime.drain_pending_input(),
            b"\x1b",
            "CTX-0475: informational dismissal still delivers Esc to the PTY"
        );
        // Same-chord toggle still works after an Esc dismissal.
        assert!(!drive_chrome(&mut app, alt_mods()));
        assert!(drive_chrome(&mut app, char_press("`", "`", false)));
        assert!(app.runtime.help_visible(), "re-arms after Esc");
        assert!(!drive_chrome(&mut app, char_release("`")));
        assert!(!drive_chrome(&mut app, no_mods()));
    }

    #[test]
    fn help_question_gesture_matches_physical_shift() {
        // `?` physically carries Shift (winit reports `?` with shift
        // held): the `alt+shift+?` registry spelling matches that press
        // exactly and toggles the popup.
        let maps = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
            .expect("defaults");
        let mut app = help_test_app(maps);
        assert!(!drive_chrome(&mut app, alt_shift_mods()));
        assert!(drive_chrome(&mut app, char_press("?", "?", false)));
        assert!(app.runtime.help_visible(), "Mod+? shows");
        assert!(
            app.runtime
                .help_rows()
                .iter()
                .any(|r| r == "alt+shift+?  toggle_help"),
            "physical spelling listed: {:?}",
            app.runtime.help_rows()
        );
        assert!(!drive_chrome(&mut app, char_release("?")));
    }

    #[test]
    fn help_super_flip_spelling_toggles_and_lists() {
        // Super flip re-spells the whole gesture: Super+backtick toggles
        // and the popup lists the Super spellings (never stale Alt).
        use bitty_config::ModKey;
        let effective = bitty_config::EffectiveConfig {
            mod_key: ModKey::Super,
            ..Default::default()
        };
        let maps = bitty_config::resolve_keymaps(&effective).expect("resolves");
        let mut app = help_test_app(maps);
        assert!(!drive_chrome(
            &mut app,
            WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
                shift: false,
                control: false,
                alt: false,
                super_pressed: true,
            })
        ));
        assert!(drive_chrome(&mut app, char_press("`", "`", false)));
        assert!(app.runtime.help_visible(), "Super+backtick shows");
        let rows = app.runtime.help_rows();
        assert!(
            rows.iter().any(|r| r == "super+`  toggle_help"),
            "super spelling listed: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.starts_with("alt+")),
            "no Alt spellings survive the flip: {rows:?}"
        );
    }

    /// Latch the app modifier mirror like the compositor-fed stream does and
    /// drive one character press/release through the real intercept. The
    /// mirror is set *before* the press, exactly like the `ModifiersChanged`
    /// stream that precedes a chord on Wayland/winit.
    fn drive_mod_char(
        app: &mut TerminalApp,
        reported: &str,
        ctrl: bool,
        alt: bool,
        shift: bool,
        super_held: bool,
    ) -> bool {
        app.chrome.app_mods = AppModifiers {
            shift,
            control: ctrl,
            alt,
            super_held,
        };
        let consumed = drive_chrome(app, char_press(reported, reported, false));
        let _ = drive_chrome(app, char_release(reported));
        app.chrome.app_mods = AppModifiers::default();
        consumed
    }

    #[test]
    fn mod_number_switches_workspaces_on_default_keymap() {
        // Issue #1446, switch half: `Mod+Number` jumps workspaces through the
        // real intercept on the default config (the shipped `alt+1..=9`
        // defaults), and the view layer re-activates the target workspace's
        // tree — index, layout, and focus all follow the jump, never just the
        // tabline.
        let mut app = workspace_test_app();
        // Mod+T creates workspaces (the shipped creation chord; docs line).
        assert!(drive_mod_char(&mut app, "t", false, true, false, false));
        assert!(drive_mod_char(&mut app, "t", false, true, false, false));
        assert_eq!(app.runtime.workspace_count(), 3);
        assert_eq!(
            app.runtime.active_workspace_index(),
            2,
            "Mod+T lands on ws3"
        );
        // Give ws1 two panes so a layout swap is observable, not just an index.
        assert!(drive_mod_char(&mut app, "1", false, true, false, false));
        assert_eq!(app.runtime.active_workspace_index(), 0);
        let first_leaf = app.runtime.focused_view().expect("ws1 focus");
        let fresh = ViewId::new(9);
        let mut layout = app.runtime.layout().clone();
        let old = layout
            .find_leaf(first_leaf)
            .cloned()
            .expect("ws1 leaf present");
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(View::new(fresh, 80, 24)),
        );
        app.runtime.set_layout(layout);
        assert!(app.runtime.set_focus(fresh));
        // Every supported digit is consumed (never typed into the shell), and
        // in-range digits land exactly where they name.
        for (digit, want_index) in [("1", 0usize), ("2", 1), ("3", 2)] {
            assert!(
                drive_mod_char(&mut app, digit, false, true, false, false),
                "Mod+{digit} is bound (consumed)"
            );
            assert_eq!(
                app.runtime.active_workspace_index(),
                want_index,
                "Mod+{digit} lands on ws{}",
                want_index + 1
            );
            assert!(
                app.runtime.drain_pending_input().is_empty(),
                "the digit must never reach the shell"
            );
        }
        // Jumping back to ws1 re-activates ITS tree (two leaves, the split
        // leaf still focused), not ws3's single leaf.
        assert!(drive_mod_char(&mut app, "1", false, true, false, false));
        assert_eq!(app.runtime.active_workspace_index(), 0);
        assert_eq!(app.runtime.layout().leaf_count(), 2);
        assert!(app.runtime.layout().leaf_ids().contains(&fresh));
        assert_eq!(app.runtime.focused_view(), Some(fresh));
        // Out-of-range N clamps to the last workspace (issue #1365, accepted)
        // and still never leaks the digit.
        assert!(drive_mod_char(&mut app, "9", false, true, false, false));
        assert_eq!(app.runtime.active_workspace_index(), 2);
        assert_eq!(app.runtime.workspaceline_text(), "1:ws1 2:ws2 3:ws3* (3)");
        assert!(app.runtime.drain_pending_input().is_empty());
    }

    #[test]
    fn mod_number_single_workspace_clamps_and_never_leaks() {
        // Issue #1446, "stuck on workspace 1" half: the shipped default
        // session has exactly ONE workspace, so `Mod+2..9` clamp to it
        // (issue #1365, accepted) and nothing visible changes — the reason a
        // daily driver reads the numbers as dead. Creation is the separate
        // `Mod+T` chord, so this test pins the two properties that must hold
        // regardless: the bound chord is consumed, and the digit never
        // reaches the shell.
        let mut app = workspace_test_app();
        assert_eq!(app.runtime.workspace_count(), 1, "shipped default");
        for digit in ["1", "2", "9"] {
            assert!(
                drive_mod_char(&mut app, digit, false, true, false, false),
                "Mod+{digit} is a bound chord (consumed)"
            );
            assert_eq!(app.runtime.active_workspace_index(), 0);
            assert_eq!(app.runtime.workspace_count(), 1);
            assert!(
                app.runtime.drain_pending_input().is_empty(),
                "the digit must never reach the shell"
            );
        }
    }

    #[test]
    fn workspace_creation_chord_is_listed_in_help() {
        // Issue #1446, "unclear how to create a new workspace": the shipped
        // creation chord is `Mod+T` (`alt+t` = `workspace_new`, the docs line
        // for bitty-terminal-docs), and the `Mod+`` popup lists it from the
        // live registry so the gesture is discoverable in-app.
        let mut app = workspace_test_app();
        assert!(drive_mod_char(&mut app, "`", false, true, false, false));
        assert!(app.runtime.help_visible(), "Mod+backtick shows the popup");
        assert!(
            app.runtime
                .help_rows()
                .iter()
                .any(|row| row == "alt+t  workspace_new"),
            "creation chord listed: {:?}",
            app.runtime.help_rows()
        );
        assert!(
            app.runtime
                .help_rows()
                .iter()
                .any(|row| row == "alt+1  workspace_focus:1"),
            "switch chord listed: {:?}",
            app.runtime.help_rows()
        );
        assert!(
            app.runtime
                .help_rows()
                .iter()
                .any(|row| row == "alt+shift+1  workspace_move:1"),
            "move chord listed under its canonical spelling: {:?}",
            app.runtime.help_rows()
        );
    }

    #[test]
    fn mod_shift_number_moves_focused_pane_on_default_keymap() {
        // Issue #1446, move half (the regression): the platform reports the
        // modifier-applied character, so the physical move gesture arrives as
        // the shifted symbol (`@` for Shift+2) while DEC-0034 spells the
        // chord `shift+alt+2`. Before the base-key fallback the press matched
        // nothing: no pane moved AND the symbol leaked to the PTY as shell
        // input. Through the real intercept the gesture now reparents the
        // focused leaf into the target workspace without switching.
        let mut app = workspace_test_app();
        assert!(drive_mod_char(&mut app, "t", false, true, false, false));
        assert_eq!(app.runtime.workspace_count(), 2);
        assert!(drive_mod_char(&mut app, "1", false, true, false, false));
        assert_eq!(app.runtime.active_workspace_index(), 0);
        // Split ws1: the move needs a second leaf to reparent.
        let moved = ViewId::new(9);
        let mut layout = app.runtime.layout().clone();
        let focused = app.runtime.focused_view().expect("ws1 focus");
        let old = layout
            .find_leaf(focused)
            .cloned()
            .expect("ws1 leaf present");
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(View::new(moved, 80, 24)),
        );
        app.runtime.set_layout(layout);
        assert!(app.runtime.set_focus(moved));
        // Physical Mod+Shift+2: the platform reports `@` with Shift held.
        assert!(
            drive_mod_char(&mut app, "@", false, true, true, false),
            "the physical move gesture is chrome-owned"
        );
        assert!(
            app.runtime.drain_pending_input().is_empty(),
            "the shifted symbol must never reach the shell"
        );
        assert_eq!(
            app.runtime.workspace_count(),
            2,
            "move never removes a slot"
        );
        assert_eq!(
            app.runtime.active_workspace_index(),
            0,
            "move reparents; it never switches the active workspace"
        );
        assert_eq!(app.runtime.layout().leaf_count(), 1, "ws1 promoted a leaf");
        // The moved pane is in ws2, focused, with its id intact.
        assert!(drive_mod_char(&mut app, "2", false, true, false, false));
        assert_eq!(app.runtime.active_workspace_index(), 1);
        assert!(app.runtime.layout().leaf_ids().contains(&moved));
        assert_eq!(app.runtime.focused_view(), Some(moved));
        // Every digit's shifted symbol folds the same way (1..=9).
        let symbols = ["!", "@", "#", "$", "%", "^", "&", "*", "("];
        for (index, symbol) in symbols.iter().enumerate() {
            let one_based = index + 1;
            assert!(
                drive_mod_char(&mut app, symbol, false, true, true, false),
                "physical Mod+Shift+{one_based} ({symbol}) is consumed"
            );
            assert!(
                app.runtime.drain_pending_input().is_empty(),
                "no byte leak for {symbol}"
            );
            assert!(
                app.runtime.workspace_count() >= 2,
                "Mod+Shift+{one_based} never removes a workspace"
            );
        }
        // Same gesture under the Super flip (`mod_key = "super"`).
        let effective = bitty_config::EffectiveConfig {
            mod_key: bitty_config::ModKey::Super,
            ..Default::default()
        };
        let maps = bitty_config::resolve_keymaps(&effective).expect("resolves");
        let mut app = TerminalApp::with_theme(
            Runtime::with_defaults().expect("must build"),
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        assert!(drive_mod_char(&mut app, "t", false, false, false, true));
        assert!(drive_mod_char(&mut app, "1", false, false, false, true));
        assert_eq!(app.runtime.active_workspace_index(), 0);
        assert!(
            drive_mod_char(&mut app, "@", false, false, true, true),
            "shift+super+2 is consumed"
        );
        assert!(
            app.runtime.drain_pending_input().is_empty(),
            "no byte leak under the Super flip"
        );
    }

    // Live-spawn: real POSIX shells in two workspaces. `#[cfg(unix)]` keeps it
    // off Windows CI; `require_pty!()` keeps the force-no-PTY simulation path.
    // ConPTY coverage lives in bitty-pty/tests/spawn_windows.rs (CTX-0268).
    #[test]
    #[cfg(unix)]
    fn workspace_keys_switch_and_move_keep_live_sessions() {
        require_pty!();
        let mut app = workspace_test_app();
        // The primary shell attaches to ws1's leaf and its recipe is
        // remembered; every fresh workspace replays it (CTX-0359).
        app.runtime
            .spawn_shell_with_args("/bin/sh", &[])
            .expect("primary shell must spawn headless");
        let ws1_leaf = app.runtime.focused_view().expect("primary leaf");
        // Mod+T: a fresh workspace with its OWN shell, never the primary's.
        assert!(drive_mod_char(&mut app, "t", false, true, false, false));
        assert_eq!(app.runtime.workspace_count(), 2);
        assert_eq!(app.runtime.active_workspace_index(), 1);
        let ws2_leaf = app.runtime.focused_view().expect("ws2 leaf");
        assert_ne!(ws2_leaf, ws1_leaf, "ws2 gets its own View");
        assert!(app.runtime.has_pane_session(&ws2_leaf), "ws2 owns a shell");
        // Switching jumps the live layout and focus; the other workspace's
        // session keeps running (view layer re-activates, no session teardown).
        assert!(drive_mod_char(&mut app, "1", false, true, false, false));
        assert_eq!(app.runtime.active_workspace_index(), 0);
        assert_eq!(app.runtime.focused_view(), Some(ws1_leaf));
        assert!(
            app.runtime.has_pane_session(&ws2_leaf),
            "ws2's shell survives the switch away"
        );
        assert!(drive_mod_char(&mut app, "2", false, true, false, false));
        assert_eq!(app.runtime.active_workspace_index(), 1);
        assert_eq!(app.runtime.focused_view(), Some(ws2_leaf));
        // Physical Mod+Shift+1 (reported `!`): ws2's pane moves into ws1 with
        // its session, without switching away from ws2.
        assert!(drive_mod_char(&mut app, "!", false, true, true, false));
        assert!(
            app.runtime.drain_pending_input().is_empty(),
            "the shifted symbol never reaches a shell"
        );
        assert_eq!(
            app.runtime.active_workspace_index(),
            1,
            "the move reparents, it never switches"
        );
        assert!(drive_mod_char(&mut app, "1", false, true, false, false));
        assert!(
            app.runtime.layout().leaf_ids().contains(&ws2_leaf),
            "the moved pane is live in ws1"
        );
        assert!(
            app.runtime.has_pane_session(&ws2_leaf),
            "the moved pane keeps its shell"
        );
    }

    #[test]
    fn user_symbol_chord_keeps_precedence_over_base_key_fallback() {
        // Single owner: the reported spelling is matched first, so an explicit
        // user binding on the shifted symbol wins over the physical base-key
        // fallback — the fallback only ever runs when nothing matched.
        use bitty_config::{ChromeAction, EffectiveConfig, KeymapEntry, ModKey};
        let effective = EffectiveConfig {
            mod_key: ModKey::Alt,
            keymaps: vec![KeymapEntry {
                chord: "shift+alt+@".into(),
                action: "focus_next".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        let maps = bitty_config::resolve_keymaps(&effective).expect("resolves");
        // The reported spelling stays the first owner at the table level.
        assert_eq!(
            bitty_config::match_keymap(
                &maps,
                bitty_config::KeyRef {
                    key: bitty_config::KeyName::Char('@'),
                    ctrl: false,
                    alt: true,
                    shift: true,
                    super_held: false,
                }
            ),
            Some(ChromeAction::FocusNext)
        );
        let mut app = TerminalApp::with_theme(
            Runtime::with_defaults().expect("must build"),
            bitty_config::theme::DEFAULT_THEME_NAME,
            "default",
            maps,
            SpawnSpec::default(),
        );
        // ws2 exists, so the base-key fallback (workspace_move:2) would be
        // observable if it won.
        assert!(drive_mod_char(&mut app, "t", false, true, false, false));
        assert!(drive_mod_char(&mut app, "1", false, true, false, false));
        assert_eq!(app.runtime.active_workspace_index(), 0);
        // Split ws1 and focus the new leaf so `focus_next` is observable.
        let fresh = ViewId::new(9);
        let mut layout = app.runtime.layout().clone();
        let focused = app.runtime.focused_view().expect("ws1 focus");
        let old = layout
            .find_leaf(focused)
            .cloned()
            .expect("ws1 leaf present");
        layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(View::new(fresh, 80, 24)),
        );
        app.runtime.set_layout(layout);
        assert!(app.runtime.set_focus(fresh));
        assert!(drive_mod_char(&mut app, "@", false, true, true, false));
        assert_ne!(
            app.runtime.focused_view(),
            Some(fresh),
            "the user's symbol chord ran (focus_next)"
        );
        assert_eq!(
            app.runtime.layout().leaf_count(),
            2,
            "no move: both ws1 panes stay put"
        );
        assert_eq!(app.runtime.active_workspace_index(), 0, "no switch either");
        assert_eq!(app.runtime.workspace_count(), 2);
        assert!(
            app.runtime.drain_pending_input().is_empty(),
            "a consumed chord types nothing into the shell"
        );
        // A shifted symbol with no chord either way still routes to the PTY
        // (the fallback never over-consumes).
        assert!(
            !drive_mod_char(&mut app, "\"", false, true, true, false),
            "unbound shifted symbol falls through to the terminal path"
        );
    }

    // Issue #1445: Regression tests for Ctrl+Shift+hjkl resize directions.
    // Divider moves in the pressed direction regardless of focus
    // (Hyprland/Niri model): Left/Up shrinks the first pane (ratio down),
    // Right/Down grows it (ratio up). Both legacy (shift+ctrl+hjkl) and
    // Mod-aware (ctrl+shift+alt+hjkl, rebound to super) chords resolve to
    // the same actions and move the ratio the same way.

    fn ratio_of(layout: &LayoutNode) -> f32 {
        layout.split_ratio_at(&[]).expect("root split")
    }

    #[test]
    fn resize_left_moves_divider_left_from_either_side() {
        use bitty_config::SplitDir;
        for focused in [ViewId::new(1), ViewId::new(2)] {
            let mut layout = two_pane_layout();
            assert!(
                resize_focused_pane(&mut layout, focused, SplitDir::Left),
                "resize:left with focus {focused:?} should succeed"
            );
            assert!(
                (ratio_of(&layout) - 0.4).abs() < 1e-6,
                "resize:left must move divider left (0.5 -> 0.4), got {}",
                ratio_of(&layout)
            );
        }
    }

    #[test]
    fn resize_right_moves_divider_right_from_either_side() {
        use bitty_config::SplitDir;
        for focused in [ViewId::new(1), ViewId::new(2)] {
            let mut layout = two_pane_layout();
            assert!(
                resize_focused_pane(&mut layout, focused, SplitDir::Right),
                "resize:right with focus {focused:?} should succeed"
            );
            assert!(
                (ratio_of(&layout) - 0.6).abs() < 1e-6,
                "resize:right must move divider right (0.5 -> 0.6), got {}",
                ratio_of(&layout)
            );
        }
    }

    #[test]
    fn resize_up_moves_divider_up_from_either_side() {
        use bitty_config::SplitDir;
        let vertical = || {
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(1), 80, 12)),
                LayoutNode::leaf(View::new(ViewId::new(2), 80, 12)),
            )
        };
        for focused in [ViewId::new(1), ViewId::new(2)] {
            let mut layout = vertical();
            assert!(
                resize_focused_pane(&mut layout, focused, SplitDir::Up),
                "resize:up with focus {focused:?} should succeed"
            );
            assert!(
                (ratio_of(&layout) - 0.4).abs() < 1e-6,
                "resize:up must move divider up (0.5 -> 0.4), got {}",
                ratio_of(&layout)
            );
        }
    }

    #[test]
    fn resize_down_moves_divider_down_from_either_side() {
        use bitty_config::SplitDir;
        let vertical = || {
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(1), 80, 12)),
                LayoutNode::leaf(View::new(ViewId::new(2), 80, 12)),
            )
        };
        for focused in [ViewId::new(1), ViewId::new(2)] {
            let mut layout = vertical();
            assert!(
                resize_focused_pane(&mut layout, focused, SplitDir::Down),
                "resize:down with focus {focused:?} should succeed"
            );
            assert!(
                (ratio_of(&layout) - 0.6).abs() < 1e-6,
                "resize:down must move divider down (0.5 -> 0.6), got {}",
                ratio_of(&layout)
            );
        }
    }

    #[test]
    fn hjkl_direction_mapping_is_vim_standard() {
        use bitty_config::SplitDir;
        // Pin the canonical direction names (issue #1445).
        assert_eq!(SplitDir::Left.canonical(), "left", "h maps to left");
        assert_eq!(SplitDir::Down.canonical(), "down", "j maps to down");
        assert_eq!(SplitDir::Up.canonical(), "up", "k maps to up");
        assert_eq!(SplitDir::Right.canonical(), "right", "l maps to right");
    }

    #[test]
    fn hjkl_chords_resize_through_match_keymap_both_mods() {
        use bitty_config::{
            ChromeAction, KeyName, KeyRef, ModKey, SplitDir, default_keymaps_with_mod, match_keymap,
        };
        // Issue #1445: chord-level pin — every hjkl resize chord (legacy
        // shift+ctrl+hjkl plus Mod-aware ctrl+shift+alt+hjkl, the latter
        // rebound to ctrl+shift+super+hjkl under a Super flip) must resolve
        // to its vim-standard direction AND move the ratio the same way
        // (Left/Up decrease, Right/Down increase), from either focus side.
        // A swap in DEFAULT_KEYMAPS or a sign flip in resize_focused_pane
        // fails here (the old bool-only tests could not detect either).
        let cases: &[(char, SplitDir, bool)] = &[
            ('h', SplitDir::Left, false),
            ('j', SplitDir::Down, true),
            ('k', SplitDir::Up, false),
            ('l', SplitDir::Right, true),
        ];
        for mod_key in [ModKey::Alt, ModKey::Super] {
            let maps = default_keymaps_with_mod(mod_key).expect("defaults valid");
            for (key_char, dir, increases) in cases {
                let key = KeyName::Char(*key_char);
                // Legacy mod-independent chord: shift+ctrl+<key>.
                let legacy = KeyRef {
                    key,
                    ctrl: true,
                    alt: false,
                    shift: true,
                    super_held: false,
                };
                assert_eq!(
                    match_keymap(&maps, legacy),
                    Some(ChromeAction::ResizeSplit(*dir)),
                    "shift+ctrl+{key_char} resizes {dir:?} under mod {:?}",
                    mod_key.canonical()
                );
                // Mod-aware chord: alt spelling under Alt, super under Super.
                let mod_aware = match mod_key {
                    ModKey::Alt => KeyRef {
                        key,
                        ctrl: true,
                        alt: true,
                        shift: true,
                        super_held: false,
                    },
                    ModKey::Super => KeyRef {
                        key,
                        ctrl: true,
                        alt: false,
                        shift: true,
                        super_held: true,
                    },
                };
                assert_eq!(
                    match_keymap(&maps, mod_aware),
                    Some(ChromeAction::ResizeSplit(*dir)),
                    "mod-aware {key_char} resizes {dir:?} under mod {:?}",
                    mod_key.canonical()
                );
                // Both chords drive the same ratio sign, from either side.
                for focused in [ViewId::new(1), ViewId::new(2)] {
                    for (label, action) in [
                        ("legacy", ChromeAction::ResizeSplit(*dir)),
                        ("mod-aware", ChromeAction::ResizeSplit(*dir)),
                    ] {
                        let ChromeAction::ResizeSplit(resize_dir) = action else {
                            unreachable!("resize action")
                        };
                        let mut layout = if matches!(resize_dir, SplitDir::Left | SplitDir::Right) {
                            two_pane_layout()
                        } else {
                            LayoutNode::split(
                                SplitAxis::Vertical,
                                0.5,
                                LayoutNode::leaf(View::new(ViewId::new(1), 80, 12)),
                                LayoutNode::leaf(View::new(ViewId::new(2), 80, 12)),
                            )
                        };
                        assert!(
                            resize_focused_pane(&mut layout, focused, resize_dir),
                            "{label} {key_char} with focus {focused:?} should succeed"
                        );
                        let ratio = ratio_of(&layout);
                        if *increases {
                            assert!(
                                (ratio - 0.6).abs() < 1e-6,
                                "{label} {key_char} must increase ratio (0.5 -> 0.6), got {ratio}"
                            );
                        } else {
                            assert!(
                                (ratio - 0.4).abs() < 1e-6,
                                "{label} {key_char} must decrease ratio (0.5 -> 0.4), got {ratio}"
                            );
                        }
                    }
                }
            }
        }
    }
}
