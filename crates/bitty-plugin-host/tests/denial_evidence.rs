//! Plugin denial cells (bitty#2921, CTX-1865).
//!
//! Two cells the security corpus records as unproven, at the layer that owns
//! the refusal:
//!
//! 1. **network**: a plugin that declares/requests `network.connect` cannot
//!    gain it. The six-layer intersection
//!    ([`bitty_plugin_host::authorize`]) refuses with a typed
//!    [`EffectiveDenial`] (`DenialKind::ExceedsPolicy`) naming the capability
//!    and the layer that withheld it; a positive control proves the refusal is
//!    not vacuous. The plugin-facing end of the same refusal is the
//!    `capability_denied` host class
//!    (`crates/bitty-runtime/tests/component_host.rs:232` and `:705`), and the
//!    manifest pairing rule is
//!    `crates/bitty-plugin-host/src/manifest.rs:4483`; neither is re-tested
//!    here.
//! 2. **filesystem escape**: a request that leaves its granted roots — parent
//!    traversal, or an absolute `/etc/...` target — is refused with a typed
//!    [`FsDenialKind`], never silently clamped onto an in-scope path.
//!    [`bitty_plugin_host::authorize_fs`] already covers the hostile-shape
//!    corpus (`src/fs_authz.rs:1638` `hostile_grant_shapes_deny_before_scope`,
//!    `:1699` `absolute_paths_never_covered_even_by_broad_scope`), and the
//!    runtime listing gate covers `~/docs/../outside` and `/etc`
//!    (`crates/bitty-runtime/src/plugin_runtime/fs_list.rs:817` and `:823`).
//!    This cell adds the `/etc/shadow`-shaped absolute target the criterion
//!    names and asserts the "refused, not clamped" property at both the
//!    authorizer and the matcher.
//!
//! # What this target does not claim
//!
//! It is not an acceptance record and it does not claim an `AC-0xx` id: the
//! daemon corpus maps `P0-AC-024` to the MCP/Agent read-only default
//! (`crates/bitty-daemon/tests/agent_readonly.rs:1`), so the criterion these
//! cells roll up into is the security reviewer's call. The live daemon campaign
//! also stays open on bitty#2921. Run it with
//! `cargo test -p bitty-plugin-host --test denial_evidence --offline -- --nocapture`.

use bitty_plugin_host::{
    AgentRequest, CapabilityId, CapabilityScope, DenialKind, EffectiveStack, FilesystemScope,
    FsDecision, FsDenialKind, RequestKind, SensitivePathPolicy, authorize, authorize_fs,
};

/// One layer's voice holding exactly `caps`.
fn layer(source: &str, caps: &[&str]) -> CapabilityScope {
    let mut scope = CapabilityScope::unconstrained(source);
    for raw in caps {
        scope
            .caps
            .insert(CapabilityId::parse(raw).unwrap_or_else(|_| panic!("capability {raw}")));
    }
    scope
}

fn network_cap() -> &'static str {
    "network.connect:example.com:443"
}

