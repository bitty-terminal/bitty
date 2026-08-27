#![forbid(unsafe_code)]

//! Measurement harness for isolation/resource RC budgets — CTX-0037.
//!
//! Headless, deterministic, no window/GPU. Proves enforcement of the
//! three-level queue budgets aligned to the isolation/resource RFC RC-5:
//!
//! - `PerSubscription 64` strict at `EventQueue::push` (`PER_SUBSCRIPTION_QUEUE_LIMIT`)
//! - `PerPlugin 1024 events / 256 KiB` aggregate at `EventPipeline::publish`
//!   via `DropOldest` (accepted v1 default, OQ-013 closed) or `DropNewest`
//! - `Global 8192 events / 2 MiB` tracking via `total_queued_*` (candidate
//!   admission-control open item, not hard-gated)
//! - `EVENT_MAX_BYTES 8 KiB` via `BoundedText` (strict)
//! - `drain_batch` strict byte limit (`<= max_bytes`, no one-event exception)
//! - Invariants `invariant_queue_bounds` / `invariant_global_bounds`
//! - Perf counters `BudgetSnapshot` / `PluginHost::budget_snapshot` and
//!   `publish_count` for `bitty plugin doctor`.
//!
//! RC-1 (`10^7` instr / `50 ms`, warning `8 ms`) and RC-2 (`32 MiB` per VM,
//! `512 MiB` aggregate) are **Open** candidates — no Lua VM is wired yet, so
//! no enforcement is claimed. Constants `RC1_*` / `RC2_*` are exposed only for
//! harness parameterization and are documented as `Open` with follow-up
//! `CTX-0038` / `OQ-014`.
//!
//! RFC lifecycle: `Draft -> experimental review evidence -> Accepted -> normative`
//! per `bitty-docs` workflow. Queue budgets are `candidate` (`OQ-014`);
//! `DropOldest` is `accepted` for v1 (`OQ-013` closed). This harness targets
//! `OQ-014` measurement and is the experimental evidence for that review.
//!
//! All tests are deterministic: fixed sequences, fixed payload sizes, fixed
//! plugin counts, no randomness, no time, no IO.

use bitty_plugin_host::{
    BoundedText, BudgetSnapshot, DropPolicy, EVENT_MAX_BYTES, Event, EventKind, EventPayload,
    EventPipeline, GLOBAL_QUEUED_BYTES_LIMIT, GLOBAL_QUEUED_EVENT_LIMIT,
    PER_PLUGIN_QUEUED_BYTES_LIMIT, PER_PLUGIN_QUEUED_EVENT_LIMIT, PER_SUBSCRIPTION_QUEUE_LIMIT,
    PluginHost, RC1_INSTRUCTION_BUDGET, RC1_WALL_CLOCK_BUDGET_MS, RC1_WARNING_MS,
    RC2_MEMORY_PER_PLUGIN_BYTES,
};

fn pid(s: &str) -> bitty_plugin_host::PluginId {
    bitty_plugin_host::PluginId::new(s).unwrap()
}

fn minimal_manifest(id: &str, events: Vec<&str>) -> bitty_plugin_host::PluginManifest {
    use bitty_plugin_host::{CapabilityRequests, Compat, LazyTriggers, PluginIdentity};
    bitty_plugin_host::PluginManifest {
        identity: PluginIdentity {
            id: pid(id),
            name: "Test".to_string(),
            version: "0.1.0".to_string(),
            description: "desc".to_string(),
            license: Some("MIT".to_string()),
        },
        compat: Compat {
            bitty: Some(">=0.5,<1.0".to_string()),
            plugin_api: Some("^1.0".to_string()),
        },
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        capabilities: CapabilityRequests::default(),
        lazy: LazyTriggers {
            commands: Vec::new(),
            events: events.into_iter().map(|s| s.to_string()).collect(),
            claims: Vec::new(),
        },
        raw_bytes_len: 256,
    }
}

// ── per-subscription 64 strict ───────────────────────────────────────────

