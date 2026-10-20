//! CTX-2254 (bitty#2254): the production request surface reaches an installed
//! component.
//!
//! The fixture component is staged through the real `bitty extension add`
//! implementation, activated plugin generations are mirrored into the runtime's
//! policy host by the same composition-root function `main` calls, and the
//! request is produced by plugin Lua through `bitty.component.request`, drained
//! by the production per-tick pump, and read back through
//! `bitty.component.poll`. Nothing writes the install layout by hand and no
//! test-only producer stands in for the surface.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bitty_lua::LuaValue;
use bitty_plugin_host::{CapabilityId, GrantRecord, PluginId, PluginState};
use bitty_runtime::Runtime;
use bitty_runtime::component::{
    ActivationDenial, ActivationState, ComponentHostError, ComponentWarnSink, executable_file_name,
    read_activation,
};
use bitty_runtime::plugin_runtime::package::{LocalInstallOptions, install_local_dir};
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};
use bitty_terminal::component::{ComponentContext, run_component_subcommand};
use bitty_terminal::component_bridge::{mirror_plugin_authority, pump_component_requests};
use bitty_terminal::component_host::install_with_roots;

const FIXTURE: &str = env!("CARGO_BIN_EXE_bitty-component-fixture");
const VERSION: &str = "0.0.1";
const ECHO: &str = "fixture";
const CRASHY: &str = "crashy";
const HOLDY: &str = "holdy";
const URL: &str = "https://api.example.com/v1/ping";
/// Same host over the other scheme's default port: outside the granted
/// `443` egress, so the component must refuse it.
const OUT_OF_GRANT_URL: &str = "http://api.example.com/v1/ping";
const EGRESS_HOST: &str = "api.example.com";
const EGRESS_PORT: u16 = 443;
const CAPABILITY: &str = "network.connect:api.example.com:443";
/// Second declared capability so a per-capability revocation leaves the grant
/// record in place (the last capability revoke escalates to a plugin denial).
const SECOND_CAPABILITY: &str = "terminal.semantic-read";
const POLL_BUDGET: Duration = Duration::from_millis(10);
const POLL_ATTEMPTS: usize = 400;
const RELOAD_PLUGIN: &str = "acme.reload";
const RELOAD_V2_URL: &str = "https://api.example.com/v2/reloaded";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bitty-ctx2254-{tag}-{}", std::process::id()));
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

struct SilentWarn;

impl ComponentWarnSink for SilentWarn {
    fn warn(&mut self, _component: &str, _reason: &str, _stderr_tail: &str) {}
}

/// Stage `executable` as component `name` through the real `add` path.
fn install_via_add(root: &Path, name: &str, executable: &Path, mode: &str) {
    let staging = root.join(format!("staging-{name}"));
    std::fs::create_dir_all(&staging).expect("staging dir");
    let file_name = executable_file_name(&format!("bitty-{name}"));
    let staged_executable = staging.join(&file_name);
    std::fs::copy(executable, &staged_executable).expect("copy component");
    let bytes = std::fs::read(&staged_executable).expect("read component");
    let digest = bitty_package::integrity::sha256_hex(&bytes);
    let descriptor = format!(
        "[component]\nname = \"{name}\"\nversion = \"{VERSION}\"\nprotocol = [1, 1]\nexecutable = \"bitty-{name}\"\nsha256 = \"{digest}\"\n"
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
        xdg_state_home: None,
        component_state_dir: None,
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
        "real add failed for {name}: {}",
        String::from_utf8_lossy(&output)
    );
    std::fs::write(root.join(name).join(VERSION).join("fixture-mode"), mode).expect("fixture mode");

    // The descriptor digest covers the executable only; rewriting the mode file
    // never changes it, so the broker's digest check stays valid.
    assert!(root.join(name).join("current").exists());
}

/// One dev plugin whose Lua exposes `go` (queue a request) and `poll`.
fn write_plugin(root: &Path, id: &str, lua: &str, capability: Option<&str>, egress: Option<&str>) {
    write_versioned_plugin(root, id, "0.1.0", lua, capability, &[], egress);
}

/// One plugin source at an explicit version, used by the reload regression.
fn write_versioned_plugin(
    root: &Path,
    id: &str,
    version: &str,
    lua: &str,
    capability: Option<&str>,
    extra_capabilities: &[&str],
    egress: Option<&str>,
) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let mut capabilities = capability
        .map(|capability| format!("\"{capability}\" = true\n"))
        .unwrap_or_default();
    for capability in extra_capabilities {
        capabilities.push_str(&format!("\"{capability}\" = true\n"));
    }
    let network = egress
        .map(|host| format!("\n[[network.egress]]\nhost = \"{host}\"\nports = [{EGRESS_PORT}]\n"))
        .unwrap_or_default();
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Component Surface Test"
version = "{version}"
description = "production component request surface"

