//! Runtime-owned device host for bar modules (CTX-1238, issue #2100).
//!
//! Publishes the batch-B descriptors from [`super`] into the runtime
//! [`ServiceDirectory`](super::super::services::ServiceDirectory) and serves
//! reads plus controls through host dispatch (no VM, no Lua provider). One
//! runtime owns one host, so the provider-side ceilings stay global: bursty
//! bar ticks from every consumer share one source attempt per window.
//!
//! Live sources use only `std::fs` (existing seam, no new dependency):
//! `/sys/class/net`, `/proc/net/dev`, `/proc/net/route`, and
//! `/sys/class/backlight`. Wi-Fi identity (NetworkManager D-Bus), BlueZ
//! reads, and the Wayland idle-inhibit binding stay fail-closed stubs in
//! this slice (hide, never fabricate); their live bindings are explicit
//! follow-ups with their own `cargo-deny` plus W-147 justification. No new
//! production dependency edge is introduced (`std` plus the existing
//! `bitty-platform` and `bitty-lua` value types only).
//!
//! Time is the host monotonic clock (elapsed since creation) unless a test
//! pins a manual instant. Every `get()` preserves the provider ceilings and
//! `stale_age_ms` end-to-end; hide surfaces as Lua `nil` (the bar hides),
//! never as fabricated zeros. Thresholds and icon buckets stay Lua-side:
//! only raw numbers/states cross here.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Instant;

use bitty_lua::{BridgeError, LuaValue};
use bitty_platform::backlight_state::PowerClock;
use bitty_platform::bluetooth_state::{BluetoothClock, RawBluetoothDevice};
use bitty_platform::network_state::{EssidSource, NetClock};

use super::bluetooth::{
    BluetoothBridge, MemBluetoothBackend, MemBluetoothSource, bluetooth_set_power,
};
use super::network::{NetworkBridge, ProcNetDevCounters, SysfsNetLink};
use super::power::{
    BacklightBridge, IdleBridge, MemIdleBackend, MemIdleSource, SysfsBacklightBackend,
    SysfsBacklightSource, backlight_set_step, idle_set,
};
use super::{
    BACKLIGHT_CONTROL_IFACE, BACKLIGHT_STATE_IFACE, BLUETOOTH_CONTROL_IFACE, BLUETOOTH_STATE_IFACE,
    DEVICE_PROVIDER, IDLE_CONTROL_IFACE, IDLE_STATE_IFACE, NETWORK_ESSID_IFACE,
    NETWORK_STATE_IFACE,
};

/// Generation pinned into native device records.
///
/// Runtime-owned (never a plugin activation generation): the directory route
/// carries it, so a republished replacement could never hijack a live
/// handle. Native records never suspend or revoke (their provider is never
/// a plugin id), so the generation never advances.
pub const DEVICE_GENERATION: u32 = 0;

/// In-memory ESSID source for tests and headless proofs.
///
/// `None` mimics the NM-less stub ([`super::network::NoEssid`]); `Some`
/// serves the fixture identity through the same sanitizer path.
#[derive(Debug, Clone, Default)]
pub struct MemEssidSource {
    value: Option<String>,
}

impl MemEssidSource {
    /// Source that never answers (live NM-less posture).
    #[must_use]
    pub fn none() -> Self {
        Self { value: None }
    }

    /// Source answering one fixture ESSID (tests).
    #[must_use]
    pub fn with_value(value: Option<String>) -> Self {
        Self { value }
    }
}

impl EssidSource for MemEssidSource {
    fn essid(&mut self, _ifname: &str) -> Option<String> {
        self.value.clone()
    }
}

