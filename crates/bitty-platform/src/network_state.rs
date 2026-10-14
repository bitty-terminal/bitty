#![forbid(unsafe_code)]
//! Network-state provider interface (CTX-1197, bitty#2012).
//!
//! Bar modules (`network` parity) consume link state plus bandwidth
//! counters; Wi-Fi identity (ESSID) flows only through the
//! separately-granted `network.essid` sub-interface backed by the
//! NetworkManager D-Bus where present. Absent NM (or an ungranted ESSID
//! head) surfaces as "no essid" with link/bandwidth unaffected, and an
//! ungranted read head is indistinguishable from an absent source: both
//! surface as `None`.
//!
//! Sources (all injected; this module performs no filesystem, D-Bus, or
//! network access itself):
//!
//! - link: one operstate read per default-route interface below
//!   [`SysNetPaths::class_net`] plus a medium hint (wired vs Wi-Fi). The
//!   live adapter (host task) derives these from
//!   `/sys/class/net/<iface>/operstate`, `/sys/class/net/<iface>/wireless`
//!   presence, and the default route; the standard roots live in exactly
//!   one place ([`SysNetPaths::system`]) and tests inject fixtures.
//! - counters: [`parse_proc_net_dev`] parses `/proc/net/dev` text into
//!   per-interface `(rx_bytes, tx_bytes)`; rates derive from consecutive
//!   samples with wrap-aware monotonicity (a wrapped counter re-baselines
//!   and reports unknown rates for that tick, never a spike or a
//!   negative).
//! - ESSID: [`EssidSource`] (NM D-Bus in the live adapter); `None` keeps
//!   the rest of the snapshot intact.
//!
//! Contract bounds: the provider enforces a 3 s poll floor
//! ([`NETWORK_POLL_FLOOR_MS`]); over-poll coalesces onto the held
//! snapshot (same precedent as [`crate::metrics`]). A lost source serves
//! the last good snapshot with a growing [`NetworkSnapshot::stale_age_ms`]
//! until [`NETWORK_STALE_HIDE_MS`], then hides (`None`) — stale-then-hide,
//! never fabricated zeros.
//!
//! Control operations (Wi-Fi on/off, connection-editor equivalents) are
//! explicitly out of scope: this provider is read-only by construction
//! (it exposes no control method and no control head), and any future
//! control is a separate control-head decision, not smuggled here.
//!
//! Bar mapping (Lua owns presentation): `link` selects the icon,
//! `down_bps`/`up_bps` format through Lua-side humanization (`None`
//! renders as the missing placeholder), `essid` appends when present.
//! No new bar API is required.

use super::providers::{GrantSet, NETWORK_ESSID_HEAD, NETWORK_READ_HEAD};

// ---------------------------------------------------------------------------
// Bounds (policy, timeout, and default values live here, never inline)
// ---------------------------------------------------------------------------

/// Provider-side poll floor: configuration can never sample faster.
pub const NETWORK_POLL_FLOOR_MS: u64 = 3000;

/// Default sampling interval (equal to the floor per the 3 s ceiling).
pub const NETWORK_DEFAULT_INTERVAL_MS: u64 = 3000;

/// Stale-then-hide bound: a lost source hides after this long without a
/// good sample (ten missed polls at the floor).
pub const NETWORK_STALE_HIDE_MS: u64 = 30_000;

/// Maximum interface-name bytes accepted from sysfs/`/proc/net/dev`
/// text (Linux `IFNAMSIZ` is 16 with the NUL; longer names are skipped,
/// never fatal).
pub const IFACE_NAME_MAX_BYTES: usize = 16;

/// Maximum ESSID bytes admitted (802.11 SSIDs cap at 32 bytes).
pub const ESSID_MAX_BYTES: usize = 32;

/// Maximum `/proc/net/dev` text bytes parsed (64 KiB; larger input is
/// rejected fail-closed so a hostile mount cannot bloat the parser).
pub const PROC_NET_DEV_MAX_BYTES: usize = 65_536;

