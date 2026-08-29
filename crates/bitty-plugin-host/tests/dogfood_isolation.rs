#![forbid(unsafe_code)]

//! Dogfood plugin isolation evidence — Phase E cheap stub (CTX-0080).
//!
//! Bounded, headless, deterministic, `forbid(unsafe)`.
//! Proves: grant isolation per plugin+hash, queue isolation, no execution
//! before `Activated`, hash binding, safe-mode third-party isolation.
//! Uses a tiny internal `xuepoo.dogfood` / `xuepoo.dogfood2` manifest stub.

use std::collections::BTreeSet;

use bitty_plugin_host::{
    CapabilityId, DropPolicy, Event, EventKind, EventPayload, PluginHost, PluginId,
};

fn pid(s: &str) -> PluginId {
    PluginId::new(s).unwrap()
}

fn minimal_manifest(id: &str, events: Vec<&str>) -> bitty_plugin_host::PluginManifest {
    use bitty_plugin_host::{CapabilityRequests, Compat, LazyTriggers, PluginIdentity};
    bitty_plugin_host::PluginManifest {
        identity: PluginIdentity {
            id: pid(id),
            name: "Dogfood".to_string(),
            version: "0.1.0".to_string(),
            description: "dogfood isolation stub".to_string(),
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

fn manifest_with_caps(id: &str, caps: Vec<&str>) -> bitty_plugin_host::PluginManifest {
    let mut m = minimal_manifest(id, vec![]);
    for c in caps {
        m.capabilities.ids.insert(CapabilityId::parse(c).unwrap());
    }
    m
}

#[test]
fn dogfood_grant_isolation_per_plugin() {
    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    let m1 = manifest_with_caps("xuepoo.dogfood", vec!["terminal.semantic-read"]);
    let m2 = manifest_with_caps("xuepoo.dogfood2", vec!["terminal.semantic-read"]);
    for m in [m1.clone(), m2.clone()] {
        host.declare(m.clone()).unwrap();
        host.resolve(&m.identity.id).unwrap();
        host.register(&m.identity.id).unwrap();
    }
    let hash1 = m1.manifest_hash();
    let mut granted = BTreeSet::new();
    granted.insert(CapabilityId::parse("terminal.semantic-read").unwrap());
    host.insert_grant(bitty_plugin_host::GrantRecord::granted(
        pid("xuepoo.dogfood"),
        hash1.clone(),
        granted.clone(),
        1,
    ));
    // dogfood activates, dogfood2 does not (grant isolation)
    assert!(host.activate(&pid("xuepoo.dogfood")).is_ok());
    assert!(host.activate(&pid("xuepoo.dogfood2")).is_err());
}

#[test]
fn dogfood_hash_binding_isolates_version_bump() {
    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    let m = manifest_with_caps("xuepoo.dogfood", vec!["terminal.semantic-read"]);
    host.declare(m.clone()).unwrap();
    host.resolve(&pid("xuepoo.dogfood")).unwrap();
    host.register(&pid("xuepoo.dogfood")).unwrap();
    let hash_v1 = m.manifest_hash();
    let mut granted = BTreeSet::new();
    granted.insert(CapabilityId::parse("terminal.semantic-read").unwrap());
    host.insert_grant(bitty_plugin_host::GrantRecord::granted(
        pid("xuepoo.dogfood"),
        hash_v1,
        granted,
        1,
    ));
    assert!(host.activate(&pid("xuepoo.dogfood")).is_ok());
    // Bump version => hash changes, grant must not follow automatically
    let mut host2 = PluginHost::new(DropPolicy::DropOldest, 16);
    let mut m2 = manifest_with_caps("xuepoo.dogfood", vec!["terminal.semantic-read"]);
    m2.identity.version = "0.2.0".to_string();
    host2.declare(m2.clone()).unwrap();
    host2.resolve(&pid("xuepoo.dogfood")).unwrap();
    host2.register(&pid("xuepoo.dogfood")).unwrap();
    // Reuse old hash grant—should fail
    let old_hash = m.manifest_hash();
    let mut granted2 = BTreeSet::new();
    granted2.insert(CapabilityId::parse("terminal.semantic-read").unwrap());
    host2.insert_grant(bitty_plugin_host::GrantRecord::granted(
        pid("xuepoo.dogfood"),
        old_hash,
        granted2,
        1,
    ));
    assert!(host2.activate(&pid("xuepoo.dogfood")).is_err());
}

#[test]
fn dogfood_no_execution_before_activated() {
    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    let m = manifest_with_caps("xuepoo.dogfood", vec!["terminal.semantic-read"]);
    host.declare(m.clone()).unwrap();
    // Before resolve/register/activate, plugin must not be considered active
    assert_eq!(
        host.registry().get(&pid("xuepoo.dogfood")).unwrap().state,
        bitty_plugin_host::PluginState::Declared
    );
    host.resolve(&pid("xuepoo.dogfood")).unwrap();
    assert_eq!(
        host.registry().get(&pid("xuepoo.dogfood")).unwrap().state,
        bitty_plugin_host::PluginState::Resolved
    );
    host.register(&pid("xuepoo.dogfood")).unwrap();
    assert_eq!(
        host.registry().get(&pid("xuepoo.dogfood")).unwrap().state,
        bitty_plugin_host::PluginState::Registered
    );
    // Activate without grant must fail closed—no partial activation
    assert!(host.activate(&pid("xuepoo.dogfood")).is_err());
    assert_eq!(
        host.registry().get(&pid("xuepoo.dogfood")).unwrap().state,
        bitty_plugin_host::PluginState::Registered
    );
}

#[test]
fn dogfood_queue_isolation_flood_one_plugin_does_not_starve_other() {
    let mut host = PluginHost::with_capacity(DropPolicy::DropOldest, 64, 16);
    let m1 = minimal_manifest("xuepoo.dogfood", vec!["terminal.bell"]);
    let m2 = minimal_manifest("xuepoo.dogfood2", vec!["terminal.bell"]);
    for m in [&m1, &m2] {
        host.declare(m.clone()).unwrap();
        host.resolve(&m.identity.id).unwrap();
        host.register(&m.identity.id).unwrap();
        host.subscribe(&m.identity.id, EventKind::TerminalBell)
            .unwrap();
    }
    // Flood dogfood's queue via publish_to isolation (per-plugin queues)
    for i in 0..200 {
        host.publish_to(
            &pid("xuepoo.dogfood"),
            Event::new(EventKind::TerminalBell, EventPayload::Empty, i),
        )
        .unwrap();
    }
    // dogfood capped at 64, dogfood2 still empty and can receive
    assert_eq!(host.queued_events_for_plugin(&pid("xuepoo.dogfood")), 64);
    assert_eq!(host.queued_events_for_plugin(&pid("xuepoo.dogfood2")), 0);
    for i in 0..10 {
        host.publish_to(
            &pid("xuepoo.dogfood2"),
            Event::new(EventKind::TerminalBell, EventPayload::Empty, i),
        )
        .unwrap();
    }
    assert_eq!(host.queued_events_for_plugin(&pid("xuepoo.dogfood2")), 10);
    assert!(host.invariant_queue_bounds());
}

#[test]
fn dogfood_safe_mode_isolates_third_party() {
    let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
    host.set_safe_mode(true);
    let third = minimal_manifest("xuepoo.dogfood", vec![]);
    assert!(host.declare(third).is_err());
    let builtin = minimal_manifest("bitty.dogfood", vec![]);
    assert!(host.declare(builtin).is_ok());
}

#[test]
fn dogfood_side_queue_does_not_block_hot_path() {
    let mut host = PluginHost::new(DropPolicy::DropOldest, 4);
    // Side queue bounded 4, push 10—oldest dropped but len stays bounded, producer never blocks
    for i in 0..10 {
        host.push_observation(bitty_plugin_host::HostObservation::TitleChanged(format!(
            "t{i}"
        )));
    }
    assert_eq!(host.side_queue().len(), 4);
    assert_eq!(host.side_queue().dropped(), 6);
    let drained = host.drain_observations();
    assert_eq!(drained.len(), 4);
}
