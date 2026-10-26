//! Device-state provider host wiring for bar modules (`CTX-1201`).
//!
//! Publishes the merged Phase B privilege-sensitive interfaces (`CTX-1197`,
//! bitty#2012 network, bitty#2017 bluetooth, bitty#2020 backlight plus
//! idle-inhibit) through the dynapi broker with the Lua bridge end to end.
//! Bar-side modules stay in `bitty-plugins` (follow-up, noted only); this
//! module proves the host publication plus dispatch hermetically so wiring
//! changes no contract.
//!
//! # Interfaces (`owner:iface@version`, short names below)
//!
//! One native component ([`DEVICE_PROVIDER`]) serves eight interfaces at
//! [`DEVICE_INTERFACE_VERSION`]:
//!
//! | Interface | Method | Backing broker head | Platform head | Ceiling |
//! |---|---|---|---|
//! | `network.state` | `get()` | `platform.network.read` | same | 3 s |
//! | `network.essid` | `get()` | `platform.essid.read` | `platform.network.essid.read` (alias) | 3 s |
//! | `bluetooth.state` | `get()` | `platform.bluetooth.read` | same | 5 s |
//! | `bluetooth.control` | `set_power(on)` | `platform.bluetooth.control` | same | per-call |
//! | `backlight.state` | `get()` | `platform.backlight.read` | same | 5 s |
//! | `backlight.control` | `set_step(delta)` | `platform.backlight.control` | same | per-call |
//! | `idle.state` | `get()` | `platform.idle.read` | same | 5 s |
//! | `idle.control` | `set(on)` | `platform.idle.control` | same | per-call |
//!
//! Every `get()` returns a table carrying `stale_age_ms`. Thresholds, icon
//! buckets, and toggle semantics stay in Lua policy; providers return raw
//! numbers/states only. Consumers declare the short interface in
//! `services.required` with a version range and resolve through the standard
//! dynapi broker (arbitration bitty#2003, gating bitty#2005, negotiation
//! bitty#2006): highest compatible version wins, ungranted consumption fails
//! closed as if no provider existed, and an ambiguous short fails closed
//! naming each owner.
//!
//! # ESSID alias (grammar-bound deviation from the `#2056` proposal)
//!
//! The `#2056` registry mints the ESSID head as
//! `platform.network.essid.read` (four dot segments). The closed capability
//! grammar admits only two or three segments
//! (`family.resource[.scope]`), so the four-segment spelling cannot enter
//! the broker set. The broker head is therefore [`ESSID_BROKER_HEAD`]
//! (`platform.essid.read`, three segments); the bridge maps it onto the
//! platform registry head on every call. No authority changes: the ESSID
//! sub-grant stays separate, deny-by-default, and ungranted stays
//! indistinguishable from NM-less (no `essid` key, rest unaffected).
//!
//! # Live-source posture (no new dependency edge)
//!
//! Live link/counter/backlight reads use only `std::fs` (existing seam):
//! `/sys/class/net`, `/proc/net/dev`, `/proc/net/route`, and
//! `/sys/class/backlight`. Wi-Fi identity (NetworkManager D-Bus), BlueZ
//! reads, and the Wayland idle-inhibit binding stay fail-closed stubs in
//! this slice (hide, never fabricate); their live bindings are explicit
//! follow-ups with their own `cargo-deny` plus W-147 justification. No new
//! production dependency edge is introduced (`std` plus the existing
//! `bitty-platform` and `bitty-lua` value types only).
//!
//! # Trust posture
//!
//! Read vs control never share an interface; every interface is backed
//! (unbacked rejected by test). Control dispatches re-check the control
//! head per call and honor `E_NOT_PERMITTED` without touching the backend;
//! backlight set additionally honors the `DEC-0115` permission decision
//! (attempt only when permitted, never escalate, never setuid). BlueZ paths
//! stay scoped (`/org/bluez/...` only) with bounded enumeration (16 entries,
//! true total plus explicit truncated flag). Stale-then-hide is preserved
//! from the platform samplers on every read path.

#![forbid(unsafe_code)]

pub mod bluetooth;
pub mod host;
pub mod network;
pub mod power;

