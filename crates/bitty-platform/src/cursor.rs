//! Owned mouse-pointer cursor icon (issue #1762, `OSC 22`).
//!
//! Bitty-owned mirror of `winit::window::CursorIcon` (itself the
//! `cursor-icon` CSS keyword set). No `winit` type escapes this crate: the
//! public surface is [`CursorIcon`], and the `winit` conversion lives behind
//! [`crate::WindowHandle::set_cursor_icon`]. Name strings match
//! `bitty-vt`'s `PointerShape::name()` 1:1; the `PointerShape` → `CursorIcon`
//! map lives in `bitty-runtime` (which owns both deps) so this crate keeps
//! its no-workspace-dependency rule (ADR-0003).

/// Owned mouse-pointer cursor icon.
///
/// Variant names and [`CursorIcon::name`] strings match
/// `winit::window::CursorIcon` exactly (CSS cursor keywords, kebab-case), so
/// the platform mapping is total and fail-open to [`CursorIcon::Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CursorIcon {
    /// Platform default (usually an arrow).
    #[default]
    Default,
    /// Context menu available.
    ContextMenu,
    /// Help available.
    Help,
    /// Link pointer (hand).
    Pointer,
    /// Progress (busy but interactive).
    Progress,
    /// Busy, user should wait.
    Wait,
    /// Cell selection.
    Cell,
    /// Crosshair.
    Crosshair,
    /// Text selection (I-beam).
    Text,
    /// Vertical text selection.
    VerticalText,
    /// Alias/shortcut to be created.
    Alias,
    /// Something to be copied.
    Copy,
    /// Something to be moved.
    Move,
    /// Dragged item cannot be dropped here.
    NoDrop,
    /// Requested action will not be carried out.
    NotAllowed,
    /// Something can be grabbed.
    Grab,
    /// Something is being grabbed.
    Grabbing,
    /// East border resize.
    EResize,
    /// North border resize.
    NResize,
    /// North-east corner resize.
    NeResize,
    /// North-west corner resize.
    NwResize,
    /// South border resize.
    SResize,
    /// South-east corner resize.
    SeResize,
    /// South-west corner resize.
    SwResize,
    /// West border resize.
    WResize,
    /// East-west resize.
    EwResize,
    /// North-south resize.
    NsResize,
    /// North-east/south-west diagonal resize.
    NeswResize,
    /// North-west/south-east diagonal resize.
    NwseResize,
    /// Column resize.
    ColResize,
    /// Row resize.
    RowResize,
    /// Scroll in any direction.
    AllScroll,
    /// Zoom in.
    ZoomIn,
    /// Zoom out.
    ZoomOut,
}

