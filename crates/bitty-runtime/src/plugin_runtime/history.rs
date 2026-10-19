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
use std::collections::VecDeque;

use bitty_lua::{BridgeError, HistoryQuerySource, LuaValue};
use bitty_plugin_host::{
    Freshness, HistoryCaps, HistoryError, HistorySnapshot, HistorySource, SnapshotPage, StoredRow,
    secrets::scrub_text_with_secrets,
};
use bitty_term_state::{ScrollbackLine, State};

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
/// Maximum caller-named query scopes cached as standing grants per plugin and
/// rate window.
///
/// The accepted history heads carry no grant parameter, so the host caches
/// one grant per distinct `(source, panel, workspace)` scope. A fixed bound
/// keeps both the cache and [`HistoryGate`](bitty_plugin_host::HistoryGate)
/// grant storage from growing with caller-chosen scopes; reaching it clears
/// only grants, never rate usage.
pub const HISTORY_GRANT_SCOPE_CACHE_MAX: usize = 64;

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

    /// Whether this provider has a real backing source for `source`.
    ///
    /// A provider may be present while one source has no implementation at
    /// all (for example command history before submitted-command spans are
    /// tracked). That source denies with the typed
    /// `E_HISTORY_UNAVAILABLE`; it is never served as a fabricated empty
    /// page. Capture opt-in is a separate axis: an available transcript
    /// source with capture disabled is an empty page, not a denial.
    fn source_available(&self, _source: HistorySource) -> bool {
        true
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
    /// Retained rows in insertion order, so drop-oldest retention is O(1)
    /// and evicts the globally oldest row even across sources. Snapshots are
    /// materialized only when a query asks for one.
    rows: RefCell<VecDeque<(HistorySource, StoredRow)>>,
    available: Cell<bool>,
    capture_transcript: Cell<bool>,
    capture_commands: Cell<bool>,
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
            rows: RefCell::new(VecDeque::new()),
            available: Cell::new(false),
            capture_transcript: Cell::new(false),
            capture_commands: Cell::new(false),
        }
    }

    /// Available, empty provider with the given terminal capture opt-ins.
    #[must_use]
    pub fn empty(capture_transcript: bool, capture_commands: bool) -> Self {
        Self {
            rows: RefCell::new(VecDeque::new()),
            available: Cell::new(true),
            capture_transcript: Cell::new(capture_transcript),
            capture_commands: Cell::new(capture_commands),
        }
    }

    /// Replace the committed snapshot (replace, never append) and mark the
    /// provider available.
    ///
    /// Incoming rows still pass through the bounded ring, so a publisher can
    /// never install more than [`HISTORY_SNAPSHOT_MAX_ROWS`] rows.
    pub fn publish(&self, snapshot: HistorySnapshot) {
        let rows = snapshot.into_rows();
        let mut ring = self.rows.borrow_mut();
        ring.clear();
        for (source, row) in rows {
            if ring.len() >= HISTORY_SNAPSHOT_MAX_ROWS {
                ring.pop_front();
            }
            ring.push_back((source, row));
        }
        drop(ring);
        self.available.set(true);
    }

    /// Append one already-redacted row with O(1) drop-oldest retention once
    /// [`HISTORY_SNAPSHOT_MAX_ROWS`] is reached.
    ///
    /// Returns `true` once the row is retained; the ring never rejects an
    /// otherwise-valid row and never grows without bound.
    pub fn push(&self, source: HistorySource, row: StoredRow) -> bool {
        let mut ring = self.rows.borrow_mut();
        if ring.len() >= HISTORY_SNAPSHOT_MAX_ROWS {
            ring.pop_front();
        }
        ring.push_back((source, row));
        drop(ring);
        self.available.set(true);
        true
    }

    /// Clear every retained row while keeping the provider available.
    ///
    /// Capture uses this on a buffer epoch change (clear/reset/reflow): the
    /// rows described the previous buffer, so they are not carried into the
    /// new one.
    pub fn reset_rows(&self) {
        self.rows.borrow_mut().clear();
    }

    /// Number of retained rows across every source.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.borrow().len()
    }

    /// Whether no rows are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.borrow().is_empty()
    }

    /// Set the terminal-derived capture opt-ins.
    pub fn set_capture(&self, capture_transcript: bool, capture_commands: bool) {
        self.capture_transcript.set(capture_transcript);
        self.capture_commands.set(capture_commands);
    }

    /// Mark the provider unavailable (for example after a purge or uninstall).
    pub fn clear(&self) {
        self.rows.borrow_mut().clear();
        self.available.set(false);
    }
}

