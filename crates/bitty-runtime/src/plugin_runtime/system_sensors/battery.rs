//! Battery-state provider for bar modules (`battery` parity, bitty#2016).
//!
//! Sources (Linux): UPower D-Bus preferred, `/sys/class/power_supply`
//! fallback. Desktops without either hide fail-closed. The provider returns
//! capacity/state/rate numbers only; icon-bucket selection (charging vs
//! default glyph arrays) lives in Lua policy, never here.
//!
//! UPower transport note: the [`UpowerSource`] seam carries the preferred
//! path (bus name [`UPOWER_BUS_NAME`], DisplayDevice
//! [`UPOWER_DISPLAY_PATH`]) with [`NoUpower`] and fixture fakes behind it.
//! The live D-Bus call itself is a scoped follow-up (one bus name, no broad
//! `org.freedesktop` proxy); this revision ships the sysfs fallback live
//! and proves preference/selection/ceiling hermetically.
//!
//! Policy (shared plumbing in [`super::common`]): default 5s provider-side
//! ceiling with over-poll coalescing (the fallback path keeps its own
//! ceiling, so UPower-less systems never storm), stale-then-hide past three
//! windows, and fail-closed grants (ungranted reads are indistinguishable
//! from an absent source).

use std::path::PathBuf;

use bitty_lua::LuaValue;

use super::common::{RateGate, SLOW_SENSOR_CEILING_MS, StalePolicy, horizon_for_ceiling};

/// Contract root for the Linux sysfs battery source (exactly one place).
pub const SYSFS_POWER_SUPPLY_PATH: &str = "/sys/class/power_supply";

/// Spec-defined UPower bus name (exactly one place; no broad proxy).
pub const UPOWER_BUS_NAME: &str = "org.freedesktop.UPower";

/// Spec-defined UPower composite-battery object path (exactly one place).
pub const UPOWER_DISPLAY_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

/// Battery states served to bar modules (issue #2016 vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatteryState {
    /// Actively charging.
    Charging,
    /// Actively discharging.
    Discharging,
    /// Charged and holding.
    Full,
    /// Connected without charging or discharging (held, pending, or idle).
    Plugged,
    /// No trustworthy state (unknown source value or absent sensor).
    #[default]
    Unknown,
}

impl BatteryState {
    /// Bar-facing state string (exact vocabulary, stable for Lua matching).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Charging => "charging",
            Self::Discharging => "discharging",
            Self::Full => "full",
            Self::Plugged => "plugged",
            Self::Unknown => "unknown",
        }
    }

    /// Map a `/sys/class/power_supply/*/status` value.
    ///
    /// Kernel vocabulary: `Charging`, `Discharging`, `Full`, `Not charging`,
    /// `Unknown`. `Not charging` (plugged, held below threshold, idle) maps
    /// to `plugged`; anything unrecognized maps to `unknown`, never to a
    /// guessed sibling.
    #[must_use]
    pub fn from_sysfs_status(status: &str) -> Self {
        match status.trim() {
            "Charging" => Self::Charging,
            "Discharging" => Self::Discharging,
            "Full" => Self::Full,
            "Not charging" => Self::Plugged,
            _ => Self::Unknown,
        }
    }

    /// Map a UPower `State` enum value (`UP_DEVICE_STATE_*`).
    ///
    /// `1` charging, `2` discharging, `4` fully charged, `5`/`6`
    /// pending-charge/discharge (connected, held) map to `plugged`;
    /// `0` unknown, `3` empty, and any out-of-range value map to `unknown`.
    #[must_use]
    pub fn from_upower_state(state: u32) -> Self {
        match state {
            1 => Self::Charging,
            2 => Self::Discharging,
            4 => Self::Full,
            5 | 6 => Self::Plugged,
            _ => Self::Unknown,
        }
    }
}

/// One raw sysfs battery candidate (file contents, unparsed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysfsBatteryRaw {
    /// Supply entry name (e.g. `BAT0`); selection order is by name.
    pub name: String,
    /// Raw `capacity` file (percent integer).
    pub capacity_text: Option<String>,
    /// Raw `status` file (kernel vocabulary).
    pub status_text: Option<String>,
    /// Raw `power_now` file (microwatts).
    pub power_uw_text: Option<String>,
}

