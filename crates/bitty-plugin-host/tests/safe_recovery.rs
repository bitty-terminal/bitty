#![forbid(unsafe_code)]

//! Safe-recovery hostile-fixture matrix — CTX-0627 (SEC-06, issue #1075).
//!
//! Risk `R-009` (user cannot recover from a broken or hostile
//! plugin/configuration) with acceptance `P0-AC-019` (safe mode always
//! available) and `P0-AC-020` (targeted plugin disable). Source: the
//! risk register plus the accepted Default Distribution RFC (five disable
//! surfaces with safe-mode precedence and generation disposal).
//!
//! Host-level mapping of the five disable surfaces (the config, managed
//! manifest, CLI, and profile layers live outside this crate; this matrix
//! pins the host primitives those layers drive):
//!
//! | Disable surface (RFC)              | Host primitive under test             |
//! |------------------------------------|---------------------------------------|
//! | typed config `plugins.<id>.enabled` | [`PluginHost::suspend`] (deactivate, identity + grants kept) |
//! | managed manifest `enabled` flag     | [`PluginHost::dispose`] (generation disposal, identity kept) |
//! | CLI `bitty plugin disable <id>`     | [`PluginHost::remove`] (identity purge by id) |
//! | profile-scoped override             | [`GrantStore::revoke_all`] (grant denial, fail-closed activation) |
//! | `bitty --safe`                      | [`PluginHost::set_safe_mode`] (unconditional third-party skip) |
//!
//! Headless and deterministic: no I/O, no VM, no wall clock. Every test
//! asserts the surgical effect (exactly the target is disabled), data
//! preservation (siblings and unrelated grants survive), and a reversible
//! or sticky outcome matching the surface contract.

use std::collections::BTreeSet;

use bitty_plugin_host::{
    BudgetDimension, CapabilityId, CapabilityRequests, Clock, Compat, DropPolicy,
    EnforcementAction, FilesystemRequest, FsAccess, GrantRecord, LazyTriggers, LifecycleEnforcer,
    ManualClock, PluginHost, PluginId, PluginIdentity, PluginManifest, PluginState, QualifiedName,
    ToolDeclaration,
};

const BUILTIN: &str = "bitty.core";
const TARGET: &str = "xuepoo.target";
const ALPHA: &str = "xuepoo.alpha";
const BETA: &str = "xuepoo.beta";

const CAP_TARGET: &str = "terminal.semantic-read";
const CAP_ALPHA: &str = "clipboard.read";
const CAP_BETA: &str = "clipboard.write";

fn minimal(id: &str) -> PluginManifest {
    PluginManifest {
        identity: PluginIdentity {
            id: PluginId::new(id).expect("fixture id must parse"),
            name: "Fixture".to_string(),
            version: "0.1.0".to_string(),
            description: "safe-recovery fixture".to_string(),
            license: Some("MIT".to_string()),
        },
        compat: Compat {
            bitty: Some(">=0.5,<1.0".to_string()),
            plugin_api: Some("^1.0".to_string()),
        },
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: CapabilityRequests::default(),
        tools: Vec::new(),
        network: Vec::new(),
        limits: Default::default(),
        lazy: LazyTriggers {
            commands: Vec::new(),
            events: Vec::new(),
            claims: Vec::new(),
        },
        raw_bytes_len: 256,
    }
}

fn with_cap(id: &str, cap: &str) -> PluginManifest {
    let mut manifest = minimal(id);
    manifest
        .capabilities
        .ids
        .insert(CapabilityId::parse(cap).expect("fixture capability must parse"));
    manifest
}

fn plugin_id(id: &str) -> PluginId {
    PluginId::new(id).expect("fixture id must parse")
}

fn grant_for(manifest: &PluginManifest, cap: &str) -> GrantRecord {
    let mut granted = BTreeSet::new();
    granted.insert(CapabilityId::parse(cap).expect("fixture capability must parse"));
    GrantRecord::granted(
        manifest.identity.id.clone(),
        manifest.manifest_hash(),
        granted,
        1,
    )
}

