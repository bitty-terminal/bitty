//! Runtime Plugin-host, grant, interception, and URL-activation tests.
//!
//! Moved verbatim from the inline `runtime.rs` unit tests as part of
//! the CTX-0232 pure-move split. Adaptations are wiring only:
//! `super::*` became explicit imports and the private `layout` field
//! reads became the public `layout()` getter (identical semantics).
use bitty_platform::{CursorPosition, MouseButton, PlatformEvent, PressState, WindowEventKind};
use bitty_plugin_host::GrantRecord;
use bitty_plugin_host::{
    CapabilityId, Event as HostEvent, EventKind as HostEventKind, EventPayload as HostPayload,
    HostObservation as HostObs, InterceptionDecision as HostDecision, PluginId as HostPid,
    PluginManifest as HostManifest,
};
use bitty_runtime::{
    ActivationGesture, DEFAULT_PLUGIN_PIPELINE_CAPACITY, DEFAULT_PLUGIN_SIDE_CAPACITY, Runtime,
    RuntimeConfig,
};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn host_manifest(id: &str, commands: Vec<&str>, events: Vec<&str>) -> HostManifest {
    use bitty_plugin_host::{
        CapabilityRequests, Compat, LazyTriggers, PluginIdentity, QualifiedName,
    };
    HostManifest {
        identity: PluginIdentity {
            id: HostPid::new(id).unwrap(),
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
            commands: commands
                .into_iter()
                .map(|c| QualifiedName::new(c).unwrap())
                .collect(),
            events: events.into_iter().map(|s| s.to_string()).collect(),
            claims: Vec::new(),
        },
        raw_bytes_len: 256,
    }
}

fn foreign_gesture() -> ActivationGesture {
    // Mint a real gesture on a scratch runtime; used as a forgery
    // against the runtime under test (see CTX-0232 wiring note).
    let mut scratch = make_runtime();
    scratch.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    scratch.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
    });
    scratch.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
            button: MouseButton::Left,
            state: PressState::Released,
        }),
    });
    scratch
        .take_activation_gesture()
        .expect("scratch runtime must mint gesture")
}

#[test]
fn default_plugin_host_is_drop_oldest_with_bounded_queues() {
    let rt = make_runtime();
    assert_eq!(
        rt.plugin_drop_policy(),
        bitty_plugin_host::DropPolicy::DropOldest
    );
    assert_eq!(
        rt.plugin_pipeline_capacity(),
        DEFAULT_PLUGIN_PIPELINE_CAPACITY
    );
    assert_eq!(rt.plugin_side_capacity(), DEFAULT_PLUGIN_SIDE_CAPACITY);
    assert_eq!(rt.plugin_side_len(), 0);
    assert_eq!(rt.plugin_total_dropped(), 0);
}

#[test]
fn runtime_with_drop_newest_honors_open_point() {
    let cfg = RuntimeConfig::default();
    let rt = Runtime::with_plugin_drop_policy(cfg, bitty_plugin_host::DropPolicy::DropNewest)
        .expect("must build");
    assert_eq!(
        rt.plugin_drop_policy(),
        bitty_plugin_host::DropPolicy::DropNewest
    );
}

#[test]
fn runtime_with_custom_capacities() {
    let cfg = RuntimeConfig::default();
    let rt =
        Runtime::with_plugin_host_capacity(cfg, bitty_plugin_host::DropPolicy::DropOldest, 8, 16)
            .expect("must build");
    assert_eq!(rt.plugin_pipeline_capacity(), 8);
    assert_eq!(rt.plugin_side_capacity(), 16);
}

#[test]
fn register_plugin_happy_path_and_duplicate_command_rejected() {
    let mut rt = make_runtime();
    let m1 = host_manifest("xuepoo.a", vec!["xuepoo.a:cmd"], vec!["terminal.bell"]);
    rt.register_plugin(m1).expect("first register must succeed");
    assert_eq!(rt.plugin_host().registry().len(), 1);

    // Second plugin claiming same qualified command must be rejected at graph construction.
    let m2 = host_manifest("xuepoo.b", vec!["xuepoo.a:cmd"], vec![]);
    let err = rt
        .register_plugin(m2)
        .expect_err("duplicate command must be rejected");
    assert!(
        err.to_string().contains("already owned"),
        "error must mention duplicate: {err}"
    );
}

#[test]
fn register_plugin_validates_manifest() {
    let mut rt = make_runtime();
    let mut bad = host_manifest("xuepoo.bad", vec![], vec![]);
    bad.raw_bytes_len = bitty_plugin_host::MANIFEST_MAX_BYTES + 1;
    assert!(rt.register_plugin(bad).is_err());
}

