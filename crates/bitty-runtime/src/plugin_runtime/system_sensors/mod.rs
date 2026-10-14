//! Read-only system-sensor provider interfaces for bar modules (CTX-1193).
//!
//! One PR covers all three Phase B batch-A providers (bitty#2019 cpu,
//! bitty#2011 memory, bitty#2016 battery) because they share the provider
//! plumbing in [`common`]: a provider-side poll ceiling with over-poll
//! coalescing, stale-then-hide past three windows, and fail-closed grants
//! where an ungranted read is indistinguishable from an absent source.
//! Nothing here fabricates zeros.
//!
//! # Interface names (`owner:iface@version`)
//!
//! The bar (bitty-plugins#86 Phase B) consumes broker-provided
//! `owner:iface@version` interfaces from native components. These three
//! are served by the native sensor component [`SENSOR_PROVIDER`] at
//! version [`SENSOR_INTERFACE_VERSION`]:
//!
//! | Interface | Method | Backing head | Ceiling |
//! |---|---|---|---|
//! | `system.cpu` | `get()` | `platform.cpu-read` | 2 s |
//! | `system.memory` | `get()` | `platform.memory-read` | 2 s |
//! | `system.battery` | `get()` | `platform.battery-read` | 5 s |
//!
//! Every `get()` returns a table carrying `stale_age_ms`. Thresholds
//! (warning/critical) and icon buckets stay in Lua policy; the provider
//! returns raw numbers/states only. Consumers declare the short interface
//! in `services.required` with a version range and resolve through the
//! standard dynapi broker (arbitration bitty#2003, gating bitty#2005,
//! negotiation bitty#2006): highest compatible version wins, ungranted
//! consumption fails closed as if no provider existed, and an ambiguous
//! short fails closed naming each owner.
//!
//! # Per-target source table
//!
//! | Target | cpu | memory | battery |
//! |---|---|---|-------|
//! | Linux, `/proc` + `/sys` present | `/proc/stat` | `/proc/meminfo` | UPower preferred, `/sys/class/power_supply` fallback |
//! | Linux, UPower absent | n/a | n/a | sysfs fallback serves under its own 5 s ceiling |
//! | Linux, source missing (container without `/proc`, sysfs-less desktop) | stale-then-hide | stale-then-hide | stale-then-hide |
//! | Non-Linux (macOS/Windows) | no source: hide | no source: hide | no source: hide |
//!
//! Rows without a source fail closed (hide), never with fabricated zeros.
//! Live host wiring (native publication into the service directory plus
//! the Lua `get()` bridge) is a separate step on top of this interface;
//! this module proves the parsing, policy, descriptors, and broker
//! compatibility hermetically so that wiring changes no contract.
//!
//! # Trust posture
//!
//! The three backing heads are read-only by construction: no control
//! method exists in this revision (backlight/idle/audio/network control
//! heads are explicitly out of scope for batch A). Providers perform no
//! network, process, or IPC access; the only filesystem reads are the
//! contract paths above plus the UPower bus name, each named in exactly
//! one place. No new production dependency edges are introduced
//! (`std` plus the existing `bitty-lua` value type only).

pub mod battery;
pub mod common;
pub mod cpu;
pub mod memory;

pub use battery::{BatteryProvider, BatterySnapshot, BatteryState, FsPowerSupply, NoUpower};
pub use common::{FAST_SENSOR_CEILING_MS, SLOW_SENSOR_CEILING_MS, STALE_HIDE_WINDOWS};
pub use cpu::{CpuProvider, CpuSnapshot};
pub use memory::{MemoryProvider, MemorySnapshot};

use bitty_plugin_host::ProvidedService;

/// Native sensor component publishing the batch-A interfaces.
pub const SENSOR_PROVIDER: &str = "bitty-terminal.sensors";

/// Interface version served by this revision (exact SemVer).
pub const SENSOR_INTERFACE_VERSION: &str = "1.0.0";

/// Short interface name for CPU state (`get() -> {total_pct, per_core, stale_age_ms}`).
pub const SYSTEM_CPU_IFACE: &str = "system.cpu";

/// Short interface name for memory state
/// (`get() -> {pct, used_gib, total_gib, swap_pct, swap_used_gib, swap_total_gib, stale_age_ms}`).
pub const SYSTEM_MEMORY_IFACE: &str = "system.memory";

