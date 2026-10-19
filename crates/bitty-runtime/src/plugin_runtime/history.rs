//! Runtime half of the read-only `bitty.history` surface (RFC-0004, W-146).
//!
//! The policy gate lives in [`bitty_plugin_host::history_read`]: grants,
//! scopes, safe mode, capture opt-in, bounds, budgets, and the typed-denial
//! taxonomy. This module supplies the two pieces the gate deliberately does
//! not own:
//!
//! - [`HistoryProvider`], the composition-root seam over already-persisted
//!   state. Production installs a bounded provider; tests install fixtures.
//!   The gate never opens a store, database, segment file, PTY hook, or
//!   native library.
//! - [`history_page_value`], the bounded Lua-shaped page crossing the bridge.
//!   Records are point-in-time snapshots: no streaming, subscription, watch,
//!   tail-follow, or cross-call cursor exists.
//!
//! A missing provider is unavailable, not empty: callers receive the typed
//! `E_HISTORY_UNAVAILABLE` denial rather than a fabricated empty page.

use std::cell::{Cell, RefCell};

use bitty_lua::{BridgeError, HistoryQuerySource, LuaValue};
use bitty_plugin_host::{
    Freshness, HistoryCaps, HistoryError, HistorySnapshot, HistorySource, SnapshotPage, StoredRow,
};

/// Maximum rows one query returns (RFC-0004 shape, W-146 reused bounds).
///
/// Reuses the accepted W-137/W-139 placeholder bounds the three official
/// plugins mirror (`MAX_ROWS_PER_QUERY`). The gate rejects over-bound
/// requests; it never clamps silently.
pub const HISTORY_MAX_ROWS_PER_QUERY: u32 = 16;
/// Maximum result bytes one query returns.
pub const HISTORY_MAX_BYTES_PER_QUERY: u32 = 4096;
/// Maximum bytes of one record before marked truncation.
pub const HISTORY_MAX_BYTES_PER_ROW: u32 = 256;
/// Maximum successful queries per plugin per rate window.
pub const HISTORY_MAX_QUERIES_PER_WINDOW: u64 = 4;
/// Maximum successful result bytes per plugin per rate window.
pub const HISTORY_MAX_BYTES_PER_WINDOW: u64 = 8192;
/// Rate-window length for the per-plugin history budgets.
pub const HISTORY_RATE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
/// Maximum rows one bounded provider retains across all sources.
pub const HISTORY_SNAPSHOT_MAX_ROWS: usize = 4096;

/// Composition-root seam over already-persisted history state.
///
/// Implementations return a snapshot of redacted rows. `snapshot` must not
/// block on I/O beyond the bridge deadline and must never expose content the
/// user did not opt in to capture. [`BoundedHistory`] is the in-memory
/// production-shaped implementation; tests provide their own fixtures.
pub trait HistoryProvider {
    /// Current point-in-time snapshot view.
    ///
    /// # Errors
    ///
    /// Returns a typed `E_HISTORY_UNAVAILABLE` when no backing source is
    /// wired or the store is absent.
    fn snapshot(&self) -> Result<HistorySnapshot, BridgeError>;

    /// Whether opt-in capture currently holds for `source`.
    ///
    /// Terminal-derived sources default closed in Core policy; a provider
    /// that serves rows must override this only where capture is actually
    /// opted in. KV is plugin-authored and defaults open.
    fn capture_enabled(&self, source: HistorySource) -> bool {
        matches!(source, HistorySource::PluginKv)
    }
}

/// In-memory bounded provider used by the composition root and tests.
///
/// The default state is unavailable (no store). [`BoundedHistory::publish`]
/// installs a committed snapshot and marks the provider available, so
/// "absent source" stays a typed denial while "present but empty" is a
/// successful empty page. Rows are caller-redacted before they enter here.
#[derive(Debug)]
pub struct BoundedHistory {
    snapshot: RefCell<HistorySnapshot>,
    available: Cell<bool>,
    capture_transcript: Cell<bool>,
    capture_commands: Cell<bool>,
    rows: Cell<usize>,
}