[compat]
plugin-api = "^1.0"

[capabilities]
{capabilities}{network}
[lazy]
commands = ["{id}:go", "{id}:poll", "{id}:big"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), lua).expect("init");
}

/// Lua shared by every surface plugin: `go` queues one request and returns the
/// ticket, `poll` reports `pending`, `ok:<status>:<body>`, or `err:<code>`, and
/// `big` submits an over-bound payload.
fn surface_lua(component: &str, url: &str, timeout_ms: u32) -> String {
    format!(
        r#"
bitty.commands.register({{
  id = "go",
  title = "Surface: queue one request",
  description = "Queue one bounded component request and return its ticket.",
  run = function()
    local ticket = bitty.component.request({{
      id = "{component}",
      method = "GET",
      url = "{url}",
      timeout_ms = {timeout_ms},
    }})
    return tostring(ticket)
  end,
}})

bitty.commands.register({{
  id = "poll",
  title = "Surface: poll",
  description = "Read one ticket: pending, ok:<status>:<body>, or err:<code>.",
  run = function(args)
    local ticket = tonumber((args or {{}}).ticket)
    local ok, result = pcall(bitty.component.poll, ticket)
    if not ok then
      local report = "err:" .. tostring(result.code)
      if result.code == "E_COMPONENT_REQUEST_FAILED" then
        report = report .. ":" .. tostring(result.message)
      end
      return report
    end
    if result == nil then
      return "pending"
    end
    local grant = ""
    for _, header in ipairs(result.headers or {{}}) do
      if header.name == "x-grant" then
        grant = header.value
      end
    end
    return "ok:" .. tostring(result.status) .. ":" .. result.body .. "|grant:" .. grant
  end,
}})

bitty.commands.register({{
  id = "big",
  title = "Surface: over-bound payload",
  description = "Submit a payload above the 8 KiB bridge ceiling.",
  run = function()
    local ok, result = pcall(bitty.component.request, {{
      id = "{component}",
      method = "POST",
      url = "{url}",
      body = string.rep("x", 9216),
    }})
    if ok then
      return "ok:" .. tostring(result)
    end
    return "err:" .. tostring(result.code)
  end,
}})

return {{}}
"#
    )
}

/// Drive one ticket to completion through the production pump.
fn poll_until_settled(
    runtime: &mut Runtime,
    plugins: &mut PluginRuntime,
    plugin: &str,
    ticket: &str,
) -> String {
    let ticket = LuaValue::Integer(ticket.parse().expect("ticket integer"));
    let args = LuaValue::table([("ticket", ticket)]);
    for _ in 0..POLL_ATTEMPTS {
        pump_component_requests(runtime, plugins, Instant::now());
        match plugins.invoke_command(&format!("{plugin}:poll"), &args) {
            Ok(LuaValue::String(report)) if report != "pending" => return report,
            Ok(_) => {}
            Err(error) => panic!("poll command failed: {error}"),
        }
        std::thread::sleep(POLL_BUDGET);
    }
    panic!("component request did not settle within {POLL_ATTEMPTS} polls");
}

/// Queue one request through the plugin's Lua surface.
fn queue_request(plugins: &mut PluginRuntime, plugin: &str) -> String {
    match plugins
        .invoke_command(&format!("{plugin}:go"), &LuaValue::Nil)
        .expect("go command")
    {
        LuaValue::String(ticket) => ticket,
        other => panic!("go must return a ticket string, got {other:?}"),
    }
}

