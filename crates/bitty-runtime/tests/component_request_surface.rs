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
use bitty_plugin_host::{GrantRecord, PluginId};
use bitty_runtime::Runtime;
use bitty_runtime::component::{ComponentWarnSink, executable_file_name};
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
const POLL_BUDGET: Duration = Duration::from_millis(10);
const POLL_ATTEMPTS: usize = 400;

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
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let capabilities = capability
        .map(|capability| format!("\"{capability}\" = true\n"))
        .unwrap_or_default();
    let network = egress
        .map(|host| format!("\n[[network.egress]]\nhost = \"{host}\"\nports = [{EGRESS_PORT}]\n"))
        .unwrap_or_default();
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Component Surface Test"
version = "0.1.0"
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