pub use bluetooth::{BluetoothBridge, MemBluetoothBackend, MemBluetoothSource, NoBluetoothSource};
pub use host::{DEVICE_GENERATION, DeviceHost, MemEssidSource};
pub use network::{NetworkBridge, NoEssid, ProcNetDevCounters, SysfsNetLink};
pub use power::{
    BacklightBridge, IdleBridge, MemBacklightBackend, MemIdleBackend, MemIdleSource, NoIdleSource,
    SysfsBacklightBackend, SysfsBacklightSource,
};

use bitty_plugin_host::ProvidedService;

/// Native device component publishing the batch-B interfaces.
pub const DEVICE_PROVIDER: &str = "bitty-terminal.devices";

/// Interface version served by this revision (exact SemVer, matches the
/// platform batch version).
pub const DEVICE_INTERFACE_VERSION: &str = "1.0.0";

/// Short interface name for network link plus bandwidth
/// (`get() -> {link, down_bps?, up_bps?, stale_age_ms}`; ESSID never here).
pub const NETWORK_STATE_IFACE: &str = "network.state";

/// Short interface name for Wi-Fi identity
/// (`get() -> {essid?, stale_age_ms}`; separately granted sub-interface).
pub const NETWORK_ESSID_IFACE: &str = "network.essid";

/// Short interface name for Bluetooth radio plus bonded devices
/// (`get() -> {powered, connected_count, total_count, devices, truncated, stale_age_ms}`).
pub const BLUETOOTH_STATE_IFACE: &str = "bluetooth.state";

/// Short interface name for Bluetooth radio power (`set_power(on)`).
pub const BLUETOOTH_CONTROL_IFACE: &str = "bluetooth.control";

/// Short interface name for backlight brightness
/// (`get() -> {pct, max, stale_age_ms}`).
pub const BACKLIGHT_STATE_IFACE: &str = "backlight.state";

/// Short interface name for backlight stepping (`set_step(delta)`).
pub const BACKLIGHT_CONTROL_IFACE: &str = "backlight.control";

/// Short interface name for idle-inhibit state (`get() -> {inhibited, stale_age_ms}`).
pub const IDLE_STATE_IFACE: &str = "idle.state";

/// Short interface name for idle-inhibit toggle (`set(on)`).
pub const IDLE_CONTROL_IFACE: &str = "idle.control";

/// Backing broker head for [`NETWORK_STATE_IFACE`].
pub const NETWORK_READ_BROKER_HEAD: &str = "platform.network.read";

/// Backing broker head for [`NETWORK_ESSID_IFACE`] (grammar-bound alias;
/// maps onto the platform `platform.network.essid.read` head in the bridge).
pub const ESSID_BROKER_HEAD: &str = "platform.essid.read";

/// Backing broker head for [`BLUETOOTH_STATE_IFACE`].
pub const BLUETOOTH_READ_BROKER_HEAD: &str = "platform.bluetooth.read";

/// Backing broker head for [`BLUETOOTH_CONTROL_IFACE`] (`set_power` only).
pub const BLUETOOTH_CONTROL_BROKER_HEAD: &str = "platform.bluetooth.control";

/// Backing broker head for [`BACKLIGHT_STATE_IFACE`].
pub const BACKLIGHT_READ_BROKER_HEAD: &str = "platform.backlight.read";

/// Backing broker head for [`BACKLIGHT_CONTROL_IFACE`] (`set_step` only).
pub const BACKLIGHT_CONTROL_BROKER_HEAD: &str = "platform.backlight.control";

/// Backing broker head for [`IDLE_STATE_IFACE`].
pub const IDLE_READ_BROKER_HEAD: &str = "platform.idle.read";

/// Backing broker head for [`IDLE_CONTROL_IFACE`] (`set` only).
pub const IDLE_CONTROL_BROKER_HEAD: &str = "platform.idle.control";

