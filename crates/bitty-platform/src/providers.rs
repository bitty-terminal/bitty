#![forbid(unsafe_code)]
//! Privilege-sensitive provider interface registry (CTX-1197, bitty#2012/#2017/#2020).
//!
//! Contract source: the three Phase B provider issues (network-state,
//! bluetooth-state, backlight + idle-inhibit). This module defines the
//! **interface + contract only**; the provider host/sandbox decision
//! (live sysfs reads, D-Bus bindings, Wayland protocol wiring, Lua
//! exposure) is a separate follow-up per each issue's dependencies
//! section. No code here touches the filesystem, D-Bus, Wayland, or any
//! process: sources stay behind injected traits in the per-provider
//! modules, and this registry holds only names, versions, methods, and
//! grant heads.
//!
//! Dynapi shape (frozen bitty#2003/#2005/#2006 rules, applied unchanged):
//!
//! - Owner-qualified identity `owner:iface@version` with Core owner
//!   [`PROVIDER_OWNER`]; the short grammar mirrors the manifest
//!   `services.provided` ceiling (1..128 bytes, dot-separated 1..64 byte
//!   segments, no NUL or space) so a future broker record validates
//!   without re-shaping.
//! - Capability backing (bitty#2005): every interface names at least one
//!   head; an unbacked interface is rejected by the registry test below.
//!   Consumers resolve only when their grant covers every head; ungranted
//!   consumes fail closed, indistinguishable from absent (each sampler
//!   returns `None` for both cases, covered by per-provider tests).
//! - Version negotiation (bitty#2006): exact SemVer per interface; an
//!   empty intersection fails closed, never a silent downgrade.
//!
//! Read vs control is never smuggled: no interface carries both a read
//! head and a control head. Control lives on a sibling `*.control`
//! interface (bluetooth, backlight, idle) or is absent entirely
//! (network is read-only; ESSID is a separately-granted read
//! sub-interface because it is location-adjacent data).
//!
//! The eight head names below are **proposed closed-set additions** for
//! the host task to register in `bitty-package`/`bitty-plugin-host`.
//! Until registered, samplers gate on the injected [`GrantSet`] only,
//! which is exactly the deny-by-default posture the closed set will
//! enforce.
//!
//! Bar mapping (zero bar-side redesign): every snapshot in the
//! per-provider modules is bounded sanitized primitives (enums, clamped
//! integers, printable strings); Lua selects icons, thresholds, and
//! toggle semantics. No new bar API is required.

use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Owner identity
// ---------------------------------------------------------------------------

/// Core owner qualifying every provider interface (`owner:iface@version`).
///
/// The provider is Platform Core, not an extension: the owner is implied
/// by Core authorship and pinned here in exactly one place.
pub const PROVIDER_OWNER: &str = "bitty";

/// Version minted for every provider interface in this slice.
///
/// One version for the whole batch: the interfaces land together, so they
/// negotiate together. Bumped per interface if behavior ever diverges.
pub const PROVIDER_INTERFACE_VERSION: &str = "1.0.0";

// ---------------------------------------------------------------------------
// Capability heads (proposed closed-set additions, one place)
// ---------------------------------------------------------------------------

/// Base read head: link state + bandwidth counters (unprivileged).
pub const NETWORK_READ_HEAD: &str = "platform.network.read";

/// Separately-granted ESSID read head (location-adjacent; may be denied
/// by default on some targets; ungranted is indistinguishable from an
/// NM-less host).
pub const NETWORK_ESSID_HEAD: &str = "platform.network.essid.read";

/// Bluetooth read head (radio power + bonded-device enumeration).
pub const BLUETOOTH_READ_HEAD: &str = "platform.bluetooth.read";

/// Bluetooth control head (`set_power` only).
pub const BLUETOOTH_CONTROL_HEAD: &str = "platform.bluetooth.control";

/// Backlight read head (brightness/max, unprivileged sysfs read).
pub const BACKLIGHT_READ_HEAD: &str = "platform.backlight.read";

