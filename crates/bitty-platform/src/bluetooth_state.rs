#![forbid(unsafe_code)]
//! Bluetooth-state provider interface (CTX-1197, bitty#2017).
//!
//! Bar modules (`bluetooth` parity) consume radio power plus a bounded
//! connection list; radio power control (`set_power`) lives behind the
//! separate [`BLUETOOTH_CONTROL_HEAD`] grant and is never smuggled into
//! the read path.
//!
//! Source (injected; this module performs no D-Bus access itself): BlueZ
//! with **scoped object paths only** — the adapter plus bonded devices,
//! never a broad bus proxy. The scoping identifiers live in exactly one
//! place ([`BLUEZ_BUS_NAME`], [`BLUEZ_ADAPTER_INTERFACE`],
//! [`BLUEZ_DEVICE_INTERFACE`], [`BLUEZ_OBJECT_ROOT`]); the live adapter
//! (host task) binds them, and tests inject fixtures. No BlueZ on the
//! bus surfaces as fail-closed hide (`None`), identical to an ungranted
//! read head.
//!
//! Contract bounds: the read path enforces a 5 s poll floor
//! ([`BLUETOOTH_POLL_FLOOR_MS`]); device enumeration caps at
//! [`BLUETOOTH_MAX_DEVICES`] entries with [`BLUETOOTH_ALIAS_MAX_CHARS`]
//! per-entry alias cap. The cap is a **privacy bound**, not just a perf
//! bound: addresses are device identifiers, so enumeration stays bounded
//! even when the bus would offer more. Capping is never silent: the
//! snapshot carries the true [`BluetoothSnapshot::total_count`] plus an
//! explicit [`BluetoothSnapshot::truncated`] flag (capped, counted, no
//! truncation lie). A lost adapter serves the last good snapshot with a
//! growing [`BluetoothSnapshot::stale_age_ms`] until
//! [`BLUETOOTH_STALE_HIDE_MS`], then hides.
//!
//! Bar mapping (Lua owns presentation): `powered` selects the radio
//! glyph, `connected_count`/`total_count` render the counts, each device
//! contributes alias + address text, and click-to-open-manager stays
//! Lua-side (the manager itself is a command, never a shell). No new bar
//! API is required.

use super::providers::{BLUETOOTH_CONTROL_HEAD, BLUETOOTH_READ_HEAD, GrantSet};

// ---------------------------------------------------------------------------
// Bounds (policy, timeout, and default values live here, never inline)
// ---------------------------------------------------------------------------

/// Read-path poll floor: configuration can never sample faster.
pub const BLUETOOTH_POLL_FLOOR_MS: u64 = 5000;

/// Default sampling interval (equal to the floor per the 5 s ceiling).
pub const BLUETOOTH_DEFAULT_INTERVAL_MS: u64 = 5000;

/// Stale-then-hide bound: a lost adapter hides after this long without a
/// good sample (six missed polls at the floor).
pub const BLUETOOTH_STALE_HIDE_MS: u64 = 30_000;

/// Maximum devices retained per snapshot (count + per-entry caps bound
/// the enumeration; the true total stays visible via `total_count`).
pub const BLUETOOTH_MAX_DEVICES: usize = 16;

/// Maximum alias characters per entry (truncated at a char boundary).
pub const BLUETOOTH_ALIAS_MAX_CHARS: usize = 64;

/// Exact byte length of a `XX:XX:XX:XX:XX:XX` Bluetooth address.
pub const BLUETOOTH_ADDRESS_BYTES: usize = 17;

// ---------------------------------------------------------------------------
// D-Bus scoping identifiers (spec-defined, exactly one place)
// ---------------------------------------------------------------------------

/// BlueZ system-bus name (the only bus name this provider ever addresses).
pub const BLUEZ_BUS_NAME: &str = "org.bluez";

/// Adapter interface (radio power + discovery state).
pub const BLUEZ_ADAPTER_INTERFACE: &str = "org.bluez.Adapter1";

/// Device interface (bonded-device state only).
pub const BLUEZ_DEVICE_INTERFACE: &str = "org.bluez.Device1";

/// Object root below which every accessed path must fall: adapters
/// (`/org/bluez/hciX`) and their bonded devices only. A path outside
/// this root is never queried (no broad bus proxy, host-enforced).
pub const BLUEZ_OBJECT_ROOT: &str = "/org/bluez";