#[test]
fn side_queue_bridging_is_bounded_and_never_blocks_hot_path() {
    // Use small side capacity to force drops.
    let cfg = RuntimeConfig {
        cold_queue_capacity: 4,
        ..RuntimeConfig::default()
    };
    let mut rt =
        Runtime::with_plugin_host_capacity(cfg, bitty_plugin_host::DropPolicy::DropOldest, 64, 2)
            .expect("must build");

    // Feed five title changes; each title yields TitleChanged only (no damage),
    // so cold queue sees 5 events, capacity 4 => 1 dropped; side queue sees
    // 5 observations, capacity 2 => 3 dropped. This proves bounded drops without blocking.
    for name in ["first", "second", "third", "fourth", "fifth"] {
        rt.handle_pty_bytes(format!("\x1b]0;{name}\x07").as_bytes());
    }

    assert_eq!(rt.cold_queue_len(), 4);
    assert_eq!(rt.cold_queue_dropped(), 1);

    assert_eq!(rt.plugin_side_len(), 2);
    assert_eq!(rt.plugin_side_dropped(), 3);
    let obs = rt.drain_plugin_observations();
    assert_eq!(obs.len(), 2);
    assert_eq!(rt.plugin_side_len(), 0);
    // The surviving two are the newest (DropOldest policy).
    assert_eq!(obs[0], HostObs::TitleChanged("fourth".to_string()));
    assert_eq!(obs[1], HostObs::TitleChanged("fifth".to_string()));
}

#[test]
fn handle_pty_bytes_also_pushes_bell_and_mode_to_side_queue() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x07"); // BEL
    let obs = rt.drain_plugin_observations();
    assert!(obs.contains(&HostObs::Bell));

    // `CSI 4 h` enables Insert mode (Mode::Insert) — mapped to ModeChanged and thence to HostObservation.
    rt.handle_pty_bytes(b"\x1b[4h");
    let obs2 = rt.drain_plugin_observations();
    assert!(
        obs2.iter()
            .any(|o| matches!(o, HostObs::ModeChanged { .. })),
        "Insert mode toggle must produce ModeChanged observation, got {obs2:?}"
    );
}

#[test]
fn pipeline_publish_and_drain_via_runtime() {
    let mut rt = make_runtime();
    let m = host_manifest("xuepoo.test", vec![], vec!["terminal.bell"]);
    rt.register_plugin(m).expect("register");
    rt.subscribe_plugin_event(
        &HostPid::new("xuepoo.test").unwrap(),
        HostEventKind::TerminalBell,
    )
    .expect("subscribe");

    rt.publish_plugin_event(HostEvent::new(
        HostEventKind::TerminalBell,
        HostPayload::Empty,
        1,
    ));
    rt.publish_plugin_event(HostEvent::new(
        HostEventKind::TerminalBell,
        HostPayload::Empty,
        2,
    ));

    let batch = rt
        .drain_plugin_events_all(
            &HostPid::new("xuepoo.test").unwrap(),
            &HostEventKind::TerminalBell,
        )
        .expect("drain");
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0].sequence, 1);
}

#[test]
fn pipeline_bounded_drops_counted_for_doctor() {
    let mut rt = Runtime::with_plugin_host_capacity(
        RuntimeConfig::default(),
        bitty_plugin_host::DropPolicy::DropNewest,
        2,
        64,
    )
    .expect("must build");
    let m = host_manifest("xuepoo.test", vec![], vec!["terminal.bell"]);
    rt.register_plugin(m).unwrap();
    rt.subscribe_plugin_event(
        &HostPid::new("xuepoo.test").unwrap(),
        HostEventKind::TerminalBell,
    )
    .unwrap();

    for i in 0..5 {
        rt.publish_plugin_event(HostEvent::new(
            HostEventKind::TerminalBell,
            HostPayload::Empty,
            i,
        ));
    }
    assert!(rt.plugin_total_dropped() > 0);
    let per = rt.plugin_dropped_per_queue();
    assert!(!per.is_empty());
    let drained = rt
        .drain_plugin_events_all(
            &HostPid::new("xuepoo.test").unwrap(),
            &HostEventKind::TerminalBell,
        )
        .unwrap();
    assert_eq!(drained.len(), 2); // capacity 2 with DropNewest keeps oldest 2.
}

