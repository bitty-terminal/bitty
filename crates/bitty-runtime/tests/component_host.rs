//! CTX-1361 (bitty#2254): installed native components through the production
//! host.
//!
//! The fixture is staged by the real `bitty extension add` implementation
//! (`bitty_terminal::component::run_component_subcommand`), then requested
//! through [`Runtime::component_request`] with the capability model the
//! production composition root uses. No test writes the install layout by
//! hand.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bitty_network_wire::{ErrorKind, Method};
use bitty_plugin_host::manifest::NetworkEgress;
use bitty_plugin_host::{
    CapabilityId, CapabilityRequests, Compat, GrantRecord, LazyTriggers, PluginId, PluginIdentity,
    PluginManifest,
};
use bitty_runtime::Runtime;
use bitty_runtime::component::{
    BrokerEventKind, COMPONENT_HANDSHAKE_TIMEOUT, ComponentRequest, ComponentState,
    ComponentWarnSink, StopOutcome, executable_file_name,
};
use bitty_terminal::component::{ComponentContext, run_component_subcommand};
use bitty_terminal::component_host::install_with_roots;

const FIXTURE: &str = env!("CARGO_BIN_EXE_bitty-component-fixture");
const NAME: &str = "fixture";
const VERSION: &str = "0.0.1";
const PLUGIN: &str = "acme.component-test";
const HOST: &str = "api.example.com";
const URL: &str = "https://api.example.com/v1/ping";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bitty-ctx1361-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch root");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Stage `executable` as component `NAME` through the real `add` path.
fn install_via_add(root: &Path, executable: &Path) {
    let staging = root.join("staging");
    std::fs::create_dir_all(&staging).expect("staging dir");
    let file_name = executable_file_name(&format!("bitty-{NAME}"));
    let staged_executable = staging.join(&file_name);
    std::fs::copy(executable, &staged_executable).expect("copy component");
    let bytes = std::fs::read(&staged_executable).expect("read component");
    let digest = bitty_package::integrity::sha256_hex(&bytes);
    let descriptor = format!(
        "[component]\nname = \"{NAME}\"\nversion = \"{VERSION}\"\nprotocol = [1, 1]\nexecutable = \"bitty-{NAME}\"\nsha256 = \"{digest}\"\n"
    );
    std::fs::write(
        staging.join(bitty_runtime::component::COMPONENT_DESCRIPTOR_FILE),
        descriptor,
    )
    .expect("descriptor");

    let components_dir = root.to_string_lossy().into_owned();
    let system_dir = root.join("system").to_string_lossy().into_owned();
    let context = ComponentContext {
        xdg_data_home: None,
        home: None,
        components_dir: Some(&components_dir),
        system_components_dir: Some(&system_dir),
        wsl_distro_env: None,
        pre_format: None,
        pre_no_color: true,
    };
    let mut input = std::io::Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let code = run_component_subcommand(
        &["add".to_string(), staging.to_string_lossy().into_owned()],
        &context,
        &mut input,
        &mut output,
    );
    assert_eq!(
        code,
        0,
        "real add failed: {}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        root.join(NAME).join("current").exists(),
        "add did not publish current"
    );
}

struct SilentWarn;

impl ComponentWarnSink for SilentWarn {
    fn warn(&mut self, _component: &str, _reason: &str, _stderr_tail: &str) {}
}

/// Drive the production installer (the same function `main` calls), with
/// explicit roots instead of process-global environment mutation.
fn runtime_with_host(root: &Path) -> Runtime {
    let mut runtime =
        Runtime::with_deterministic_rasterizer(bitty_runtime::RuntimeConfig::default())
            .expect("runtime");
    assert!(install_with_roots(
        &mut runtime,
        Some(root.to_owned()),
        None,
        SilentWarn,
    ));
    runtime
}