#[test]
fn production_surface_reaches_an_installed_component_and_fails_closed() {
    let scratch = Scratch::new("surface");
    let components = scratch.path().join("extensions");
    std::fs::create_dir_all(&components).expect("component root");
    install_via_add(&components, ECHO, Path::new(FIXTURE), "echo");
    install_via_add(&components, CRASHY, Path::new(FIXTURE), "crash");
    install_via_add(&components, HOLDY, Path::new(FIXTURE), "hold");

    let plugins_root = scratch.path().join("plugins");
    std::fs::create_dir_all(&plugins_root).expect("plugin root");
    let allowed = "acme.surface-allowed";
    let denied = "acme.surface-denied";
    let out_of_grant = "acme.surface-out-of-grant";
    let oversize = "acme.surface-oversize";
    let crashed = "acme.surface-crashed";
    let held = "acme.surface-held";
    write_plugin(
        &plugins_root,
        allowed,
        &surface_lua(ECHO, URL, 5_000),
        Some(CAPABILITY),
        Some(EGRESS_HOST),
    );
    // Same declared authority as the allowed plugin; the denial below is an
    // explicit revoked grant record, the accepted DIR-030 model.
    write_plugin(
        &plugins_root,
        denied,
        &surface_lua(ECHO, URL, 5_000),
        Some(CAPABILITY),
        Some(EGRESS_HOST),
    );
    // Same declared authority as the allowed plugin, but the request URL's
    // effective port (80) is outside the granted `443` egress. The component
    // enforces the Core-issued grant, so this must fail closed.
    write_plugin(
        &plugins_root,
        out_of_grant,
        &surface_lua(ECHO, OUT_OF_GRANT_URL, 5_000),
        Some(CAPABILITY),
        Some(EGRESS_HOST),
    );
    write_plugin(
        &plugins_root,
        oversize,
        &surface_lua(ECHO, URL, 5_000),
        Some(CAPABILITY),
        Some(EGRESS_HOST),
    );
    write_plugin(
        &plugins_root,
        crashed,
        &surface_lua(CRASHY, URL, 5_000),
        Some(CAPABILITY),
        Some(EGRESS_HOST),
    );
    write_plugin(
        &plugins_root,
        held,
        &surface_lua(HOLDY, URL, 200),
        Some(CAPABILITY),
        Some(EGRESS_HOST),
    );

    let mut runtime =
        Runtime::with_deterministic_rasterizer(bitty_runtime::RuntimeConfig::default())
            .expect("runtime");
    assert!(install_with_roots(
        &mut runtime,
        Some(components.clone()),
        None,
        SilentWarn
    ));

    let mut plugins = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(scratch.path().join("plugins-state")),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![plugins_root.clone()],
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(EmptySnapshot),
    });
    let discovered = plugins.discover();
    for (id, result) in &discovered {
        if let Err(error) = result {
            eprintln!("discovery refused {id}: {error}");
        }
    }
    assert_eq!(discovered.len(), 6, "six dev plugins discovered");
    for (id, result) in plugins.activate_discovered() {
        let report = result.unwrap_or_else(|error| panic!("activation failed for {id}: {error}"));
        assert_eq!(report.state, LifecycleState::Active);
    }

    let mirrored = mirror_plugin_authority(&mut runtime, &plugins, |plugin, reason| {
        panic!("mirror refused {plugin}: {reason}");
    });
    assert_eq!(mirrored, 6, "every active generation must be mirrored");

    // ── one request/response round trip through the production surface ─────
    let ticket = queue_request(&mut plugins, allowed);
    let report = poll_until_settled(&mut runtime, &mut plugins, allowed, &ticket);
    assert_eq!(
        report,
        format!("ok:200:{URL}|grant:{EGRESS_HOST}:{EGRESS_PORT}"),
        "the served request echoes the URL and the Core-issued grant host:port"
    );
    let status = runtime
        .component_status(ECHO)
        .expect("component must be running after a served request");
    assert!(
        status.pid.is_some(),
        "a served request spawned the component"
    );
    eprintln!(
        "served: {report} (component pid {:?}, spawns {})",
        status.pid, status.spawns
    );

    // ── a denied request fails closed with the named class ────────────────
    // Revoke the denied plugin's grant record in the runtime policy host (the
    // production explicit-revoke shape) and prove the request is refused by
    // the canonical capability gate before any submit.
    let denied_id = PluginId::new(denied).expect("valid id");
    let denied_hash = plugins
        .discovered_package(&denied_id)
        .expect("denied package")
        .manifest
        .manifest_hash();
    runtime.insert_grant(GrantRecord::denied(denied_id, denied_hash, 2));
    let ticket = queue_request(&mut plugins, denied);
    let report = poll_until_settled(&mut runtime, &mut plugins, denied, &ticket);
    assert_eq!(
        report, "err:E_COMPONENT_CAPABILITY_DENIED",
        "an offline grant must deny with capability_denied"
    );
    eprintln!("denied: {report}");

    // ── an out-of-grant port is refused by the component's grant check ───
    // The granted egress is `api.example.com:443`; `http://` resolves to port
    // 80. Core hands the exact grant to the component and the component
    // refuses the destination before any connection, so the ticket fails
    // closed instead of echoing the URL.
    let ticket = queue_request(&mut plugins, out_of_grant);
    let report = poll_until_settled(&mut runtime, &mut plugins, out_of_grant, &ticket);
    assert!(
        report.starts_with("err:E_COMPONENT_REQUEST_FAILED"),
        "an out-of-grant destination must fail closed: {report}"
    );
    assert!(
        report.contains("grant denied: api.example.com:80"),
        "the fixture must refuse the destination against the Core-issued grant: {report}"
    );
    assert_eq!(
        runtime
            .component_status(ECHO)
            .map(|status| status.last_stop),
        Some(None),
        "a grant denial is not a component crash"
    );
    eprintln!("out-of-grant: {report}");

    // ── an over-bound payload is rejected before any send ─────────────────
    let report = match plugins
        .invoke_command(&format!("{oversize}:big"), &LuaValue::Nil)
        .expect("big command")
    {
        LuaValue::String(report) => report,
        other => panic!("big must return a report string, got {other:?}"),
    };
    assert_eq!(report, "err:E_COMPONENT_PAYLOAD_TOO_LARGE");
    let queue = plugins.component_request_queue().borrow();
    assert_eq!(queue.pending(), 0, "an over-bound payload never enqueues");
    assert_eq!(queue.in_flight(), 0, "nothing was sent for it");
    drop(queue);
    eprintln!("oversize: {report}");

    // ── a crashed component fails the request with a typed class ──────────
    let ticket = queue_request(&mut plugins, crashed);
    let report = poll_until_settled(&mut runtime, &mut plugins, crashed, &ticket);
    assert!(
        report.starts_with("err:E_COMPONENT_REQUEST_FAILED"),
        "a component lost mid-request fails the ticket: {report}"
    );
    assert_eq!(
        runtime
            .component_status(CRASHY)
            .map(|status| status.last_stop),
        Some(Some(bitty_runtime::component::StopOutcome::Crashed)),
        "the crash must be recorded on the component"
    );
    eprintln!("crashed: {report}");

    // ── the request deadline is enforced ──────────────────────────────────
    let ticket = queue_request(&mut plugins, held);
    let report = poll_until_settled(&mut runtime, &mut plugins, held, &ticket);
    assert_eq!(
        report, "err:E_COMPONENT_TIMEOUT",
        "a silent component must hit the ticket deadline"
    );
    eprintln!("deadline: {report}");

    // ── the queue drains: no ticket is left in flight ─────────────────────
    let queue = plugins.component_request_queue().borrow();
    assert_eq!(queue.in_flight(), 0);
    assert_eq!(plugins.component_requests_dropped(), 0);
}

