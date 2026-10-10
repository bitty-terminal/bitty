//! Read-only backend for the Lua `bitty.debug.*` surface (CTX-0897).
//!
//! Two runtime-owned, single-threaded structures are shared with every
//! plugin generation's [`PluginServices`](super::PluginServices):
//!
//! - [`DebugView`]: a sanitized snapshot of plugin lifecycle state, command
//!   registrations, and event subscriptions, rebuilt by the runtime after
//!   every lifecycle transition. It carries ids, versions, stable lowercase
//!   state labels, generations, command titles, and event kinds only — never
//!   settings values, store contents, secrets, terminal content, or the
//!   message of a `Failed` state.
//! - [`TraceHub`]: per-owner bounded event traces. The runtime records each
//!   delivered event once (before fan-out); every live trace whose owner
//!   declares the event kind in its manifest `lazy.events` (the same
//!   precondition `bitty.events.subscribe` enforces) and whose filter
//!   matches receives a copy into a drop-oldest ring buffer. The copy is
//!   redacted for the owner's own grants (the same
//!   [`redaction`](super::redaction) policy the subscriber fan-out applies)
//!   and then size-bounded.
//!
//! Capability gating (`debug.inspect`, `debug.trace`) is enforced by
//! `PluginServices` before any of these structures are touched.
//!
//! CTX-0926 (W-100 first slice, W-71 observability boundary) transition
//! note: this backend is optional debug/trace *implementation* whose future
//! owner is `bitty-observability`; it stays compiled into Core until the
//! `W-71` removal gates pass (`W-110` conformance, default/safe parity,
//! redaction and bounds evidence, caller audit, fail-closed version
//! negotiation, docs sync, independent review plus green CI). This slice
//! retires nothing and changes no behavior.
//!
//! CTX-1087 (F7, #1891) readiness gate: same story — Core keeps the
//! debug/trace seam only because `bitty-observability` is not consumed yet.
//! When that repo signals readiness, the Core path retires there (tracked in
//! `bitty-observability`, not here). No retirements in this slice.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use bitty_lua::{BridgeError, LuaValue};

use super::LifecycleState;
use super::redaction::{self, RecipientView};
use super::store;

/// Default ring-buffer capacity of one trace (`max_events` when omitted).
pub const DEFAULT_TRACE_MAX_EVENTS: usize = 1000;
/// Largest accepted `max_events` for one trace.
pub const TRACE_MAX_EVENTS_LIMIT: usize = 10_000;
/// Smallest accepted `max_events` for one trace.
pub const TRACE_MIN_EVENTS: usize = 1;
/// Maximum concurrently open traces owned by one plugin (`E_DEF_LIMIT`).
pub const MAX_TRACES_PER_PLUGIN: usize = 4;
/// Largest encoded-JSON payload kept verbatim in a trace record; larger
/// payloads are replaced by `{ truncated = true, bytes = <n> }`.
pub const TRACE_PAYLOAD_MAX_BYTES: usize = 4096;
/// Maximum byte length of a trace topic filter pattern.
pub const TRACE_FILTER_MAX_BYTES: usize = 128;
/// Aggregate retained bytes (topic + encoded payload) of one trace buffer.
///
/// Caps memory independently of `max_events`: without it four traces of
/// [`TRACE_MAX_EVENTS_LIMIT`] records at [`TRACE_PAYLOAD_MAX_BYTES`] each
/// could pin ~160 MiB per plugin. Oldest records are dropped first.
pub const TRACE_BUFFER_MAX_BYTES: usize = 1024 * 1024;
/// Maximum items returned by one `bitty.debug.inspect` call.
pub const MAX_INSPECT_ITEMS: usize = 1024;

/// Stable lowercase label for a lifecycle state.
///
/// `Failed` maps to `"failed"` without its message: failure details can
/// carry paths or plugin-supplied text and never cross to another plugin.
#[must_use]
pub fn lifecycle_label(state: &LifecycleState) -> &'static str {
    match state {
        LifecycleState::Unloaded => "unloaded",
        LifecycleState::Loading => "loading",
        LifecycleState::Activating => "activating",
        LifecycleState::Active => "active",
        LifecycleState::Suspended => "suspended",
        LifecycleState::Disposing => "disposing",
        LifecycleState::Disposed => "disposed",
        LifecycleState::Failed(_) => "failed",
    }
}

/// One plugin row of the [`DebugView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugPlugin {
    /// Owner-qualified plugin id.
    pub id: String,
    /// Manifest version.
    pub version: String,
    /// Stable lowercase lifecycle label ([`lifecycle_label`]).
    pub state: &'static str,
    /// Activation generation (0 before the first activation).
    pub generation: u32,
}

/// One command registration row of the [`DebugView`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DebugCommand {
    /// Owning plugin id.
    pub plugin: String,
    /// Command id as registered (unqualified).
    pub id: String,
    /// Bounded command title.
    pub title: String,
}

/// One event subscription row of the [`DebugView`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DebugEvent {
    /// Owning plugin id.
    pub plugin: String,
    /// Subscribed event kind.
    pub kind: String,
}

