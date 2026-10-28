//! Per-component-family wire protocol selection (bitty#2777).
//!
//! `bitty-component.toml` declares the wire protocol range a component
//! speaks. Core must match that range against the protocol the *host seam for
//! that component family* implements, not against one global constant:
//!
//! - `bitty-network-wire` v1 is the shared install/inventory ABI and the
//!   protocol spoken by the generic broker. `graphics`, `net`, `storage`, and
//!   `a11y` speak it (their pinned codecs are the same v1 contract).
//! - The `execution` family advances independently on the
//!   `bitty-execution` v2 codec and is hosted by
//!   [`crate::execution_component`], not by the v1 broker.
//!
//! The table is deliberately explicit and strict. A descriptor whose range
//! does not intersect its family's seam range is refused with the typed
//! [`super::ResolveError::IncompatibleProtocol`]. A component name that is not
//! listed falls back to the shared v1 seam, so an unknown family (or an
//! unknown protocol version) still fails closed instead of matching loosely.

use crate::execution_component::{
    EXECUTION_COMPONENT_PROTOCOL_MAX, EXECUTION_COMPONENT_PROTOCOL_MIN,
};
use bitty_network_wire::PROTOCOL_VERSION as NETWORK_WIRE_PROTOCOL_VERSION;

/// The host seam that speaks one component family's wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentHostSeam {
    /// The shared native-component broker over the `bitty-network-wire` v1
    /// codec ([`super::ComponentBroker`]), also used by the storage, graphics,
    /// and accessibility adapters.
    NetworkWireV1,
    /// The dedicated execution-component host over the `bitty-execution` v2
    /// codec ([`crate::execution_component::ComponentExecutionHost`]).
    ExecutionV2,
}

impl ComponentHostSeam {
    /// Stable lowercase label used by diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NetworkWireV1 => "network-wire-v1",
            Self::ExecutionV2 => "execution-v2",
        }
    }

    /// Lowest wire protocol version this seam implements.
    #[must_use]
    pub const fn protocol_min(self) -> u16 {
        match self {
            Self::NetworkWireV1 => NETWORK_WIRE_PROTOCOL_VERSION,
            Self::ExecutionV2 => EXECUTION_COMPONENT_PROTOCOL_MIN,
        }
    }

    /// Highest wire protocol version this seam implements.
    #[must_use]
    pub const fn protocol_max(self) -> u16 {
        match self {
            Self::NetworkWireV1 => NETWORK_WIRE_PROTOCOL_VERSION,
            Self::ExecutionV2 => EXECUTION_COMPONENT_PROTOCOL_MAX,
        }
    }

    /// Whether a descriptor's inclusive `[min, max]` range and this seam's
    /// inclusive range share at least one protocol version.
    ///
    /// Both ranges are validated (`1 <= min <= max`) before they reach this
    /// check: the descriptor parser enforces its side, and seam ranges are
    /// compile-time constants from the owning host.
    #[must_use]
    pub const fn accepts(self, descriptor_min: u16, descriptor_max: u16) -> bool {
        descriptor_min <= self.protocol_max() && descriptor_max >= self.protocol_min()
    }
}

/// One row of the component-family protocol table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentFamily {
    /// Install-directory and descriptor name (`[a-z][a-z0-9-]{0,31}`).
    pub name: &'static str,
    /// Host seam that speaks this family's wire protocol.
    pub seam: ComponentHostSeam,
}