/// Real terminal-state transcript provider (CTX-1352, issue #2247).
///
/// Samples the active session's completed scrollback lines into a bounded,
/// redacted in-memory ring. There is no persistence, PTY hook, streaming, or
/// cursor: callers ask for a point-in-time page through the Core gate.
///
/// Capture is opt-in by configuration. When the opt-in is off the provider
/// still answers with a successful empty page (the owner directive for this
/// slice); the gate remains authoritative for grants, scopes, safe mode,
/// bounds, and budgets. Command history has no backing source yet (the
/// terminal tracks OSC 133 boundaries but not submitted command spans), so it
/// denies with the typed `E_HISTORY_UNAVAILABLE` instead of inventing rows.
/// KV stays unavailable in this slice for the same reason.
#[derive(Debug)]
pub struct TranscriptHistory {
    history: BoundedHistory,
    capture_transcript: bool,
    /// Reserved for the future command-source implementation; the source
    /// stays unavailable until submitted-command spans are tracked.
    capture_commands: bool,
    /// Next scrollback line id to sample, if sampling has started.
    next_line_id: Cell<Option<u64>>,
    /// Buffer epoch the tracker belongs to (clear/reset/reflow resets it).
    epoch: Cell<u64>,
    /// Monotonic row sequence across captures.
    next_seq: Cell<u64>,
}

impl TranscriptHistory {
    /// Build a provider from the effective capture opt-ins.
    #[must_use]
    pub fn new(capture_transcript: bool, capture_commands: bool) -> Self {
        Self {
            history: BoundedHistory::empty(true, false),
            capture_transcript,
            capture_commands,
            next_line_id: Cell::new(None),
            epoch: Cell::new(u64::MAX),
            next_seq: Cell::new(0),
        }
    }

    /// Sample newly completed scrollback lines from `state`.
    ///
    /// `panel` and `workspace` are the row attribution scope ids. Lines
    /// already sampled are skipped by scrollback id; an epoch change
    /// (clear/reset/reflow) drops the previous snapshot and starts over.
    /// Returns the number of rows accepted into the ring.
    pub fn capture_state(&self, state: &State, panel: &str, workspace: &str) -> usize {
        if !self.capture_transcript {
            return 0;
        }
        if state.buffer_epoch() != self.epoch.get() {
            self.history.reset_rows();
            self.next_line_id.set(None);
            self.epoch.set(state.buffer_epoch());
        }
        let mut accepted = 0;
        let mut next = self.next_line_id.get();
        for line in state.scrollback() {
            if next.is_some_and(|next| line.id < next) {
                continue;
            }
            let body = redact_transcript_line(line);
            next = Some(line.id.saturating_add(1));
            if body.is_empty() {
                continue;
            }
            let seq = self.next_seq.get();
            self.next_seq.set(seq.saturating_add(1));
            let row = StoredRow {
                owner: None,
                panel: panel.to_string(),
                workspace: workspace.to_string(),
                seq,
                redacted_body: body,
                attribution: bitty_plugin_host::Attribution {
                    panel: panel.to_string(),
                    workspace: workspace.to_string(),
                    command: None,
                    recorded_at: seq,
                    actor: None,
                },
                purged: false,
            };
            let _ = self.history.push(HistorySource::Transcript, row);
            accepted += 1;
        }
        if next.is_some() {
            self.next_line_id.set(next);
        }
        accepted
    }

    /// Borrow the bounded ring (tests and composition-root diagnostics).
    #[must_use]
    pub fn history(&self) -> &BoundedHistory {
        &self.history
    }

    /// Configured command-capture opt-in. Command history has no backing
    /// source yet, so this stays a recorded preference rather than an
    /// availability switch.
    #[must_use]
    pub fn capture_commands(&self) -> bool {
        self.capture_commands
    }
}