/// Native device host: live bridges plus the monotonic clock.
///
/// Shared per runtime (`Rc<RefCell<...>>`): every generation dispatches
/// through the same bridges, so ceilings are global, not per-consumer.
pub struct DeviceHost {
    network: NetworkBridge<SysfsNetLink, ProcNetDevCounters, MemEssidSource>,
    bluetooth: BluetoothBridge<MemBluetoothSource>,
    bluetooth_backend: MemBluetoothBackend,
    backlight: BacklightBridge<SysfsBacklightSource>,
    backlight_root: PathBuf,
    idle: IdleBridge<MemIdleSource>,
    idle_backend: MemIdleBackend,
    start: Instant,
    manual_now_ms: Option<u64>,
}

struct DeviceClock {
    now: u64,
}

impl NetClock for DeviceClock {
    fn now_ms(&self) -> u64 {
        self.now
    }
}

impl BluetoothClock for DeviceClock {
    fn now_ms(&self) -> u64 {
        self.now
    }
}

impl PowerClock for DeviceClock {
    fn now_ms(&self) -> u64 {
        self.now
    }
}

impl DeviceHost {
    /// Live host: real sysfs/procfs sources plus fail-closed stubs.
    ///
    /// Bluetooth presents absent, ESSID answers none, idle presents absent,
    /// and both bluetooth/idle control backends fail with
    /// `E_BACKEND_UNAVAILABLE`. Backlight reads live sysfs; backlight
    /// control attempts the sysfs write only when permitted (DEC-0115).
    #[must_use]
    pub fn live() -> Self {
        Self {
            network: NetworkBridge::new(
                SysfsNetLink::system(),
                ProcNetDevCounters::system(),
                MemEssidSource::none(),
            ),
            bluetooth: BluetoothBridge::new(MemBluetoothSource::new(false, None, None)),
            bluetooth_backend: MemBluetoothBackend {
                calls: Vec::new(),
                fail: Some(
                    "BlueZ control unavailable (fail-closed stub; live D-Bus binding is a follow-up)"
                        .to_string(),
                ),
            },
            backlight: BacklightBridge::new(SysfsBacklightSource::system()),
            backlight_root: PathBuf::from("/sys/class/backlight"),
            idle: IdleBridge::new(MemIdleSource {
                present: false,
                inhibited: None,
            }),
            idle_backend: MemIdleBackend {
                calls: Vec::new(),
                inhibited: false,
                fail: Some(
                    "idle control unavailable (fail-closed stub; live Wayland binding is a follow-up)"
                        .to_string(),
                ),
            },
            start: Instant::now(),
            manual_now_ms: None,
        }
    }

    /// Test host over injected fixture roots plus in-memory peers.
    ///
    /// `class_net`/`proc_route`/`proc_dev` answer the network sysfs roots;
    /// `essid` answers Wi-Fi identity (`None` mimics NM-less); `bt_present`
    /// plus `bt_rows` answer the bluetooth adapter; `backlight_root` is the
    /// `/sys/class/backlight` replacement; `idle_present`/`idle_inhibited`
    /// answer the compositor flag. Control backends record (bluetooth/idle)
    /// or write through the fixture root (backlight). `initial_now_ms` pins
    /// the clock; advance it with [`Self::set_manual_now_ms`].
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn for_test(
        class_net: PathBuf,
        proc_route: PathBuf,
        proc_dev: PathBuf,
        essid: Option<String>,
        bt_present: bool,
        bt_powered: Option<bool>,
        bt_rows: Option<Vec<(String, RawBluetoothDevice)>>,
        backlight_root: PathBuf,
        idle_present: bool,
        idle_inhibited: Option<bool>,
        initial_now_ms: u64,
    ) -> Self {
        let backlight_root_clone = backlight_root.clone();
        Self {
            network: NetworkBridge::new(
                SysfsNetLink::with_roots(class_net, proc_route),
                ProcNetDevCounters::with_path(proc_dev),
                MemEssidSource::with_value(essid),
            ),
            bluetooth: BluetoothBridge::new(MemBluetoothSource::new(
                bt_present, bt_powered, bt_rows,
            )),
            bluetooth_backend: MemBluetoothBackend::default(),
            backlight: BacklightBridge::new(SysfsBacklightSource::with_root(backlight_root_clone)),
            backlight_root,
            idle: IdleBridge::new(MemIdleSource {
                present: idle_present,
                inhibited: idle_inhibited,
            }),
            idle_backend: MemIdleBackend::default(),
            start: Instant::now(),
            manual_now_ms: Some(initial_now_ms),
        }
    }

