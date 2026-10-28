//! Plugin-runtime publication for the accepted `bitty.debug` inspect surface.
//!
//! Issue #2773: the IPC dispatcher registered the plugin-runtime methods but
//! every one of them failed closed with `capability`/`PluginRuntimeUnavailable`
//! because the server slice had no backend. The runtime owns the sanitized
//! lifecycle snapshot ([`PluginRuntime::debug_plugins`]); this module publishes
//! an owned copy into a process-wide store that the connection threads can
//! read, exactly like the existing grid/frame publications in `bitty-ipc`.
//!
//! Only two accepted v1 methods are backed here: `listPlugins` and
//! `getPlugin`. Their accepted result shape is metadata (`id`, `version`,
//! `generation`, `state`, `manifestHash`, `capabilities`); this module never
//! reads a settings value, store body, terminal content, queue, budget, or VM
//! handle, and registers no command, trace, or control method. Every call is
//! still gated by the existing debug scope check, so the publication adds no
//! authority.
//!
//! When no runtime is attached (`--safe` with no store, or a plugin-less run)
//! the methods keep the pre-existing typed `PluginRuntimeUnavailable` verdict
//! instead of claiming an empty registry.

use std::sync::{Mutex, OnceLock};

use bitty_ipc::Scope;
use bitty_ipc::devtools::{DevtoolsRequest, Dispatcher, HandlerError, ServeContext};
use bitty_runtime::plugin_runtime::PluginRuntime;
use bitty_runtime::plugin_runtime::debug::DebugPlugin;

/// Maximum `pluginId` bytes (accepted client bound).
const MAX_PLUGIN_ID_BYTES: usize = 128;

/// Published read-only plugin rows plus whether a runtime is attached.
#[derive(Default)]
struct PublishedPlugins {
    attached: bool,
    rows: Vec<DebugPlugin>,
}

fn store() -> &'static Mutex<PublishedPlugins> {
    static STORE: OnceLock<Mutex<PublishedPlugins>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(PublishedPlugins::default()))
}

/// Serialize tests that mutate the process-wide publication.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock_store() -> std::sync::MutexGuard<'static, PublishedPlugins> {
    store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Publish the attached runtime's sanitized rows (or detach the surface).
///
/// `Some(runtime)` marks the backend attached even when the runtime currently
/// holds zero plugins (`listPlugins` then returns an empty array). `None`
/// restores the typed unavailable verdict. The runtime stays on its owning
/// thread; only owned metadata crosses this boundary.
pub(crate) fn publish(runtime: Option<&PluginRuntime>) {
    let mut published = lock_store();
    match runtime {
        Some(runtime) => {
            published.attached = true;
            published.rows = runtime.debug_plugins();
        }
        None => {
            published.attached = false;
            published.rows.clear();
        }
    }
}

/// Replace the publication from explicit rows (tests and future hosts).
#[cfg(test)]
pub(crate) fn publish_rows(rows: Vec<DebugPlugin>) {
    let mut published = lock_store();
    published.attached = true;
    published.rows = rows;
}

/// Register the live plugin-runtime inspect handlers on `dispatcher`.
///
/// Registration is additive and overrides only the two fail-closed stubs; the
/// trace and control methods keep their existing behavior.
///
/// # Errors
///
/// Returns the dispatcher's method-validation error (a programming error for
/// these statically valid names).
pub(crate) fn register(dispatcher: &mut Dispatcher) -> Result<(), bitty_ipc::IpcError> {
    dispatcher.register(
        bitty_ipc::devtools::METHOD_LIST_PLUGINS,
        handle_list_plugins,
    )?;
    dispatcher.register(bitty_ipc::devtools::METHOD_GET_PLUGIN, handle_get_plugin)?;
    Ok(())
}

fn require_inspect_scope(context: &ServeContext, method: &str) -> Result<(), HandlerError> {
    if context.has_any_scope(&[Scope::DebugInspect, Scope::DebugTrace, Scope::DebugControl]) {
        return Ok(());
    }
    Err(HandlerError::new(
        "scope",
        "ScopeDenied",
        format!("permission denied: scope 'debug.inspect' denied for {method} (needs elevation)"),
    ))
}

fn unavailable(method: &str) -> HandlerError {
    HandlerError::new(
        "capability",
        "PluginRuntimeUnavailable",
        format!(
            "{method}: plugin runtime not attached to this IPC server slice (no registry, budgets, queues, or handle tables; wire bitty-plugin-host/bitty-lua for live data)"
        ),
    )
}

/// Find the raw value span after a top-level `"key":` (caller decides shape).
fn top_value_span<'a>(params: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let pos = params.find(needle.as_str())?;
    let after_key = &params[pos + needle.len()..];
    let colon = after_key.find(':')?;
    Some(after_key[colon + 1..].trim_start())
}

