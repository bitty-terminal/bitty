#![forbid(unsafe_code)]
//! Issue #2577 (CTX-1488): deleting a panel must never repaint its old
//! rectangle in the theme's focused-outline accent.
//!
//! Root cause class: a closed `View` has no live allocation, so the RFC-0002
//! close transition keeps its last presented decoration ring as a
//! `ClosingFrame`. The old ring was composited in the overlay phase, *after*
//! every live leaf, so for the close duration it drew a complete accent
//! rounded rectangle over the sibling that had already expanded into the
//! vacated slot. Under `catppuccin-mocha` the focused outline (`#94E2D5`)
//! reads as green, hence the owner-visible "green flash".
//!
//! These regressions are frame-level and deterministic (no GUI, no timing):
//! for the split, stacked, zoomed, floating, and pinned deletion paths, the
//! animated close frames must be pixel-identical to an instant-delete
//! reference under the same theme. A stale ring, a stale leaf fill, or a
//! wrong clear color would differ.
//!
//! Invariant (normative for the present path): **live ownership wins over
//! stale chrome.** A removed leaf's retained frame may never paint over a
//! pixel owned by a live allocation; the vacated rect presents the surviving
//! owner (or the theme background where the layout owns nothing).

use std::time::{Duration, Instant};

use bitty_config::theme::{Theme, resolve_theme};
use bitty_runtime::{
    AnimationKind, AnimationPolicy, LayoutNode, Runtime, RuntimeConfig, SplitAxis, ThemePalette,
    UiRect, View, ViewId,
};

const COLS: usize = 80;
const ROWS: usize = 24;
const CLOSE_MS: u32 = 120;
const FRAME_STEP: Duration = Duration::from_millis(16);
const SETTLE: Duration = Duration::from_millis(500);

/// `catppuccin-mocha` (the owner report) plus a neutral light preset.
const THEMES: [&str; 2] = ["catppuccin-mocha", "github-light"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delete {
    /// Horizontal split; the focused right pane is deleted.
    Split,
    /// Vertical stack; the focused bottom pane is deleted.
    Stacked,
    /// The focused pane is zoomed (full-window proxy) when it is deleted;
    /// the app restores the real tree and closes in the same event.
    Zoomed,
    /// The focused pane was detached as a floating panel and is discarded.
    Floating,
    /// The focused pane was pinned and is discarded.
    Pinned,
    /// The last tiled pane is closed while a pinned panel survives.
    PinnedBase,
}

impl Delete {
    fn label(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::Stacked => "stacked",
            Self::Zoomed => "zoomed",
            Self::Floating => "floating",
            Self::Pinned => "pinned",
            Self::PinnedBase => "pinned-base",
        }
    }

    /// View removed by the delete (the pin in `PinnedBase` survives).
    fn removed_view(self) -> ViewId {
        match self {
            Self::PinnedBase => ViewId::new(1),
            Self::Zoomed => ViewId::new(3),
            Self::Split | Self::Stacked | Self::Floating | Self::Pinned => ViewId::new(2),
        }
    }

    /// Close transitions this path arms (the discard paths only force a
    /// redraw because the stored view vanished).
    fn arms_close(self) -> bool {
        matches!(
            self,
            Self::Split | Self::Stacked | Self::Zoomed | Self::PinnedBase
        )
    }

    /// Whether the removed view paints the focused accent before the delete
    /// (the probe would be vacuous otherwise).
    fn removed_is_focused(self) -> bool {
        !matches!(self, Self::Floating | Self::Pinned)
    }
}

fn resolved(theme_name: &str) -> &'static Theme {
    resolve_theme(Some(theme_name))
}

fn palette(theme_name: &str) -> ThemePalette {
    ThemePalette::from_theme(resolved(theme_name))
}

/// The focused-outline color the removed pane paints before it closes.
fn flash_color(theme_name: &str) -> [u8; 4] {
    resolved(theme_name).border_focused.0
}

/// Close-only animation policy: every other transition is instant so the
/// only possible pixel difference between a close run and its reference is
/// the retained closing frame itself.
fn policy(close_ms: u32) -> AnimationPolicy {
    AnimationPolicy {
        enabled: true,
        duration_ms: [0, close_ms, 0, 0, 0, 0, 0],
        ..AnimationPolicy::default()
    }
}

fn runtime(theme_name: &str, close_ms: u32) -> Runtime {
    runtime_with_policy(theme_name, policy(close_ms))
}