fn plugin_manifest(id: &str, network_capability: Option<&str>) -> PluginManifest {
    let mut ids = BTreeSet::new();
    if let Some(capability) = network_capability {
        ids.insert(CapabilityId::parse(capability).expect("valid capability"));
    }
    PluginManifest {
        identity: PluginIdentity {
            id: PluginId::new(id).expect("valid plugin id"),
            name: "Component test".to_string(),
            version: "0.1.0".to_string(),
            description: "component host test".to_string(),
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
        network: network_capability.map_or_else(Vec::new, |_| {
            vec![NetworkEgress {
                host: HOST.to_string(),
                ports: vec![443],
            }]
        }),
        limits: Default::default(),
        lazy: LazyTriggers::default(),
        settings: Vec::new(),
        next_steps: Vec::new(),
        raw_bytes_len: 256,
    }
}

fn network_capability() -> CapabilityId {
    CapabilityId::parse(&format!("network.connect:{HOST}")).expect("capability")
}

fn register_granted(runtime: &mut Runtime) -> String {
    let manifest = plugin_manifest(PLUGIN, Some(&format!("network.connect:{HOST}")));
    let hash = manifest.manifest_hash();
    let plugin_id = PluginId::new(PLUGIN).expect("valid id");
    runtime.register_plugin(manifest).expect("register plugin");
    let mut granted = BTreeSet::new();
    granted.insert(network_capability());
    runtime.insert_grant(GrantRecord::granted(
        plugin_id.clone(),
        hash.clone(),
        granted,
        1,
    ));
    runtime
        .activate_plugin(&plugin_id)
        .expect("activate plugin after grant");
    hash
}

fn request() -> ComponentRequest {
    ComponentRequest::new(Method::Get, URL)
}

#[test]
fn installed_fixture_round_trips_through_production_host() {
    let scratch = Scratch::new("roundtrip");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    let hash = register_granted(&mut runtime);

    let response = runtime
        .component_request(PLUGIN, &hash, NAME, request())
        .expect("component response");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, URL.as_bytes());
    let headers: std::collections::BTreeMap<_, _> = response.headers.into_iter().collect();
    assert_eq!(headers["x-plugin-id"], PLUGIN);
    assert_eq!(headers["x-grant"], "api.example.com:443");

    let status = runtime.component_status(NAME).expect("status");
    assert_eq!(status.spawns, 1);
    assert!(status.pid.is_some());
}

#[test]
fn absent_host_fails_closed() {
    let mut runtime =
        Runtime::with_deterministic_rasterizer(bitty_runtime::RuntimeConfig::default())
            .expect("runtime");
    let hash = register_granted(&mut runtime);
    let error = runtime
        .component_request(PLUGIN, &hash, NAME, request())
        .expect_err("no host");
    assert_eq!(error.class(), "component_host_unavailable");
}

#[test]
fn missing_capability_grant_denies_before_spawn() {
    let scratch = Scratch::new("denied");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    let manifest = plugin_manifest(PLUGIN, None);
    let hash = manifest.manifest_hash();
    runtime.register_plugin(manifest).expect("register plugin");
    let plugin_id = PluginId::new(PLUGIN).expect("valid id");
    // No declared authority: activation needs no grant.
    runtime
        .activate_plugin(&plugin_id)
        .expect("activate plugin");
    // A real (non-denied) record with an empty capability set reaches the
    // grant computation and must still be denied as offline.
    runtime.insert_grant(GrantRecord::granted(
        plugin_id,
        hash.clone(),
        BTreeSet::new(),
        1,
    ));

    let error = runtime
        .component_request(PLUGIN, &hash, NAME, request())
        .expect_err("denied");
    assert_eq!(error.class(), "capability_denied");
    assert!(runtime.component_status(NAME).is_none(), "must not spawn");
}

#[test]
fn unknown_component_denies_without_spawn() {
    let scratch = Scratch::new("unknown");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    let hash = register_granted(&mut runtime);

    let error = runtime
        .component_request(PLUGIN, &hash, "not-installed", request())
        .expect_err("unknown component");
    assert_eq!(error.class(), "component_unavailable");
    assert_eq!(
        runtime
            .component_status("not-installed")
            .map(|status| status.spawns),
        Some(0)
    );
}