#[test]
fn per_subscription_64_strict_dropoldest() {
    // Non-coalescable kind so overflow is not hidden by coalescing.
    let mut p = EventPipeline::new(PER_SUBSCRIPTION_QUEUE_LIMIT, DropPolicy::DropOldest);
    p.subscribe(&pid("xuepoo.a"), EventKind::TerminalBell)
        .unwrap();
    for i in 0..(PER_SUBSCRIPTION_QUEUE_LIMIT + 36) {
        p.publish(Event::new(
            EventKind::TerminalBell,
            EventPayload::Empty,
            i as u64,
        ));
    }
    // Strict: never exceeds 64, drops counted, newest survive.
    assert_eq!(
        p.queued_events_for_plugin("xuepoo.a"),
        PER_SUBSCRIPTION_QUEUE_LIMIT
    );
    assert_eq!(p.total_queued_events(), PER_SUBSCRIPTION_QUEUE_LIMIT);
    assert_eq!(p.total_dropped(), 36);
    assert!(p.invariant_queue_bounds());
    // DropOldest: oldest evicted, newest retained.
    let drained = p.drain(&pid("xuepoo.a"), &EventKind::TerminalBell).unwrap();
    assert_eq!(drained.len(), PER_SUBSCRIPTION_QUEUE_LIMIT);
    assert_eq!(drained.first().unwrap().sequence, 36);
    assert_eq!(drained.last().unwrap().sequence, 99);
    let snap = p.budget_snapshot();
    assert!(snap.per_subscription_hold);
    assert_eq!(snap.total_dropped, 36);
    assert_eq!(snap.queue_count, 1);
}

#[test]
fn per_subscription_64_strict_dropnewest() {
    let mut p = EventPipeline::new(PER_SUBSCRIPTION_QUEUE_LIMIT, DropPolicy::DropNewest);
    p.subscribe(&pid("xuepoo.a"), EventKind::TerminalBell)
        .unwrap();
    for i in 0..(PER_SUBSCRIPTION_QUEUE_LIMIT + 20) {
        p.publish(Event::new(
            EventKind::TerminalBell,
            EventPayload::Empty,
            i as u64,
        ));
    }
    assert_eq!(p.total_queued_events(), PER_SUBSCRIPTION_QUEUE_LIMIT);
    assert_eq!(p.total_dropped(), 20);
    assert!(p.invariant_queue_bounds());
    let drained = p.drain(&pid("xuepoo.a"), &EventKind::TerminalBell).unwrap();
    assert_eq!(drained.first().unwrap().sequence, 0);
    assert_eq!(drained.last().unwrap().sequence, 63);
}

#[test]
fn host_per_subscription_delegation_matches_pipeline() {
    let mut host =
        PluginHost::with_capacity(DropPolicy::DropOldest, PER_SUBSCRIPTION_QUEUE_LIMIT, 8);
    let m = minimal_manifest("xuepoo.host-sub", vec!["terminal.bell"]);
    host.declare(m).unwrap();
    host.resolve(&pid("xuepoo.host-sub")).unwrap();
    host.register(&pid("xuepoo.host-sub")).unwrap();
    host.subscribe(&pid("xuepoo.host-sub"), EventKind::TerminalBell)
        .unwrap();
    for i in 0..100 {
        host.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, i));
    }
    assert_eq!(host.total_queued_events(), PER_SUBSCRIPTION_QUEUE_LIMIT);
    assert_eq!(host.queued_events_for_plugin(&pid("xuepoo.host-sub")), 64);
    assert_eq!(host.total_dropped(), 36);
    assert!(host.invariant_queue_bounds());
    let snap = host.budget_snapshot();
    assert_eq!(snap.total_queued_events, 64);
    assert_eq!(snap.total_dropped, 36);
    assert!(snap.per_subscription_hold);
}

// ── per-plugin aggregate 1024 / 256 KiB ─────────────────────────────────