/// Manifest descriptor for one device interface (backing head).
///
/// Version and heads come from this definition, never from Lua; the broker
/// records them at publication (dynapi gating, bitty#2005). Method names
/// (`get`, `set_power`, `set_step`, `set`) are fixed per interface in the
/// table above and recorded on the broker `ServiceRecord` at publication.
#[must_use]
pub fn device_descriptor(iface: &str) -> ProvidedService {
    let head = match iface {
        NETWORK_STATE_IFACE => NETWORK_READ_BROKER_HEAD,
        NETWORK_ESSID_IFACE => ESSID_BROKER_HEAD,
        BLUETOOTH_STATE_IFACE => BLUETOOTH_READ_BROKER_HEAD,
        BLUETOOTH_CONTROL_IFACE => BLUETOOTH_CONTROL_BROKER_HEAD,
        BACKLIGHT_STATE_IFACE => BACKLIGHT_READ_BROKER_HEAD,
        BACKLIGHT_CONTROL_IFACE => BACKLIGHT_CONTROL_BROKER_HEAD,
        IDLE_STATE_IFACE => IDLE_READ_BROKER_HEAD,
        IDLE_CONTROL_IFACE => IDLE_CONTROL_BROKER_HEAD,
        _ => iface,
    };
    ProvidedService {
        iface: iface.to_string(),
        version: DEVICE_INTERFACE_VERSION.to_string(),
        args_schema: None,
        result_schema: None,
        heads: vec![head.to_string()],
    }
}

/// Method names published for one device interface, in call order.
///
/// Mirrors the platform registry (`providers.rs`): read interfaces serve
/// `get`; control interfaces serve their single mutation.
#[must_use]
pub fn device_methods(iface: &str) -> Vec<String> {
    match iface {
        BLUETOOTH_CONTROL_IFACE => vec!["set_power".to_string()],
        BACKLIGHT_CONTROL_IFACE => vec!["set_step".to_string()],
        IDLE_CONTROL_IFACE => vec!["set".to_string()],
        _ => vec!["get".to_string()],
    }
}