fn runtime_with_policy(theme_name: &str, animations: AnimationPolicy) -> Runtime {
    let theme = resolved(theme_name);
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        cols: COLS,
        rows: ROWS,
        theme: palette(theme_name),
        theme_resolved: true,
        outline_focused: theme.border_focused.0,
        outline_idle: theme.border_idle.0,
        animations,
        ..RuntimeConfig::default()
    })
    .expect("deterministic headless runtime")
}

fn three_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), COLS, ROWS)),
        LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(2), COLS, ROWS)),
            LayoutNode::leaf(View::new(ViewId::new(3), COLS, ROWS)),
        ),
    )
}

fn two_pane(axis: SplitAxis) -> LayoutNode {
    LayoutNode::split(
        axis,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), COLS, ROWS)),
        LayoutNode::leaf(View::new(ViewId::new(2), COLS, ROWS)),
    )
}

fn zoom_proxy(view: ViewId) -> LayoutNode {
    LayoutNode::leaf(View::new(view, COLS, ROWS))
}

/// Installs the pre-delete scene.
fn scene(rt: &mut Runtime, delete: Delete) {
    rt.set_container(UiRect::new(0, 0, COLS as u16, ROWS as u16));
    match delete {
        Delete::Split => {
            rt.set_layout(two_pane(SplitAxis::Horizontal));
            rt.set_focus(ViewId::new(2));
        }
        Delete::Stacked => {
            rt.set_layout(two_pane(SplitAxis::Vertical));
            rt.set_focus(ViewId::new(2));
        }
        Delete::Zoomed => {
            rt.set_layout(three_pane());
            rt.set_focus(ViewId::new(3));
        }
        Delete::Floating | Delete::Pinned | Delete::PinnedBase => {
            rt.set_layout(two_pane(SplitAxis::Horizontal));
            rt.set_focus(ViewId::new(2));
        }
    }
    // Populate the primary grid so content exists behind the chrome probes;
    // the caret is hidden so no blink phase can differ between frames.
    rt.handle_pty_bytes(b"\x1b[2J\x1b[H");
    rt.handle_pty_bytes(b"\x1b[?25l");
    for row in 0..ROWS as u8 - 4 {
        rt.handle_pty_bytes(format!("row-{row:02} 0123456789abcdef \r\n").as_bytes());
    }
    assert!(rt.focused_view().is_some(), "scene must focus a leaf");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl Rect {
    fn pixels(self) -> impl Iterator<Item = (usize, usize)> {
        (self.y0..self.y1).flat_map(move |y| (self.x0..self.x1).map(move |x| (x, y)))
    }
}

fn window_rect(rt: &Runtime, frame: &bitty_runtime::PresentFrame) -> Rect {
    let pad = i64::from(rt.window_padding_physical());
    let x0 = (pad + i64::from(frame.frame.x)).max(0) as usize;
    let y0 = (pad + i64::from(frame.frame.y)).max(0) as usize;
    Rect {
        x0,
        y0,
        x1: x0 + frame.frame.width as usize,
        y1: y0 + frame.frame.height as usize,
    }
}

fn frame_of(rt: &Runtime, view: ViewId, context: &str) -> Rect {
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented ({context})"));
    window_rect(rt, &frame)
}

fn px(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let index = (y * width + x) * 4;
    [
        rgba[index],
        rgba[index + 1],
        rgba[index + 2],
        rgba[index + 3],
    ]
}

fn count_color(rgba: &[u8], width: usize, area: Rect, color: [u8; 4]) -> usize {
    area.pixels()
        .filter(|(x, y)| px(rgba, width, *x, *y) == color)
        .count()
}

/// Brings the path into its "about to delete" state (float/pin/zoom proxy)
/// and returns the clock anchor for the delete frame.
fn prepare(rt: &mut Runtime, delete: Delete, closed_at: Instant) -> Instant {
    match delete {
        Delete::Zoomed => {
            // The zoomed proxy is the last presented frame; the close below
            // restores the real tree in the same event, exactly like
            // `chrome_keys` (no interim tick).
            rt.set_layout(zoom_proxy(ViewId::new(3)));
            let _ = rt.tick_at(closed_at);
            let proxy_settled = closed_at + Duration::from_millis(u64::from(CLOSE_MS));
            let _ = rt.tick_at(proxy_settled);
            proxy_settled
        }
        Delete::Floating => {
            rt.float_detach(ViewId::new(2)).expect("target must detach");
            let floated = closed_at + Duration::from_millis(u64::from(CLOSE_MS));
            let _ = rt.tick_at(floated);
            floated
        }
        Delete::Pinned | Delete::PinnedBase => {
            rt.pin_floating(ViewId::new(2)).expect("target must pin");
            let pinned = closed_at + Duration::from_millis(u64::from(CLOSE_MS));
            let _ = rt.tick_at(pinned);
            pinned
        }
        Delete::Split | Delete::Stacked => closed_at,
    }
}

/// Applies the delete exactly like the app/keymap close funnel: the survivor
/// is focused first, then the tree is installed through the explicit-close
/// funnel. Detach/pin discards follow their runtime path.
fn delete_target(rt: &mut Runtime, delete: Delete) {
    match delete {
        Delete::Split | Delete::Stacked => {
            rt.set_focus(ViewId::new(1));
            let mut layout = rt.layout().clone();
            layout
                .remove_leaf(ViewId::new(2))
                .expect("target leaf must exist");
            rt.set_layout_closing(layout, ViewId::new(2));
        }
        Delete::Zoomed => {
            let backup = three_pane();
            rt.set_layout(backup.clone());
            rt.set_focus(ViewId::new(2));
            let mut layout = backup;
            layout
                .remove_leaf(ViewId::new(3))
                .expect("target leaf must exist");
            rt.set_layout_closing(layout, ViewId::new(3));
        }
        Delete::Floating | Delete::Pinned => {
            assert!(
                rt.discard_floating_view(ViewId::new(2)),
                "detached/pinned panel must be discardable"
            );
        }
        Delete::PinnedBase => {
            rt.set_focus(ViewId::new(1));
            let survivor = rt
                .retire_sole_tiled_leaf_for_pinned(ViewId::new(1))
                .expect("pinned base must retire against the surviving pin");
            assert_eq!(survivor, ViewId::new(2), "the pin must take over focus");
        }
    }
}

struct Run {
    frames: Vec<Vec<u8>>,
    width: usize,
    settled: Vec<u8>,
    close_armed: bool,
}

/// Drives one delete run from the same scene at the same virtual clock.
fn run(theme_name: &str, delete: Delete, close_ms: u32) -> (Run, Rect, Instant) {
    let mut rt = runtime(theme_name, close_ms);
    scene(&mut rt, delete);
    let start = Instant::now();
    let _ = rt.tick_at(start);
    let settled_at = start + SETTLE;
    let _ = rt.tick_at(settled_at);

    let delete_at = prepare(&mut rt, delete, settled_at);
    let removed = delete.removed_view();
    let old_rect = frame_of(&rt, removed, delete.label());
    let width = rt.surface_extent().expect("extent").width() as usize;
    let pre = rt.headless_rgba().expect("pre-delete rgba");
    if delete.removed_is_focused() {
        assert!(
            count_color(&pre, width, old_rect, flash_color(theme_name)) > 0,
            "{theme_name}/{}: probe is vacuous unless the removed pane's focused ring \
             is live before the delete",
            delete.label()
        );
    }

    delete_target(&mut rt, delete);
    let mut frames = Vec::new();
    let mut close_armed = false;
    let mut now = delete_at;
    for _ in 0..20 {
        now += FRAME_STEP;
        let Some(_stats) = rt.tick_at(now) else {
            break;
        };
        if !close_armed {
            close_armed = rt
                .animation_progress(AnimationKind::Close, Some(removed), now)
                .is_some();
        }
        frames.push(rt.headless_rgba().expect("post-delete rgba"));
    }
    // Commit the animation end state before sampling the settled frame.
    let tail = delete_at + Duration::from_millis(u64::from(close_ms) + 16);
    let _ = rt.tick_at(tail);
    if !close_armed {
        close_armed = rt
            .animation_progress(AnimationKind::Close, Some(removed), tail)
            .is_some();
    }
    let settled = rt.headless_rgba().expect("settled rgba");
    (
        Run {
            frames,
            width,
            settled,
            close_armed,
        },
        old_rect,
        delete_at,
    )
}

fn assert_no_flash(
    theme_name: &str,
    delete: Delete,
    animated: &Run,
    reference: &Run,
    old_rect: Rect,
) {
    let flash = flash_color(theme_name);
    assert!(
        !animated.frames.is_empty(),
        "{theme_name}/{}: the delete must present at least one frame",
        delete.label()
    );
    assert_eq!(
        animated.width,
        reference.width,
        "{theme_name}/{}: surface width must match the reference run",
        delete.label()
    );
    let reference_flash = count_color(&reference.settled, reference.width, old_rect, flash);
    for (step, frame) in animated.frames.iter().enumerate() {
        let stale_flash = old_rect
            .pixels()
            .filter(|&(x, y)| {
                let post = px(frame, animated.width, x, y);
                let reference = px(&reference.settled, reference.width, x, y);
                post == flash && reference != flash
            })
            .count();
        assert_eq!(
            stale_flash,
            0,
            "{theme_name}/{}: frame {step} paints the removed rect in the flash color \
             (theme accent {flash:?}); the surviving owner must own every pixel",
            delete.label()
        );
        // Every post-delete pixel must be byte-identical to the instant
        // delete: the close transition may only retain chrome for space the
        // live layout no longer owns, and these deletes re-own the whole
        // removed rectangle.
        assert_eq!(
            frame,
            &reference.settled,
            "{theme_name}/{}: frame {step} must equal the instant-delete reference; \
             stale closing chrome or a stale leaf fill changed the frame",
            delete.label()
        );
        assert!(
            count_color(frame, animated.width, old_rect, flash) <= reference_flash,
            "{theme_name}/{}: post-delete frames must not add accent pixels over the reference",
            delete.label()
        );
    }
    assert_eq!(
        animated.settled,
        reference.settled,
        "{theme_name}/{}: the settled frame must equal the instant-delete reference",
        delete.label()
    );
}

#[test]
fn deleting_a_panel_never_repaints_the_removed_rect_in_the_accent() {
    for theme_name in THEMES {
        for delete in [
            Delete::Split,
            Delete::Stacked,
            Delete::Zoomed,
            Delete::Floating,
            Delete::Pinned,
            Delete::PinnedBase,
        ] {
            let (animated, old_rect, _) = run(theme_name, delete, CLOSE_MS);
            let (reference, reference_rect, _) = run(theme_name, delete, 0);
            assert_eq!(
                old_rect,
                reference_rect,
                "{theme_name}/{}: both runs must remove the same rectangle",
                delete.label()
            );
            if delete.arms_close() {
                assert!(
                    animated.close_armed,
                    "{theme_name}/{}: the close transition must arm, or the probe never \
                     exercises the retained closing frame",
                    delete.label()
                );
            }
            assert_no_flash(theme_name, delete, &animated, &reference, old_rect);
        }
    }
}

/// The reported theme must actually carry a green-looking focused outline,
/// so the regression names the owner-visible color rather than assuming it.
#[test]
fn catppuccin_mocha_focused_outline_is_the_reported_green() {
    let [r, g, b, a] = flash_color("catppuccin-mocha");
    assert_eq!(a, 0xFF, "the focused outline must be opaque");
    assert!(
        g > r.saturating_add(32) && g >= b,
        "catppuccin-mocha border_focused must read green-ish, got #{r:02X}{g:02X}{b:02X}"
    );
}

/// The owner's exact sequence with the shipped animation policy (every
/// transition at its default duration). The expanding sibling starts its
/// `Resize` tween at its pre-close bounds, so on the first close frame it
/// covers none of the removed slot: pre-fix that is where the focused
/// closing ring painted its full green rectangle. The first frame therefore
/// has to contain no accent pixel inside the removed rect at all.
#[test]
fn default_animation_policy_close_starts_without_a_stale_ring() {
    for theme_name in THEMES {
        for delete in [Delete::Split, Delete::Stacked] {
            let mut rt = runtime_with_policy(theme_name, AnimationPolicy::default());
            scene(&mut rt, delete);
            let start = Instant::now();
            let _ = rt.tick_at(start);
            let settled = start + SETTLE;
            let _ = rt.tick_at(settled);
            let old_rect = frame_of(&rt, delete.removed_view(), delete.label());
            let width = rt.surface_extent().expect("extent").width() as usize;
            let pre = rt.headless_rgba().expect("pre-close rgba");
            assert!(
                count_color(&pre, width, old_rect, flash_color(theme_name)) > 0,
                "{theme_name}/{}: probe is vacuous unless the removed pane's focused ring \
                 is live before the close",
                delete.label()
            );

            delete_target(&mut rt, delete);
            let first = settled + FRAME_STEP;
            assert!(
                rt.tick_at(first).is_some(),
                "{theme_name}/{}: the close must present its first frame",
                delete.label()
            );
            let rgba = rt.headless_rgba().expect("post-close rgba");
            assert_eq!(
                count_color(&rgba, width, old_rect, flash_color(theme_name)),
                0,
                "{theme_name}/{}: the first close frame paints the removed rect in the \
                 focused accent; the retained closing ring must be ownership-gated",
                delete.label()
            );
            assert!(
                rt.animation_progress(AnimationKind::Close, Some(delete.removed_view()), first)
                    .is_some(),
                "{theme_name}/{}: the close transition must be live on that frame",
                delete.label()
            );
        }
    }
}