impl Default for BoundedHistory {
    fn default() -> Self {
        Self::unavailable()
    }
}

impl BoundedHistory {
    /// Provider with no backing store: every query denies unavailable.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            snapshot: RefCell::new(HistorySnapshot::new()),
            available: Cell::new(false),
            capture_transcript: Cell::new(false),
            capture_commands: Cell::new(false),
            rows: Cell::new(0),
        }
    }

    /// Available, empty provider with the given terminal capture opt-ins.
    #[must_use]
    pub fn empty(capture_transcript: bool, capture_commands: bool) -> Self {
        Self {
            snapshot: RefCell::new(HistorySnapshot::new()),
            available: Cell::new(true),
            capture_transcript: Cell::new(capture_transcript),
            capture_commands: Cell::new(capture_commands),
            rows: Cell::new(0),
        }
    }

    /// Replace the committed snapshot (replace, never append) and mark the
    /// provider available.
    pub fn publish(&self, snapshot: HistorySnapshot) {
        *self.snapshot.borrow_mut() = snapshot;
        self.available.set(true);
        self.rows.set(0);
    }

    /// Append one already-redacted row, bounded by
    /// [`HISTORY_SNAPSHOT_MAX_ROWS`]. Returns whether it was accepted.
    pub fn push(&self, source: HistorySource, row: StoredRow) -> bool {
        let next = self.rows.get().saturating_add(1);
        if next > HISTORY_SNAPSHOT_MAX_ROWS {
            return false;
        }
        self.snapshot.borrow_mut().push(source, row);
        self.rows.set(next);
        self.available.set(true);
        true
    }

    /// Set the terminal-derived capture opt-ins.
    pub fn set_capture(&self, capture_transcript: bool, capture_commands: bool) {
        self.capture_transcript.set(capture_transcript);
        self.capture_commands.set(capture_commands);
    }

    /// Mark the provider unavailable (for example after a purge or uninstall).
    pub fn clear(&self) {
        *self.snapshot.borrow_mut() = HistorySnapshot::new();
        self.rows.set(0);
        self.available.set(false);
    }
}

impl HistoryProvider for BoundedHistory {
    fn snapshot(&self) -> Result<HistorySnapshot, BridgeError> {
        if !self.available.get() {
            return Err(history_unavailable());
        }
        Ok(self.snapshot.borrow().clone())
    }

    fn capture_enabled(&self, source: HistorySource) -> bool {
        match source {
            HistorySource::Transcript => self.capture_transcript.get(),
            HistorySource::CommandHistory => self.capture_commands.get(),
            HistorySource::PluginKv => true,
        }
    }
}

/// Typed denial used whenever no history source is wired.
#[must_use]
pub fn history_unavailable() -> BridgeError {
    BridgeError::new(
        "runtime",
        "E_HISTORY_UNAVAILABLE",
        "E_HISTORY_UNAVAILABLE denied for level 'third-party-lua' (family 'history')",
    )
}

/// Map a gate error onto the bridge error table, preserving typed denials.
///
/// `Disabled` is a surface diagnostic (version mismatch or malformed
/// scope/source), never a denial category; the bridge reports it as a
/// validation error with the diagnostic only.
#[must_use]
pub fn history_error(error: HistoryError) -> BridgeError {
    match error {
        HistoryError::Denied(denial) => {
            BridgeError::new("runtime", denial.kind().code(), denial.message())
        }
        HistoryError::Disabled { diagnostic } => {
            BridgeError::new("validation", "E_DEF_INVALID", diagnostic)
        }
    }
}