/// Sysfs battery source seam (real fs vs fixtures).
pub trait SysfsBatterySource {
    /// Read every supply candidate (unparsed; selection stays in the provider).
    fn sample(&self) -> Vec<SysfsBatteryRaw>;
}

/// Live `/sys/class/power_supply` reader.
#[derive(Debug, Clone)]
pub struct FsPowerSupply {
    root: PathBuf,
}

impl FsPowerSupply {
    /// Reader over the contract root.
    #[must_use]
    pub fn system() -> Self {
        Self {
            root: PathBuf::from(SYSFS_POWER_SUPPLY_PATH),
        }
    }

    /// Reader over an explicit root (tests, containers).
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    /// Read one supply file; absent/unreadable stays `None` (fail-closed).
    fn read_entry(&self, name: &str, file: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(name).join(file)).ok()
    }
}

impl SysfsBatterySource for FsPowerSupply {
    fn sample(&self) -> Vec<SysfsBatteryRaw> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for entry in entries.flatten() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        names
            .into_iter()
            .map(|name| SysfsBatteryRaw {
                capacity_text: self.read_entry(&name, "capacity"),
                status_text: self.read_entry(&name, "status"),
                power_uw_text: self.read_entry(&name, "power_now"),
                name,
            })
            .collect()
    }
}

/// One UPower DisplayDevice reading.
#[derive(Debug, Clone, PartialEq)]
pub struct UpowerSample {
    /// Whether UPower answered at all (`false` = fall through to sysfs).
    pub present: bool,
    /// Composite percentage (`0..100`; clamped at serve time).
    pub capacity_pct: f64,
    /// Raw UPower `State` enum value.
    pub state_raw: u32,
    /// Energy rate in watts, when reported.
    pub power_w: Option<f64>,
    /// Seconds to empty, when reported.
    pub seconds_to_empty: Option<u64>,
}

/// UPower source seam (live D-Bus vs absent vs fixtures).
pub trait UpowerSource {
    /// Read the composite battery device.
    fn sample(&self) -> UpowerSample;
}

/// UPower-less system: always absent, so the sysfs fallback (with its own
/// ceiling) serves instead. No polling storm is possible: absence costs one
/// cheap check per window, proven by test.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoUpower;

impl UpowerSource for NoUpower {
    fn sample(&self) -> UpowerSample {
        UpowerSample {
            present: false,
            capacity_pct: 0.0,
            state_raw: 0,
            power_w: None,
            seconds_to_empty: None,
        }
    }
}

/// One servable battery snapshot (numbers/states only, no icon policy).
#[derive(Debug, Clone, PartialEq)]
pub struct BatterySnapshot {
    /// Capacity percent in `[0, 100]`.
    pub capacity_pct: f64,
    /// Charge state.
    pub state: BatteryState,
    /// Discharge/charge rate in watts, when the source reports it.
    pub power_w: Option<f64>,
    /// Seconds to empty, when the source reports it.
    pub seconds_to_empty: Option<u64>,
    /// Milliseconds since the serving sample (0 when freshly sampled).
    pub stale_age_ms: u64,
}

impl BatterySnapshot {
    /// Marshal into the bar-facing Lua table.
    ///
    /// Exact keys: `capacity_pct`, `state`, `stale_age_ms`, plus `power_w`
    /// and `seconds_to_empty` only when reported. Absent optionals are
    /// omitted, never fabricated; icon buckets stay in Lua (pinned by test).
    #[must_use]
    pub fn to_lua_value(&self) -> LuaValue {
        let mut pairs = vec![
            (
                LuaValue::String("capacity_pct".to_string()),
                LuaValue::Number(self.capacity_pct),
            ),
            (
                LuaValue::String("state".to_string()),
                LuaValue::String(self.state.as_str().to_string()),
            ),
        ];
        if let Some(power_w) = self.power_w {
            pairs.push((
                LuaValue::String("power_w".to_string()),
                LuaValue::Number(power_w),
            ));
        }
        if let Some(seconds) = self.seconds_to_empty {
            pairs.push((
                LuaValue::String("seconds_to_empty".to_string()),
                LuaValue::Integer(seconds as i64),
            ));
        }
        pairs.push((
            LuaValue::String("stale_age_ms".to_string()),
            LuaValue::Integer(self.stale_age_ms as i64),
        ));
        LuaValue::Table(pairs)
    }
}