#[test]
fn per_plugin_aggregate_events_dropoldest() {
    let mut p = EventPipeline::new(1024, DropPolicy::DropOldest);
    p.subscribe(&pid("xuepoo.agg"), EventKind::TerminalBell)
        .unwrap();
    p.subscribe(&pid("xuepoo.agg"), EventKind::TerminalOpened)
        .unwrap();
    // Two queues capacity 64 each = 128 if only per-queue, but per-plugin
    // aggregate 1024 allows up to 1024; push 1500 so we exceed aggregate
    // via non-coalescable events. Alternate kinds to spread across queues.
    for i in 0..1500 {
        let kind = if i % 2 == 0 {
            EventKind::TerminalBell
        } else {
            EventKind::TerminalOpened
        };
        p.publish(Event::new(kind, EventPayload::Empty, i));
    }
    // Enforced aggregate: never over 1024 for this plugin.
    assert!(p.queued_events_for_plugin("xuepoo.agg") <= PER_PLUGIN_QUEUED_EVENT_LIMIT);
    assert_eq!(
        p.queued_events_for_plugin("xuepoo.agg"),
        PER_PLUGIN_QUEUED_EVENT_LIMIT
    );
    assert!(p.invariant_queue_bounds());
    assert_eq!(p.total_dropped(), 1500 - 1024);
    // Budget snapshot reflects adherence.
    let snap: BudgetSnapshot = p.budget_snapshot();
    assert!(snap.per_plugin_hold);
    assert!(snap.per_subscription_hold);
    assert!(snap.invariants_hold() || !snap.global_hold || snap.per_plugin_hold);
    assert!(snap.invariants_hold());
    // DropOldest retains newest sequence numbers globally across plugin.
    let mut all: Vec<u64> = Vec::new();
    all.extend(
        p.drain(&pid("xuepoo.agg"), &EventKind::TerminalBell)
            .unwrap()
            .into_iter()
            .map(|e| e.sequence),
    );
    all.extend(
        p.drain(&pid("xuepoo.agg"), &EventKind::TerminalOpened)
            .unwrap()
            .into_iter()
            .map(|e| e.sequence),
    );
    all.sort_unstable();
    // Oldest 476 evicted (1500-1024), retained are 476..1499.
    assert_eq!(all.first().copied().unwrap(), 476);
    assert_eq!(all.last().copied().unwrap(), 1499);
    assert_eq!(all.len(), 1024);
}

#[test]
fn per_plugin_aggregate_events_dropnewest() {
    let mut p = EventPipeline::new(1024, DropPolicy::DropNewest);
    p.subscribe(&pid("xuepoo.agg2"), EventKind::TerminalBell)
        .unwrap();
    p.subscribe(&pid("xuepoo.agg2"), EventKind::TerminalOpened)
        .unwrap();
    for i in 0..2000 {
        p.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, i));
    }
    assert!(p.queued_events_for_plugin("xuepoo.agg2") <= PER_PLUGIN_QUEUED_EVENT_LIMIT);
    assert!(p.invariant_queue_bounds());
    // DropNewest keeps oldest.
    let drained = p
        .drain(&pid("xuepoo.agg2"), &EventKind::TerminalBell)
        .unwrap();
    assert_eq!(drained.first().unwrap().sequence, 0);
}

#[test]
fn per_plugin_aggregate_bytes_enforced() {
    let mut p = EventPipeline::new(1024, DropPolicy::DropOldest);
    p.subscribe(&pid("xuepoo.bytes"), EventKind::TerminalBell)
        .unwrap();
    p.subscribe(&pid("xuepoo.bytes"), EventKind::TerminalOpened)
        .unwrap();
    // Each payload 8 KiB max, use 4 KiB payloads to approach 256 KiB bytes
    // aggregate. Per-plugin bytes limit 256 KiB allows ~64 events of 4 KiB.
    // But per-plugin event limit is 1024, so bytes will bound first if we use
    // large payloads. Use 4 KiB * 100 = 400 KiB > 256 KiB, so we should be capped
    // by bytes, not events.
    let payload = "a".repeat(4 * 1024);
    for i in 0..100 {
        let kind = if i % 2 == 0 {
            EventKind::TerminalBell
        } else {
            EventKind::TerminalOpened
        };
        p.publish(Event::new(
            kind,
            EventPayload::try_text(payload.clone()).unwrap(),
            i,
        ));
    }
    assert!(
        p.queued_bytes_for_plugin("xuepoo.bytes") <= PER_PLUGIN_QUEUED_BYTES_LIMIT,
        "bytes {} > limit {}",
        p.queued_bytes_for_plugin("xuepoo.bytes"),
        PER_PLUGIN_QUEUED_BYTES_LIMIT
    );
    assert!(p.invariant_queue_bounds());
    // With 4 KiB payload, max queued events by bytes is 64 (256 KiB /4 KiB)
    assert_eq!(
        p.queued_events_for_plugin("xuepoo.bytes"),
        PER_PLUGIN_QUEUED_BYTES_LIMIT / (4 * 1024)
    );
    let snap = p.budget_snapshot();
    assert!(snap.per_plugin_hold);
    assert!(snap.per_plugin_bytes["xuepoo.bytes"] <= PER_PLUGIN_QUEUED_BYTES_LIMIT);
}

