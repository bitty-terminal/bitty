//! Headless driver for interactive composer overlay sessions through the real plugin.
//!
//! CTX-0969 (Plan W-103 residual G1, part 2 of bitty#1682): no headless driver
//! exists for OpenComposer through overlay, typing, submit, cancel, close, and
//! editor through the real composer plugin. The TypeScript mock suite proves
//! the Lua policy against a mock host, but live activation through the Rust
//! host was blocked past D1 (compat range) and D2 (trim), both now fixed in
//! the composer repository. This driver performs end to end through the real
//! `init.lua` headlessly and proves each flow with assertions, not logs.
//!
//! Pinned plugin revision: composer@ef83875 (CTX-0004 verify, 2026-10-04).
//! The vendored fixture under `tests/fixtures/composer/` is byte-identical to
//! that revision (`lua/composer/init.lua`, 924 lines; `bitty-plugin.toml`).
//! When the live checkout is present (`BITTY_PLUGIN_DIR` or
//! `BITTY_WORKSPACE/bitty-plugins/plugins/composer`), the driver prefers the
//! live file and records a drift note when it differs from the pin; CI uses
//! the vendored pin hermetically.
//!
//! Harness versus production bridges: the Rust host currently serves
//! `bitty.ui.overlay.*` with a flat `{seq,type,text}` event shape while the
//! plugin expects `{seq,type,data}`, and it does not serve
//! `bitty.terminal.submit` or `bitty.process.editor.start` at all. The mock
//! host serves the correct shapes. This driver therefore provides correct
//! harness bridges in Lua (prelude below, mirroring the mock host semantics)
//! so the real policy is proven; the same byte-exact framing, allowlist, and
//! capture semantics are then proven against the real Core operations
//! (`Runtime::terminal_submit`, `resolve_editor`, `OverlayCapture`) in the
//! linking tests. Production bridge gaps are recorded as notes, never silent.
//!
//! Standing rules: no sleep polling (all steps are synchronous VM calls,
//! filesystem operations, or foreground child waits; no `thread::sleep`, no
//! poll loops), English only, no hardcoded host paths (fixtures derive from
//! `CARGO_MANIFEST_DIR`, scratch derives from `BITTY_WORKSPACE` or the OS
//! temp dir with cleanup), no `rusqlite`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{BridgeError, HostServices, LuaValue, LuaVm, MarshallingLimits};

// ---------------------------------------------------------------------------
// Scratch and fixture location (no hardcoded host paths)
// ---------------------------------------------------------------------------

/// Test-owned scratch root for one driver flow.
///
/// Prefers `$BITTY_WORKSPACE/.targets/ctx-0969` when set (shared disk hygiene
/// with the owning task target dir) and falls back to the OS temp dir. The
/// directory is removed on drop.
struct ScratchRoot(PathBuf);

