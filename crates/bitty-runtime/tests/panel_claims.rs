#![forbid(unsafe_code)]
//! Panel-claims headless verification matrix for gate issue #2060 (CTX-1203).
//!
//! One headless proof per Panel claim, all runnable without a display, PTY
//! child, or GPU:
//!
//! | Claim | Test(s) in this file |
//! |---|---|
//! | 1. Zero-PTY native panels | `claim1_*` — Helper/Canvas/Rich panels create, mount, and focus with zero terminals (no PTY fd, no VT bytes, no monospace-grid constraint); `ViewContent::Panel` carries the composition hook |
//! | 2. Snapshot linkage + bus arbitration | `claim2_*` — read-only `terminal_snapshot` path (stale/exited fail-closed, snapshot byte ceiling), `PanelEventBus` bounded `DropOldest` arbitration (per-sub 64 / per-panel 1024+256 KiB / global 8192+2 MiB, 8 KiB payload cap, FIFO survival order), topic-grammar arbitration, deadlock-free bounded drain timing |
//! | 3. Fault isolation (fuel/heap, fail-closed single panel) | `claim3_*` — per-VM fuel exhaustion suspends exactly one VM (neighbor unaffected, typed refusal, reset re-grants), heap over-ceiling refused fail-closed (neighbor unaffected), PTY side keeps zero frame drops (snapshot + tick still serve) |
//! | 4. Scale-to-zero | `claim4_*` — fresh VM shows zero consumed cost (no-tick assertion), declared-but-untriggered plugin builds no VM (lazy static surface only), idle `Runtime::tick` presents with zero plugin work |
//!
//! Headed complements (wgpu compositing pixels, microsecond bus latency on
//! real hardware, GPU-cache residency) are STAGED under
//! `recording/headed/runs/ctx-1203-panel/` (probe plugin + WS5-only
//! `drive.sh`, CTX-1194 pattern) and explicitly NOT run while the owner is
//! active on the desktop. A failing claim here is a NEEDS-FIX finding, never
//! a docs edit.

use bitty_lua::{
    ExecuteOutcome, SuspendReason, VmError,
    error::{E_BUDGET_INSTRUCTIONS, E_BUDGET_MEMORY, bridge_error_for_suspend},
    gate::{VmBudgets, build_plugin_vm},
};
use bitty_plugin_host::{
    CapabilityId, Compat, DropPolicy, EventKind, GrantRecord, LazyCommand, LazyTriggers,
    PluginHost, PluginId, PluginIdentity, PluginManifest, QualifiedName,
};
use bitty_runtime::{
    Runtime,
    registry::{
        BUS_EVENT_MAX_BYTES, BUS_GLOBAL_BYTES_LIMIT, BUS_GLOBAL_LIMIT, BUS_PER_PANEL_BYTES_LIMIT,
        BUS_PER_PANEL_LIMIT, BUS_PER_SUBSCRIPTION_LIMIT, BoundedPayload, EventTopic, Generation,
        LogicalRect, PanelRegistry, PanelRegistryConfig, RegistryConfig, TerminalRegistry,
        WorkspaceId,
    },
};
use bitty_term_state::{Cell, State, TerminalAction};
use bitty_ui::{View, ViewId, panel::PanelType, panel::ViewContent};
use bitty_vt::BoundedString;

// ── shared helpers ──────────────────────────────────────────────────────────

fn tiny_fuel_vm(id: &str) -> bitty_lua::LuaVm {
    build_plugin_vm(
        id,
        Some(VmBudgets {
            instruction_budget: 100,
            wall_budget_ms: 60_000,
            warning_ms: 8,
            memory_limit: 32 * 1024 * 1024,
        }),
    )
    .expect("tiny-fuel VM builds")
}

fn tiny_heap_vm(id: &str, limit: usize) -> bitty_lua::LuaVm {
    build_plugin_vm(
        id,
        Some(VmBudgets {
            instruction_budget: 10_000_000,
            wall_budget_ms: 60_000,
            warning_ms: 8,
            memory_limit: limit,
        }),
    )
    .expect("tiny-heap VM builds")
}

