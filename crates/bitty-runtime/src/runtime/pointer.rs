//! Per-pane `OSC 22` pointer-shape stacks (issue #1762).
//!
//! Presentation-only state, never terminal truth: each visible leaf owns a
//! bounded stack of [`bitty_vt::PointerShape`] (kitty `pointer-shapes.rst`,
//! minimum 16). The parser classifies set/push/pop ops; this module owns the
//! stack transitions plus the `PointerShape` → [`bitty_platform::CursorIcon`]
//! map so `bitty-platform` keeps its no-workspace-dependency rule. The app
//! queries [`Runtime::cursor_icon_for_focused`] each tick and applies it
//! through `WindowHandle::set_cursor_icon`; an empty stack (or unknown view)
//! fails open to [`bitty_platform::CursorIcon::Default`].

use std::collections::BTreeMap;

use bitty_vt::{PointerShape, PointerShapeOp};

/// Maximum shapes retained per pane (kitty minimum stack size).
pub const POINTER_STACK_MAX: usize = 16;

/// Per-pane pointer-shape stacks keyed by leaf [`crate::ViewId`].
#[derive(Debug, Default)]
pub struct PointerStacks {
    stacks: BTreeMap<crate::ViewId, Vec<PointerShape>>,
}

impl PointerStacks {
    /// Creates empty stacks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stacks: BTreeMap::new(),
        }
    }

    /// Current (top) shape for `view`, or `None` when the stack is empty.
    #[must_use]
    pub fn current_for(&self, view: crate::ViewId) -> Option<PointerShape> {
        self.stacks
            .get(&view)
            .and_then(|stack| stack.last().copied())
    }

    /// Applies one parsed `OSC 22` op to `view`'s stack.
    pub fn apply(&mut self, view: crate::ViewId, op: &PointerShapeOp) {
        match op {
            PointerShapeOp::Set { shape } => match shape {
                None => {
                    self.stacks.remove(&view);
                }
                Some(shape) => {
                    self.stacks.insert(view, vec![*shape]);
                }
            },
            PointerShapeOp::Push { shapes } => {
                let stack = self.stacks.entry(view).or_default();
                for shape in shapes.iter() {
                    if stack.len() >= POINTER_STACK_MAX {
                        stack.remove(0);
                    }
                    stack.push(*shape);
                }
            }
            PointerShapeOp::Pop => {
                if let Some(stack) = self.stacks.get_mut(&view) {
                    stack.pop();
                    if stack.is_empty() {
                        self.stacks.remove(&view);
                    }
                }
            }
        }
    }

    /// Clears `view`'s stack (RIS/`FullReset` and pane-exit paths).
    pub fn clear(&mut self, view: crate::ViewId) {
        self.stacks.remove(&view);
    }

    /// Removes `view`'s stack, returning whether one was present.
    pub fn remove(&mut self, view: &crate::ViewId) -> bool {
        self.stacks.remove(view).is_some()
    }

    /// Whether any stack is retained (headless test seam).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stacks.is_empty()
    }
}

