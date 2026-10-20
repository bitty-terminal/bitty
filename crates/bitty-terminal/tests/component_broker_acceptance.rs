//! Headed-less acceptance evidence for #2254 (CTX-1361): one first-party
//! component serving a real request through the production broker, one
//! denied request failing closed with a named error class, and
//! `bitty extension list` reporting the recorded activation state.
//!
//! Ignored by default because it needs a `bitty-net` install produced by the
//! real CLI. When it runs without that fixture it fails instead of returning:
//! a green `--run-ignored ignored-only` run always means the flow executed.
//!
//! ```text
//! build/debug/bitty extension add <staging-dir>     # XDG_DATA_HOME=<scratch>
//! BITTY_NET_COMPONENTS_DIR=<scratch>/bitty/extensions \
//!   cargo nextest run -p bitty-terminal --test component_broker_acceptance \
//!   --run-ignored ignored-only -- --nocapture
//! ```
//!
//! `recording/ctx-1361-component-broker/` holds the exact commands and
//! output of the recorded run.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use bitty_plugin_host::manifest::NetworkEgress;
use bitty_plugin_host::{
    CapabilityId, CapabilityRequests, Compat, GrantRecord, LazyTriggers, PluginId, PluginIdentity,
    PluginManifest,
};
use bitty_runtime::Runtime;
use bitty_runtime::component::{
    ActivationState, ComponentRequest, ComponentWarnSink, Method, read_component_activation,
};
use bitty_terminal::component_host::install_with_roots;

const COMPONENT: &str = "net";
const ALLOWED: &str = "acme.acceptance-allowed";
const DENIED: &str = "acme.acceptance-denied";

struct SilentWarn;

impl ComponentWarnSink for SilentWarn {
    fn warn(&mut self, _component: &str, _reason: &str, _stderr_tail: &str) {}
}

fn manifest(id: &str, capability: Option<&str>, host: Option<(&str, u16)>) -> PluginManifest {
    let mut ids = BTreeSet::new();
    if let Some(capability) = capability {
        ids.insert(CapabilityId::parse(capability).expect("valid capability"));
    }
    PluginManifest {
        identity: PluginIdentity {
            id: PluginId::new(id).expect("valid plugin id"),
            name: "Acceptance plugin".to_string(),
            version: "0.1.0".to_string(),
            description: "component broker acceptance".to_string(),
            license: Some("MIT".to_string()),
        },
        compat: Compat {
            bitty: Some(">=0.5,<1.0".to_string()),
            plugin_api: Some("^1.0".to_string()),
        },
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: CapabilityRequests {
            ids,
            ..Default::default()
        },
        tools: Vec::new(),
        network: host
            .map(|(host, port)| {
                vec![NetworkEgress {
                    host: host.to_string(),
                    ports: vec![port],
                }]
            })
            .unwrap_or_default(),
        limits: Default::default(),
        lazy: LazyTriggers::default(),
        settings: Vec::new(),
        next_steps: Vec::new(),
        raw_bytes_len: 256,
    }
}

/// Register `manifest`, grant `capabilities`, and activate the plugin.
fn register(runtime: &mut Runtime, manifest: PluginManifest, capabilities: &[&str]) -> String {
    let hash = manifest.manifest_hash();
    let plugin_id = PluginId::new(manifest.identity.id.as_str()).expect("plugin id");
    runtime.register_plugin(manifest).expect("register plugin");
    if !capabilities.is_empty() {
        let granted: BTreeSet<CapabilityId> = capabilities
            .iter()
            .map(|capability| CapabilityId::parse(capability).expect("capability"))
            .collect();
        runtime.insert_grant(GrantRecord::granted(
            plugin_id.clone(),
            hash.clone(),
            granted,
            1,
        ));
    }
    runtime
        .activate_plugin(&plugin_id)
        .expect("activate plugin");
    hash
}