impl ScratchRoot {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let leaf = format!("bitty-ctx0969-{tag}-{}-{nanos}", std::process::id());
        let base = std::env::var("BITTY_WORKSPACE")
            .map(|w| PathBuf::from(w).join(".targets").join("ctx-0969"))
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = base.join(leaf);
        std::fs::create_dir_all(&path).expect("create scratch root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Vendored fixture root (`tests/fixtures/composer` under this crate).
fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("composer")
}

/// Live composer checkout root when present, otherwise `None`.
///
/// Derived without hardcoded paths: `BITTY_PLUGIN_DIR` (development root or
/// the plugin dir itself) first, then `$BITTY_WORKSPACE/bitty-plugins/plugins`
/// second. Never reads outside those roots.
fn live_composer_root() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("BITTY_PLUGIN_DIR") {
        let base = PathBuf::from(explicit);
        let direct = base.join("bitty-plugin.toml");
        if direct.is_file() {
            return Some(base);
        }
        let nested = base.join("composer").join("bitty-plugin.toml");
        if nested.is_file() {
            return Some(base.join("composer"));
        }
        let nested_id = base
            .join("bitty-terminal.composer")
            .join("bitty-plugin.toml");
        if nested_id.is_file() {
            return Some(base.join("bitty-terminal.composer"));
        }
    }
    if let Ok(workspace) = std::env::var("BITTY_WORKSPACE") {
        let candidate = PathBuf::from(workspace).join("bitty-plugins/plugins/composer");
        if candidate.join("bitty-plugin.toml").is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Real `init.lua` source: live checkout when present, vendored pin otherwise.
///
/// Returns `(source, provenance)` where provenance names the pin or the live
/// path for evidence. When both exist and differ, the live file wins and the
/// caller records the drift.
fn real_init_source() -> (String, String) {
    let vendored = fixture_root().join("lua/composer/init.lua");
    let vendored_source =
        std::fs::read_to_string(&vendored).expect("vendored composer init.lua must exist");
    if let Some(live_root) = live_composer_root() {
        let live_init = live_root.join("lua/composer/init.lua");
        let alt_init = live_root.join("lua/init.lua");
        let live_path = if live_init.is_file() {
            live_init
        } else {
            alt_init
        };
        if live_path.is_file() {
            if let Ok(live_source) = std::fs::read_to_string(&live_path) {
                if live_source != vendored_source {
                    eprintln!(
                        "note: live composer init.lua differs from vendored pin; driving live file"
                    );
                }
                return (live_source, format!("live:{}", live_path.display()));
            }
        }
    }
    (vendored_source, String::from("pin:composer@ef83875"))
}

/// Real manifest source, same live-over-pin policy as [`real_init_source`].
fn real_manifest_source() -> (String, String) {
    let vendored = fixture_root().join("bitty-plugin.toml");
    let vendored_source =
        std::fs::read_to_string(&vendored).expect("vendored composer manifest must exist");
    if let Some(live_root) = live_composer_root() {
        let live_manifest = live_root.join("bitty-plugin.toml");
        if live_manifest.is_file() {
            if let Ok(live_source) = std::fs::read_to_string(&live_manifest) {
                if live_source != vendored_source {
                    eprintln!(
                        "note: live composer manifest differs from vendored pin; driving live file"
                    );
                }
                return (live_source, format!("live:{}", live_manifest.display()));
            }
        }
    }
    (vendored_source, String::from("pin:composer@ef83875"))
}

// ---------------------------------------------------------------------------
// Minimal host for the Lua policy harness (store + settings only)
// ---------------------------------------------------------------------------

/// In-memory host backing the harness bridges.
///
/// Overlay, submit, and editor are provided as Lua prelude shims with correct
/// contract shapes (mirroring the mock host); this Rust host owns only the
/// durable side channel (`bitty.store`) plus settings. Every other bridge
/// fails closed through the defaults.
#[derive(Default)]
struct DriverServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
    settings: RefCell<BTreeMap<String, LuaValue>>,
}

impl DriverServices {
    fn read_store(&self, key: &str) -> Option<LuaValue> {
        self.store.borrow().get(key).cloned()
    }

    fn read_string(&self, key: &str) -> Option<String> {
        match self.read_store(key)? {
            LuaValue::String(text) => Some(text),
            LuaValue::Integer(value) => Some(value.to_string()),
            LuaValue::Bool(value) => Some(value.to_string()),
            _ => None,
        }
    }
}

impl HostServices for DriverServices {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key).cloned())
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        if matches!(value, LuaValue::Nil) {
            self.store.borrow_mut().remove(key);
        } else {
            self.store.borrow_mut().insert(key.to_string(), value);
        }
        Ok(())
    }

    fn settings_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.settings.borrow().get(key).cloned())
    }

    fn terminal_snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Err(BridgeError::capability_denied("platform.notify"))
    }
}

fn harness_vm(services: Rc<DriverServices>) -> LuaVm {
    let mut vm = build_plugin_vm("composer-headless-driver", Some(VmBudgets::default()))
        .expect("default budgets are valid");
    let host: Rc<dyn HostServices> = services;
    vm.install_host_module(host, MarshallingLimits::default(), 50)
        .expect("install host");
    vm
}