    /// Current host instant in milliseconds.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        if let Some(now) = self.manual_now_ms {
            return now;
        }
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Pin the clock (tests); `None` restores the live monotonic clock.
    pub fn set_manual_now_ms(&mut self, now_ms: Option<u64>) {
        self.manual_now_ms = now_ms;
    }

    /// Whether `iface` is one of the eight batch-B device interfaces.
    #[must_use]
    pub fn is_device_iface(iface: &str) -> bool {
        matches!(
            iface,
            NETWORK_STATE_IFACE
                | NETWORK_ESSID_IFACE
                | BLUETOOTH_STATE_IFACE
                | BLUETOOTH_CONTROL_IFACE
                | BACKLIGHT_STATE_IFACE
                | BACKLIGHT_CONTROL_IFACE
                | IDLE_STATE_IFACE
                | IDLE_CONTROL_IFACE
        )
    }

    /// Whether `provider` names the native device component.
    #[must_use]
    pub fn is_device_provider(provider: &str) -> bool {
        provider == DEVICE_PROVIDER
    }

    /// Backing head for one device interface (or `None` outside batch B).
    #[must_use]
    pub fn head_for_iface(iface: &str) -> Option<&'static str> {
        match iface {
            NETWORK_STATE_IFACE => Some(super::NETWORK_READ_BROKER_HEAD),
            NETWORK_ESSID_IFACE => Some(super::ESSID_BROKER_HEAD),
            BLUETOOTH_STATE_IFACE => Some(super::BLUETOOTH_READ_BROKER_HEAD),
            BLUETOOTH_CONTROL_IFACE => Some(super::BLUETOOTH_CONTROL_BROKER_HEAD),
            BACKLIGHT_STATE_IFACE => Some(super::BACKLIGHT_READ_BROKER_HEAD),
            BACKLIGHT_CONTROL_IFACE => Some(super::BACKLIGHT_CONTROL_BROKER_HEAD),
            IDLE_STATE_IFACE => Some(super::IDLE_READ_BROKER_HEAD),
            IDLE_CONTROL_IFACE => Some(super::IDLE_CONTROL_BROKER_HEAD),
            _ => None,
        }
    }

    /// Serve one native call at the host instant.
    ///
    /// `granted_heads` must be the caller generation grant snapshot (already
    /// checked by the broker; the bridges re-check per call for defense in
    /// depth). Reads hide as `LuaValue::Nil` (stale past horizon, lost
    /// source, first-poll baseline); controls return `{ok=true}` or fail
    /// closed (`E_NOT_PERMITTED`, `E_BACKEND_UNAVAILABLE`,
    /// `E_SERVICE_INVALID`). Unknown interfaces or methods fail closed with
    /// `E_SERVICE_GONE` (indistinguishable from a parked provider, never a
    /// new code).
    pub fn call(
        &mut self,
        iface: &str,
        method: &str,
        args: &LuaValue,
        granted_heads: &[String],
    ) -> Result<LuaValue, BridgeError> {
        let expected = match iface {
            BLUETOOTH_CONTROL_IFACE => "set_power",
            BACKLIGHT_CONTROL_IFACE => "set_step",
            IDLE_CONTROL_IFACE => "set",
            NETWORK_STATE_IFACE
            | NETWORK_ESSID_IFACE
            | BLUETOOTH_STATE_IFACE
            | BACKLIGHT_STATE_IFACE
            | IDLE_STATE_IFACE => "get",
            _ => {
                return Err(BridgeError::new(
                    "runtime",
                    "E_SERVICE_GONE",
                    format!("service '{iface}.{method}' is not published"),
                ));
            }
        };
        if method != expected {
            return Err(BridgeError::new(
                "runtime",
                "E_SERVICE_GONE",
                format!("service '{iface}.{method}' is not published"),
            ));
        }
        let now = self.now_ms();
        let clock = DeviceClock { now };
        match iface {
            NETWORK_STATE_IFACE => {
                self.network.set_broker_grants(granted_heads);
                Ok(self
                    .network
                    .get(now, &clock)
                    .map(|value| strip_essid_unless_doubled(value, granted_heads))
                    .unwrap_or(LuaValue::Nil))
            }
            NETWORK_ESSID_IFACE => {
                self.network.set_broker_grants(granted_heads);
                let Some(value) = self.network.get(now, &clock) else {
                    return Ok(LuaValue::Nil);
                };
                match value.get("essid") {
                    Some(LuaValue::String(essid)) => Ok(LuaValue::table([
                        ("essid", LuaValue::String(essid.clone())),
                        (
                            "stale_age_ms",
                            value
                                .get("stale_age_ms")
                                .cloned()
                                .unwrap_or(LuaValue::Integer(now as i64)),
                        ),
                    ])),
                    _ => Ok(LuaValue::Nil),
                }
            }
            BLUETOOTH_STATE_IFACE => {
                self.bluetooth.set_broker_grants(granted_heads);
                Ok(self.bluetooth.get(&clock).unwrap_or(LuaValue::Nil))
            }
            BACKLIGHT_STATE_IFACE => {
                self.backlight.set_broker_grants(granted_heads);
                Ok(self.backlight.get(&clock).unwrap_or(LuaValue::Nil))
            }
            IDLE_STATE_IFACE => {
                self.idle.set_broker_grants(granted_heads);
                Ok(self.idle.get(&clock).unwrap_or(LuaValue::Nil))
            }
            BLUETOOTH_CONTROL_IFACE => {
                let on = parse_on_arg(args)?;
                bluetooth_set_power(granted_heads, &mut self.bluetooth_backend, on)?;
                Ok(LuaValue::table([("ok", LuaValue::Bool(true))]))
            }
            BACKLIGHT_CONTROL_IFACE => {
                let delta = parse_delta_arg(args)?;
                let (max, current) = self.backlight_levels().ok_or_else(|| {
                    BridgeError::new(
                        "runtime",
                        "E_SERVICE_GONE",
                        "service 'backlight.control' is unavailable",
                    )
                })?;
                let device = self.backlight_device().ok_or_else(|| {
                    BridgeError::new(
                        "runtime",
                        "E_SERVICE_GONE",
                        "service 'backlight.control' is unavailable",
                    )
                })?;
                let mut backend =
                    SysfsBacklightBackend::with_root(self.backlight_root.clone(), device);
                backlight_set_step(granted_heads, &mut backend, max, current, delta)?;
                Ok(LuaValue::table([("ok", LuaValue::Bool(true))]))
            }
            IDLE_CONTROL_IFACE => {
                let on = parse_on_arg(args)?;
                idle_set(granted_heads, &mut self.idle_backend, on)?;
                Ok(LuaValue::table([("ok", LuaValue::Bool(true))]))
            }
            _ => Err(BridgeError::new(
                "runtime",
                "E_SERVICE_GONE",
                format!("service '{iface}' is unavailable"),
            )),
        }
    }

    fn backlight_device(&mut self) -> Option<String> {
        use bitty_platform::backlight_state::BacklightSource;
        let mut probe = SysfsBacklightSource::with_root(self.backlight_root.clone());
        probe.primary_device()
    }

    fn backlight_levels(&mut self) -> Option<(u32, u32)> {
        use bitty_platform::backlight_state::BacklightSource;
        let mut probe = SysfsBacklightSource::with_root(self.backlight_root.clone());
        let device = probe.primary_device()?;
        probe.levels(&device)
    }

    /// Bluetooth control calls admitted to the backend, in order (tests).
    #[must_use]
    pub fn bluetooth_calls(&self) -> &[bool] {
        &self.bluetooth_backend.calls
    }

    /// Idle control calls admitted to the backend, in order (tests).
    #[must_use]
    pub fn idle_calls(&self) -> &[bool] {
        &self.idle_backend.calls
    }

    /// Current idle inhibit flag behind the seam (tests).
    #[must_use]
    pub fn idle_inhibited(&self) -> bool {
        self.idle_backend.inhibited
    }
}