/// Extract a top-level JSON string field with escape decoding; `None` when
/// the key is absent or the value is not a JSON string.
fn top_string(params: &str, key: &str) -> Option<String> {
    let mut value = top_value_span(params, key)?;
    if !value.starts_with('"') {
        return None;
    }
    value = &value[1..];
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000c}'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code)?);
                }
                _ => return None,
            },
            ch => out.push(ch),
        }
    }
    None
}

/// Extract a top-level unsigned integer field; `None` for strings/objects/
/// arrays/non-digits.
fn top_uint(params: &str, key: &str) -> Option<u64> {
    let value = top_value_span(params, key)?;
    if value.starts_with('"') || value.starts_with('{') || value.starts_with('[') {
        return None;
    }
    let digits_len = value.bytes().take_while(u8::is_ascii_digit).count();
    if digits_len == 0 || digits_len > 20 {
        return None;
    }
    value[..digits_len].parse().ok()
}

/// Whether `params` carries an explicit JSON `null` for `key`.
fn top_is_null(params: &str, key: &str) -> bool {
    top_value_span(params, key).is_some_and(|value| value.starts_with("null"))
}

fn validate_plugin_id(raw: &str) -> Result<(), String> {
    if raw.is_empty() || raw.len() > MAX_PLUGIN_ID_BYTES || raw.contains('\0') {
        return Err(format!(
            "pluginId must be 1..={MAX_PLUGIN_ID_BYTES} bytes, no NUL"
        ));
    }
    Ok(())
}

fn validate_generation(value: u64) -> Result<(), String> {
    if value < 1 {
        return Err("generation must be >= 1".to_string());
    }
    Ok(())
}

/// Parse the optional `generation` filter for `listPlugins`.
fn list_generation(params: &str) -> Result<Option<u64>, HandlerError> {
    if !params.trim_start().starts_with('{') {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            "listPlugins requires an object params".to_string(),
        ));
    }
    if top_is_null(params, "generation") {
        return Ok(None);
    }
    if let Some(generation) = top_uint(params, "generation") {
        validate_generation(generation)
            .map_err(|reason| HandlerError::new("usage", "InvalidParams", reason))?;
        return Ok(Some(generation));
    }
    if top_value_span(params, "generation").is_some() {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            "generation must be null or an integer >= 1".to_string(),
        ));
    }
    Ok(None)
}

/// Map the runtime's stable lowercase lifecycle label to the accepted
/// platform lifecycle vocabulary (`Declared -> ... -> Disposed`).
fn wire_state(label: &str) -> &'static str {
    match label {
        "unloaded" => "Declared",
        "loading" => "Resolved",
        "activating" => "Registered",
        "active" => "Activated",
        "suspended" => "Suspended",
        "disposing" | "disposed" | "failed" => "Disposed",
        _ => "Disposed",
    }
}

fn write_row(out: &mut String, row: &DebugPlugin) {
    out.push_str("{\"id\":\"");
    out.push_str(&crate::inspect::json_escape(&row.id));
    out.push_str("\",\"version\":\"");
    out.push_str(&crate::inspect::json_escape(&row.version));
    out.push_str("\",\"generation\":");
    out.push_str(&row.generation.to_string());
    out.push_str(",\"state\":\"");
    out.push_str(wire_state(row.state));
    out.push_str("\",\"manifestHash\":\"sha256:");
    out.push_str(&crate::inspect::json_escape(&row.manifest_hash));
    out.push_str("\",\"capabilities\":[");
    for (index, capability) in row.capabilities.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&crate::inspect::json_escape(capability));
        out.push('"');
    }
    out.push_str("]}");
}

/// `bitty.debug/listPlugins`: the published rows, optionally generation
/// filtered.
pub(crate) fn handle_list_plugins(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    require_inspect_scope(context, bitty_ipc::devtools::METHOD_LIST_PLUGINS)?;
    let params = request.params_raw.as_deref().unwrap_or("{}");
    let generation = list_generation(params)?;
    let rows = {
        let published = lock_store();
        if !published.attached {
            return Err(unavailable(bitty_ipc::devtools::METHOD_LIST_PLUGINS));
        }
        published.rows.clone()
    };
    let mut out = String::from("{\"plugins\":[");
    let mut first = true;
    for row in rows {
        if generation.is_some_and(|wanted| u64::from(row.generation) != wanted) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        write_row(&mut out, &row);
    }
    out.push_str("]}");
    Ok(out)
}