/// Run the real `bitty extension list` against the acceptance data home and
/// return its stdout (evidence for the activation-state bullet).
fn extension_list(data_home: &Path, format: Option<&str>) -> String {
    let home = data_home.join("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_bitty"));
    command
        .args(["extension", "list"])
        .env("XDG_CONFIG_HOME", data_home.join("config"))
        .env("XDG_DATA_HOME", data_home)
        .env("XDG_STATE_HOME", data_home.join("state"))
        .env("HOME", &home)
        .env_remove("BITTY_COMPONENTS_DIR")
        .env_remove("BITTY_COMPONENT_STATE_DIR")
        .env_remove("BITTY_CONFIG")
        .env("NO_COLOR", "1");
    if let Some(format) = format {
        command.args(["--format", format]);
    }
    let output = command.output().expect("run bitty extension list");
    assert!(
        output.status.success(),
        "extension list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Activation column of the `net` row in a table render.
fn activation_column(text: &str) -> Option<String> {
    text.lines()
        .find(|line| line.starts_with("net "))
        .and_then(|line| line.split_whitespace().nth(5).map(str::to_owned))
}

#[test]
#[ignore = "requires BITTY_NET_COMPONENTS_DIR pointing at an install made by `bitty extension add`"]
fn real_bitty_net_serves_and_denies_with_a_named_class() {
    // A green `--run-ignored ignored-only` run must mean the acceptance flow
    // actually executed. Without the fixture this test fails loudly instead
    // of returning early, so a missing environment can never look like a pass.
    let root = match std::env::var("BITTY_NET_COMPONENTS_DIR") {
        Ok(root) => PathBuf::from(root),
        Err(std::env::VarError::NotPresent) => {
            panic!(
                "BITTY_NET_COMPONENTS_DIR is not set: the bitty-net acceptance run needs an \
                 install made by `bitty extension add` (see the module docs); refusing to \
                 report a vacuous PASS"
            );
        }
        Err(error) => panic!("BITTY_NET_COMPONENTS_DIR is not valid unicode: {error}"),
    };
    assert!(
        root.join(COMPONENT).is_dir(),
        "{} must hold the installed `{COMPONENT}` component produced by `bitty extension add`",
        root.display()
    );
    // <data>/bitty/extensions -> <data>
    let data_home = root
        .parent()
        .and_then(Path::parent)
        .expect("data home")
        .to_path_buf();

    // Installed through the real `bitty extension add`, never requested yet.
    assert_eq!(
        read_component_activation(Some(&root), None, COMPONENT),
        ActivationState::Installed,
        "fresh install must not claim an activation"
    );
    eprintln!(
        "$ bitty extension list\n{}",
        extension_list(&data_home, None)
    );

    let mut runtime =
        Runtime::with_deterministic_rasterizer(bitty_runtime::RuntimeConfig::default())
            .expect("runtime");
    assert!(install_with_roots(
        &mut runtime,
        Some(root.clone()),
        None,
        SilentWarn
    ));

    // Denied request: the plugin declares no capability, so the grant is
    // offline and the request fails closed before any spawn.
    let denied_hash = register(&mut runtime, manifest(DENIED, None, None), &[]);
    let denied = runtime
        .component_request(
            DENIED,
            &denied_hash,
            COMPONENT,
            ComponentRequest::new(Method::Get, "http://127.0.0.1:9/denied"),
        )
        .expect_err("a plugin without network authority must be denied");
    assert_eq!(denied.class(), "capability_denied");
    assert!(
        runtime.component_status(COMPONENT).is_none(),
        "a denied request must not spawn the component"
    );
    assert_eq!(
        read_component_activation(Some(&root), None, COMPONENT),
        ActivationState::Denied(bitty_runtime::component::ActivationDenial::CapabilityDenied)
    );
    eprintln!(
        "denied request: class={} message={denied}\n$ bitty extension list\n{}",
        denied.class(),
        extension_list(&data_home, None)
    );

    // Real request: a local HTTP server answers once through bitty-net.
    let listener = TcpListener::bind("127.0.0.1:0").expect("local listener");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
            .expect("write response");
    });
    let capability = format!("network.connect:127.0.0.1:{port}");
    let allowed_hash = register(
        &mut runtime,
        manifest(ALLOWED, Some(&capability), Some(("127.0.0.1", port))),
        &[&capability],
    );
    let started = Instant::now();
    let response = runtime
        .component_request(
            ALLOWED,
            &allowed_hash,
            COMPONENT,
            ComponentRequest::new(Method::Get, format!("http://127.0.0.1:{port}/ping")),
        )
        .expect("real bitty-net response");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"hello");
    server.join().expect("server thread");
    let status = runtime.component_status(COMPONENT).expect("status");
    assert!(status.pid.is_some(), "component must be running");
    assert_eq!(
        read_component_activation(Some(&root), None, COMPONENT),
        ActivationState::Active
    );
    eprintln!(
        "served request: status={} body={:?} elapsed_ms={} pid={:?}\n$ bitty extension list\n{}",
        response.status,
        String::from_utf8_lossy(&response.body),
        started.elapsed().as_millis(),
        status.pid,
        extension_list(&data_home, None)
    );
    eprintln!(
        "$ bitty extension list --format json\n{}",
        extension_list(&data_home, Some("json"))
    );
    assert_eq!(
        activation_column(&extension_list(&data_home, None)).as_deref(),
        Some("active"),
        "list must report the live activation"
    );

    // Graceful shutdown of the recorded child, then the list reports the
    // stop (a stopped component is still not `active`).
    let _ = runtime.component_host_mut().expect("host").shutdown();
    assert_eq!(
        read_component_activation(Some(&root), None, COMPONENT),
        ActivationState::Stopped
    );
    eprintln!(
        "after shutdown: record={:?}\n$ bitty extension list\n{}",
        read_component_activation(Some(&root), None, COMPONENT),
        extension_list(&data_home, None)
    );
    assert_eq!(
        activation_column(&extension_list(&data_home, None)).as_deref(),
        Some("stopped")
    );
}