/// Sanitized runtime snapshot served by `bitty.debug.inspect`.
#[derive(Debug, Default)]
pub struct DebugView {
    plugins: Vec<DebugPlugin>,
    commands: Vec<DebugCommand>,
    events: Vec<DebugEvent>,
}

impl DebugView {
    /// Empty view.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole snapshot; rows are sorted for deterministic output.
    ///
    /// O(n log n) in the number of rows.
    pub fn replace(
        &mut self,
        mut plugins: Vec<DebugPlugin>,
        mut commands: Vec<DebugCommand>,
        mut events: Vec<DebugEvent>,
    ) {
        plugins.sort_by(|a, b| a.id.cmp(&b.id));
        commands.sort();
        events.sort();
        self.plugins = plugins;
        self.commands = commands;
        self.events = events;
    }

    /// Plugin rows sorted by id.
    #[must_use]
    pub fn plugins(&self) -> &[DebugPlugin] {
        &self.plugins
    }

    /// Command rows sorted by `(plugin, id, title)`.
    #[must_use]
    pub fn commands(&self) -> &[DebugCommand] {
        &self.commands
    }

    /// Event rows sorted by `(plugin, kind)`.
    #[must_use]
    pub fn events(&self) -> &[DebugEvent] {
        &self.events
    }

    /// Serve the `plugins`/`commands`/`events` inspect targets.
    ///
    /// Returns `None` for any other target (the caller owns `grants`,
    /// `panels`, and the unknown-target error).
    #[must_use]
    pub fn inspect(&self, target: &str) -> Option<LuaValue> {
        let items: Vec<LuaValue> = match target {
            "plugins" => self
                .plugins
                .iter()
                .map(|plugin| {
                    LuaValue::table([
                        ("id", LuaValue::String(plugin.id.clone())),
                        ("version", LuaValue::String(plugin.version.clone())),
                        ("state", LuaValue::String(plugin.state.to_string())),
                        (
                            "generation",
                            LuaValue::Integer(i64::from(plugin.generation)),
                        ),
                    ])
                })
                .collect(),
            "commands" => self
                .commands
                .iter()
                .map(|command| {
                    LuaValue::table([
                        ("plugin", LuaValue::String(command.plugin.clone())),
                        ("id", LuaValue::String(command.id.clone())),
                        ("title", LuaValue::String(command.title.clone())),
                    ])
                })
                .collect(),
            "events" => self
                .events
                .iter()
                .map(|event| {
                    LuaValue::table([
                        ("plugin", LuaValue::String(event.plugin.clone())),
                        ("kind", LuaValue::String(event.kind.clone())),
                    ])
                })
                .collect(),
            _ => return None,
        };
        Some(inspect_result(target, items))
    }
}

/// Build the `{ target, items, truncated }` inspect shape, capping `items`
/// at [`MAX_INSPECT_ITEMS`].
#[must_use]
pub fn inspect_result(target: &str, mut items: Vec<LuaValue>) -> LuaValue {
    let truncated = items.len() > MAX_INSPECT_ITEMS;
    items.truncate(MAX_INSPECT_ITEMS);
    LuaValue::table([
        ("target", LuaValue::String(target.to_string())),
        ("items", LuaValue::array(items)),
        ("truncated", LuaValue::Bool(truncated)),
    ])
}

fn invalid(message: impl Into<String>) -> BridgeError {
    BridgeError::new("validation", "E_DEF_INVALID", message)
}

/// Topic filter of one trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceFilter {
    /// No filter: every topic matches.
    All,
    /// Exact topic match.
    Exact(String),
    /// Prefix match (pattern with its single trailing `*` removed).
    Prefix(String),
}

impl TraceFilter {
    /// Parse and validate a filter pattern.
    ///
    /// Valid patterns are non-empty, at most [`TRACE_FILTER_MAX_BYTES`]
    /// bytes, printable ASCII (no spaces or controls), and contain at most
    /// one `*`, which must be the last byte. A lone `*` matches everything.
    ///
    /// # Errors
    ///
    /// `E_DEF_INVALID` for any pattern outside that shape.
    pub fn parse(pattern: &str) -> Result<Self, BridgeError> {
        if pattern.is_empty() || pattern.len() > TRACE_FILTER_MAX_BYTES {
            return Err(invalid(format!(
                "debug.trace filter must be 1..={TRACE_FILTER_MAX_BYTES} bytes"
            )));
        }
        if !pattern.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(invalid(
                "debug.trace filter must be printable ASCII without spaces",
            ));
        }
        match pattern.find('*') {
            None => Ok(Self::Exact(pattern.to_string())),
            Some(index) if index == pattern.len() - 1 => {
                Ok(Self::Prefix(pattern[..index].to_string()))
            }
            Some(_) => Err(invalid(
                "debug.trace filter allows a single trailing '*' only",
            )),
        }
    }

    /// Whether `topic` passes this filter.
    #[must_use]
    pub fn matches(&self, topic: &str) -> bool {
        match self {
            Self::All => true,
            Self::Exact(exact) => topic == exact,
            Self::Prefix(prefix) => topic.starts_with(prefix.as_str()),
        }
    }
}

/// Validated parameters for a new trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceSpec {
    /// Topic filter.
    pub filter: TraceFilter,
    /// Ring-buffer capacity in records.
    pub max_events: usize,
}