impl HistoryProvider for TranscriptHistory {
    fn snapshot(&self) -> Result<HistorySnapshot, BridgeError> {
        self.history.snapshot()
    }

    /// Terminal capture opt-ins do not gate availability for this provider:
    /// a disabled setting yields an empty page. The rows themselves only
    /// enter the ring while capture is enabled.
    fn capture_enabled(&self, source: HistorySource) -> bool {
        matches!(source, HistorySource::Transcript)
    }

    fn source_available(&self, source: HistorySource) -> bool {
        match source {
            HistorySource::Transcript => true,
            HistorySource::CommandHistory | HistorySource::PluginKv => false,
        }
    }
}

/// Project one scrollback line to redacted text, bounded by
/// [`HISTORY_MAX_BYTES_PER_ROW`].
///
/// Redaction runs before truncation, using the host's existing secret
/// scrubbing policy. Wide spacers are skipped, combining marks ride their
/// base glyph, and trailing blanks trim.
fn redact_transcript_line(line: &ScrollbackLine) -> String {
    let mut text = String::new();
    for cell in line.cells.iter() {
        if cell.spacer {
            continue;
        }
        text.push(cell.glyph);
        for mark in cell.zerowidth.iter() {
            text.push(*mark);
        }
    }
    let redacted = scrub_text_with_secrets(text.trim_end(), &[]);
    truncate_row(redacted)
}

