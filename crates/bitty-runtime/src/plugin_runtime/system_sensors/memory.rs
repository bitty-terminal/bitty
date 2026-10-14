//! Memory-state provider for bar modules (`memory` parity, bitty#2011).
//!
//! Source: `/proc/meminfo` (Linux). `MemTotal` and `MemAvailable` (falling
//! back to `MemFree` on kernels without the former) yield
//! `pct`/`used_gib`/`total_gib`; `SwapTotal`/`SwapFree` yield the `swap_*`
//! triple. All values are raw numbers: warning/critical thresholds are bar
//! (Lua) policy, never provider policy, so no threshold strings cross here.
//!
//! Policy (shared plumbing in [`super::common`]): default 2s provider-side
//! ceiling with over-poll coalescing, stale-then-hide past three windows,
//! and fail-closed grants (ungranted reads are indistinguishable from an
//! absent source).

use std::rc::Rc;

use bitty_lua::LuaValue;

use super::common::{FAST_SENSOR_CEILING_MS, RateGate, StalePolicy, horizon_for_ceiling};

/// Contract path for the Linux memory source (exactly one place).
pub const PROC_MEMINFO_PATH: &str = "/proc/meminfo";

/// Kibibytes per gibibyte (field unit conversion, `1024 * 1024`).
pub const KIB_PER_GIB: f64 = 1_048_576.0;

/// Parsed `/proc/meminfo` values in kibibytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MeminfoValues {
    /// `MemTotal`.
    pub total_kib: u64,
    /// `MemAvailable`, or `MemFree` where the former is absent.
    pub available_kib: u64,
    /// `SwapTotal` (zero on swapless machines).
    pub swap_total_kib: u64,
    /// `SwapFree` (zero on swapless machines).
    pub swap_free_kib: u64,
}

/// Parse `/proc/meminfo` text.
///
/// Returns `None` when `MemTotal` is missing or zero (no denominator
/// exists). `MemAvailable` falls back to `MemFree`; swap fields default to
/// zero (swapless is exact, not fabricated). A malformed value skips its
/// own line, so unknown or future keys never fail the whole parse.
#[must_use]
pub fn parse_meminfo(text: &str) -> Option<MeminfoValues> {
    let mut total: Option<u64> = None;
    let mut available: Option<u64> = None;
    let mut free: Option<u64> = None;
    let mut swap_total = 0_u64;
    let mut swap_free = 0_u64;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        // Third field is the `kB` unit where present; it is accepted but
        // never required, so unit-less fixtures still parse. A non-numeric
        // value skips its own line (unknown or future keys never fail the
        // whole parse); required keys missing afterwards still hide.
        let Ok(parsed) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "MemTotal:" => total = Some(parsed),
            "MemAvailable:" => available = Some(parsed),
            "MemFree:" => free = Some(parsed),
            "SwapTotal:" => swap_total = parsed,
            "SwapFree:" => swap_free = parsed,
            _ => {}
        }
    }
    let total_kib = total.filter(|v| *v > 0)?;
    Some(MeminfoValues {
        total_kib,
        available_kib: available.or(free).unwrap_or(0).min(total_kib),
        swap_total_kib: swap_total,
        swap_free_kib: swap_free.min(swap_total),
    })
}

/// One servable memory snapshot (raw numbers only, no policy strings).
#[derive(Debug, Clone, PartialEq)]
pub struct MemorySnapshot {
    /// RAM used percent in `[0, 100]`.
    pub pct: f64,
    /// RAM used in gibibytes.
    pub used_gib: f64,
    /// RAM total in gibibytes.
    pub total_gib: f64,
    /// Swap used percent in `[0, 100]` (zero on swapless machines).
    pub swap_pct: f64,
    /// Swap used in gibibytes.
    pub swap_used_gib: f64,
    /// Swap total in gibibytes.
    pub swap_total_gib: f64,
    /// Milliseconds since the serving sample (0 when freshly sampled).
    pub stale_age_ms: u64,
}

impl MemorySnapshot {
    /// Build a snapshot from parsed values (total is nonzero by construction).
    #[must_use]
    pub fn from_values(values: MeminfoValues, stale_age_ms: u64) -> Self {
        let used_kib = values.total_kib.saturating_sub(values.available_kib);
        let swap_used_kib = values.swap_total_kib.saturating_sub(values.swap_free_kib);
        Self {
            pct: (used_kib as f64 / values.total_kib as f64 * 100.0).clamp(0.0, 100.0),
            used_gib: used_kib as f64 / KIB_PER_GIB,
            total_gib: values.total_kib as f64 / KIB_PER_GIB,
            swap_pct: if values.swap_total_kib == 0 {
                0.0
            } else {
                (swap_used_kib as f64 / values.swap_total_kib as f64 * 100.0).clamp(0.0, 100.0)
            },
            swap_used_gib: swap_used_kib as f64 / KIB_PER_GIB,
            swap_total_gib: values.swap_total_kib as f64 / KIB_PER_GIB,
            stale_age_ms,
        }
    }