/// Backlight control head (`set_step` only; attempts only when
/// permitted, `E_NOT_PERMITTED` otherwise, never escalates).
pub const BACKLIGHT_CONTROL_HEAD: &str = "platform.backlight.control";

/// Idle-inhibit read head (`inhibited` state).
pub const IDLE_READ_HEAD: &str = "platform.idle.read";

/// Idle-inhibit control head (`set` only, compositor-side).
pub const IDLE_CONTROL_HEAD: &str = "platform.idle.control";

/// Every head proposed by this slice, in registry order.
pub const PROVIDER_HEADS: &[&str] = &[
    NETWORK_READ_HEAD,
    NETWORK_ESSID_HEAD,
    BLUETOOTH_READ_HEAD,
    BLUETOOTH_CONTROL_HEAD,
    BACKLIGHT_READ_HEAD,
    BACKLIGHT_CONTROL_HEAD,
    IDLE_READ_HEAD,
    IDLE_CONTROL_HEAD,
];

// ---------------------------------------------------------------------------
// Grant snapshot
// ---------------------------------------------------------------------------

/// Deny-by-default grant snapshot gating one generation's provider reads.
///
/// Mirrors the `PluginServices` granted-capability snapshot: sorted and
/// deduplicated at construction, re-read on every `get()`/control call
/// so narrowing takes effect without restart. An empty set denies
/// everything; unknown heads never imply each other (ESSID needs its own
/// head even when the base read head is granted; control never follows
/// from read).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrantSet {
    heads: BTreeSet<String>,
}

impl GrantSet {
    /// Builds the snapshot; duplicates collapse, order is canonical.
    #[must_use]
    pub fn new(heads: Vec<String>) -> Self {
        Self {
            heads: heads.into_iter().collect(),
        }
    }

    /// Whether `head` is granted in this snapshot.
    #[must_use]
    pub fn contains(&self, head: &str) -> bool {
        self.heads.contains(head)
    }

    /// Whether no head is granted (denies everything).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }

    /// Number of granted heads.
    #[must_use]
    pub fn len(&self) -> usize {
        self.heads.len()
    }
}

// ---------------------------------------------------------------------------
// Interface descriptors
// ---------------------------------------------------------------------------

/// Maximum bytes of one short interface name (mirrors the manifest
/// `services.provided` ceiling so broker validation never re-shapes).
pub const MAX_IFACE_BYTES: usize = 128;

/// Maximum bytes of one dot-separated interface segment (same mirror).
pub const MAX_IFACE_SEGMENT_BYTES: usize = 64;

/// Maximum provided methods per interface (bounded surface by construction).
pub const MAX_METHODS_PER_INTERFACE: usize = 4;

/// One dynapi provider interface: identity, methods, and backing heads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderInterface {
    /// Short interface name (dot-separated, e.g. `network.state`).
    pub iface: &'static str,
    /// Provided method names in call order.
    pub methods: &'static [&'static str],
    /// Backing Core capability heads (never empty: unbacked is rejected).
    pub heads: &'static [&'static str],
}

impl ProviderInterface {
    /// Owner-qualified reference (`owner:iface@version`).
    #[must_use]
    pub fn qualified(&self) -> String {
        format!(
            "{PROVIDER_OWNER}:{}@{PROVIDER_INTERFACE_VERSION}",
            self.iface
        )
    }
}

/// Network link + bandwidth (read-only; control explicitly out).
pub const NETWORK_STATE_INTERFACE: ProviderInterface = ProviderInterface {
    iface: "network.state",
    methods: &["get"],
    heads: &[NETWORK_READ_HEAD],
};

/// Wi-Fi identity (separately-granted read sub-interface; absent NM or
/// absent grant both surface as "no essid", rest unaffected).
pub const NETWORK_ESSID_INTERFACE: ProviderInterface = ProviderInterface {
    iface: "network.essid",
    methods: &["get"],
    heads: &[NETWORK_ESSID_HEAD],
};