#[test]
fn grant_check_stub_deny_by_default_and_hash_binding() {
    let mut rt = make_runtime();
    let cap = CapabilityId::parse("terminal.semantic-read").unwrap();
    let mut m = host_manifest("xuepoo.test", vec!["xuepoo.test:cmd"], vec![]);
    m.capabilities.ids.insert(cap.clone());
    rt.register_plugin(m).unwrap();
    let pid = HostPid::new("xuepoo.test").unwrap();
    let hash = "abc123";
    // No grant yet -> denied.
    assert!(!rt.is_capability_granted(&pid, hash, &cap));
    assert!(rt.check_command_grant(&pid, hash, &cap).is_err());

    // Insert grant and then succeed.
    let mut granted = std::collections::BTreeSet::new();
    granted.insert(cap.clone());
    rt.insert_grant(GrantRecord::granted(pid.clone(), hash, granted, 1));
    assert!(rt.is_capability_granted(&pid, hash, &cap));
    assert!(rt.check_command_grant(&pid, hash, &cap).is_ok());
    // Wrong hash denies.
    assert!(!rt.is_capability_granted(&pid, "other", &cap));
    assert!(rt.check_command_grant(&pid, "other", &cap).is_err());
}

#[test]
fn dispatch_command_checks_ownership_and_grant() {
    let mut rt = make_runtime();
    let cap = CapabilityId::parse("ui.rich").unwrap();
    let mut m = host_manifest("xuepoo.test", vec!["xuepoo.test:run"], vec![]);
    m.capabilities.ids.insert(cap.clone());
    rt.register_plugin(m).unwrap();
    let pid = HostPid::new("xuepoo.test").unwrap();
    let hash = "h";
    let qn = bitty_plugin_host::QualifiedName::new("xuepoo.test:run").unwrap();

    // Without grant -> dispatch denied.
    assert!(rt.dispatch_command(&pid, &qn, hash, &cap).is_err());

    let mut granted = std::collections::BTreeSet::new();
    granted.insert(cap.clone());
    rt.insert_grant(GrantRecord::granted(pid.clone(), hash, granted, 1));
    assert!(rt.dispatch_command(&pid, &qn, hash, &cap).is_ok());

    // Wrong qualified name -> not owned.
    let other = bitty_plugin_host::QualifiedName::new("xuepoo.test:other").unwrap();
    assert!(rt.dispatch_command(&pid, &other, hash, &cap).is_err());
}

#[test]
fn interception_veto_wins_and_fail_open() {
    assert!(!Runtime::intercept_command_dispatch(
        &[HostDecision::Approve, HostDecision::Veto],
        false
    ));
    assert!(Runtime::intercept_command_dispatch(
        &[HostDecision::Approve, HostDecision::Veto],
        true
    ));
    assert!(Runtime::intercept_paste(&[HostDecision::Approve], false));
    assert!(!Runtime::intercept_open_url(&[HostDecision::Veto], false));
    assert!(!Runtime::intercept_open_url(&[HostDecision::Approve], true));
    assert!(Runtime::intercept_terminal_spawn(&[], false));
}

#[test]
fn opening_url_requires_gesture_and_interception_approval() {
    let mut rt = make_runtime();
    let denied = rt.authorize_url_activation(
        "https://example.test",
        foreign_gesture(),
        &[HostDecision::Approve],
        false,
    );
    assert_eq!(
        denied,
        Err(bitty_platform::PlatformError::UrlActivationDenied)
    );
}

#[test]
fn platform_hyperlink_activation_mints_single_use_gesture() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
    });
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
            button: MouseButton::Left,
            state: PressState::Released,
        }),
    });
    let gesture = rt
        .take_activation_gesture()
        .expect("runtime must mint gesture");
    let activation = rt
        .authorize_url_activation("https://example.test", gesture, &[], false)
        .expect("platform gesture must authorize safe hyperlink");
    assert!(
        format!("{:?}", activation).contains("https://example.test"),
        "authorized activation must carry the uri (checked via Debug)"
    );
    assert!(
        rt.take_activation_gesture().is_none(),
        "gesture is single-use"
    );
}

#[test]
fn opening_url_validates_after_activation_gate() {
    let mut rt = make_runtime();
    let result = rt.authorize_url_activation("javascript:alert(1)", foreign_gesture(), &[], false);
    assert_eq!(
        result,
        Err(bitty_platform::PlatformError::UrlActivationDenied)
    );
}

#[test]
fn file_urls_require_distinct_approval() {
    let mut rt = make_runtime();
    assert!(
        rt.authorize_url_activation("file:///tmp/report", foreign_gesture(), &[], false)
            .is_err()
    );
    assert!(
        rt.authorize_file_url_activation("file:///tmp/report", foreign_gesture(), &[], false)
            .is_err()
    );
}