// ── global invariants under storm ───────────────────────────────────────

#[test]
fn global_invariants_held_under_storm() {
    // 4 plugins * 2 kinds each, each plugin limited to 1024, so global max
    // would be 4096 < 8192, so global invariant should hold after storm.
    let mut p = EventPipeline::new(1024, DropPolicy::DropOldest);
    let plugins = ["xuepoo.g0", "xuepoo.g1", "xuepoo.g2", "xuepoo.g3"];
    for pid_str in plugins {
        p.subscribe(&pid(pid_str), EventKind::TerminalBell).unwrap();
        p.subscribe(&pid(pid_str), EventKind::TerminalOpened)
            .unwrap();
    }
    // Storm: 2000 publishes per plugin across both kinds -> each plugin caps at 1024.
    for seq in 0..2000 {
        for pid_str in plugins {
            let kind = if seq % 2 == 0 {
                EventKind::TerminalBell
            } else {
                EventKind::TerminalOpened
            };
            // Publish to all subscribers of kind via pipeline publish fanout:
            // we publish one event per seq which fans out to each plugin subscribed to that kind.
            // To keep determinism and avoid cross-plugin fanout confusion, use publish_to per plugin.
            p.publish_to(
                &pid(pid_str),
                Event::new(kind.clone(), EventPayload::Empty, seq),
            )
            .unwrap();
        }
    }
    assert!(p.invariant_queue_bounds());
    assert!(p.invariant_global_bounds());
    assert_eq!(p.total_queued_events(), 4 * PER_PLUGIN_QUEUED_EVENT_LIMIT);
    assert!(p.total_queued_events() <= GLOBAL_QUEUED_EVENT_LIMIT);
    assert!(p.total_queued_bytes() <= GLOBAL_QUEUED_BYTES_LIMIT);
    let snap = p.budget_snapshot();
    assert!(snap.global_hold);
    assert!(snap.per_plugin_hold);
    assert!(snap.per_subscription_hold);
    assert_eq!(snap.total_queued_events, 4096);
    // Per-plugin snapshots sum to global.
    let sum_per_plugin: usize = snap.per_plugin_events.values().sum();
    assert_eq!(sum_per_plugin, snap.total_queued_events);
    // Perf counter: publish_count matches attempts (2000*4 =8000 publishes).
    assert_eq!(p.publish_count(), 8000);
    assert_eq!(snap.publish_count, 8000);
}

#[test]
fn global_tracking_isolation_not_hard_gated() {
    // CTX-0040: global is now enforced (fail-closed) at admission — this test
    // proves enforcement, not just tracking. Storm 9 plugins * 1024 = 9216 >8192
    // would have overflowed before; now shedding via DropOldest keeps
    // total <= GLOBAL limit and invariant holds.
    let mut p = EventPipeline::new(1024, DropPolicy::DropOldest);
    let plugins: Vec<String> = (0..9).map(|i| format!("xuepoo.over{i}")).collect();
    for pid_str in &plugins {
        p.subscribe(&pid(pid_str), EventKind::TerminalBell).unwrap();
        p.subscribe(&pid(pid_str), EventKind::TerminalOpened)
            .unwrap();
    }
    for seq in 0..2000 {
        for pid_str in &plugins {
            p.publish_to(
                &pid(pid_str),
                Event::new(EventKind::TerminalBell, EventPayload::Empty, seq),
            )
            .unwrap();
        }
    }
    // Global is hard-gated: never exceeds 8192, invariant holds, shedding counted.
    assert!(p.total_queued_events() <= GLOBAL_QUEUED_EVENT_LIMIT);
    assert_eq!(p.total_queued_events(), GLOBAL_QUEUED_EVENT_LIMIT);
    assert!(p.invariant_global_bounds());
    assert!(p.invariant_queue_bounds());
    let snap = p.budget_snapshot();
    assert!(snap.global_hold);
    assert_eq!(snap.total_queued_events, GLOBAL_QUEUED_EVENT_LIMIT);
    assert_eq!(snap.global_event_limit, GLOBAL_QUEUED_EVENT_LIMIT);
    assert!(snap.total_dropped > 0);
    assert!(p.total_dropped() > 0);
}