/// Install one local source into the store with its declared set approved.
fn install_approved(store: &Path, source: &Path) {
    install_local_dir(store, source, &LocalInstallOptions::default(), &mut |_| {
        Ok(true)
    })
    .expect("install")
    .expect("approved");
}

/// Store-backed v1 generation mirrored into the policy host, with a v2
/// source staged for the reload scenarios.
struct ReloadFixture {
    runtime: Runtime,
    plugins: PluginRuntime,
    id: PluginId,
    store: PathBuf,
    source_v2: PathBuf,
    v1_hash: String,
    // Kept alive for the fixture's lifetime; dropped last so the runtime and
    // plugin session tear down before the scratch tree disappears.
    _scratch: Scratch,
}

impl ReloadFixture {
    /// Install v2 over the store and reload the live generation.
    fn reload_to_v2(&mut self) -> String {
        install_approved(&self.store, &self.source_v2.join(RELOAD_PLUGIN));
        let report = self.plugins.reload(&self.id).expect("reload v2");
        assert_eq!(report.state, LifecycleState::Active);
        let v2_hash = self
            .plugins
            .discovered_package(&self.id)
            .expect("v2 package")
            .manifest
            .manifest_hash();
        assert_ne!(
            self.v1_hash, v2_hash,
            "reload must move to a new manifest hash"
        );
        v2_hash
    }
}

fn reload_fixture(tag: &str) -> ReloadFixture {
    let scratch = Scratch::new(tag);
    let components = scratch.path().join("extensions");
    std::fs::create_dir_all(&components).expect("component root");
    install_via_add(&components, ECHO, Path::new(FIXTURE), "echo");

    let store = scratch.path().join("store");
    let source_v1 = scratch.path().join("source-v1");
    let source_v2 = scratch.path().join("source-v2");
    write_versioned_plugin(
        &source_v1,
        RELOAD_PLUGIN,
        "0.1.0",
        &surface_lua(ECHO, URL, 5_000),
        Some(CAPABILITY),
        &[SECOND_CAPABILITY],
        Some(EGRESS_HOST),
    );
    write_versioned_plugin(
        &source_v2,
        RELOAD_PLUGIN,
        "0.2.0",
        &surface_lua(ECHO, RELOAD_V2_URL, 5_000),
        Some(CAPABILITY),
        &[SECOND_CAPABILITY],
        Some(EGRESS_HOST),
    );
    install_approved(&store, &source_v1.join(RELOAD_PLUGIN));

    let mut runtime =
        Runtime::with_deterministic_rasterizer(bitty_runtime::RuntimeConfig::default())
            .expect("runtime");
    assert!(install_with_roots(
        &mut runtime,
        Some(components.clone()),
        None,
        SilentWarn
    ));

    let mut plugins = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(scratch.path().join("plugins-state")),
        store_root: Some(store.clone()),
        bundled_roots: Vec::new(),
        third_party_roots: Vec::new(),
        settings: std::rc::Rc::new(EmptySettings),
        snapshot: std::rc::Rc::new(EmptySnapshot),
    });
    assert_eq!(plugins.discover().len(), 1, "one store plugin discovered");
    let id = PluginId::new(RELOAD_PLUGIN).expect("plugin id");
    let report = plugins.activate(&id).expect("activate v1");
    assert_eq!(report.state, LifecycleState::Active);

    let v1_hash = plugins
        .discovered_package(&id)
        .expect("v1 package")
        .manifest
        .manifest_hash();
    assert_eq!(
        mirror_plugin_authority(&mut runtime, &plugins, |plugin, reason| {
            panic!("mirror refused {plugin}: {reason}")
        }),
        1,
        "the v1 generation is mirrored"
    );

    ReloadFixture {
        _scratch: scratch,
        runtime,
        plugins,
        id,
        store,
        source_v2,
        v1_hash,
    }
}