/// Build the bounded Lua page for one successful snapshot read.
///
/// `ok = true` is additive to the accepted W-139 page shape; the three
/// official plugins read `records`, `total_in_scope`, and `freshness`, and
/// treat any thrown error table as a typed denial. Empty pages are a
/// successful result with an empty `records` array.
#[must_use]
pub fn history_page_value(page: &SnapshotPage) -> LuaValue {
    let records = page
        .records
        .iter()
        .map(|record| {
            let mut attribution = vec![
                (
                    LuaValue::String("panel".to_string()),
                    LuaValue::String(record.attribution.panel.clone()),
                ),
                (
                    LuaValue::String("workspace".to_string()),
                    LuaValue::String(record.attribution.workspace.clone()),
                ),
                (
                    LuaValue::String("recorded_at".to_string()),
                    LuaValue::Integer(
                        i64::try_from(record.attribution.recorded_at).unwrap_or(i64::MAX),
                    ),
                ),
            ];
            if let Some(command) = &record.attribution.command {
                attribution.push((
                    LuaValue::String("command".to_string()),
                    LuaValue::String(command.clone()),
                ));
            }
            if let Some(actor) = &record.attribution.actor {
                attribution.push((
                    LuaValue::String("actor".to_string()),
                    LuaValue::String(actor.clone()),
                ));
            }
            LuaValue::table([
                (
                    "seq",
                    LuaValue::Integer(i64::try_from(record.seq).unwrap_or(i64::MAX)),
                ),
                ("body", LuaValue::String(record.body.clone())),
                ("truncated", LuaValue::Bool(record.truncated)),
                ("redacted", LuaValue::Bool(record.redacted)),
                ("label", LuaValue::String(record.label.as_str().to_string())),
                ("attribution", LuaValue::Table(attribution)),
            ])
        })
        .collect();
    LuaValue::table([
        ("ok", LuaValue::Bool(true)),
        ("records", LuaValue::array(records)),
        (
            "total_in_scope",
            LuaValue::Integer(i64::try_from(page.total_in_scope).unwrap_or(i64::MAX)),
        ),
        (
            "freshness",
            LuaValue::String(
                match page.freshness {
                    Freshness::PointInTimeNoGuarantee => "point-in-time-no-guarantee",
                }
                .to_string(),
            ),
        ),
    ])
}

/// Bridge source -> Core gate source (exhaustive, no string dispatch).
#[must_use]
pub const fn history_source(source: HistoryQuerySource) -> HistorySource {
    match source {
        HistoryQuerySource::Transcript => HistorySource::Transcript,
        HistoryQuerySource::CommandHistory => HistorySource::CommandHistory,
        HistoryQuerySource::PluginKv => HistorySource::PluginKv,
    }
}