/// Parsed, servable battery values (before staleness is attached).
#[derive(Debug, Clone, Copy, PartialEq)]
struct BatteryValues {
    capacity_pct: f64,
    state: BatteryState,
    power_w: Option<f64>,
    seconds_to_empty: Option<u64>,
}

/// Parse one sysfs candidate; `None` when its capacity is unusable.
///
/// A garbled capacity disqualifies only its own entry (the provider tries
/// the next battery by name); power glitches clear the optional field
/// without failing the snapshot.
fn parse_sysfs_candidate(raw: &SysfsBatteryRaw) -> Option<BatteryValues> {
    let capacity_pct: f64 = raw.capacity_text.as_deref()?.trim().parse().ok()?;
    if !(0.0..=100.0).contains(&capacity_pct) {
        return None;
    }
    let state = raw
        .status_text
        .as_deref()
        .map_or(BatteryState::Unknown, BatteryState::from_sysfs_status);
    let power_w = raw
        .power_uw_text
        .as_deref()
        .and_then(|text| text.trim().parse::<f64>().ok())
        .and_then(|uw| {
            if uw.is_finite() && uw >= 0.0 {
                Some(uw / 1_000_000.0)
            } else {
                None
            }
        });
    Some(BatteryValues {
        capacity_pct,
        state,
        power_w,
        seconds_to_empty: None,
    })
}

/// Validate one UPower sample; `None` when it carries nothing servable.
fn parse_upower_sample(sample: &UpowerSample) -> Option<BatteryValues> {
    if !sample.present || !sample.capacity_pct.is_finite() {
        return None;
    }
    Some(BatteryValues {
        capacity_pct: sample.capacity_pct.clamp(0.0, 100.0),
        state: BatteryState::from_upower_state(sample.state_raw),
        power_w: sample.power_w.filter(|w| w.is_finite() && *w >= 0.0),
        seconds_to_empty: sample.seconds_to_empty,
    })
}

/// UPower-preferred, sysfs-fallback battery provider.
pub struct BatteryProvider<U, S> {
    upower: U,
    sysfs: S,
    gate: RateGate,
    policy: StalePolicy,
    last: Option<BatteryValues>,
    last_success_ms: u64,
}

impl<U: UpowerSource, S: SysfsBatterySource> BatteryProvider<U, S> {
    /// Create a provider with the default 5s policy.
    pub fn new(upower: U, sysfs: S) -> Self {
        Self::with_policy(upower, sysfs, SLOW_SENSOR_CEILING_MS)
    }

    /// Create a provider with an explicit ceiling (tests); the stale
    /// horizon stays at three windows.
    pub fn with_policy(upower: U, sysfs: S, ceiling_ms: u64) -> Self {
        Self {
            upower,
            sysfs,
            gate: RateGate::new(ceiling_ms),
            policy: StalePolicy::new(horizon_for_ceiling(ceiling_ms)),
            last: None,
            last_success_ms: 0,
        }
    }

    /// Read the current snapshot.
    ///
    /// Shares the cpu provider contract: ungranted calls return `None`
    /// without touching either source; polls inside the ceiling serve the
    /// cached sample; a lost source serves last-good while fresh, then
    /// hides. UPower wins when it yields a valid sample, otherwise the
    /// first valid sysfs battery by name serves.
    pub fn get(&mut self, now_ms: u64, granted: bool) -> Option<BatterySnapshot> {
        if !granted {
            return None;
        }
        if self.gate.due(now_ms) {
            self.gate.mark_attempt(now_ms);
            if let Some(values) = self.sample_sources() {
                self.last = Some(values);
                self.last_success_ms = now_ms;
            }
        }
        let values = self.last?;
        let stale_age_ms = StalePolicy::age_ms(self.last_success_ms, now_ms);
        if !self.policy.visible(stale_age_ms) {
            return None;
        }
        Some(BatterySnapshot {
            capacity_pct: values.capacity_pct,
            state: values.state,
            power_w: values.power_w,
            seconds_to_empty: values.seconds_to_empty,
            stale_age_ms,
        })
    }