// ---------------------------------------------------------------------------
// Snapshot types
// ---------------------------------------------------------------------------

/// One bonded device, sanitized and bounded for bar text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BluetoothDevice {
    /// Validated `XX:XX:XX:XX:XX:XX` address (malformed entries are
    /// dropped before the snapshot, never served).
    pub address: String,
    /// Alias with control characters stripped, capped at
    /// [`BLUETOOTH_ALIAS_MAX_CHARS`] characters.
    pub alias: String,
    /// Whether the device is currently connected.
    pub connected: bool,
}

/// Bluetooth-state snapshot: bounded primitives only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BluetoothSnapshot {
    /// Radio power state.
    pub powered: bool,
    /// Connected bonded devices (counted over the full valid set, not
    /// just the retained entries).
    pub connected_count: usize,
    /// Valid bonded devices (the true total, even when capped).
    pub total_count: usize,
    /// Retained entries, at most [`BLUETOOTH_MAX_DEVICES`], sorted by
    /// address for determinism.
    pub devices: Vec<BluetoothDevice>,
    /// `true` when valid devices were dropped to fit the cap.
    pub truncated: bool,
    /// Milliseconds since the last good sample (0 on a fresh sample).
    pub stale_age_ms: u64,
}

/// Raw bonded-device row from the source, pre-sanitization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBluetoothDevice {
    /// Claimed address (validated before admission).
    pub address: String,
    /// Claimed alias (sanitized before admission).
    pub alias: String,
    /// Claimed connection state.
    pub connected: bool,
    /// Whether BlueZ reports the device as bonded. Unbonded rows are
    /// filtered here (defense in depth: the host must already scope to
    /// bonded paths, and the provider refuses anything else).
    pub bonded: bool,
}

/// Whether an object path falls inside the scoped BlueZ subtree
/// ([`BLUEZ_OBJECT_ROOT`]). The host enforces this before any bus read;
/// the check lives here so fixtures and host share one rule.
#[must_use]
pub fn bluez_path_in_scope(path: &str) -> bool {
    if path == BLUEZ_OBJECT_ROOT || path.is_empty() {
        return false;
    }
    path.starts_with("/org/bluez/")
}

