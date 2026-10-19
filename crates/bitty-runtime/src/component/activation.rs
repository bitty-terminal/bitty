//! Last-activation projection for `bitty extension list` (CTX-1361, #2254).
//!
//! `bitty extension list` is a local-class command: it never starts an
//! instance, opens IPC, or executes a component, so it cannot read a live
//! broker's in-memory slots. Instead, the broker records each activation
//! transition it already makes (spawn, crash, idle/shutdown stop) and each
//! fail-closed denial into one bounded plain-text record beside the
//! component's `current` pointer:
//!
//! ```text
//! <user-root>/<name>/activation     # "v1 active\n", "v1 denied:capability_denied\n"
//! ```
//!
//! This is a projection of [`ComponentState`](super::ComponentState) and the
//! stable [`ComponentHostError::class`](super::ComponentHostError) labels, not
//! a second state machine: the broker writes it exactly at the transition
//! points it already handles, and [`read_activation`] is the only reader.
//! A missing, oversized, non-regular, or malformed record reads as
//! [`ActivationState::Installed`] ("no activation recorded"), never as an
//! unearned `active`.
//!
//! Only the writable user tier is recorded. The system tier is read-only
//! (owned by the package manager), so a system-only component lists as
//! `installed` until a later user-tier activation overwrites the record.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Record file name beside `current` in a user-tier component directory.
pub const COMPONENT_ACTIVATION_FILE: &str = "activation";

/// Maximum bytes read from or written to one activation record.
///
/// The longest record is `v1 denied:component_untrusted\n` (31 bytes); the
/// bound leaves room for future denial classes without growing unbounded.
pub const COMPONENT_ACTIVATION_MAX_BYTES: u64 = 64;

/// Record format prefix (`v1`).
const ACTIVATION_FORMAT_PREFIX: &str = "v1";

/// Why the last request for a component failed closed before a process ran.
///
/// The variants are the closed subset of
/// [`ComponentHostError::class`](super::ComponentHostError) labels that mean
/// "denied", so a record can never name a free-form string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationDenial {
    /// The plugin's granted capabilities do not authorize the request.
    CapabilityDenied,
    /// The requesting plugin is not in the `Activated` lifecycle state.
    PluginNotActive,
    /// The plugin's grant could not be computed (declared authority invalid).
    GrantInvalid,
    /// The installed component failed digest/descriptor verification.
    Untrusted,
    /// No installed component resolved for the request.
    Unavailable,
}

impl ActivationDenial {
    /// Every denial class a record may carry.
    pub const ALL: [Self; 5] = [
        Self::CapabilityDenied,
        Self::PluginNotActive,
        Self::GrantInvalid,
        Self::Untrusted,
        Self::Unavailable,
    ];

    /// Stable machine label (identical to the host error class).
    #[must_use]
    pub fn class(self) -> &'static str {
        match self {
            Self::CapabilityDenied => "capability_denied",
            Self::PluginNotActive => "plugin_not_active",
            Self::GrantInvalid => "component_grant_invalid",
            Self::Untrusted => "component_untrusted",
            Self::Unavailable => "component_unavailable",
        }
    }

    fn from_class(class: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|denial| denial.class() == class)
    }
}

/// Last recorded activation of one component name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationState {
    /// Installed, but no activation has been recorded (never requested
    /// since install, or the record is absent/unreadable).
    Installed,
    /// The broker spawned and handshook the component for a request.
    Active,
    /// The last request was refused before any spawn, naming the class.
    Denied(ActivationDenial),
    /// The last recorded process ended as a crash (including the crash
    /// latch that makes the component unavailable until restart).
    Crashed,
    /// The last recorded process stopped cleanly (idle stop or shutdown).
    Stopped,
}

