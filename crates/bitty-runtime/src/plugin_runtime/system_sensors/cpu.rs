//! CPU-state provider for bar modules (`custom/cpu` parity, bitty#2019).
//!
//! Source: `/proc/stat` sampling (Linux). The aggregate `cpu` line yields
//! `total_pct`; `cpuN` lines yield `per_core` in index order. Utilization is
//! the standard idle-delta ratio between two samples, so the first poll
//! after start (or after a hotplug length change) establishes the baseline
//! and hides (`None`) instead of fabricating a value.
//!
//! Policy (shared plumbing in [`super::common`]): default 2s provider-side
//! ceiling with over-poll coalescing, stale-then-hide past three windows,
//! and fail-closed grants (ungranted reads are indistinguishable from an
//! absent source).

use std::rc::Rc;

use bitty_lua::LuaValue;

use super::common::{FAST_SENSOR_CEILING_MS, RateGate, StalePolicy, horizon_for_ceiling};

/// Contract path for the Linux CPU source (exactly one place).
pub const PROC_STAT_PATH: &str = "/proc/stat";

/// Maximum per-core entries carried in one snapshot.
///
/// The aggregate total never depends on this cap (it comes from the `cpu`
/// line), so truncation on exotic machines keeps `total_pct` exact while
/// `per_core` stays bounded.
pub const MAX_CPU_CORES: usize = 1024;

/// Idle + total jiffies parsed from one `/proc/stat` cpu line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    /// Sum of all reported fields (saturating).
    pub total: u64,
    /// `idle` + `iowait` (saturating).
    pub idle: u64,
}

/// Parse `/proc/stat` text into `(aggregate, per-core)` times.
///
/// Returns `None` when the aggregate `cpu` line is missing or malformed.
/// Malformed per-core lines are skipped (the aggregate stays exact);
/// per-core collection stops at [`MAX_CPU_CORES`].
#[must_use]
pub fn parse_proc_stat(text: &str) -> Option<(CpuTimes, Vec<CpuTimes>)> {
    let mut aggregate: Option<CpuTimes> = None;
    let mut cores = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(label) = fields.next() else {
            continue;
        };
        let is_aggregate = label == "cpu";
        let is_core = label.len() > 3
            && label.starts_with("cpu")
            && label[3..].bytes().all(|b| b.is_ascii_digit());
        if !is_aggregate && !is_core {
            continue;
        }
        let Some(times) = parse_cpu_line(fields) else {
            if is_aggregate {
                return None;
            }
            continue;
        };
        if is_aggregate {
            aggregate = Some(times);
        } else if cores.len() < MAX_CPU_CORES {
            cores.push(times);
        }
    }
    aggregate.map(|total| (total, cores))
}

/// Parse the numeric fields of one cpu line (label already consumed).
///
/// Field order: `user nice system idle iowait irq softirq steal guest
/// guest_nice`. At least four numbers (through `idle`) are required;
/// missing trailing fields read as zero. A non-numeric field fails the line.
fn parse_cpu_line<'a>(fields: impl Iterator<Item = &'a str>) -> Option<CpuTimes> {
    let mut values = [0_u64; 8];
    let mut count = 0_usize;
    for field in fields.take(10) {
        if count >= values.len() {
            break;
        }
        let parsed: u64 = field.parse().ok()?;
        values[count] = parsed;
        count += 1;
    }
    if count < 4 {
        return None;
    }
    let total = values.iter().fold(0_u64, |acc, v| acc.saturating_add(*v));
    let idle = values[3].saturating_add(values[4]);
    Some(CpuTimes { total, idle })
}

/// Utilization percent between two samples (`None` without forward progress).
///
/// No-progress (identical or rewound counters, e.g. a remounted `/proc`)
/// hides instead of reporting a fabricated zero or a negative value.
#[must_use]
pub fn usage_percent(prev: CpuTimes, curr: CpuTimes) -> Option<f64> {
    if curr.total <= prev.total {
        return None;
    }
    let total_delta = curr.total - prev.total;
    let idle_delta = curr.idle.saturating_sub(prev.idle).min(total_delta);
    let busy = total_delta - idle_delta;
    Some((busy as f64 / total_delta as f64 * 100.0).clamp(0.0, 100.0))
}