/// Network `get()` serves the full snapshot (link plus optional essid);
/// the bar `network.state` shape never carries `essid` (it has its own
/// sub-interface). Strip it here so an ESSID grant never leaks through the
/// base interface; `network.essid` serves the sub-field above.
fn strip_essid_unless_doubled(mut value: LuaValue, _granted: &[String]) -> LuaValue {
    if let LuaValue::Table(ref mut pairs) = value {
        pairs.retain(|(k, _)| k != &LuaValue::String("essid".to_string()));
    }
    value
}

/// Parse `{on|powered|enabled|inhibited = bool}` control args.
fn parse_on_arg(args: &LuaValue) -> Result<bool, BridgeError> {
    for key in ["on", "powered", "enabled", "inhibited"] {
        match args.get(key) {
            Some(LuaValue::Bool(v)) => return Ok(*v),
            Some(LuaValue::Integer(n)) if *n == 0 || *n == 1 => return Ok(*n == 1),
            Some(LuaValue::Number(n)) if *n == 0.0 || *n == 1.0 => return Ok(*n == 1.0),
            Some(_) => {
                return Err(BridgeError::new(
                    "validation",
                    "E_SERVICE_INVALID",
                    format!("control arg '{key}' must be a boolean"),
                ));
            }
            None => {}
        }
    }
    Err(BridgeError::new(
        "validation",
        "E_SERVICE_INVALID",
        "control needs {on = bool}",
    ))
}

