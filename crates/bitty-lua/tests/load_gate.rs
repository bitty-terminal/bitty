//! Fail-closed load gate (FS-7) and stable bridge-code tests.
//!
//! A plugin VM build without RC-1/RC-2 budgets is refused with a typed
//! [`VmError::Budget`](bitty_lua::VmError); the unguarded path is
//! unreachable because the public plugin constructors demand budgets. Every
//! suspend reason maps to a stable `E_*` code in the `budget` class, with
//! wall-clock timeout sharing the bridge `E_TIMEOUT`.

use bitty_lua::{
    BridgeError, ExecuteOutcome, RC1_INSTRUCTION_BUDGET, RC1_WALL_CLOCK_BUDGET_MS, RC1_WARNING_MS,
    RC2_MEMORY_PER_PLUGIN_BYTES, SuspendReason, VmError,
    error::{
        ALL_BUDGET_CODES, BUDGET_CLASS, E_BUDGET_INSTRUCTIONS, E_BUDGET_MEMORY, E_TIMEOUT,
        RuntimeFault, bridge_error_for_suspend, bridge_error_for_vm_error, is_budget_code,
    },
    gate::{PluginVmBuilder, VmBudgets, build_plugin_vm},
};

#[test]
fn refuses_plugin_vm_without_budgets() {
    let err = match build_plugin_vm("unbudgeted.example", None) {
        Err(err) => err,
        Ok(_) => panic!("must refuse"),
    };
    assert!(
        matches!(err, VmError::Budget(_)),
        "missing budgets must fail closed with Budget, got {err:?}"
    );

    let err = match PluginVmBuilder::new("unbudgeted.example").build() {
        Err(err) => err,
        Ok(_) => panic!("must refuse"),
    };
    assert!(
        matches!(err, VmError::Budget(_)),
        "builder without budgets must fail closed with Budget, got {err:?}"
    );
}

#[test]
fn refuses_zero_budget_dimensions() {
    let base = VmBudgets::default();

    for budgets in [
        VmBudgets {
            instruction_budget: 0,
            ..base
        },
        VmBudgets {
            wall_budget_ms: 0,
            ..base
        },
        VmBudgets {
            memory_limit: 0,
            ..base
        },
    ] {
        let err = match build_plugin_vm("zero.example", Some(budgets)) {
            Err(err) => err,
            Ok(_) => panic!("must refuse"),
        };
        assert!(
            matches!(err, VmError::Budget(_)),
            "zero dimension must fail closed with Budget, got {err:?}"
        );
        assert!(
            base.validate().is_ok(),
            "test baseline budgets must validate"
        );
    }
}

#[test]
fn default_budgets_match_rc_consts() {
    let budgets = VmBudgets::default();
    assert_eq!(budgets.instruction_budget, RC1_INSTRUCTION_BUDGET);
    assert_eq!(budgets.wall_budget_ms, RC1_WALL_CLOCK_BUDGET_MS);
    assert_eq!(budgets.warning_ms, RC1_WARNING_MS);
    assert_eq!(budgets.memory_limit, RC2_MEMORY_PER_PLUGIN_BYTES);
}

#[test]
fn valid_budgets_build_and_execute() {
    let mut vm = build_plugin_vm("ok.example", Some(VmBudgets::default())).expect("build");
    assert!(!vm.is_suspended());
    let outcome = vm.execute("return 1 + 2").expect("execute");
    assert!(
        matches!(outcome, ExecuteOutcome::Completed { .. }),
        "expected completion, got {outcome:?}"
    );

    let mut vm = PluginVmBuilder::new("ok-builder.example")
        .budgets(VmBudgets::default())
        .build()
        .expect("builder build");
    assert_eq!(vm.instruction_budget(), RC1_INSTRUCTION_BUDGET);
    assert_eq!(vm.memory_limit(), RC2_MEMORY_PER_PLUGIN_BYTES);
    let outcome = vm.execute("return 1").expect("execute");
    assert!(matches!(outcome, ExecuteOutcome::Completed { .. }));
}