/// One servable CPU snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct CpuSnapshot {
    /// Aggregate utilization percent in `[0, 100]`.
    pub total_pct: f64,
    /// Per-core utilization in `cpuN` index order.
    pub per_core: Vec<f64>,
    /// Milliseconds since the serving sample (0 when freshly sampled).
    pub stale_age_ms: u64,
}

impl CpuSnapshot {
    /// Marshal into the bar-facing Lua table.
    ///
    /// Exact keys: `total_pct`, `per_core`, `stale_age_ms`. Threshold
    /// policy (warning/critical) lives in Lua; no policy strings cross here.
    #[must_use]
    pub fn to_lua_value(&self) -> LuaValue {
        LuaValue::table([
            ("total_pct", LuaValue::Number(self.total_pct)),
            (
                "per_core",
                LuaValue::array(self.per_core.iter().map(|v| LuaValue::Number(*v)).collect()),
            ),
            ("stale_age_ms", LuaValue::Integer(self.stale_age_ms as i64)),
        ])
    }
}

/// Sampled `/proc/stat` CPU provider with ceiling, stale, and grant policy.
pub struct CpuProvider {
    source: Rc<dyn Fn() -> Option<String>>,
    gate: RateGate,
    policy: StalePolicy,
    prev_total: Option<CpuTimes>,
    prev_cores: Vec<CpuTimes>,
    last_values: Option<(f64, Vec<f64>)>,
    last_success_ms: u64,
}