/// Validate a `XX:XX:XX:XX:XX:XX` address shape (17 bytes, five colons,
/// twelve hex digits). Anything else is corrupt data, never served.
#[must_use]
pub fn valid_bluetooth_address(address: &str) -> bool {
    if address.len() != BLUETOOTH_ADDRESS_BYTES {
        return false;
    }
    let mut groups = 0;
    for part in address.split(':') {
        groups += 1;
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    groups == 6
}

/// Sanitize a claimed alias: strip control characters, trim, cap at
/// [`BLUETOOTH_ALIAS_MAX_CHARS`] characters at a char boundary.
/// Never fails: an empty result stays empty (Lua falls back to the
/// address for display).
#[must_use]
pub fn sanitize_alias(raw: &str) -> String {
    let stripped: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = stripped.trim();
    // Byte index of the 65th character (or the end when shorter): the
    // truncation point keeping at most 64 characters, always a boundary.
    let end = trimmed
        .char_indices()
        .nth(BLUETOOTH_ALIAS_MAX_CHARS)
        .map_or(trimmed.len(), |(idx, _)| idx);
    trimmed[..end].to_string()
}

// ---------------------------------------------------------------------------
// Source traits (injected; the live BlueZ adapter is host work)
// ---------------------------------------------------------------------------

/// Bluetooth read source (scoped BlueZ reads in the live adapter).
pub trait BluetoothSource {
    /// Whether BlueZ answers at all (`false` drives stale-then-hide).
    fn present(&mut self) -> bool;
    /// Adapter radio power (`None` = unreadable property).
    fn adapter_powered(&mut self) -> Option<bool>;
    /// Bonded-device rows (`None` = enumeration failed).
    fn devices(&mut self) -> Option<Vec<RawBluetoothDevice>>;
}

/// Bluetooth control backend (scoped `Adapter1.Powered` write in live use).
pub trait BluetoothControlBackend {
    /// Sets radio power; `Err` carries a short diagnostic (never panics).
    fn set_adapter_power(&mut self, on: bool) -> Result<(), String>;
}

/// Injected monotonic clock (milliseconds) for cadence control.
pub trait BluetoothClock {
    /// Current time in milliseconds.
    fn now_ms(&self) -> u64;
}

// ---------------------------------------------------------------------------
// Read service
// ---------------------------------------------------------------------------

/// Bluetooth-state read provider: rate-floored, grant-gated, fail-closed.
///
/// Grants are re-read on every [`BluetoothStateService::get`]; without
/// [`BLUETOOTH_READ_HEAD`] the answer is `None`, byte-identical to a
/// missing adapter.
#[derive(Debug)]
pub struct BluetoothStateService {
    interval_ms: u64,
    last_poll_ms: Option<u64>,
    last_sample_ms: Option<u64>,
    cached: Option<BluetoothSnapshot>,
    grants: GrantSet,
}

impl BluetoothStateService {
    /// Creates the service with the default interval and no grants.
    #[must_use]
    pub fn new() -> Self {
        Self {
            interval_ms: BLUETOOTH_DEFAULT_INTERVAL_MS,
            last_poll_ms: None,
            last_sample_ms: None,
            cached: None,
            grants: GrantSet::default(),
        }
    }

    /// Creates the service with a custom interval, clamped up to the
    /// floor. Configuration can never request sub-5 s polling.
    #[must_use]
    pub fn with_interval(requested_ms: u64) -> Self {
        Self {
            interval_ms: requested_ms.max(BLUETOOTH_POLL_FLOOR_MS),
            last_poll_ms: None,
            last_sample_ms: None,
            cached: None,
            grants: GrantSet::default(),
        }
    }

    /// Effective sampling interval in milliseconds.
    #[must_use]
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Replaces the granted-head snapshot (narrowing takes effect on
    /// the next `get` with no restart).
    pub fn set_grants(&mut self, grants: GrantSet) {
        self.grants = grants;
    }

    /// Reads the provider: `None` when ungranted or hidden, otherwise
    /// the floored snapshot.
    pub fn get(
        &mut self,
        clock: &dyn BluetoothClock,
        source: &mut dyn BluetoothSource,
    ) -> Option<BluetoothSnapshot> {
        if !self.grants.contains(BLUETOOTH_READ_HEAD) {
            return None;
        }
        let now = clock.now_ms();
        let due = self
            .last_poll_ms
            .is_none_or(|last| now.wrapping_sub(last) >= self.interval_ms);
        if !due {
            return self.held(now);
        }
        self.last_poll_ms = Some(now);
        if !source.present() {
            return self.stale_or_hidden(now);
        }
        // A half-answered source (unreadable property or failed
        // enumeration) is a lost source, not a partial snapshot: serve
        // stale or hide, never fabricated device state.
        let sample = match (source.adapter_powered(), source.devices()) {
            (Some(powered), Some(rows)) => Some(build_snapshot(&rows, powered)),
            _ => None,
        };
        let Some(snapshot) = sample else {
            return self.stale_or_hidden(now);
        };
        self.last_sample_ms = Some(now);
        self.cached = Some(snapshot.clone());
        Some(snapshot)
    }

    /// Serves the held snapshot with a refreshed age (floored polls).
    fn held(&self, now: u64) -> Option<BluetoothSnapshot> {
        let mut snapshot = self.cached.clone()?;
        let last = self.last_sample_ms?;
        snapshot.stale_age_ms = now.wrapping_sub(last);
        Some(snapshot)
    }

    /// Serves the last good snapshot while inside the stale bound, else
    /// hides (`None`).
    fn stale_or_hidden(&mut self, now: u64) -> Option<BluetoothSnapshot> {
        let last = self.last_sample_ms?;
        let age = now.wrapping_sub(last);
        if age > BLUETOOTH_STALE_HIDE_MS {
            self.cached = None;
            self.last_sample_ms = None;
            return None;
        }
        let mut snapshot = self.cached.clone()?;
        snapshot.stale_age_ms = age;
        Some(snapshot)
    }
}

impl Default for BluetoothStateService {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Control head (separate grant, separate type, never smuggled into read)
// ---------------------------------------------------------------------------

/// Bluetooth control failure: typed, with a stable broker-facing code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BluetoothControlError {
    /// The control head is not granted. The backend is never touched.
    NotPermitted,
    /// The backend call failed (carries the backend diagnostic).
    BackendUnavailable(String),
}

impl BluetoothControlError {
    /// Stable error code for broker/diagnostic mapping.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotPermitted => "E_NOT_PERMITTED",
            Self::BackendUnavailable(_) => "E_BACKEND_UNAVAILABLE",
        }
    }
}

