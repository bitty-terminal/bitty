//! OS light/dark appearance signal (CTX-0951, issue #1669).
//!
//! [`SystemAppearance`] is the owned vocabulary for the desktop's
//! light/dark preference. Two seams:
//!
//! 1. [`query_system_appearance`]: synchronous cold-path default. It
//!    degrades to [`SystemAppearance::Unknown`] on every platform today:
//!    Linux needs the `org.freedesktop.portal.Settings` `color-scheme`
//!    read (D-Bus), macOS needs `NSApplication.effectiveAppearance`, and
//!    Windows needs the `AppsUseLightTheme` registry value — none of which
//!    has a dependency-free synchronous read in this crate, and this slice
//!    adds no new OS bindings. Callers treat `Unknown` as dark
//!    (dark-first default, matching the `bitty-dark` preset).
//! 2. Live toggles arrive as
//!    [`PlatformEvent::SystemAppearanceChanged`](crate::event::PlatformEvent::SystemAppearanceChanged),
//!    re-dispatched app-level from winit `ThemeChanged` by the `App` glue
//!    (emitted natively on macOS and Windows; backends without OS theme
//!    support never emit it, so no spurious swaps). Consumers re-resolve
//!    dual `appearance.theme` selections and swap the live palette; single
//!    selections ignore it.
//!
//! Zero `unsafe`; no new dependencies (winit only).

use winit::window::Theme as WinitTheme;

/// Desktop light/dark preference, owned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SystemAppearance {
    /// The desktop prefers light chrome.
    Light,
    /// The desktop prefers dark chrome.
    Dark,
    /// No signal (cold-path default on every platform today, and any
    /// backend without OS theme support). Callers use dark-first behavior.
    #[default]
    Unknown,
}

impl SystemAppearance {
    /// Stable lowercase label (for logs and diagnostics).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this is a real OS signal (as opposed to [`Self::Unknown`]).
    #[must_use]
    pub const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

impl std::fmt::Display for SystemAppearance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Maps an upstream theme payload to the app-level owned event.
///
/// `pub(crate)`: winit types never escape this crate (see crate-level
/// ownership rules); the `App` window-event glue calls this before the
/// window-scoped translation.
///
/// The payload is always a real signal ([`SystemAppearance::Light`] or
/// [`SystemAppearance::Dark`]): winit only emits `ThemeChanged` on a true
/// OS toggle, never `Unknown`.
pub(crate) const fn map_appearance_changed(theme: WinitTheme) -> crate::event::PlatformEvent {
    crate::event::PlatformEvent::SystemAppearanceChanged(match theme {
        WinitTheme::Light => SystemAppearance::Light,
        WinitTheme::Dark => SystemAppearance::Dark,
    })
}

/// Synchronous OS appearance query with per-platform gates.
///
/// Always [`SystemAppearance::Unknown`] in this slice (see module docs for
/// the native API each gate will bind): cold-path callers fall back to the
/// dark half of a dual theme, and live `ThemeChanged` events drive swaps
/// where the backend emits them. Pure and headless-safe.
#[must_use]
pub fn query_system_appearance() -> SystemAppearance {
    query_system_appearance_inner()
}

#[cfg(target_os = "macos")]
fn query_system_appearance_inner() -> SystemAppearance {
    // Binds `NSApplication.effectiveAppearance` when an AppKit seam lands;
    // until then Unknown (live `ThemeChanged` events still drive swaps).
    SystemAppearance::Unknown
}

#[cfg(target_os = "windows")]
fn query_system_appearance_inner() -> SystemAppearance {
    // Binds the `AppsUseLightTheme` registry value when a registry seam
    // lands; until then Unknown (live `ThemeChanged` events still drive
    // swaps).
    SystemAppearance::Unknown
}

#[cfg(target_os = "linux")]
fn query_system_appearance_inner() -> SystemAppearance {
    // Binds the `org.freedesktop.portal.Settings` `color-scheme` read when
    // a D-Bus seam lands; until then Unknown (Wayland/X11 backends do not
    // emit `ThemeChanged` either, so dual themes stay on the dark half).
    SystemAppearance::Unknown
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn query_system_appearance_inner() -> SystemAppearance {
    // No OS appearance source is defined for this target.
    SystemAppearance::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_labels_and_default() {
        assert_eq!(SystemAppearance::Light.as_str(), "light");
        assert_eq!(SystemAppearance::Dark.as_str(), "dark");
        assert_eq!(SystemAppearance::Unknown.as_str(), "unknown");
        assert_eq!(SystemAppearance::default(), SystemAppearance::Unknown);
        assert!(SystemAppearance::Light.is_known());
        assert!(SystemAppearance::Dark.is_known());
        assert!(!SystemAppearance::Unknown.is_known());
        assert_eq!(SystemAppearance::Dark.to_string(), "dark");
    }

    #[test]
    fn query_degrades_cleanly_without_display() {
        // Headless-safe by construction: no window system is touched, so
        // CI asserts the documented degradation (dark-first callers proceed
        // on the dark half; live events drive swaps where emitted).
        assert_eq!(query_system_appearance(), SystemAppearance::Unknown);
    }

    #[test]
    fn upstream_themes_map_to_owned_appearance() {
        assert_eq!(
            map_appearance_changed(WinitTheme::Light),
            crate::event::PlatformEvent::SystemAppearanceChanged(SystemAppearance::Light)
        );
        assert_eq!(
            map_appearance_changed(WinitTheme::Dark),
            crate::event::PlatformEvent::SystemAppearanceChanged(SystemAppearance::Dark)
        );
    }
}