/// Maps a `bitty-vt` pointer shape to its platform cursor icon (1:1 by name).
#[must_use]
pub const fn cursor_icon_for_shape(shape: PointerShape) -> bitty_platform::CursorIcon {
    match shape {
        PointerShape::Default => bitty_platform::CursorIcon::Default,
        PointerShape::ContextMenu => bitty_platform::CursorIcon::ContextMenu,
        PointerShape::Help => bitty_platform::CursorIcon::Help,
        PointerShape::Pointer => bitty_platform::CursorIcon::Pointer,
        PointerShape::Progress => bitty_platform::CursorIcon::Progress,
        PointerShape::Wait => bitty_platform::CursorIcon::Wait,
        PointerShape::Cell => bitty_platform::CursorIcon::Cell,
        PointerShape::Crosshair => bitty_platform::CursorIcon::Crosshair,
        PointerShape::Text => bitty_platform::CursorIcon::Text,
        PointerShape::VerticalText => bitty_platform::CursorIcon::VerticalText,
        PointerShape::Alias => bitty_platform::CursorIcon::Alias,
        PointerShape::Copy => bitty_platform::CursorIcon::Copy,
        PointerShape::Move => bitty_platform::CursorIcon::Move,
        PointerShape::NoDrop => bitty_platform::CursorIcon::NoDrop,
        PointerShape::NotAllowed => bitty_platform::CursorIcon::NotAllowed,
        PointerShape::Grab => bitty_platform::CursorIcon::Grab,
        PointerShape::Grabbing => bitty_platform::CursorIcon::Grabbing,
        PointerShape::EResize => bitty_platform::CursorIcon::EResize,
        PointerShape::NResize => bitty_platform::CursorIcon::NResize,
        PointerShape::NeResize => bitty_platform::CursorIcon::NeResize,
        PointerShape::NwResize => bitty_platform::CursorIcon::NwResize,
        PointerShape::SResize => bitty_platform::CursorIcon::SResize,
        PointerShape::SeResize => bitty_platform::CursorIcon::SeResize,
        PointerShape::SwResize => bitty_platform::CursorIcon::SwResize,
        PointerShape::WResize => bitty_platform::CursorIcon::WResize,
        PointerShape::EwResize => bitty_platform::CursorIcon::EwResize,
        PointerShape::NsResize => bitty_platform::CursorIcon::NsResize,
        PointerShape::NeswResize => bitty_platform::CursorIcon::NeswResize,
        PointerShape::NwseResize => bitty_platform::CursorIcon::NwseResize,
        PointerShape::ColResize => bitty_platform::CursorIcon::ColResize,
        PointerShape::RowResize => bitty_platform::CursorIcon::RowResize,
        PointerShape::AllScroll => bitty_platform::CursorIcon::AllScroll,
        PointerShape::ZoomIn => bitty_platform::CursorIcon::ZoomIn,
        PointerShape::ZoomOut => bitty_platform::CursorIcon::ZoomOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ViewId;

    #[test]
    fn set_push_pop_follow_stack_semantics() {
        let view = ViewId::new(1);
        let mut stacks = PointerStacks::new();
        assert_eq!(stacks.current_for(view), None);

        stacks.apply(
            view,
            &PointerShapeOp::Set {
                shape: Some(PointerShape::Pointer),
            },
        );
        assert_eq!(stacks.current_for(view), Some(PointerShape::Pointer));

        stacks.apply(
            view,
            &PointerShapeOp::Push {
                shapes: vec![PointerShape::Wait, PointerShape::Text].into_boxed_slice(),
            },
        );
        assert_eq!(stacks.current_for(view), Some(PointerShape::Text));

        stacks.apply(view, &PointerShapeOp::Pop);
        assert_eq!(stacks.current_for(view), Some(PointerShape::Wait));

        stacks.apply(view, &PointerShapeOp::Pop);
        assert_eq!(stacks.current_for(view), Some(PointerShape::Pointer));

        stacks.apply(view, &PointerShapeOp::Pop);
        assert_eq!(stacks.current_for(view), None);

        // Popping an empty stack is a no-op.
        stacks.apply(view, &PointerShapeOp::Pop);
        assert_eq!(stacks.current_for(view), None);
    }

    #[test]
    fn set_empty_resets_and_stacks_are_per_view() {
        let a = ViewId::new(1);
        let b = ViewId::new(2);
        let mut stacks = PointerStacks::new();
        stacks.apply(
            a,
            &PointerShapeOp::Set {
                shape: Some(PointerShape::Pointer),
            },
        );
        stacks.apply(
            b,
            &PointerShapeOp::Set {
                shape: Some(PointerShape::Crosshair),
            },
        );
        assert_eq!(stacks.current_for(a), Some(PointerShape::Pointer));
        assert_eq!(stacks.current_for(b), Some(PointerShape::Crosshair));

        stacks.apply(a, &PointerShapeOp::Set { shape: None });
        assert_eq!(stacks.current_for(a), None);
        assert_eq!(stacks.current_for(b), Some(PointerShape::Crosshair));

        stacks.clear(b);
        assert_eq!(stacks.current_for(b), None);
        assert!(stacks.is_empty());
    }

    #[test]
    fn push_evicts_oldest_past_the_cap() {
        let view = ViewId::new(7);
        let mut stacks = PointerStacks::new();
        for _ in 0..POINTER_STACK_MAX + 4 {
            stacks.apply(
                view,
                &PointerShapeOp::Push {
                    shapes: vec![PointerShape::Wait].into_boxed_slice(),
                },
            );
        }
        // Bounded: exactly the cap is retained, all the same shape here.
        let stack = stacks.stacks.get(&view).expect("stack retained");
        assert_eq!(stack.len(), POINTER_STACK_MAX);
    }

    #[test]
    fn shape_to_icon_is_one_to_one_by_name() {
        for shape in [
            PointerShape::Default,
            PointerShape::Pointer,
            PointerShape::Text,
            PointerShape::Crosshair,
            PointerShape::ColResize,
            PointerShape::RowResize,
            PointerShape::NotAllowed,
            PointerShape::Grab,
        ] {
            assert_eq!(cursor_icon_for_shape(shape).name(), shape.name());
        }
    }
}