impl std::fmt::Display for BluetoothControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPermitted => write!(
                f,
                "E_NOT_PERMITTED: bluetooth control requires the '{}' grant",
                BLUETOOTH_CONTROL_HEAD
            ),
            Self::BackendUnavailable(detail) => {
                write!(f, "E_BACKEND_UNAVAILABLE: {detail}")
            }
        }
    }
}

impl std::error::Error for BluetoothControlError {}

/// Builds a snapshot from validated rows: unbonded and malformed rows
/// are dropped, survivors are sanitized and sorted by address, the
/// counts cover the full valid set, and the cap is explicit.
fn build_snapshot(rows: &[RawBluetoothDevice], powered: bool) -> BluetoothSnapshot {
    let mut valid: Vec<BluetoothDevice> = rows
        .iter()
        .filter(|row| row.bonded)
        .filter(|row| valid_bluetooth_address(&row.address))
        .map(|row| BluetoothDevice {
            address: row.address.clone(),
            alias: sanitize_alias(&row.alias),
            connected: row.connected,
        })
        .collect();
    valid.sort_by(|a, b| a.address.cmp(&b.address));
    let total_count = valid.len();
    let connected_count = valid.iter().filter(|device| device.connected).count();
    let truncated = total_count > BLUETOOTH_MAX_DEVICES;
    valid.truncate(BLUETOOTH_MAX_DEVICES);
    BluetoothSnapshot {
        powered,
        connected_count,
        total_count,
        devices: valid,
        truncated,
        stale_age_ms: 0,
    }
}

/// Bluetooth radio-power control: usable only with the control grant.
///
/// Deliberately grant-checked per call (not per construction) so grant
/// narrowing closes the control path without restart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BluetoothControl {
    grants: GrantSet,
}

impl BluetoothControl {
    /// Binds the control head to a grant snapshot.
    #[must_use]
    pub fn with_grants(grants: GrantSet) -> Self {
        Self { grants }
    }

    /// Sets radio power. Without the control head this fails closed
    /// with [`BluetoothControlError::NotPermitted`] and the backend is
    /// never invoked.
    pub fn set_power(
        &self,
        backend: &mut dyn BluetoothControlBackend,
        on: bool,
    ) -> Result<(), BluetoothControlError> {
        if !self.grants.contains(BLUETOOTH_CONTROL_HEAD) {
            return Err(BluetoothControlError::NotPermitted);
        }
        backend
            .set_adapter_power(on)
            .map_err(BluetoothControlError::BackendUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeClock {
        now: u64,
    }

    impl BluetoothClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now
        }
    }

    struct FakeSource {
        present: bool,
        powered: Option<bool>,
        devices: Option<Vec<RawBluetoothDevice>>,
        polls: usize,
    }

    impl BluetoothSource for FakeSource {
        fn present(&mut self) -> bool {
            self.present
        }

        fn adapter_powered(&mut self) -> Option<bool> {
            self.polls += 1;
            self.powered
        }

        fn devices(&mut self) -> Option<Vec<RawBluetoothDevice>> {
            self.devices.clone()
        }
    }

    struct FakeBackend {
        calls: Vec<bool>,
        fail: Option<String>,
    }