/// Gate ceilings used by every plugin generation.
#[must_use]
pub fn history_caps() -> HistoryCaps {
    HistoryCaps::new(
        HISTORY_MAX_ROWS_PER_QUERY,
        HISTORY_MAX_BYTES_PER_QUERY,
        HISTORY_MAX_BYTES_PER_ROW,
        HISTORY_MAX_QUERIES_PER_WINDOW,
        HISTORY_MAX_BYTES_PER_WINDOW,
    )
    .expect("history caps are nonzero constants")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_plugin_host::{
        Attribution, HISTORY_READ_VERSION, HistoryCaps, HistoryGate, HistoryGrant, HistoryScope,
        PluginId, QueryOp, SnapshotQuery, TrustLevel,
    };

    fn row(panel: &str, seq: u64, body: &str) -> StoredRow {
        StoredRow {
            owner: None,
            panel: panel.to_string(),
            workspace: "ws-1".to_string(),
            seq,
            redacted_body: body.to_string(),
            attribution: Attribution {
                panel: panel.to_string(),
                workspace: "ws-1".to_string(),
                command: None,
                recorded_at: seq,
                actor: None,
            },
            purged: false,
        }
    }

    #[test]
    fn unavailable_provider_denies_typed_unavailable() {
        let provider = BoundedHistory::unavailable();
        let error = provider.snapshot().expect_err("unavailable");
        assert_eq!(error.class, "runtime");
        assert_eq!(error.code, "E_HISTORY_UNAVAILABLE");
        assert!(!error.message.contains("transcript"));
    }

    #[test]
    fn empty_provider_is_successful_not_unavailable() {
        let provider = BoundedHistory::empty(true, true);
        assert_eq!(
            provider.snapshot().expect("available empty"),
            HistorySnapshot::new()
        );
        assert!(provider.capture_enabled(HistorySource::Transcript));
        assert!(provider.capture_enabled(HistorySource::CommandHistory));
        assert!(provider.capture_enabled(HistorySource::PluginKv));
    }

    #[test]
    fn terminal_capture_defaults_closed() {
        let provider = BoundedHistory::unavailable();
        assert!(!provider.capture_enabled(HistorySource::Transcript));
        assert!(!provider.capture_enabled(HistorySource::CommandHistory));
        assert!(provider.capture_enabled(HistorySource::PluginKv));
    }

    #[test]
    fn publish_replaces_snapshot_and_clear_unavailable() {
        let provider = BoundedHistory::unavailable();
        let mut first = HistorySnapshot::new();
        first.push(HistorySource::Transcript, row("pane-a", 0, "one"));
        provider.publish(first.clone());
        assert_eq!(provider.snapshot().expect("available"), first);
        let mut second = HistorySnapshot::new();
        second.push(HistorySource::CommandHistory, row("pane-a", 1, "two"));
        provider.publish(second.clone());
        assert_eq!(provider.snapshot().expect("available"), second);
        provider.clear();
        assert_eq!(
            provider.snapshot().expect_err("cleared").code,
            "E_HISTORY_UNAVAILABLE"
        );
    }

    #[test]
    fn page_shape_is_additive_and_labeled() {
        let caps = history_caps();
        let mut gate = HistoryGate::new(caps);
        let plugin = PluginId::new("xuepoo.test").expect("plugin id");
        let scope = HistoryScope::new(HistorySource::Transcript, Some("pane-a"), Some("ws-1"))
            .expect("scope");
        gate.issue_grant(HistoryGrant::standing(plugin.clone(), scope.clone()));
        gate.set_capture(HistorySource::Transcript, true);
        let mut store = HistorySnapshot::new();
        store.push(HistorySource::Transcript, row("pane-a", 0, "body"));
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &SnapshotQuery {
                    client_version: HISTORY_READ_VERSION,
                    scope,
                    row_start: 0,
                    row_count: 1,
                    max_bytes: 1024,
                    op: QueryOp::List,
                },
                &store,
            )
            .expect("page");
        let value = history_page_value(&page);
        assert_eq!(value.get("ok"), Some(&LuaValue::Bool(true)));
        assert_eq!(
            value.get("freshness").and_then(|value| match value {
                LuaValue::String(text) => Some(text.as_str()),
                _ => None,
            }),
            Some("point-in-time-no-guarantee")
        );
        let Some(LuaValue::Table(records)) = value.get("records") else {
            panic!("records array");
        };
        assert_eq!(records.len(), 1);
        let Some(LuaValue::Table(record)) = records.first().map(|(_, value)| value) else {
            panic!("record table");
        };
        let record = LuaValue::Table(record.clone());
        assert_eq!(
            record.get("label").and_then(|value| match value {
                LuaValue::String(text) => Some(text.as_str()),
                _ => None,
            }),
            Some("untrusted-observation")
        );
        assert_eq!(record.get("redacted"), Some(&LuaValue::Bool(true)));
    }

    #[test]
    fn caps_are_nonzero_and_bounded() {
        let caps = history_caps();
        assert_eq!(caps.max_rows_per_query, HISTORY_MAX_ROWS_PER_QUERY);
        assert_eq!(caps.max_bytes_per_query, HISTORY_MAX_BYTES_PER_QUERY);
        assert_eq!(caps.max_bytes_per_row, HISTORY_MAX_BYTES_PER_ROW);
        assert_eq!(caps.max_queries_per_window, HISTORY_MAX_QUERIES_PER_WINDOW);
        assert_eq!(caps.max_bytes_per_window, HISTORY_MAX_BYTES_PER_WINDOW);
        let _ = HistoryCaps::new(1, 1, 1, 1, 1).expect("valid");
    }
}