/// Maximum interfaces retained from one `/proc/net/dev` parse (bounded
/// enumeration; extras are dropped after sorting for determinism).
pub const PROC_NET_DEV_MAX_IFACES: usize = 64;

// ---------------------------------------------------------------------------
// Paths (standard locations in exactly one place; tests inject fixtures)
// ---------------------------------------------------------------------------

/// Filesystem roots backing the link/counter sources.
///
/// The live adapter uses [`SysNetPaths::system`]; tests inject fixture
/// directories. Paths never appear as literals at call sites.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SysNetPaths {
    /// Root of per-interface state (`operstate`, `wireless` presence).
    pub class_net: std::path::PathBuf,
    /// Counter table Consistent with `/proc/net/dev` layout.
    pub proc_net_dev: std::path::PathBuf,
}

impl SysNetPaths {
    /// Standard Linux roots (portable contract paths, one place).
    #[must_use]
    pub fn system() -> Self {
        Self {
            class_net: std::path::PathBuf::from("/sys/class/net"),
            proc_net_dev: std::path::PathBuf::from("/proc/net/dev"),
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot types
// ---------------------------------------------------------------------------

/// Link classification served to bar modules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// Default route over a wired interface.
    Ethernet,
    /// Default route over a Wi-Fi interface.
    Wifi,
    /// No usable route (down, dormant, or no default interface).
    Disconnected,
    /// Networking administratively disabled (radio off / killed).
    Disabled,
}

/// Network-state snapshot: bounded primitives only, safe for bar text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkSnapshot {
    /// Classified link state.
    pub link: LinkKind,
    /// Downstream bits per second (`None` until two good samples, on
    /// counter wrap for that tick, or when counters are unavailable).
    pub down_bps: Option<u64>,
    /// Upstream bits per second (same `None` rules as `down_bps`).
    pub up_bps: Option<u64>,
    /// Wi-Fi identity, present only for Wi-Fi links with the ESSID head
    /// granted and an NM-backed source answering. `None` is
    /// indistinguishable across "absent NM", "ungranted", and "no value".
    pub essid: Option<String>,
    /// Milliseconds since the last good sample (0 on a fresh sample).
    pub stale_age_ms: u64,
}

/// Sysfs operstate vocabulary (parsed by [`parse_operstate`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperState {
    /// Interface reports `up`.
    Up,
    /// Interface reports `down`.
    Down,
    /// Interface reports `dormant` (associated but not passing traffic).
    Dormant,
    /// Anything else (unknown, testing, lower-layer-down, garbage).
    Unknown,
}

/// Parse one `operstate` file body into [`OperState`] (case-insensitive,
/// whitespace-tolerant; garbage maps to [`OperState::Unknown`]).
#[must_use]
pub fn parse_operstate(text: &str) -> OperState {
    match text.trim().to_ascii_lowercase().as_str() {
        "up" => OperState::Up,
        "down" => OperState::Down,
        "dormant" => OperState::Dormant,
        _ => OperState::Unknown,
    }
}