    impl BluetoothControlBackend for FakeBackend {
        fn set_adapter_power(&mut self, on: bool) -> Result<(), String> {
            self.calls.push(on);
            if let Some(detail) = &self.fail {
                return Err(detail.clone());
            }
            Ok(())
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

    fn read_grants() -> GrantSet {
        GrantSet::new(vec![BLUETOOTH_READ_HEAD.to_string()])
    }

    fn healthy_source(count: usize) -> FakeSource {
        let devices = (0..count)
            .map(|i| {
                bonded(
                    &format!("AA:BB:CC:DD:EE:{i:02X}"),
                    &format!("device-{i}"),
                    i % 2 == 0,
                )
            })
            .collect();
        FakeSource {
            present: true,
            powered: Some(true),
            devices: Some(devices),
            polls: 0,
        }
    }

    #[test]
    fn address_validation_accepts_only_canonical_shape() {
        assert!(valid_bluetooth_address("AA:BB:CC:DD:EE:FF"));
        assert!(valid_bluetooth_address("aa:bb:cc:dd:ee:00"));
        assert!(valid_bluetooth_address("01:23:45:67:89:AB"));
        assert!(!valid_bluetooth_address("AA:BB:CC:DD:EE"));
        assert!(!valid_bluetooth_address("AA:BB:CC:DD:EE:FF:00"));
        assert!(!valid_bluetooth_address("AABBCCDDEEFF"));
        assert!(!valid_bluetooth_address("GG:BB:CC:DD:EE:FF"));
        assert!(!valid_bluetooth_address("AA-BB-CC-DD-EE-FF"));
        assert!(!valid_bluetooth_address(""));
        assert!(!valid_bluetooth_address("AA:BB:CC:DD:EE:F "));
    }

    #[test]
    fn alias_sanitizer_strips_controls_and_caps() {
        assert_eq!(sanitize_alias("  Headphones \n"), "Headphones");
        assert_eq!(sanitize_alias("a\tb\x07c"), "abc");
        assert_eq!(sanitize_alias(""), "");
        let long = "a".repeat(BLUETOOTH_ALIAS_MAX_CHARS + 20);
        assert_eq!(sanitize_alias(&long), "a".repeat(BLUETOOTH_ALIAS_MAX_CHARS));
    }

    #[test]
    fn scope_check_admits_only_the_bluez_subtree() {
        assert!(bluez_path_in_scope("/org/bluez/hci0"));
        assert!(bluez_path_in_scope("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"));
        assert!(!bluez_path_in_scope("/org/bluez"));
        assert!(!bluez_path_in_scope("/org/bluezother/hci0"));
        assert!(!bluez_path_in_scope("/org/freedesktop/DBus"));
        assert!(!bluez_path_in_scope(""));
    }

    #[test]
    fn ungranted_read_is_indistinguishable_from_absent() {
        let mut service = BluetoothStateService::new();
        let clock = FakeClock { now: 0 };
        let mut source = healthy_source(2);
        assert_eq!(service.get(&clock, &mut source), None);
        assert_eq!(source.polls, 0, "denied reads must not touch the bus");
    }

    #[test]
    fn no_bluez_hides_fail_closed() {
        let mut service = BluetoothStateService::new();
        service.set_grants(read_grants());
        let mut source = FakeSource {
            present: false,
            powered: None,
            devices: None,
            polls: 0,
        };
        assert_eq!(service.get(&FakeClock { now: 0 }, &mut source), None);
    }

    #[test]
    fn healthy_read_serves_power_and_devices() {
        let mut service = BluetoothStateService::new();
        service.set_grants(read_grants());
        let mut source = healthy_source(3);
        let snapshot = service
            .get(&FakeClock { now: 0 }, &mut source)
            .expect("healthy serves");
        assert!(snapshot.powered);
        assert_eq!(snapshot.total_count, 3);
        assert_eq!(snapshot.connected_count, 2);
        assert_eq!(snapshot.devices.len(), 3);
        assert!(!snapshot.truncated);
        assert_eq!(snapshot.stale_age_ms, 0);
        assert_eq!(snapshot.devices[0].address, "AA:BB:CC:DD:EE:00");
    }

    #[test]
    fn enumeration_cap_counts_without_lying() {
        let mut service = BluetoothStateService::new();
        service.set_grants(read_grants());
        let mut source = healthy_source(100);
        let snapshot = service
            .get(&FakeClock { now: 0 }, &mut source)
            .expect("capped serves");
        assert_eq!(snapshot.total_count, 100);
        assert_eq!(snapshot.connected_count, 50);
        assert_eq!(snapshot.devices.len(), BLUETOOTH_MAX_DEVICES);
        assert!(snapshot.truncated, "cap must be explicit, never silent");
    }

    #[test]
    fn unbonded_and_malformed_rows_never_served() {
        let mut service = BluetoothStateService::new();
        service.set_grants(read_grants());
        let mut source = FakeSource {
            present: true,
            powered: Some(false),
            devices: Some(vec![
                bonded("NOT-AN-ADDRESS", "evil", true),
                RawBluetoothDevice {
                    address: "AA:BB:CC:DD:EE:01".to_string(),
                    alias: "unbonded".to_string(),
                    connected: true,
                    bonded: false,
                },
                bonded("AA:BB:CC:DD:EE:02", "good", true),
            ]),
            polls: 0,
        };
        let snapshot = service
            .get(&FakeClock { now: 0 }, &mut source)
            .expect("serves the valid subset");
        assert!(!snapshot.powered);
        assert_eq!(snapshot.total_count, 1);
        assert_eq!(snapshot.connected_count, 1);
        assert_eq!(snapshot.devices.len(), 1);
        assert_eq!(snapshot.devices[0].alias, "good");
    }

    #[test]
    fn read_floor_coalesces_and_loss_hides_after_bound() {
        let mut service = BluetoothStateService::new();
        service.set_grants(read_grants());
        let mut source = healthy_source(1);
        let first = service
            .get(&FakeClock { now: 0 }, &mut source)
            .expect("first serves");
        assert_eq!(source.polls, 1);
        let held = service
            .get(&FakeClock { now: 1_000 }, &mut source)
            .expect("held serves");
        assert_eq!(source.polls, 1, "over-poll must not resample");
        assert_eq!(held.stale_age_ms, 1_000);
        assert_eq!(held.devices, first.devices);
        source.present = false;
        let stale = service
            .get(
                &FakeClock {
                    now: BLUETOOTH_POLL_FLOOR_MS,
                },
                &mut source,
            )
            .expect("stale serves before the bound");
        assert!(stale.stale_age_ms >= BLUETOOTH_POLL_FLOOR_MS);
        let hidden = service.get(
            &FakeClock {
                now: BLUETOOTH_STALE_HIDE_MS + BLUETOOTH_POLL_FLOOR_MS,
            },
            &mut source,
        );
        assert_eq!(hidden, None, "lost adapter hides after the bound");
    }

    #[test]
    fn control_without_head_fails_closed_without_touching_backend() {
        let control = BluetoothControl::with_grants(GrantSet::default());
        let mut backend = FakeBackend {
            calls: Vec::new(),
            fail: None,
        };
        let error = control
            .set_power(&mut backend, true)
            .expect_err("ungranted control must fail");
        assert_eq!(error, BluetoothControlError::NotPermitted);
        assert_eq!(error.code(), "E_NOT_PERMITTED");
        assert!(
            backend.calls.is_empty(),
            "denied control must never reach the backend"
        );
    }

    #[test]
    fn granted_control_drives_backend_and_maps_failures() {
        let control =
            BluetoothControl::with_grants(GrantSet::new(vec![BLUETOOTH_CONTROL_HEAD.to_string()]));
        let mut backend = FakeBackend {
            calls: Vec::new(),
            fail: None,
        };
        control
            .set_power(&mut backend, false)
            .expect("granted sets");
        assert_eq!(backend.calls, vec![false]);
        let mut failing = FakeBackend {
            calls: Vec::new(),
            fail: Some("bus timeout".to_string()),
        };
        let error = control
            .set_power(&mut failing, true)
            .expect_err("backend failure surfaces");
        assert_eq!(
            error,
            BluetoothControlError::BackendUnavailable("bus timeout".to_string())
        );
        assert_eq!(error.code(), "E_BACKEND_UNAVAILABLE");
    }

    #[test]
    fn read_grant_never_implies_control() {
        let control = BluetoothControl::with_grants(read_grants());
        let mut backend = FakeBackend {
            calls: Vec::new(),
            fail: None,
        };
        assert_eq!(
            control.set_power(&mut backend, true),
            Err(BluetoothControlError::NotPermitted)
        );
        assert!(backend.calls.is_empty());
    }

    #[test]
    fn interval_clamps_up_to_floor() {
        assert_eq!(
            BluetoothStateService::with_interval(100).interval_ms(),
            BLUETOOTH_POLL_FLOOR_MS
        );
        assert_eq!(
            BluetoothStateService::with_interval(9_000).interval_ms(),
            9_000
        );
        assert_eq!(
            BluetoothStateService::new().interval_ms(),
            BLUETOOTH_DEFAULT_INTERVAL_MS
        );
    }
}