#[test]
fn every_suspend_reason_maps_to_budget_class() {
    let cases = [
        (
            SuspendReason::WallClockExceeded {
                elapsed_ms: 50,
                budget_ms: 50,
            },
            E_TIMEOUT,
        ),
        (
            SuspendReason::InstructionBudgetExceeded {
                used: 10_000_000,
                budget: 10_000_000,
            },
            E_BUDGET_INSTRUCTIONS,
        ),
        (
            SuspendReason::MemoryExceeded {
                used: 33_554_433,
                limit: 33_554_432,
            },
            E_BUDGET_MEMORY,
        ),
    ];
    for (reason, code) in cases {
        let err = bridge_error_for_suspend(&reason);
        assert_eq!(err.class, BUDGET_CLASS, "reason {reason:?}");
        assert_eq!(err.code, code, "reason {reason:?}");
        assert!(is_budget_code(err.code), "reason {reason:?}");
    }
}

#[test]
fn wall_clock_timeout_matches_host_bridge_timeout() {
    let reason = SuspendReason::WallClockExceeded {
        elapsed_ms: 61,
        budget_ms: 50,
    };
    assert_eq!(bridge_error_for_suspend(&reason), BridgeError::timeout());
}

#[test]
fn runtime_fault_codes_are_stable() {
    assert_eq!(E_TIMEOUT, "E_TIMEOUT");
    assert_eq!(E_BUDGET_INSTRUCTIONS, "E_BUDGET_INSTRUCTIONS");
    assert_eq!(E_BUDGET_MEMORY, "E_BUDGET_MEMORY");
    assert_eq!(BUDGET_CLASS, "budget");
    assert_eq!(ALL_BUDGET_CODES.len(), 3);

    let faults = [
        RuntimeFault::HostOpCancelled {
            elapsed_ms: 50,
            budget_ms: 50,
        },
        RuntimeFault::FuelExhausted {
            used: 10_000_000,
            budget: 10_000_000,
        },
        RuntimeFault::OutOfMemory {
            used: 33_554_433,
            limit: 33_554_432,
        },
    ];
    for fault in faults {
        let err = fault.to_bridge_error();
        assert_eq!(err.class, BUDGET_CLASS, "fault {fault:?}");
        assert_eq!(err.code, fault.code(), "fault {fault:?}");
        assert!(is_budget_code(err.code), "fault {fault:?}");
        assert!(!err.message.is_empty(), "fault {fault:?}");
    }
    assert!(!is_budget_code("E_NO_SUCH_CODE"));
}

#[test]
fn vm_error_mapping_covers_suspension_only() {
    let suspended = VmError::Suspended {
        reason: SuspendReason::MemoryExceeded {
            used: 40_000_000,
            limit: 33_554_432,
        },
    };
    let err = bridge_error_for_vm_error(&suspended).expect("suspension must map");
    assert_eq!(err.class, BUDGET_CLASS);
    assert_eq!(err.code, E_BUDGET_MEMORY);

    for err in [
        VmError::Load("syntax".to_string()),
        VmError::Runtime("boom".to_string()),
        VmError::Budget("zero".to_string()),
    ] {
        assert!(
            bridge_error_for_vm_error(&err).is_none(),
            "non-budget error must not map: {err:?}"
        );
    }
}

#[test]
fn exhausted_vm_maps_through_gate_built_vm() {
    let mut vm = build_plugin_vm(
        "loop.example",
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
        other => panic!("expected suspension, got {other:?}"),
    };
    let err = bridge_error_for_suspend(&reason);
    assert_eq!(err.code, E_BUDGET_INSTRUCTIONS);
    assert_eq!(err.class, BUDGET_CLASS);

    let refused = vm.execute("return 1").expect_err("must refuse");
    assert!(
        matches!(refused, VmError::Suspended { .. }),
        "suspended VM must refuse: {refused:?}"
    );
}