impl ActivationState {
    /// Stable table label (`installed`, `active`, `denied:<class>`,
    /// `crashed`, `stopped`).
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Installed => "installed".to_owned(),
            Self::Active => "active".to_owned(),
            Self::Denied(denial) => format!("denied:{}", denial.class()),
            Self::Crashed => "crashed".to_owned(),
            Self::Stopped => "stopped".to_owned(),
        }
    }

    /// Named error class for a denial, `None` for every other state.
    #[must_use]
    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Denied(denial) => Some(denial.class()),
            _ => None,
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let rest = text
            .strip_prefix(ACTIVATION_FORMAT_PREFIX)?
            .strip_prefix(' ')?;
        match rest {
            "installed" => Some(Self::Installed),
            "active" => Some(Self::Active),
            "crashed" => Some(Self::Crashed),
            "stopped" => Some(Self::Stopped),
            other => other
                .strip_prefix("denied:")
                .and_then(ActivationDenial::from_class)
                .map(Self::Denied),
        }
    }

    /// One bounded record line including the trailing newline.
    fn record(self) -> String {
        let mut line = String::with_capacity(32);
        line.push_str(ACTIVATION_FORMAT_PREFIX);
        line.push(' ');
        line.push_str(&self.label());
        line.push('\n');
        line
    }
}

/// Record path inside one user-tier component directory.
#[must_use]
pub fn activation_path(component_dir: &Path) -> PathBuf {
    component_dir.join(COMPONENT_ACTIVATION_FILE)
}

/// Read the recorded activation of one component directory.
///
/// Best-effort and bounded: a missing, oversized, symlinked, non-file, or
/// malformed record reads as [`ActivationState::Installed`]. Reading never
/// creates anything and never follows a symlink at the record path.
#[must_use]
pub fn read_activation(component_dir: &Path) -> ActivationState {
    let path = activation_path(component_dir);
    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return ActivationState::Installed;
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > COMPONENT_ACTIVATION_MAX_BYTES
    {
        return ActivationState::Installed;
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return ActivationState::Installed;
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return ActivationState::Installed;
    };
    ActivationState::parse(text.trim_end_matches(['\n', '\r']))
        .unwrap_or(ActivationState::Installed)
}

/// Read the recorded activation for `name` across both tiers.
///
/// The user tier wins (it is the writable tier the broker records into);
/// the system tier is read as a fallback for installations that were
/// activated before a user-tier record existed. An invalid name reads as
/// [`ActivationState::Installed`] instead of touching an escaped path.
#[must_use]
pub fn read_component_activation(
    user_root: Option<&Path>,
    system_root: Option<&Path>,
    name: &str,
) -> ActivationState {
    if super::descriptor::validate_component_name(name).is_err() {
        return ActivationState::Installed;
    }
    for root in [user_root, system_root].into_iter().flatten() {
        let state = read_activation(&root.join(name));
        if state != ActivationState::Installed {
            return state;
        }
    }
    ActivationState::Installed
}

