//! The per-cell containment plan selector.
//!
//! Exactly one OS module owns probe/spawn/kill. This module re-exports that
//! module under the stable names [`crate::containment`] uses, so `mod.rs`
//! stays platform-neutral and readable. A cell with no accepted mechanism
//! selects the private `unsupported` module, whose probe returns
//! [`crate::containment::ContainmentRefusal::UnsupportedPlatform`] and whose
//! spawn refuses before creating anything.

#[cfg(target_os = "linux")]
mod selected {
    pub use crate::containment::child::{ContainedChild, spawn_with};
    pub use crate::containment::linux::probe;
}

#[cfg(target_os = "windows")]
mod selected {
    pub use crate::containment::windows::{ContainedChild, probe, spawn_with};
}

#[cfg(target_os = "freebsd")]
mod selected {
    pub use crate::containment::freebsd::{ContainedChild, probe, spawn_with};
}

#[cfg(target_os = "macos")]
mod selected {
    pub use crate::containment::macos::{ContainedChild, probe, spawn_with};
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "windows",
    target_os = "freebsd",
    target_os = "macos"
)))]
mod selected {
    pub use crate::containment::unsupported::{ContainedChild, probe, spawn_with};
}

pub use selected::{ContainedChild, probe, spawn_with};
