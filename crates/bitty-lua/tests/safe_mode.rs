//! Safe-mode verification (FS-8).
//!
//! `--safe` starts with zero third-party plugins: the safe load policy drops
//! every third-party candidate before any VM is built, so hostile fixtures
//! stay inert (never loaded, never executed). A safe-config VM additionally
//! starts with zero third-party surface: no `bitty` host module, no ambient
//! `io`/`package`/`debug` authority, and no cross-plugin visibility.

use bitty_lua::{
    ExecuteOutcome, RC2_MEMORY_PER_PLUGIN_BYTES, SuspendReason, VmError,
    error::{E_BUDGET_INSTRUCTIONS, bridge_error_for_suspend},
    gate::{
        LoadPolicy, PluginCandidate, VmBudgets, build_plugin_vm, count_third_party_selected,
        select_candidates,
    },
};

/// Hostile third-party candidates: each pairs an identity with a Lua source
/// that a compromised or malicious plugin might ship.
fn hostile_candidates() -> Vec<(PluginCandidate, &'static str)> {
    vec![
        (
            PluginCandidate::third_party("evil-exec.example"),
            "os.execute('touch pwned')",
        ),
        (
            PluginCandidate::third_party("evil-io.example"),
            "local f = io.open('secrets', 'r')",
        ),
        (
            PluginCandidate::third_party("evil-require.example"),
            "require('backdoor')",
        ),
        (
            PluginCandidate::third_party("evil-loop.example"),
            "while true do end",
        ),
    ]
}

#[test]
fn safe_mode_admits_zero_third_party() {
    let policy = LoadPolicy::safe_mode();
    assert!(policy.is_safe_mode());
    assert!(!policy.allows_third_party());

    let mut candidates = vec![PluginCandidate::first_party("core.example")];
    candidates.extend(hostile_candidates().into_iter().map(|(c, _)| c));

    let selected = select_candidates(&policy, &candidates);
    assert_eq!(selected.len(), 1, "only first-party loads: {selected:?}");
    assert_eq!(selected[0].id, "core.example");
    assert_eq!(count_third_party_selected(&policy, &candidates), 0);
}

#[test]
fn safe_mode_with_only_hostile_candidates_loads_nothing() {
    let policy = LoadPolicy::safe_mode();
    let candidates: Vec<PluginCandidate> =
        hostile_candidates().into_iter().map(|(c, _)| c).collect();

    let selected = select_candidates(&policy, &candidates);
    assert!(selected.is_empty(), "hostile set must load nothing");

    // Proof the fixtures stay inert: no VM is ever built for them.
    let mut executed = 0_u32;
    for candidate in &selected {
        let mut vm =
            build_plugin_vm(candidate.id.clone(), Some(VmBudgets::default())).expect("build");
        let _ = vm.execute("return 1");
        executed += 1;
    }
    assert_eq!(executed, 0, "no hostile source may execute");
}

#[test]
fn safe_config_vm_starts_with_zero_third_party_surface() {
    let mut vm = build_plugin_vm("safe.example", Some(VmBudgets::default())).expect("build");

    // No host module is installed: `bitty` is nil.
    let outcome = vm.execute("assert(bitty == nil)").expect("execute");
    assert!(
        matches!(outcome, ExecuteOutcome::Completed { .. }),
        "safe VM must have no bitty surface: {outcome:?}"
    );

    // No ambient authority: `io` is never loaded (core without `load_io`),
    // there are no file loaders, and `os` stays narrowed by the host.
    let outcome = vm
        .execute(
            "assert(io == nil and loadfile == nil and dofile == nil \
             and os.execute == nil and os.getenv == nil and os.remove == nil)",
        )
        .expect("execute");
    assert!(
        matches!(outcome, ExecuteOutcome::Completed { .. }),
        "safe VM must have no ambient surface: {outcome:?}"
    );

    // In-core sandbox baseline (Phodopus core, post-RUN-28): `package`,
    // `debug`, and `load` exist but narrowed — empty `package.path` with no
    // native loader, traceback-only `debug`, text-only `load` — so an
    // unmounted module still resolves to nothing.
    let outcome = vm
        .execute(
            "assert(package ~= nil and package.path == \"\" and package.loadlib == nil) \
             assert(debug ~= nil and type(debug.traceback) == \"function\") \
             assert(load ~= nil and load(\"return 40 + 2\", \"probe\", \"t\")() == 42) \
             assert(pcall(require, \"no.such.module\") == false)",
        )
        .expect("execute");
    assert!(
        matches!(outcome, ExecuteOutcome::Completed { .. }),
        "safe VM must keep the narrowed in-core baseline: {outcome:?}"
    );
}

