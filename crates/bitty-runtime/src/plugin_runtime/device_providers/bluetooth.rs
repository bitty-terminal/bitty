//! Bluetooth-state host bridge: scoped-path read plus separate control head.
//!
//! Wraps the merged `bitty-platform` sampler (`CTX-1197`, bitty#2017) with
//! the privilege paths end to end. The read path serves radio power plus a
//! bounded bonded-device list (cap 16, true total plus explicit truncated
//! flag); the control path (`set_power`) honors `E_NOT_PERMITTED` without
//! ever touching the backend when the control head is absent.
//!
//! No new D-Bus dependency in this slice (existing-seam rule): the live
//! BlueZ binding is a fail-closed stub ([`NoBluetoothSource`], present is
//! false, so the provider hides exactly like an absent adapter). Scoped
//! object paths (`/org/bluez/...` only) and bounded enumeration are enforced
//! by the platform sampler and re-pinned here; the live bus read is an
//! explicit follow-up with its own deny plus W-147 justification.

#![forbid(unsafe_code)]

use bitty_lua::{BridgeError, LuaValue};
use bitty_platform::bluetooth_state::{
    BLUEZ_OBJECT_ROOT, BluetoothClock, BluetoothControl, BluetoothControlBackend,
    BluetoothControlError, BluetoothSnapshot, BluetoothSource, BluetoothStateService,
    RawBluetoothDevice, bluez_path_in_scope,
};
use bitty_platform::providers::GrantSet;

/// Translates broker-granted heads into the platform [`GrantSet`].
#[must_use]
pub fn grants_from_broker(granted: &[String]) -> GrantSet {
    let mut heads = Vec::new();
    for head in granted {
        match head.as_str() {
            "platform.bluetooth.read" => {
                heads.push(bitty_platform::providers::BLUETOOTH_READ_HEAD.to_string());
            }
            "platform.bluetooth.control" => {
                heads.push(bitty_platform::providers::BLUETOOTH_CONTROL_HEAD.to_string());
            }
            _ => {}
        }
    }
    GrantSet::new(heads)
}

/// Marshal one served [`BluetoothSnapshot`] into the bar-facing Lua table.
///
/// Exact keys: `powered`, `connected_count`, `total_count`, `devices`
/// (array of `{address, alias, connected}`), `truncated`, `stale_age_ms`.
/// Lua owns alias/address rendering plus click-to-open-manager (the manager
/// itself is a command, never a shell).
#[must_use]
pub fn bluetooth_snapshot_to_lua(snapshot: &BluetoothSnapshot) -> LuaValue {
    let devices: Vec<LuaValue> = snapshot
        .devices
        .iter()
        .map(|device| {
            LuaValue::Table(vec![
                (
                    LuaValue::String("address".to_string()),
                    LuaValue::String(device.address.clone()),
                ),
                (
                    LuaValue::String("alias".to_string()),
                    LuaValue::String(device.alias.clone()),
                ),
                (
                    LuaValue::String("connected".to_string()),
                    LuaValue::Bool(device.connected),
                ),
            ])
        })
        .collect();
    LuaValue::Table(vec![
        (
            LuaValue::String("powered".to_string()),
            LuaValue::Bool(snapshot.powered),
        ),
        (
            LuaValue::String("connected_count".to_string()),
            LuaValue::Integer(snapshot.connected_count as i64),
        ),
        (
            LuaValue::String("total_count".to_string()),
            LuaValue::Integer(snapshot.total_count as i64),
        ),
        (
            LuaValue::String("devices".to_string()),
            LuaValue::array(devices),
        ),
        (
            LuaValue::String("truncated".to_string()),
            LuaValue::Bool(snapshot.truncated),
        ),
        (
            LuaValue::String("stale_age_ms".to_string()),
            LuaValue::Integer(snapshot.stale_age_ms as i64),
        ),
    ])
}

/// Fail-closed BlueZ stub: no adapter on the bus, so the provider hides
/// (stale-then-hide from the last good sample, then `None`).
#[derive(Debug, Default)]
pub struct NoBluetoothSource;

impl BluetoothSource for NoBluetoothSource {
    fn present(&mut self) -> bool {
        false
    }
    fn adapter_powered(&mut self) -> Option<bool> {
        None
    }
    fn devices(&mut self) -> Option<Vec<RawBluetoothDevice>> {
        None
    }
}

/// In-memory Bluetooth source for tests and headless proofs.
///
/// Every device row carries its object path; rows outside
/// [`BLUEZ_OBJECT_ROOT`] are dropped before the platform sampler (defense in
/// depth: the host must already scope to bonded paths, and the bridge
/// refuses anything else).
pub struct MemBluetoothSource {
    present: bool,
    powered: Option<bool>,
    rows: Option<Vec<(String, RawBluetoothDevice)>>,
}

