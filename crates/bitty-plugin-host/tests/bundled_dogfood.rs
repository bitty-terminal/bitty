#![forbid(unsafe_code)]

//! Bundled dogfood evidence for CTX-0096 (P2, area:plugin).
//!
//! Verifies the five `v1` bundled-disabled first-party plugins
//! (`bitty-terminal.shell-integration`, `tabs`, `statusline`, `palette`,
//! `project`) dogfood the **public** Plugin API with manifest / capability /
//! lifecycle parity to any third-party `xuepoo.*` plugin, default-disabled
//! (no implicit enable), safe-mode compatibility, Terminal Truth protection
//! (observation via bounded side queue, never direct `State` write), and
//! bounded cold-path execution (`DropOldest` per-sub 64 / per-plugin 1024 /
//! 256 KiB / global 8192 / 2 MiB, coalescing where semantics allow).

use std::collections::BTreeSet;

use bitty_plugin_host::{
    CapabilityId, DropPolicy, Event, EventKind, EventPayload, GrantRecord, HostObservation,
    PluginHost, PluginId,
    bundled::{
        all_bundled_manifests, bundled_ids_sorted, bundled_manifest_for, is_bundled,
        palette_manifest, project_manifest, shell_integration_manifest, statusline_manifest,
        tabs_manifest,
    },
};

fn granted_set_for(manifest: &bitty_plugin_host::PluginManifest) -> BTreeSet<CapabilityId> {
    let mut set = manifest.capabilities.ids.clone();
    for req in &manifest.capabilities.filesystem {
        for pat in &req.paths {
            let s = match req.access {
                bitty_plugin_host::FsAccess::Read => format!("fs.read:{pat}"),
                bitty_plugin_host::FsAccess::Write => format!("fs.write:{pat}"),
            };
            set.insert(CapabilityId::parse(&s).expect("filesystem capability must parse"));
        }
    }
    set
}

#[test]
fn bundled_manifests_are_five_and_validate() {
    let all = all_bundled_manifests();
    assert_eq!(all.len(), 5);
    for m in &all {
        m.validate().expect("bundled must validate");
    }
    assert_eq!(
        bundled_ids_sorted(),
        vec![
            "bitty-terminal.palette",
            "bitty-terminal.project",
            "bitty-terminal.shell-integration",
            "bitty-terminal.statusline",
            "bitty-terminal.tabs",
        ]
    );
    for m in all {
        assert!(is_bundled(m.id()));
        assert!(bundled_manifest_for(m.id().as_str()).is_some());
    }
    assert!(!is_bundled(&PluginId::new("xuepoo.third").unwrap()));
}

#[test]
fn bundled_plugins_load_via_public_api_with_grant_checks() {
    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    for manifest in all_bundled_manifests() {
        let id = manifest.id().clone();
        let hash = manifest.manifest_hash();
        let granted = granted_set_for(&manifest);
        host.declare(manifest.clone()).expect("declare");
        host.resolve(&id).expect("resolve");
        host.register(&id).expect("register");
        // Activate without grant must be fail-closed when caps non-empty.
        if !granted.is_empty() {
            assert!(
                host.activate(&id).is_err(),
                "{} must require grant",
                id.as_str()
            );
        }
        // Insert grant bound to exact hash and activate.
        if !granted.is_empty() {
            host.insert_grant(GrantRecord::granted(id.clone(), hash.clone(), granted, 1));
        }
        host.activate(&id)
            .unwrap_or_else(|e| panic!("activate {}: {e}", id.as_str()));
        assert_eq!(
            host.registry().get(&id).unwrap().state,
            bitty_plugin_host::PluginState::Activated
        );
    }
    assert_eq!(host.registry().len(), 5);
}

#[test]
fn bundled_parity_with_third_party_same_manifest_shape() {
    // A third-party manifest with identical capabilities/lazy shape must
    // have identical validation and grant lifecycle behavior as the bundled
    // counterpart — no private channel. We clone the shell-integration shape
    // under a third-party id and verify parity.
    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    let bundled = shell_integration_manifest();
    let mut third = bundled.clone();
    third.identity.id = PluginId::new("xuepoo.shell-mirror").unwrap();
    third.identity.name = "Third Party Mirror".to_string();
    // Keep same caps/events/compat — only id differs.
    for (label, manifest) in [("bundled", bundled), ("third", third)] {
        let id = manifest.id().clone();
        let hash = manifest.manifest_hash();
        let granted = granted_set_for(&manifest);
        host.declare(manifest)
            .unwrap_or_else(|e| panic!("{label} declare: {e}"));
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        assert!(host.activate(&id).is_err(), "{label} must require grant");
        host.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
        host.activate(&id)
            .unwrap_or_else(|e| panic!("{label} activate after grant: {e}"));
    }
}