#[test]
fn hostile_fixtures_stay_inert_in_safe_vms() {
    // Each hostile source runs in a fresh gate-built VM and achieves nothing:
    // ambient calls fail as runtime errors without effects, and the loop is
    // contained by the instruction budget, then fail-closed.
    let ambient_cases = [
        "os.execute('touch pwned')",
        "local f = io.open('secrets', 'r')",
        "require('backdoor')",
    ];
    for source in ambient_cases {
        let mut vm =
            build_plugin_vm("hostile-target.example", Some(VmBudgets::default())).expect("build");
        let outcome = vm.execute(source).expect("execute");
        assert!(
            matches!(outcome, ExecuteOutcome::RuntimeError { .. }),
            "hostile source must fail without effects: {source}: {outcome:?}"
        );
        assert!(!vm.is_suspended());
    }

    // The infinite loop suspends on fuel and maps to the stable budget code.
    let mut vm = build_plugin_vm(
        "hostile-loop.example",
        Some(VmBudgets {
            instruction_budget: 1_000,
            wall_budget_ms: 60_000,
            warning_ms: 8,
            memory_limit: RC2_MEMORY_PER_PLUGIN_BYTES,
        }),
    )
    .expect("build");
    let outcome = vm.execute("while true do end").expect("execute");
    let reason = match outcome {
        ExecuteOutcome::Suspended { reason, .. } => reason,
        other => panic!("loop must suspend, got {other:?}"),
    };
    assert!(
        matches!(reason, SuspendReason::InstructionBudgetExceeded { .. }),
        "loop must exhaust fuel, got {reason:?}"
    );
    let err = bridge_error_for_suspend(&reason);
    assert_eq!(err.code, E_BUDGET_INSTRUCTIONS);

    // Fail-closed: the suspended VM refuses further execution.
    let refused = vm.execute("return 1").expect_err("must refuse");
    assert!(
        matches!(refused, VmError::Suspended { .. }),
        "suspended VM must refuse: {refused:?}"
    );
}

#[test]
fn plugins_cannot_observe_each_other() {
    let mut first = build_plugin_vm("first.example", Some(VmBudgets::default())).expect("build");
    let outcome = first.execute("secret = 42").expect("execute");
    assert!(matches!(outcome, ExecuteOutcome::Completed { .. }));

    let mut second = build_plugin_vm("second.example", Some(VmBudgets::default())).expect("build");
    let outcome = second.execute("assert(secret == nil)").expect("execute");
    assert!(
        matches!(outcome, ExecuteOutcome::Completed { .. }),
        "second VM must not see first VM globals: {outcome:?}"
    );
}

#[test]
fn standard_policy_still_loads_first_party() {
    let policy = LoadPolicy::standard();
    assert!(!policy.is_safe_mode());
    assert!(policy.allows_third_party());

    let candidates = vec![
        PluginCandidate::first_party("core.example"),
        PluginCandidate::third_party("opt-in.example"),
    ];
    let selected = select_candidates(&policy, &candidates);
    assert_eq!(selected.len(), 2);
    assert_eq!(count_third_party_selected(&policy, &candidates), 1);
}
