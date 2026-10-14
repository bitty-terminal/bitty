//! Live native sensor host for bar modules (CTX-1199).
//!
//! Publishes the batch-A descriptors from [`super`] into the runtime
//! [`ServiceDirectory`](super::super::services::ServiceDirectory) and serves
//! `get()` through host dispatch (no VM, no Lua provider). One runtime owns
//! one host, so the provider-side ceilings stay global: bursty bar ticks
//! from every consumer share one source attempt per window.
//!
//! Sources are live by default: `/proc/stat` and `/proc/meminfo` plus the
//! `/sys/class/power_supply` fallback behind [`NoUpower`]. Live UPower D-Bus
//! transport stays a scoped follow-up (one bus name, no broad proxy); this
//! revision proves preference/selection/ceiling hermetically and serves the
//! fallback live, so UPower-less systems never storm.
//!
//! Time is the host monotonic clock (elapsed since creation) unless a test
//! pins a manual instant. Every `get()` preserves the provider ceilings and
//! `stale_age_ms` end-to-end; hide surfaces as Lua `nil` (the bar hides),
//! never as fabricated zeros. Thresholds and icon buckets stay Lua-side:
//! only raw numbers/states cross here.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use bitty_lua::{BridgeError, LuaValue};

use super::battery::{BatteryProvider, FsPowerSupply, NoUpower};
use super::cpu::CpuProvider;
use super::memory::MemoryProvider;
use super::{BATTERY_BACKING_HEAD, CPU_BACKING_HEAD, MEMORY_BACKING_HEAD, SENSOR_PROVIDER};

/// Generation pinned into native sensor records.
///
/// Runtime-owned (never a plugin activation generation): the directory route
/// carries it, so a republished replacement could never hijack a live
/// handle. Native records never suspend or revoke (their provider is never
/// a plugin id), so the generation never advances.
pub const SENSOR_GENERATION: u32 = 0;

/// Native sensor host: three live providers plus the monotonic clock.
///
/// Shared per runtime (`Rc<RefCell<...>>`): every generation dispatches
/// through the same providers, so ceilings are global, not per-consumer.
pub struct SensorHost {
    cpu: CpuProvider,
    memory: MemoryProvider,
    battery: BatteryProvider<NoUpower, FsPowerSupply>,
    start: Instant,
    manual_now_ms: Option<u64>,
}

impl SensorHost {
    /// Live host: real `/proc` sources plus the live sysfs fallback.
    #[must_use]
    pub fn live() -> Self {
        Self {
            cpu: CpuProvider::procfs(),
            memory: MemoryProvider::procfs(),
            battery: BatteryProvider::new(NoUpower, FsPowerSupply::system()),
            start: Instant::now(),
            manual_now_ms: None,
        }
    }

    /// Test host over injected `/proc` fixtures plus a sysfs root.
    ///
    /// `cpu_source`/`mem_source` answer `/proc/stat` and `/proc/meminfo`
    /// text (or `None` for a lost source); `sysfs_root` is the
    /// `/sys/class/power_supply` replacement. `initial_now_ms` pins the
    /// clock; advance it with [`Self::set_manual_now_ms`].
    #[must_use]
    pub fn for_test(
        cpu_source: Rc<dyn Fn() -> Option<String>>,
        mem_source: Rc<dyn Fn() -> Option<String>>,
        sysfs_root: PathBuf,
        initial_now_ms: u64,
    ) -> Self {
        Self {
            cpu: CpuProvider::with_source(cpu_source),
            memory: MemoryProvider::with_source(mem_source),
            battery: BatteryProvider::new(NoUpower, FsPowerSupply::with_root(sysfs_root)),
            start: Instant::now(),
            manual_now_ms: Some(initial_now_ms),
        }
    }

    /// Current host instant in milliseconds.
    ///
    /// Manual when pinned (tests), otherwise elapsed since creation. Wrap
    /// is fail-closed downstream (providers use wrapping age math).
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

    /// Whether `iface` is one of the three batch-A sensor interfaces.
    #[must_use]
    pub fn is_sensor_iface(iface: &str) -> bool {
        matches!(
            iface,
            super::SYSTEM_BATTERY_IFACE | super::SYSTEM_CPU_IFACE | super::SYSTEM_MEMORY_IFACE
        )
    }