impl MemBluetoothSource {
    /// Binds an in-memory adapter with path-tagged rows.
    #[must_use]
    pub fn new(
        present: bool,
        powered: Option<bool>,
        rows: Option<Vec<(String, RawBluetoothDevice)>>,
    ) -> Self {
        Self {
            present,
            powered,
            rows,
        }
    }
}

impl BluetoothSource for MemBluetoothSource {
    fn present(&mut self) -> bool {
        self.present
    }
    fn adapter_powered(&mut self) -> Option<bool> {
        self.powered
    }
    fn devices(&mut self) -> Option<Vec<RawBluetoothDevice>> {
        let rows = self.rows.clone()?;
        Some(
            rows.into_iter()
                .filter(|(path, _)| bluez_path_in_scope(path))
                .map(|(_, row)| row)
                .collect(),
        )
    }
}

/// In-memory control backend for tests (records power writes).
#[derive(Debug, Default)]
pub struct MemBluetoothBackend {
    /// Power values admitted to the backend, in call order.
    pub calls: Vec<bool>,
    /// When set, writes fail with this diagnostic.
    pub fail: Option<String>,
}

impl BluetoothControlBackend for MemBluetoothBackend {
    fn set_adapter_power(&mut self, on: bool) -> Result<(), String> {
        self.calls.push(on);
        if let Some(detail) = &self.fail {
            return Err(detail.clone());
        }
        Ok(())
    }
}

/// Maps a platform control failure into the broker-facing [`BridgeError`],
/// preserving `E_NOT_PERMITTED` (never escalated) and the backend code.
fn control_error_to_bridge(error: BluetoothControlError) -> BridgeError {
    match &error {
        BluetoothControlError::NotPermitted => {
            BridgeError::new("runtime", error.code(), error.to_string())
        }
        BluetoothControlError::BackendUnavailable(_) => {
            BridgeError::new("runtime", error.code(), error.to_string())
        }
    }
}

/// Host bridge: owns the read sampler, serving Lua tables with the grant
/// translation applied. Grants re-read on every `get`; narrowing hides
/// without restart.
pub struct BluetoothBridge<S> {
    service: BluetoothStateService,
    source: S,
}

impl<S> BluetoothBridge<S>
where
    S: BluetoothSource,
{
    /// Binds the sampler to its source with no grants (denies everything).
    pub fn new(source: S) -> Self {
        Self {
            service: BluetoothStateService::new(),
            source,
        }
    }

    /// Replaces the granted-head snapshot from broker heads.
    pub fn set_broker_grants(&mut self, granted: &[String]) {
        self.service.set_grants(grants_from_broker(granted));
    }

    /// Replaces the grant snapshot directly (tests).
    pub fn set_grants(&mut self, grants: GrantSet) {
        self.service.set_grants(grants);
    }

    /// Reads the provider: `None` when ungranted or hidden, otherwise the
    /// floored Lua table.
    pub fn get(&mut self, clock: &dyn BluetoothClock) -> Option<LuaValue> {
        let snapshot = self.service.get(clock, &mut self.source)?;
        Some(bluetooth_snapshot_to_lua(&snapshot))
    }
}

/// Dispatches `bluetooth.control.set_power` honoring `E_NOT_PERMITTED`.
///
/// Without the control head the backend is never touched (fail-closed);
/// backend failures map to `E_BACKEND_UNAVAILABLE`. Never escalates.
pub fn bluetooth_set_power(
    granted: &[String],
    backend: &mut dyn BluetoothControlBackend,
    on: bool,
) -> Result<(), BridgeError> {
    let control = BluetoothControl::with_grants(grants_from_broker(granted));
    control
        .set_power(backend, on)
        .map_err(control_error_to_bridge)
}

/// Whether an object path falls inside the scoped BlueZ subtree (shared
/// rule, re-exported for host enforcement).
#[must_use]
pub fn path_in_scope(path: &str) -> bool {
    bluez_path_in_scope(path)
}