/// Parse `/proc/net/dev` text into `(ifname, rx_bytes, tx_bytes)` rows.
///
/// Skips the two header lines, skips malformed rows, drops over-long
/// names, sorts by name, and caps at [`PROC_NET_DEV_MAX_IFACES`].
/// Oversized input yields an empty table (fail-closed, never partial).
#[must_use]
pub fn parse_proc_net_dev(text: &str) -> Vec<(String, u64, u64)> {
    if text.len() > PROC_NET_DEV_MAX_BYTES {
        return Vec::new();
    }
    let mut rows: Vec<(String, u64, u64)> = text
        .lines()
        .skip(2)
        .filter_map(|line| {
            let (name, fields) = line.split_once(':')?;
            let name = name.trim();
            if name.is_empty() || name.len() > IFACE_NAME_MAX_BYTES {
                return None;
            }
            let fields: Vec<&str> = fields.split_whitespace().collect();
            if fields.len() < 9 {
                return None;
            }
            let rx = fields[0].parse::<u64>().ok()?;
            let tx = fields[8].parse::<u64>().ok()?;
            Some((name.to_string(), rx, tx))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.truncate(PROC_NET_DEV_MAX_IFACES);
    rows
}

/// Sanitize a raw ESSID into bar-safe text: trimmed, 1..=
/// [`ESSID_MAX_BYTES`] bytes at a char boundary, no control characters.
/// Anything else maps to `None` (indistinguishable from absent).
#[must_use]
pub fn sanitize_essid(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut end = trimmed.len().min(ESSID_MAX_BYTES);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let bounded = &trimmed[..end];
    if bounded.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(bounded.to_string())
}

// ---------------------------------------------------------------------------
// Source traits (injected; the live adapter is host work)
// ---------------------------------------------------------------------------

/// Link-state source: default route, operstate, medium, kill-switch.
pub trait NetLinkSource {
    /// Whether the backing root answers at all (`false` when the
    /// sysfs root is unreadable, e.g. a container without `/sys`).
    /// `false` on a due poll drives the stale-then-hide path.
    fn available(&mut self) -> bool;
    /// Default-route interface name (`None` = no route).
    fn default_route_iface(&mut self) -> Option<String>;
    /// Operstate body for `ifname` (`None` = unreadable).
    fn operstate(&mut self, ifname: &str) -> Option<String>;
    /// `true` when `ifname` is a Wi-Fi interface (`None` = unknown).
    ///
    /// Wi-Fi is the privileged classification (it alone permits an
    /// ESSID bus read), so it requires positive evidence: `None` and
    /// `Some(false)` both classify an up link as wired.
    fn medium_is_wifi(&mut self, ifname: &str) -> Option<bool>;
    /// `true` when networking is administratively disabled.
    fn networking_disabled(&mut self) -> bool;
}

/// Counter source: per-interface `(rx_bytes, tx_bytes)` table.
pub trait NetCounterSource {
    /// Current counter table (`None` = unreadable).
    fn counters(&mut self) -> Option<Vec<(String, u64, u64)>>;
}

/// Wi-Fi identity source (NetworkManager D-Bus in the live adapter).
///
/// `None` means "no value here" (NM absent, interface not Wi-Fi,
/// permission denied at the bus): the caller keeps the rest of the
/// snapshot and reports no ESSID.
pub trait EssidSource {
    /// ESSID for `ifname` (`None` = no value).
    fn essid(&mut self, ifname: &str) -> Option<String>;
}

/// Injected monotonic clock (milliseconds) for cadence control.
pub trait NetClock {
    /// Current time in milliseconds.
    fn now_ms(&self) -> u64;
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Network-state provider: rate-floored, grant-gated, fail-closed.
///
/// Grants are re-read on every [`NetworkStateService::get`] (narrowing
/// takes effect without restart). Without [`NETWORK_READ_HEAD`] the
/// answer is `None`, byte-identical to a missing source.
#[derive(Debug)]
pub struct NetworkStateService {
    interval_ms: u64,
    last_poll_ms: Option<u64>,
    last_sample_ms: Option<u64>,
    cached: Option<NetworkSnapshot>,
    prev_counters: Option<(String, u64, u64, u64)>,
    grants: GrantSet,
}

impl NetworkStateService {
    /// Creates the service with the default interval and no grants
    /// (denies everything until grants arrive).
    #[must_use]
    pub fn new() -> Self {
        Self {
            interval_ms: NETWORK_DEFAULT_INTERVAL_MS,
            last_poll_ms: None,
            last_sample_ms: None,
            cached: None,
            prev_counters: None,
            grants: GrantSet::default(),
        }
    }

    /// Creates the service with a custom interval, clamped up to the
    /// floor. Configuration can never request sub-3 s polling.
    #[must_use]
    pub fn with_interval(requested_ms: u64) -> Self {
        Self {
            interval_ms: requested_ms.max(NETWORK_POLL_FLOOR_MS),
            last_poll_ms: None,
            last_sample_ms: None,
            cached: None,
            prev_counters: None,
            grants: GrantSet::default(),
        }
    }

    /// Effective sampling interval in milliseconds.
    #[must_use]
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Replaces the granted-head snapshot (deny-by-default; narrowing
    /// takes effect on the next `get` with no restart).
    pub fn set_grants(&mut self, grants: GrantSet) {
        self.grants = grants;
    }

    /// Reads the provider: `None` when ungranted or hidden, otherwise
    /// the floored snapshot (fresh, held, stale, or re-hidden per the
    /// contract above).
    pub fn get(
        &mut self,
        clock: &dyn NetClock,
        link: &mut dyn NetLinkSource,
        counters: &mut dyn NetCounterSource,
        essid: &mut dyn EssidSource,
    ) -> Option<NetworkSnapshot> {
        if !self.grants.contains(NETWORK_READ_HEAD) {
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
        if !link.available() {
            return self.stale_or_hidden(now);
        }
        let snapshot = self.sample(now, link, counters, essid);
        self.last_sample_ms = Some(now);
        self.cached = Some(snapshot.clone());
        Some(snapshot)
    }

    /// Serves the held snapshot with a refreshed age (floored polls).
    fn held(&self, now: u64) -> Option<NetworkSnapshot> {
        let mut snapshot = self.cached.clone()?;
        let last = self.last_sample_ms?;
        snapshot.stale_age_ms = now.wrapping_sub(last);
        Some(snapshot)
    }

    /// Serves the last good snapshot while inside the stale bound, else
    /// hides (`None`) and clears the baseline.
    fn stale_or_hidden(&mut self, now: u64) -> Option<NetworkSnapshot> {
        let last = self.last_sample_ms?;
        let age = now.wrapping_sub(last);
        if age > NETWORK_STALE_HIDE_MS {
            self.cached = None;
            self.last_sample_ms = None;
            self.prev_counters = None;
            return None;
        }
        let mut snapshot = self.cached.clone()?;
        snapshot.stale_age_ms = age;
        Some(snapshot)
    }

    /// Takes one fresh sample (link classification, rates, ESSID).
    fn sample(
        &mut self,
        now: u64,
        link: &mut dyn NetLinkSource,
        counters: &mut dyn NetCounterSource,
        essid: &mut dyn EssidSource,
    ) -> NetworkSnapshot {
        if link.networking_disabled() {
            return NetworkSnapshot {
                link: LinkKind::Disabled,
                down_bps: None,
                up_bps: None,
                essid: None,
                stale_age_ms: 0,
            };
        }
        let Some(ifname) = link.default_route_iface() else {
            return NetworkSnapshot {
                link: LinkKind::Disconnected,
                down_bps: None,
                up_bps: None,
                essid: None,
                stale_age_ms: 0,
            };
        };
        let up = link
            .operstate(&ifname)
            .is_some_and(|body| parse_operstate(&body) == OperState::Up);
        if !up {
            // Down, dormant, unknown, or unreadable operstate never
            // claims connectivity: conservative disconnect, still served.
            return NetworkSnapshot {
                link: LinkKind::Disconnected,
                down_bps: None,
                up_bps: None,
                essid: None,
                stale_age_ms: 0,
            };
        }
        let wifi = link.medium_is_wifi(&ifname).unwrap_or(false);
        let (down_bps, up_bps) = self.rates(now, &ifname, counters);
        let essid_value = if wifi && self.grants.contains(NETWORK_ESSID_HEAD) {
            essid.essid(&ifname).and_then(|raw| sanitize_essid(&raw))
        } else {
            None
        };
        NetworkSnapshot {
            link: if wifi {
                LinkKind::Wifi
            } else {
                LinkKind::Ethernet
            },
            down_bps,
            up_bps,
            essid: essid_value,
            stale_age_ms: 0,
        }
    }

    /// Derives bit-rates from consecutive counter samples.
    ///
    /// First sample, interface change, unreadable table, zero elapsed
    /// time, and counter wrap all report unknown rates for the tick; a
    /// wrap additionally re-baselines so the next tick rates cleanly.
    /// Arithmetic runs in `u128` and saturates at `u64::MAX` (never a
    /// spike, never a panic).
    fn rates(
        &mut self,
        now: u64,
        ifname: &str,
        counters: &mut dyn NetCounterSource,
    ) -> (Option<u64>, Option<u64>) {
        let table = counters.counters();
        let current = table
            .as_ref()
            .and_then(|rows| rows.iter().find(|row| row.0 == ifname));
        let Some((_, rx, tx)) = current else {
            return (None, None);
        };
        let (prev_iface, prev_rx, prev_tx, prev_at) = self
            .prev_counters
            .clone()
            .unwrap_or_else(|| (String::new(), *rx, *tx, now));
        self.prev_counters = Some((ifname.to_string(), *rx, *tx, now));
        if prev_iface != ifname {
            return (None, None);
        }
        if *rx < prev_rx || *tx < prev_tx {
            return (None, None);
        }
        let elapsed = now.wrapping_sub(prev_at);
        if elapsed == 0 {
            return (None, None);
        }
        let down = to_bps(rx - prev_rx, elapsed);
        let up = to_bps(tx - prev_tx, elapsed);
        (Some(down), Some(up))
    }
}

impl Default for NetworkStateService {
    fn default() -> Self {
        Self::new()
    }
}

/// Converts a byte delta over `elapsed_ms` into bits per second,
/// saturating at `u64::MAX`.
fn to_bps(delta_bytes: u64, elapsed_ms: u64) -> u64 {
    let scaled = u128::from(delta_bytes) * 8_u128 * 1_000_u128 / u128::from(elapsed_ms);
    scaled.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::GrantSet;

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
        default_iface: Option<String>,
        operstate: Option<String>,
        is_wifi: Option<bool>,
        disabled: bool,
    }

    impl NetLinkSource for FakeLink {
        fn available(&mut self) -> bool {
            self.available
        }

        fn default_route_iface(&mut self) -> Option<String> {
            self.default_iface.clone()
        }

        fn operstate(&mut self, _ifname: &str) -> Option<String> {
            self.operstate.clone()
        }

        fn medium_is_wifi(&mut self, _ifname: &str) -> Option<bool> {
            self.is_wifi
        }

        fn networking_disabled(&mut self) -> bool {
            self.disabled
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

    fn read_grants() -> GrantSet {
        GrantSet::new(vec![NETWORK_READ_HEAD.to_string()])
    }

    fn read_and_essid_grants() -> GrantSet {
        GrantSet::new(vec![
            NETWORK_READ_HEAD.to_string(),
            NETWORK_ESSID_HEAD.to_string(),
        ])
    }

    fn wifi_link() -> FakeLink {
        FakeLink {
            available: true,
            default_iface: Some("wlan0".to_string()),
            operstate: Some("up\n".to_string()),
            is_wifi: Some(true),
            disabled: false,
        }
    }

    fn counters_for(rx: u64, tx: u64) -> FakeCounters {
        FakeCounters {
            table: Some(vec![("wlan0".to_string(), rx, tx)]),
        }
    }

    #[test]
    fn operstate_parsing_covers_sysfs_vocabulary() {
        assert_eq!(parse_operstate("up\n"), OperState::Up);
        assert_eq!(parse_operstate("UP"), OperState::Up);
        assert_eq!(parse_operstate("  down  \n"), OperState::Down);
        assert_eq!(parse_operstate("dormant\n"), OperState::Dormant);
        assert_eq!(parse_operstate("unknown\n"), OperState::Unknown);
        assert_eq!(parse_operstate("testing\n"), OperState::Unknown);
        assert_eq!(parse_operstate(""), OperState::Unknown);
        assert_eq!(parse_operstate("upward\n"), OperState::Unknown);
    }

    #[test]
    fn proc_net_dev_fixture_parses_counters() {
        let text = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n  wlan0: 1000       1    0    0    0     0          0         0     2000       2    0    0    0     0       0          0\n    lo: 500       5    0    0    0     0          0         0      500       5    0    0    0     0       0          0\n";
        let rows = parse_proc_net_dev(text);
        assert_eq!(
            rows,
            vec![
                ("lo".to_string(), 500, 500),
                ("wlan0".to_string(), 1000, 2000),
            ]
        );
    }

    #[test]
    fn proc_net_dev_skips_malformed_and_caps_names() {
        let long_name = "x".repeat(IFACE_NAME_MAX_BYTES + 1);
        let text = format!(
            "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n  {long_name}: 1 0 0 0 0 0 0 0 2 0 0 0 0 0 0 0\n  good0: 3 0 0 0 0 0 0 0 4 0 0 0 0 0 0 0\n  badline-without-colon\n  eh0: not-a-number 0 0 0 0 0 0 0 4 0 0 0 0 0 0 0\n"
        );
        let rows = parse_proc_net_dev(&text);
        assert_eq!(rows, vec![("good0".to_string(), 3, 4)]);
    }

    #[test]
    fn proc_net_dev_rejects_oversized_input_fail_closed() {
        let big = "x".repeat(PROC_NET_DEV_MAX_BYTES + 1);
        assert!(parse_proc_net_dev(&big).is_empty());
    }

    #[test]
    fn essid_sanitizer_bounds_and_rejects_controls() {
        assert_eq!(
            sanitize_essid("  Home Net \n"),
            Some("Home Net".to_string())
        );
        assert_eq!(sanitize_essid(""), None);
        assert_eq!(sanitize_essid("   "), None);
        assert_eq!(sanitize_essid("a\tb"), None);
        assert_eq!(sanitize_essid("a\x07b"), None);
        let long = "e".repeat(ESSID_MAX_BYTES + 10);
        assert_eq!(sanitize_essid(&long), Some("e".repeat(ESSID_MAX_BYTES)));
        let multibyte = "e".repeat(ESSID_MAX_BYTES - 1) + "\u{00e9}\u{00e9}";
        let cleaned = sanitize_essid(&multibyte).expect("bounded essid");
        assert!(cleaned.len() <= ESSID_MAX_BYTES);
        assert!(cleaned.is_char_boundary(cleaned.len()));
    }

    #[test]
    fn ungranted_read_is_indistinguishable_from_absent() {
        let mut denied = NetworkStateService::new();
        let clock = FakeClock { now: 0 };
        let mut link = wifi_link();
        let mut counters = counters_for(1000, 2000);
        let mut essid = FakeEssid {
            value: Some("Home".to_string()),
            calls: 0,
        };
        assert_eq!(
            denied.get(&clock, &mut link, &mut counters, &mut essid),
            None
        );
        assert_eq!(essid.calls, 0, "denied reads must not touch the bus");
    }

    #[test]
    fn nm_less_fixture_serves_link_and_bandwidth_without_essid() {
        let mut service = NetworkStateService::new();
        service.set_grants(read_and_essid_grants());
        let mut link = wifi_link();
        let mut essid = FakeEssid {
            value: None,
            calls: 0,
        };
        let mut first_counters = counters_for(1_000, 2_000);
        let first = service
            .get(
                &FakeClock { now: 0 },
                &mut link,
                &mut first_counters,
                &mut essid,
            )
            .expect("link serves on first sample");
        assert_eq!(first.link, LinkKind::Wifi);
        assert_eq!(first.down_bps, None, "first sample cannot rate");
        assert_eq!(first.up_bps, None);
        assert_eq!(first.essid, None, "NM-less host reports no essid");
        assert_eq!(first.stale_age_ms, 0);
        let mut second_counters = counters_for(4_000, 5_000);
        let second = service
            .get(
                &FakeClock {
                    now: NETWORK_POLL_FLOOR_MS,
                },
                &mut link,
                &mut second_counters,
                &mut essid,
            )
            .expect("second sample rates");
        assert_eq!(second.down_bps, Some(8_000));
        assert_eq!(second.up_bps, Some(8_000));
        assert_eq!(second.essid, None);
    }

    #[test]
    fn ungranted_essid_matches_nm_less_output() {
        let essid_value = Some("Home".to_string());
        let mut granted_full = NetworkStateService::new();
        granted_full.set_grants(read_and_essid_grants());
        let mut link = wifi_link();
        let mut essid = FakeEssid {
            value: essid_value,
            calls: 0,
        };
        let mut counters = counters_for(1_000, 2_000);
        let with_essid = granted_full
            .get(&FakeClock { now: 0 }, &mut link, &mut counters, &mut essid)
            .expect("serves");
        assert_eq!(with_essid.essid, Some("Home".to_string()));

        let mut granted_base = NetworkStateService::new();
        granted_base.set_grants(read_grants());
        let mut link = wifi_link();
        let mut essid = FakeEssid {
            value: Some("Home".to_string()),
            calls: 0,
        };
        let mut counters = counters_for(1_000, 2_000);
        let without_essid = granted_base
            .get(&FakeClock { now: 0 }, &mut link, &mut counters, &mut essid)
            .expect("serves");
        assert_eq!(without_essid.essid, None);
        assert_eq!(essid.calls, 0, "ungranted ESSID must not touch the bus");
        // Same snapshot modulo the essid field: ungranted == absent.
        assert_eq!(without_essid.link, with_essid.link);
        assert_eq!(without_essid.down_bps, with_essid.down_bps);
    }

    #[test]
    fn counter_wrap_rebaselines_without_spike() {
        let mut service = NetworkStateService::new();
        service.set_grants(read_grants());
        let mut link = wifi_link();
        let mut essid = FakeEssid {
            value: None,
            calls: 0,
        };
        let mut near_max = counters_for(u64::MAX - 10, u64::MAX - 20);
        service
            .get(&FakeClock { now: 0 }, &mut link, &mut near_max, &mut essid)
            .expect("first sample");
        // Counters wrapped past u64::MAX: rates unknown this tick, never a spike.
        let mut wrapped = counters_for(5, 10);
        let tick = service
            .get(
                &FakeClock {
                    now: NETWORK_POLL_FLOOR_MS,
                },
                &mut link,
                &mut wrapped,
                &mut essid,
            )
            .expect("wrap tick serves");
        assert_eq!(tick.down_bps, None);
        assert_eq!(tick.up_bps, None);
        // Next tick rates from the new baseline.
        let mut after = counters_for(3_005, 3_010);
        let next = service
            .get(
                &FakeClock {
                    now: 2 * NETWORK_POLL_FLOOR_MS,
                },
                &mut link,
                &mut after,
                &mut essid,
            )
            .expect("post-wrap tick serves");
        assert_eq!(next.down_bps, Some(8_000));
        assert_eq!(next.up_bps, Some(8_000));
    }

    #[test]
    fn link_classification_covers_contract_states() {
        let mut service = NetworkStateService::new();
        service.set_grants(read_grants());
        let mut essid = FakeEssid {
            value: None,
            calls: 0,
        };
        let mut counters = FakeCounters {
            table: Some(vec![]),
        };
        // Disabled wins over everything.
        let mut link = FakeLink {
            available: true,
            default_iface: Some("wlan0".to_string()),
            operstate: Some("up".to_string()),
            is_wifi: Some(true),
            disabled: true,
        };
        let snapshot = service
            .get(&FakeClock { now: 0 }, &mut link, &mut counters, &mut essid)
            .expect("disabled serves");
        assert_eq!(snapshot.link, LinkKind::Disabled);
        // No route disconnects.
        let mut service = NetworkStateService::new();
        service.set_grants(read_grants());
        let mut link = FakeLink {
            available: true,
            default_iface: None,
            operstate: None,
            is_wifi: None,
            disabled: false,
        };
        let snapshot = service
            .get(&FakeClock { now: 0 }, &mut link, &mut counters, &mut essid)
            .expect("no-route serves");
        assert_eq!(snapshot.link, LinkKind::Disconnected);
        // Down operstate disconnects.
        let mut service = NetworkStateService::new();
        service.set_grants(read_grants());
        let mut link = FakeLink {
            available: true,
            default_iface: Some("eth0".to_string()),
            operstate: Some("down".to_string()),
            is_wifi: Some(false),
            disabled: false,
        };
        let snapshot = service
            .get(&FakeClock { now: 0 }, &mut link, &mut counters, &mut essid)
            .expect("down serves");
        assert_eq!(snapshot.link, LinkKind::Disconnected);
        // Wired up classifies ethernet.
        let mut service = NetworkStateService::new();
        service.set_grants(read_grants());
        let mut link = FakeLink {
            available: true,
            default_iface: Some("eth0".to_string()),
            operstate: Some("up".to_string()),
            is_wifi: Some(false),
            disabled: false,
        };
        let snapshot = service
            .get(&FakeClock { now: 0 }, &mut link, &mut counters, &mut essid)
            .expect("ethernet serves");
        assert_eq!(snapshot.link, LinkKind::Ethernet);
        assert_eq!(snapshot.essid, None, "wired links never carry an essid");
    }

    #[test]
    fn over_poll_coalesces_and_source_loss_hides_after_bound() {
        let mut service = NetworkStateService::new();
        service.set_grants(read_grants());
        let mut link = wifi_link();
        let mut essid = FakeEssid {
            value: None,
            calls: 0,
        };
        let mut counters = counters_for(1_000, 2_000);
        let first = service
            .get(&FakeClock { now: 0 }, &mut link, &mut counters, &mut essid)
            .expect("first serves");
        assert_eq!(first.stale_age_ms, 0);
        // Over-poll inside the floor: held snapshot, age advances, no resample.
        let mut counters = counters_for(99_000, 99_000);
        let held = service
            .get(
                &FakeClock { now: 1_000 },
                &mut link,
                &mut counters,
                &mut essid,
            )
            .expect("held serves");
        assert_eq!(held.stale_age_ms, 1_000);
        assert_eq!(held.down_bps, first.down_bps);
        // Source lost: stale served with age until the bound, then hidden.
        let mut dead_counters = FakeCounters { table: None };
        let mut dead_link = FakeLink {
            available: false,
            default_iface: None,
            operstate: None,
            is_wifi: None,
            disabled: false,
        };
        let stale = service
            .get(
                &FakeClock {
                    now: NETWORK_POLL_FLOOR_MS,
                },
                &mut dead_link,
                &mut dead_counters,
                &mut essid,
            )
            .expect("stale serves before the bound");
        assert!(stale.stale_age_ms >= NETWORK_POLL_FLOOR_MS);
        let hidden = service.get(
            &FakeClock {
                now: NETWORK_STALE_HIDE_MS + NETWORK_POLL_FLOOR_MS,
            },
            &mut dead_link,
            &mut dead_counters,
            &mut essid,
        );
        assert_eq!(hidden, None, "lost source hides after the bound");
    }

    #[test]
    fn interval_clamps_up_to_floor() {
        assert_eq!(
            NetworkStateService::with_interval(100).interval_ms(),
            NETWORK_POLL_FLOOR_MS
        );
        assert_eq!(
            NetworkStateService::with_interval(9_000).interval_ms(),
            9_000
        );
        assert_eq!(
            NetworkStateService::new().interval_ms(),
            NETWORK_DEFAULT_INTERVAL_MS
        );
    }

    #[test]
    fn grant_narrowing_takes_effect_without_restart() {
        let mut service = NetworkStateService::new();
        service.set_grants(read_grants());
        let mut link = wifi_link();
        let mut essid = FakeEssid {
            value: None,
            calls: 0,
        };
        let mut counters = counters_for(1_000, 2_000);
        assert!(
            service
                .get(&FakeClock { now: 0 }, &mut link, &mut counters, &mut essid)
                .is_some()
        );
        service.set_grants(GrantSet::default());
        let mut counters = counters_for(1_000, 2_000);
        assert_eq!(
            service.get(&FakeClock { now: 0 }, &mut link, &mut counters, &mut essid),
            None,
            "narrowed grants fail closed on the live service"
        );
    }
}