/// Drive a manifest to `Activated`, wiring its grant record first.
fn drive_to_activated(host: &mut PluginHost, manifest: &PluginManifest, cap: Option<&str>) {
    let id = manifest.identity.id.clone();
    host.declare(manifest.clone())
        .expect("fixture must declare");
    host.resolve(&id).expect("fixture must resolve");
    host.register(&id).expect("fixture must register");
    if let Some(capability) = cap {
        host.grants_mut().insert(grant_for(manifest, capability));
    }
    host.activate(&id).expect("fixture must activate");
    assert_eq!(
        host.registry().get(&id).expect("entry").state,
        PluginState::Activated
    );
}

fn safe_mode_error_text(host: &mut PluginHost, manifest: PluginManifest) -> String {
    let err = host
        .declare(manifest)
        .expect_err("safe mode must reject third-party declare");
    err.to_string()
}

fn assert_no_partial_entry(host: &PluginHost, id: &PluginId) {
    assert!(
        host.registry().get(id).is_none(),
        "rejected declare must leave no partial entry for '{id}'"
    );
}

// ── Hostile fixtures ────────────────────────────────────────────────────
// Class V: valid manifests with hostile behavior (declare succeeds in
// normal mode, so safe mode is the layer under test). Class I: hostile
// input that fails validation in every mode (recovery means the host
// stays usable and deterministic).

/// Valid-but-hostile third-party fixtures, one per hostile dimension.
fn hostile_valid_fixtures() -> Vec<(&'static str, PluginManifest)> {
    let mut fixtures = Vec::new();

    // Command-table saturation at the hard bound.
    let mut saturated = minimal("xuepoo.cmds");
    saturated.lazy.commands = (0..128)
        .map(|i| {
            QualifiedName::new(&format!("xuepoo.cmds:cmd{i}")).expect("qualified name must parse")
        })
        .collect();
    fixtures.push(("command saturation", saturated));

    // Event-table saturation at the hard bound.
    let mut events = minimal("xuepoo.events");
    events.lazy.events = (0..256).map(|i| format!("custom.event-{i}")).collect();
    fixtures.push(("event saturation", events));

    // Dependency cycle: each half declares first.
    let mut cycle_a = minimal("xuepoo.cyclea");
    cycle_a.dependencies = vec![(plugin_id("xuepoo.cycleb"), "^1.0".to_string())];
    fixtures.push(("dependency cycle A", cycle_a));
    let mut cycle_b = minimal("xuepoo.cycleb");
    cycle_b.dependencies = vec![(plugin_id("xuepoo.cyclea"), "^1.0".to_string())];
    fixtures.push(("dependency cycle B", cycle_b));

    // Missing dependency: resolves nowhere.
    let mut missing = minimal("xuepoo.missing");
    missing.dependencies = vec![(plugin_id("xuepoo.ghost"), "^1.0".to_string())];
    fixtures.push(("missing dependency", missing));

    // Capability overreach: high-risk input observation plus raw read.
    let mut overreach = minimal("xuepoo.overreach");
    for cap in ["terminal.input.all", "terminal.raw-read", "terminal.manage"] {
        overreach
            .capabilities
            .ids
            .insert(CapabilityId::parse(cap).expect("capability must parse"));
    }
    fixtures.push(("capability overreach", overreach));

    // Filesystem requests at the per-kind bound (benign relative globs).
    let mut fs_heavy = minimal("xuepoo.fsheavy");
    fs_heavy.capabilities.filesystem = vec![FilesystemRequest {
        access: FsAccess::Read,
        paths: (0..32).map(|i| format!("notes/proj-{i}/*.md")).collect(),
    }];
    fixtures.push(("filesystem bound", fs_heavy));

    fixtures
}