    /// Sample UPower first, then sysfs by name.
    fn sample_sources(&self) -> Option<BatteryValues> {
        if let Some(values) = parse_upower_sample(&self.upower.sample()) {
            return Some(values);
        }
        let mut candidates = self.sysfs.sample();
        candidates.sort_by(|a, b| a.name.cmp(&b.name));
        candidates.iter().find_map(parse_sysfs_candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct FakeUpower {
        sample: UpowerSample,
        calls: Rc<Cell<usize>>,
    }

    impl FakeUpower {
        fn serving(capacity_pct: f64, state_raw: u32) -> Self {
            Self {
                sample: UpowerSample {
                    present: true,
                    capacity_pct,
                    state_raw,
                    power_w: Some(7.5),
                    seconds_to_empty: Some(3600),
                },
                calls: Rc::new(Cell::new(0)),
            }
        }

        fn absent() -> Self {
            Self {
                sample: UpowerSample {
                    present: false,
                    capacity_pct: 0.0,
                    state_raw: 0,
                    power_w: None,
                    seconds_to_empty: None,
                },
                calls: Rc::new(Cell::new(0)),
            }
        }
    }

    impl UpowerSource for FakeUpower {
        fn sample(&self) -> UpowerSample {
            self.calls.set(self.calls.get() + 1);
            self.sample.clone()
        }
    }

    struct FakeSysfs {
        raws: Vec<SysfsBatteryRaw>,
        calls: Rc<Cell<usize>>,
    }

    impl FakeSysfs {
        fn batteries(raws: Vec<(&str, &str, &str)>) -> Self {
            Self {
                raws: raws
                    .into_iter()
                    .map(|(name, capacity, status)| SysfsBatteryRaw {
                        name: name.to_string(),
                        capacity_text: Some(capacity.to_string()),
                        status_text: Some(status.to_string()),
                        power_uw_text: Some("5000000".to_string()),
                    })
                    .collect(),
                calls: Rc::new(Cell::new(0)),
            }
        }

        fn empty() -> Self {
            Self {
                raws: Vec::new(),
                calls: Rc::new(Cell::new(0)),
            }
        }
    }

    impl SysfsBatterySource for FakeSysfs {
        fn sample(&self) -> Vec<SysfsBatteryRaw> {
            self.calls.set(self.calls.get() + 1);
            self.raws.clone()
        }
    }

    fn raw(name: &str, capacity: Option<&str>, status: Option<&str>) -> SysfsBatteryRaw {
        SysfsBatteryRaw {
            name: name.to_string(),
            capacity_text: capacity.map(str::to_string),
            status_text: status.map(str::to_string),
            power_uw_text: None,
        }
    }

    #[test]
    fn all_five_states_plus_missing_source_are_covered() {
        // Sysfs vocabulary row.
        for (status, state) in [
            ("Charging", BatteryState::Charging),
            ("Discharging", BatteryState::Discharging),
            ("Full", BatteryState::Full),
            ("Not charging", BatteryState::Plugged),
            ("Unknown", BatteryState::Unknown),
            ("Calibrating", BatteryState::Unknown),
        ] {
            let provider = BatteryProvider::with_policy(
                FakeUpower::absent(),
                FakeSysfs::batteries(vec![("BAT0", "42", status)]),
                5000,
            );
            let mut provider = provider;
            let snapshot = provider.get(0, true).expect("sysfs serves");
            assert_eq!(snapshot.state, state, "sysfs '{status}'");
            assert!((snapshot.capacity_pct - 42.0).abs() < 1e-9);
        }
        // UPower vocabulary row.
        for (raw, state) in [
            (1_u32, BatteryState::Charging),
            (2, BatteryState::Discharging),
            (4, BatteryState::Full),
            (5, BatteryState::Plugged),
            (6, BatteryState::Plugged),
            (0, BatteryState::Unknown),
            (3, BatteryState::Unknown),
            (99, BatteryState::Unknown),
        ] {
            let mut provider = BatteryProvider::with_policy(
                FakeUpower::serving(55.0, raw),
                FakeSysfs::empty(),
                5000,
            );
            let snapshot = provider.get(0, true).expect("upower serves");
            assert_eq!(snapshot.state, state, "upower state {raw}");
        }
        // Missing source hides.
        let mut missing =
            BatteryProvider::with_policy(FakeUpower::absent(), FakeSysfs::empty(), 5000);
        assert!(missing.get(0, true).is_none());
    }

    #[test]
    fn upower_preferred_over_sysfs() {
        let mut provider = BatteryProvider::with_policy(
            FakeUpower::serving(80.0, 1),
            FakeSysfs::batteries(vec![("BAT0", "10", "Discharging")]),
            5000,
        );
        let snapshot = provider.get(0, true).expect("serves");
        assert!((snapshot.capacity_pct - 80.0).abs() < 1e-9, "upower wins");
        assert_eq!(snapshot.state, BatteryState::Charging);
        assert_eq!(snapshot.power_w, Some(7.5));
        assert_eq!(snapshot.seconds_to_empty, Some(3600));
    }

    #[test]
    fn upower_less_system_uses_sysfs_with_its_own_ceiling() {
        let sysfs = FakeSysfs::batteries(vec![("BAT0", "63", "Discharging")]);
        let sysfs_calls = Rc::clone(&sysfs.calls);
        let mut provider = BatteryProvider::with_policy(FakeUpower::absent(), sysfs, 5000);
        let first = provider.get(0, true).expect("fallback serves");
        assert!((first.capacity_pct - 63.0).abs() < 1e-9);
        assert!(first.seconds_to_empty.is_none(), "sysfs reports no ETA");
        // power_now 5,000,000 uW renders as 5 W.
        assert!((first.power_w.expect("power serves") - 5.0).abs() < 1e-9);
        assert_eq!(sysfs_calls.get(), 1);
        for now in [1, 1000, 4999] {
            assert!(
                provider.get(now, true).is_some(),
                "burst serves cached at {now}"
            );
        }
        assert_eq!(sysfs_calls.get(), 1, "fallback burst never re-reads");
        assert!(provider.get(5000, true).is_some());
        assert_eq!(sysfs_calls.get(), 2, "next window re-reads once");
    }

    #[test]
    fn upower_absence_costs_one_check_per_window() {
        let upower = FakeUpower::absent();
        let upower_calls = Rc::clone(&upower.calls);
        let sysfs = FakeSysfs::batteries(vec![("BAT0", "63", "Discharging")]);
        let sysfs_calls = Rc::clone(&sysfs.calls);
        let mut provider = BatteryProvider::with_policy(upower, sysfs, 5000);
        assert!(provider.get(0, true).is_some());
        for now in [10, 100, 1000, 4999] {
            assert!(provider.get(now, true).is_some());
        }
        // One round per window: one UPower absence check plus one sysfs
        // read, never a retry loop.
        assert_eq!(upower_calls.get(), 1);
        assert_eq!(sysfs_calls.get(), 1);
    }

    #[test]
    fn garbled_capacity_disqualifies_its_entry_only() {
        let sysfs = FakeSysfs {
            raws: vec![
                raw("BAT0", Some("garbage"), Some("Charging")),
                raw("BAT1", Some("101"), Some("Charging")),
                raw("BAT2", Some("77"), Some("Full")),
            ],
            calls: Rc::new(Cell::new(0)),
        };
        let mut provider = BatteryProvider::with_policy(FakeUpower::absent(), sysfs, 5000);
        let snapshot = provider.get(0, true).expect("second valid battery serves");
        assert!((snapshot.capacity_pct - 77.0).abs() < 1e-9);
        assert_eq!(snapshot.state, BatteryState::Full);
    }

    #[test]
    fn source_loss_hides_past_horizon() {
        struct Flaky {
            raws: Cell<Option<Vec<SysfsBatteryRaw>>>,
        }
        impl SysfsBatterySource for Flaky {
            fn sample(&self) -> Vec<SysfsBatteryRaw> {
                self.raws.take().unwrap_or_default()
            }
        }
        let flaky = Flaky {
            raws: Cell::new(Some(vec![raw("BAT0", Some("50"), Some("Discharging"))])),
        };
        let mut provider = BatteryProvider::with_policy(FakeUpower::absent(), flaky, 5000);
        assert!(provider.get(0, true).is_some());
        // Source now fails every window; last good serves while fresh.
        let stale = provider.get(5000, true).expect("stale serves");
        assert!((stale.capacity_pct - 50.0).abs() < 1e-9, "held, not zeroed");
        assert_eq!(stale.stale_age_ms, 5000);
        assert!(provider.get(15000, true).is_some(), "horizon still serves");
        assert!(provider.get(15001, true).is_none(), "past horizon hides");
    }

    #[test]
    fn ungranted_is_indistinguishable_from_absent() {
        let upower = FakeUpower::serving(80.0, 1);
        let sysfs = FakeSysfs::batteries(vec![("BAT0", "10", "Discharging")]);
        let mut provider = BatteryProvider::with_policy(upower, sysfs, 5000);
        assert!(provider.get(0, false).is_none());
        let mut missing =
            BatteryProvider::with_policy(FakeUpower::absent(), FakeSysfs::empty(), 5000);
        assert!(missing.get(0, true).is_none());
    }

    #[test]
    fn icon_buckets_stay_lua_side() {
        // The provider returns numbers/states only: no glyph, icon, or
        // bucket vocabulary may cross (pinned so a future icon field fails
        // here, not in review).
        let mut provider =
            BatteryProvider::with_policy(FakeUpower::serving(80.0, 1), FakeSysfs::empty(), 5000);
        let snapshot = provider.get(0, true).expect("serves");
        let value = snapshot.to_lua_value();
        let rendered = format!("{value:?}").to_lowercase();
        for forbidden in ["icon", "glyph", "bucket", "emoji", "nerd"] {
            assert!(
                !rendered.contains(forbidden),
                "battery shape must not carry '{forbidden}': {rendered}"
            );
        }
        let LuaValue::Table(pairs) = &value else {
            panic!("snapshot marshals as a table");
        };
        let keys: Vec<&str> = pairs
            .iter()
            .filter_map(|(k, _)| match k {
                LuaValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        for key in &keys {
            assert!(
                [
                    "capacity_pct",
                    "state",
                    "power_w",
                    "seconds_to_empty",
                    "stale_age_ms"
                ]
                .contains(key),
                "unexpected battery key '{key}'"
            );
        }
    }

    #[test]
    fn live_sysfs_reader_lists_fixture_dir() {
        let dir = std::env::temp_dir().join(format!("bitty-battery-{}", std::process::id()));
        let bat = dir.join("BAT0");
        std::fs::create_dir_all(&bat).expect("fixture dir");
        std::fs::write(bat.join("capacity"), "42\n").expect("capacity");
        std::fs::write(bat.join("status"), "Charging\n").expect("status");
        std::fs::write(dir.join("AC").join("type"), "Mains").ok();
        std::fs::create_dir_all(dir.join("AC")).ok();
        std::fs::write(dir.join("AC").join("type"), "Mains\n").ok();
        let reader = FsPowerSupply::with_root(dir.clone());
        let raws = reader.sample();
        let bat_raw = raws.iter().find(|r| r.name == "BAT0").expect("BAT0 listed");
        assert_eq!(bat_raw.capacity_text.as_deref().map(str::trim), Some("42"));
        let mut provider =
            BatteryProvider::with_policy(NoUpower, FsPowerSupply::with_root(dir.clone()), 5000);
        let snapshot = provider.get(0, true).expect("fixture serves");
        assert!((snapshot.capacity_pct - 42.0).abs() < 1e-9);
        assert_eq!(snapshot.state, BatteryState::Charging);
        std::fs::remove_dir_all(&dir).ok();
    }
}
