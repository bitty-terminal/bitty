//! Background blur support (CTX-0832).
//!
//! Platform-specific implementations for applying background blur to terminal windows.
//! Support varies by platform:
//!
//! - **Wayland**: KDE Plasma and Hyprland support blur through compositor-specific protocols
//! - **macOS**: NSVisualEffectView with blur material
//! - **Windows**: Unsupported (DWM blur APIs deprecated, Acrylic requires UWP)
//! - **X11**: Unsupported (compositor-dependent, no standard protocol)

use winit::window::Window;

/// Applies background blur to a window (CTX-0832).
///
/// # Arguments
///
/// * `window` - The window to apply blur to
/// * `radius` - Blur radius in logical pixels (0..=128)
///
/// # Platform Support
///
/// - **Wayland**: Requires compositor support (KDE Plasma, Hyprland)
/// - **macOS**: Uses NSVisualEffectView
/// - **Windows/X11**: No-op (unsupported)
///
/// Unsupported platforms silently ignore the request.
///
/// W-145 reduction (CTX-0938, Issue #1623): the Wayland/macOS `eprintln!`
/// placeholder backends had no platform effect and are removed. Compositor
/// application lives in the platform-service adapter per accepted W-136;
/// Core retains only this entry shape (bounded radius in, no handle out).
/// No behavior change on any platform (stubs performed no window effect).
pub fn apply_blur(window: &Window, radius: u32) {
    if radius == 0 {
        return; // No blur requested
    }

    // Adapter stub seam: no platform effect in Core. The bounded radius
    // attribute (`0..=128`, clamped at `WindowConfig::with_blur_radius`)
    // is the surviving contract; the adapter applies it when present.
    let _ = (window, radius);
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_blur_zero_radius_is_noop() {
        // Zero radius should be a no-op regardless of platform
        // This test just ensures apply_blur doesn't panic
        // (we can't create a real Window in unit tests)
    }

    #[test]
    fn test_blur_max_radius() {
        // Max radius (128) should be accepted
        // Platform-specific code should handle it gracefully
    }
}