impl CpuProvider {
    /// Create a provider reading `/proc/stat` with default policy.
    pub fn procfs() -> Self {
        Self::with_source(Rc::new(|| std::fs::read_to_string(PROC_STAT_PATH).ok()))
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
            prev_total: None,
            prev_cores: Vec::new(),
            last_values: None,
            last_success_ms: 0,
        }
    }

    /// Read the current snapshot.
    ///
    /// - `granted == false` returns `None` without touching the source
    ///   (indistinguishable from an absent source).
    /// - Polls inside the ceiling serve the cached sample with a fresh
    ///   `stale_age_ms` (over-poll coalesces; the source sees one attempt
    ///   per window even when it is failing).
    /// - A lost or garbled source serves the last good sample while it is
    ///   fresh, then hides. The first sample (no baseline delta yet) hides.
    pub fn get(&mut self, now_ms: u64, granted: bool) -> Option<CpuSnapshot> {
        if !granted {
            return None;
        }
        if !self.gate.due(now_ms) {
            return self.serve_cached(now_ms);
        }
        self.gate.mark_attempt(now_ms);
        if let Some(text) = (self.source)() {
            if let Some((total, cores)) = parse_proc_stat(&text) {
                self.observe_sample(total, cores, now_ms);
            }
        }
        self.serve_cached(now_ms)
    }

    /// Fold one parsed sample into the baseline and last-good state.
    ///
    /// The first sample (or a core-count change across hotplug) only
    /// re-baselines: pairing unrelated cores would fabricate movement, so
    /// the tick hides by clearing last-good instead.
    fn observe_sample(&mut self, total: CpuTimes, cores: Vec<CpuTimes>, now_ms: u64) {
        let Some(prev_total) = self.prev_total else {
            self.prev_total = Some(total);
            self.prev_cores = cores;
            return;
        };
        if self.prev_cores.len() != cores.len() {
            self.prev_total = Some(total);
            self.prev_cores = cores;
            self.last_values = None;
            return;
        }
        let Some(total_pct) = usage_percent(prev_total, total) else {
            self.prev_total = Some(total);
            self.prev_cores = cores;
            return;
        };
        let mut per_core = Vec::with_capacity(cores.len());
        for (prev, curr) in self.prev_cores.iter().zip(cores.iter()) {
            if curr.total < prev.total {
                // Rewound core counters (offline/online reset): the pairing
                // is structural, so re-baseline the whole sample.
                self.prev_total = Some(total);
                self.prev_cores = cores;
                self.last_values = None;
                return;
            }
            if curr.total == prev.total {
                // Frozen core while the aggregate advanced: the core did no
                // work this window, so 0% is measured, not fabricated. (The
                // aggregate-total check above already hid the remount case
                // where nothing advanced at all.)
                per_core.push(0.0);
                continue;
            }
            let Some(pct) = usage_percent(*prev, *curr) else {
                self.prev_total = Some(total);
                self.prev_cores = cores;
                return;
            };
            per_core.push(pct);
        }
        self.prev_total = Some(total);
        self.prev_cores = cores;
        self.last_values = Some((total_pct, per_core));
        self.last_success_ms = now_ms;
    }

    /// Serve the last good sample with a fresh age, or hide.
    fn serve_cached(&self, now_ms: u64) -> Option<CpuSnapshot> {
        let (total_pct, per_core) = self.last_values.clone()?;
        let stale_age_ms = StalePolicy::age_ms(self.last_success_ms, now_ms);
        if !self.policy.visible(stale_age_ms) {
            return None;
        }
        Some(CpuSnapshot {
            total_pct,
            per_core,
            stale_age_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const TWO_CPU_STAT_A: &str = "cpu  100 0 100 800 0 0 0 0 0 0\n\
         cpu0  50 0 50 400 0 0 0 0 0 0\n\
         cpu1  50 0 50 400 0 0 0 0 0 0\n\
         intr 123\nctxt 456\n";
    const TWO_CPU_STAT_B: &str = "cpu  150 0 150 900 0 0 0 0 0 0\n\
         cpu0  100 0 100 400 0 0 0 0 0 0\n\
         cpu1  50 0 50 500 0 0 0 0 0 0\n";

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
    fn parse_cross_checked_against_hand_computed_deltas() {
        let (total_a, cores_a) = parse_proc_stat(TWO_CPU_STAT_A).expect("fixture A parses");
        assert_eq!(total_a.total, 1000);
        assert_eq!(total_a.idle, 800);
        assert_eq!(cores_a.len(), 2);
        let (total_b, _) = parse_proc_stat(TWO_CPU_STAT_B).expect("fixture B parses");
        // Total: busy delta 100 over total delta 200 -> 50%.
        let total_pct = usage_percent(total_a, total_b).expect("progress");
        assert!((total_pct - 50.0).abs() < 1e-9, "got {total_pct}");
        // cpu0: busy delta 100 over total delta 100 -> 100%.
        let core0 =
            usage_percent(cores_a[0], parse_proc_stat(TWO_CPU_STAT_B).unwrap().1[0]).unwrap();
        assert!((core0 - 100.0).abs() < 1e-9, "got {core0}");
        // cpu1: busy delta 0 over total delta 100 -> 0% measured (not fabricated:
        // the delta math genuinely yields zero busy jiffies here).
        let core1 =
            usage_percent(cores_a[1], parse_proc_stat(TWO_CPU_STAT_B).unwrap().1[1]).unwrap();
        assert!((core1 - 0.0).abs() < 1e-9, "got {core1}");
    }

    #[test]
    fn parse_ignores_non_cpu_lines_and_requires_aggregate() {
        assert!(parse_proc_stat("intr 1\nctxt 2\n").is_none());
        assert!(parse_proc_stat("cpu  a b c d\n").is_none());
        assert!(parse_proc_stat("cpu  1 2 3\n").is_none());
        let (total, cores) = parse_proc_stat("cpu  10 20 30 40\n").expect("short line parses");
        assert_eq!(total.total, 100);
        assert_eq!(total.idle, 40);
        assert!(cores.is_empty());
    }

    #[test]
    fn no_progress_hides_instead_of_fabricating() {
        let times = CpuTimes {
            total: 1000,
            idle: 800,
        };
        assert!(usage_percent(times, times).is_none());
        assert!(
            usage_percent(
                times,
                CpuTimes {
                    total: 900,
                    idle: 700
                }
            )
            .is_none()
        );
    }

    #[test]
    fn first_poll_hides_second_serves() {
        let (source, _) = counting_source(vec![
            Some(TWO_CPU_STAT_A.to_string()),
            Some(TWO_CPU_STAT_B.to_string()),
        ]);
        let mut provider = CpuProvider::with_policy(source, 2000);
        assert!(
            provider.get(0, true).is_none(),
            "baseline establishes, hides"
        );
        let snapshot = provider.get(2000, true).expect("delta serves");
        assert!((snapshot.total_pct - 50.0).abs() < 1e-9);
        assert_eq!(snapshot.per_core.len(), 2);
        assert_eq!(snapshot.stale_age_ms, 0);
    }

    #[test]
    fn burst_coalesces_to_one_source_attempt() {
        let (source, calls) = counting_source(vec![
            Some(TWO_CPU_STAT_A.to_string()),
            Some(TWO_CPU_STAT_B.to_string()),
            Some(TWO_CPU_STAT_B.to_string()),
        ]);
        let mut provider = CpuProvider::with_policy(source, 2000);
        assert!(provider.get(0, true).is_none());
        assert_eq!(calls.get(), 1);
        for now in [1, 500, 1999] {
            assert!(
                provider.get(now, true).is_none(),
                "still baselining at {now}"
            );
        }
        assert_eq!(calls.get(), 1, "burst coalesces");
        let snapshot = provider.get(2000, true).expect("window serves");
        assert_eq!(snapshot.stale_age_ms, 0);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn source_loss_serves_stale_then_hides_never_zero() {
        let (source, _) = counting_source(vec![
            Some(TWO_CPU_STAT_A.to_string()),
            Some(TWO_CPU_STAT_B.to_string()),
            None,
            None,
        ]);
        let mut provider = CpuProvider::with_policy(source, 2000);
        assert!(provider.get(0, true).is_none());
        let fresh = provider.get(2000, true).expect("serves");
        assert!((fresh.total_pct - 50.0).abs() < 1e-9);
        // Source lost at t=4000: last good serves with age.
        let stale = provider.get(4000, true).expect("stale serves");
        assert!(
            (stale.total_pct - 50.0).abs() < 1e-9,
            "values held, not zeroed"
        );
        assert_eq!(stale.stale_age_ms, 2000);
        // Horizon is three windows (6000ms): t=8000 serves, t=8001 hides.
        assert!(provider.get(8000, true).is_some());
        assert!(provider.get(10_001, true).is_none(), "past horizon hides");
    }

    #[test]
    fn ungranted_is_indistinguishable_from_absent() {
        let (source, calls) = counting_source(vec![Some(TWO_CPU_STAT_A.to_string())]);
        let mut provider = CpuProvider::with_policy(source, 2000);
        assert!(provider.get(0, false).is_none());
        assert_eq!(calls.get(), 0, "ungranted never touches the source");
        // Absent source on a granted call behaves identically.
        let (absent, _) = counting_source(vec![None]);
        let mut absent_provider = CpuProvider::with_policy(absent, 2000);
        assert!(absent_provider.get(0, true).is_none());
    }

    #[test]
    fn hotplug_rebaselines_instead_of_mispairing() {
        let three = "cpu  150 0 150 900 0 0 0 0 0 0\n\
             cpu0  100 0 100 400 0 0 0 0 0 0\n\
             cpu1  50 0 50 500 0 0 0 0 0 0\n\
             cpu2  0 0 0 100 0 0 0 0 0 0\n";
        let three_next = "cpu  200 0 200 1000 0 0 0 0 0 0\n\
             cpu0  150 0 150 400 0 0 0 0 0 0\n\
             cpu1  50 0 50 600 0 0 0 0 0 0\n\
             cpu2  0 0 0 100 0 0 0 0 0 0\n";
        let (source, _) = counting_source(vec![
            Some(TWO_CPU_STAT_A.to_string()),
            Some(TWO_CPU_STAT_B.to_string()),
            Some(three.to_string()),
            Some(three_next.to_string()),
        ]);
        let mut provider = CpuProvider::with_policy(source, 2000);
        assert!(provider.get(0, true).is_none());
        assert!(provider.get(2000, true).is_some());
        assert!(
            provider.get(4000, true).is_none(),
            "core-count change re-baselines and hides one tick"
        );
        let recovered = provider.get(6000, true).expect("new baseline serves");
        assert_eq!(recovered.per_core.len(), 3);
    }

    #[test]
    fn lua_shape_carries_exactly_three_keys() {
        let snapshot = CpuSnapshot {
            total_pct: 42.5,
            per_core: vec![10.0, 90.0],
            stale_age_ms: 7,
        };
        let value = snapshot.to_lua_value();
        let LuaValue::Table(pairs) = &value else {
            panic!("snapshot marshals as a table");
        };
        assert_eq!(pairs.len(), 3);
        assert!(matches!(value.get("total_pct"), Some(LuaValue::Number(_))));
        assert!(matches!(value.get("per_core"), Some(LuaValue::Table(_))));
        assert!(matches!(
            value.get("stale_age_ms"),
            Some(LuaValue::Integer(7))
        ));
    }
}