impl Default for TraceSpec {
    fn default() -> Self {
        Self {
            filter: TraceFilter::All,
            max_events: DEFAULT_TRACE_MAX_EVENTS,
        }
    }
}

/// Parsed `bitty.debug.trace(opts)` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceRequest {
    /// Open a new trace.
    Start(TraceSpec),
    /// Close the caller-owned trace `handle`.
    Stop(i64),
}

/// Parse `bitty.debug.trace` options.
///
/// - `nil` opens a trace with defaults (all topics,
///   [`DEFAULT_TRACE_MAX_EVENTS`]).
/// - A table accepts only `enabled` (boolean, default `true`), `filter`
///   (string, see [`TraceFilter::parse`]), `max_events` (integer in
///   `TRACE_MIN_EVENTS..=TRACE_MAX_EVENTS_LIMIT`), and `handle` (integer).
/// - `enabled = false` closes a trace: `handle` is required and
///   `filter`/`max_events` are rejected. With `enabled = true` (or absent),
///   `handle` is rejected.
///
/// # Errors
///
/// `E_DEF_INVALID` for a non-table, an unknown key, a wrong field type, or
/// an out-of-range value.
pub fn parse_trace_opts(opts: &LuaValue) -> Result<TraceRequest, BridgeError> {
    let pairs = match opts {
        LuaValue::Nil => return Ok(TraceRequest::Start(TraceSpec::default())),
        LuaValue::Table(pairs) => pairs,
        _ => return Err(invalid("debug.trace opts must be a table or nil")),
    };
    let mut enabled = true;
    let mut filter = None;
    let mut max_events = None;
    let mut handle = None;
    for (key, value) in pairs {
        let LuaValue::String(key) = key else {
            return Err(invalid("debug.trace opts keys must be strings"));
        };
        match (key.as_str(), value) {
            ("enabled", LuaValue::Bool(flag)) => enabled = *flag,
            ("filter", LuaValue::String(pattern)) => filter = Some(TraceFilter::parse(pattern)?),
            ("max_events", LuaValue::Integer(count)) => {
                let in_range = usize::try_from(*count).is_ok_and(|count| {
                    (TRACE_MIN_EVENTS..=TRACE_MAX_EVENTS_LIMIT).contains(&count)
                });
                if !in_range {
                    return Err(invalid(format!(
                        "debug.trace max_events must be \
                         {TRACE_MIN_EVENTS}..={TRACE_MAX_EVENTS_LIMIT}"
                    )));
                }
                max_events = usize::try_from(*count).ok();
            }
            ("handle", LuaValue::Integer(value)) => handle = Some(*value),
            ("enabled" | "filter" | "max_events" | "handle", _) => {
                return Err(invalid(format!(
                    "debug.trace opts.{key} has the wrong type"
                )));
            }
            _ => return Err(invalid(format!("debug.trace opts.{key} is not supported"))),
        }
    }
    if enabled {
        if handle.is_some() {
            return Err(invalid(
                "debug.trace opts.handle is only valid with enabled = false",
            ));
        }
        Ok(TraceRequest::Start(TraceSpec {
            filter: filter.unwrap_or(TraceFilter::All),
            max_events: max_events.unwrap_or(DEFAULT_TRACE_MAX_EVENTS),
        }))
    } else {
        if filter.is_some() || max_events.is_some() {
            return Err(invalid(
                "debug.trace opts.filter/max_events are invalid with enabled = false",
            ));
        }
        handle
            .map(TraceRequest::Stop)
            .ok_or_else(|| invalid("debug.trace enabled = false requires opts.handle"))
    }
}

/// One recorded event.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceRecord {
    /// Event topic (kind).
    pub topic: String,
    /// Runtime event sequence number.
    pub sequence: u64,
    /// Monotonic milliseconds since the [`TraceHub`] was created.
    pub timestamp_ms: u64,
    /// Bounded payload (see [`bound_payload`]).
    pub payload: LuaValue,
    /// Retained bytes charged against [`TRACE_BUFFER_MAX_BYTES`].
    bytes: usize,
}

impl TraceRecord {
    /// `{ topic, sequence, timestamp, payload }` Lua shape.
    #[must_use]
    pub fn to_value(&self) -> LuaValue {
        LuaValue::table([
            ("topic", LuaValue::String(self.topic.clone())),
            (
                "sequence",
                LuaValue::Integer(i64::try_from(self.sequence).unwrap_or(i64::MAX)),
            ),
            (
                "timestamp",
                LuaValue::Integer(i64::try_from(self.timestamp_ms).unwrap_or(i64::MAX)),
            ),
            ("payload", self.payload.clone()),
        ])
    }
}

/// Replace a payload whose encoded JSON exceeds [`TRACE_PAYLOAD_MAX_BYTES`]
/// by `{ truncated = true, bytes = <encoded length> }`.
///
/// Returns the bounded payload and its retained byte cost.
#[must_use]
pub fn bound_payload(payload: &LuaValue) -> (LuaValue, usize) {
    let encoded = store::encode_json(payload).len();
    if encoded <= TRACE_PAYLOAD_MAX_BYTES {
        return (payload.clone(), encoded);
    }
    let marker = LuaValue::table([
        ("truncated", LuaValue::Bool(true)),
        (
            "bytes",
            LuaValue::Integer(i64::try_from(encoded).unwrap_or(i64::MAX)),
        ),
    ]);
    let cost = store::encode_json(&marker).len();
    (marker, cost)
}

