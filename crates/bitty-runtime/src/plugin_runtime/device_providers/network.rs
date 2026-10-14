//! Network-state host bridge: live sysfs adapters plus the Lua table shape.
//!
//! Wraps the merged `bitty-platform` sampler (`CTX-1197`, bitty#2012) with
//! production I/O behind the injected seams and a deny-by-default grant
//! translation. The ESSID sub-grant is enforced end to end: without the
//! ESSID head the served Lua table carries no `essid` key at all (rest
//! unaffected), and the bus source is never touched.
//!
//! Live sources use only `std::fs` (existing seam, no new dependency):
//! link classification from `/sys/class/net`, counters from
//! `/proc/net/dev` text via the platform parser. Wi-Fi identity stays a
//! fail-closed stub ([`NoEssid`]): absent NM surfaces as no ESSID with
//! link/bandwidth unaffected, and the live NetworkManager D-Bus binding is
//! an explicit follow-up (no `zbus` edge in this slice; see the risk note
//! in the parent module).

#![forbid(unsafe_code)]

use std::path::Path;

use bitty_lua::LuaValue;
use bitty_platform::network_state::{
    EssidSource, LinkKind, NetClock, NetCounterSource, NetLinkSource, NetworkSnapshot,
    NetworkStateService, SysNetPaths, parse_proc_net_dev,
};
use bitty_platform::providers::GrantSet;

/// Translates broker-granted capability heads into the platform [`GrantSet`].
///
/// The broker head for the base read is `platform.network.read`; the ESSID
/// sub-grant arrives as the broker head `platform.essid.read` (grammar-bound
/// alias for the platform registry head `platform.network.essid.read`, which
/// carries four dot segments and cannot enter the closed capability set;
/// see the parent module). Both mappings are deny-by-default.
#[must_use]
pub fn grants_from_broker(granted: &[String]) -> GrantSet {
    let mut heads = Vec::new();
    for head in granted {
        match head.as_str() {
            "platform.network.read" => {
                heads.push(bitty_platform::providers::NETWORK_READ_HEAD.to_string());
            }
            "platform.essid.read" => {
                heads.push(bitty_platform::providers::NETWORK_ESSID_HEAD.to_string());
            }
            _ => {}
        }
    }
    GrantSet::new(heads)
}

/// Marshal one served [`NetworkSnapshot`] into the bar-facing Lua table.
///
/// Exact keys: `link` (`ethernet`|`wifi`|`disconnected`|`disabled`),
/// `stale_age_ms`, plus `down_bps`/`up_bps` only when measured and `essid`
/// only when the sub-grant held and the bus answered. Absent optionals are
/// omitted, never fabricated; an ungranted ESSID is indistinguishable from
/// an NM-less host (no `essid` key, rest unaffected).
#[must_use]
pub fn network_snapshot_to_lua(snapshot: &NetworkSnapshot) -> LuaValue {
    let link = match snapshot.link {
        LinkKind::Ethernet => "ethernet",
        LinkKind::Wifi => "wifi",
        LinkKind::Disconnected => "disconnected",
        LinkKind::Disabled => "disabled",
    };
    let mut pairs = vec![
        (
            LuaValue::String("link".to_string()),
            LuaValue::String(link.to_string()),
        ),
        (
            LuaValue::String("stale_age_ms".to_string()),
            LuaValue::Integer(snapshot.stale_age_ms as i64),
        ),
    ];
    if let Some(down) = snapshot.down_bps {
        pairs.push((
            LuaValue::String("down_bps".to_string()),
            LuaValue::Integer(down as i64),
        ));
    }
    if let Some(up) = snapshot.up_bps {
        pairs.push((
            LuaValue::String("up_bps".to_string()),
            LuaValue::Integer(up as i64),
        ));
    }
    if let Some(essid) = &snapshot.essid {
        pairs.push((
            LuaValue::String("essid".to_string()),
            LuaValue::String(essid.clone()),
        ));
    }
    LuaValue::Table(pairs)
}

/// Fail-closed Wi-Fi identity stub: NetworkManager absent (or live binding
/// deferred) surfaces as no ESSID with the rest of the snapshot intact.
#[derive(Debug, Default)]
pub struct NoEssid;

impl EssidSource for NoEssid {
    fn essid(&mut self, _ifname: &str) -> Option<String> {
        None
    }
}

/// Live link source over injected sysfs roots (production: [`SysNetPaths::system`]).
///
/// Reads are bounded and fail-closed: unreadable roots drive the
/// stale-then-hide path, never fabricated link claims. The default route is
/// derived from `/proc/net/route` (destination `00000000`, lowest metric);
/// tests inject fixture roots.
pub struct SysfsNetLink {
    class_net: std::path::PathBuf,
    proc_net_route: std::path::PathBuf,
}

/// Maximum `/proc/net/route` bytes parsed (fail-closed above).
const PROC_NET_ROUTE_MAX_BYTES: usize = 65_536;

/// Maximum route rows scanned (bounded enumeration).
const PROC_NET_ROUTE_MAX_ROWS: usize = 128;