/// Short interface name for battery state
/// (`get() -> {capacity_pct, state, power_w?, seconds_to_empty?, stale_age_ms}`).
pub const SYSTEM_BATTERY_IFACE: &str = "system.battery";

/// Backing capability head for [`SYSTEM_CPU_IFACE`].
pub const CPU_BACKING_HEAD: &str = "platform.cpu-read";

/// Backing capability head for [`SYSTEM_MEMORY_IFACE`].
pub const MEMORY_BACKING_HEAD: &str = "platform.memory-read";

/// Backing capability head for [`SYSTEM_BATTERY_IFACE`].
pub const BATTERY_BACKING_HEAD: &str = "platform.battery-read";

/// Manifest descriptor for one sensor interface (single `get` method).
///
/// Version, schemas, and heads come from this definition, never from Lua;
/// the broker records them at publication (dynapi gating, bitty#2005).
#[must_use]
pub fn sensor_descriptor(iface: &str) -> ProvidedService {
    let head = match iface {
        SYSTEM_CPU_IFACE => CPU_BACKING_HEAD,
        SYSTEM_MEMORY_IFACE => MEMORY_BACKING_HEAD,
        SYSTEM_BATTERY_IFACE => BATTERY_BACKING_HEAD,
        _ => iface,
    };
    ProvidedService {
        iface: iface.to_string(),
        version: SENSOR_INTERFACE_VERSION.to_string(),
        args_schema: None,
        result_schema: None,
        heads: vec![head.to_string()],
    }
}