#[test]
fn host_global_delegation_and_snapshot() {
    let mut host = PluginHost::with_capacity(DropPolicy::DropOldest, 1024, 8);
    for i in 0..4 {
        let id = format!("xuepoo.hg{i}");
        let m = minimal_manifest(&id, vec!["terminal.bell", "terminal.opened"]);
        host.declare(m).unwrap();
        host.resolve(&pid(&id)).unwrap();
        host.register(&pid(&id)).unwrap();
        host.subscribe(&pid(&id), EventKind::TerminalBell).unwrap();
        host.subscribe(&pid(&id), EventKind::TerminalOpened)
            .unwrap();
    }
    for seq in 0..1500 {
        host.publish(Event::new(
            EventKind::TerminalBell,
            EventPayload::Empty,
            seq,
        ));
    }
    assert!(host.invariant_queue_bounds());
    assert!(host.invariant_global_bounds());
    assert!(host.total_queued_events() <= GLOBAL_QUEUED_EVENT_LIMIT);
    assert!(host.total_queued_bytes() <= GLOBAL_QUEUED_BYTES_LIMIT);
    let snap = host.budget_snapshot();
    assert!(snap.global_hold);
    assert_eq!(host.publish_count(), 1500);
    assert_eq!(snap.publish_count, 1500);
    // Dropped attribution exists.
    assert!(host.total_dropped() > 0);
    assert!(!host.dropped_per_queue().is_empty());
}

// ── BoundedText 8 KiB enforcement ────────────────────────────────────────

#[test]
fn bounded_text_8kib_strict() {
    assert_eq!(EVENT_MAX_BYTES, 8 * 1024);
    let ok = "a".repeat(EVENT_MAX_BYTES);
    assert!(BoundedText::try_new(ok.clone()).is_ok());
    assert!(EventPayload::try_text(ok.clone()).is_ok());
    let over = "a".repeat(EVENT_MAX_BYTES + 1);
    assert!(BoundedText::try_new(over.clone()).is_err());
    assert!(EventPayload::try_text(over.clone()).is_err());
    // Truncation fits exactly at char boundary, including multibyte.
    let truncated = BoundedText::new_truncated("a".repeat(EVENT_MAX_BYTES + 100));
    assert_eq!(truncated.len(), EVENT_MAX_BYTES);
    // Multibyte truncation: 3-byte chars (e.g., "é" is 2 bytes, "€" 3 bytes, "🦀" 4 bytes)
    let crab = "🦀".repeat(3000); // 4*3000=12000 >8192
    let trunc_crab = BoundedText::new_truncated(crab);
    assert!(trunc_crab.len() <= EVENT_MAX_BYTES);
    assert!(trunc_crab.as_str().is_char_boundary(trunc_crab.len()));
    // Queue push counts oversized as drop if bypassed (defense in depth):
    // Directly constructing an oversized BoundedText is not possible via
    // try_new (it errors), so we prove via truncated path that 8 KiB is the max.
    let mut p = EventPipeline::new(8, DropPolicy::DropOldest);
    p.subscribe(&pid("xuepoo.bounded"), EventKind::TerminalBell)
        .unwrap();
    p.publish(Event::new(
        EventKind::TerminalBell,
        EventPayload::text_truncated("x".repeat(EVENT_MAX_BYTES + 500)),
        1,
    ));
    assert_eq!(p.total_queued_events(), 1);
    assert_eq!(p.queued_bytes_for_plugin("xuepoo.bounded"), EVENT_MAX_BYTES);
}

#[test]
fn event_payload_byte_len_bounded_invariant() {
    let payload = EventPayload::try_text("hello").unwrap();
    assert!(payload.is_bounded());
    assert_eq!(payload.byte_len(), 5);
    let empty = EventPayload::Empty;
    assert!(empty.is_bounded());
    assert_eq!(empty.byte_len(), 0);
    let big = EventPayload::text_truncated("z".repeat(EVENT_MAX_BYTES + 10));
    assert!(big.is_bounded());
    assert!(big.byte_len() <= EVENT_MAX_BYTES);
}

// ── drain_batch strict respected ─────────────────────────────────────────