impl SysfsNetLink {
    /// Binds the standard Linux roots.
    #[must_use]
    pub fn system() -> Self {
        Self {
            class_net: SysNetPaths::system().class_net,
            proc_net_route: std::path::PathBuf::from("/proc/net/route"),
        }
    }

    /// Binds injected roots (fixtures, tests).
    #[must_use]
    pub fn with_roots(class_net: std::path::PathBuf, proc_net_route: std::path::PathBuf) -> Self {
        Self {
            class_net,
            proc_net_route,
        }
    }

    fn read_default_iface(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.proc_net_route).ok()?;
        if text.len() > PROC_NET_ROUTE_MAX_BYTES {
            return None;
        }
        let mut best: Option<(String, u64)> = None;
        for line in text.lines().skip(1).take(PROC_NET_ROUTE_MAX_ROWS) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 8 {
                continue;
            }
            let iface = fields[0];
            let destination = fields[1];
            if destination != "00000000" {
                continue;
            }
            if iface.is_empty() || iface.len() > 16 || iface.contains('/') || iface.contains('\0') {
                continue;
            }
            let metric: u64 = fields[6].parse().unwrap_or(u64::MAX);
            match &best {
                Some((_, best_metric)) if metric >= *best_metric => {}
                _ => best = Some((iface.to_string(), metric)),
            }
        }
        best.map(|(iface, _)| iface)
    }
}

impl NetLinkSource for SysfsNetLink {
    fn available(&mut self) -> bool {
        self.class_net.is_dir()
    }

    fn default_route_iface(&mut self) -> Option<String> {
        self.read_default_iface()
    }

    fn operstate(&mut self, ifname: &str) -> Option<String> {
        if ifname.is_empty() || ifname.len() > 16 || ifname.contains('/') || ifname.contains('\0') {
            return None;
        }
        let path = Path::new(&self.class_net).join(ifname).join("operstate");
        std::fs::read_to_string(path).ok()
    }

    fn medium_is_wifi(&mut self, ifname: &str) -> Option<bool> {
        if ifname.is_empty() || ifname.len() > 16 || ifname.contains('/') || ifname.contains('\0') {
            return None;
        }
        let wireless = Path::new(&self.class_net).join(ifname).join("wireless");
        Some(wireless.exists())
    }

    fn networking_disabled(&mut self) -> bool {
        false
    }
}

/// Live counter source over an injected `/proc/net/dev` path (production:
/// the standard path; tests inject fixtures).
pub struct ProcNetDevCounters {
    path: std::path::PathBuf,
}

impl ProcNetDevCounters {
    /// Binds the standard Linux counter table.
    #[must_use]
    pub fn system() -> Self {
        Self {
            path: SysNetPaths::system().proc_net_dev,
        }
    }

