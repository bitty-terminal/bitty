//! Non-blocking background sampler cache for host providers.
//!
//! Live `/proc` and `/sys` reads can block in kernel/driver hooks longer than
//! the 50 ms RC-1 Lua wall-clock slice. This helper moves those synchronous
//! filesystem reads onto a persistent 256 KiB worker thread so the executor
//! thread only swaps slots or serves cached readings within the hide horizon.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::Builder;
use std::time::Instant;

use bitty_lua::LuaValue;

const SAMPLE_THREAD_STACK_BYTES: usize = 256 * 1024;
const SAMPLE_QUEUE_CAPACITY: usize = 8;

/// Completed sample payload produced by a background worker read.
#[derive(Debug, Clone)]
pub(in crate::plugin_runtime) struct SampledValue {
    pub value: LuaValue,
    pub base_stale_age_ms: u64,
}

impl SampledValue {
    /// Extracts `stale_age_ms` from a provider snapshot table (defaulting to
    /// `0` when absent or non-integer) so the cache can age it across hits.
    pub(in crate::plugin_runtime) fn from_lua(value: LuaValue) -> Self {
        let base_stale_age_ms = match value.get("stale_age_ms") {
            Some(LuaValue::Integer(n)) if *n >= 0 => *n as u64,
            _ => 0,
        };
        Self {
            value,
            base_stale_age_ms,
        }
    }
}

#[derive(Debug, Clone)]
struct CachedSample {
    value: LuaValue,
    base_stale_age_ms: u64,
    landed_ms: u64,
}

#[derive(Debug, Default)]
struct SampleSlot {
    cached: Option<CachedSample>,
    request_ms: Option<u64>,
    in_flight: bool,
}

type ReaderFn<K> = Box<dyn FnMut(K, u64) -> Option<SampledValue>>;
type ReaderFactory<K> = Box<dyn FnOnce() -> ReaderFn<K> + Send>;
type SampleRequest<K> = (K, u64, Instant);

/// Bounded per-endpoint background sampler cache shared by native providers.
pub(in crate::plugin_runtime) struct SampledCache<K, const N: usize> {
    thread_name: &'static str,
    slots: Arc<Mutex<[SampleSlot; N]>>,
    worker_factory: RefCell<Option<ReaderFactory<K>>>,
    worker_tx: RefCell<Option<SyncSender<SampleRequest<K>>>>,
}

impl<K, const N: usize> SampledCache<K, N>
where
    K: Copy + Into<usize> + Send + 'static,
{
    pub(in crate::plugin_runtime) fn new<F>(thread_name: &'static str, factory: F) -> Self
    where
        F: FnOnce() -> ReaderFn<K> + Send + 'static,
    {
        Self {
            thread_name,
            slots: Arc::new(Mutex::new(std::array::from_fn(|_| SampleSlot::default()))),
            worker_factory: RefCell::new(Some(Box::new(factory))),
            worker_tx: RefCell::new(None),
        }
    }

    /// Reads the cached sample (or `LuaValue::Nil` before the first read lands
    /// or past `hide_horizon_ms`) and triggers a non-blocking background refresh
    /// when `ceiling_ms` has elapsed and no read for `kind` is in flight.
    pub(in crate::plugin_runtime) fn get_or_request(
        &self,
        kind: K,
        now_ms: u64,
        ceiling_ms: u64,
        hide_horizon_ms: u64,
    ) -> LuaValue {
        let idx = kind.into();
        let (should_send, value) = {
            let mut guard = lock_slots(&self.slots);
            let slot = &mut guard[idx];
            let due = !slot.in_flight
                && slot
                    .request_ms
                    .is_none_or(|last| now_ms.saturating_sub(last) >= ceiling_ms);
            if due {
                slot.in_flight = true;
                slot.request_ms = Some(now_ms);
            }
            let value = match &slot.cached {
                Some(cached) => {
                    let effective_stale = cached
                        .base_stale_age_ms
                        .saturating_add(now_ms.saturating_sub(cached.landed_ms));
                    if effective_stale > hide_horizon_ms {
                        slot.cached = None;
                        LuaValue::Nil
                    } else {
                        with_stale_age_ms(&cached.value, effective_stale)
                    }
                }
                None => LuaValue::Nil,
            };
            (due, value)
        };

        if should_send {
            self.dispatch(kind, now_ms);
        }
        value
    }

    fn dispatch(&self, kind: K, now_ms: u64) {
        let idx = kind.into();
        let started = Instant::now();
        let mut tx_slot = self.worker_tx.borrow_mut();
        if let Some(tx) = tx_slot.as_ref() {
            match tx.try_send((kind, now_ms, started)) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => {
                    lock_slots(&self.slots)[idx].in_flight = false;
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    *tx_slot = None;
                }
            }
        }
        let Some(factory) = self.worker_factory.borrow_mut().take() else {
            lock_slots(&self.slots)[idx].in_flight = false;
            return;
        };
        let (tx, rx) = std::sync::mpsc::sync_channel::<SampleRequest<K>>(SAMPLE_QUEUE_CAPACITY);
        let _ = tx.try_send((kind, now_ms, started));
        let slots_arc = Arc::clone(&self.slots);
        let spawned = Builder::new()
            .name(self.thread_name.to_string())
            .stack_size(SAMPLE_THREAD_STACK_BYTES)
            .spawn(move || {
                let mut reader = factory();
                while let Ok((req_kind, req_ms, req_started)) = rx.recv() {
                    let req_idx = req_kind.into();
                    let _guard = SlotFlightGuard {
                        slots: Arc::clone(&slots_arc),
                        idx: req_idx,
                    };
                    if let Ok(sample) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            reader(req_kind, req_ms)
                        }))
                    {
                        let landed_ms = req_ms.saturating_add(
                            u64::try_from(req_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        let mut guard = lock_slots(&slots_arc);
                        guard[req_idx].cached = sample.map(|s| CachedSample {
                            value: s.value,
                            base_stale_age_ms: s.base_stale_age_ms,
                            landed_ms,
                        });
                        guard[req_idx].in_flight = false;
                    }
                }
            });
        match spawned {
            Ok(_) => {
                *tx_slot = Some(tx);
            }
            Err(_) => {
                lock_slots(&self.slots)[idx].in_flight = false;
            }
        }
    }
}

struct SlotFlightGuard<const N: usize> {
    slots: Arc<Mutex<[SampleSlot; N]>>,
    idx: usize,
}

impl<const N: usize> Drop for SlotFlightGuard<N> {
    fn drop(&mut self) {
        lock_slots(&self.slots)[self.idx].in_flight = false;
    }
}

fn lock_slots<const N: usize>(slots: &Mutex<[SampleSlot; N]>) -> MutexGuard<'_, [SampleSlot; N]> {
    slots.lock().unwrap_or_else(PoisonError::into_inner)
}

fn with_stale_age_ms(value: &LuaValue, stale_age_ms: u64) -> LuaValue {
    let age_i64 = i64::try_from(stale_age_ms).unwrap_or(i64::MAX);
    let LuaValue::Table(pairs) = value else {
        return value.clone();
    };
    let mut updated = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        if matches!(k, LuaValue::String(key) if key == "stale_age_ms") {
            updated.push((k.clone(), LuaValue::Integer(age_i64)));
        } else {
            updated.push((k.clone(), v.clone()));
        }
    }
    LuaValue::Table(updated)
}