/// Production contract for a hot reload (bitty#2302): the mirror is keyed by
/// the generation hash, so re-running it after `PluginRuntime::reload`
/// authorizes the new manifest while the predecessor's hash and tickets stay
/// refused.
#[test]
fn reload_refreshes_the_mirrored_generation_and_purges_the_predecessor() {
    let mut fixture = reload_fixture("reload");
    let ticket = queue_request(&mut fixture.plugins, RELOAD_PLUGIN);
    assert_eq!(
        poll_until_settled(
            &mut fixture.runtime,
            &mut fixture.plugins,
            RELOAD_PLUGIN,
            &ticket
        ),
        format!("ok:200:{URL}|grant:{EGRESS_HOST}:{EGRESS_PORT}"),
        "the v1 generation serves through the mirrored authority"
    );

    // A ticket queued by v1 is purged by the reload, not delivered to v2.
    let stale_ticket = queue_request(&mut fixture.plugins, RELOAD_PLUGIN);
    let v2_hash = fixture.reload_to_v2();
    let stale = LuaValue::table([(
        "ticket",
        LuaValue::Integer(stale_ticket.parse().expect("stale ticket integer")),
    )]);
    assert_eq!(
        fixture
            .plugins
            .invoke_command(&format!("{RELOAD_PLUGIN}:poll"), &stale)
            .expect("poll stale ticket"),
        LuaValue::String("err:E_COMPONENT_TICKET".to_string()),
        "the predecessor's ticket is purged by the reload"
    );

    // Regression: the un-refreshed mirror still holds v1's hash, so v2 is
    // denied until the composition root mirrors the reloaded generation.
    let ticket = queue_request(&mut fixture.plugins, RELOAD_PLUGIN);
    assert_eq!(
        poll_until_settled(
            &mut fixture.runtime,
            &mut fixture.plugins,
            RELOAD_PLUGIN,
            &ticket
        ),
        "err:E_COMPONENT_CAPABILITY_DENIED",
        "a stale mirror must fail closed for the reloaded generation"
    );

    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            panic!("mirror refused {plugin}: {reason}")
        }),
        1,
        "the reloaded generation is mirrored once"
    );
    let registered = fixture
        .runtime
        .plugin_host()
        .registry()
        .get(&fixture.id)
        .expect("reloaded generation registered");
    assert_eq!(registered.state, PluginState::Activated);
    assert_eq!(
        registered.manifest.manifest_hash(),
        v2_hash,
        "the policy host now holds the reloaded manifest"
    );
    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            panic!("mirror refused {plugin}: {reason}")
        }),
        0,
        "the same generation is already mirrored and is not churned"
    );

    let ticket = queue_request(&mut fixture.plugins, RELOAD_PLUGIN);
    assert_eq!(
        poll_until_settled(
            &mut fixture.runtime,
            &mut fixture.plugins,
            RELOAD_PLUGIN,
            &ticket
        ),
        format!("ok:200:{RELOAD_V2_URL}|grant:{EGRESS_HOST}:{EGRESS_PORT}"),
        "the reloaded generation serves with its own code and hash"
    );

    // The predecessor's hash no longer authorizes anything.
    match fixture
        .runtime
        .component_grant(RELOAD_PLUGIN, &fixture.v1_hash)
    {
        Err(ComponentHostError::CapabilityDenied { reason, .. }) => assert!(
            reason.contains("manifest hash is not the registered generation"),
            "unexpected denial reason: {reason}"
        ),
        other => panic!("the predecessor generation must be refused, got {other:?}"),
    }
    assert!(
        !fixture
            .runtime
            .component_grant(RELOAD_PLUGIN, &v2_hash)
            .expect("reloaded generation authorized")
            .is_offline(),
        "the reloaded generation keeps its granted network authority"
    );

    let queue = fixture.plugins.component_request_queue().borrow();
    assert_eq!(queue.pending(), 0, "no submission survives the reload");
    assert_eq!(
        queue.in_flight(),
        0,
        "no broker request survives the reload"
    );
}