#[test]
fn drain_batch_strict_byte_limit() {
    let mut p = EventPipeline::new(16, DropPolicy::DropOldest);
    p.subscribe(&pid("xuepoo.batch"), EventKind::TerminalBell)
        .unwrap();
    // Two events 100 bytes each.
    p.publish(Event::new(
        EventKind::TerminalBell,
        EventPayload::try_text("a".repeat(100)).unwrap(),
        1,
    ));
    p.publish(Event::new(
        EventKind::TerminalBell,
        EventPayload::try_text("b".repeat(100)).unwrap(),
        2,
    ));
    // Batch with max_bytes 150 should return only 1 (strict).
    let batch = p
        .drain_batch(&pid("xuepoo.batch"), &EventKind::TerminalBell, 32, 150)
        .unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].sequence, 1);
    // Remainder stays queued.
    assert_eq!(p.queued_events_for_plugin("xuepoo.batch"), 1);
    // Strict: front event 100 bytes > max_bytes 50 returns empty, retains queue.
    let empty = p
        .drain_batch(&pid("xuepoo.batch"), &EventKind::TerminalBell, 32, 50)
        .unwrap();
    assert_eq!(empty.len(), 0);
    assert_eq!(p.queued_events_for_plugin("xuepoo.batch"), 1);
    // Succeeds when byte budget sufficient.
    let batch2 = p
        .drain_batch(&pid("xuepoo.batch"), &EventKind::TerminalBell, 32, 100)
        .unwrap();
    assert_eq!(batch2.len(), 1);
    assert_eq!(batch2[0].sequence, 2);
    assert_eq!(p.queued_events_for_plugin("xuepoo.batch"), 0);
}

#[test]
fn drain_batch_event_count_and_byte_combined() {
    let mut p = EventPipeline::new(16, DropPolicy::DropOldest);
    p.subscribe(&pid("xuepoo.batch2"), EventKind::TerminalBell)
        .unwrap();
    for i in 0..5 {
        p.publish(Event::new(
            EventKind::TerminalBell,
            EventPayload::try_text("x".repeat(10)).unwrap(),
            i,
        ));
    }
    // Limit by events: max_events 2 even though bytes allow more.
    let batch = p
        .drain_batch(&pid("xuepoo.batch2"), &EventKind::TerminalBell, 2, 1024)
        .unwrap();
    assert_eq!(batch.len(), 2);
    // Limit by bytes: remaining 3*10=30 bytes, max_bytes 15 allows only 1.
    let batch2 = p
        .drain_batch(&pid("xuepoo.batch2"), &EventKind::TerminalBell, 32, 15)
        .unwrap();
    assert_eq!(batch2.len(), 1);
}

// ── invariants ────────────────────────────────────────────────────────────

#[test]
fn invariants_hold_after_mixed_storm() {
    let mut p = EventPipeline::new(64, DropPolicy::DropOldest);
    p.subscribe(&pid("xuepoo.i0"), EventKind::TerminalBell)
        .unwrap();
    p.subscribe(&pid("xuepoo.i0"), EventKind::TerminalOpened)
        .unwrap();
    p.subscribe(&pid("xuepoo.i1"), EventKind::TerminalBell)
        .unwrap();
    // Mixed coalescable and non-coalescable: title coalesces, bell does not.
    for i in 0..500 {
        p.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, i));
        p.publish(Event::new(
            EventKind::TerminalTitleChanged,
            EventPayload::try_title_changed(format!("title {i}")).unwrap(),
            i + 10000,
        ));
    }
    // For i0, title queue should have coalesced to 1 (if subscribed) — but we didn't subscribe i0 to title,
    // so only bell counts. We subscribed i0 to bell+opened, not title, so title publishes fan out to no one.
    // Add a title subscription to prove coalescing bound.
    let mut p2 = EventPipeline::new(8, DropPolicy::DropOldest);
    p2.subscribe(&pid("xuepoo.coal"), EventKind::TerminalTitleChanged)
        .unwrap();
    for i in 0..20 {
        p2.publish(Event::new(
            EventKind::TerminalTitleChanged,
            EventPayload::try_title_changed(format!("t{i}")).unwrap(),
            i,
        ));
    }
    // Coalescable per-type queue collapses to single latest, not 20.
    assert_eq!(p2.total_queued_events(), 1);
    assert_eq!(p2.total_dropped(), 0);
    assert!(p2.invariant_queue_bounds());
    // Original storm still bounded.
    assert!(p.invariant_queue_bounds());
    assert!(p.invariant_global_bounds());
    let snap = p.budget_snapshot();
    assert!(snap.per_subscription_hold);
    assert!(snap.per_plugin_hold);
}

// ── perf counters ────────────────────────────────────────────────────────