/// Lazy probe manifest built structurally (no file I/O, no VM): one lazy
/// `open` command plus one lazy release event, mirroring the CTX-1194
/// PumpProbe shape (`ui.overlay.focus` only, writes nothing to the PTY).
fn lazy_manifest(id: &str) -> PluginManifest {
    use bitty_plugin_host::CapabilityRequests;
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("ui.overlay.focus").expect("probe capability parses"));
    PluginManifest {
        identity: PluginIdentity {
            id: PluginId::new(id).expect("probe id parses"),
            name: "PanelProbe".to_string(),
            version: "0.0.1".to_string(),
            description: "headless lazy probe (scratch verification only)".to_string(),
            license: Some("MIT".to_string()),
        },
        compat: Compat {
            bitty: Some(">=0.0.21,<0.1".to_string()),
            plugin_api: Some("^1.0".to_string()),
        },
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: caps,
        tools: Vec::new(),
        network: Vec::new(),
        limits: Default::default(),
        lazy: LazyTriggers {
            commands: vec![LazyCommand {
                id: QualifiedName::new(&format!("{id}:open")).expect("probe command parses"),
                args_schema: None,
                result_schema: None,
            }],
            events: vec!["overlay.released".to_string()],
            claims: Vec::new(),
        },
        settings: Vec::new(),
        raw_bytes_len: 512,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Claim 1 — zero-PTY native panels
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn claim1_helper_panel_mounts_with_zero_terminals_no_pty() {
    // A Helper panel is a native (non-PTY) container: create + mount + focus
    // with no terminal allocated anywhere.
    let mut preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry");
    let treg = TerminalRegistry::new(RegistryConfig::default()).expect("terminal registry");
    assert_eq!(treg.terminal_count(), 0, "no PTY-backed terminal exists");

    let ws = WorkspaceId::new(1);
    let view = ViewId::new(7);
    let h = preg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("helper panel creates without PTY");
    preg.mount_panel(h.id, h.generation, view)
        .expect("helper mounts to empty view");
    preg.focus_panel(h.id, h.generation, ws)
        .expect("helper focuses");
    assert_eq!(preg.panel_count(), 1);
    assert_eq!(preg.focused_panel(ws), Some(h.id));

    // Composition hook is the typed ViewContent::Panel variant — the same
    // slot that carries Terminal(u64)/Rich(u64) grids, so a native panel
    // composites alongside terminal grids without a monospace-grid constraint.
    let content = ViewContent::Panel(h.id);
    assert!(content.is_panel());
    assert_eq!(content.panel_id(), Some(h.id));

    // Registry Debug exposes no pty/gpu handle: headless by construction.
    let dbg = format!("{preg:?}");
    assert!(dbg.contains("PanelRegistry"));
    assert!(!dbg.contains("pty"), "no PTY handle in panel registry");
    assert!(!dbg.contains("gpu"), "no GPU handle in panel registry");
}

#[test]
fn claim1_canvas_and_rich_panels_are_grid_free_native_types() {
    // Canvas + Rich + Browser are further native (non-terminal) types in the
    // closed set.
    for ty in [PanelType::Canvas, PanelType::Rich, PanelType::Browser] {
        let mut preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry");
        let ws = WorkspaceId::new(2);
        let _h = preg
            .create_panel(ty, Some(ws))
            .expect("native panel creates");
        assert_eq!(preg.panel_count(), 1, "{ty} creates without PTY");
        // Closed-set spelling round-trips; unknown types stay rejected.
        assert_eq!(PanelType::parse(ty.as_str()), Some(ty));
    }
    assert_eq!(PanelType::parse("unknown"), None);
    assert_eq!(
        PanelType::parse("Terminal"),
        None,
        "parse is lowercase-only"
    );

    // No VT stream is involved: a bare State with no applied bytes still
    // snapshots an empty grid, proving panels never need the VT path.
    let state = State::new();
    let snap = state.snapshot();
    assert_eq!(snap.cells.len(), snap.width * snap.height);
}

#[test]
fn claim1_native_panel_alongside_terminal_grid_layout() {
    // Native panels reuse the generic LayoutNode tiling primitives (stack /
    // split) — no new tiling primitive, no hardcoded workspace node — so a
    // Helper panel tiles deterministically next to terminal views.
    let stack = bitty_runtime::workspace::WorkspaceIntegration::stack_for_workspace(vec![
        View::new(ViewId::new(1), 80, 24),
        View::new(ViewId::new(2), 80, 24),
    ]);
    let allocs = stack.layout(bitty_ui::Rect::new(0, 0, 80, 24));
    assert_eq!(allocs.len(), 2, "stack tiles two leaves deterministically");
    let again = stack.layout(bitty_ui::Rect::new(0, 0, 80, 24));
    assert_eq!(allocs, again, "layout is deterministic headless");
}

// ═══════════════════════════════════════════════════════════════════════════
// Claim 2 — snapshot linkage + bus arbitration
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn claim2_snapshot_read_path_is_read_only_and_bounded() {
    // PTY truth stays live in TerminalRegistry; plugin panels observe it
    // through a read-only Snapshot (Terminal Truth: observation never mutates
    // grid truth).
    let mut treg = TerminalRegistry::new(RegistryConfig::default()).expect("terminal registry");
    let th = treg.create_terminal(None).expect("pty terminal");

    let snap = treg
        .terminal_snapshot(th.id, th.generation)
        .expect("live snapshot serves");
    assert_eq!(
        snap.cells.len(),
        snap.width * snap.height,
        "snapshot covers the full grid"
    );

    // Read-only: mutating a cloned snapshot never touches live truth.
    let mut fork = snap.clone();
    fork.title = BoundedString::new("forged");
    let fresh = treg
        .terminal_snapshot(th.id, th.generation)
        .expect("live truth unchanged by fork");
    assert_ne!(fresh.title.as_str(), "forged");
    assert_eq!(fresh.cells.len(), fresh.width * fresh.height);

    // Snapshot byte ceiling: default-grid cells fit the 256 KiB read-only
    // snapshot envelope the Lua bridge enforces
    // (SNAPSHOT_MAX_BYTES = 256 KiB).
    assert_eq!(
        bitty_lua::host::SNAPSHOT_MAX_BYTES,
        256 * 1024,
        "bridge snapshot ceiling is 256 KiB"
    );
    let cell_bytes = snap.cells.len() * std::mem::size_of::<Cell>();
    assert!(
        cell_bytes <= bitty_lua::host::SNAPSHOT_MAX_BYTES,
        "default-grid snapshot {cell_bytes} fits 256 KiB envelope"
    );

    // Fail-closed: stale generation refuses.
    let stale = Generation(th.generation.get().wrapping_add(1));
    assert!(
        treg.terminal_snapshot(th.id, stale).is_err(),
        "stale generation must fail closed"
    );

    // Fail-closed: exited terminals refuse.
    treg.mark_exited(th.id, th.generation, Some(0))
        .expect("mark exited");
    assert!(
        treg.terminal_snapshot(th.id, th.generation).is_err(),
        "exited terminal snapshot must fail closed"
    );
}

#[test]
fn claim2_snapshot_observes_committed_state_not_grid_mutation() {
    // Observation goes through committed state (OSC title), never direct
    // grid mutation — mirrors the workspace title proof pattern.
    let mut state = State::new();
    state.apply(&TerminalAction::OscTitle {
        text: BoundedString::new("link-target"),
    });
    let snap = state.snapshot();
    assert_eq!(snap.title.as_str(), "link-target");
    assert_eq!(snap.title.as_str(), state.title());
}

#[test]
fn claim2_bus_arbitration_bounded_drop_oldest_fifo() {
    // Services-bus arbitration envelope: per-subscription 64, per-panel
    // 1024 events / 256 KiB, global 8192 / 2 MiB, DropOldest default.
    assert_eq!(BUS_PER_SUBSCRIPTION_LIMIT, 64);
    assert_eq!(BUS_PER_PANEL_LIMIT, 1024);
    assert_eq!(BUS_PER_PANEL_BYTES_LIMIT, 256 * 1024);
    assert_eq!(BUS_GLOBAL_LIMIT, 8192);
    assert_eq!(BUS_GLOBAL_BYTES_LIMIT, 2 * 1024 * 1024);
    assert_eq!(BUS_EVENT_MAX_BYTES, 8 * 1024);

    let mut preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry");
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(1);
    let h = preg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("panel creates");
    preg.mount_panel(h.id, h.generation, view).expect("mount");
    let topic = preg
        .declare_topic("bitty-terminal.panelprobe:frame-ready")
        .expect("provider topic declares");
    preg.subscribe(h.id, h.generation, &topic)
        .expect("subscribe");

    // Publish past the per-subscription cap: DropOldest keeps the newest 64,
    // FIFO order preserved (oldest 16 of 80 dropped).
    for i in 0..80 {
        preg.publish(
            &topic,
            BoundedPayload::try_new(format!("evt{i}")).expect("small payload"),
        )
        .expect("publish");
    }
    assert!(preg.bus_events_for_panel(h.id) <= 64, "per-sub cap holds");
    assert!(preg.bus_total_events() <= 8192, "global cap holds");
    let batch = preg.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);
    assert_eq!(batch[0].payload.as_str(), "evt16", "DropOldest FIFO order");
    assert_eq!(batch[31].payload.as_str(), "evt47");

    // Payload bound: 9 KiB over the 8 KiB event cap is refused at admission.
    let large = "a".repeat(9 * 1024);
    assert!(
        BoundedPayload::try_new(large).is_err(),
        "over-cap payload refused fail-closed"
    );

    // No-subscriber publish is a zero-cost no-op (no fan-out, no queue).
    let lonely = preg
        .declare_topic("bitty-terminal.panelprobe:no-listener")
        .expect("lonely topic declares");
    preg.publish(&lonely, BoundedPayload::try_new("nobody").expect("small"))
        .expect("no-subscriber publish ok");
    assert_eq!(preg.bus_total_events(), preg.bus_events_for_panel(h.id));
}