/// B1 (bitty#2302 review): an explicit plugin-level denial in the policy host
/// must survive a reload and a mirror refresh; the reloaded generation must be
/// neither re-granted nor activated.
#[test]
fn explicit_denial_survives_reload_and_mirror_refresh() {
    let mut fixture = reload_fixture("denied-reload");
    let id = fixture.id.clone();
    fixture
        .runtime
        .insert_grant(GrantRecord::denied(id.clone(), fixture.v1_hash.clone(), 7));
    let v2_hash = fixture.reload_to_v2();

    let mut warnings = Vec::new();
    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            warnings.push(format!("{plugin}: {reason}"));
        }),
        0,
        "a denied predecessor must not be re-granted for the reloaded generation"
    );
    assert!(
        fixture.runtime.plugin_host().grants().is_denied(&id),
        "the plugin-level denial marker must survive the refresh"
    );
    assert!(
        fixture.runtime.plugin_host().registry().get(&id).is_none(),
        "a denied generation must not be left activated in the policy host"
    );
    match fixture.runtime.component_grant(RELOAD_PLUGIN, &v2_hash) {
        Err(ComponentHostError::CapabilityDenied { reason, .. }) => assert!(
            reason.contains("plugin is not registered") || reason.contains("denied"),
            "unexpected denial reason: {reason}"
        ),
        other => panic!("the denied reloaded generation must be refused, got {other:?}"),
    }
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("explicit policy-host denial")),
        "the refusal must be reported: {warnings:?}"
    );
}

/// Leave the live generation `Registered` after a failed mirror activation.
///
/// Revoking the network capability keeps the second declared capability
/// granted, so the grant record survives and this stays a per-capability
/// denial rather than a plugin-level denial. The mirrored entry is dropped so
/// the next mirror registers from scratch and reaches activation, which fails
/// on the explicit denial.
fn mirror_with_failed_activation(fixture: &mut ReloadFixture) {
    let id = fixture.id.clone();
    let network = CapabilityId::parse(CAPABILITY).expect("network capability");
    let report = fixture
        .runtime
        .revoke_grant(&id, Some(&network))
        .expect("revoke network capability");
    assert!(
        !report.fully_revoked,
        "the fixture must keep a second granted capability"
    );
    fixture
        .runtime
        .plugin_host_mut()
        .remove(&id)
        .expect("drop the mirrored generation");
    let mut warnings = Vec::new();
    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            warnings.push(format!("{plugin}: {reason}"));
        }),
        0,
        "activation must fail while the capability denial persists"
    );
    assert_eq!(
        fixture
            .runtime
            .plugin_host()
            .registry()
            .get(&id)
            .expect("failed activation keeps the registration")
            .state,
        PluginState::Registered,
        "a failed activation leaves the generation Registered"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("explicitly denied capabilities")),
        "the failed activation must be reported: {warnings:?}"
    );
}