/// Bluetooth radio + bonded-device enumeration (bounded read).
pub const BLUETOOTH_STATE_INTERFACE: ProviderInterface = ProviderInterface {
    iface: "bluetooth.state",
    methods: &["get"],
    heads: &[BLUETOOTH_READ_HEAD],
};

/// Bluetooth radio power (separate control head; never smuggled into read).
pub const BLUETOOTH_CONTROL_INTERFACE: ProviderInterface = ProviderInterface {
    iface: "bluetooth.control",
    methods: &["set_power"],
    heads: &[BLUETOOTH_CONTROL_HEAD],
};

/// Backlight brightness (unprivileged read).
pub const BACKLIGHT_STATE_INTERFACE: ProviderInterface = ProviderInterface {
    iface: "backlight.state",
    methods: &["get"],
    heads: &[BACKLIGHT_READ_HEAD],
};

/// Backlight stepping (privilege-decided control: attempt only when
/// permitted, `E_NOT_PERMITTED` otherwise, never escalate, never setuid).
pub const BACKLIGHT_CONTROL_INTERFACE: ProviderInterface = ProviderInterface {
    iface: "backlight.control",
    methods: &["set_step"],
    heads: &[BACKLIGHT_CONTROL_HEAD],
};

/// Idle-inhibit state (compositor-side read).
pub const IDLE_STATE_INTERFACE: ProviderInterface = ProviderInterface {
    iface: "idle.state",
    methods: &["get"],
    heads: &[IDLE_READ_HEAD],
};

/// Idle-inhibit toggle (separate control head).
pub const IDLE_CONTROL_INTERFACE: ProviderInterface = ProviderInterface {
    iface: "idle.control",
    methods: &["set"],
    heads: &[IDLE_CONTROL_HEAD],
};

/// Every provider interface in this slice, in registry order.
pub const PROVIDER_INTERFACES: &[ProviderInterface] = &[
    NETWORK_STATE_INTERFACE,
    NETWORK_ESSID_INTERFACE,
    BLUETOOTH_STATE_INTERFACE,
    BLUETOOTH_CONTROL_INTERFACE,
    BACKLIGHT_STATE_INTERFACE,
    BACKLIGHT_CONTROL_INTERFACE,
    IDLE_STATE_INTERFACE,
    IDLE_CONTROL_INTERFACE,
];