#[test]
fn budget_snapshot_perf_counters_and_utilization() {
    let mut p = EventPipeline::new(1024, DropPolicy::DropOldest);
    p.subscribe(&pid("xuepoo.perf"), EventKind::TerminalBell)
        .unwrap();
    for i in 0..10 {
        p.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, i));
    }
    let snap = p.budget_snapshot();
    assert_eq!(snap.total_queued_events, 10);
    assert_eq!(snap.publish_count, 10);
    assert_eq!(snap.queue_count, 1);
    assert!(
        snap.per_queue_dropped
            .contains_key(&("xuepoo.perf".to_string(), "terminal.bell".to_string()))
    );
    // Utilization.
    assert!(snap.utilization_global_events() < 0.01);
    assert!(snap.utilization_per_plugin_events("xuepoo.perf") < 0.02);
    // After filling to per-plugin limit, utilization approaches 1.0.
    for i in 10..PER_PLUGIN_QUEUED_EVENT_LIMIT as u64 + 10 {
        p.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, i));
    }
    let snap2 = p.budget_snapshot();
    assert_eq!(
        snap2.per_plugin_events["xuepoo.perf"],
        PER_PLUGIN_QUEUED_EVENT_LIMIT
    );
    assert!((snap2.utilization_per_plugin_events("xuepoo.perf") - 1.0).abs() < 1e-9);
}

#[test]
fn rc1_rc2_constants_are_open_and_documented() {
    // Constants exist for harness parameterization but are not enforced — this
    // test documents the intended values and serves as the "Open with
    // follow-up" evidence. No enforcement is asserted.
    assert_eq!(RC1_INSTRUCTION_BUDGET, 10_000_000);
    assert_eq!(RC1_WALL_CLOCK_BUDGET_MS, 50);
    assert_eq!(RC1_WARNING_MS, 8);
    assert_eq!(RC2_MEMORY_PER_PLUGIN_BYTES, 32 * 1024 * 1024);
    // No VM: we intentionally do not test enforcement, only that values are
    // stable for parameterization.
}

// ── global enforced (CTX-0040) ────────────────────────────────────────────

#[test]
fn global_events_enforced_dropoldest_via_publish() {
    // Publish fanout to many plugins exceeding global 8192 must be hard-gated.
    // Use large per-queue capacity so global is the binding limit, not per-sub.
    let mut p = EventPipeline::new(1024, DropPolicy::DropOldest);
    let plugins: Vec<String> = (0..10).map(|i| format!("xuepoo.ge{i}")).collect();
    for pid_str in &plugins {
        p.subscribe(&pid(pid_str), EventKind::TerminalBell).unwrap();
    }
    // 10 plugins * 1024 per-plugin = 10240 > 8192, so global must shed.
    // Each publish fans out to 10 queues (10 events per publish), 2000 publishes = 20000 attempts.
    for seq in 0..2000 {
        p.publish(Event::new(
            EventKind::TerminalBell,
            EventPayload::Empty,
            seq,
        ));
    }
    assert!(p.total_queued_events() <= GLOBAL_QUEUED_EVENT_LIMIT);
    assert_eq!(p.total_queued_events(), GLOBAL_QUEUED_EVENT_LIMIT);
    assert!(p.invariant_global_bounds());
    assert!(p.invariant_queue_bounds());
    let snap = p.budget_snapshot();
    assert!(snap.global_hold);
    assert!(snap.per_plugin_hold);
    assert!(snap.per_subscription_hold);
    // DropOldest retains newest sequences globally.
    let mut all_seqs: Vec<u64> = Vec::new();
    for pid_str in &plugins {
        let mut drained = p.drain(&pid(pid_str), &EventKind::TerminalBell).unwrap();
        all_seqs.extend(drained.drain(..).map(|e| e.sequence));
    }
    all_seqs.sort_unstable();
    // Oldest should be >0 due to shedding, newest should be 1999.
    assert!(!all_seqs.is_empty());
    assert_eq!(all_seqs.last().copied().unwrap(), 1999);
    assert!(all_seqs.first().copied().unwrap() > 0);
    assert!(p.total_dropped() > 0);
}