/// Follow-up to bitty#2311 / bitty#2318: a generation left `Registered` by a
/// failed activation is retried on the next mirror once the explicit denial is
/// cleared, and its component authority appears only with that activation.
#[test]
fn registered_generation_is_reactivated_after_grant_repair() {
    let mut fixture = reload_fixture("registered-retry");
    let id = fixture.id.clone();
    let network = CapabilityId::parse(CAPABILITY).expect("network capability");
    mirror_with_failed_activation(&mut fixture);

    // Before the repair the request surface fails closed as not active.
    match fixture
        .runtime
        .component_grant(RELOAD_PLUGIN, &fixture.v1_hash)
    {
        Err(ComponentHostError::PluginNotActive { state, .. }) => {
            assert_eq!(state, "Registered");
        }
        other => panic!("a Registered generation must fail closed, got {other:?}"),
    }
    let ticket = queue_request(&mut fixture.plugins, RELOAD_PLUGIN);
    assert_eq!(
        poll_until_settled(
            &mut fixture.runtime,
            &mut fixture.plugins,
            RELOAD_PLUGIN,
            &ticket
        ),
        "err:E_COMPONENT_NOT_ACTIVE",
        "a request before the retried activation must fail closed"
    );

    // The denial is still present, so the mirror retry must not activate.
    let mut warnings = Vec::new();
    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            warnings.push(format!("{plugin}: {reason}"));
        }),
        0,
        "the denial must prevent the retry from activating"
    );
    assert_eq!(
        fixture
            .runtime
            .plugin_host()
            .registry()
            .get(&id)
            .expect("denied retry keeps the registration")
            .state,
        PluginState::Registered
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("explicitly denied capabilities")),
        "the denied retry must be reported: {warnings:?}"
    );

    // Explicit user action clears the denial; authority still requires the
    // retried activation and does not appear with the clearance alone.
    assert!(
        fixture
            .runtime
            .plugin_host_mut()
            .grants_mut()
            .clear_cap_denial(&id, &network),
        "the explicit denial must be present before it is cleared"
    );
    match fixture
        .runtime
        .component_grant(RELOAD_PLUGIN, &fixture.v1_hash)
    {
        Err(ComponentHostError::PluginNotActive { .. }) => {}
        other => panic!("grant must stay unavailable until activation is retried, got {other:?}"),
    }
    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            panic!("mirror refused {plugin}: {reason}")
        }),
        1,
        "the repaired grant must reactivate the existing registration"
    );
    assert_eq!(
        fixture
            .runtime
            .plugin_host()
            .registry()
            .get(&id)
            .expect("retried generation registered")
            .state,
        PluginState::Activated
    );
    assert!(
        !fixture
            .runtime
            .component_grant(RELOAD_PLUGIN, &fixture.v1_hash)
            .expect("component grant available after the retry")
            .is_offline(),
        "the retried activation restores the granted network authority"
    );

    let ticket = queue_request(&mut fixture.plugins, RELOAD_PLUGIN);
    assert_eq!(
        poll_until_settled(
            &mut fixture.runtime,
            &mut fixture.plugins,
            RELOAD_PLUGIN,
            &ticket
        ),
        format!("ok:200:{URL}|grant:{EGRESS_HOST}:{EGRESS_PORT}"),
        "the request is authorized only after the retried activation"
    );

    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            panic!("mirror refused {plugin}: {reason}")
        }),
        0,
        "the retried generation is idempotent once Activated"
    );
}

/// Negative follow-up to bitty#2311 / bitty#2318: a plugin-level denial must
/// prevent the retry even when the manifest hash matches a `Registered`
/// entry, and the denial marker must not be cleared by re-inserting a grant.
#[test]
fn explicit_denial_prevents_retrying_a_registered_activation() {
    let mut fixture = reload_fixture("denied-retry");
    let id = fixture.id.clone();
    mirror_with_failed_activation(&mut fixture);
    fixture
        .runtime
        .insert_grant(GrantRecord::denied(id.clone(), fixture.v1_hash.clone(), 11));

    let mut warnings = Vec::new();
    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            warnings.push(format!("{plugin}: {reason}"));
        }),
        0,
        "an explicit denial must not be re-granted for a Registered generation"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("explicit policy-host denial")),
        "the denied retry must be reported: {warnings:?}"
    );
    assert!(
        fixture.runtime.plugin_host().registry().get(&id).is_none(),
        "a denied Registered generation must lose its registration, not activate"
    );
    assert!(
        fixture.runtime.plugin_host().grants().is_denied(&id),
        "the plugin-level denial marker must survive the mirror"
    );

    let ticket = queue_request(&mut fixture.plugins, RELOAD_PLUGIN);
    assert_eq!(
        poll_until_settled(
            &mut fixture.runtime,
            &mut fixture.plugins,
            RELOAD_PLUGIN,
            &ticket
        ),
        "err:E_COMPONENT_CAPABILITY_DENIED",
        "a denied generation must keep denying requests"
    );
}

/// B2 (bitty#2302 review): a generation that is no longer live loses its
/// mirrored policy-host authority on the next reconciliation.
#[test]
fn disposed_generation_loses_mirrored_authority() {
    let mut fixture = reload_fixture("disposed-reload");
    let id = fixture.id.clone();
    fixture.plugins.dispose(&id).expect("dispose v1");
    assert_eq!(fixture.plugins.state(&id), Some(&LifecycleState::Disposed));

    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            panic!("mirror refused {plugin}: {reason}")
        }),
        0,
        "a disposed generation adds no mirrored authority"
    );
    assert!(
        fixture.runtime.plugin_host().registry().get(&id).is_none(),
        "the disposed id must be removed from the policy host"
    );
    match fixture
        .runtime
        .component_grant(RELOAD_PLUGIN, &fixture.v1_hash)
    {
        Err(ComponentHostError::CapabilityDenied { reason, .. }) => assert!(
            reason.contains("plugin is not registered"),
            "unexpected denial reason: {reason}"
        ),
        other => panic!("the disposed generation must be refused, got {other:?}"),
    }
}