/// Hostile input that fails manifest validation in every mode.
fn hostile_invalid_fixtures() -> Vec<(&'static str, PluginManifest)> {
    let mut fixtures = Vec::new();

    // Absolute path escape.
    let mut absolute = minimal("xuepoo.absolute");
    absolute.capabilities.filesystem = vec![FilesystemRequest {
        access: FsAccess::Read,
        paths: vec!["/etc/passwd".to_string()],
    }];
    fixtures.push(("absolute path", absolute));

    // Traversal escape.
    let mut traversal = minimal("xuepoo.traversal");
    traversal.capabilities.filesystem = vec![FilesystemRequest {
        access: FsAccess::Write,
        paths: vec!["notes/../../escape".to_string()],
    }];
    fixtures.push(("traversal path", traversal));

    // Credential-directory pattern.
    let mut creds = minimal("xuepoo.creds");
    creds.capabilities.filesystem = vec![FilesystemRequest {
        access: FsAccess::Read,
        paths: vec!["~/.ssh/id_rsa".to_string()],
    }];
    fixtures.push(("credential path", creds));

    // Command table one past the hard bound.
    let mut over_commands = minimal("xuepoo.overcmds");
    over_commands.lazy.commands = (0..129)
        .map(|i| QualifiedName::new(&format!("xuepoo.overcmds:cmd{i}")).expect("name must parse"))
        .collect();
    fixtures.push(("oversized commands", over_commands));

    // Unaccepted Layer-2 tool.
    let mut tool = minimal("xuepoo.tool");
    tool.tools = vec![ToolDeclaration {
        tool: "curl".to_string(),
        required: true,
        version_req: ">=7.0".to_string(),
    }];
    fixtures.push(("unaccepted tool", tool));

    // Manifest size past the 256 KiB bound.
    let mut oversized = minimal("xuepoo.oversized");
    oversized.raw_bytes_len = 256 * 1024 + 1;
    fixtures.push(("oversized bytes", oversized));

    // NUL byte in an event name.
    let mut nul = minimal("xuepoo.nulevent");
    nul.lazy.events = vec!["bad\0event".to_string()];
    fixtures.push(("NUL event", nul));

    fixtures
}

fn fresh_safe_host() -> PluginHost {
    let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
    host.set_safe_mode(true);
    host
}

// ── P0-AC-019: safe mode always available ───────────────────────────────

#[test]
fn safe_mode_rejects_every_hostile_fixture_at_declare() {
    for (label, manifest) in hostile_valid_fixtures() {
        let id = manifest.identity.id.clone();
        // Negative control: the fixture is valid, so normal mode declares it.
        let mut normal = PluginHost::new(DropPolicy::DropOldest, 8);
        normal
            .declare(manifest.clone())
            .unwrap_or_else(|err| panic!("class-V fixture '{label}' must declare normally: {err}"));

        let mut safe = fresh_safe_host();
        let text = safe_mode_error_text(&mut safe, manifest);
        assert!(
            text.contains("safe mode"),
            "fixture '{label}' must fail with a safe-mode error, got: {text}"
        );
        assert_no_partial_entry(&safe, &id);
    }
}

#[test]
fn safe_mode_rejects_invalid_hostile_input_without_partial_state() {
    for (label, manifest) in hostile_invalid_fixtures() {
        let id = manifest.identity.id.clone();
        let mut safe = fresh_safe_host();
        let first = safe_mode_error_text(&mut safe, manifest.clone());
        // Deterministic: the same fixture fails identically on retry.
        let mut retry = fresh_safe_host();
        let second = safe_mode_error_text(&mut retry, manifest);
        assert_eq!(
            first, second,
            "fixture '{label}' must fail deterministically"
        );
        assert_no_partial_entry(&safe, &id);
        // Recovery: the host still starts the minimal builtin path afterwards.
        safe.declare(minimal(BUILTIN))
            .unwrap_or_else(|err| panic!("host must stay usable after '{label}': {err}"));
    }
}

#[test]
fn safe_startup_minimal_builtin_path_succeeds() {
    let mut host = fresh_safe_host();
    // Hostile third-party plugins are skipped while the builtin path works,
    // including an authority-bearing builtin with a bound grant.
    let builtin = with_cap(BUILTIN, CAP_TARGET);
    assert!(
        host.declare(minimal("xuepoo.noise")).is_err(),
        "safe mode must skip third-party plugins"
    );
    drive_to_activated(&mut host, &builtin, Some(CAP_TARGET));
    assert!(host.is_safe_mode());
}