/// All eight batch-B descriptors in interface order.
#[must_use]
pub fn device_descriptors() -> Vec<ProvidedService> {
    vec![
        device_descriptor(NETWORK_STATE_IFACE),
        device_descriptor(NETWORK_ESSID_IFACE),
        device_descriptor(BLUETOOTH_STATE_IFACE),
        device_descriptor(BLUETOOTH_CONTROL_IFACE),
        device_descriptor(BACKLIGHT_STATE_IFACE),
        device_descriptor(BACKLIGHT_CONTROL_IFACE),
        device_descriptor(IDLE_STATE_IFACE),
        device_descriptor(IDLE_CONTROL_IFACE),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::{Rc, Weak};

    use bitty_lua::{HostServices, LuaValue};
    use bitty_plugin_host::{parse_service_reference, service_version_satisfies};

    use crate::plugin_runtime::services::{PluginServices, ServiceDirectory, ServiceRecord};
    use crate::plugin_runtime::store::PluginStore;

    struct EmptySettings;

    impl crate::plugin_runtime::services::SettingsSource for EmptySettings {
        fn get(&self, _key: &str) -> Option<LuaValue> {
            None
        }
    }

    struct EmptySnapshot;

    impl crate::plugin_runtime::services::SnapshotSource for EmptySnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, bitty_lua::BridgeError> {
            Err(bitty_lua::BridgeError::not_implemented("snapshot"))
        }
    }

    fn bar_host() -> PluginServices {
        PluginServices::new(
            "xuepoo.bar",
            PluginStore::in_memory(),
            Rc::new(EmptySettings),
            Rc::new(EmptySnapshot),
            Rc::new(RefCell::new(
                crate::plugin_runtime::services::NotificationQueue::new(8),
            )),
            false,
            false,
        )
    }

    fn publish_native(
        directory: &Rc<RefCell<ServiceDirectory>>,
        provider: &str,
        iface: &str,
        version: &str,
        heads: &[&str],
        methods: &[&str],
    ) {
        directory.borrow_mut().publish(ServiceRecord {
            provider: provider.to_string(),
            generation: 1,
            iface: iface.to_string(),
            version: version.to_string(),
            methods: methods.iter().map(|m| (*m).to_string()).collect(),
            funcs: BTreeMap::new(),
            args_schema: None,
            result_schema: None,
            heads: heads.iter().map(|h| (*h).to_string()).collect(),
            vm: Weak::new(),
            suspended: false,
        });
    }

    fn publish_all(directory: &Rc<RefCell<ServiceDirectory>>) {
        for service in device_descriptors() {
            let methods = device_methods(&service.iface);
            publish_native(
                directory,
                DEVICE_PROVIDER,
                &service.iface,
                &service.version,
                &service.heads.iter().map(String::as_str).collect::<Vec<_>>(),
                &methods.iter().map(String::as_str).collect::<Vec<_>>(),
            );
        }
    }

    #[test]
    fn descriptors_validate_and_name_short_interfaces() {
        for service in device_descriptors() {
            service.validate().expect("descriptor validates");
            let (owner, short) = parse_service_reference(&service.iface, "services.get")
                .expect("descriptor reference parses");
            assert!(owner.is_none(), "provide-side names the short form");
            assert!(!short.starts_with("bitty."), "never Core-reserved");
            assert!(!service.heads.is_empty(), "every interface is backed");
        }
        assert_eq!(device_descriptors().len(), 8);
    }

    #[test]
    fn descriptors_pin_owner_version_and_heads() {
        let network = device_descriptor(NETWORK_STATE_IFACE);
        assert_eq!(network.version, DEVICE_INTERFACE_VERSION);
        assert_eq!(network.heads, vec![NETWORK_READ_BROKER_HEAD.to_string()]);
        assert!(service_version_satisfies(&network.version, "^1.0"));
        let essid = device_descriptor(NETWORK_ESSID_IFACE);
        assert_eq!(essid.heads, vec![ESSID_BROKER_HEAD.to_string()]);
        assert_ne!(
            NETWORK_READ_BROKER_HEAD, ESSID_BROKER_HEAD,
            "ESSID stays a separate grant"
        );
        let bt_control = device_descriptor(BLUETOOTH_CONTROL_IFACE);
        assert_eq!(
            bt_control.heads,
            vec![BLUETOOTH_CONTROL_BROKER_HEAD.to_string()]
        );
        assert_eq!(
            device_methods(BLUETOOTH_CONTROL_IFACE),
            vec!["set_power".to_string()]
        );
        assert_eq!(
            device_methods(BACKLIGHT_CONTROL_IFACE),
            vec!["set_step".to_string()]
        );
        assert_eq!(device_methods(IDLE_CONTROL_IFACE), vec!["set".to_string()]);
        assert_eq!(device_methods(NETWORK_STATE_IFACE), vec!["get".to_string()]);
    }

    #[test]
    fn read_and_control_are_never_smuggled() {
        for service in device_descriptors() {
            let is_control = service.iface.ends_with(".control");
            let is_state = service.iface.ends_with(".state") || service.iface.ends_with(".essid");
            assert!(
                is_control ^ is_state,
                "interface '{}' must be read or control, never both",
                service.iface
            );
        }
        for control_iface in [
            BLUETOOTH_CONTROL_IFACE,
            BACKLIGHT_CONTROL_IFACE,
            IDLE_CONTROL_IFACE,
        ] {
            let stem = control_iface.split('.').next().expect("stem");
            let sibling = if stem == "idle" {
                IDLE_STATE_IFACE
            } else if stem == "bluetooth" {
                BLUETOOTH_STATE_IFACE
            } else {
                BACKLIGHT_STATE_IFACE
            };
            assert!(
                device_descriptors().iter().any(|d| d.iface == sibling),
                "control '{control_iface}' has a read sibling"
            );
        }
    }

    #[test]
    fn granted_bar_resolves_devices_ungranted_matches_absent() {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        publish_all(&directory);
        let host = bar_host();
        host.set_service_directory(directory);
        host.set_service_manifest(
            Vec::new(),
            device_descriptors()
                .iter()
                .map(|s| (s.iface.clone(), "^1.0".to_string()))
                .chain([("device.missing".to_string(), "^1.0".to_string())])
                .collect(),
        );
        host.set_granted_capabilities(
            device_descriptors()
                .iter()
                .flat_map(|s| s.heads.clone())
                .collect(),
        );
        for service in device_descriptors() {
            let route = host
                .service_resolve(&service.iface, None, false)
                .expect("granted resolves")
                .expect("non-optional route");
            assert_eq!(route.provider, DEVICE_PROVIDER);
            assert_eq!(route.version, DEVICE_INTERFACE_VERSION);
        }
        host.set_granted_capabilities(Vec::new());
        let mut codes = Vec::new();
        for iface in device_descriptors()
            .iter()
            .map(|s| s.iface.clone())
            .chain(["device.missing".to_string()])
        {
            let error = host
                .service_resolve(&iface, None, false)
                .expect_err("ungranted or absent must fail");
            assert_eq!(error.code, "E_SERVICE_RESOLUTION");
            codes.push(error.code);
        }
        assert!(codes.iter().all(|code| *code == "E_SERVICE_RESOLUTION"));
    }

    #[test]
    fn essid_grant_gates_essid_only_rest_unaffected() {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        publish_all(&directory);
        let host = bar_host();
        host.set_service_directory(directory);
        host.set_service_manifest(
            Vec::new(),
            vec![
                (NETWORK_STATE_IFACE.to_string(), "^1.0".to_string()),
                (NETWORK_ESSID_IFACE.to_string(), "^1.0".to_string()),
            ],
        );
        host.set_granted_capabilities(vec![NETWORK_READ_BROKER_HEAD.to_string()]);
        let base = host
            .service_resolve(NETWORK_STATE_IFACE, None, false)
            .expect("base resolves")
            .expect("route");
        assert_eq!(base.provider, DEVICE_PROVIDER);
        let essid_missing = host.service_resolve(NETWORK_ESSID_IFACE, None, false);
        let error = essid_missing.expect_err("ungranted essid matches absent");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        host.set_granted_capabilities(vec![
            NETWORK_READ_BROKER_HEAD.to_string(),
            ESSID_BROKER_HEAD.to_string(),
        ]);
        let essid = host
            .service_resolve(NETWORK_ESSID_IFACE, None, false)
            .expect("granted essid resolves")
            .expect("route");
        assert_eq!(essid.provider, DEVICE_PROVIDER);
    }

    #[test]
    fn negotiation_serves_highest_compatible_never_downgrade() {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        publish_native(
            &directory,
            DEVICE_PROVIDER,
            NETWORK_STATE_IFACE,
            "1.0.0",
            &[NETWORK_READ_BROKER_HEAD],
            &["get"],
        );
        publish_native(
            &directory,
            DEVICE_PROVIDER,
            NETWORK_STATE_IFACE,
            "1.1.0",
            &[NETWORK_READ_BROKER_HEAD],
            &["get"],
        );
        let host = bar_host();
        host.set_service_directory(directory);
        host.set_service_manifest(
            vec![],
            vec![(NETWORK_STATE_IFACE.to_string(), "^1.0".to_string())],
        );
        host.set_granted_capabilities(vec![NETWORK_READ_BROKER_HEAD.to_string()]);
        let route = host
            .service_resolve(NETWORK_STATE_IFACE, None, false)
            .expect("resolves")
            .expect("route");
        assert_eq!(route.version, "1.1.0", "highest compatible wins");
        let incompatible = host.service_resolve(NETWORK_STATE_IFACE, Some(">=2.0"), false);
        assert!(incompatible.is_err());
    }

    #[test]
    fn owner_qualified_pins_provider_short_ambiguity_names_owners() {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        publish_native(
            &directory,
            DEVICE_PROVIDER,
            NETWORK_STATE_IFACE,
            "1.0.0",
            &[NETWORK_READ_BROKER_HEAD],
            &["get"],
        );
        publish_native(
            &directory,
            "xuepoo.other",
            NETWORK_STATE_IFACE,
            "1.0.0",
            &[NETWORK_READ_BROKER_HEAD],
            &["get"],
        );
        let host = bar_host();
        host.set_service_directory(directory);
        host.set_service_manifest(
            vec![],
            vec![
                (NETWORK_STATE_IFACE.to_string(), "^1.0".to_string()),
                (
                    format!("{DEVICE_PROVIDER}:{NETWORK_STATE_IFACE}"),
                    "^1.0".to_string(),
                ),
            ],
        );
        host.set_granted_capabilities(vec![NETWORK_READ_BROKER_HEAD.to_string()]);
        let short = host.service_resolve(NETWORK_STATE_IFACE, None, false);
        let error = short.expect_err("ambiguous short must fail");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert!(error.message.contains(DEVICE_PROVIDER));
        assert!(error.message.contains("xuepoo.other"));
        let qualified = host
            .service_resolve(
                &format!("{DEVICE_PROVIDER}:{NETWORK_STATE_IFACE}"),
                None,
                false,
            )
            .expect("qualified resolves")
            .expect("route");
        assert_eq!(qualified.provider, DEVICE_PROVIDER);
    }

    /// Chassis-equivalent `{key}` substitution (bounded, pure function).
    fn render_template(template: &str, values: &[(&str, String)]) -> String {
        let mut out = template.to_string();
        for (key, value) in values {
            out = out.replace(&format!("{{{key}}}"), value);
        }
        out
    }

    #[test]
    fn network_snapshot_renders_through_chassis_template() {
        use crate::plugin_runtime::device_providers::network::network_snapshot_to_lua;
        use bitty_platform::network_state::{LinkKind, NetworkSnapshot};
        let snapshot = NetworkSnapshot {
            link: LinkKind::Wifi,
            down_bps: Some(8_000),
            up_bps: Some(4_000),
            essid: Some("Home".to_string()),
            stale_age_ms: 0,
        };
        let value = network_snapshot_to_lua(&snapshot);
        let link = match value.get("link") {
            Some(LuaValue::String(s)) => s.clone(),
            other => panic!("link must be a string, got {other:?}"),
        };
        assert_eq!(render_template("{link} up", &[("link", link)]), "wifi up");
        assert!(value.get("essid").is_some());
        assert_eq!(value.get("stale_age_ms"), Some(&LuaValue::Integer(0)));
    }

    #[test]
    fn bluetooth_snapshot_counts_render_without_new_vehicle() {
        use crate::plugin_runtime::device_providers::bluetooth::bluetooth_snapshot_to_lua;
        use bitty_platform::bluetooth_state::{BluetoothDevice, BluetoothSnapshot};
        let snapshot = BluetoothSnapshot {
            powered: true,
            connected_count: 1,
            total_count: 2,
            devices: vec![
                BluetoothDevice {
                    address: "AA:BB:CC:DD:EE:01".to_string(),
                    alias: "Headphones".to_string(),
                    connected: true,
                },
                BluetoothDevice {
                    address: "AA:BB:CC:DD:EE:02".to_string(),
                    alias: "Keyboard".to_string(),
                    connected: false,
                },
            ],
            truncated: false,
            stale_age_ms: 0,
        };
        let value = bluetooth_snapshot_to_lua(&snapshot);
        let total = match value.get("total_count") {
            Some(LuaValue::Integer(v)) => *v,
            other => panic!("total_count must be an integer, got {other:?}"),
        };
        assert_eq!(
            render_template("{n} devices", &[("n", total.to_string())]),
            "2 devices"
        );
    }

    #[test]
    fn backlight_and_idle_snapshots_carry_toggle_fields() {
        use crate::plugin_runtime::device_providers::power::{
            backlight_snapshot_to_lua, idle_snapshot_to_lua,
        };
        use bitty_platform::backlight_state::{BacklightSnapshot, IdleSnapshot};
        let backlight = BacklightSnapshot {
            pct: 25,
            max: 1_200,
            stale_age_ms: 0,
        };
        let value = backlight_snapshot_to_lua(&backlight);
        assert_eq!(value.get("pct"), Some(&LuaValue::Integer(25)));
        let idle = IdleSnapshot {
            inhibited: true,
            stale_age_ms: 0,
        };
        let value = idle_snapshot_to_lua(&idle);
        assert_eq!(value.get("inhibited"), Some(&LuaValue::Bool(true)));
    }

    #[test]
    fn stale_snapshots_render_age_without_new_vehicle() {
        use crate::plugin_runtime::device_providers::power::backlight_snapshot_to_lua;
        use bitty_platform::backlight_state::BacklightSnapshot;
        let snapshot = BacklightSnapshot {
            pct: 50,
            max: 1_000,
            stale_age_ms: 4_000,
        };
        let value = backlight_snapshot_to_lua(&snapshot);
        assert_eq!(value.get("stale_age_ms"), Some(&LuaValue::Integer(4_000)));
    }
}