#[derive(Debug)]
struct Trace {
    owner: String,
    /// Owner's manifest `lazy.events`, snapshotted at open. Least privilege:
    /// a trace never records a kind its owner could not subscribe to. The
    /// manifest is fixed per generation and traces never outlive their
    /// generation (dispose, reload, and failure drop them), so the snapshot
    /// cannot go stale.
    declared: BTreeSet<String>,
    /// Owner's granted capability ids, snapshotted at open for payload
    /// redaction. Same staleness argument as `declared`: grants are fixed
    /// per generation and traces drop with it.
    granted: BTreeSet<String>,
    filter: TraceFilter,
    max_events: usize,
    records: VecDeque<TraceRecord>,
    bytes: usize,
    dropped: u64,
}

impl Trace {
    /// Push with drop-oldest on the record count and the byte budget.
    fn push(&mut self, record: TraceRecord) {
        while !self.records.is_empty()
            && (self.records.len() >= self.max_events
                || self.bytes + record.bytes > TRACE_BUFFER_MAX_BYTES)
        {
            if let Some(old) = self.records.pop_front() {
                self.bytes -= old.bytes;
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        self.bytes += record.bytes;
        self.records.push_back(record);
    }
}

/// Drained contents of one trace.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceDrain {
    /// Records in arrival order.
    pub records: Vec<TraceRecord>,
    /// Records dropped (drop-oldest) since the previous drain.
    pub dropped: u64,
}

impl TraceDrain {
    /// `{ records = { ... }, dropped = <n> }` Lua shape.
    #[must_use]
    pub fn to_value(&self) -> LuaValue {
        LuaValue::table([
            (
                "records",
                LuaValue::array(self.records.iter().map(TraceRecord::to_value).collect()),
            ),
            (
                "dropped",
                LuaValue::Integer(i64::try_from(self.dropped).unwrap_or(i64::MAX)),
            ),
        ])
    }
}

/// Runtime-wide registry of per-owner event traces.
#[derive(Debug)]
pub struct TraceHub {
    epoch: Instant,
    next_handle: i64,
    traces: BTreeMap<i64, Trace>,
}

impl Default for TraceHub {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceHub {
    /// Empty hub; record timestamps count from now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            next_handle: 1,
            traces: BTreeMap::new(),
        }
    }

    /// Whether no trace is open (the runtime skips recording entirely).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }

    /// Open traces owned by `owner`.
    #[must_use]
    pub fn trace_count(&self, owner: &str) -> usize {
        self.traces
            .values()
            .filter(|trace| trace.owner == owner)
            .count()
    }