    /// Marshal into the bar-facing Lua table.
    ///
    /// Exact keys: `pct`, `used_gib`, `total_gib`, `swap_pct`,
    /// `swap_used_gib`, `swap_total_gib`, `stale_age_ms`. Threshold
    /// evaluation stays in Lua: no `warning`/`critical` strings exist
    /// anywhere on this path (pinned by test).
    #[must_use]
    pub fn to_lua_value(&self) -> LuaValue {
        LuaValue::table([
            ("pct", LuaValue::Number(self.pct)),
            ("used_gib", LuaValue::Number(self.used_gib)),
            ("total_gib", LuaValue::Number(self.total_gib)),
            ("swap_pct", LuaValue::Number(self.swap_pct)),
            ("swap_used_gib", LuaValue::Number(self.swap_used_gib)),
            ("swap_total_gib", LuaValue::Number(self.swap_total_gib)),
            ("stale_age_ms", LuaValue::Integer(self.stale_age_ms as i64)),
        ])
    }
}

/// Sampled `/proc/meminfo` memory provider with ceiling, stale, and grant policy.
pub struct MemoryProvider {
    source: Rc<dyn Fn() -> Option<String>>,
    gate: RateGate,
    policy: StalePolicy,
    last_values: Option<MeminfoValues>,
    last_success_ms: u64,
}

impl MemoryProvider {
    /// Create a provider reading `/proc/meminfo` with default policy.
    pub fn procfs() -> Self {
        Self::with_source(Rc::new(|| std::fs::read_to_string(PROC_MEMINFO_PATH).ok()))
    }

    /// Create a provider over an injected source (fixtures, tests).
    pub fn with_source(source: Rc<dyn Fn() -> Option<String>>) -> Self {
        Self::with_policy(source, FAST_SENSOR_CEILING_MS)
    }

    /// Create a provider with an explicit ceiling (tests); the stale
    /// horizon stays at three windows.
    pub fn with_policy(source: Rc<dyn Fn() -> Option<String>>, ceiling_ms: u64) -> Self {
        Self {
            source,
            gate: RateGate::new(ceiling_ms),
            policy: StalePolicy::new(horizon_for_ceiling(ceiling_ms)),
            last_values: None,
            last_success_ms: 0,
        }
    }