#[test]
fn safe_mode_is_transient_and_preserves_prior_state() {
    let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
    // Pre-existing third-party generation with bound grants.
    let prior = with_cap("xuepoo.prior", CAP_ALPHA);
    drive_to_activated(&mut host, &prior, Some(CAP_ALPHA));
    let grants_before = host.grants().len();

    // Entering safe mode gates new declarations but purges nothing.
    host.set_safe_mode(true);
    assert!(host.declare(minimal("xuepoo.late")).is_err());
    let id = plugin_id("xuepoo.prior");
    assert_eq!(
        host.registry().get(&id).expect("prior entry").state,
        PluginState::Activated,
        "safe mode must not disturb the prior generation"
    );
    assert_eq!(host.grants().len(), grants_before);
    assert!(host.grants().get(&id).is_some());

    // Leaving safe mode restores the declare path with state intact.
    host.set_safe_mode(false);
    host.declare(minimal("xuepoo.late"))
        .expect("declare must work after safe mode");
    assert_eq!(
        host.registry().get(&id).expect("prior entry").state,
        PluginState::Activated
    );
}

#[test]
fn safe_mode_reload_rejects_hostile_replacement_preserving_generation() {
    let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
    let current = with_cap(TARGET, CAP_TARGET);
    drive_to_activated(&mut host, &current, Some(CAP_TARGET));

    host.set_safe_mode(true);
    let mut hostile = with_cap(TARGET, "terminal.input.all");
    hostile.lazy.events = vec!["custom.spam".to_string()];
    let err = host
        .reload(&plugin_id(TARGET), hostile)
        .expect_err("safe reload of third-party must fail");
    assert!(
        err.to_string().contains("safe mode"),
        "reload must fail with a safe-mode error, got: {err}"
    );
    // Old generation preserved with grants intact (fail-closed reload).
    let id = plugin_id(TARGET);
    assert_eq!(
        host.registry().get(&id).expect("entry").state,
        PluginState::Activated
    );
    assert!(host.grants().get(&id).is_some());
    host.set_safe_mode(false);
    host.suspend(&id).expect("old generation still manageable");
}

#[test]
fn safe_startup_matrix_is_deterministic() {
    fn run_matrix() -> Vec<String> {
        let mut outcomes = Vec::new();
        for (label, manifest) in hostile_valid_fixtures()
            .into_iter()
            .chain(hostile_invalid_fixtures())
        {
            let mut host = fresh_safe_host();
            let outcome = match host.declare(manifest) {
                Ok(()) => format!("{label}: ok"),
                Err(err) => format!("{label}: {err}"),
            };
            outcomes.push(outcome);
        }
        let mut builtin_host = fresh_safe_host();
        builtin_host
            .declare(minimal(BUILTIN))
            .expect("builtin must declare");
        outcomes.push("builtin: ok".to_string());
        outcomes
    }
    assert_eq!(run_matrix(), run_matrix());
}

// ── P0-AC-020: targeted plugin disable ──────────────────────────────────
// Three-plugin rig: the target plus two siblings, each with a distinct
// capability and bound grant, all `Activated`. Every surface test asserts
// the target is disabled, the siblings keep working, and unrelated user
// data (sibling grants and entries) is preserved.

struct Rig {
    host: PluginHost,
    target: PluginManifest,
    alpha: PluginManifest,
    #[allow(dead_code)]
    beta: PluginManifest,
}

fn activated_rig() -> Rig {
    let mut host = PluginHost::new(DropPolicy::DropOldest, 8);
    let target = with_cap(TARGET, CAP_TARGET);
    let alpha = with_cap(ALPHA, CAP_ALPHA);
    let beta = with_cap(BETA, CAP_BETA);
    drive_to_activated(&mut host, &target, Some(CAP_TARGET));
    drive_to_activated(&mut host, &alpha, Some(CAP_ALPHA));
    drive_to_activated(&mut host, &beta, Some(CAP_BETA));
    Rig {
        host,
        target,
        alpha,
        beta,
    }
}

fn assert_siblings_active(host: &PluginHost) {
    for id in [ALPHA, BETA] {
        let entry = host
            .registry()
            .get(&plugin_id(id))
            .unwrap_or_else(|| panic!("sibling '{id}' entry must survive"));
        assert_eq!(
            entry.state,
            PluginState::Activated,
            "sibling '{id}' stays active"
        );
        assert!(
            host.grants().get(&plugin_id(id)).is_some(),
            "sibling '{id}' grant must survive"
        );
    }
}