/// Truncate at a UTF-8 boundary to [`HISTORY_MAX_BYTES_PER_ROW`].
fn truncate_row(mut text: String) -> String {
    let cap = HISTORY_MAX_BYTES_PER_ROW as usize;
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

impl HistoryProvider for BoundedHistory {
    fn snapshot(&self) -> Result<HistorySnapshot, BridgeError> {
        if !self.available.get() {
            return Err(history_unavailable());
        }
        let mut snapshot = HistorySnapshot::new();
        for (source, row) in self.rows.borrow().iter() {
            snapshot.push(*source, row.clone());
        }
        Ok(snapshot)
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

    fn feed_line(state: &mut bitty_term_state::State, text: &str) {
        use bitty_vt::{ControlChar, GraphemeCell, TerminalAction};
        for ch in text.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
        }
        state.apply(&TerminalAction::PrintControl(ControlChar(0x0D)));
        state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }

    fn state_with_lines(lines: usize) -> bitty_term_state::State {
        let mut state = bitty_term_state::State::with_scrollback_lines(100);
        for index in 0..lines {
            feed_line(&mut state, &format!("line{index:02}"));
        }
        state
    }

    fn snapshot_rows(provider: &TranscriptHistory) -> Vec<(HistorySource, StoredRow)> {
        provider
            .snapshot()
            .expect("available transcript")
            .into_rows()
    }

    #[test]
    fn transcript_captures_completed_scrollback_rows_once() {
        let provider = TranscriptHistory::new(true, false);
        let mut state = state_with_lines(30);
        let retained = state.scrollback_len();
        assert!(retained > 0);
        assert_eq!(provider.capture_state(&state, "1", "7"), retained);
        let rows = snapshot_rows(&provider);
        assert_eq!(rows.len(), retained);
        assert_eq!(rows[0].0, HistorySource::Transcript);
        assert_eq!(rows[0].1.redacted_body, "line00");
        assert_eq!(rows[0].1.panel, "1");
        assert_eq!(rows[0].1.workspace, "7");
        assert_eq!(
            rows[retained - 1].1.redacted_body,
            format!("line{:02}", retained - 1)
        );

        // A second sample of the same buffer adds nothing.
        assert_eq!(provider.capture_state(&state, "1", "7"), 0);
        assert_eq!(snapshot_rows(&provider).len(), retained);

        // New completed lines are the only rows appended.
        feed_line(&mut state, "line30");
        feed_line(&mut state, "line31");
        assert_eq!(provider.capture_state(&state, "1", "7"), 2);
        let rows = snapshot_rows(&provider);
        assert_eq!(rows.len(), retained + 2);
        // The two oldest grid rows scrolled into history; later input has
        // not reached the ring yet.
        assert_eq!(rows[retained].1.redacted_body, "line07");
        assert_eq!(rows[retained + 1].1.redacted_body, "line08");
        assert!(provider.history().len() <= HISTORY_SNAPSHOT_MAX_ROWS);
    }

    #[test]
    fn capture_off_serves_an_available_empty_snapshot() {
        let provider = TranscriptHistory::new(false, false);
        let state = state_with_lines(30);
        assert_eq!(provider.capture_state(&state, "1", "7"), 0);
        let snapshot = provider.snapshot().expect("provider is available");
        assert!(snapshot.is_empty());
        assert!(provider.capture_enabled(HistorySource::Transcript));
    }

    #[test]
    fn transcript_redacts_secret_shaped_tokens_before_they_enter_the_ring() {
        use bitty_term_state::{Cell, Style};

        fn scrollback_line(text: &str) -> ScrollbackLine {
            let cells: Vec<Cell> = text
                .chars()
                .map(|ch| {
                    let mut cell = Cell::erased(Style::default());
                    cell.glyph = ch;
                    cell
                })
                .collect();
            ScrollbackLine {
                id: 0,
                cells: cells.into_boxed_slice(),
                wrapped: false,
            }
        }

        let body = redact_transcript_line(&scrollback_line("export API_KEY=sk-live-SECRET-abc123"));
        assert!(!body.contains("sk-live-SECRET-abc123"), "{body}");
        assert!(body.contains("[redacted]"), "{body}");

        // The same bytes flow through capture unchanged: only redacted text
        // ever reaches the ring.
        let provider = TranscriptHistory::new(true, false);
        let mut state = bitty_term_state::State::with_scrollback_lines(100);
        for index in 0..40 {
            feed_line(&mut state, &format!("line{index:02} sk-live-SECRET-abc123"));
        }
        assert!(provider.capture_state(&state, "1", "7") > 0);
        let rendered = format!("{:?}", snapshot_rows(&provider));
        assert!(!rendered.contains("sk-live-SECRET-abc123"), "{rendered}");
        assert!(rendered.contains("[redacted]"), "{rendered}");
    }

    #[test]
    fn capture_is_scoped_per_panel_and_never_crosses_rows() {
        use bitty_plugin_host::{
            HISTORY_READ_VERSION, HistoryGate, HistoryGrant, HistoryScope, PluginId, QueryOp,
            SnapshotQuery, TrustLevel,
        };

        let provider = TranscriptHistory::new(true, false);
        let mut state = state_with_lines(30);
        let per_side = state.scrollback_len();
        assert_eq!(provider.capture_state(&state, "1", "7"), per_side);
        // Complete more lines under a second panel scope; the same state
        // keeps its trackers, so only the new rows join the ring.
        for index in 30..(30 + per_side) {
            feed_line(&mut state, &format!("line{index:02}"));
        }
        assert_eq!(provider.capture_state(&state, "2", "7"), per_side);

        let mut gate = HistoryGate::new(history_caps());
        let plugin = PluginId::new("xuepoo.test").expect("plugin id");
        let scope =
            HistoryScope::new(HistorySource::Transcript, Some("1"), Some("7")).expect("scope");
        gate.issue_grant(HistoryGrant::standing(plugin.clone(), scope.clone()));
        gate.set_capture(HistorySource::Transcript, true);
        let page = gate
            .query(
                &plugin,
                TrustLevel::ThirdPartyLua,
                &SnapshotQuery {
                    client_version: HISTORY_READ_VERSION,
                    scope,
                    row_start: 0,
                    row_count: HISTORY_MAX_ROWS_PER_QUERY,
                    max_bytes: HISTORY_MAX_BYTES_PER_QUERY,
                    op: QueryOp::List,
                },
                &provider.snapshot().expect("available"),
            )
            .expect("scoped page");
        assert_eq!(page.total_in_scope, per_side as u64);
        assert_eq!(page.records.len(), per_side);
        assert!(
            page.records
                .iter()
                .all(|record| record.attribution.panel == "1")
        );
    }

    #[test]
    fn command_history_reports_typed_unavailability_while_transcript_serves() {
        let provider = TranscriptHistory::new(true, true);
        assert!(provider.source_available(HistorySource::Transcript));
        assert!(!provider.source_available(HistorySource::CommandHistory));
        assert!(!provider.capture_enabled(HistorySource::CommandHistory));
        assert!(!provider.source_available(HistorySource::PluginKv));
        assert!(provider.capture_commands());
    }

    #[test]
    fn bounded_ring_evicts_the_oldest_row_globally() {
        let provider = BoundedHistory::empty(true, true);
        for seq in 0..=(HISTORY_SNAPSHOT_MAX_ROWS as u64) {
            assert!(provider.push(HistorySource::Transcript, row("pane-a", seq, "line")));
        }
        assert_eq!(provider.len(), HISTORY_SNAPSHOT_MAX_ROWS);
        let rows = provider.snapshot().expect("available").into_rows();
        assert_eq!(rows.len(), HISTORY_SNAPSHOT_MAX_ROWS);
        assert_eq!(rows[0].1.seq, 1);
        assert_eq!(
            rows[HISTORY_SNAPSHOT_MAX_ROWS - 1].1.seq,
            HISTORY_SNAPSHOT_MAX_ROWS as u64
        );
    }

    fn snapshot_of(count: u64) -> HistorySnapshot {
        let mut snapshot = HistorySnapshot::new();
        for seq in 0..count {
            snapshot.push(HistorySource::Transcript, row("pane-a", seq, "line"));
        }
        snapshot
    }

    #[test]
    fn publish_is_bounded_at_the_snapshot_cap() {
        let provider = BoundedHistory::unavailable();

        provider.publish(snapshot_of(HISTORY_SNAPSHOT_MAX_ROWS as u64));
        assert_eq!(provider.len(), HISTORY_SNAPSHOT_MAX_ROWS);
        let rows = provider.snapshot().expect("available").into_rows();
        assert_eq!(rows[0].1.seq, 0);
        assert_eq!(
            rows[HISTORY_SNAPSHOT_MAX_ROWS - 1].1.seq,
            HISTORY_SNAPSHOT_MAX_ROWS as u64 - 1
        );

        provider.publish(snapshot_of(HISTORY_SNAPSHOT_MAX_ROWS as u64 + 1));
        assert_eq!(provider.len(), HISTORY_SNAPSHOT_MAX_ROWS);
        let rows = provider.snapshot().expect("available").into_rows();
        assert_eq!(rows[0].1.seq, 1);
        assert_eq!(
            rows[HISTORY_SNAPSHOT_MAX_ROWS - 1].1.seq,
            HISTORY_SNAPSHOT_MAX_ROWS as u64
        );

        provider.publish(snapshot_of(5000));
        assert_eq!(provider.len(), HISTORY_SNAPSHOT_MAX_ROWS);
        let rows = provider.snapshot().expect("available").into_rows();
        assert_eq!(rows[0].1.seq, 904);
        assert_eq!(rows[HISTORY_SNAPSHOT_MAX_ROWS - 1].1.seq, 4999);
    }

    #[test]
    fn publish_then_push_keeps_the_same_ring_bound() {
        let provider = BoundedHistory::unavailable();
        provider.publish(snapshot_of(HISTORY_SNAPSHOT_MAX_ROWS as u64));
        assert_eq!(provider.len(), HISTORY_SNAPSHOT_MAX_ROWS);
        assert!(provider.push(
            HistorySource::Transcript,
            row("pane-a", HISTORY_SNAPSHOT_MAX_ROWS as u64, "new")
        ));
        assert_eq!(provider.len(), HISTORY_SNAPSHOT_MAX_ROWS);
        let rows = provider.snapshot().expect("available").into_rows();
        assert_eq!(rows[0].1.seq, 1);
        assert_eq!(
            rows[HISTORY_SNAPSHOT_MAX_ROWS - 1].1.seq,
            HISTORY_SNAPSHOT_MAX_ROWS as u64
        );
        assert_eq!(rows[HISTORY_SNAPSHOT_MAX_ROWS - 1].1.redacted_body, "new");
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