    /// Open a trace for `owner`; handles are monotonic and never reused.
    ///
    /// `declared` is the owner's manifest `lazy.events` set: only those
    /// kinds are ever recorded into this trace. A filter that matches none
    /// of them is accepted and simply never records. `granted` is the
    /// owner's capability snapshot; recorded payloads are redacted for it.
    ///
    /// # Errors
    ///
    /// `E_DEF_LIMIT` when `owner` already has [`MAX_TRACES_PER_PLUGIN`]
    /// open traces or the handle space is exhausted.
    pub fn start(
        &mut self,
        owner: &str,
        declared: BTreeSet<String>,
        granted: BTreeSet<String>,
        spec: TraceSpec,
    ) -> Result<i64, BridgeError> {
        if self.trace_count(owner) >= MAX_TRACES_PER_PLUGIN {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                format!("debug.trace limit ({MAX_TRACES_PER_PLUGIN} per plugin) exceeded"),
            ));
        }
        let handle = self.next_handle;
        let Some(next) = handle.checked_add(1) else {
            return Err(BridgeError::new(
                "budget",
                "E_DEF_LIMIT",
                "debug.trace handle space exhausted",
            ));
        };
        self.next_handle = next;
        self.traces.insert(
            handle,
            Trace {
                owner: owner.to_string(),
                declared,
                granted,
                filter: spec.filter,
                max_events: spec.max_events,
                records: VecDeque::new(),
                bytes: 0,
                dropped: 0,
            },
        );
        Ok(handle)
    }

    /// Close `handle` if `owner` owns it; unknown and foreign handles are
    /// indistinguishable (`false`).
    pub fn stop(&mut self, owner: &str, handle: i64) -> bool {
        if self
            .traces
            .get(&handle)
            .is_some_and(|trace| trace.owner == owner)
        {
            self.traces.remove(&handle);
            true
        } else {
            false
        }
    }

    /// Drain `handle` if `owner` owns it, resetting its dropped counter.
    ///
    /// O(r) in the drained record count. Unknown and foreign handles both
    /// return `None`, so a plugin cannot probe another plugin's traces.
    pub fn drain(&mut self, owner: &str, handle: i64) -> Option<TraceDrain> {
        let trace = self.traces.get_mut(&handle)?;
        if trace.owner != owner {
            return None;
        }
        let records: Vec<TraceRecord> = trace.records.drain(..).collect();
        trace.bytes = 0;
        let dropped = std::mem::take(&mut trace.dropped);
        Some(TraceDrain { records, dropped })
    }

    /// Record one event into every trace whose owner declares `topic` in its
    /// manifest `lazy.events`, whose filter matches, and whose owner
    /// `is_live`.
    ///
    /// Redaction rule (CTX-0899): a trace owner never observes more than a
    /// `bitty.events.subscribe` handler holding the same grants. Each
    /// owner's copy is passed through [`redaction::recipient_view`] with the
    /// owner's grant snapshot (taken at [`Self::start`]) before it is
    /// bounded by [`bound_payload`], so the size marker and the byte charge
    /// reflect the redacted payload, never the raw one. Unknown kinds are
    /// withheld (fail closed).
    ///
    /// O(t log k) checks over open traces (k = declared kinds per owner),
    /// then one redaction + encoding (O(payload)) per distinct recipient
    /// view (at most three) and one clone per matching trace; amortized O(1)
    /// ring push each. No work beyond the emptiness check when no trace is
    /// open.
    pub fn record(
        &mut self,
        topic: &str,
        sequence: u64,
        payload: &LuaValue,
        is_live: impl Fn(&str) -> bool,
    ) {
        if self.traces.is_empty() {
            return;
        }
        let targets: Vec<(i64, RecipientView)> = self
            .traces
            .iter()
            .filter(|(_, trace)| {
                trace.declared.contains(topic)
                    && trace.filter.matches(topic)
                    && is_live(&trace.owner)
            })
            .map(|(handle, trace)| {
                let view = redaction::recipient_view(topic, |capability| {
                    trace.granted.contains(capability)
                });
                (*handle, view)
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        let timestamp_ms = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        // One bounded record per distinct view; the view set is tiny.
        let mut built: Vec<(RecipientView, TraceRecord)> = Vec::new();
        for (handle, view) in targets {
            let record = match built.iter().find(|(seen, _)| *seen == view) {
                Some((_, record)) => record.clone(),
                None => {
                    let redacted = redaction::apply_view(view, payload);
                    let (bounded, payload_bytes) = bound_payload(&redacted);
                    let record = TraceRecord {
                        topic: topic.to_string(),
                        sequence,
                        timestamp_ms,
                        payload: bounded,
                        bytes: topic.len() + payload_bytes,
                    };
                    built.push((view, record.clone()));
                    record
                }
            };
            if let Some(trace) = self.traces.get_mut(&handle) {
                trace.push(record);
            }
        }
    }

    /// Drop every trace whose owner fails `keep` (dispose/failure cleanup).
    pub fn retain_owners(&mut self, keep: impl Fn(&str) -> bool) {
        self.traces.retain(|_, trace| keep(&trace.owner));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: Vec<(&str, LuaValue)>) -> LuaValue {
        LuaValue::Table(
            pairs
                .into_iter()
                .map(|(k, v)| (LuaValue::String(k.to_string()), v))
                .collect(),
        )
    }

    fn spec(filter: TraceFilter, max_events: usize) -> TraceSpec {
        TraceSpec { filter, max_events }
    }

    fn live(_: &str) -> bool {
        true
    }

    /// Declared kinds covering every topic the tests below record.
    fn declared() -> BTreeSet<String> {
        ["t", "terminal.opened", "focus.changed"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn filter_exact_prefix_and_all() {
        let exact = TraceFilter::parse("terminal.opened").expect("valid");
        assert!(exact.matches("terminal.opened"));
        assert!(!exact.matches("terminal.opened2"));
        assert!(!exact.matches("terminal.closed"));
        let prefix = TraceFilter::parse("terminal.*").expect("valid");
        assert_eq!(prefix, TraceFilter::Prefix("terminal.".to_string()));
        assert!(prefix.matches("terminal.opened"));
        assert!(prefix.matches("terminal."));
        assert!(!prefix.matches("focus.changed"));
        let star = TraceFilter::parse("*").expect("valid");
        assert!(star.matches("anything"));
        assert!(TraceFilter::All.matches(""));
        assert!(TraceFilter::parse("bitty.plugin:*").is_ok());
    }

    #[test]
    fn filter_rejects_malformed_patterns() {
        for bad in [
            String::new(),
            "a".repeat(TRACE_FILTER_MAX_BYTES + 1),
            "a*b".to_string(),
            "**".to_string(),
            "*a".to_string(),
            "has space".to_string(),
            "tab\t".to_string(),
            "caf\u{e9}".to_string(),
        ] {
            let error = TraceFilter::parse(&bad).expect_err("must reject");
            assert_eq!(error.code, "E_DEF_INVALID", "{bad:?}");
        }
        assert!(TraceFilter::parse(&"a".repeat(TRACE_FILTER_MAX_BYTES)).is_ok());
    }

    #[test]
    fn opts_defaults_and_bounds() {
        assert_eq!(
            parse_trace_opts(&LuaValue::Nil),
            Ok(TraceRequest::Start(TraceSpec::default()))
        );
        assert_eq!(
            parse_trace_opts(&opts(vec![])),
            Ok(TraceRequest::Start(TraceSpec::default()))
        );
        let max = i64::try_from(TRACE_MAX_EVENTS_LIMIT).expect("fits");
        assert_eq!(
            parse_trace_opts(&opts(vec![
                ("enabled", LuaValue::Bool(true)),
                ("filter", LuaValue::String("focus.*".to_string())),
                ("max_events", LuaValue::Integer(max)),
            ])),
            Ok(TraceRequest::Start(spec(
                TraceFilter::Prefix("focus.".to_string()),
                TRACE_MAX_EVENTS_LIMIT
            )))
        );
        for bad in [0, -1, max + 1, i64::MAX, i64::MIN] {
            let error = parse_trace_opts(&opts(vec![("max_events", LuaValue::Integer(bad))]))
                .expect_err("out of range");
            assert_eq!(error.code, "E_DEF_INVALID", "{bad}");
        }
        assert!(parse_trace_opts(&opts(vec![("max_events", LuaValue::Integer(1))])).is_ok());
    }

    #[test]
    fn opts_reject_bad_shapes() {
        for bad in [
            LuaValue::Bool(true),
            LuaValue::Integer(1),
            LuaValue::String("x".to_string()),
            opts(vec![("unknown", LuaValue::Bool(true))]),
            opts(vec![("enabled", LuaValue::Integer(1))]),
            opts(vec![("filter", LuaValue::Integer(1))]),
            opts(vec![("max_events", LuaValue::Number(1.5))]),
            opts(vec![("handle", LuaValue::Integer(1))]),
            opts(vec![("enabled", LuaValue::Bool(false))]),
            opts(vec![
                ("enabled", LuaValue::Bool(false)),
                ("handle", LuaValue::Integer(1)),
                ("filter", LuaValue::String("a".to_string())),
            ]),
            LuaValue::Table(vec![(LuaValue::Integer(1), LuaValue::Bool(true))]),
        ] {
            let error = parse_trace_opts(&bad).expect_err("must reject");
            assert_eq!(error.code, "E_DEF_INVALID", "{bad:?}");
        }
        assert_eq!(
            parse_trace_opts(&opts(vec![
                ("enabled", LuaValue::Bool(false)),
                ("handle", LuaValue::Integer(7)),
            ])),
            Ok(TraceRequest::Stop(7))
        );
    }

    #[test]
    fn ring_buffer_drops_oldest_and_counts() {
        let mut hub = TraceHub::new();
        let handle = hub
            .start(
                "a.p",
                declared(),
                BTreeSet::new(),
                spec(TraceFilter::All, 3),
            )
            .expect("start");
        for sequence in 1..=5 {
            hub.record("t", sequence, &LuaValue::Integer(0), live);
        }
        let drain = hub.drain("a.p", handle).expect("owned");
        let sequences: Vec<u64> = drain.records.iter().map(|r| r.sequence).collect();
        assert_eq!(sequences, vec![3, 4, 5]);
        assert_eq!(drain.dropped, 2);
        let again = hub.drain("a.p", handle).expect("owned");
        assert!(again.records.is_empty());
        assert_eq!(again.dropped, 0, "dropped resets after reporting");
    }

    #[test]
    fn byte_budget_drops_oldest() {
        let mut hub = TraceHub::new();
        let handle = hub
            .start(
                "a.p",
                declared(),
                BTreeSet::new(),
                spec(TraceFilter::All, TRACE_MAX_EVENTS_LIMIT),
            )
            .expect("start");
        let payload = LuaValue::String("x".repeat(TRACE_PAYLOAD_MAX_BYTES - 2));
        // A known, ungated kind: unknown kinds are withheld (fail closed),
        // which would replace the payload before the size accounting.
        let topic = "terminal.opened";
        let per_record = topic.len() + TRACE_PAYLOAD_MAX_BYTES;
        let fits = TRACE_BUFFER_MAX_BYTES / per_record;
        let total = fits + 10;
        for sequence in 0..total {
            hub.record(topic, sequence as u64, &payload, live);
        }
        let drain = hub.drain("a.p", handle).expect("owned");
        assert_eq!(drain.records.len(), fits);
        assert_eq!(drain.dropped, 10);
        assert_eq!(drain.records[0].sequence, 10);
    }

    #[test]
    fn payload_truncation_replaces_oversized_payload() {
        let small = LuaValue::table([("k", LuaValue::Integer(1))]);
        assert_eq!(bound_payload(&small).0, small);
        let big = LuaValue::String("x".repeat(TRACE_PAYLOAD_MAX_BYTES));
        let (bounded, _) = bound_payload(&big);
        assert_eq!(bounded.get("truncated"), Some(&LuaValue::Bool(true)));
        assert_eq!(
            bounded.get("bytes"),
            Some(&LuaValue::Integer(
                i64::try_from(TRACE_PAYLOAD_MAX_BYTES + 2).expect("fits")
            ))
        );
    }

    #[test]
    fn filter_applies_on_record() {
        let mut hub = TraceHub::new();
        let handle = hub
            .start(
                "a.p",
                declared(),
                BTreeSet::new(),
                spec(TraceFilter::Prefix("terminal.".to_string()), 10),
            )
            .expect("start");
        hub.record("terminal.opened", 1, &LuaValue::Nil, live);
        hub.record("focus.changed", 2, &LuaValue::Nil, live);
        let drain = hub.drain("a.p", handle).expect("owned");
        assert_eq!(drain.records.len(), 1);
        assert_eq!(drain.records[0].topic, "terminal.opened");
        let value = drain.to_value();
        let records = value.get("records").expect("records");
        let first = match records {
            LuaValue::Table(pairs) => pairs[0].1.clone(),
            other => panic!("records must be a table: {other:?}"),
        };
        assert_eq!(first.get("sequence"), Some(&LuaValue::Integer(1)));
        assert!(matches!(first.get("timestamp"), Some(LuaValue::Integer(ms)) if *ms >= 0));
        assert_eq!(value.get("dropped"), Some(&LuaValue::Integer(0)));
    }

    #[test]
    fn only_owner_declared_kinds_are_recorded() {
        let mut hub = TraceHub::new();
        let declared: BTreeSet<String> = ["terminal.opened".to_string()].into();
        let all = hub
            .start(
                "a.p",
                declared.clone(),
                BTreeSet::new(),
                TraceSpec::default(),
            )
            .expect("start");
        let prefix = hub
            .start(
                "a.p",
                declared.clone(),
                BTreeSet::new(),
                spec(TraceFilter::Prefix("terminal.".to_string()), 10),
            )
            .expect("start");
        // A filter matching no declared kind is accepted and never records.
        let never = hub
            .start(
                "a.p",
                declared,
                BTreeSet::new(),
                spec(TraceFilter::Exact("focus.changed".to_string()), 10),
            )
            .expect("unmatchable filter is not an error");
        hub.record("terminal.opened", 1, &LuaValue::Nil, live);
        hub.record("terminal.closed", 2, &LuaValue::Nil, live);
        hub.record("focus.changed", 3, &LuaValue::Nil, live);
        for handle in [all, prefix] {
            let drain = hub.drain("a.p", handle).expect("owned");
            let topics: Vec<&str> = drain.records.iter().map(|r| r.topic.as_str()).collect();
            assert_eq!(topics, vec!["terminal.opened"], "handle {handle}");
            assert_eq!(drain.dropped, 0);
        }
        assert!(hub.drain("a.p", never).expect("owned").records.is_empty());
        // An owner with no declared kinds records nothing at all.
        let empty = hub
            .start(
                "b.p",
                BTreeSet::new(),
                BTreeSet::new(),
                TraceSpec::default(),
            )
            .expect("start");
        hub.record("terminal.opened", 4, &LuaValue::Nil, live);
        assert!(hub.drain("b.p", empty).expect("owned").records.is_empty());
    }

    fn paste_payload() -> LuaValue {
        LuaValue::table([
            ("action", LuaValue::String("paste".into())),
            ("origin", LuaValue::String("user".into())),
            ("preview", LuaValue::String("hunter2".into())),
        ])
    }

    #[test]
    fn owners_with_different_grants_get_different_payloads() {
        let mut hub = TraceHub::new();
        let declared: BTreeSet<String> = ["intercept.paste".to_string()].into();
        let reader = hub
            .start(
                "a.p",
                declared.clone(),
                ["clipboard.read".to_string(), "debug.trace".to_string()].into(),
                TraceSpec::default(),
            )
            .expect("start");
        let blind = hub
            .start(
                "b.p",
                declared,
                ["debug.trace".to_string()].into(),
                TraceSpec::default(),
            )
            .expect("start");
        let payload = paste_payload();
        hub.record("intercept.paste", 1, &payload, live);

        let full = hub.drain("a.p", reader).expect("owned");
        assert_eq!(full.records[0].payload, payload);

        let redacted = hub.drain("b.p", blind).expect("owned");
        let record = &redacted.records[0];
        assert_eq!(record.payload.get("preview"), None);
        assert_eq!(
            record.payload.get("action"),
            Some(&LuaValue::String("paste".into()))
        );
        assert_eq!(
            record.payload.get(redaction::REDACTED_KEY),
            Some(&LuaValue::Bool(true))
        );
        assert!(!store::encode_json(&record.to_value()).contains("hunter2"));
        // The byte charge reflects the redacted payload, not the raw one.
        assert_eq!(
            record.bytes,
            "intercept.paste".len() + store::encode_json(&record.payload).len()
        );
    }

    #[test]
    fn redaction_runs_before_the_size_bound() {
        let mut hub = TraceHub::new();
        let declared: BTreeSet<String> = ["intercept.paste".to_string()].into();
        let blind = hub
            .start("b.p", declared, BTreeSet::new(), TraceSpec::default())
            .expect("start");
        // An oversized preview must not leak even its size: the redacted
        // payload is small, so no `truncated`/`bytes` marker appears.
        let payload = LuaValue::table([
            ("action", LuaValue::String("paste".into())),
            (
                "preview",
                LuaValue::String("x".repeat(TRACE_PAYLOAD_MAX_BYTES * 2)),
            ),
        ]);
        hub.record("intercept.paste", 1, &payload, live);
        let drain = hub.drain("b.p", blind).expect("owned");
        let recorded = &drain.records[0].payload;
        assert_eq!(recorded.get("truncated"), None);
        assert_eq!(recorded.get("bytes"), None);
        assert_eq!(recorded.get("preview"), None);
    }

    #[test]
    fn unknown_topics_are_withheld_in_traces() {
        let mut hub = TraceHub::new();
        let handle = hub
            .start(
                "a.p",
                declared(),
                ["clipboard.read".to_string()].into(),
                TraceSpec::default(),
            )
            .expect("start");
        hub.record("t", 1, &paste_payload(), live);
        let drain = hub.drain("a.p", handle).expect("owned");
        assert_eq!(
            drain.records[0].payload,
            LuaValue::table([(redaction::REDACTED_KEY, LuaValue::Bool(true))])
        );
    }

    #[test]
    fn max_traces_per_plugin_and_monotonic_handles() {
        let mut hub = TraceHub::new();
        let mut handles = Vec::new();
        for _ in 0..MAX_TRACES_PER_PLUGIN {
            handles.push(
                hub.start("a.p", declared(), BTreeSet::new(), TraceSpec::default())
                    .expect("start"),
            );
        }
        let error = hub
            .start("a.p", declared(), BTreeSet::new(), TraceSpec::default())
            .expect_err("limit");
        assert_eq!(error.code, "E_DEF_LIMIT");
        // The limit is per owner.
        let other = hub
            .start("b.p", declared(), BTreeSet::new(), TraceSpec::default())
            .expect("other owner");
        assert_eq!(handles, vec![1, 2, 3, 4]);
        assert_eq!(other, 5);
        assert!(hub.stop("a.p", 1));
        let reopened = hub
            .start("a.p", declared(), BTreeSet::new(), TraceSpec::default())
            .expect("slot freed");
        assert_eq!(reopened, 6, "handles are never reused");
    }

    #[test]
    fn handles_are_owner_isolated() {
        let mut hub = TraceHub::new();
        let handle = hub
            .start("a.p", declared(), BTreeSet::new(), TraceSpec::default())
            .expect("start");
        hub.record("t", 1, &LuaValue::Nil, live);
        assert!(hub.drain("b.p", handle).is_none(), "foreign drain");
        assert!(hub.drain("a.p", 999).is_none(), "unknown drain");
        assert!(!hub.stop("b.p", handle), "foreign stop");
        assert_eq!(hub.trace_count("a.p"), 1);
        let drain = hub.drain("a.p", handle).expect("owner still drains");
        assert_eq!(drain.records.len(), 1, "foreign probe consumed nothing");
    }

    #[test]
    fn non_live_owners_record_nothing_and_retain_prunes() {
        let mut hub = TraceHub::new();
        let a = hub
            .start("a.p", declared(), BTreeSet::new(), TraceSpec::default())
            .expect("start");
        let b = hub
            .start("b.p", declared(), BTreeSet::new(), TraceSpec::default())
            .expect("start");
        hub.record("t", 1, &LuaValue::Nil, |owner| owner == "a.p");
        assert_eq!(hub.drain("a.p", a).expect("a").records.len(), 1);
        assert!(hub.drain("b.p", b).expect("b").records.is_empty());
        hub.retain_owners(|owner| owner != "a.p");
        assert_eq!(hub.trace_count("a.p"), 0);
        assert!(hub.drain("a.p", a).is_none());
        assert_eq!(hub.trace_count("b.p"), 1);
    }

    #[test]
    fn view_inspect_shapes_and_truncation() {
        let mut view = DebugView::new();
        view.replace(
            vec![
                DebugPlugin {
                    id: "z.p".to_string(),
                    version: "1.0.0".to_string(),
                    state: lifecycle_label(&LifecycleState::Failed("secret /path".to_string())),
                    generation: 2,
                },
                DebugPlugin {
                    id: "a.p".to_string(),
                    version: "0.1.0".to_string(),
                    state: lifecycle_label(&LifecycleState::Active),
                    generation: 1,
                },
            ],
            Vec::new(),
            Vec::new(),
        );
        let value = view.inspect("plugins").expect("served");
        assert_eq!(
            value.get("target"),
            Some(&LuaValue::String("plugins".into()))
        );
        assert_eq!(value.get("truncated"), Some(&LuaValue::Bool(false)));
        let encoded = store::encode_json(&value);
        assert!(
            encoded.find("a.p") < encoded.find("z.p"),
            "sorted: {encoded}"
        );
        assert!(encoded.contains("\"failed\""));
        assert!(!encoded.contains("secret"), "failure message leaked");
        assert!(view.inspect("grants").is_none());

        let many = (0..=MAX_INSPECT_ITEMS)
            .map(|index| LuaValue::Integer(i64::try_from(index).expect("fits")))
            .collect();
        let capped = inspect_result("events", many);
        assert_eq!(capped.get("truncated"), Some(&LuaValue::Bool(true)));
        match capped.get("items") {
            Some(LuaValue::Table(items)) => assert_eq!(items.len(), MAX_INSPECT_ITEMS),
            other => panic!("items must be a table: {other:?}"),
        }
    }
}