#[test]
fn disable_via_config_surface_suspend_is_surgical_and_reversible() {
    // Typed config `plugins.<id>.enabled=false` maps to suspend: handlers
    // detach while identity, grants, and stored state are retained.
    let mut rig = activated_rig();
    rig.host
        .suspend(&plugin_id(TARGET))
        .expect("suspend must succeed");
    assert_eq!(
        rig.host
            .registry()
            .get(&plugin_id(TARGET))
            .expect("entry")
            .state,
        PluginState::Suspended
    );
    assert_siblings_active(&rig.host);
    assert!(rig.host.grants().get(&plugin_id(TARGET)).is_some());

    // Reversible: re-enable returns through resume + activate with the same grant.
    rig.host
        .resume(&plugin_id(TARGET))
        .expect("resume must succeed");
    rig.host
        .activate(&plugin_id(TARGET))
        .expect("re-activate must succeed");
    assert_eq!(
        rig.host
            .registry()
            .get(&plugin_id(TARGET))
            .expect("entry")
            .state,
        PluginState::Activated
    );
    assert_siblings_active(&rig.host);
}

#[test]
fn disable_via_managed_manifest_surface_dispose_is_surgical() {
    // Managed manifest `enabled=false` maps to generation disposal: the
    // generation releases resources while the identity is retained.
    let mut rig = activated_rig();
    rig.host
        .dispose(&plugin_id(TARGET))
        .expect("dispose must succeed");
    assert_eq!(
        rig.host
            .registry()
            .get(&plugin_id(TARGET))
            .expect("entry")
            .state,
        PluginState::Disposed
    );
    assert_siblings_active(&rig.host);
    // Grants are retained until explicit clear (capability orthogonality).
    assert!(rig.host.grants().get(&plugin_id(TARGET)).is_some());

    // Re-enable path: purge the disposed identity and declare fresh; the
    // retained grant still binds the unchanged manifest hash.
    rig.host
        .remove(&plugin_id(TARGET))
        .expect("remove must succeed");
    rig.host
        .declare(rig.target.clone())
        .expect("re-declare must succeed");
    rig.host
        .resolve(&plugin_id(TARGET))
        .expect("resolve must succeed");
    rig.host
        .register(&plugin_id(TARGET))
        .expect("register must succeed");
    rig.host
        .activate(&plugin_id(TARGET))
        .expect("grant still binds");
}

#[test]
fn disable_via_cli_surface_remove_purges_only_target() {
    // CLI `bitty plugin disable <id>` maps to remove: the identity is
    // purged by id so the same id can be declared again on re-enable.
    let mut rig = activated_rig();
    rig.host
        .remove(&plugin_id(TARGET))
        .expect("remove must succeed");
    assert!(rig.host.registry().get(&plugin_id(TARGET)).is_none());
    assert_siblings_active(&rig.host);

    // The disabled id no longer resolves or activates (sticky disable).
    assert!(rig.host.resolve(&plugin_id(TARGET)).is_err());
    assert!(rig.host.activate(&plugin_id(TARGET)).is_err());

    // Re-enable declares the same id fresh without touching siblings.
    rig.host
        .declare(rig.target.clone())
        .expect("re-declare must succeed");
    assert_siblings_active(&rig.host);
}

#[test]
fn disable_via_profile_surface_revoke_blocks_reactivation_until_regrant() {
    // Profile-scoped override maps to grant revocation: activation fails
    // closed with a denial marker until explicit re-grant.
    let mut rig = activated_rig();
    let report = rig
        .host
        .grants_mut()
        .revoke_all(&plugin_id(TARGET))
        .expect("revoke must succeed");
    assert!(report.fully_revoked);

    // A fresh lifecycle for the same manifest still cannot activate: the
    // denial marker persists across re-declare (no silent re-prompt loop).
    rig.host
        .remove(&plugin_id(TARGET))
        .expect("remove must succeed");
    rig.host
        .declare(rig.target.clone())
        .expect("re-declare must succeed");
    rig.host
        .resolve(&plugin_id(TARGET))
        .expect("resolve must succeed");
    rig.host
        .register(&plugin_id(TARGET))
        .expect("register must succeed");
    let err = rig
        .host
        .activate(&plugin_id(TARGET))
        .expect_err("activation must fail closed after revoke");
    // Fail-closed either way, and the denial marker itself survives the
    // re-declare so the target cannot silently re-prompt its grant back.
    assert!(
        err.to_string().starts_with("grant:"),
        "activation must fail at the grant gate, got: {err}"
    );
    assert!(
        rig.host.grants().is_denied(&plugin_id(TARGET)),
        "denial marker must persist across re-declare"
    );
    assert_siblings_active(&rig.host);

    // Explicit re-grant (user action) restores activation; siblings untouched.
    rig.host
        .grants_mut()
        .insert(grant_for(&rig.target, CAP_TARGET));
    rig.host
        .activate(&plugin_id(TARGET))
        .expect("re-grant must restore activation");
    assert_siblings_active(&rig.host);
}