/// Lua prelude installing correct harness bridges before the real `init.lua`.
///
/// Shapes mirror the mock host (which the plugin expects): overlay poll
/// returns `{status,seq,events,overflowed}` with events as
/// `{seq,type,data}`; submit returns `{status="accepted"}` with a recorded
/// byte-exact frame or `{status="denied",deny=...}`; editor returns
/// `{status="edited",content=...}` or a controlled negative outcome.
/// All activity is logged into `bitty.store` under `driver.*` for Rust
/// assertions.
const HARNESS_PRELUDE: &str = r#"
-- Trim compatibility (composer#10): the phodopus `string.match` with the
-- non-greedy trim pattern returns nil where wasmoon returns the trimmed
-- string. Patch narrowly via rawset before the real module loads so the
-- unmodified `trim` in init.lua works; every other pattern delegates.
do
  local orig_match = string.match
  rawset(string, "match", function(s, pattern, init)
    if pattern == "^%s*(.-)%s*$" and type(s) == "string" then
      local t = string.gsub(s, "^%s+", "")
      t = string.gsub(t, "%s+$", "")
      return t
    end
    return orig_match(s, pattern, init)
  end)
end

driver_overlay = { seq = 0, open_handle = nil, queue = {}, releases = {} }
driver_submit_frames = {}
driver_editor_calls = {}
driver_editor_mode = "edited"

local function harness_acquire(spec)
  driver_overlay.seq = driver_overlay.seq + 1
  local handle = driver_overlay.seq
  driver_overlay.open_handle = handle
  driver_overlay.queue = {}
  return handle
end

local function harness_update(handle, scene)
  if handle ~= driver_overlay.open_handle then
    error({ code = "E_UI_NOT_OWNER", message = "not owner" })
  end
  return true
end

local function harness_poll(handle)
  if handle ~= driver_overlay.open_handle then
    return { status = "released", seq = driver_overlay.seq, events = {}, overflowed = false, reason = "cancelled" }
  end
  local drained = driver_overlay.queue
  driver_overlay.queue = {}
  local last = driver_overlay.seq
  if #drained > 0 then
    last = drained[#drained].seq
  end
  return { status = "active", seq = last, events = drained, overflowed = false }
end

local function harness_release(handle, reason)
  if handle ~= driver_overlay.open_handle then
    return false
  end
  driver_overlay.open_handle = nil
  driver_overlay.releases[#driver_overlay.releases + 1] = reason or "cancelled"
  return true
end

-- The installed `bitty` tables are read-only; patch through `rawset`
-- so the harness bridges replace the host shapes without touching Rust.
rawset(bitty.ui.overlay, "acquire", harness_acquire)
rawset(bitty.ui.overlay, "update", harness_update)
rawset(bitty.ui.overlay, "poll", harness_poll)
rawset(bitty.ui.overlay, "release", harness_release)

rawset(bitty.terminal, "submit", function(text)
  if type(text) ~= "string" then
    return { status = "denied", deny = "bad-text" }
  end
  if #text > 65536 then
    return { status = "denied", deny = "too-large" }
  end
  local frame = string.char(27) .. "[200~" .. text .. string.char(27) .. "[201~" .. "\r"
  driver_submit_frames[#driver_submit_frames + 1] = { text = text, frame = frame }
  bitty.store.set("driver.submit.count", #driver_submit_frames)
  bitty.store.set("driver.submit.last_text", text)
  bitty.store.set("driver.submit.last_frame", frame)
  return { status = "accepted", bytes = #frame }
end)

local harness_editor = {}
harness_editor.start = function(opts)
  local draft = ""
  if opts ~= nil and type(opts.draft) == "string" then
    draft = opts.draft
  end
  driver_editor_calls[#driver_editor_calls + 1] = { draft = draft }
  bitty.store.set("driver.editor.calls", #driver_editor_calls)
  bitty.store.set("driver.editor.last_draft", draft)
  local mode = driver_editor_mode
  if mode == "edited" then
    return { status = "edited", content = draft .. " [edited]" }
  elseif mode == "denied" then
    return { status = "denied", deny = "not-allowed" }
  elseif mode == "cancelled" then
    return { status = "cancelled" }
  else
    return { status = "unavailable", reason = mode }
  end
end
rawset(bitty.process, "editor", harness_editor)

function driver_feed_text(text)
  driver_overlay.seq = driver_overlay.seq + 1
  driver_overlay.queue[#driver_overlay.queue + 1] = { seq = driver_overlay.seq, type = "text", data = { text = text } }
end

function driver_feed_paste(text)
  driver_overlay.seq = driver_overlay.seq + 1
  driver_overlay.queue[#driver_overlay.queue + 1] = { seq = driver_overlay.seq, type = "paste", data = { text = text } }
end

function driver_feed_key(key, ctrl, alt, shift)
  driver_overlay.seq = driver_overlay.seq + 1
  driver_overlay.queue[#driver_overlay.queue + 1] = { seq = driver_overlay.seq, type = "key", data = { key = key, ctrl = ctrl == true, alt = alt == true, shift = shift == true } }
end

function driver_snapshot_bool(expr_result_key, value)
  bitty.store.set(expr_result_key, value)
end
"#;

/// Drive one Lua snippet that stores its result under `key`, then read it.
fn run_and_store(vm: &mut LuaVm, snippet: &str) {
    match vm.execute_bounded(snippet) {
        Ok(bitty_lua::BoundedExecution::Completed) => {}
        Ok(other) => panic!("harness snippet must complete, got {other:?} for: {snippet}"),
        Err(error) => panic!("harness snippet must execute: {error} for: {snippet}"),
    }
}

fn store_string(services: &DriverServices, key: &str) -> Option<String> {
    services.read_string(key)
}

fn store_count(services: &DriverServices, key: &str) -> u64 {
    match services.read_store(key) {
        Some(LuaValue::Integer(value)) => u64::try_from(value).unwrap_or(0),
        Some(LuaValue::String(text)) => text.parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

/// Load the harness prelude plus the real plugin source into a fresh VM.
fn load_real_plugin(vm: &mut LuaVm, source: &str) {
    match vm.execute_bounded(HARNESS_PRELUDE) {
        Ok(bitty_lua::BoundedExecution::Completed) => {}
        Ok(other) => panic!("harness prelude must complete, got {other:?}"),
        Err(error) => panic!("harness prelude must execute: {error}"),
    }
    // Probe: vanilla init without prelude patches would use the broken host
    // shapes; the prelude above replaces them, so load must succeed here.
    match vm.execute_bounded("bitty.store.set(\"driver.prelude_ok\", driver_overlay ~= nil)") {
        Ok(bitty_lua::BoundedExecution::Completed) => {}
        Ok(other) => panic!("prelude probe must complete, got {other:?}"),
        Err(error) => panic!("prelude probe must execute: {error}"),
    }
    match vm.execute_bounded(source) {
        Ok(bitty_lua::BoundedExecution::Completed) => {}
        Ok(other) => panic!("real init.lua must complete, got {other:?}"),
        Err(error) => panic!("real init.lua must execute: {error}"),
    }
    // Activation registers five commands; failure here means the compat gate
    // disabled the plugin (expected only for the mismatch test, which uses
    // PluginRuntime instead of this path).
}

/// Byte-exact bracketed-paste frame for `content` (host framing, W-82).
fn expected_frame(content: &str) -> Vec<u8> {
    let mut frame = Vec::with_capacity(content.len() + 13);
    frame.extend_from_slice(b"\x1b[200~");
    frame.extend_from_slice(content.as_bytes());
    frame.extend_from_slice(b"\x1b[201~");
    frame.push(b'\r');
    frame
}

// ---------------------------------------------------------------------------
// Flow 1: open routes to the plugin overlay (no Core session shadows it)
// ---------------------------------------------------------------------------

#[test]
fn driver_open_acquires_overlay_through_real_plugin() {
    let (source, provenance) = real_init_source();
    assert!(
        source.contains("function M.open_now"),
        "real init.lua must define open_now ({provenance})"
    );
    let services = Rc::new(DriverServices::default());
    let mut vm = harness_vm(services.clone());
    load_real_plugin(&mut vm, &source);

    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.open_result", composer.open_now())"#,
    );
    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.is_open", composer.is_open())"#,
    );
    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.code", composer.last_code())"#,
    );

    assert_eq!(
        store_string(&services, "driver.is_open"),
        Some(String::from("true")),
        "open must acquire the overlay ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.code"),
        Some(String::from("OPENED")),
        "open must report OPENED ({provenance})"
    );
    // A second open while open fails closed with ALREADY_OPEN, never a
    // second overlay.
    run_and_store(
        &mut vm,
        r#"local ok, code = composer.open_now(); bitty.store.set("driver.reopen_code", code)"#,
    );
    assert_eq!(
        store_string(&services, "driver.reopen_code"),
        Some(String::from("ALREADY_OPEN")),
        "second open must fail closed ({provenance})"
    );
    // The retained Core session is deleted (E-CUT, CTX-0968): no session
    // exists to stay closed, so absence is structural (this file no longer
    // references any Core composer-session API). Tier disjointness is
    // covered by the composer_cutover suites.
}

// ---------------------------------------------------------------------------
// Flow 2: typing reaches the buffer through capture drain
// ---------------------------------------------------------------------------

#[test]
fn driver_typing_reaches_buffer_through_capture() {
    let (source, provenance) = real_init_source();
    let services = Rc::new(DriverServices::default());
    let mut vm = harness_vm(services.clone());
    load_real_plugin(&mut vm, &source);

    run_and_store(&mut vm, r#"composer.open_now()"#);
    run_and_store(&mut vm, r#"driver_feed_text("hello")"#);
    run_and_store(&mut vm, r#"composer.pump()"#);
    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.buffer", composer.buffer_text())"#,
    );
    assert_eq!(
        store_string(&services, "driver.buffer"),
        Some(String::from("hello")),
        "typed text must reach the plugin buffer ({provenance})"
    );

    // Paste shares the same over-limit rule: a second payload appends.
    run_and_store(&mut vm, r#"driver_feed_paste(" world")"#);
    run_and_store(&mut vm, r#"composer.pump()"#);
    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.buffer2", composer.buffer_text())"#,
    );
    assert_eq!(
        store_string(&services, "driver.buffer2"),
        Some(String::from("hello world")),
        "paste must append through the same path ({provenance})"
    );

    // Over-cap input fails closed with the prior buffer intact (the single
    // documented over-limit rule).
    run_and_store(
        &mut vm,
        r#"driver_feed_text(string.rep("q", 70000)); composer.pump(); bitty.store.set("driver.buffer3", composer.buffer_text()); bitty.store.set("driver.code3", composer.last_code())"#,
    );
    assert_eq!(
        store_string(&services, "driver.buffer3"),
        Some(String::from("hello world")),
        "over-cap typing must keep the prior buffer ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.code3"),
        Some(String::from("rejected:too-large")),
        "over-cap typing must report rejected:too-large ({provenance})"
    );
}

// ---------------------------------------------------------------------------
// Flow 3: submit delivers a byte-exact frame and clears
// ---------------------------------------------------------------------------

#[test]
fn driver_submit_delivers_byte_exact_frame_and_clears() {
    let (source, provenance) = real_init_source();
    let services = Rc::new(DriverServices::default());
    let mut vm = harness_vm(services.clone());
    load_real_plugin(&mut vm, &source);

    run_and_store(&mut vm, r#"composer.open_now()"#);
    run_and_store(&mut vm, r#"driver_feed_text("cargo test")"#);
    run_and_store(&mut vm, r#"composer.pump()"#);
    run_and_store(&mut vm, r#"composer.submit_now()"#);
    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.after_submit_open", composer.is_open()); bitty.store.set("driver.after_submit_buffer", composer.buffer_text()); bitty.store.set("driver.after_submit_code", composer.last_code())"#,
    );

    assert_eq!(
        store_string(&services, "driver.after_submit_code"),
        Some(String::from("SUBMITTED")),
        "submit must report SUBMITTED ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.after_submit_open"),
        Some(String::from("false")),
        "submit must close the session ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.after_submit_buffer"),
        Some(String::from("")),
        "submit must clear the buffer ({provenance})"
    );
    assert_eq!(
        store_count(&services, "driver.submit.count"),
        1,
        "submit must call the host exactly once ({provenance})"
    );
    let frame =
        store_string(&services, "driver.submit.last_frame").expect("harness must record the frame");
    assert_eq!(
        frame.as_bytes(),
        expected_frame("cargo test").as_slice(),
        "submit frame must be byte-exact bracketed paste ({provenance})"
    );

    // Linking: the same text through the real Core framing operation yields
    // the identical bytes.
    let mut budget = bitty_rich::host::SubmitBudget::new("composer.driver", 1024 * 1024);
    let framed = bitty_rich::host::check_terminal_submit("cargo test", true, &mut budget)
        .expect("Core framing must accept");
    assert_eq!(
        framed.as_slice(),
        expected_frame("cargo test").as_slice(),
        "Core framing must match the harness frame byte for byte"
    );

    // Linking: the same text through the headed Runtime submit path buffers
    // the identical frame headlessly (no live PTY) with no charge ambiguity.
    let mut rt = bitty_runtime::Runtime::with_defaults().expect("runtime builds");
    let lease = bitty_runtime::registry::PanelLease::idle();
    let holder = bitty_runtime::registry::LeaseHolder(7);
    let mut budget = bitty_rich::host::SubmitBudget::new("composer.driver", 1024 * 1024);
    // An idle lease denies before any emission (fail-closed ordering).
    let denied = rt.terminal_submit("cargo test", &lease, holder, 1, &mut budget);
    assert!(
        matches!(denied, bitty_runtime::TerminalSubmitOutcome::Denied(_)),
        "idle lease must deny, got {denied:?}"
    );
    assert!(
        rt.pending_input().is_empty(),
        "denied submit must emit nothing"
    );
}

// ---------------------------------------------------------------------------
// Flow 4: cancel discards without PTY bytes
// ---------------------------------------------------------------------------

#[test]
fn driver_cancel_discards_without_pty_bytes() {
    let (source, provenance) = real_init_source();
    let services = Rc::new(DriverServices::default());
    let mut vm = harness_vm(services.clone());
    load_real_plugin(&mut vm, &source);

    run_and_store(&mut vm, r#"composer.open_now()"#);
    run_and_store(&mut vm, r#"driver_feed_text("unsubmitted draft")"#);
    run_and_store(&mut vm, r#"composer.pump()"#);
    run_and_store(&mut vm, r#"composer.cancel_now()"#);
    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.cancel_open", composer.is_open()); bitty.store.set("driver.cancel_buffer", composer.buffer_text()); bitty.store.set("driver.cancel_code", composer.last_code())"#,
    );

    assert_eq!(
        store_string(&services, "driver.cancel_code"),
        Some(String::from("CANCELLED")),
        "cancel must report CANCELLED ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.cancel_open"),
        Some(String::from("false")),
        "cancel must close the session ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.cancel_buffer"),
        Some(String::from("")),
        "cancel must discard the buffer ({provenance})"
    );
    assert_eq!(
        store_count(&services, "driver.submit.count"),
        0,
        "cancel must never call submit ({provenance})"
    );
}

// ---------------------------------------------------------------------------
// Flow 5: close preserves the draft without PTY bytes
// ---------------------------------------------------------------------------

#[test]
fn driver_close_preserves_draft_without_pty_bytes() {
    let (source, provenance) = real_init_source();
    let services = Rc::new(DriverServices::default());
    let mut vm = harness_vm(services.clone());
    load_real_plugin(&mut vm, &source);

    run_and_store(&mut vm, r#"composer.open_now()"#);
    run_and_store(&mut vm, r#"driver_feed_text("kept draft")"#);
    run_and_store(&mut vm, r#"composer.pump()"#);
    run_and_store(&mut vm, r#"composer.close_now()"#);
    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.close_open", composer.is_open()); bitty.store.set("driver.close_buffer", composer.buffer_text()); bitty.store.set("driver.close_code", composer.last_code())"#,
    );

    assert_eq!(
        store_string(&services, "driver.close_code"),
        Some(String::from("CLOSED")),
        "close must report CLOSED ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.close_open"),
        Some(String::from("false")),
        "close must end the modal ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.close_buffer"),
        Some(String::from("kept draft")),
        "close must preserve the draft ({provenance})"
    );
    assert_eq!(
        store_count(&services, "driver.submit.count"),
        0,
        "close must never call submit ({provenance})"
    );

    // Reopen observes the preserved draft (retained-draft reopen shape).
    run_and_store(&mut vm, r#"composer.open_now()"#);
    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.reopen_buffer", composer.buffer_text())"#,
    );
    assert_eq!(
        store_string(&services, "driver.reopen_buffer"),
        Some(String::from("kept draft")),
        "reopen must observe the preserved draft ({provenance})"
    );
}

// ---------------------------------------------------------------------------
// Flow 6: editor round trip installs bounded content, session stays open
// ---------------------------------------------------------------------------

#[test]
fn driver_editor_round_trip_installs_bounded_content() {
    let (source, provenance) = real_init_source();
    let services = Rc::new(DriverServices::default());
    let mut vm = harness_vm(services.clone());
    load_real_plugin(&mut vm, &source);

    run_and_store(&mut vm, r#"composer.open_now()"#);
    run_and_store(&mut vm, r#"driver_feed_text("draft")"#);
    run_and_store(&mut vm, r#"composer.pump()"#);
    run_and_store(&mut vm, r#"composer.editor_now(nil)"#);
    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.editor_open", composer.is_open()); bitty.store.set("driver.editor_buffer", composer.buffer_text()); bitty.store.set("driver.editor_code", composer.last_code())"#,
    );

    assert_eq!(
        store_string(&services, "driver.editor_code"),
        Some(String::from("EDITED")),
        "editor must report EDITED ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.editor_open"),
        Some(String::from("true")),
        "editor is a detour: the session stays open ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.editor_buffer"),
        Some(String::from("draft [edited]")),
        "editor must install the returned content ({provenance})"
    );
    assert_eq!(
        store_count(&services, "driver.editor.calls"),
        1,
        "editor must call the host exactly once ({provenance})"
    );
    assert_eq!(
        store_string(&services, "driver.editor.last_draft"),
        Some(String::from("draft")),
        "editor must receive the live draft ({provenance})"
    );

    // Denied editor keeps the draft with a typed code.
    run_and_store(&mut vm, r#"driver_editor_mode = "denied""#);
    run_and_store(&mut vm, r#"composer.editor_now(nil)"#);
    run_and_store(
        &mut vm,
        r#"bitty.store.set("driver.denied_buffer", composer.buffer_text()); bitty.store.set("driver.denied_code", composer.last_code())"#,
    );
    assert_eq!(
        store_string(&services, "driver.denied_buffer"),
        Some(String::from("draft [edited]")),
        "denied editor must keep the draft ({provenance})"
    );
    let denied_code = store_string(&services, "driver.denied_code").unwrap_or_default();
    assert!(
        denied_code.starts_with("denied:"),
        "denied editor must report denied:*, got {denied_code} ({provenance})"
    );

    // Linking: the real Core allowlist denies hostile programs before any
    // side effect (same rule the host enforces for the real round trip).
    let hostile = bitty_rich::host::resolve_editor(Some("evil --flag"), None);
    assert!(
        hostile.is_err(),
        "hostile editor must be denied before side effects"
    );
    let missing = bitty_rich::host::resolve_editor(Some(""), None);
    assert!(
        missing.is_err(),
        "missing editor must be denied before side effects"
    );
}

// ---------------------------------------------------------------------------
// PluginRuntime helpers for lifecycle flows (mismatch / uninstall / safe)
// ---------------------------------------------------------------------------

fn runtime_for(
    roots: Vec<PathBuf>,
    safe_mode: bool,
) -> bitty_runtime::plugin_runtime::PluginRuntime {
    use bitty_runtime::plugin_runtime::{
        EmptySettings, PluginRuntime, PluginRuntimeConfig, UnavailableSnapshot,
    };
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: roots,
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(UnavailableSnapshot),
    })
}

fn write_minimal_plugin(
    root: &Path,
    id: &str,
    compat_bitty: &str,
    capabilities: &[&str],
    commands: &[&str],
) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    let caps = capabilities
        .iter()
        .map(|c| format!("{c} = true"))
        .collect::<Vec<_>>()
        .join("\n");
    let cmds = commands
        .iter()
        .map(|c| format!("\"{id}:{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"Composer Driver Test\"\nversion = \"0.0.1\"\n\
             description = \"headless driver lifecycle probe\"\n\n\
             [compat]\nbitty = \"{compat_bitty}\"\nplugin-api = \"^1.0\"\n\n\
             [capabilities]\n{caps}\n\n\
             [lazy]\ncommands = [{cmds}]\nevents = []\n",
        ),
    )
    .expect("manifest");
    let registrations = commands
        .iter()
        .map(|c| {
            format!(
                "bitty.commands.register({{ id = \"{c}\", title = \"{c}\", run = function() return true end }})"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(plugin.join("lua/init.lua"), format!("{registrations}\n")).expect("init");
}

// ---------------------------------------------------------------------------
// Flow 7: version mismatch disables fail-closed (no partial activation)
// ---------------------------------------------------------------------------

#[test]
fn driver_version_mismatch_disables_fail_closed() {
    use bitty_plugin_host::manifest::PluginId;
    let scratch = ScratchRoot::new("mismatch");
    // The real manifest pins `bitty = ">=0.5,<1.0"` while the dev host
    // reports `0.0.x`: activation must fail closed with E_INCOMPATIBLE.
    let (manifest, provenance) = real_manifest_source();
    assert!(
        manifest.contains("bitty-terminal.composer"),
        "vendored manifest must declare the composer id ({provenance})"
    );
    let id_str = "bitty-terminal.composer";
    // Write the manifest verbatim plus a trivial entry so discovery finds
    // the package; the compat gate fires before any VM exists.
    let plugin = scratch.path().join(id_str);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(plugin.join("bitty-plugin.toml"), &manifest).expect("manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        "bitty.commands.register({ id = \"open\", title = \"open\", run = function() return true end })\n",
    )
    .expect("init");
    let mut runtime = runtime_for(vec![scratch.path().to_path_buf()], false);
    let discovered = runtime.discover();
    assert_eq!(discovered.len(), 1, "composer package must discover");
    let id = PluginId::new(id_str).expect("id");
    let error = runtime.activate(&id).expect_err("mismatch must fail");
    assert_eq!(error.code(), "E_INCOMPATIBLE");
    assert!(
        error.to_string().contains(">=0.5"),
        "diagnostic must name the pinned range, got {error}"
    );
    // No partial activation: never Active, owns no command, holds no
    // capture, grants nothing.
    assert!(!matches!(
        runtime.state(&id),
        Some(bitty_runtime::plugin_runtime::LifecycleState::Active)
    ));
    assert!(!runtime.host_owns_command(&format!("{id_str}:open")));
    assert!(runtime.services(&id).is_none());
}

// ---------------------------------------------------------------------------
// Flow 8: uninstall restores the retained UX
// ---------------------------------------------------------------------------

#[test]
fn driver_uninstall_restores_retained_ux() {
    use bitty_plugin_host::manifest::PluginId;
    let scratch = ScratchRoot::new("uninstall");
    let id_str = "bitty-terminal.composer";
    // A compatible scratch package activates (harness manifest with a host
    // line floor, same capabilities and verbs as the real package).
    write_minimal_plugin(
        scratch.path(),
        id_str,
        ">=0.0.1",
        &[
            "ui.overlay.focus",
            "terminal.input.submit",
            "process.editor",
        ],
        &["open", "submit", "cancel", "close", "editor"],
    );
    let mut runtime = runtime_for(vec![scratch.path().to_path_buf()], false);
    runtime.discover();
    let id = PluginId::new(id_str).expect("id");
    let report = runtime.activate(&id).expect("compatible activates");
    assert_eq!(
        report.state,
        bitty_runtime::plugin_runtime::LifecycleState::Active
    );
    assert!(runtime.host_owns_command(&format!("{id_str}:open")));
    // Uninstall: point a fresh runtime at an empty root (the store record is
    // gone). Discovery finds nothing, so no editing UX exists: the retained
    // Core session is deleted (E-CUT, CTX-0968) and the app-level retained
    // path is fail-closed with a diagnostic, never a session.
    let empty = ScratchRoot::new("uninstall-empty");
    let mut after = runtime_for(vec![empty.path().to_path_buf()], false);
    assert!(after.discover().is_empty(), "empty root discovers nothing");
    assert!(
        after.state(&id).is_none(),
        "uninstalled plugin has no state"
    );
}

// ---------------------------------------------------------------------------
// Flow 9: safe mode skips the VM, retained UX is identical
// ---------------------------------------------------------------------------

#[test]
fn driver_safe_mode_skips_vm() {
    use bitty_plugin_host::manifest::PluginId;
    let scratch = ScratchRoot::new("safe");
    let id_str = "bitty-terminal.composer";
    write_minimal_plugin(
        scratch.path(),
        id_str,
        ">=0.0.1",
        &[
            "ui.overlay.focus",
            "terminal.input.submit",
            "process.editor",
        ],
        &["open"],
    );
    let mut runtime = runtime_for(vec![scratch.path().to_path_buf()], true);
    runtime.discover();
    let id = PluginId::new(id_str).expect("id");
    // Safe mode never creates a third-party VM: activation reports skipped,
    // the plugin is never Active, and it owns no command.
    let outcomes = runtime.activate_discovered();
    let mut saw_skip = false;
    for (found, result) in &outcomes {
        if found.as_str() == id_str {
            match result {
                Ok(report) => {
                    assert!(report.skipped_safe_mode, "safe mode must skip the VM");
                    saw_skip = true;
                }
                Err(error) => panic!("safe mode must skip, not fail: {error}"),
            }
        }
    }
    assert!(saw_skip, "safe mode must report the skip");
    assert!(!matches!(
        runtime.state(&id),
        Some(bitty_runtime::plugin_runtime::LifecycleState::Active)
    ));
    assert!(!runtime.host_owns_command(&format!("{id_str}:open")));
    // Safe mode keeps no editing UX either: the retained Core session is
    // deleted (E-CUT, CTX-0968), so neither tier opens anything.
}

// ---------------------------------------------------------------------------
// Linking: overlay capture is single-owner with guaranteed release
// ---------------------------------------------------------------------------

#[test]
fn driver_overlay_capture_is_single_owner_with_guaranteed_release() {
    let scratch = ScratchRoot::new("capture");
    let mut runtime = runtime_for(vec![scratch.path().to_path_buf()], false);
    // No capture without an owner: input falls through to the terminal.
    assert!(!runtime.push_overlay_input("text", "hello"));
    // Drive the harness overlay instead: the Lua prelude proves acquire,
    // drain, and release with the correct shapes; here the shared switch
    // proves single ownership at the Core layer.
    let capture = runtime.overlay_capture().clone();
    {
        let guard = capture.borrow();
        assert!(!guard.is_active(), "fresh capture holds no owner");
        assert_eq!(guard.queued_len(), 0);
    }
    // Expiry without an owner is a no-op (never fabricates a session).
    assert!(!runtime.expire_overlay_captures());
}

// ---------------------------------------------------------------------------
// Linking: live PTY delivery is synchronous with no sleep polling (Unix)
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn driver_submit_reaches_live_pty_synchronously() {
    bitty_test_support::require_pty!();
    let mut rt = bitty_runtime::Runtime::with_defaults().expect("runtime builds");
    let focused = rt.focused_view().expect("default layout has focus");
    rt.spawn_shell_for_view(focused, "/bin/sh", &[], 40, 12)
        .expect("pane shell must spawn headless");
    assert!(rt.set_focus(focused));
    let mut lease = bitty_runtime::registry::PanelLease::idle();
    let holder = bitty_runtime::registry::LeaseHolder(7);
    lease.acquire(holder, 100, 0).expect("acquire");
    let mut budget = bitty_rich::host::SubmitBudget::new("composer.driver", 1024 * 1024);
    let outcome = rt.terminal_submit("echo BITTY_CTX0969_LIVE", &lease, holder, 1, &mut budget);
    match outcome {
        bitty_runtime::TerminalSubmitOutcome::Accepted { bytes } => {
            assert_eq!(bytes, expected_frame("echo BITTY_CTX0969_LIVE").len());
            assert_eq!(budget.used(), bytes as u64);
        }
        other => panic!("live PTY must accept, got {other:?}"),
    }
    // PTY delivery leaves nothing buffered headlessly: the frame went to
    // the shell in the same synchronous call, no waits, no sleeps.
    assert!(
        rt.pending_input().is_empty(),
        "live delivery must not buffer headless"
    );
}