/// Parse `{delta|step|delta_pct = int}` backlight args, bounded to +/-100.
fn parse_delta_arg(args: &LuaValue) -> Result<i32, BridgeError> {
    for key in ["delta", "step", "delta_pct"] {
        match args.get(key) {
            Some(LuaValue::Integer(n)) => {
                if !(-100..=100).contains(n) {
                    return Err(BridgeError::new(
                        "validation",
                        "E_SERVICE_INVALID",
                        format!("backlight step {n} exceeds +/-100"),
                    ));
                }
                return Ok(*n as i32);
            }
            Some(LuaValue::Number(n)) => {
                if !n.is_finite() || n.abs() > 100.0 || n.fract() != 0.0 {
                    return Err(BridgeError::new(
                        "validation",
                        "E_SERVICE_INVALID",
                        format!("backlight step {n} exceeds +/-100"),
                    ));
                }
                return Ok(*n as i32);
            }
            Some(_) => {
                return Err(BridgeError::new(
                    "validation",
                    "E_SERVICE_INVALID",
                    format!("backlight arg '{key}' must be an integer -100..=+100"),
                ));
            }
            None => {}
        }
    }
    Err(BridgeError::new(
        "validation",
        "E_SERVICE_INVALID",
        "backlight set_step needs {delta = -100..=100}",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bitty-device-host-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn net_fixture(root: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
        let class_net = root.join("class_net");
        let eth = class_net.join("eth0");
        std::fs::create_dir_all(&eth).expect("eth dir");
        std::fs::write(eth.join("operstate"), "up\n").expect("operstate");
        let proc_route = root.join("route");
        std::fs::write(
            &proc_route,
            "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\neth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n",
        )
        .expect("route");
        let proc_dev = root.join("dev");
        std::fs::write(
            &proc_dev,
            "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n eth0: 1000000    100    0    0    0     0          0         0  500000      50    0    0    0     0       0          0\n",
        )
        .expect("dev");
        (class_net, proc_route, proc_dev)
    }

    fn wifi_fixture(root: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
        let class_net = root.join("class_net");
        let wlan = class_net.join("wlan0");
        std::fs::create_dir_all(&wlan).expect("wlan dir");
        std::fs::write(
            wlan.join("operstate"),
            "up
",
        )
        .expect("operstate");
        std::fs::write(wlan.join("wireless"), "").expect("wireless");
        let proc_route = root.join("route");
        std::fs::write(
            &proc_route,
            "Iface	Destination	Gateway	Flags	RefCnt	Use	Metric	Mask	MTU	Window	IRTT
wlan0	00000000	0101A8C0	0003	0	0	100	00000000	0	0	0
",
        )
        .expect("route");
        let proc_dev = root.join("dev");
        std::fs::write(
            &proc_dev,
            "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
 wlan0: 1000000    100    0    0    0     0          0         0  500000      50    0    0    0     0       0          0
",
        )
        .expect("dev");
        (class_net, proc_route, proc_dev)
    }

    fn backlight_fixture(root: &std::path::Path, brightness: &str, max: &str) -> PathBuf {
        let class_root = root.join("backlight");
        let dev = class_root.join("intel_backlight");
        std::fs::create_dir_all(&dev).expect("backlight dev");
        std::fs::write(dev.join("brightness"), brightness).expect("brightness");
        std::fs::write(dev.join("max_brightness"), max).expect("max");
        class_root
    }

    fn live_nogrant() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn network_serves_ethernet_and_strips_essid() {
        let root = temp_dir("net");
        let (class_net, proc_route, proc_dev) = wifi_fixture(&root);
        let bl_root = backlight_fixture(&root, "300\n", "1200\n");
        let mut host = DeviceHost::for_test(
            class_net,
            proc_route,
            proc_dev,
            Some("Home".to_string()),
            false,
            None,
            None,
            bl_root,
            false,
            None,
            0,
        );
        let granted = vec![
            super::super::NETWORK_READ_BROKER_HEAD.to_string(),
            super::super::ESSID_BROKER_HEAD.to_string(),
        ];
        let value = host
            .call(NETWORK_STATE_IFACE, "get", &LuaValue::Nil, &granted)
            .expect("network serves");
        assert_eq!(
            value.get("link"),
            Some(&LuaValue::String("wifi".to_string()))
        );
        assert_eq!(value.get("essid"), None, "base never carries essid");
        let essid = host
            .call(NETWORK_ESSID_IFACE, "get", &LuaValue::Nil, &granted)
            .expect("essid serves");
        assert_eq!(
            essid.get("essid"),
            Some(&LuaValue::String("Home".to_string()))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ungranted_hides_without_touching_sources() {
        let root = temp_dir("ungranted");
        let (class_net, proc_route, proc_dev) = net_fixture(&root);
        let bl_root = backlight_fixture(&root, "300\n", "1200\n");
        let mut host = DeviceHost::for_test(
            class_net,
            proc_route,
            proc_dev,
            Some("Home".to_string()),
            true,
            Some(true),
            Some(vec![(
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01".to_string(),
                RawBluetoothDevice {
                    address: "AA:BB:CC:DD:EE:01".to_string(),
                    alias: "Headphones".to_string(),
                    connected: true,
                    bonded: true,
                },
            )]),
            bl_root,
            true,
            Some(false),
            0,
        );
        for iface in [
            NETWORK_STATE_IFACE,
            NETWORK_ESSID_IFACE,
            BLUETOOTH_STATE_IFACE,
            BACKLIGHT_STATE_IFACE,
            IDLE_STATE_IFACE,
        ] {
            assert_eq!(
                host.call(iface, "get", &LuaValue::Nil, &live_nogrant())
                    .expect("ungranted hides"),
                LuaValue::Nil
            );
        }
        assert!(host.bluetooth_calls().is_empty());
        assert!(host.idle_calls().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bluetooth_and_power_round_trip_with_fixtures() {
        let root = temp_dir("bt-power");
        let (class_net, proc_route, proc_dev) = net_fixture(&root);
        let bl_root = backlight_fixture(&root, "300\n", "1200\n");
        let mut host = DeviceHost::for_test(
            class_net,
            proc_route,
            proc_dev,
            None,
            true,
            Some(true),
            Some(vec![(
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01".to_string(),
                RawBluetoothDevice {
                    address: "AA:BB:CC:DD:EE:01".to_string(),
                    alias: "Headphones".to_string(),
                    connected: true,
                    bonded: true,
                },
            )]),
            bl_root,
            true,
            Some(false),
            0,
        );
        let bt = host
            .call(
                BLUETOOTH_STATE_IFACE,
                "get",
                &LuaValue::Nil,
                &[super::super::BLUETOOTH_READ_BROKER_HEAD.to_string()],
            )
            .expect("bluetooth serves");
        assert_eq!(bt.get("powered"), Some(&LuaValue::Bool(true)));
        let bl = host
            .call(
                BACKLIGHT_STATE_IFACE,
                "get",
                &LuaValue::Nil,
                &[super::super::BACKLIGHT_READ_BROKER_HEAD.to_string()],
            )
            .expect("backlight serves");
        assert_eq!(bl.get("pct"), Some(&LuaValue::Integer(25)));
        let idle = host
            .call(
                IDLE_STATE_IFACE,
                "get",
                &LuaValue::Nil,
                &[super::super::IDLE_READ_BROKER_HEAD.to_string()],
            )
            .expect("idle serves");
        assert_eq!(idle.get("inhibited"), Some(&LuaValue::Bool(false)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn controls_enforce_grants_and_touch_backends_once() {
        let root = temp_dir("controls");
        let (class_net, proc_route, proc_dev) = net_fixture(&root);
        let bl_root = backlight_fixture(&root, "300\n", "1200\n");
        let mut host = DeviceHost::for_test(
            class_net,
            proc_route,
            proc_dev,
            None,
            false,
            None,
            None,
            bl_root,
            true,
            Some(false),
            0,
        );
        let denied = host
            .call(
                BLUETOOTH_CONTROL_IFACE,
                "set_power",
                &LuaValue::table([("on", LuaValue::Bool(true))]),
                &[],
            )
            .expect_err("ungranted control fails");
        assert_eq!(denied.code, "E_NOT_PERMITTED");
        assert!(host.bluetooth_calls().is_empty());
        let ok = host
            .call(
                BLUETOOTH_CONTROL_IFACE,
                "set_power",
                &LuaValue::table([("on", LuaValue::Bool(true))]),
                &[super::super::BLUETOOTH_CONTROL_BROKER_HEAD.to_string()],
            )
            .expect("granted sets");
        assert_eq!(ok.get("ok"), Some(&LuaValue::Bool(true)));
        assert_eq!(host.bluetooth_calls(), &[true]);
        let ok = host
            .call(
                IDLE_CONTROL_IFACE,
                "set",
                &LuaValue::table([("on", LuaValue::Bool(true))]),
                &[super::super::IDLE_CONTROL_BROKER_HEAD.to_string()],
            )
            .expect("idle sets");
        assert_eq!(ok.get("ok"), Some(&LuaValue::Bool(true)));
        assert!(host.idle_inhibited());
        let bad = host
            .call(
                BACKLIGHT_CONTROL_IFACE,
                "set_step",
                &LuaValue::table([("delta", LuaValue::Integer(500))]),
                &[super::super::BACKLIGHT_CONTROL_BROKER_HEAD.to_string()],
            )
            .expect_err("over-bound denied");
        assert_eq!(bad.code, "E_SERVICE_INVALID");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_method_or_iface_fails_closed_as_gone() {
        let mut host = DeviceHost::live();
        for (iface, method) in [(NETWORK_STATE_IFACE, "set"), ("device.unknown", "get")] {
            let error = host
                .call(iface, method, &LuaValue::Nil, &[])
                .expect_err("must fail closed");
            assert_eq!(error.code, "E_SERVICE_GONE");
        }
        let _ = Rc::new(RefCell::new(()));
    }
}