#[test]
fn disable_via_safe_surface_skips_unconditionally_with_precedence() {
    // `bitty --safe` skips unconditionally: even a fully granted target
    // cannot be (re-)declared while safe mode holds, and safe mode never
    // rewrites the stored enable/disable state.
    let mut rig = activated_rig();
    rig.host.set_safe_mode(true);
    let err = rig
        .host
        .declare(rig.alpha.clone())
        .expect_err("safe mode must skip even granted plugins");
    assert!(err.to_string().contains("safe mode"));
    // Stored state untouched: entries and grants are exactly as before.
    assert_siblings_active(&rig.host);
    assert!(rig.host.grants().get(&plugin_id(TARGET)).is_some());

    rig.host.set_safe_mode(false);
    assert!(!rig.host.is_safe_mode());
    assert_siblings_active(&rig.host);
}

#[test]
fn targeted_disable_survives_restart_simulation() {
    // Persistence across restarts at host level: after CLI-disable plus
    // profile-revoke, a fresh host rebuilt from the same manifests plus
    // the retained (non-revoked) records keeps the target disabled while
    // siblings reactivate. Revoked records are never carried forward.
    let mut rig = activated_rig();
    rig.host
        .remove(&plugin_id(TARGET))
        .expect("remove must succeed");
    rig.host
        .grants_mut()
        .revoke_all(&plugin_id(TARGET))
        .expect("revoke must succeed");

    // Simulate restart: fresh host, re-declare all three, re-insert only
    // the grants that were not revoked (sibling user data preserved).
    let mut restarted = PluginHost::new(DropPolicy::DropOldest, 8);
    for manifest in [&rig.target, &rig.alpha, &rig.beta] {
        restarted
            .declare(manifest.clone())
            .expect("re-declare must succeed");
    }
    restarted
        .grants_mut()
        .insert(grant_for(&rig.alpha, CAP_ALPHA));
    restarted
        .grants_mut()
        .insert(grant_for(&rig.beta, CAP_BETA));
    for id in [&rig.target, &rig.alpha, &rig.beta] {
        restarted
            .resolve(&id.identity.id)
            .expect("resolve must succeed");
        restarted
            .register(&id.identity.id)
            .expect("register must succeed");
    }
    assert!(
        restarted.activate(&plugin_id(TARGET)).is_err(),
        "target stays disabled: no grant was carried forward"
    );
    restarted
        .activate(&plugin_id(ALPHA))
        .expect("sibling reactivates");
    restarted
        .activate(&plugin_id(BETA))
        .expect("sibling reactivates");
    assert_eq!(
        restarted
            .registry()
            .get(&plugin_id(ALPHA))
            .expect("entry")
            .state,
        PluginState::Activated
    );
}

#[test]
fn lifecycle_auto_disable_contains_faulty_generation() {
    // Containment surface: repeated budget violations escalate to
    // `DisablePlugin`; the host maps that to removal of exactly the
    // faulty generation while siblings keep running.
    let mut rig = activated_rig();
    let clock = ManualClock::new(1_000);
    let mut enforcer = LifecycleEnforcer::new();
    let mut action = EnforcementAction::Refuse;
    for _ in 0..4 {
        action = enforcer
            .report_violation(
                TARGET,
                1,
                BudgetDimension::Instructions,
                12_000_000,
                10_000_000,
                clock.now_secs(),
            )
            .expect("owner table has room")
            .action;
        clock.advance(5);
    }
    assert_eq!(action, EnforcementAction::DisablePlugin);
    rig.host
        .remove(&plugin_id(TARGET))
        .expect("faulty generation removed");
    assert!(rig.host.registry().get(&plugin_id(TARGET)).is_none());
    assert_siblings_active(&rig.host);
}