#[test]
fn mutated_digest_is_untrusted_and_never_spawns() {
    let scratch = Scratch::new("digest");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let descriptor = scratch
        .path()
        .join(NAME)
        .join(VERSION)
        .join(bitty_runtime::component::COMPONENT_DESCRIPTOR_FILE);
    let text = std::fs::read_to_string(&descriptor).expect("descriptor");
    let mutated = text
        .lines()
        .map(|line| {
            if line.starts_with("sha256 = ") {
                format!("sha256 = \"{}\"", "0".repeat(64))
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&descriptor, format!("{mutated}\n")).expect("mutate descriptor");

    let mut runtime = runtime_with_host(scratch.path());
    let hash = register_granted(&mut runtime);
    let error = runtime
        .component_request(PLUGIN, &hash, NAME, request())
        .expect_err("digest mismatch");
    assert_eq!(error.class(), "component_untrusted");
    let status = runtime.component_status(NAME).expect("status slot");
    assert_eq!(status.spawns, 0);
    assert!(status.pid.is_none());
}

fn assert_no_spawn(runtime: &Runtime, component: &str) {
    let spawns = runtime
        .component_status(component)
        .map_or(0, |status| status.spawns);
    assert_eq!(spawns, 0, "component '{component}' must not spawn");
}

#[test]
fn revoked_capability_is_denied_even_after_record_reinsert() {
    let scratch = Scratch::new("revoked");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    let hash = register_granted(&mut runtime);
    let plugin_id = PluginId::new(PLUGIN).expect("valid id");
    let capability = network_capability();

    runtime
        .revoke_grant(&plugin_id, Some(&capability))
        .expect("revoke capability");
    // The grant API explicitly allows re-inserting a record; the per-capability
    // denial must still win in `is_granted`.
    let mut granted = BTreeSet::new();
    granted.insert(capability.clone());
    runtime.insert_grant(GrantRecord::granted(
        plugin_id.clone(),
        hash.clone(),
        granted,
        2,
    ));
    assert!(
        !runtime.is_capability_granted(&plugin_id, &hash, &capability),
        "revoked capability must stay denied after re-insert"
    );

    let error = runtime
        .component_request(PLUGIN, &hash, NAME, request())
        .expect_err("revoked capability must deny");
    assert_eq!(error.class(), "capability_denied");
    assert_no_spawn(&runtime, NAME);
}

#[test]
fn registered_plugin_is_denied() {
    let scratch = Scratch::new("registered");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    let manifest = plugin_manifest(PLUGIN, Some(&format!("network.connect:{HOST}")));
    let hash = manifest.manifest_hash();
    let plugin_id = PluginId::new(PLUGIN).expect("valid id");
    runtime.register_plugin(manifest).expect("register plugin");
    let mut granted = BTreeSet::new();
    granted.insert(network_capability());
    runtime.insert_grant(GrantRecord::granted(plugin_id, hash.clone(), granted, 1));

    let error = runtime
        .component_request(PLUGIN, &hash, NAME, request())
        .expect_err("registered plugin must deny");
    assert_eq!(error.class(), "plugin_not_active");
    assert_no_spawn(&runtime, NAME);
}

#[test]
fn suspended_plugin_is_denied() {
    let scratch = Scratch::new("suspended");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    let hash = register_granted(&mut runtime);
    let plugin_id = PluginId::new(PLUGIN).expect("valid id");
    runtime
        .suspend_plugin(&plugin_id)
        .expect("suspend activated plugin");

    let error = runtime
        .component_request(PLUGIN, &hash, NAME, request())
        .expect_err("suspended plugin must deny");
    assert_eq!(error.class(), "plugin_not_active");
    assert_no_spawn(&runtime, NAME);
}

#[test]
fn caller_manifest_hash_mismatch_is_denied() {
    let scratch = Scratch::new("hash-mismatch");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    let _ = register_granted(&mut runtime);
    let wrong = "0".repeat(64);

    let error = runtime
        .component_request(PLUGIN, &wrong, NAME, request())
        .expect_err("wrong caller hash must deny");
    assert_eq!(error.class(), "capability_denied");
    assert_no_spawn(&runtime, NAME);
}

#[test]
fn denied_grant_record_is_denied() {
    let scratch = Scratch::new("denied-record");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    let hash = register_granted(&mut runtime);
    runtime.insert_grant(GrantRecord::denied(
        PluginId::new(PLUGIN).expect("valid id"),
        hash.clone(),
        2,
    ));

    let error = runtime
        .component_request(PLUGIN, &hash, NAME, request())
        .expect_err("denied record must deny");
    assert_eq!(error.class(), "capability_denied");
    assert_no_spawn(&runtime, NAME);
}

#[test]
fn grant_bound_to_foreign_hash_is_denied() {
    let scratch = Scratch::new("foreign-hash");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    let hash = register_granted(&mut runtime);
    let mut granted = BTreeSet::new();
    granted.insert(network_capability());
    runtime.insert_grant(GrantRecord::granted(
        PluginId::new(PLUGIN).expect("valid id"),
        "foreign-manifest-hash",
        granted,
        2,
    ));

    let error = runtime
        .component_request(PLUGIN, &hash, NAME, request())
        .expect_err("foreign-hash record must deny");
    assert_eq!(error.class(), "capability_denied");
    assert_no_spawn(&runtime, NAME);
}

#[test]
fn undeclared_capability_is_denied() {
    let scratch = Scratch::new("undeclared");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    // Declares no authority and no egress, so activation needs no grant.
    let manifest = plugin_manifest(PLUGIN, None);
    let hash = manifest.manifest_hash();
    let plugin_id = PluginId::new(PLUGIN).expect("valid id");
    runtime.register_plugin(manifest).expect("register plugin");
    runtime
        .activate_plugin(&plugin_id)
        .expect("activate plugin");
    let mut granted = BTreeSet::new();
    granted.insert(network_capability());
    runtime.insert_grant(GrantRecord::granted(plugin_id, hash.clone(), granted, 1));

    let error = runtime
        .component_request(PLUGIN, &hash, NAME, request())
        .expect_err("undeclared capability must deny");
    assert_eq!(error.class(), "capability_denied");
    assert_no_spawn(&runtime, NAME);
}

#[test]
fn tick_at_drives_handshake_timeout_without_an_explicit_poll() {
    let scratch = Scratch::new("tick-timeout");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    // The fixture reads `fixture-mode` from its version directory; `mute`
    // never answers the handshake.
    std::fs::write(
        scratch.path().join(NAME).join(VERSION).join("fixture-mode"),
        "mute",
    )
    .expect("fixture mode");
    let mut runtime = runtime_with_host(scratch.path());
    let hash = register_granted(&mut runtime);
    let id = runtime
        .submit_component_request(PLUGIN, &hash, NAME, request())
        .expect("submit");
    assert_eq!(
        runtime.component_status(NAME).map(|status| status.state),
        Some(ComponentState::Handshaking)
    );

    let now = Instant::now() + COMPONENT_HANDSHAKE_TIMEOUT + Duration::from_secs(1);
    runtime.tick_at(now);

    let events = runtime.take_component_events();
    assert!(
        events.iter().any(|event| {
            event.id == id
                && matches!(
                    event.kind,
                    BrokerEventKind::Failed {
                        kind: ErrorKind::ComponentLost,
                        ..
                    }
                )
        }),
        "tick_at must surface the handshake-timeout failure: {events:?}"
    );
    let status = runtime.component_status(NAME).expect("status");
    assert_eq!(status.state, ComponentState::Stopped);
    assert_eq!(status.last_stop, Some(StopOutcome::Crashed));
}

#[test]
fn tick_at_retains_response_events_for_the_embedder() {
    let scratch = Scratch::new("tick-delivery");
    install_via_add(scratch.path(), Path::new(FIXTURE));
    let mut runtime = runtime_with_host(scratch.path());
    let hash = register_granted(&mut runtime);
    let id = runtime
        .submit_component_request(PLUGIN, &hash, NAME, request())
        .expect("submit");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut terminal = false;
    while Instant::now() < deadline {
        runtime.tick_at(Instant::now());
        let events = runtime.take_component_events();
        if events
            .iter()
            .any(|event| event.id == id && event.is_terminal())
        {
            terminal = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(terminal, "tick_at discarded the response events");
}

/// Real `bitty-net` proof. Not part of the default suite: the component lives
/// in the `bitty-network` repository and is not a Core build artifact.
///
/// Run after building `bitty-net`:
/// `BITTY_NET_BIN=/path/to/bitty-net cargo test -p bitty-runtime --test
/// component_host real_bitty_net_request -- --ignored --nocapture`.
#[test]
#[ignore = "requires BITTY_NET_BIN pointing at a built bitty-net component"]
fn real_bitty_net_request() {
    let Ok(bin) = std::env::var("BITTY_NET_BIN") else {
        eprintln!("BITTY_NET_BIN not set; skipping real bitty-net proof");
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").expect("local listener");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0u8; 2048];
        let _ = stream.read(&mut buffer);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
            .expect("write response");
    });

    let scratch = Scratch::new("bitty-net");
    install_via_add_with_name(scratch.path(), Path::new(&bin), "net", "0.0.1");

    let mut runtime = runtime_with_host(scratch.path());
    let plugin = "acme.net-test";
    let capability = format!("network.connect:127.0.0.1:{port}");
    let manifest = PluginManifest {
        identity: PluginIdentity {
            id: PluginId::new(plugin).expect("valid id"),
            name: "Net test".to_string(),
            version: "0.1.0".to_string(),
            description: "real bitty-net proof".to_string(),
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
            ids: [CapabilityId::parse(&capability).expect("capability")]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        tools: Vec::new(),
        network: vec![NetworkEgress {
            host: "127.0.0.1".to_string(),
            ports: vec![port],
        }],
        limits: Default::default(),
        lazy: LazyTriggers::default(),
        settings: Vec::new(),
        next_steps: Vec::new(),
        raw_bytes_len: 256,
    };
    let hash = manifest.manifest_hash();
    runtime.register_plugin(manifest).expect("register plugin");
    let plugin_id = PluginId::new(plugin).expect("valid id");
    runtime.insert_grant(GrantRecord::granted(
        plugin_id.clone(),
        hash.clone(),
        [CapabilityId::parse(&capability).expect("capability")]
            .into_iter()
            .collect(),
        1,
    ));
    runtime
        .activate_plugin(&plugin_id)
        .expect("activate plugin after grant");

    let response = runtime
        .component_request(
            plugin,
            &hash,
            "net",
            ComponentRequest::new(Method::Get, format!("http://127.0.0.1:{port}/ping")),
        )
        .expect("real bitty-net response");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"hello");
    server.join().expect("server thread");
}

/// Variant of [`install_via_add`] for a non-fixture component name.
fn install_via_add_with_name(root: &Path, executable: &Path, name: &str, version: &str) {
    let staging = root.join(format!("staging-{name}"));
    std::fs::create_dir_all(&staging).expect("staging dir");
    let file_name = executable_file_name(&format!("bitty-{name}"));
    let staged_executable = staging.join(&file_name);
    std::fs::copy(executable, &staged_executable).expect("copy component");
    let bytes = std::fs::read(&staged_executable).expect("read component");
    let digest = bitty_package::integrity::sha256_hex(&bytes);
    let descriptor = format!(
        "[component]\nname = \"{name}\"\nversion = \"{version}\"\nprotocol = [1, 1]\nexecutable = \"bitty-{name}\"\nsha256 = \"{digest}\"\n"
    );
    std::fs::write(
        staging.join(bitty_runtime::component::COMPONENT_DESCRIPTOR_FILE),
        descriptor,
    )
    .expect("descriptor");

    let components_dir = root.to_string_lossy().into_owned();
    let system_dir = root.join("system").to_string_lossy().into_owned();
    let context = ComponentContext {
        xdg_data_home: None,
        home: None,
        components_dir: Some(&components_dir),
        system_components_dir: Some(&system_dir),
        wsl_distro_env: None,
        pre_format: None,
        pre_no_color: true,
    };
    let mut input = std::io::Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let code = run_component_subcommand(
        &["add".to_string(), staging.to_string_lossy().into_owned()],
        &context,
        &mut input,
        &mut output,
    );
    assert_eq!(
        code,
        0,
        "real add failed: {}",
        String::from_utf8_lossy(&output)
    );
}