    /// Read the current snapshot.
    ///
    /// Shares the cpu provider contract: ungranted calls return `None`
    /// without touching the source; polls inside the ceiling serve the
    /// cached sample with a fresh `stale_age_ms`; a lost source serves
    /// last-good while fresh, then hides. Single-shot reads serve on the
    /// first poll (no baseline delta is needed, unlike cpu).
    pub fn get(&mut self, now_ms: u64, granted: bool) -> Option<MemorySnapshot> {
        if !granted {
            return None;
        }
        if self.gate.due(now_ms) {
            self.gate.mark_attempt(now_ms);
            if let Some(text) = (self.source)() {
                if let Some(values) = parse_meminfo(&text) {
                    self.last_values = Some(values);
                    self.last_success_ms = now_ms;
                }
            }
        }
        let values = self.last_values?;
        let stale_age_ms = StalePolicy::age_ms(self.last_success_ms, now_ms);
        if !self.policy.visible(stale_age_ms) {
            return None;
        }
        Some(MemorySnapshot::from_values(values, stale_age_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// 2 GiB total / 1 GiB available / 1 GiB swap total / 0.5 GiB swap free.
    const MEMINFO_FULL: &str = "MemTotal:        2097152 kB\n\
         MemFree:          524288 kB\n\
         MemAvailable:    1048576 kB\n\
         Buffers:           65536 kB\n\
         SwapTotal:       1048576 kB\n\
         SwapFree:         524288 kB\n";

    /// Injected fixture source plus its attempt counter.
    type CountingSource = (Rc<dyn Fn() -> Option<String>>, Rc<Cell<usize>>);

    fn counting_source(texts: Vec<Option<String>>) -> CountingSource {
        let calls = Rc::new(Cell::new(0));
        let texts = Rc::new(std::cell::RefCell::new(texts.into_iter()));
        let probed = Rc::clone(&calls);
        let source: Rc<dyn Fn() -> Option<String>> = Rc::new(move || {
            probed.set(probed.get() + 1);
            texts.borrow_mut().next().unwrap_or(None)
        });
        (source, calls)
    }

    #[test]
    fn parse_cross_checked_against_hand_computed_values() {
        let values = parse_meminfo(MEMINFO_FULL).expect("fixture parses");
        assert_eq!(values.total_kib, 2_097_152);
        assert_eq!(values.available_kib, 1_048_576);
        let snapshot = MemorySnapshot::from_values(values, 0);
        assert!((snapshot.pct - 50.0).abs() < 1e-9, "got {}", snapshot.pct);
        assert!(
            (snapshot.used_gib - 1.0).abs() < 1e-9,
            "got {}",
            snapshot.used_gib
        );
        assert!(
            (snapshot.total_gib - 2.0).abs() < 1e-9,
            "got {}",
            snapshot.total_gib
        );
        assert!(
            (snapshot.swap_pct - 50.0).abs() < 1e-9,
            "got {}",
            snapshot.swap_pct
        );
        assert!((snapshot.swap_used_gib - 0.5).abs() < 1e-9);
        assert!((snapshot.swap_total_gib - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_prefers_available_falls_back_to_free() {
        let legacy = "MemTotal:        2097152 kB\nMemFree:        1048576 kB\n";
        let values = parse_meminfo(legacy).expect("legacy parses without MemAvailable");
        assert_eq!(values.available_kib, 1_048_576);
        let snapshot = MemorySnapshot::from_values(values, 0);
        assert!((snapshot.pct - 50.0).abs() < 1e-9);
        // Swapless machines report exact zeros (no swap exists, none used).
        assert_eq!(snapshot.swap_total_gib, 0.0);
        assert_eq!(snapshot.swap_used_gib, 0.0);
        assert_eq!(snapshot.swap_pct, 0.0);
    }

    #[test]
    fn parse_rejects_missing_or_zero_total() {
        assert!(parse_meminfo("MemAvailable: 100 kB\n").is_none());
        assert!(parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n").is_none());
        assert!(parse_meminfo("not meminfo at all\n").is_none());
    }

    #[test]
    fn unknown_garbled_lines_never_fail_the_parse() {
        let text = "MemTotal:        2097152 kB\n\
             MemAvailable:    1048576 kB\n\
             FutureKernelThing: lots\n\
             Hugetlb:       not-a-number kB\n";
        let values = parse_meminfo(text).expect("unknown lines are skipped");
        assert_eq!(values.total_kib, 2_097_152);
        assert_eq!(values.available_kib, 1_048_576);
    }

    #[test]
    fn first_poll_serves_rate_ceiling_coalesces_burst() {
        let (source, calls) = counting_source(vec![
            Some(MEMINFO_FULL.to_string()),
            Some(MEMINFO_FULL.to_string()),
        ]);
        let mut provider = MemoryProvider::with_policy(source, 2000);
        let first = provider
            .get(0, true)
            .expect("single-shot serves immediately");
        assert_eq!(first.stale_age_ms, 0);
        assert_eq!(calls.get(), 1);
        for now in [1, 999, 1999] {
            let cached = provider.get(now, true).expect("burst serves cached");
            assert_eq!(cached.stale_age_ms, now);
        }
        assert_eq!(calls.get(), 1, "burst coalesces onto one attempt");
        assert!(provider.get(2000, true).is_some());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn source_loss_serves_stale_then_hides() {
        let (source, _) = counting_source(vec![Some(MEMINFO_FULL.to_string()), None, None, None]);
        let mut provider = MemoryProvider::with_policy(source, 2000);
        let fresh = provider.get(0, true).expect("serves");
        assert!((fresh.pct - 50.0).abs() < 1e-9);
        let stale = provider.get(2000, true).expect("stale serves");
        assert!((stale.pct - 50.0).abs() < 1e-9, "values held, not zeroed");
        assert_eq!(stale.stale_age_ms, 2000);
        assert!(provider.get(6000, true).is_some(), "horizon still serves");
        assert!(provider.get(6001, true).is_none(), "past horizon hides");
    }

    #[test]
    fn ungranted_is_indistinguishable_from_absent() {
        let (source, calls) = counting_source(vec![Some(MEMINFO_FULL.to_string())]);
        let mut provider = MemoryProvider::with_policy(source, 2000);
        assert!(provider.get(0, false).is_none());
        assert_eq!(calls.get(), 0, "ungranted never touches the source");
        let (absent, _) = counting_source(vec![None]);
        let mut absent_provider = MemoryProvider::with_policy(absent, 2000);
        assert!(absent_provider.get(0, true).is_none());
    }

    #[test]
    fn provider_carries_no_threshold_policy_strings() {
        // Thresholds are evaluated in Lua: the provider must return raw
        // numbers only. This scans the full marshalled shape so a future
        // `warning`/`critical` field fails here, not in review.
        let values = parse_meminfo(MEMINFO_FULL).expect("fixture parses");
        let snapshot = MemorySnapshot::from_values(values, 3);
        let rendered = format!("{:?}", snapshot.to_lua_value());
        for forbidden in ["warning", "critical", "threshold", "alert"] {
            assert!(
                !rendered.to_lowercase().contains(forbidden),
                "provider shape must not carry '{forbidden}': {rendered}"
            );
        }
        let LuaValue::Table(pairs) = snapshot.to_lua_value() else {
            panic!("snapshot marshals as a table");
        };
        assert_eq!(pairs.len(), 7);
    }
}