    /// Binds an injected counter table (fixtures, tests).
    #[must_use]
    pub fn with_path(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl NetCounterSource for ProcNetDevCounters {
    fn counters(&mut self) -> Option<Vec<(String, u64, u64)>> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        Some(parse_proc_net_dev(&text))
    }
}

/// Monotonic host clock (milliseconds) for cadence control.
#[derive(Debug, Clone, Copy)]
pub struct HostClock;

impl NetClock for HostClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Host bridge: owns the platform sampler plus the live (or fixture)
/// sources, serving Lua tables with the grant translation applied.
///
/// Grants are re-read on every [`NetworkBridge::get`] via
/// [`NetworkBridge::set_grants`]; narrowing takes effect without restart.
pub struct NetworkBridge<L, C, E> {
    service: NetworkStateService,
    link: L,
    counters: C,
    essid: E,
}

impl<L, C, E> NetworkBridge<L, C, E>
where
    L: NetLinkSource,
    C: NetCounterSource,
    E: EssidSource,
{
    /// Binds the sampler to its sources with no grants (denies everything).
    pub fn new(link: L, counters: C, essid: E) -> Self {
        Self {
            service: NetworkStateService::new(),
            link,
            counters,
            essid,
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
    /// floored Lua table (ESSID key only when sub-granted and answered).
    pub fn get(&mut self, now_ms: u64, clock: &dyn NetClock) -> Option<LuaValue> {
        let _ = now_ms;
        let snapshot =
            self.service
                .get(clock, &mut self.link, &mut self.counters, &mut self.essid)?;
        Some(network_snapshot_to_lua(&snapshot))
    }

    /// Effective sampling interval in milliseconds.
    #[must_use]
    pub fn interval_ms(&self) -> u64 {
        self.service.interval_ms()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_platform::providers::{NETWORK_ESSID_HEAD, NETWORK_READ_HEAD};

    struct FakeClock {
        now: u64,
    }

    impl NetClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now
        }
    }

    struct FakeLink {
        available: bool,
        iface: Option<String>,
        operstate: Option<String>,
        wifi: Option<bool>,
    }

    impl NetLinkSource for FakeLink {
        fn available(&mut self) -> bool {
            self.available
        }
        fn default_route_iface(&mut self) -> Option<String> {
            self.iface.clone()
        }
        fn operstate(&mut self, _ifname: &str) -> Option<String> {
            self.operstate.clone()
        }
        fn medium_is_wifi(&mut self, _ifname: &str) -> Option<bool> {
            self.wifi
        }
        fn networking_disabled(&mut self) -> bool {
            false
        }
    }

    struct FakeCounters {
        table: Option<Vec<(String, u64, u64)>>,
    }

    impl NetCounterSource for FakeCounters {
        fn counters(&mut self) -> Option<Vec<(String, u64, u64)>> {
            self.table.clone()
        }
    }

    struct FakeEssid {
        value: Option<String>,
        calls: usize,
    }

    impl EssidSource for FakeEssid {
        fn essid(&mut self, _ifname: &str) -> Option<String> {
            self.calls += 1;
            self.value.clone()
        }
    }

    fn wifi_link() -> FakeLink {
        FakeLink {
            available: true,
            iface: Some("wlan0".to_string()),
            operstate: Some("up\n".to_string()),
            wifi: Some(true),
        }
    }

    #[test]
    fn broker_grant_translation_is_deny_by_default() {
        assert!(grants_from_broker(&[]).is_empty());
        let grants = grants_from_broker(&["platform.network.read".to_string()]);
        assert!(grants.contains(NETWORK_READ_HEAD));
        assert!(!grants.contains(NETWORK_ESSID_HEAD));
        let grants = grants_from_broker(&[
            "platform.network.read".to_string(),
            "platform.essid.read".to_string(),
        ]);
        assert!(grants.contains(NETWORK_READ_HEAD));
        assert!(grants.contains(NETWORK_ESSID_HEAD));
        let grants = grants_from_broker(&["platform.bluetooth.read".to_string()]);
        assert!(grants.is_empty(), "foreign heads never imply network");
    }

    #[test]
    fn ungranted_essid_omits_key_rest_unaffected() {
        let mut full = NetworkBridge::new(
            wifi_link(),
            FakeCounters {
                table: Some(vec![("wlan0".to_string(), 1_000, 2_000)]),
            },
            FakeEssid {
                value: Some("Home".to_string()),
                calls: 0,
            },
        );
        full.set_grants(GrantSet::new(vec![
            NETWORK_READ_HEAD.to_string(),
            NETWORK_ESSID_HEAD.to_string(),
        ]));
        let with = full.get(0, &FakeClock { now: 0 }).expect("granted serves");
        assert_eq!(
            with.get("essid"),
            Some(&LuaValue::String("Home".to_string()))
        );

        let mut base = NetworkBridge::new(
            wifi_link(),
            FakeCounters {
                table: Some(vec![("wlan0".to_string(), 1_000, 2_000)]),
            },
            FakeEssid {
                value: Some("Home".to_string()),
                calls: 0,
            },
        );
        base.set_grants(GrantSet::new(vec![NETWORK_READ_HEAD.to_string()]));
        let without = base.get(0, &FakeClock { now: 0 }).expect("base serves");
        assert_eq!(without.get("essid"), None, "no essid key when ungranted");
        assert_eq!(without.get("link"), with.get("link"));
        assert_eq!(without.get("down_bps"), with.get("down_bps"));
        assert_eq!(without.get("stale_age_ms"), with.get("stale_age_ms"));
    }

    #[test]
    fn nm_less_stub_serves_link_without_essid() {
        let mut bridge = NetworkBridge::new(
            wifi_link(),
            FakeCounters {
                table: Some(vec![("wlan0".to_string(), 1_000, 2_000)]),
            },
            NoEssid,
        );
        bridge.set_grants(GrantSet::new(vec![
            NETWORK_READ_HEAD.to_string(),
            NETWORK_ESSID_HEAD.to_string(),
        ]));
        let value = bridge
            .get(0, &FakeClock { now: 0 })
            .expect("NM-less serves");
        assert_eq!(value.get("essid"), None);
        assert_eq!(
            value.get("link"),
            Some(&LuaValue::String("wifi".to_string()))
        );
    }

    #[test]
    fn lua_shape_omits_unmeasured_rates() {
        let snapshot = NetworkSnapshot {
            link: LinkKind::Ethernet,
            down_bps: None,
            up_bps: None,
            essid: None,
            stale_age_ms: 0,
        };
        let value = network_snapshot_to_lua(&snapshot);
        assert_eq!(
            value.get("link"),
            Some(&LuaValue::String("ethernet".to_string()))
        );
        assert_eq!(value.get("down_bps"), None);
        assert_eq!(value.get("up_bps"), None);
        assert_eq!(value.get("essid"), None);
        assert_eq!(value.get("stale_age_ms"), Some(&LuaValue::Integer(0)));
    }

    #[test]
    fn sysfs_link_rejects_path_escape() {
        let link = SysfsNetLink::with_roots(
            std::path::PathBuf::from("/sys/class/net"),
            std::path::PathBuf::from("/proc/net/route"),
        );
        let mut link = link;
        assert_eq!(link.operstate("../evil"), None);
        assert_eq!(link.operstate(""), None);
        assert_eq!(link.medium_is_wifi("a/b"), None);
    }
}