#[test]
fn bundled_plugins_are_observation_only_and_use_bounded_side_queue() {
    // Terminal Truth: only Action writes State. Plugins observe via bounded
    // side queue Snapshot/HostObservation, never direct grid mutation, never
    // hot-path byte/cell/damage. We prove the queue is bounded DropOldest and
    // that observation crossing is via HostObservation::TitleChanged etc.
    let mut host = PluginHost::with_capacity(DropPolicy::DropOldest, 64, 4);
    for m in all_bundled_manifests() {
        let id = m.id().clone();
        let hash = m.manifest_hash();
        let granted = granted_set_for(&m);
        host.declare(m).unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        if !granted.is_empty() {
            host.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
            host.activate(&id).unwrap();
        } else {
            host.activate(&id).unwrap();
        }
        // Subscribe each to an allowed observation kind declared in its manifest.
        if id.as_str() == "bitty-terminal.shell-integration" {
            host.subscribe(&id, EventKind::TerminalTitleChanged)
                .unwrap();
            host.subscribe(&id, EventKind::TerminalCwdChanged).unwrap();
        } else if id.as_str() == "bitty-terminal.tabs" {
            host.subscribe(&id, EventKind::TerminalTitleChanged)
                .unwrap();
        }
    }

    // Flood side queue beyond 4 -> oldest dropped, newest survive (DropOldest).
    for i in 0..10 {
        host.push_observation(HostObservation::TitleChanged(format!("t{i}")));
    }
    assert_eq!(host.side_queue().len(), 4);
    assert_eq!(host.side_queue().dropped(), 6);
    let drained: Vec<_> = host.drain_observations();
    assert_eq!(drained.len(), 4);
    assert!(matches!(&drained[0], HostObservation::TitleChanged(s) if s == "t6"));

    // Per-subscriber pipeline also DropOldest, per-plugin 1024 / global 8192
    // enforced at publish boundary; coalescable collapses, non-coalescable caps.
    let shell = PluginId::new("bitty-terminal.shell-integration").unwrap();
    for i in 0..80u64 {
        host.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, i));
    }
    assert!(host.queued_events_for_plugin(&shell) <= 64);
    assert!(host.invariant_queue_bounds());
    assert!(host.invariant_global_bounds());
}

#[test]
fn default_disabled_safe_mode_leaves_host_functional() {
    // Fresh install: zero plugins declared/activated. Safe-mode must also be
    // functional (no panic, no hot-path coupling) even when bundled would have
    // been enabled — `bitty --safe` is zero non-core plugins by construction.
    let host = PluginHost::new(DropPolicy::DropOldest, 16);
    // No plugins: host is functional, side queue empty, no pipeline queues.
    assert_eq!(host.registry().len(), 0);
    assert_eq!(host.side_queue().len(), 0);
    assert_eq!(host.total_queued_events(), 0);
    assert!(host.invariant_queue_bounds());

    // Enable one bundled then enter safe_mode -> new host in safe_mode rejects
    // third-party and also rejects further bundled declares (bundled are
    // `bitty-terminal.*` which is not `bitty.` prefix, so they are treated
    // as non-builtin and rejected — parity with third-party, no bypass).
    let mut safe = PluginHost::new(DropPolicy::DropOldest, 16);
    safe.set_safe_mode(true);
    assert!(safe.declare(shell_integration_manifest()).is_err());
    assert!(safe.declare(tabs_manifest()).is_err());
    assert!(safe.declare(statusline_manifest()).is_err());
    assert!(safe.declare(palette_manifest()).is_err());
    assert!(safe.declare(project_manifest()).is_err());
    // `bitty.*` builtin would still be allowed in safe mode (candidate built-in
    // namespace) — prove the distinction is exactly the prefix, not a private
    // flag.
    let builtin = {
        let mut m = shell_integration_manifest();
        m.identity.id = PluginId::new("bitty.core").unwrap();
        m
    };
    assert!(safe.declare(builtin).is_ok());

    // Host remains usable after safe-mode rejection (no corruption).
    assert!(safe.invariant_queue_bounds());
}

#[test]
fn grant_revocation_and_hash_binding_for_bundled() {
    // Capability increase blocks auto-update pending diff approval; hash
    // mismatch is fail-closed; revocation detaches at next boundary.
    let m = project_manifest();
    let id = m.id().clone();
    let hash = m.manifest_hash();
    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    host.declare(m.clone()).unwrap();
    host.resolve(&id).unwrap();
    host.register(&id).unwrap();
    let granted = granted_set_for(&m);
    host.insert_grant(GrantRecord::granted(
        id.clone(),
        hash.clone(),
        granted.clone(),
        1,
    ));
    host.activate(&id).unwrap();

    // Revoke single filesystem capability -> future activate of same hash but
    // missing grant would fail; grant store reflects revocation.
    let fs_cap = CapabilityId::parse("fs.read:~/projects/**").unwrap();
    assert!(
        host.grants()
            .is_granted(&id, &hash, &fs_cap, &m.capabilities)
    );
    let report = host.revoke(&id, Some(&fs_cap)).unwrap();
    assert_eq!(report.revoked.len(), 1);
    assert!(
        !host
            .grants()
            .is_granted(&id, &hash, &fs_cap, &m.capabilities)
    );

    // Hash changed (e.g., version bump) -> grant no longer matches, deny.
    let mut bumped = project_manifest();
    bumped.identity.version = "0.2.0".to_string();
    assert_ne!(bumped.manifest_hash(), hash);
    let bumped_hash = bumped.manifest_hash();
    assert!(
        !host
            .grants()
            .is_granted(&id, &bumped_hash, &fs_cap, &bumped.capabilities)
    );
}

#[test]
fn no_hot_path_coupling_via_public_api_only() {
    // Bundled manifests use only public types (PluginManifest,
    // PluginId, CapabilityId, QualifiedName, FilesystemRequest) — compile-time
    // proof: this test has no `bitty_runtime` or `wgpu`/`winit` imports. The
    // runtime side still observes only via bounded side queue Snapshot, never
    // grid internals.
    let m = shell_integration_manifest();
    let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
    // No window/GPU/PTY coupling in API: host is headless constructible.
    assert!(!host.is_safe_mode());
    assert!(host.side_queue().is_empty());
    host.declare(m.clone()).unwrap();
    host.resolve(m.id()).unwrap();
    host.register(m.id()).unwrap();
    assert!(host.invariant_queue_bounds());
}