impl CursorIcon {
    /// Kebab-case name matching `winit::window::CursorIcon::name()`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::ContextMenu => "context-menu",
            Self::Help => "help",
            Self::Pointer => "pointer",
            Self::Progress => "progress",
            Self::Wait => "wait",
            Self::Cell => "cell",
            Self::Crosshair => "crosshair",
            Self::Text => "text",
            Self::VerticalText => "vertical-text",
            Self::Alias => "alias",
            Self::Copy => "copy",
            Self::Move => "move",
            Self::NoDrop => "no-drop",
            Self::NotAllowed => "not-allowed",
            Self::Grab => "grab",
            Self::Grabbing => "grabbing",
            Self::EResize => "e-resize",
            Self::NResize => "n-resize",
            Self::NeResize => "ne-resize",
            Self::NwResize => "nw-resize",
            Self::SResize => "s-resize",
            Self::SeResize => "se-resize",
            Self::SwResize => "sw-resize",
            Self::WResize => "w-resize",
            Self::EwResize => "ew-resize",
            Self::NsResize => "ns-resize",
            Self::NeswResize => "nesw-resize",
            Self::NwseResize => "nwse-resize",
            Self::ColResize => "col-resize",
            Self::RowResize => "row-resize",
            Self::AllScroll => "all-scroll",
            Self::ZoomIn => "zoom-in",
            Self::ZoomOut => "zoom-out",
        }
    }

    /// Parses a CSS kebab-case cursor name.
    ///
    /// Returns `None` for unknown names (callers fail open to
    /// [`CursorIcon::Default`]). X11 cursor-font aliases are resolved by
    /// `bitty-vt` before this boundary; this parser accepts only the
    /// canonical kebab-case set.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "default" => Some(Self::Default),
            "context-menu" => Some(Self::ContextMenu),
            "help" => Some(Self::Help),
            "pointer" => Some(Self::Pointer),
            "progress" => Some(Self::Progress),
            "wait" => Some(Self::Wait),
            "cell" => Some(Self::Cell),
            "crosshair" => Some(Self::Crosshair),
            "text" => Some(Self::Text),
            "vertical-text" => Some(Self::VerticalText),
            "alias" => Some(Self::Alias),
            "copy" => Some(Self::Copy),
            "move" => Some(Self::Move),
            "no-drop" => Some(Self::NoDrop),
            "not-allowed" => Some(Self::NotAllowed),
            "grab" => Some(Self::Grab),
            "grabbing" => Some(Self::Grabbing),
            "e-resize" => Some(Self::EResize),
            "n-resize" => Some(Self::NResize),
            "ne-resize" => Some(Self::NeResize),
            "nw-resize" => Some(Self::NwResize),
            "s-resize" => Some(Self::SResize),
            "se-resize" => Some(Self::SeResize),
            "sw-resize" => Some(Self::SwResize),
            "w-resize" => Some(Self::WResize),
            "ew-resize" => Some(Self::EwResize),
            "ns-resize" => Some(Self::NsResize),
            "nesw-resize" => Some(Self::NeswResize),
            "nwse-resize" => Some(Self::NwseResize),
            "col-resize" => Some(Self::ColResize),
            "row-resize" => Some(Self::RowResize),
            "all-scroll" => Some(Self::AllScroll),
            "zoom-in" => Some(Self::ZoomIn),
            "zoom-out" => Some(Self::ZoomOut),
            _ => None,
        }
    }

    /// Converts to the upstream `winit` icon (private boundary, never public).
    pub(crate) fn to_winit(self) -> winit::window::CursorIcon {
        match self {
            Self::Default => winit::window::CursorIcon::Default,
            Self::ContextMenu => winit::window::CursorIcon::ContextMenu,
            Self::Help => winit::window::CursorIcon::Help,
            Self::Pointer => winit::window::CursorIcon::Pointer,
            Self::Progress => winit::window::CursorIcon::Progress,
            Self::Wait => winit::window::CursorIcon::Wait,
            Self::Cell => winit::window::CursorIcon::Cell,
            Self::Crosshair => winit::window::CursorIcon::Crosshair,
            Self::Text => winit::window::CursorIcon::Text,
            Self::VerticalText => winit::window::CursorIcon::VerticalText,
            Self::Alias => winit::window::CursorIcon::Alias,
            Self::Copy => winit::window::CursorIcon::Copy,
            Self::Move => winit::window::CursorIcon::Move,
            Self::NoDrop => winit::window::CursorIcon::NoDrop,
            Self::NotAllowed => winit::window::CursorIcon::NotAllowed,
            Self::Grab => winit::window::CursorIcon::Grab,
            Self::Grabbing => winit::window::CursorIcon::Grabbing,
            Self::EResize => winit::window::CursorIcon::EResize,
            Self::NResize => winit::window::CursorIcon::NResize,
            Self::NeResize => winit::window::CursorIcon::NeResize,
            Self::NwResize => winit::window::CursorIcon::NwResize,
            Self::SResize => winit::window::CursorIcon::SResize,
            Self::SeResize => winit::window::CursorIcon::SeResize,
            Self::SwResize => winit::window::CursorIcon::SwResize,
            Self::WResize => winit::window::CursorIcon::WResize,
            Self::EwResize => winit::window::CursorIcon::EwResize,
            Self::NsResize => winit::window::CursorIcon::NsResize,
            Self::NeswResize => winit::window::CursorIcon::NeswResize,
            Self::NwseResize => winit::window::CursorIcon::NwseResize,
            Self::ColResize => winit::window::CursorIcon::ColResize,
            Self::RowResize => winit::window::CursorIcon::RowResize,
            Self::AllScroll => winit::window::CursorIcon::AllScroll,
            Self::ZoomIn => winit::window::CursorIcon::ZoomIn,
            Self::ZoomOut => winit::window::CursorIcon::ZoomOut,
        }
    }
}

impl std::fmt::Display for CursorIcon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_through_winit() {
        // Every owned icon maps to a winit icon whose name parses back to
        // the same owned icon: the 1:1 contract the runtime relies on.
        let all = [
            CursorIcon::Default,
            CursorIcon::ContextMenu,
            CursorIcon::Help,
            CursorIcon::Pointer,
            CursorIcon::Progress,
            CursorIcon::Wait,
            CursorIcon::Cell,
            CursorIcon::Crosshair,
            CursorIcon::Text,
            CursorIcon::VerticalText,
            CursorIcon::Alias,
            CursorIcon::Copy,
            CursorIcon::Move,
            CursorIcon::NoDrop,
            CursorIcon::NotAllowed,
            CursorIcon::Grab,
            CursorIcon::Grabbing,
            CursorIcon::EResize,
            CursorIcon::NResize,
            CursorIcon::NeResize,
            CursorIcon::NwResize,
            CursorIcon::SResize,
            CursorIcon::SeResize,
            CursorIcon::SwResize,
            CursorIcon::WResize,
            CursorIcon::EwResize,
            CursorIcon::NsResize,
            CursorIcon::NeswResize,
            CursorIcon::NwseResize,
            CursorIcon::ColResize,
            CursorIcon::RowResize,
            CursorIcon::AllScroll,
            CursorIcon::ZoomIn,
            CursorIcon::ZoomOut,
        ];
        for icon in all {
            let winit_icon = icon.to_winit();
            assert_eq!(
                winit_icon.name(),
                icon.name(),
                "winit name must match owned name for {icon:?}"
            );
            assert_eq!(
                CursorIcon::from_name(icon.name()),
                Some(icon),
                "owned parse must round-trip {icon:?}"
            );
        }
    }

    #[test]
    fn unknown_names_fail_open_to_default_at_call_site() {
        assert_eq!(CursorIcon::from_name("no-such-cursor"), None);
        assert_eq!(CursorIcon::from_name(""), None);
        assert_eq!(CursorIcon::from_name("POINTER"), None);
    }
}