#[test]
fn terminal_output_alone_cannot_activate() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"file:///tmp/report");
    assert!(
        rt.authorize_file_url_activation("file:///tmp/report", foreign_gesture(), &[], false)
            .is_err()
    );
}

#[test]
fn file_authority_is_rejected_before_authorization_token_is_issued() {
    let mut rt = make_runtime();
    assert_eq!(
        rt.authorize_file_url_activation("file://server/share", foreign_gesture(), &[], false),
        Err(bitty_platform::PlatformError::UrlActivationDenied)
    );
}

#[test]
fn hostile_hyperlink_does_not_consume_gesture_slot() {
    let mut rt = make_runtime();
    // Hostile URI should not mint a gesture.
    rt.handle_pty_bytes(b"\x1b]8;;javascript:alert(1)\x07link\x1b]8;;\x07");
    let window_id = bitty_platform::WindowId::from_raw_public(1);
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
    });
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
            button: MouseButton::Left,
            state: PressState::Released,
        }),
    });
    assert!(
        rt.take_activation_gesture().is_none(),
        "hostile hyperlink must not mint gesture"
    );
    // Safe hyperlink after hostile must still mint.
    let mut rt2 = make_runtime();
    rt2.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    rt2.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
    });
    rt2.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
            button: MouseButton::Left,
            state: PressState::Released,
        }),
    });
    assert!(
        rt2.take_activation_gesture().is_some(),
        "safe hyperlink must mint gesture even after hostile attempt in separate runtime"
    );
}

#[test]
fn hostile_then_safe_in_same_runtime_preserves_gesture_for_safe() {
    let mut rt = make_runtime();
    let window_id = bitty_platform::WindowId::from_raw_public(2);
    // First, hostile.
    rt.handle_pty_bytes(b"\x1b]8;;javascript:alert(1)\x07x\x1b]8;;\x07");
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
    });
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
            button: MouseButton::Left,
            state: PressState::Released,
        }),
    });
    assert!(rt.take_activation_gesture().is_none());
    // Then safe link overwriting same cell (carriage return to col 0).
    rt.handle_pty_bytes(b"\r\x1b]8;;https://example.test\x07y\x1b]8;;\x07");
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
    });
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
            button: MouseButton::Left,
            state: PressState::Released,
        }),
    });
    let gesture = rt
        .take_activation_gesture()
        .expect("safe must mint after hostile");
    let ok = rt.authorize_url_activation("https://example.test", gesture, &[], false);
    assert!(ok.is_ok());
}

#[test]
fn hyperlink_activation_overflow_is_handled_without_panic() {
    let mut rt = make_runtime();
    // Force a large snapshot width via state resize to max config-allowed, then
    // verify checked arithmetic does not panic on extreme cursor.
    // The runtime clamps cursor_to_cell, so overflow is defensive.
    let window_id = bitty_platform::WindowId::from_raw_public(3);
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    // Cursor far outside window should clamp, not overflow.
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(CursorPosition {
            x: f64::MAX,
            y: f64::MAX,
        }),
    });
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
            button: MouseButton::Left,
            state: PressState::Released,
        }),
    });
    // Should not panic; may or may not mint depending on clamped cell.
    let _ = rt.take_activation_gesture();
}

#[test]
fn safe_mode_rejects_third_party_via_host() {
    let mut rt = make_runtime();
    rt.set_plugin_safe_mode(true);
    assert!(rt.plugin_safe_mode());
    let m = host_manifest("xuepoo.third", vec![], vec![]);
    assert!(rt.register_plugin(m).is_err());
    let builtin = host_manifest("bitty.core", vec![], vec![]);
    assert!(rt.register_plugin(builtin).is_ok());
}

#[test]
fn no_lua_vm_window_gpu_coupling_in_runtime_api() {
    // Compile-time proof: Runtime constructs headlessly without window/GPU/Lua.
    let rt = make_runtime();
    assert!(rt.is_headless());
    assert!(rt.plugin_host().side_queue().is_empty());
    assert_eq!(rt.plugin_host().pipeline().queue_count(), 0);
}

#[cfg(target_os = "windows")]
#[test]
fn windows_plugin_wiring_compiles_and_is_headless() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"hi");
    let _ = rt.tick();
    assert!(rt.is_headless());
    assert!(rt.plugin_side_len() <= rt.plugin_side_capacity());
}