/// The closed component-family protocol table.
///
/// Only listed names select a non-default seam. `graphics`, `net`, `storage`,
/// and `a11y` are listed explicitly so the table is the single place that
/// records their v1 contract; `execution` selects the v2 seam. Adding a family
/// means adding a row here and wiring the seam that implements it; the
/// install/inventory gate reads this table and needs no second change.
pub const COMPONENT_FAMILIES: &[ComponentFamily] = &[
    ComponentFamily {
        name: "graphics",
        seam: ComponentHostSeam::NetworkWireV1,
    },
    ComponentFamily {
        name: "net",
        seam: ComponentHostSeam::NetworkWireV1,
    },
    ComponentFamily {
        name: "storage",
        seam: ComponentHostSeam::NetworkWireV1,
    },
    ComponentFamily {
        name: "a11y",
        seam: ComponentHostSeam::NetworkWireV1,
    },
    ComponentFamily {
        name: "execution",
        seam: ComponentHostSeam::ExecutionV2,
    },
];

/// Seam used for every component name the table does not list.
///
/// The shared v1 wire is the install/inventory ABI, so the default stays
/// strict: a component declaring a range that excludes v1 is refused.
pub const DEFAULT_COMPONENT_HOST_SEAM: ComponentHostSeam = ComponentHostSeam::NetworkWireV1;

/// The table row for `name`, if the family is listed.
#[must_use]
pub fn family_for(name: &str) -> Option<&'static ComponentFamily> {
    COMPONENT_FAMILIES.iter().find(|family| family.name == name)
}

/// The host seam that must speak `name`'s wire protocol.
///
/// Unlisted names resolve to [`DEFAULT_COMPONENT_HOST_SEAM`].
#[must_use]
pub fn seam_for(name: &str) -> ComponentHostSeam {
    family_for(name).map_or(DEFAULT_COMPONENT_HOST_SEAM, |family| family.seam)
}

/// Whether `name`'s family seam and the descriptor range share a version.
#[must_use]
pub fn family_supports_protocol(name: &str, descriptor_min: u16, descriptor_max: u16) -> bool {
    seam_for(name).accepts(descriptor_min, descriptor_max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_keeps_the_v1_families_on_the_shared_wire() {
        for name in ["graphics", "net", "storage", "a11y"] {
            let family = family_for(name).unwrap_or_else(|| panic!("{name} must be listed"));
            assert_eq!(family.seam, ComponentHostSeam::NetworkWireV1, "{name}");
            assert_eq!(family.seam.protocol_min(), 1, "{name}");
            assert_eq!(family.seam.protocol_max(), 1, "{name}");
        }
    }

    #[test]
    fn execution_is_the_only_v2_family() {
        let family = family_for("execution").expect("execution must be listed");
        assert_eq!(family.seam, ComponentHostSeam::ExecutionV2);
        assert_eq!(family.seam.protocol_min(), 2);
        assert_eq!(family.seam.protocol_max(), 2);
        let v2: Vec<&str> = COMPONENT_FAMILIES
            .iter()
            .filter(|family| family.seam == ComponentHostSeam::ExecutionV2)
            .map(|family| family.name)
            .collect();
        assert_eq!(v2, ["execution"]);
    }

    #[test]
    fn unknown_names_fall_back_to_the_strict_v1_seam() {
        assert!(family_for("mystery").is_none());
        assert_eq!(seam_for("mystery"), ComponentHostSeam::NetworkWireV1);
        assert!(!family_supports_protocol("mystery", 2, 2));
        assert!(family_supports_protocol("mystery", 1, 1));
        // An unlisted protocol version fails closed as well.
        assert!(!family_supports_protocol("mystery", 3, 4));
    }

    #[test]
    fn seam_acceptance_is_range_overlap() {
        let v1 = ComponentHostSeam::NetworkWireV1;
        let v2 = ComponentHostSeam::ExecutionV2;
        // Exact family ranges.
        assert!(v1.accepts(1, 1));
        assert!(!v1.accepts(2, 2));
        assert!(v2.accepts(2, 2));
        assert!(!v2.accepts(1, 1));
        // Widened ranges are accepted only when they overlap the seam.
        assert!(v1.accepts(1, 2));
        assert!(v2.accepts(1, 2));
        assert!(!v1.accepts(2, 3));
        assert!(!v2.accepts(3, 3));
    }
}