#[test]
fn plugin_cannot_gain_network_capability_through_the_effective_set() {
    // Every layer honours a read capability; none of them carries the network
    // destination. A plugin request for it is excess, not a widening: the
    // intersection is what may be exercised, and the request is refused.
    let read_only = ["terminal.semantic-read"];
    let stack = EffectiveStack {
        host: layer("host-ceiling", &read_only),
        user: layer("user-policy", &read_only),
        project: Some(layer("project-policy", &read_only)),
        parent: layer("parent-delegation", &read_only),
        task: layer("task-grant", &read_only),
    };
    let request = AgentRequest {
        scope: layer("plugin-manifest", &[read_only[0], network_cap()]),
        raw_wide: Vec::new(),
    };

    // Kind fit: `network.connect` under the execution kind is a network
    // destination request, which is the enforcement point the criterion names.
    let denial = authorize(&stack, &request, RequestKind::ExecutionRun, true)
        .expect_err("a plugin must not gain a destination no layer grants");
    assert_eq!(denial.kind, DenialKind::ExceedsPolicy);
    assert_eq!(denial.request_kind, RequestKind::ExecutionRun);
    let chain = denial.reason_chain().join("\n");
    assert!(
        chain.contains(network_cap()),
        "the refusal must name the capability it withheld: {chain}"
    );
    assert!(
        chain.contains("host-ceiling") || chain.contains("user-policy"),
        "the refusal must name the layer that withheld it: {chain}"
    );
    println!("DENY network gain refused: {}", denial.kind);
    println!("DENY reason chain:\n{chain}");

    // The dedicated `NetworkConnect` kind refuses the same request, so the
    // denial is not an artefact of the request kind chosen above.
    let denial = authorize(&stack, &request, RequestKind::NetworkConnect, true)
        .expect_err("the network kind must refuse a destination no layer grants");
    assert_eq!(denial.kind, DenialKind::ExceedsPolicy);
    println!(
        "DENY network.connect kind refused: {} ({})",
        denial.kind, denial.request_kind
    );

    // Control: with the destination granted by every layer, the same request
    // authorizes with exactly that capability, so the refusals above are not
    // vacuous.
    let granted = [read_only[0], network_cap()];
    let stack = EffectiveStack {
        host: layer("host-ceiling", &granted),
        user: layer("user-policy", &granted),
        project: Some(layer("project-policy", &granted)),
        parent: layer("parent-delegation", &granted),
        task: layer("task-grant", &granted),
    };
    let effective = authorize(&stack, &request, RequestKind::NetworkConnect, true)
        .expect("a fully granted destination must authorize");
    assert_eq!(
        effective.caps.len(),
        2,
        "the effective set is exactly what every layer granted: {:?}",
        effective.caps
    );
    println!(
        "DENY control: fully granted destination authorizes {} capabilities",
        effective.caps.len()
    );

    // And the plugin manifest's own voice never becomes authority: a request
    // whose every layer is silent is denied by default even when the plugin
    // declares the capability itself.
    let silent = EffectiveStack {
        host: layer("host-ceiling", &read_only),
        user: CapabilityScope::unconstrained("user-policy"),
        project: None,
        parent: CapabilityScope::unconstrained("parent-delegation"),
        task: CapabilityScope::unconstrained("task-grant"),
    };
    let denial = authorize(&silent, &request, RequestKind::NetworkConnect, true)
        .expect_err("silent layers never confer a destination");
    assert_eq!(denial.kind, DenialKind::ExceedsPolicy);
    println!(
        "DENY denied by default across silent layers: {}",
        denial.kind
    );
}

/// Every escape shape this cell asserts, paired with the class it must land in.
const ESCAPES: [(&str, &str); 4] = [
    ("../secret", "parent traversal"),
    (
        "~/projects/../../etc/shadow",
        "traversal out of a granted root",
    ),
    ("/etc/shadow", "absolute credential target"),
    (
        "~/projects/../../../root/.ssh/id_ed25519",
        "traversal onto a credential path",
    ),
];

#[test]
fn plugin_fs_request_cannot_escape_its_granted_roots() {
    let scope = FilesystemScope::from_patterns(&["~/projects/**".to_string()])
        .expect("a home-rooted grant validates");
    let policy = SensitivePathPolicy::default_policy();

    for (request, label) in ESCAPES {
        let evaluated = authorize_fs(&scope, &policy, request, false, None, 0);
        assert!(
            !evaluated.is_authorized(),
            "{label} ({request}) must not authorize"
        );
        match evaluated.decision {
            FsDecision::Deny { kind, .. } => {
                assert!(
                    matches!(
                        kind,
                        FsDenialKind::HostilePattern | FsDenialKind::InvalidRequest
                    ),
                    "{label} ({request}) must deny as hostile/invalid, got {kind:?}"
                );
                println!("DENY fs escape {request:?} denied: {}", kind.as_str());
            }
            // Consent must never be offered for an escape: consent names a
            // concrete sensitive path the user picks, never a traversal.
            other => panic!("{label} ({request}) must be a typed Deny, got {other:?}"),
        }

        // Not silently clamped: the matcher itself refuses the request shape,
        // so a caller that skips `authorize_fs` gains nothing, and no
        // normalized in-scope spelling is produced.
        assert!(
            !scope.allows(request),
            "{label} ({request}) must not be covered by the grant"
        );
    }

    // Control: an in-scope path authorizes, so the refusals above are a
    // boundary and not a blanket refusal.
    let allowed = authorize_fs(&scope, &policy, "~/projects/notes.md", false, None, 0);
    assert!(
        allowed.is_authorized(),
        "an in-scope path must authorize: {:?}",
        allowed.decision
    );

    // A sibling that merely shares a string prefix is outside the grant too:
    // containment is component-wise, not textual.
    let sibling = authorize_fs(&scope, &policy, "~/projects2/notes.md", false, None, 0);
    assert!(
        !sibling.is_authorized(),
        "a sibling prefix must not be covered: {:?}",
        sibling.decision
    );
    assert!(
        matches!(
            sibling.decision,
            FsDecision::Deny {
                kind: FsDenialKind::OutsideScope,
                ..
            }
        ),
        "a sibling prefix is an out-of-scope deny: {:?}",
        sibling.decision
    );
    println!("DENY fs control: in-scope allows, sibling prefix denied OutsideScope");
}
