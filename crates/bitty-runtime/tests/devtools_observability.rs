//! DT-02 observability event pipeline verification (CTX-0591, issue #1098).
//!
//! Verifies the observability lane on today's reachable surface: bounded
//! queues with counted `DropOldest` attribution and no unbounded growth.
//! Source: `devtools-rfc.md` § "Observability event pipeline"
//! (delivery/ordering/coalescing points) and § "Instrumentation" (queue
//! accounting emits per-queue drop counts). Cited control: P0-AC-014
//! (resource budgets attributable) and P0-AC-015 (plugins out of hot paths).
//!
//! Covered here (the fixed gap: the bounded input-observability ring dropped
//! its oldest event silently, with no counted attribution):
//! - `inspect_input_dropped` counts every eviction at the ring cap.
//! - Retained history converges to the newest events (DropOldest).
//! - Within-bound publishes report zero fabricated loss.
//! - A sustained producer storm cannot grow the ring past its cap.
//!
//! Headless.

use bitty_platform::{KeyEvent, LogicalKey, PressState};
use bitty_runtime::{Runtime, inspect::INSPECT_MAX_RING};

fn rt() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn char_key(logical: &str) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(logical.to_string()),
        text: Some(logical.to_string()),
        location: bitty_platform::KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

#[test]
fn obs_input_ring_drop_oldest_is_counted_not_silent() {
    // The runtime's input-observability ring is a bounded cold-path queue:
    // beyond `INSPECT_MAX_RING` it must evict oldest and *count* the
    // evictions, matching the counted-attribution rule every sibling
    // observability queue follows. Before CTX-0591 the eviction was silent.
    let mut rt = rt();
    for i in 0..(INSPECT_MAX_RING + 10) {
        rt.handle_key_event(char_key(&format!("k{i}")));
    }
    assert_eq!(rt.inspect_input_len(), INSPECT_MAX_RING, "ring holds cap");
    assert_eq!(
        rt.inspect_input_dropped(),
        10,
        "the 10 evictions must be counted, never silent (P0-AC-014)"
    );
    // Retained history converges to the newest events (DropOldest): the
    // oldest 10 are gone.
    let snapshot = rt.inspect_input_snapshot(INSPECT_MAX_RING);
    assert_eq!(snapshot.len(), INSPECT_MAX_RING);
    assert_eq!(snapshot[0].seq, 11, "oldest 10 dropped: {snapshot:?}");
}

#[test]
fn obs_input_ring_within_bound_counts_no_drops() {
    let mut rt = rt();
    for i in 0..3 {
        rt.handle_key_event(char_key(&format!("k{i}")));
    }
    assert_eq!(rt.inspect_input_len(), 3);
    assert_eq!(
        rt.inspect_input_dropped(),
        0,
        "no over-bound publish may fabricate loss"
    );
}

#[test]
fn obs_input_ring_never_grows_unbounded_under_storm() {
    let mut rt = rt();
    let storm = 10_000usize;
    for i in 0..storm {
        rt.handle_key_event(char_key(&format!("k{i}")));
    }
    // Bounded growth: the store stays at its cap and every eviction is
    // accounted (no unbounded growth, no silent loss).
    assert_eq!(
        rt.inspect_input_len(),
        INSPECT_MAX_RING,
        "the ring bound must hold"
    );
    assert_eq!(
        rt.inspect_input_dropped(),
        (storm - INSPECT_MAX_RING) as u64,
        "every eviction must be counted"
    );
}