#[test]
fn global_events_enforced_dropnewest() {
    let mut p = EventPipeline::new(1024, DropPolicy::DropNewest);
    let plugins: Vec<String> = (0..10).map(|i| format!("xuepoo.gn{i}")).collect();
    for pid_str in &plugins {
        p.subscribe(&pid(pid_str), EventKind::TerminalBell).unwrap();
    }
    for seq in 0..2000 {
        p.publish(Event::new(
            EventKind::TerminalBell,
            EventPayload::Empty,
            seq,
        ));
    }
    assert!(p.total_queued_events() <= GLOBAL_QUEUED_EVENT_LIMIT);
    assert!(p.invariant_global_bounds());
    // DropNewest keeps oldest sequences.
    let drained = p
        .drain(&pid("xuepoo.gn0"), &EventKind::TerminalBell)
        .unwrap();
    assert_eq!(drained.first().unwrap().sequence, 0);
}

#[test]
fn global_bytes_enforced() {
    let mut p = EventPipeline::new(1024, DropPolicy::DropOldest);
    // Need enough plugins so global bytes is binding before per-plugin bytes.
    // Per-plugin 256 KiB with 4 KiB payload = 64 events/plug; global 2 MiB = 512 events.
    // 10 plugins *64 = 640 events >512, so global bytes will cap.
    let plugins: Vec<String> = (0..10).map(|i| format!("xuepoo.gb{i}")).collect();
    for pid_str in &plugins {
        p.subscribe(&pid(pid_str), EventKind::TerminalBell).unwrap();
    }
    let payload = "a".repeat(4 * 1024);
    for seq in 0..2000 {
        for pid_str in &plugins {
            p.publish_to(
                &pid(pid_str),
                Event::new(
                    EventKind::TerminalBell,
                    EventPayload::try_text(payload.clone()).unwrap(),
                    seq,
                ),
            )
            .unwrap();
        }
    }
    assert!(p.total_queued_bytes() <= GLOBAL_QUEUED_BYTES_LIMIT);
    assert!(p.total_queued_events() <= GLOBAL_QUEUED_EVENT_LIMIT);
    assert!(p.invariant_global_bounds());
    assert!(p.invariant_queue_bounds());
    let snap = p.budget_snapshot();
    assert!(snap.global_hold);
    // Global bytes utilization should be ~1.0.
    assert!(snap.utilization_global_bytes() <= 1.0 + 1e-9);
    assert!(snap.utilization_global_bytes() >= 0.9);
}

#[test]
fn global_host_enforced_via_publish() {
    let mut host = PluginHost::with_capacity(DropPolicy::DropOldest, 1024, 8);
    for i in 0..10 {
        let id = format!("xuepoo.hge{i}");
        let m = minimal_manifest(&id, vec!["terminal.bell"]);
        host.declare(m).unwrap();
        host.resolve(&pid(&id)).unwrap();
        host.register(&pid(&id)).unwrap();
        host.subscribe(&pid(&id), EventKind::TerminalBell).unwrap();
    }
    for seq in 0..2000 {
        host.publish(Event::new(
            EventKind::TerminalBell,
            EventPayload::Empty,
            seq,
        ));
    }
    assert!(host.total_queued_events() <= GLOBAL_QUEUED_EVENT_LIMIT);
    assert!(host.invariant_global_bounds());
    assert!(host.invariant_queue_bounds());
    let snap = host.budget_snapshot();
    assert!(snap.global_hold);
    assert_eq!(snap.total_queued_events, GLOBAL_QUEUED_EVENT_LIMIT);
}

// ── host vs pipeline parity ────────────────────────────────────────────

#[test]
fn host_snapshot_parity_with_pipeline() {
    let mut host = PluginHost::with_capacity(DropPolicy::DropOldest, 1024, 8);
    let id = "xuepoo.parity";
    let m = minimal_manifest(id, vec!["terminal.bell"]);
    host.declare(m).unwrap();
    host.resolve(&pid(id)).unwrap();
    host.register(&pid(id)).unwrap();
    host.subscribe(&pid(id), EventKind::TerminalBell).unwrap();
    for i in 0..70 {
        host.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, i));
    }
    let host_snap = host.budget_snapshot();
    let pipe_snap = host.pipeline().budget_snapshot();
    assert_eq!(host_snap.total_queued_events, pipe_snap.total_queued_events);
    assert_eq!(host_snap.total_dropped, pipe_snap.total_dropped);
    assert_eq!(host_snap.publish_count, pipe_snap.publish_count);
    assert_eq!(host.total_queued_events(), pipe_snap.total_queued_events);
    assert_eq!(
        host.queued_events_for_plugin(&pid(id)),
        pipe_snap.per_plugin_events[id]
    );
}
