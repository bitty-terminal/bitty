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
//!
//! # Publication semantics
//!
//! The published copy is a **one-shot startup snapshot**: the binary publishes
//! the attached runtime once, when it assembles the serve slice. The runtime
//! keeps its own [`PluginRuntime::debug_plugins`] view current after every
//! lifecycle transition, but this store is never re-read, so a lazy
//! activation, suspension, disposal, or failure after startup is visible to
//! the in-process runtime API and not on the wire. Serving live transitions
//! means publishing from the runtime (a transition hook) instead of this
//! binary-side copy.
//!
//! Registration is **not test-mode-only**: `build_dispatcher` installs the two
//! handlers on both the test-mode and the default table. A normal instance
//! reads the same read-only metadata under the same elevated debug scope, and
//! a runtime-less run keeps the typed unavailable verdict.
//!
//! # Accepted result bounds
//!
//! Rows are normalized to the accepted client's `PluginSummary` guards before
//! they enter the store, so one unrepresentable row can never fail a whole
//! `listPlugins` call: `generation >= 1`, at most 256 rows, at most 256
//! `capabilities` entries per row, and at most 128 bytes per capability id.
//! Addressable generations are the only servable ones (issue #2773 asks for
//! the activated plugins), and capability ids that exceed the per-entry bound
//! are omitted rather than truncated because an id must stay exact; the
//! authoritative grant set is served by the grant surfaces, never by this row.

use std::sync::{Mutex, OnceLock};

use bitty_ipc::Scope;
use bitty_ipc::devtools::{DevtoolsRequest, Dispatcher, HandlerError, ServeContext};
use bitty_runtime::plugin_runtime::PluginRuntime;
use bitty_runtime::plugin_runtime::debug::DebugPlugin;

/// Maximum `pluginId` bytes (accepted client bound).
const MAX_PLUGIN_ID_BYTES: usize = 128;

/// Maximum rows served by one `listPlugins` result (accepted client
/// `MAX_PLUGINS`).
const MAX_SERVED_ROWS: usize = 256;

/// Maximum `capabilities` entries per served row (accepted client
/// `requireStringArray` item bound).
const MAX_SERVED_CAPABILITIES: usize = 256;

/// Maximum bytes of one served capability id (accepted client per-entry
/// bound).
const MAX_CAPABILITY_BYTES: usize = 128;

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

/// Normalize runtime rows to the accepted `PluginSummary` guards.
///
/// Only addressable generations are served. `generation == 0` means "before
/// the first activation", and the runtime publishes a row for every discovered
/// entry, including entries whose activation rolled back before
/// `entry.generation += 1` (issue #2773 asks for the activated plugins). Such
/// a row has no `(pluginId, generation)` resource to address and would fail
/// the accepted client's `requireGeneration` check for the whole call.
///
/// Declared capability ids may exceed the accepted 128-byte per-entry bound (a
/// capability id is legal up to 512 bytes plus a 1024-byte parameter, for
/// example `fs.read:<pattern>`), so ids that cannot be represented are omitted
/// rather than truncated: an id must stay exact, and this array is display
/// metadata. The authoritative grant set is served by the grant surfaces.
fn servable_rows(rows: Vec<DebugPlugin>) -> Vec<DebugPlugin> {
    rows.into_iter()
        .filter(|row| row.generation >= 1)
        .take(MAX_SERVED_ROWS)
        .map(|mut row| {
            row.capabilities
                .retain(|capability| capability.len() <= MAX_CAPABILITY_BYTES);
            row.capabilities.truncate(MAX_SERVED_CAPABILITIES);
            row
        })
        .collect()
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
            published.rows = servable_rows(runtime.debug_plugins());
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
    published.rows = servable_rows(rows);
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

/// Decode one JSON string literal starting at byte 0 (`"`), returning the
/// decoded string and the byte offset immediately after the closing quote.
fn decode_json_string(raw: &str) -> Option<(String, usize)> {
    let inner = raw.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = inner.char_indices();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => return Some((out, 1 + idx + 1)),
            '\\' => match chars.next()?.1 {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000c}'),
                'u' => {
                    // Exactly four hex digits, mirroring the accepted
                    // `bitty-ipc` scanner: a short run is malformed, not a
                    // smaller code point.
                    let mut code: u32 = 0;
                    for _ in 0..4 {
                        code = code * 16 + chars.next()?.1.to_digit(16)?;
                    }
                    out.push(char::from_u32(code)?);
                }
                _ => return None,
            },
            ch => out.push(ch),
        }
    }
    None
}