/// All three batch-A descriptors in interface order.
#[must_use]
pub fn sensor_descriptors() -> Vec<ProvidedService> {
    vec![
        sensor_descriptor(SYSTEM_CPU_IFACE),
        sensor_descriptor(SYSTEM_MEMORY_IFACE),
        sensor_descriptor(SYSTEM_BATTERY_IFACE),
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

    // -- minimal PluginServices construction --------------------------------
    //
    // The broker-compat tests below drive the real `service_resolve` path
    // (arbitration + gating + negotiation), so they need a real
    // `PluginServices`. The store/settings/snapshot/notification arguments
    // are inert stubs: resolution never touches them.

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
    ) {
        directory.borrow_mut().publish(ServiceRecord {
            provider: provider.to_string(),
            generation: 1,
            iface: iface.to_string(),
            version: version.to_string(),
            methods: vec!["get".to_string()],
            funcs: BTreeMap::new(),
            args_schema: None,
            result_schema: None,
            heads: heads.iter().map(|h| (*h).to_string()).collect(),
            vm: Weak::new(),
            suspended: false,
        });
    }

    #[test]
    fn descriptors_validate_and_name_short_interfaces() {
        for service in sensor_descriptors() {
            service.validate().expect("descriptor validates");
            let (owner, short) = parse_service_reference(&service.iface, "services.get")
                .expect("descriptor reference parses");
            assert!(owner.is_none(), "provide-side names the short form");
            assert!(!short.starts_with("bitty."), "never Core-reserved");
            assert!(!service.heads.is_empty(), "every interface is backed");
        }
        assert_eq!(sensor_descriptors().len(), 3);
    }

    #[test]
    fn descriptors_pin_owner_version_and_heads() {
        let cpu = sensor_descriptor(SYSTEM_CPU_IFACE);
        assert_eq!(cpu.version, SENSOR_INTERFACE_VERSION);
        assert_eq!(cpu.heads, vec![CPU_BACKING_HEAD.to_string()]);
        assert!(service_version_satisfies(&cpu.version, "^1.0"));
        let memory = sensor_descriptor(SYSTEM_MEMORY_IFACE);
        assert_eq!(memory.heads, vec![MEMORY_BACKING_HEAD.to_string()]);
        let battery = sensor_descriptor(SYSTEM_BATTERY_IFACE);
        assert_eq!(battery.heads, vec![BATTERY_BACKING_HEAD.to_string()]);
    }

    #[test]
    fn granted_bar_resolves_sensors_ungranted_matches_absent() {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        for service in sensor_descriptors() {
            publish_native(
                &directory,
                SENSOR_PROVIDER,
                &service.iface,
                &service.version,
                &service.heads.iter().map(String::as_str).collect::<Vec<_>>(),
            );
        }
        let host = bar_host();
        host.set_service_directory(directory);
        host.set_service_manifest(
            Vec::new(),
            vec![
                (SYSTEM_CPU_IFACE.to_string(), "^1.0".to_string()),
                (SYSTEM_MEMORY_IFACE.to_string(), "^1.0".to_string()),
                (SYSTEM_BATTERY_IFACE.to_string(), "^1.0".to_string()),
                ("system.missing".to_string(), "^1.0".to_string()),
            ],
        );
        // Granted: every sensor resolves to the native provider.
        host.set_granted_capabilities(vec![
            BATTERY_BACKING_HEAD.to_string(),
            CPU_BACKING_HEAD.to_string(),
            MEMORY_BACKING_HEAD.to_string(),
        ]);
        for iface in [SYSTEM_CPU_IFACE, SYSTEM_MEMORY_IFACE, SYSTEM_BATTERY_IFACE] {
            let route = host
                .service_resolve(iface, None, false)
                .expect("granted resolves")
                .expect("non-optional route");
            assert_eq!(route.provider, SENSOR_PROVIDER);
            assert_eq!(route.version, SENSOR_INTERFACE_VERSION);
            assert_eq!(route.methods, vec!["get".to_string()]);
        }
        // Ungranted: resolution fails with the same code as an absent
        // interface (indistinguishable, dynapi gating bitty#2005).
        host.set_granted_capabilities(Vec::new());
        let mut codes = Vec::new();
        for iface in [
            SYSTEM_CPU_IFACE,
            SYSTEM_MEMORY_IFACE,
            SYSTEM_BATTERY_IFACE,
            "system.missing",
        ] {
            let error = host
                .service_resolve(iface, None, false)
                .expect_err("ungranted or absent must fail");
            assert_eq!(error.code, "E_SERVICE_RESOLUTION");
            codes.push(error.code);
        }
        assert!(codes.iter().all(|code| *code == "E_SERVICE_RESOLUTION"));
    }

    #[test]
    fn negotiation_serves_highest_compatible_never_downgrade() {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        publish_native(
            &directory,
            SENSOR_PROVIDER,
            SYSTEM_CPU_IFACE,
            "1.0.0",
            &[CPU_BACKING_HEAD],
        );
        publish_native(
            &directory,
            SENSOR_PROVIDER,
            SYSTEM_CPU_IFACE,
            "1.1.0",
            &[CPU_BACKING_HEAD],
        );
        let host = bar_host();
        host.set_service_directory(directory);
        host.set_service_manifest(
            vec![],
            vec![(SYSTEM_CPU_IFACE.to_string(), "^1.0".to_string())],
        );
        host.set_granted_capabilities(vec![CPU_BACKING_HEAD.to_string()]);
        let route = host
            .service_resolve(SYSTEM_CPU_IFACE, None, false)
            .expect("resolves")
            .expect("route");
        assert_eq!(route.version, "1.1.0", "highest compatible wins");
        // Removing the only compatible version fails closed (never a
        // silent downgrade to an incompatible pick).
        let incompatible = host.service_resolve(SYSTEM_CPU_IFACE, Some(">=2.0"), false);
        assert!(incompatible.is_err());
    }

    #[test]
    fn owner_qualified_pins_provider_short_ambiguity_names_owners() {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        publish_native(
            &directory,
            SENSOR_PROVIDER,
            SYSTEM_CPU_IFACE,
            "1.0.0",
            &[CPU_BACKING_HEAD],
        );
        publish_native(
            &directory,
            "xuepoo.other",
            SYSTEM_CPU_IFACE,
            "1.0.0",
            &[CPU_BACKING_HEAD],
        );
        let host = bar_host();
        host.set_service_directory(directory);
        host.set_service_manifest(
            vec![],
            vec![
                (SYSTEM_CPU_IFACE.to_string(), "^1.0".to_string()),
                (
                    format!("{SENSOR_PROVIDER}:{SYSTEM_CPU_IFACE}"),
                    "^1.0".to_string(),
                ),
            ],
        );
        host.set_granted_capabilities(vec![CPU_BACKING_HEAD.to_string()]);
        let short = host.service_resolve(SYSTEM_CPU_IFACE, None, false);
        let error = short.expect_err("ambiguous short must fail");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert!(
            error.message.contains(SENSOR_PROVIDER),
            "names {SENSOR_PROVIDER}"
        );
        assert!(error.message.contains("xuepoo.other"), "names xuepoo.other");
        let qualified = host
            .service_resolve(
                &format!("{SENSOR_PROVIDER}:{SYSTEM_CPU_IFACE}"),
                None,
                false,
            )
            .expect("qualified resolves")
            .expect("route");
        assert_eq!(qualified.provider, SENSOR_PROVIDER);
    }

    // -- bar-chassis proofs ---------------------------------------------------
    //
    // bitty-plugins#86 Phase A renders modules from `{ name, render(data),
    // tick_ms, on_click }` with a bounded `{key}` template renderer and
    // theme-token state switches. These tests feed real provider snapshots
    // through that exact shape (implemented inline as the chassis
    // equivalent: substitution plus token switch, no new UI vehicle) and
    // prove the batch-A fields suffice with zero bar-side redesign.

    /// Chassis-equivalent `{key}` substitution (bounded, pure function).
    fn render_template(template: &str, values: &[(&str, String)]) -> String {
        let mut out = template.to_string();
        for (key, value) in values {
            out = out.replace(&format!("{{{key}}}"), value);
        }
        out
    }

    #[test]
    fn cpu_snapshot_renders_through_chassis_template() {
        let snapshot = CpuSnapshot {
            total_pct: 42.5,
            per_core: vec![10.0],
            stale_age_ms: 0,
        };
        let value = snapshot.to_lua_value();
        let total = match value.get("total_pct") {
            Some(LuaValue::Number(v)) => format!("{v:.0}%"),
            other => panic!("total_pct must be a number, got {other:?}"),
        };
        assert_eq!(
            render_template("{total} cpu", &[("total", total)]),
            "42% cpu"
        );
    }

    #[test]
    fn memory_snapshot_thresholds_evaluate_lua_side() {
        // Provider returns raw numbers; the warning/critical switch is bar
        // policy over the same fields (mirrors the Phase A states->tokens
        // vehicle, no provider involvement).
        let snapshot = MemorySnapshot {
            pct: 91.0,
            used_gib: 14.5,
            total_gib: 16.0,
            swap_pct: 0.0,
            swap_used_gib: 0.0,
            swap_total_gib: 0.0,
            stale_age_ms: 0,
        };
        let value = snapshot.to_lua_value();
        let pct = match value.get("pct") {
            Some(LuaValue::Number(v)) => *v,
            other => panic!("pct must be a number, got {other:?}"),
        };
        let token = if pct >= 90.0 {
            "critical"
        } else if pct >= 75.0 {
            "warning"
        } else {
            "default"
        };
        assert_eq!(token, "critical");
        assert_eq!(
            render_template(
                "{used}/{total} GiB",
                &[("used", "14.5".to_string()), ("total", "16".to_string())]
            ),
            "14.5/16 GiB"
        );
    }

    #[test]
    fn battery_snapshot_state_selects_lua_icon_bucket() {
        // Icon buckets stay Lua-side: the provider state string indexes a
        // bar-owned array (waybar-style charging/default arrays), so a new
        // glyph set never touches the provider.
        let icons = [("charging", "󰂄"), ("default", "󰁹")];
        for (state, bucket) in [
            (BatteryState::Charging, "󰂄"),
            (BatteryState::Discharging, "󰁹"),
            (BatteryState::Full, "󰁹"),
        ] {
            let snapshot = BatterySnapshot {
                capacity_pct: 80.0,
                state,
                power_w: None,
                seconds_to_empty: None,
                stale_age_ms: 0,
            };
            let value = snapshot.to_lua_value();
            let state_str = match value.get("state") {
                Some(LuaValue::String(s)) => s.clone(),
                other => panic!("state must be a string, got {other:?}"),
            };
            let icon = if state_str == "charging" {
                icons[0].1
            } else {
                icons[1].1
            };
            assert_eq!(icon, bucket, "state {state_str}");
        }
    }

    #[test]
    fn stale_snapshots_render_age_without_new_vehicle() {
        let snapshot = CpuSnapshot {
            total_pct: 10.0,
            per_core: Vec::new(),
            stale_age_ms: 4000,
        };
        let value = snapshot.to_lua_value();
        let age = match value.get("stale_age_ms") {
            Some(LuaValue::Integer(v)) => *v,
            other => panic!("stale_age_ms must be an integer, got {other:?}"),
        };
        assert_eq!(age, 4000);
    }
}