/// Scoped object root below which every accessed path must fall.
#[must_use]
pub fn object_root() -> &'static str {
    BLUEZ_OBJECT_ROOT
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_platform::providers::{BLUETOOTH_CONTROL_HEAD, BLUETOOTH_READ_HEAD};

    struct FakeClock {
        now: u64,
    }

    impl BluetoothClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now
        }
    }

    fn bonded(address: &str, alias: &str, connected: bool) -> RawBluetoothDevice {
        RawBluetoothDevice {
            address: address.to_string(),
            alias: alias.to_string(),
            connected,
            bonded: true,
        }
    }

    fn healthy_source(count: usize) -> MemBluetoothSource {
        let rows = (0..count)
            .map(|i| {
                (
                    format!("/org/bluez/hci0/dev_{i:02X}"),
                    bonded(
                        &format!("AA:BB:CC:DD:EE:{i:02X}"),
                        &format!("device-{i}"),
                        i % 2 == 0,
                    ),
                )
            })
            .collect();
        MemBluetoothSource::new(true, Some(true), Some(rows))
    }

    #[test]
    fn broker_grant_translation_is_deny_by_default() {
        assert!(grants_from_broker(&[]).is_empty());
        let grants = grants_from_broker(&["platform.bluetooth.read".to_string()]);
        assert!(grants.contains(BLUETOOTH_READ_HEAD));
        assert!(!grants.contains(BLUETOOTH_CONTROL_HEAD));
        let grants = grants_from_broker(&["platform.bluetooth.control".to_string()]);
        assert!(grants.contains(BLUETOOTH_CONTROL_HEAD));
        assert!(!grants.contains(BLUETOOTH_READ_HEAD));
    }

    #[test]
    fn enumeration_cap_counts_without_lying_end_to_end() {
        let mut bridge = BluetoothBridge::new(healthy_source(100));
        bridge.set_grants(GrantSet::new(vec![BLUETOOTH_READ_HEAD.to_string()]));
        let value = bridge.get(&FakeClock { now: 0 }).expect("capped serves");
        assert_eq!(value.get("total_count"), Some(&LuaValue::Integer(100)));
        assert_eq!(value.get("connected_count"), Some(&LuaValue::Integer(50)));
        assert_eq!(value.get("truncated"), Some(&LuaValue::Bool(true)));
        let devices = match value.get("devices") {
            Some(LuaValue::Table(pairs)) => pairs.len(),
            other => panic!("devices must be an array table, got {other:?}"),
        };
        assert_eq!(devices, 16);
    }

    #[test]
    fn out_of_scope_paths_never_served() {
        let rows = vec![
            (
                "/org/freedesktop/DBus".to_string(),
                bonded("AA:BB:CC:DD:EE:01", "evil", true),
            ),
            (
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_02".to_string(),
                bonded("AA:BB:CC:DD:EE:02", "good", true),
            ),
        ];
        let mut bridge =
            BluetoothBridge::new(MemBluetoothSource::new(true, Some(true), Some(rows)));
        bridge.set_grants(GrantSet::new(vec![BLUETOOTH_READ_HEAD.to_string()]));
        let value = bridge.get(&FakeClock { now: 0 }).expect("serves subset");
        assert_eq!(value.get("total_count"), Some(&LuaValue::Integer(1)));
        assert!(!path_in_scope("/org/freedesktop/DBus"));
        assert!(path_in_scope("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_02"));
        assert_eq!(object_root(), "/org/bluez");
    }

    #[test]
    fn control_without_head_fails_closed_without_touching_backend() {
        let mut backend = MemBluetoothBackend::default();
        let error = bluetooth_set_power(&[], &mut backend, true).expect_err("denied");
        assert_eq!(error.code, "E_NOT_PERMITTED");
        assert!(backend.calls.is_empty());
        let mut backend = MemBluetoothBackend::default();
        let error =
            bluetooth_set_power(&["platform.bluetooth.read".to_string()], &mut backend, true)
                .expect_err("read never implies control");
        assert_eq!(error.code, "E_NOT_PERMITTED");
        assert!(backend.calls.is_empty());
    }

    #[test]
    fn granted_control_drives_backend_and_maps_failures() {
        let mut backend = MemBluetoothBackend::default();
        bluetooth_set_power(
            &["platform.bluetooth.control".to_string()],
            &mut backend,
            false,
        )
        .expect("granted sets");
        assert_eq!(backend.calls, vec![false]);
        let mut failing = MemBluetoothBackend {
            calls: Vec::new(),
            fail: Some("bus timeout".to_string()),
        };
        let error = bluetooth_set_power(
            &["platform.bluetooth.control".to_string()],
            &mut failing,
            true,
        )
        .expect_err("backend failure surfaces");
        assert_eq!(error.code, "E_BACKEND_UNAVAILABLE");
    }

    #[test]
    fn no_adapter_hides_fail_closed() {
        let mut bridge = BluetoothBridge::new(NoBluetoothSource);
        bridge.set_grants(GrantSet::new(vec![BLUETOOTH_READ_HEAD.to_string()]));
        assert_eq!(bridge.get(&FakeClock { now: 0 }), None);
    }
}