/// Skip one raw JSON string literal starting at byte `start` (`"`), returning
/// the byte index immediately after the closing `"`.
fn skip_json_string(raw: &str, start: usize) -> Option<usize> {
    let inner = raw.get(start..)?.strip_prefix('"')?;
    let mut chars = inner.char_indices();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => return Some(start + 1 + idx + 1),
            '\\' => {
                chars.next()?;
            }
            _ => {}
        }
    }
    None
}

/// Outcome of a top-level object key lookup.
///
/// `Ambiguous` is not the same as `Absent`: callers that treat "absent" as
/// "no filter" or "use the default" must fail closed on an ambiguous key
/// rather than silently dropping the caller's restriction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopKey<'a> {
    /// The key does not appear at the top level.
    Absent,
    /// The key appears exactly once; this is the raw value span after `:`
    /// (caller decides the shape).
    Value(&'a str),
    /// The key appears more than once at the top level.
    Ambiguous,
}

/// Find the raw value span after a top-level `"key":` in a JSON object.
/// Quote- and depth-aware so keys embedded inside string values or nested
/// containers never match.
///
/// Duplicate top-level `key` entries are reported as [`TopKey::Ambiguous`]
/// instead of resolving to the first or last occurrence: the envelope is
/// parsed upstream with last-wins semantics (`bitty-ipc-devtools` `json.rs`),
/// so a params scanner that picked the first occurrence would hand two layers
/// of the same request two different values.
fn top_key<'a>(params: &'a str, key: &str) -> TopKey<'a> {
    let Some(body) = params.trim().strip_prefix('{') else {
        return TopKey::Absent;
    };
    let bytes = body.as_bytes();
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut expect_key = true;
    let mut found: Option<&'a str> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                if depth == 0 && expect_key {
                    let Some((decoded_key, consumed)) = decode_json_string(&body[i..]) else {
                        return TopKey::Absent;
                    };
                    let next = i + consumed;
                    let after = body[next..].trim_start();
                    if let Some(rest) = after.strip_prefix(':') {
                        if decoded_key == key {
                            if found.is_some() {
                                return TopKey::Ambiguous;
                            }
                            found = Some(rest.trim_start());
                        }
                        expect_key = false;
                        i = body.len() - rest.len();
                        continue;
                    }
                    i = next;
                } else {
                    match skip_json_string(body, i) {
                        Some(next) => i = next,
                        None => return TopKey::Absent,
                    }
                }
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                i += 1;
            }
            b',' if depth == 0 => {
                expect_key = true;
                i += 1;
            }
            b':' if depth == 0 => {
                expect_key = false;
                i += 1;
            }
            _ => {
                let Some(ch) = body[i..].chars().next() else {
                    return TopKey::Absent;
                };
                i += ch.len_utf8();
            }
        }
    }
    match found {
        Some(value) => TopKey::Value(value),
        None => TopKey::Absent,
    }
}

/// Extract a top-level JSON string field with escape decoding; `None` when
/// the key is absent, ambiguous, or the value is not a JSON string.
fn top_string(params: &str, key: &str) -> Option<String> {
    match top_key(params, key) {
        TopKey::Value(value) => decode_json_string(value).map(|(decoded, _)| decoded),
        TopKey::Absent | TopKey::Ambiguous => None,
    }
}

/// Whether `rest` begins at a top-level JSON value delimiter (end, whitespace,
/// `,`, or `}`).
fn at_json_delimiter(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_whitespace() || c == ',' || c == '}')
}

/// Extract a top-level unsigned integer field; `None` for strings/objects/
/// arrays/non-digits/floats/exponents/ambiguous keys.
fn top_uint(params: &str, key: &str) -> Option<u64> {
    let TopKey::Value(value) = top_key(params, key) else {
        return None;
    };
    if value.starts_with('"') || value.starts_with('{') || value.starts_with('[') {
        return None;
    }
    let digits_len = value.bytes().take_while(u8::is_ascii_digit).count();
    if digits_len == 0 || digits_len > 20 || !at_json_delimiter(&value[digits_len..]) {
        return None;
    }
    value[..digits_len].parse().ok()
}