/// Write one activation record (best effort; never fails the caller).
///
/// The write goes through a sibling temp file and a rename, so a concurrent
/// `bitty extension list` never observes a partial record. Symlinked or
/// non-regular record paths are refused (a planted link must not redirect a
/// write), and any I/O or path failure is dropped: activation state is
/// diagnostics, never a correctness gate.
pub(crate) fn write_activation(component_dir: &Path, state: ActivationState) {
    let path = activation_path(component_dir);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => return,
        Ok(_) | Err(_) => {}
    }
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temp = component_dir.join(format!(
        "{COMPONENT_ACTIVATION_FILE}.tmp-{}-{serial}",
        std::process::id()
    ));
    let record = state.record();
    if record.len() as u64 > COMPONENT_ACTIVATION_MAX_BYTES {
        return; // unreachable by construction; never write an oversize record
    }
    if std::fs::write(&temp, record.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&temp);
        return;
    }
    if std::fs::rename(&temp, &path).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx1361-activation-{tag}-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn every_state_round_trips_through_the_bounded_record() {
        let dir = scratch("roundtrip");
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        for state in [
            ActivationState::Active,
            ActivationState::Crashed,
            ActivationState::Stopped,
            ActivationState::Installed,
            ActivationState::Denied(ActivationDenial::CapabilityDenied),
            ActivationState::Denied(ActivationDenial::PluginNotActive),
            ActivationState::Denied(ActivationDenial::GrantInvalid),
            ActivationState::Denied(ActivationDenial::Untrusted),
            ActivationState::Denied(ActivationDenial::Unavailable),
        ] {
            write_activation(&dir, state);
            assert_eq!(read_activation(&dir), state, "{}", state.label());
            assert!(
                std::fs::metadata(activation_path(&dir))
                    .expect("record")
                    .len()
                    <= COMPONENT_ACTIVATION_MAX_BYTES
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn labels_and_classes_are_stable() {
        assert_eq!(ActivationState::Installed.label(), "installed");
        assert_eq!(ActivationState::Active.label(), "active");
        assert_eq!(ActivationState::Crashed.label(), "crashed");
        assert_eq!(ActivationState::Stopped.label(), "stopped");
        let denied = ActivationState::Denied(ActivationDenial::Untrusted);
        assert_eq!(denied.label(), "denied:component_untrusted");
        assert_eq!(denied.class(), Some("component_untrusted"));
        assert_eq!(ActivationState::Active.class(), None);
        // The denial class set matches the host error classes verbatim.
        assert_eq!(
            ActivationDenial::ALL.map(ActivationDenial::class),
            [
                "capability_denied",
                "plugin_not_active",
                "component_grant_invalid",
                "component_untrusted",
                "component_unavailable",
            ]
        );
    }

    #[test]
    fn hostile_records_read_as_installed() {
        let dir = scratch("hostile");
        let path = activation_path(&dir);
        for bad in [
            "".to_owned(),
            "\n".to_owned(),
            "active\n".to_owned(),               // missing version prefix
            "v1 active-extra\n".to_owned(),      // unknown state
            "v1 denied:made_up\n".to_owned(),    // unknown class
            format!("v1 {}\n", "a".repeat(200)), // oversize
            "v1 act\u{00e9}ve\n".to_owned(),     // multibyte tail
        ] {
            std::fs::write(&path, bad.as_bytes()).expect("bad record");
            assert_eq!(read_activation(&dir), ActivationState::Installed);
        }
        // Non-UTF-8 bytes never panic and never claim a state.
        std::fs::write(&path, [0xff, 0xfe, 0x00]).expect("non-utf8");
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        // A directory at the record path is not a record.
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir(&path).expect("dir record");
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_record_is_never_followed() {
        let dir = scratch("symlink");
        let outside = dir.join("outside");
        std::fs::write(&outside, "v1 active\n").expect("outside");
        std::os::unix::fs::symlink(&outside, activation_path(&dir)).expect("symlink");
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        // Writing must not clobber the link target.
        write_activation(&dir, ActivationState::Crashed);
        assert_eq!(
            std::fs::read_to_string(&outside).expect("outside"),
            "v1 active\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_tier_record_wins_and_invalid_names_are_inert() {
        let base = scratch("tiers");
        let user = base.join("user");
        let system = base.join("system");
        std::fs::create_dir_all(user.join("net")).expect("user dir");
        std::fs::create_dir_all(system.join("net")).expect("system dir");
        assert_eq!(
            read_component_activation(Some(&user), Some(&system), "net"),
            ActivationState::Installed
        );
        write_activation(&user.join("net"), ActivationState::Active);
        write_activation(&system.join("net"), ActivationState::Stopped);
        assert_eq!(
            read_component_activation(Some(&user), Some(&system), "net"),
            ActivationState::Active
        );
        // System-only installs still report their recorded state.
        assert_eq!(
            read_component_activation(None, Some(&system), "net"),
            ActivationState::Stopped
        );
        // Path-shaped names never escape the root.
        assert_eq!(
            read_component_activation(Some(&user), Some(&system), "../net"),
            ActivationState::Installed
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