/// Follow-up to bitty#2323 (issue #2325): a same-hash policy-host entry that
/// was explicitly suspended is not a stale generation. A mirror run must not
/// replace, re-grant, or re-activate it; only an explicit resume clears the
/// suspension.
#[test]
fn suspended_generation_survives_a_mirror_run() {
    let mut fixture = reload_fixture("suspended-mirror");
    let id = fixture.id.clone();
    fixture
        .runtime
        .suspend_plugin(&id)
        .expect("suspend the mirrored generation");
    assert_eq!(
        fixture
            .runtime
            .plugin_host()
            .registry()
            .get(&id)
            .expect("suspended generation stays registered")
            .state,
        PluginState::Suspended
    );

    // Any re-grant would overwrite this marker with the mirror's `decided_at`
    // (0), so the marker proves the mirror left the grant record untouched.
    let regrant_marker = 9_999;
    let mut record = fixture
        .runtime
        .plugin_host()
        .grants()
        .get(&id)
        .cloned()
        .expect("the mirrored generation keeps its grant record");
    record.decided_at = regrant_marker;
    fixture
        .runtime
        .plugin_host_mut()
        .grants_mut()
        .insert(record);

    let mut warnings = Vec::new();
    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            warnings.push(format!("{plugin}: {reason}"));
        }),
        0,
        "a suspended generation is already mirrored and must not be re-activated"
    );
    assert!(
        warnings.is_empty(),
        "a suspended generation must be neither refused nor retried: {warnings:?}"
    );
    assert_eq!(
        fixture
            .runtime
            .plugin_host()
            .registry()
            .get(&id)
            .expect("the suspended generation must survive the mirror")
            .state,
        PluginState::Suspended,
        "the mirror must not replace the suspended entry"
    );
    assert_eq!(
        fixture
            .runtime
            .plugin_host()
            .grants()
            .get(&id)
            .expect("the grant record must survive the mirror")
            .decided_at,
        regrant_marker,
        "the mirror must not re-grant the suspended generation"
    );
    match fixture
        .runtime
        .component_grant(RELOAD_PLUGIN, &fixture.v1_hash)
    {
        Err(ComponentHostError::PluginNotActive { state, .. }) => {
            assert_eq!(state, "Suspended")
        }
        other => panic!("a suspended generation must fail closed, got {other:?}"),
    }

    let ticket = queue_request(&mut fixture.plugins, RELOAD_PLUGIN);
    assert_eq!(
        poll_until_settled(
            &mut fixture.runtime,
            &mut fixture.plugins,
            RELOAD_PLUGIN,
            &ticket
        ),
        "err:E_COMPONENT_NOT_ACTIVE",
        "a component request under a suspended generation must fail closed"
    );
    // `bitty extension list` reads this projection: the fail-closed request is
    // recorded as the stable `plugin_not_active` class, never as an activation.
    assert_eq!(
        read_activation(&fixture._scratch.path().join("extensions").join(ECHO)),
        ActivationState::Denied(ActivationDenial::PluginNotActive),
        "the extension-list projection must report the fail-closed denial"
    );

    // An explicit resume returns the policy-host entry to `Registered`; the
    // caller may then activate it without another mirror.
    fixture.runtime.resume_plugin(&id).expect("explicit resume");
    assert_eq!(
        fixture
            .runtime
            .plugin_host()
            .registry()
            .get(&id)
            .expect("resumed generation")
            .state,
        PluginState::Registered
    );
    fixture
        .runtime
        .activate_plugin(&id)
        .expect("explicit activation after resume");
    assert!(
        !fixture
            .runtime
            .component_grant(RELOAD_PLUGIN, &fixture.v1_hash)
            .expect("explicitly resumed generation is authorized")
            .is_offline(),
        "explicit resume restores component authority"
    );
    let ticket = queue_request(&mut fixture.plugins, RELOAD_PLUGIN);
    assert_eq!(
        poll_until_settled(
            &mut fixture.runtime,
            &mut fixture.plugins,
            RELOAD_PLUGIN,
            &ticket
        ),
        format!("ok:200:{URL}|grant:{EGRESS_HOST}:{EGRESS_PORT}"),
        "the explicitly resumed generation serves again"
    );

    assert_eq!(
        mirror_plugin_authority(&mut fixture.runtime, &fixture.plugins, |plugin, reason| {
            panic!("mirror refused {plugin}: {reason}")
        }),
        0,
        "the explicitly resumed generation is current and is not churned"
    );
}

/// Manifest-shaped types are not needed by the surface test except through
/// the parsed manifests; these stubs keep the host-service config minimal.
struct EmptySettings;

impl SettingsSource for EmptySettings {
    fn get(&self, _key: &str) -> Option<LuaValue> {
        None
    }
}

struct EmptySnapshot;

impl SnapshotSource for EmptySnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, bitty_lua::BridgeError> {
        Ok(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("zones", LuaValue::array(Vec::new())),
        ]))
    }
}