/// Whether a head is a control head (grants mutation authority).
#[must_use]
pub fn head_is_control(head: &str) -> bool {
    head == BLUETOOTH_CONTROL_HEAD || head == BACKLIGHT_CONTROL_HEAD || head == IDLE_CONTROL_HEAD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_iface_shape(iface: &str) {
        assert!(!iface.is_empty(), "interface must not be empty");
        assert!(
            iface.len() <= MAX_IFACE_BYTES,
            "interface '{iface}' exceeds {MAX_IFACE_BYTES} bytes"
        );
        assert!(
            !iface.contains('\0') && !iface.contains(' ') && !iface.contains(':'),
            "interface '{iface}' must not contain NUL, space, or ':'"
        );
        for segment in iface.split('.') {
            assert!(
                !segment.is_empty(),
                "interface '{iface}' has an empty segment"
            );
            assert!(
                segment.len() <= MAX_IFACE_SEGMENT_BYTES,
                "segment of '{iface}' exceeds {MAX_IFACE_SEGMENT_BYTES} bytes"
            );
        }
    }

    #[test]
    fn every_interface_has_a_valid_short_shape() {
        assert_eq!(PROVIDER_INTERFACES.len(), 8);
        for descriptor in PROVIDER_INTERFACES {
            assert_iface_shape(descriptor.iface);
        }
    }

    #[test]
    fn qualified_names_are_unique_owner_iface_at_version() {
        let mut seen = BTreeSet::new();
        for descriptor in PROVIDER_INTERFACES {
            let qualified = descriptor.qualified();
            assert!(
                qualified.starts_with("bitty:"),
                "qualified '{qualified}' must carry the Core owner"
            );
            assert!(
                qualified.ends_with("@1.0.0"),
                "qualified '{qualified}' must pin the batch version"
            );
            assert!(
                seen.insert(qualified.clone()),
                "duplicate interface '{qualified}'"
            );
        }
    }

    #[test]
    fn every_interface_is_backed_and_bounded() {
        for descriptor in PROVIDER_INTERFACES {
            assert!(
                !descriptor.heads.is_empty(),
                "interface '{}' is unbacked (dynapi 2/4 rejects unbacked provides)",
                descriptor.iface
            );
            assert!(
                !descriptor.methods.is_empty()
                    && descriptor.methods.len() <= MAX_METHODS_PER_INTERFACE,
                "interface '{}' must name 1..={} methods",
                descriptor.iface,
                MAX_METHODS_PER_INTERFACE
            );
            for head in descriptor.heads {
                assert!(
                    PROVIDER_HEADS.contains(head),
                    "interface '{}' names an unregistered head '{head}'",
                    descriptor.iface
                );
            }
        }
    }

    #[test]
    fn read_and_control_are_never_smuggled() {
        for descriptor in PROVIDER_INTERFACES {
            let reads = descriptor
                .heads
                .iter()
                .filter(|h| !head_is_control(h))
                .count();
            let controls = descriptor
                .heads
                .iter()
                .filter(|h| head_is_control(h))
                .count();
            assert!(
                reads == 0 || controls == 0,
                "interface '{}' mixes read and control heads",
                descriptor.iface
            );
        }
        // Every control interface has a read sibling for the same device.
        for control_iface in ["bluetooth.control", "backlight.control", "idle.control"] {
            let stem = control_iface.split('.').next().expect("stem");
            assert!(
                PROVIDER_INTERFACES
                    .iter()
                    .any(|d| d.iface == format!("{stem}.state")),
                "control '{control_iface}' has no read sibling"
            );
        }
    }

    #[test]
    fn network_is_read_only_with_separate_essid_grant() {
        assert_eq!(NETWORK_STATE_INTERFACE.methods, &["get"]);
        assert_eq!(NETWORK_STATE_INTERFACE.heads, &[NETWORK_READ_HEAD]);
        assert_eq!(NETWORK_ESSID_INTERFACE.methods, &["get"]);
        assert_eq!(NETWORK_ESSID_INTERFACE.heads, &[NETWORK_ESSID_HEAD]);
        assert_ne!(NETWORK_READ_HEAD, NETWORK_ESSID_HEAD);
    }

    #[test]
    fn grant_set_is_deny_by_default_and_deduped() {
        let empty = GrantSet::default();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(!empty.contains(NETWORK_READ_HEAD));
        let granted = GrantSet::new(vec![
            NETWORK_READ_HEAD.to_string(),
            NETWORK_READ_HEAD.to_string(),
            NETWORK_ESSID_HEAD.to_string(),
        ]);
        assert_eq!(granted.len(), 2);
        assert!(granted.contains(NETWORK_READ_HEAD));
        assert!(granted.contains(NETWORK_ESSID_HEAD));
        assert!(!granted.contains(BLUETOOTH_READ_HEAD));
        // Heads never imply each other: base read does not grant ESSID,
        // read never grants control.
        let read_only = GrantSet::new(vec![BLUETOOTH_READ_HEAD.to_string()]);
        assert!(!read_only.contains(BLUETOOTH_CONTROL_HEAD));
    }

    #[test]
    fn version_is_exact_semver() {
        let parts: Vec<&str> = PROVIDER_INTERFACE_VERSION.split('.').collect();
        assert_eq!(parts.len(), 3);
        for part in parts {
            assert!(part.parse::<u64>().is_ok(), "non-numeric version part");
        }
    }
}