/// Whether `params` carries an explicit JSON `null` for `key`.
fn top_is_null(params: &str, key: &str) -> bool {
    let TopKey::Value(value) = top_key(params, key) else {
        return false;
    };
    value.strip_prefix("null").is_some_and(at_json_delimiter)
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
///
/// Every non-`null`, non-integer shape fails closed with `InvalidParams` — a
/// duplicated key included, because treating an ambiguous key as "absent"
/// would silently widen the listing the caller asked to narrow.
fn list_generation(params: &str) -> Result<Option<u64>, HandlerError> {
    if !params.trim_start().starts_with('{') {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            "listPlugins requires an object params".to_string(),
        ));
    }
    let invalid = || {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "generation must be null or an integer >= 1".to_string(),
        )
    };
    match top_key(params, "generation") {
        TopKey::Absent => Ok(None),
        TopKey::Ambiguous => Err(invalid()),
        TopKey::Value(_) => {
            if top_is_null(params, "generation") {
                return Ok(None);
            }
            match top_uint(params, "generation") {
                Some(generation) => {
                    validate_generation(generation)
                        .map_err(|reason| HandlerError::new("usage", "InvalidParams", reason))?;
                    Ok(Some(generation))
                }
                None => Err(invalid()),
            }
        }
    }
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
    fn top_level_scanners_ignore_nested_and_duplicate_keys_3075() {
        // A nested object carrying the same key names *before* the real
        // top-level entries is the input a raw `params.find` reader gets
        // wrong; it must not shadow them.
        let nested = r#"{"outer":{"generation":99,"pluginId":"xuepoo.nested"},"generation":3,"pluginId":"xuepoo.one"}"#;
        assert_eq!(super::top_uint(nested, "generation"), Some(3));
        assert_eq!(
            super::top_string(nested, "pluginId").as_deref(),
            Some("xuepoo.one")
        );
        // A nested array is skipped just the same.
        assert_eq!(
            super::top_uint(r#"{"outer":["generation",5],"generation":2}"#, "generation"),
            Some(2)
        );
        // A nested null is not a top-level null.
        assert!(!super::top_is_null(
            r#"{"outer":{"generation":null},"generation":1}"#,
            "generation"
        ));

        // Duplicate top-level keys are refused rather than resolved
        // first-wins: the envelope layer is last-wins, so two readers of one
        // request must not disagree.
        assert_eq!(
            super::top_uint(r#"{"generation":1,"generation":2}"#, "generation"),
            None
        );
        assert_eq!(
            super::top_string(r#"{"pluginId":"a.b","pluginId":"c.d"}"#, "pluginId"),
            None
        );
        assert!(!super::top_is_null(
            r#"{"generation":null,"generation":null}"#,
            "generation"
        ));

        // Null boundary: the literal `null` is a value, an identifier prefix
        // is not.
        assert!(super::top_is_null(r#"{"generation":null}"#, "generation"));
        assert!(super::top_is_null(r#"{"generation": null }"#, "generation"));
        assert!(!super::top_is_null(
            r#"{"generation":nullable}"#,
            "generation"
        ));
        assert!(!super::top_is_null(r#"{"generation":nulls}"#, "generation"));

        // Numeric prefixes stay refused.
        assert_eq!(super::top_uint(r#"{"generation":1.5}"#, "generation"), None);
        assert_eq!(super::top_uint(r#"{"generation":1e2}"#, "generation"), None);
        assert_eq!(
            super::top_uint(r#"{"generation":1abc}"#, "generation"),
            None
        );
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
    fn unaddressable_and_unrepresentable_rows_are_not_served() {
        let _serial = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut over_long = row("xuepoo.long", 3, "active");
        over_long.capabilities = vec![
            "ui.rich".to_string(),
            format!("fs.read:{}", "a".repeat(MAX_CAPABILITY_BYTES)),
        ];
        publish_rows(vec![
            row("xuepoo.pending", 0, "disposed"),
            row("xuepoo.live", 1, "active"),
            over_long,
        ]);
        let server =
            bitty_ipc::devtools::ServerInfo::new("test".into(), "test.sock".into(), 80, 24);
        let context =
            ServeContext::with_granted(&server, bitty_ipc::ScopeSet::single(Scope::DebugInspect));
        let list = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/listPlugins","version":"1.0"}"#,
        )
        .expect("request");
        let result = handle_list_plugins(&context, &list).expect("listPlugins served");
        assert!(
            !result.contains("xuepoo.pending"),
            "a generation-0 row must not be published: {result}"
        );
        assert!(result.contains("xuepoo.live"), "{result}");
        assert!(
            result.contains("xuepoo.long"),
            "a row with an over-long capability id is still addressable: {result}"
        );
        assert!(
            !result.contains("fs.read:"),
            "an over-long capability id must be omitted: {result}"
        );
        assert!(
            result.contains("\"capabilities\":[\"ui.rich\"]"),
            "representable capabilities stay: {result}"
        );

        let get_absent = bitty_ipc::devtools::parse_request(
            br#"{"id":2,"method":"bitty.debug/getPlugin","version":"1.0","params":{"pluginId":"xuepoo.pending"}}"#,
        )
        .expect("request");
        assert_eq!(
            handle_get_plugin(&context, &get_absent).expect("getPlugin served"),
            "null",
            "a generation-0 id is unknown, not a row"
        );
        let get_live = bitty_ipc::devtools::parse_request(
            br#"{"id":3,"method":"bitty.debug/getPlugin","version":"1.0","params":{"pluginId":"xuepoo.live"}}"#,
        )
        .expect("request");
        assert!(
            handle_get_plugin(&context, &get_live)
                .expect("getPlugin served")
                .contains("\"generation\":1")
        );
        publish(None);
    }

    #[test]
    fn served_rows_respect_the_client_cardinality_bounds() {
        let rows = (0..=MAX_SERVED_ROWS)
            .map(|index| row(&format!("xuepoo.row{index}"), 1, "active"))
            .collect();
        let served = servable_rows(rows);
        assert_eq!(served.len(), MAX_SERVED_ROWS);
        assert_eq!(served[0].id, "xuepoo.row0");
        assert_eq!(
            served[MAX_SERVED_ROWS - 1].id,
            format!("xuepoo.row{}", MAX_SERVED_ROWS - 1)
        );

        let mut wide = row("xuepoo.wide", 1, "active");
        wide.capabilities = (0..=MAX_SERVED_CAPABILITIES)
            .map(|index| format!("ui.cap{index}"))
            .collect();
        let served = servable_rows(vec![wide]);
        assert_eq!(served[0].capabilities.len(), MAX_SERVED_CAPABILITIES);
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

        // bitty#3075: fractional/exponent generation numbers must fail closed
        // as InvalidParams instead of truncating to their integer prefix, and
        // keys inside string values must never shadow top-level params.
        let float_gen = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/listPlugins","version":"1.0","params":{"generation":1.5}}"#,
        )
        .expect("request");
        assert_eq!(
            handle_list_plugins(&inspect, &float_gen)
                .expect_err("float generation")
                .code,
            "InvalidParams"
        );
        let shadowed = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/getPlugin","version":"1.0","params":{"note":"\"pluginId\":\"xuepoo.absent\"","pluginId":"xuepoo.one"}}"#,
        )
        .expect("request");
        assert!(
            handle_get_plugin(&inspect, &shadowed)
                .expect("served")
                .contains("\"id\":\"xuepoo.one\"")
        );

        // bitty#3075: a nested object carrying the same key name must not
        // shadow the real top-level one at the handler boundary either.
        let nested_shadow = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/getPlugin","version":"1.0","params":{"meta":{"pluginId":"xuepoo.absent"},"pluginId":"xuepoo.one"}}"#,
        )
        .expect("request");
        assert!(
            handle_get_plugin(&inspect, &nested_shadow)
                .expect("served")
                .contains("\"id\":\"xuepoo.one\"")
        );

        // `null` generation means "no filter"; an identifier prefix does not.
        let null_generation = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/listPlugins","version":"1.0","params":{"generation":null}}"#,
        )
        .expect("request");
        assert!(
            handle_list_plugins(&inspect, &null_generation)
                .expect("null generation is no filter")
                .contains("xuepoo.one")
        );
        let identifier_generation = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/listPlugins","version":"1.0","params":{"generation":nullable}}"#,
        )
        .expect("request");
        assert_eq!(
            handle_list_plugins(&inspect, &identifier_generation)
                .expect_err("identifier generation")
                .code,
            "InvalidParams"
        );

        // Duplicate top-level params keys are ambiguous, so they fail closed
        // instead of letting this scanner and the last-wins envelope parser
        // read different values out of one request.
        let duplicate_generation = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/listPlugins","version":"1.0","params":{"generation":1,"generation":2}}"#,
        )
        .expect("request");
        assert_eq!(
            handle_list_plugins(&inspect, &duplicate_generation)
                .expect_err("duplicate generation")
                .code,
            "InvalidParams"
        );
        let duplicate_plugin_id = bitty_ipc::devtools::parse_request(
            br#"{"id":1,"method":"bitty.debug/getPlugin","version":"1.0","params":{"pluginId":"xuepoo.one","pluginId":"xuepoo.absent"}}"#,
        )
        .expect("request");
        assert_eq!(
            handle_get_plugin(&inspect, &duplicate_plugin_id)
                .expect_err("duplicate pluginId")
                .code,
            "InvalidParams"
        );
        publish(None);
    }
}