    /// Whether `provider` names the native sensor component.
    #[must_use]
    pub fn is_sensor_provider(provider: &str) -> bool {
        provider == SENSOR_PROVIDER
    }

    /// Backing head for one sensor interface (or `None` outside batch A).
    #[must_use]
    pub fn head_for_iface(iface: &str) -> Option<&'static str> {
        match iface {
            super::SYSTEM_CPU_IFACE => Some(CPU_BACKING_HEAD),
            super::SYSTEM_MEMORY_IFACE => Some(MEMORY_BACKING_HEAD),
            super::SYSTEM_BATTERY_IFACE => Some(BATTERY_BACKING_HEAD),
            _ => None,
        }
    }

    /// Serve one native `get()` at the host instant.
    ///
    /// `granted` must be the caller generation gate (already checked by the
    /// broker; `false` hides without touching a source). Hide (stale past
    /// horizon, first cpu poll, lost source) surfaces as `LuaValue::Nil`
    /// so the bar hides; live snapshots marshal unchanged with their
    /// `stale_age_ms`. Unknown interfaces or methods fail closed with
    /// `E_SERVICE_GONE` (indistinguishable from a parked provider, never
    /// a new code).
    pub fn call(
        &mut self,
        iface: &str,
        method: &str,
        granted: bool,
    ) -> Result<LuaValue, BridgeError> {
        if method != "get" || !Self::is_sensor_iface(iface) {
            return Err(BridgeError::new(
                "runtime",
                "E_SERVICE_GONE",
                format!("service '{iface}.{method}' is not published"),
            ));
        }
        let now = self.now_ms();
        if !granted {
            return Ok(LuaValue::Nil);
        }
        match iface {
            super::SYSTEM_CPU_IFACE => Ok(self
                .cpu
                .get(now, true)
                .map(|snapshot| snapshot.to_lua_value())
                .unwrap_or(LuaValue::Nil)),
            super::SYSTEM_MEMORY_IFACE => Ok(self
                .memory
                .get(now, true)
                .map(|snapshot| snapshot.to_lua_value())
                .unwrap_or(LuaValue::Nil)),
            super::SYSTEM_BATTERY_IFACE => Ok(self
                .battery
                .get(now, true)
                .map(|snapshot| snapshot.to_lua_value())
                .unwrap_or(LuaValue::Nil)),
            _ => Err(BridgeError::new(
                "runtime",
                "E_SERVICE_GONE",
                format!("service '{iface}' is unavailable"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const CPU_A: &str = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0  50 0 50 400 0 0 0 0 0 0\n";
    const CPU_B: &str = "cpu  150 0 150 900 0 0 0 0 0 0\ncpu0  100 0 100 400 0 0 0 0 0 0\n";
    const MEMINFO: &str = "MemTotal:        2097152 kB\nMemAvailable:    1048576 kB\nSwapTotal:       0 kB\nSwapFree:        0 kB\n";

    fn seq_source(
        texts: Vec<Option<String>>,
        calls: Rc<Cell<usize>>,
    ) -> Rc<dyn Fn() -> Option<String>> {
        let remaining = Rc::new(RefCell::new(texts.into_iter()));
        Rc::new(move || {
            calls.set(calls.get() + 1);
            remaining.borrow_mut().next().unwrap_or(None)
        })
    }

    fn temp_sysfs_empty(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bitty-sensor-host-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp sysfs");
        dir
    }

    #[test]
    fn memory_serves_live_numbers_with_age_then_nil_past_horizon() {
        let cpu_calls = Rc::new(Cell::new(0));
        let mem_calls = Rc::new(Cell::new(0));
        let cpu = seq_source(vec![Some(CPU_A.to_string())], Rc::clone(&cpu_calls));
        let mem = seq_source(
            vec![Some(MEMINFO.to_string()), None, None, None],
            Rc::clone(&mem_calls),
        );
        let root = temp_sysfs_empty("mem-hide");
        let mut host = SensorHost::for_test(cpu, mem, root.clone(), 0);
        let value = host
            .call(super::super::SYSTEM_MEMORY_IFACE, "get", true)
            .expect("memory serves");
        match &value {
            LuaValue::Table(pairs) => {
                assert!(
                    pairs
                        .iter()
                        .any(|(k, _)| k == &LuaValue::String("pct".to_string()))
                );
                assert!(
                    pairs
                        .iter()
                        .any(|(k, _)| k == &LuaValue::String("stale_age_ms".to_string()))
                );
            }
            other => panic!("memory serves a table, got {other:?}"),
        }
        host.set_manual_now_ms(Some(2000));
        let stale = host
            .call(super::super::SYSTEM_MEMORY_IFACE, "get", true)
            .expect("stale serves");
        match &stale {
            LuaValue::Table(_) => {}
            other => panic!("stale serves a table, got {other:?}"),
        }
        host.set_manual_now_ms(Some(6001));
        // Horizon is three 2 s windows: t=6000 serves, t=6001 hides as nil.
        host.set_manual_now_ms(Some(6000));
        assert!(matches!(
            host.call(super::super::SYSTEM_MEMORY_IFACE, "get", true)
                .expect("horizon"),
            LuaValue::Table(_)
        ));
        host.set_manual_now_ms(Some(6001));
        assert_eq!(
            host.call(super::super::SYSTEM_MEMORY_IFACE, "get", true)
                .expect("hide"),
            LuaValue::Nil,
            "past horizon hides as nil, never zeros"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cpu_first_poll_hides_second_serves_with_coalescing() {
        let cpu_calls = Rc::new(Cell::new(0));
        let mem_calls = Rc::new(Cell::new(0));
        let cpu = seq_source(
            vec![Some(CPU_A.to_string()), Some(CPU_B.to_string())],
            Rc::clone(&cpu_calls),
        );
        let mem = seq_source(vec![Some(MEMINFO.to_string())], Rc::clone(&mem_calls));
        let root = temp_sysfs_empty("cpu-first");
        let mut host = SensorHost::for_test(cpu, mem, root.clone(), 0);
        assert_eq!(
            host.call(super::super::SYSTEM_CPU_IFACE, "get", true)
                .expect("first"),
            LuaValue::Nil,
            "baseline establishes, hides"
        );
        assert_eq!(cpu_calls.get(), 1);
        host.set_manual_now_ms(Some(500));
        assert_eq!(
            host.call(super::super::SYSTEM_CPU_IFACE, "get", true)
                .expect("burst"),
            LuaValue::Nil,
            "still baselining inside the window"
        );
        assert_eq!(cpu_calls.get(), 1, "burst coalesces");
        host.set_manual_now_ms(Some(2000));
        match host
            .call(super::super::SYSTEM_CPU_IFACE, "get", true)
            .expect("delta")
        {
            LuaValue::Table(_) => {}
            other => panic!("delta serves a table, got {other:?}"),
        }
        assert_eq!(cpu_calls.get(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ungranted_hides_without_touching_sources() {
        let cpu_calls = Rc::new(Cell::new(0));
        let mem_calls = Rc::new(Cell::new(0));
        let cpu = seq_source(vec![Some(CPU_A.to_string())], Rc::clone(&cpu_calls));
        let mem = seq_source(vec![Some(MEMINFO.to_string())], Rc::clone(&mem_calls));
        let root = temp_sysfs_empty("ungranted");
        let mut host = SensorHost::for_test(cpu, mem, root.clone(), 0);
        for iface in [
            super::super::SYSTEM_CPU_IFACE,
            super::super::SYSTEM_MEMORY_IFACE,
            super::super::SYSTEM_BATTERY_IFACE,
        ] {
            assert_eq!(
                host.call(iface, "get", false).expect("ungranted hides"),
                LuaValue::Nil
            );
        }
        assert_eq!(cpu_calls.get(), 0);
        assert_eq!(mem_calls.get(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_method_or_iface_fails_closed_as_gone() {
        let cpu = seq_source(vec![], Rc::new(Cell::new(0)));
        let mem = seq_source(vec![], Rc::new(Cell::new(0)));
        let root = temp_sysfs_empty("unknown");
        let mut host = SensorHost::for_test(cpu, mem, root.clone(), 0);
        for (iface, method) in [
            (super::super::SYSTEM_CPU_IFACE, "set"),
            ("system.unknown", "get"),
        ] {
            let error = host
                .call(iface, method, true)
                .expect_err("must fail closed");
            assert_eq!(error.code, "E_SERVICE_GONE");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