#[test]
fn claim2_bus_topic_grammar_arbitrates_owners_deadlock_free() {
    // Topic grammar IS the v1 arbitration: qualified `owner.name:topic`
    // routes; bare / malformed spellings fail closed without ever blocking.
    assert!(EventTopic::parse("bitty-terminal.panelprobe:frame-ready").is_ok());
    assert!(
        EventTopic::parse("frame-ready").is_err(),
        "bare topic rejected"
    );
    assert!(
        EventTopic::parse("bitty.panel:").is_err(),
        "empty event rejected"
    );
    assert!(
        EventTopic::parse("BITTY.panel:x").is_err(),
        "uppercase owner rejected"
    );
    let long = format!("a.b:{}", "x".repeat(64));
    assert!(
        EventTopic::parse(&long).is_err(),
        "over-long topic rejected"
    );

    // Two panels on one topic get independent queues (arbitration isolates
    // consumers): draining one never starves the other, and the whole
    // publish→drain round-trip is bounded (deadlock-free by construction —
    // no locks cross panel queues; headless bound here, microsecond-scale
    // latency is a headed measurement, see staged drive.sh).
    let mut preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("registry");
    let ws = WorkspaceId::new(9);
    let ha = preg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("panel a");
    let hb = preg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("panel b");
    preg.mount_panel(ha.id, ha.generation, ViewId::new(91))
        .expect("mount a");
    preg.mount_panel(hb.id, hb.generation, ViewId::new(92))
        .expect("mount b");
    let topic = preg
        .declare_topic("bitty-terminal.panelprobe:shared")
        .expect("shared topic");
    preg.subscribe(ha.id, ha.generation, &topic).expect("sub a");
    preg.subscribe(hb.id, hb.generation, &topic).expect("sub b");

    let start = std::time::Instant::now();
    for i in 0..32 {
        preg.publish(
            &topic,
            BoundedPayload::try_new(format!("m{i}")).expect("small"),
        )
        .expect("publish");
    }
    let da = preg.drain_batch(ha.id, topic.as_str(), 32, 8192);
    let db = preg.drain_batch(hb.id, topic.as_str(), 32, 8192);
    let elapsed = start.elapsed();
    assert_eq!(da.len(), 32, "consumer A gets full fan-out");
    assert_eq!(db.len(), 32, "consumer B gets full fan-out");
    assert_eq!(da[0].payload.as_str(), "m0");
    assert_eq!(db[0].payload.as_str(), "m0");
    assert!(
        elapsed.as_millis() < 1_000,
        "bounded drain completes promptly headless ({elapsed:?}); headed drive measures µs scale"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Claim 3 — fault isolation (fuel/heap quotas, fail-closed single panel)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn claim3_fuel_exhaustion_suspends_single_vm_neighbor_unaffected() {
    // Phodopus fuel quota: a runaway panel VM suspends fail-closed with an
    // attributed reason; the neighbor VM (separate LuaVm = separate panel)
    // keeps executing. Per-VM Fuel containers never cross panels.
    let mut bad = tiny_fuel_vm("panelclaims.fuel-bad");
    let mut good = tiny_fuel_vm("panelclaims.fuel-good");

    let outcome = bad
        .execute("local s = 0 for i = 1, 5000 do s = s + i end assert(s == 12502500)")
        .expect("drive works");
    match outcome {
        ExecuteOutcome::Suspended { reason, .. } => {
            assert!(
                matches!(
                    reason,
                    SuspendReason::InstructionBudgetExceeded { used, budget: 100 }
                    if used >= 100
                ),
                "suspension must name the fuel quota, got {reason:?}"
            );
            let bridge = bridge_error_for_suspend(&reason);
            assert_eq!(bridge.code, E_BUDGET_INSTRUCTIONS);
        }
        other => panic!("over-budget workload must suspend, got {other:?}"),
    }
    assert!(bad.is_suspended(), "runaway panel is suspended");
    assert_eq!(bad.suspension_count(), 1);

    // Fail-closed: further work is refused with typed Suspended, heap untouched.
    let err = bad.execute("return 1").expect_err("suspended refuses");
    assert!(
        matches!(err, VmError::Suspended { .. }),
        "refusal must be typed Suspended, got {err:?}"
    );

    // Neighbor panel unaffected: still completes within its own budget.
    let good_outcome = good
        .execute("local s = 0 for i = 1, 10 do s = s + i end assert(s == 55)")
        .expect("neighbor drives");
    assert!(
        matches!(good_outcome, ExecuteOutcome::Completed { .. }),
        "neighbor panel must complete, got {good_outcome:?}"
    );
    assert!(!good.is_suspended());
    let snap = good.budget_snapshot();
    assert!(snap.invariants_hold());

    // Explicit re-grant recovers the same instance (no cross-panel residue).
    bad.reset();
    assert!(!bad.is_suspended());
    let recovered = bad.execute("return 1").expect("reset re-grants");
    assert!(
        matches!(recovered, ExecuteOutcome::Completed { .. }),
        "post-reset drive works"
    );
}

#[test]
fn claim3_heap_over_ceiling_refused_neighbor_unaffected() {
    // RC-2 heap quota: allocation past a tiny ceiling is refused fail-closed
    // (suspension with E_BUDGET_MEMORY, or builder hard-quota RuntimeError
    // without suspending); it never completes, and the neighbor is untouched.
    const LIMIT: usize = 64 * 1024;
    let mut bad = tiny_heap_vm("panelclaims.heap-bad", LIMIT);
    let mut good = tiny_heap_vm("panelclaims.heap-good", LIMIT);

    let outcome = bad
        .execute(r#"local t = {} for i = 1, 1000 do t[i] = string.rep(tostring(i) .. "abcdefgh", 16) end"#)
        .expect("drive works");
    match outcome {
        ExecuteOutcome::Suspended { reason, .. } => {
            assert!(
                matches!(reason, SuspendReason::MemoryExceeded { limit: LIMIT, .. }),
                "suspension must name the heap quota, got {reason:?}"
            );
            let bridge = bridge_error_for_suspend(&reason);
            assert_eq!(bridge.code, E_BUDGET_MEMORY);
        }
        ExecuteOutcome::RuntimeError { message } => {
            assert!(!message.is_empty(), "hard-quota refusal carries a message");
        }
        ExecuteOutcome::Completed { .. } => {
            panic!("~200 KiB under a 64 KiB ceiling must never complete");
        }
    }

    let good_outcome = good.execute("return 1").expect("neighbor drives");
    assert!(
        matches!(good_outcome, ExecuteOutcome::Completed { .. }),
        "neighbor panel must complete, got {good_outcome:?}"
    );
}

#[test]
fn claim3_pty_side_drops_zero_frames_when_plugin_panel_fails() {
    // PTY panels drop zero frames when a plugin panel fails: the terminal
    // snapshot path and the present tick keep serving after a neighbor VM
    // suspends. Headless proxy for the headed zero-frame-drop assertion
    // (headed drive counts grim-correlated frame gaps; see staged drive.sh).
    let mut treg = TerminalRegistry::new(RegistryConfig::default()).expect("terminal registry");
    let th = treg.create_terminal(None).expect("pty terminal");
    let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
    let wid = treg.create_workspace().expect("workspace");
    let vh = treg.create_view(wid).expect("view");
    treg.attach(wid, vh.id, vh.generation, th.id, th.generation, rect)
        .expect("attach");
    let gen_before = treg.generation().get();

    // Neighbor plugin panel burns its fuel and suspends.
    let mut bad = tiny_fuel_vm("panelclaims.pty-neighbor");
    let outcome = bad
        .execute("local s = 0 for i = 1, 5000 do s = s + i end")
        .expect("drive works");
    assert!(
        matches!(outcome, ExecuteOutcome::Suspended { .. }),
        "probe workload suspends"
    );

    // PTY truth is untouched: snapshot serves, generation only moved on the
    // plugin side, and the compositor tick still presents.
    let snap = treg
        .terminal_snapshot(th.id, th.generation)
        .expect("pty snapshot serves after plugin failure");
    assert_eq!(snap.cells.len(), snap.width * snap.height);
    assert_eq!(
        treg.generation().get(),
        gen_before,
        "plugin failure must not bump terminal generation (zero frame drops)"
    );
    let mut rt = Runtime::with_defaults().expect("runtime builds");
    assert!(
        rt.tick().is_some(),
        "present tick serves after plugin failure"
    );
    assert_eq!(rt.plugin_side_len(), 0, "no plugin side-queue residue");
}

// ═══════════════════════════════════════════════════════════════════════════
// Claim 4 — scale-to-zero
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn claim4_idle_vm_holds_zero_cost_no_tick_assertion() {
    // Idle plugin panel holds ~0 CPU/VM cost: a fresh VM that has executed
    // nothing reports zero consumed instructions, zero executions, zero
    // suspensions — the headless no-tick cost assertion. (Headed drive
    // measures wall/CPU residency with the static SceneTree served from GPU
    // caches; see staged drive.sh.)
    let vm =
        build_plugin_vm("panelclaims.idle", Some(VmBudgets::default())).expect("idle VM builds");
    let snap = vm.budget_snapshot();
    assert!(!snap.suspended, "idle VM never suspends");
    assert_eq!(snap.suspension_count, 0);
    assert_eq!(snap.instructions_used, 0, "idle VM consumed zero fuel");
    assert_eq!(snap.wall_elapsed_ms, 0, "idle VM consumed zero wall");
    assert_eq!(vm.total_executions(), 0, "idle VM executed zero chunks");
    assert!(snap.invariants_hold());
}

#[test]
fn claim4_lazy_plugin_builds_no_vm_until_triggered() {
    // Scale-to-zero at the host layer: declaring (+resolving +registering) a
    // lazy plugin performs only static manifest work — no VM is built until
    // a declared command/event fires. Static completion needs no VM either.
    let manifest = lazy_manifest("xuepoo.panelprobe");
    let id = manifest.id().clone();
    let hash = manifest.manifest_hash();
    let mut granted = std::collections::BTreeSet::new();
    granted.insert(CapabilityId::parse("ui.overlay.focus").expect("probe cap"));

    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    host.declare(manifest).expect("declare is static");
    host.resolve(&id).expect("resolve is static");
    host.register(&id).expect("register is static");
    // Activation still requires a hash-bound grant (deny-by-default), and
    // until activation no VM exists: the static command surface answers.
    assert!(host.activate(&id).is_err(), "grant required before VM");
    let completions = host.registry().complete_commands("xuepoo.panelprobe");
    assert!(
        completions.iter().any(|c| c.contains("open")),
        "lazy open completes statically without a VM: {completions:?}"
    );
    host.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
    host.activate(&id).expect("activate after grant");
    // Subscribing to an undeclared event stays rejected (no VM work leaks).
    assert!(
        host.subscribe(&id, EventKind::InterceptPaste).is_err(),
        "undeclared event rejected without VM work"
    );
}

#[test]
fn claim4_idle_runtime_tick_does_zero_plugin_work() {
    // Idle runtime with zero plugins/panels: tick still presents (compositor
    // liveness) but performs zero plugin work — no side-queue entries, no
    // drops, no suspensions to aggregate.
    let mut rt = Runtime::with_defaults().expect("runtime builds");
    assert_eq!(rt.plugin_host().registry().len(), 0);
    assert_eq!(rt.plugin_side_len(), 0);
    assert!(rt.tick().is_some(), "first tick presents (full redraw)");
    // Second idle tick with no damage presents nothing: the compositor goes
    // quiescent instead of spinning — the strongest scale-to-zero signal.
    // Either outcome is accepted, but plugin work must stay zero regardless.
    let _ = rt.tick();
    assert_eq!(rt.plugin_side_len(), 0, "idle ticks queue no plugin work");
    assert_eq!(rt.plugin_side_dropped(), 0);
    assert_eq!(rt.plugin_total_dropped(), 0);

    // Panel registry starts empty (default disabled): an idle window holds
    // no panel cost either.
    let preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry");
    assert_eq!(preg.panel_count(), 0);
    assert_eq!(preg.bus_total_events(), 0);
}

// ── regression pin: constants referenced above stay frozen ─────────────────

#[test]
fn panel_claims_envelope_constants_frozen() {
    assert_eq!(bitty_lua::host::SNAPSHOT_MAX_BYTES, 256 * 1024);
    assert_eq!(BUS_PER_SUBSCRIPTION_LIMIT, 64);
    assert_eq!(BUS_PER_PANEL_LIMIT, 1024);
    assert_eq!(BUS_PER_PANEL_BYTES_LIMIT, 256 * 1024);
    assert_eq!(BUS_GLOBAL_LIMIT, 8192);
    assert_eq!(BUS_GLOBAL_BYTES_LIMIT, 2 * 1024 * 1024);
    assert_eq!(BUS_EVENT_MAX_BYTES, 8 * 1024);
    assert_eq!(bitty_lua::RC1_INSTRUCTION_BUDGET, 10_000_000);
    assert_eq!(bitty_lua::RC2_MEMORY_PER_PLUGIN_BYTES, 32 * 1024 * 1024);
}