/// `bitty.debug/getPlugin`: one published row, or `null` for an unknown id.
pub(crate) fn handle_get_plugin(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    require_inspect_scope(context, bitty_ipc::devtools::METHOD_GET_PLUGIN)?;
    let params = request.params_raw.as_deref().unwrap_or("{}");
    let plugin_id = top_string(params, "pluginId").ok_or_else(|| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "getPlugin requires pluginId".to_string(),
        )
    })?;
    validate_plugin_id(&plugin_id)
        .map_err(|reason| HandlerError::new("usage", "InvalidParams", reason))?;
    let row = {
        let published = lock_store();
        if !published.attached {
            return Err(unavailable(bitty_ipc::devtools::METHOD_GET_PLUGIN));
        }
        published
            .rows
            .iter()
            .find(|row| row.id == plugin_id)
            .cloned()
    };
    match row {
        Some(row) => {
            let mut out = String::new();
            write_row(&mut out, &row);
            Ok(out)
        }
        None => Ok("null".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, generation: u32, state: &'static str) -> DebugPlugin {
        DebugPlugin {
            id: id.to_string(),
            version: "0.1.0".to_string(),
            state,
            generation,
            manifest_hash: "a".repeat(64),
            capabilities: vec!["ui.rich".to_string()],
        }
    }

    #[test]
    fn published_rows_are_served_and_generation_filtered() {
        let _serial = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publish_rows(vec![
            row("xuepoo.one", 1, "active"),
            row("xuepoo.two", 2, "suspended"),
        ]);
        let mut dispatcher = Dispatcher::with_test_mode();
        register(&mut dispatcher).expect("register");
        let server =
            bitty_ipc::devtools::ServerInfo::new("test".into(), "test.sock".into(), 80, 24);
        let context =
            ServeContext::with_granted(&server, bitty_ipc::ScopeSet::single(Scope::DebugInspect));
        let request = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/listPlugins","version":"1.0","params":{"generation":2}}"#,
        )
        .expect("request");
        assert!(dispatcher.contains(bitty_ipc::devtools::METHOD_LIST_PLUGINS));
        let result = handle_list_plugins(&context, &request).expect("served");
        assert!(result.contains("\"plugins\":["), "{result}");
        assert!(result.contains("xuepoo.two"), "{result}");
        assert!(!result.contains("xuepoo.one"), "{result}");
        assert!(result.contains("\"state\":\"Suspended\""), "{result}");
        assert!(result.contains("\"manifestHash\":\"sha256:"), "{result}");
        publish(None);
    }

    #[test]
    fn detached_store_keeps_the_typed_unavailable_verdict() {
        let _serial = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publish(None);
        let mut dispatcher = Dispatcher::with_test_mode();
        register(&mut dispatcher).expect("register");
        let server =
            bitty_ipc::devtools::ServerInfo::new("test".into(), "test.sock".into(), 80, 24);
        let context =
            ServeContext::with_granted(&server, bitty_ipc::ScopeSet::single(Scope::DebugInspect));
        let request = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/listPlugins","version":"1.0"}"#,
        )
        .expect("request");
        let error = handle_list_plugins(&context, &request).expect_err("detached");
        assert_eq!(error.category, "capability");
        assert_eq!(error.code, "PluginRuntimeUnavailable");
    }

    #[test]
    fn inspect_scope_is_required_and_params_fail_closed() {
        let _serial = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publish_rows(vec![row("xuepoo.one", 1, "active")]);
        let mut dispatcher = Dispatcher::with_test_mode();
        register(&mut dispatcher).expect("register");
        let server =
            bitty_ipc::devtools::ServerInfo::new("test".into(), "test.sock".into(), 80, 24);
        let empty = ServeContext::with_granted(&server, bitty_ipc::ScopeSet::new());
        let request = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/listPlugins","version":"1.0"}"#,
        )
        .expect("request");
        let error = handle_list_plugins(&empty, &request).expect_err("scope");
        assert_eq!(error.code, "ScopeDenied");

        let inspect =
            ServeContext::with_granted(&server, bitty_ipc::ScopeSet::single(Scope::DebugInspect));
        let bad = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/listPlugins","version":"1.0","params":{"generation":0}}"#,
        )
        .expect("request");
        let error = handle_list_plugins(&inspect, &bad).expect_err("bad generation");
        assert_eq!(error.code, "InvalidParams");

        let missing = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/getPlugin","version":"1.0"}"#,
        )
        .expect("request");
        let error = handle_get_plugin(&inspect, &missing).expect_err("missing id");
        assert_eq!(error.code, "InvalidParams");

        let unknown = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/getPlugin","version":"1.0","params":{"pluginId":"xuepoo.absent"}}"#,
        )
        .expect("request");
        assert_eq!(
            handle_get_plugin(&inspect, &unknown).expect("served"),
            "null"
        );
        publish(None);
    }
}
