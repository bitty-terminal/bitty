#![forbid(unsafe_code)]
//! Differential M1 VT oracle corpus (CTX-0573, Issue #1133).
//!
//! A genuinely differential oracle for the M1 VT surface: every scenario's
//! expected bytes/state derive from an external reference terminal or the
//! authoritative control-sequence specification — never from Bitty's own
//! output (no self-golden). The runner replays the scenario bytes through the
//! shared compat-lab harness ([`crate::parse_bounded`] and
//! [`crate::actions_to_snapshot`]) and diffs the observed state against the
//! recorded expectation, emitting one machine-readable result per scenario.
//!
//! ## Layout
//!
//! ```text
//! tests/compat/oracle/README.md              # layout, provenance, how to extend
//! tests/compat/oracle/scenarios/<id>.bin     # raw VT bytes for one scenario
//! tests/compat/oracle/scenarios/<id>.expected# expectation (spec/capture derived)
//! ```
//!
//! Discovery is `CARGO_MANIFEST_DIR`-anchored through
//! [`crate::workspace_root`], sorted, and bounded to [`MAX_SCENARIOS`]; no
//! clock, RNG, network, display, or host path participates.
//!
//! ## Areas
//!
//! The scenario set covers the M1 protocol matrix
//! (`docs/specifications/compatibility-milestone-rfc.md`): synchronized
//! updates (DECSET 2026), OSC 10/11 color query/set, OSC 0/2 title, mouse
//! tracking 1000/1002/1003 and encodings X10/SGR/UTF-8/urxvt, mode 1007
//! alternate scroll, DECSCUSR cursor style, alternate screen 1049/47, and
//! DECCKM cursor keys.
//!
//! ## Provenance
//!
//! Each `.expected` file carries a `provenance:` line naming either the
//! authoritative specification section (`spec|<citation>`) or the captured
//! reference terminal and revision (`capture|<terminal> <version> <rev>`).
//! See `tests/compat/oracle/README.md` for the corpus-wide provenance table.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use bitty_term_state::{Modes, Snapshot};
use bitty_vt::{
    CursorStyle, DynamicColorOp, DynamicColorTarget, Mode, MouseCoordinateEncoding,
    MouseTrackingMode, TerminalAction,
};

/// Workspace-relative directory holding the oracle scenarios.
pub const SCENARIO_DIR_REL: &str = "tests/compat/oracle/scenarios";

/// Maximum scenarios honored per run (bounded discovery).
pub const MAX_SCENARIOS: usize = 64;

/// Maximum expected-file bytes accepted.
pub const MAX_EXPECTED_BYTES: usize = 16 * 1024;

/// Summary schema version.
pub const SUMMARY_VERSION: u32 = 1;

/// Scope label recorded in the summary.
pub const SUMMARY_SCOPE: &str = "M1 VT differential oracle corpus (CTX-0573)";

/// Oracle areas in priority order (M1 protocol matrix order).
pub const AREAS: &[&str] = &[
    "synchronized-update",
    "osc-color",
    "osc-title",
    "mouse-tracking",
    "mouse-encoding",
    "alternate-scroll",
    "cursor-style",
    "alternate-screen",
    "cursor-keys",
    "device-status",
];

/// Where a scenario's expectation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceKind {
    /// Authoritative control-sequence specification citation.
    Spec,
    /// Captured reference terminal, pinned by version and revision.
    Capture,
}

impl ProvenanceKind {
    /// Lowercase wire form used in the summary JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ProvenanceKind::Spec => "spec",
            ProvenanceKind::Capture => "capture",
        }
    }
}

/// One expectation's provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// Spec or terminal capture.
    pub kind: ProvenanceKind,
    /// Citation text after the `|`, e.g. `xterm patch #411 ctlseqs.txt` or
    /// `xterm 411 (2026/08/23)`. Free-form but recorded verbatim.
    pub source: String,
}

/// Expected state for one scenario.
///
/// `None` on an optional check means the expectation does not constrain it
/// (the spec does not define that field for this scenario), so the runner
/// skips it rather than fabricating a Bitty-derived value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Expected {
    /// Expected grid dimensions `(cols, rows)`; always checked.
    pub grid: (usize, usize),
    /// Expected grid text, or `None` when unchecked.
    pub text: Option<String>,
    /// `true` expects an all-space grid (compact blank assertion).
    pub text_blank: bool,
    /// Expected single-row text `(row_index, text)`, in declaration order.
    ///
    /// The row's leading `text` is compared against the row's cells; the
    /// remainder of the row is required to be blank (space), so a scenario
    /// can assert exact placement without spelling out 80 cells.
    pub grid_rows: Vec<(usize, String)>,
    /// Expected cursor `(row, col, visible)`, or `None` when unchecked.
    pub cursor: Option<(u16, u16, bool)>,
    /// Expected mode register values, in declaration order.
    pub modes: Vec<(String, String)>,
    /// Expected cursor style, or `None` when unchecked.
    pub cursor_style: Option<String>,
    /// Expected title, or `None` when unchecked.
    pub title: Option<String>,
    /// Expected exact canonical action list, or `None` when unchecked.
    pub actions: Option<Vec<String>>,
    /// Expected concatenated reply bytes, or `None` when unchecked.
    pub reply: Option<Vec<u8>>,
}

/// One oracle scenario: bytes plus the externally derived expectation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    /// Stable scenario id (file stem).
    pub id: String,
    /// Area from [`AREAS`].
    pub area: String,
    /// Provenance of the expectation.
    pub provenance: Provenance,
    /// Raw VT bytes.
    pub corpus: Vec<u8>,
    /// Expected state.
    pub expected: Expected,
}

/// Pass/fail for one scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Every checked expectation matched.
    Pass,
    /// At least one checked expectation diverged.
    Fail,
}

impl Status {
    /// Lowercase wire form used in the summary JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
        }
    }
}

/// Result of one individual check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Check name, e.g. `mode alternate_scroll`.
    pub name: String,
    /// Whether the observed value matched.
    pub passed: bool,
    /// Expected value (escaped, bounded).
    pub expected: String,
    /// Observed value (escaped, bounded).
    pub actual: String,
}

/// One scenario result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioOutcome {
    /// Scenario id.
    pub id: String,
    /// Area.
    pub area: String,
    /// Provenance.
    pub provenance: Provenance,
    /// Overall status.
    pub status: Status,
    /// Individual checks in evaluation order.
    pub checks: Vec<CheckResult>,
}

/// Aggregated oracle run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleReport {
    /// Per-scenario outcomes in sorted id order.
    pub outcomes: Vec<ScenarioOutcome>,
    /// Scenarios that passed.
    pub passed: usize,
    /// Scenarios that failed.
    pub failed: usize,
}

impl OracleReport {
    /// Total scenarios evaluated.
    #[must_use]
    pub fn total(&self) -> usize {
        self.outcomes.len()
    }

    /// True when every scenario passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }
}

fn scenario_dir() -> PathBuf {
    crate::workspace_root().join(SCENARIO_DIR_REL)
}

/// Discover scenarios: sorted by id, bounded to [`MAX_SCENARIOS`].
///
/// Only `<id>.bin` files with a sibling `<id>.expected` participate; a
/// `.bin` without an expectation is a hard error so a scenario can never
/// silently lose its oracle.
pub fn load_scenarios() -> Result<Vec<Scenario>, String> {
    let dir = scenario_dir();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("cannot read scenario dir {}: {e}", dir.display()))?;
    let mut bins: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        bins.push(path);
        if bins.len() > MAX_SCENARIOS {
            return Err(format!(
                "more than MAX_SCENARIOS ({MAX_SCENARIOS}) oracle scenarios"
            ));
        }
    }
    bins.sort();
    let mut out = Vec::with_capacity(bins.len());
    for bin in &bins {
        out.push(load_scenario_file(bin)?);
    }
    Ok(out)
}

/// Load a single scenario from an explicit `<id>.bin` path.
///
/// Used by [`load_scenarios`] and by the divergence-guard tests, which load a
/// deliberately divergent oracle from `tests/compat/oracle/divergences/` and
/// assert the runner catches it.
pub fn load_scenario_file(bin: &Path) -> Result<Scenario, String> {
    let id = bin
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("scenario file {:?} has no utf-8 stem", bin))?
        .to_string();
    let corpus = std::fs::read(bin).map_err(|e| format!("read {id}.bin: {e}"))?;
    if corpus.len() > crate::MAX_CORPUS_BYTES {
        return Err(format!(
            "{id}: corpus {} bytes > MAX_CORPUS_BYTES {}",
            corpus.len(),
            crate::MAX_CORPUS_BYTES
        ));
    }
    let expected_path = bin.with_extension("expected");
    let raw = std::fs::read(&expected_path)
        .map_err(|e| format!("{id}: missing {}: {e}", expected_path.display()))?;
    if raw.len() > MAX_EXPECTED_BYTES {
        return Err(format!("{id}: expected file exceeds MAX_EXPECTED_BYTES"));
    }
    let text = std::str::from_utf8(&raw).map_err(|e| format!("{id}: expected utf8: {e}"))?;
    let (area, provenance, expected) = parse_expected(&id, text)?;
    Ok(Scenario {
        id,
        area,
        provenance,
        corpus,
        expected,
    })
}

fn parse_expected(id: &str, text: &str) -> Result<(String, Provenance, Expected), String> {
    let mut area: Option<String> = None;
    let mut provenance: Option<Provenance> = None;
    let mut expected = Expected {
        grid: (bitty_term_state::GRID_COLUMNS, bitty_term_state::GRID_ROWS),
        ..Expected::default()
    };
    let mut saw_grid = false;
    for (lineno, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("{id}:{}: not a `key: value` line", lineno + 1));
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "area" => {
                if !AREAS.contains(&value) {
                    return Err(format!("{id}:{}: unknown area {value:?}", lineno + 1));
                }
                area = Some(value.to_string());
            }
            "provenance" => {
                let (kind, source) = value.split_once('|').ok_or_else(|| {
                    format!("{id}:{}: provenance needs `kind|citation`", lineno + 1)
                })?;
                let kind = match kind.trim() {
                    "spec" => ProvenanceKind::Spec,
                    "capture" => ProvenanceKind::Capture,
                    other => {
                        return Err(format!(
                            "{id}:{}: unknown provenance kind {other:?}",
                            lineno + 1
                        ));
                    }
                };
                provenance = Some(Provenance {
                    kind,
                    source: source.trim().to_string(),
                });
            }
            "grid" => {
                let (w, h) = value
                    .split_once('x')
                    .ok_or_else(|| format!("{id}:{}: grid needs `WxH`", lineno + 1))?;
                let w: usize = w
                    .trim()
                    .parse()
                    .map_err(|_| format!("{id}:{}: bad grid width", lineno + 1))?;
                let h: usize = h
                    .trim()
                    .parse()
                    .map_err(|_| format!("{id}:{}: bad grid height", lineno + 1))?;
                expected.grid = (w, h);
                saw_grid = true;
            }
            "grid_text" => match value {
                "blank" => expected.text_blank = true,
                "unchecked" => {}
                other => {
                    return Err(format!(
                        "{id}:{}: grid_text expects blank|unchecked, got {other:?}",
                        lineno + 1
                    ));
                }
            },
            "text" => expected.text = Some(unescape_text(value)),
            _ if key.starts_with("row ") => {
                let idx: usize = key["row ".len()..]
                    .trim()
                    .parse()
                    .map_err(|_| format!("{id}:{}: bad row index in {key:?}", lineno + 1))?;
                expected.grid_rows.push((idx, unescape_text(value)));
            }
            "cursor" => match value {
                "unchecked" => {}
                spec => {
                    let mut parts = spec.split_whitespace();
                    let row = parts.next().and_then(|p| p.parse::<u16>().ok());
                    let col = parts.next().and_then(|p| p.parse::<u16>().ok());
                    let vis = match parts.next() {
                        Some("visible") => Some(true),
                        Some("hidden") => Some(false),
                        _ => None,
                    };
                    match (row, col, vis) {
                        (Some(r), Some(c), Some(v)) => expected.cursor = Some((r, c, v)),
                        _ => {
                            return Err(format!(
                                "{id}:{}: cursor needs `ROW COL visible|hidden` or unchecked",
                                lineno + 1
                            ));
                        }
                    }
                }
            },
            "mode" => {
                let (name, val) = value
                    .split_once('=')
                    .ok_or_else(|| format!("{id}:{}: mode needs `name = value`", lineno + 1))?;
                expected
                    .modes
                    .push((name.trim().to_string(), val.trim().to_string()));
            }
            "cursor_style" => expected.cursor_style = Some(value.to_string()),
            "title" => expected.title = Some(unescape_text(value)),
            "action" => expected
                .actions
                .get_or_insert_with(Vec::new)
                .push(value.to_string()),
            "reply" => expected.reply = Some(unescape_bytes(value)),
            other => return Err(format!("{id}:{}: unknown key {other:?}", lineno + 1)),
        }
    }
    if !saw_grid {
        return Err(format!("{id}: missing `grid:` line"));
    }
    if expected.grid.0 == 0 || expected.grid.1 == 0 {
        return Err(format!("{id}: grid dimensions must be non-zero"));
    }
    let area = area.ok_or_else(|| format!("{id}: missing `area:` line"))?;
    let provenance = provenance.ok_or_else(|| format!("{id}: missing `provenance:` line"))?;
    Ok((area, provenance, expected))
}

/// Run the full oracle corpus, sorted and bounded.
pub fn run_oracle() -> Result<OracleReport, String> {
    let scenarios = load_scenarios()?;
    let mut outcomes = Vec::with_capacity(scenarios.len());
    let mut passed = 0usize;
    let mut failed = 0usize;
    for scenario in &scenarios {
        let outcome = run_scenario(scenario);
        if outcome.status == Status::Pass {
            passed += 1;
        } else {
            failed += 1;
        }
        outcomes.push(outcome);
    }
    outcomes.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(OracleReport {
        outcomes,
        passed,
        failed,
    })
}

/// Execute one scenario against the Bitty build and diff against its oracle.
#[must_use]
pub fn run_scenario(scenario: &Scenario) -> ScenarioOutcome {
    let mut checks: Vec<CheckResult> = Vec::new();

    // Bounded, deterministic parse through the shared compat-lab harness.
    let actions = std::panic::catch_unwind(|| crate::parse_bounded(&scenario.corpus))
        .expect("parse_bounded must not panic on a bounded corpus");
    let snapshot = crate::actions_to_snapshot(&actions);

    let mut state = bitty_term_state::State::new();
    for action in &actions {
        state.apply(action);
    }
    let replies = state.take_replies();
    let reply_bytes: Vec<u8> = replies.iter().flat_map(|r| r.iter().copied()).collect();

    if let Err(err) = state.check_invariants() {
        checks.push(CheckResult {
            name: "invariants".to_string(),
            passed: false,
            expected: "clean".to_string(),
            actual: format!("{err:?}"),
        });
    }

    check_grid(&scenario.expected, &snapshot, &mut checks);
    check_cursor(&scenario.expected, &snapshot, &mut checks);
    check_modes(
        &scenario.expected,
        &snapshot.modes,
        state.alt_screen_active(),
        &mut checks,
    );
    check_cursor_style(&scenario.expected, &snapshot, &mut checks);
    check_title(&scenario.expected, &snapshot, &mut checks);
    check_actions(&scenario.expected, &actions, &mut checks);
    check_reply(&scenario.expected, &reply_bytes, &mut checks);

    let status = if checks.iter().all(|c| c.passed) {
        Status::Pass
    } else {
        Status::Fail
    };
    ScenarioOutcome {
        id: scenario.id.clone(),
        area: scenario.area.clone(),
        provenance: scenario.provenance.clone(),
        status,
        checks,
    }
}

fn push_check(
    checks: &mut Vec<CheckResult>,
    name: impl Into<String>,
    passed: bool,
    expected: impl Into<String>,
    actual: impl Into<String>,
) {
    checks.push(CheckResult {
        name: name.into(),
        passed,
        expected: expected.into(),
        actual: actual.into(),
    });
}

fn snapshot_to_text(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    for row in 0..snapshot.height {
        for col in 0..snapshot.width {
            let cell = &snapshot.cells[row * snapshot.width + col];
            if cell.spacer {
                continue;
            }
            out.push(cell.glyph);
        }
        if row + 1 < snapshot.height {
            out.push('\n');
        }
    }
    out
}

fn check_grid(expected: &Expected, snapshot: &Snapshot, checks: &mut Vec<CheckResult>) {
    push_check(
        checks,
        "grid",
        (snapshot.width, snapshot.height) == expected.grid,
        format!("{}x{}", expected.grid.0, expected.grid.1),
        format!("{}x{}", snapshot.width, snapshot.height),
    );
    let actual = snapshot_to_text(snapshot);
    if expected.text_blank {
        let blank = actual.chars().all(|c| c == ' ' || c == '\n');
        push_check(
            checks,
            "grid_text",
            blank,
            "all-space grid",
            escape_text(&actual),
        );
    } else if let Some(want) = &expected.text {
        push_check(
            checks,
            "grid_text",
            &actual == want,
            escape_text(want),
            escape_text(&actual),
        );
    }
    for (idx, want) in &expected.grid_rows {
        let row = row_text(snapshot, *idx);
        let passed = row.as_deref().is_some_and(|row| {
            let prefix: String = row.chars().take(want.chars().count()).collect();
            let remainder: String = row.chars().skip(want.chars().count()).collect();
            prefix == *want && remainder.chars().all(|c| c == ' ')
        });
        push_check(
            checks,
            format!("row {idx}"),
            passed,
            escape_text(want),
            escape_text(&row.unwrap_or_default()),
        );
    }
}

fn row_text(snapshot: &Snapshot, row: usize) -> Option<String> {
    if row >= snapshot.height {
        return None;
    }
    let mut out = String::new();
    for col in 0..snapshot.width {
        let cell = &snapshot.cells[row * snapshot.width + col];
        if cell.spacer {
            continue;
        }
        out.push(cell.glyph);
    }
    Some(out)
}

fn check_cursor(expected: &Expected, snapshot: &Snapshot, checks: &mut Vec<CheckResult>) {
    if let Some((row, col, visible)) = expected.cursor {
        let actual = (
            snapshot.cursor.position.row,
            snapshot.cursor.position.col,
            snapshot.cursor.visible,
        );
        push_check(
            checks,
            "cursor",
            actual == (row, col, visible),
            format!("{row} {col} {}", vis(visible)),
            format!("{} {} {}", actual.0, actual.1, vis(actual.2)),
        );
    }
}

const fn vis(visible: bool) -> &'static str {
    if visible { "visible" } else { "hidden" }
}

fn check_modes(
    expected: &Expected,
    modes: &Modes,
    alt_screen: bool,
    checks: &mut Vec<CheckResult>,
) {
    for (name, want) in &expected.modes {
        let actual = match name.as_str() {
            "application_cursor_keys" => on_off(modes.application_cursor_keys),
            "alternate_scroll" => on_off(modes.alternate_scroll),
            "synchronized_update" => on_off(modes.synchronized_update),
            "auto_wrap" => on_off(modes.auto_wrap),
            "origin" => on_off(modes.origin),
            "bracketed_paste" => on_off(modes.bracketed_paste),
            "focus_events" => on_off(modes.focus_events),
            "alt_screen" => on_off(alt_screen),
            "mouse_tracking" => mouse_tracking(modes.mouse_tracking),
            "mouse_encoding" => mouse_encoding(modes.mouse_coordinate_encoding),
            other => {
                push_check(
                    checks,
                    format!("mode {other}"),
                    false,
                    want.clone(),
                    format!("unknown mode name {other:?}"),
                );
                continue;
            }
        };
        push_check(
            checks,
            format!("mode {name}"),
            actual == *want,
            want,
            actual,
        );
    }
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

fn mouse_tracking(mode: Option<MouseTrackingMode>) -> String {
    match mode {
        None => "off".to_string(),
        Some(MouseTrackingMode::X10) => "x10".to_string(),
        Some(MouseTrackingMode::Normal) => "normal".to_string(),
        Some(MouseTrackingMode::Button) => "button".to_string(),
        Some(MouseTrackingMode::Any) => "any".to_string(),
    }
}

fn mouse_encoding(encoding: Option<MouseCoordinateEncoding>) -> String {
    match encoding {
        None => "off".to_string(),
        Some(MouseCoordinateEncoding::Utf8) => "utf8".to_string(),
        Some(MouseCoordinateEncoding::Sgr) => "sgr".to_string(),
        Some(MouseCoordinateEncoding::Urxvt) => "urxvt".to_string(),
    }
}

fn cursor_style_name(style: CursorStyle) -> &'static str {
    match style {
        CursorStyle::Default => "default",
        CursorStyle::BlinkingBlock => "blinking_block",
        CursorStyle::SteadyBlock => "steady_block",
        CursorStyle::BlinkingUnderline => "blinking_underline",
        CursorStyle::SteadyUnderline => "steady_underline",
        CursorStyle::BlinkingBar => "blinking_bar",
        CursorStyle::SteadyBar => "steady_bar",
    }
}

fn check_cursor_style(expected: &Expected, snapshot: &Snapshot, checks: &mut Vec<CheckResult>) {
    if let Some(want) = &expected.cursor_style {
        let actual = cursor_style_name(snapshot.cursor.cursor_style);
        push_check(checks, "cursor_style", actual == want, want, actual);
    }
}

fn check_title(expected: &Expected, snapshot: &Snapshot, checks: &mut Vec<CheckResult>) {
    if let Some(want) = &expected.title {
        let actual = snapshot.title.as_str();
        push_check(
            checks,
            "title",
            actual == want,
            escape_text(want),
            escape_text(actual),
        );
    }
}

fn check_actions(expected: &Expected, actions: &[TerminalAction], checks: &mut Vec<CheckResult>) {
    let Some(want) = &expected.actions else {
        return;
    };
    let actual: Vec<String> = actions.iter().map(canonical_action).collect();
    push_check(
        checks,
        "actions",
        actual == *want,
        want.join(" / "),
        actual.join(" / "),
    );
}

fn check_reply(expected: &Expected, reply: &[u8], checks: &mut Vec<CheckResult>) {
    if let Some(want) = &expected.reply {
        push_check(
            checks,
            "reply",
            reply == want.as_slice(),
            escape_bytes(want),
            escape_bytes(reply),
        );
    }
}

/// Canonical, stable textual form of the parser actions the oracle asserts.
///
/// Only the families named by `action:` expectation lines are canonicalized;
/// everything else falls back to the derived `Debug` form so an unexpected
/// extra action still produces a deterministic, comparable string.
#[must_use]
pub fn canonical_action(action: &TerminalAction) -> String {
    match action {
        TerminalAction::OscDynamicColor { target, op } => {
            let target = match target {
                DynamicColorTarget::Foreground => "fg",
                DynamicColorTarget::Background => "bg",
            };
            let op = match op {
                DynamicColorOp::Query => "query".to_string(),
                DynamicColorOp::Set(rgb) => format!("set {} {} {}", rgb.r, rgb.g, rgb.b),
            };
            format!("osc_dynamic_color {target} {op}")
        }
        TerminalAction::OscTitle { text } => {
            format!("osc_title {}", escape_text(text.as_str()))
        }
        TerminalAction::CursorStyle { style } => {
            format!("cursor_style {}", cursor_style_name(*style))
        }
        TerminalAction::SetMode { mode, enabled } => {
            format!("set_mode {} {}", mode_label(*mode), on_off(*enabled))
        }
        other => format!("{other:?}"),
    }
}

fn mode_label(mode: Mode) -> String {
    match mode {
        Mode::Insert => "insert".to_string(),
        Mode::LineFeedNewLine => "line_feed_new_line".to_string(),
        Mode::ApplicationKeypad => "application_keypad".to_string(),
        Mode::ApplicationCursorKeys => "application_cursor_keys".to_string(),
        Mode::Column132 => "column_132".to_string(),
        Mode::ReverseVideo => "reverse_video".to_string(),
        Mode::Origin => "origin".to_string(),
        Mode::AutoWrap => "auto_wrap".to_string(),
        Mode::CursorBlinking => "cursor_blinking".to_string(),
        Mode::AlternateScreen => "alternate_screen".to_string(),
        Mode::AlternateScreenClearAndRestore => "alternate_screen_clear_and_restore".to_string(),
        Mode::BracketedPaste => "bracketed_paste".to_string(),
        Mode::FocusEvents => "focus_events".to_string(),
        Mode::AlternateScroll => "alternate_scroll".to_string(),
        Mode::SynchronizedUpdate => "synchronized_update".to_string(),
        Mode::KittyKeyboard(flags) => format!("kitty_keyboard({flags})"),
        Mode::MouseTracking(tracking) => {
            format!("mouse_tracking({})", mouse_tracking(Some(tracking)))
        }
        Mode::MouseCoordinateEncoding(encoding) => {
            format!(
                "mouse_coordinate_encoding({})",
                mouse_encoding(Some(encoding))
            )
        }
    }
}

/// Escape bytes for a deterministic, single-line expectation/observed value.
#[must_use]
pub fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 8);
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x1b => out.push_str("\\e"),
            0x20..=0x7e => out.push(char::from(byte)),
            other => {
                let _ = write!(out, "\\x{other:02x}");
            }
        }
    }
    out
}

/// Escape a string for the expected-file text fields (same grammar as bytes).
#[must_use]
pub fn escape_text(text: &str) -> String {
    escape_bytes(text.as_bytes())
}

fn unescape_text(value: &str) -> String {
    String::from_utf8_lossy(&unescape_bytes(value)).into_owned()
}

/// Decode the `\e`, `\n`, `\r`, `\t`, `\\`, `\xNN` escape grammar.
#[must_use]
pub fn unescape_bytes(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hi = chars.next().and_then(|c| c.to_digit(16));
                let lo = chars.next().and_then(|c| c.to_digit(16));
                match (hi, lo) {
                    (Some(hi), Some(lo)) => out.push(((hi << 4) | lo) as u8),
                    _ => {
                        out.push(b'\\');
                        out.push(b'x');
                    }
                }
            }
            Some(other) => {
                out.push(b'\\');
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => out.push(b'\\'),
        }
    }
    out
}

/// Render the machine-readable oracle summary JSON (deterministic, bounded).
pub fn generate_summary_json(report: &OracleReport) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"schema_version\": {SUMMARY_VERSION},\n"));
    out.push_str("  \"generator\": \"bitty-compat-lab oracle_runner\",\n");
    out.push_str(&format!(
        "  \"scope\": \"{}\",\n",
        json_escape(SUMMARY_SCOPE)
    ));
    out.push_str("  \"bounds\": {\n");
    out.push_str(&format!(
        "    \"MAX_CORPUS_BYTES\": {},\n",
        crate::MAX_CORPUS_BYTES
    ));
    out.push_str(&format!("    \"MAX_ACTIONS\": {},\n", crate::MAX_ACTIONS));
    out.push_str(&format!("    \"MAX_SCENARIOS\": {MAX_SCENARIOS}\n"));
    out.push_str("  },\n");
    out.push_str("  \"summary\": {\n");
    out.push_str(&format!("    \"total\": {},\n", report.total()));
    out.push_str(&format!("    \"passed\": {},\n", report.passed));
    out.push_str(&format!("    \"failed\": {}\n", report.failed));
    out.push_str("  },\n");
    out.push_str("  \"areas\": [\n");
    for (idx, area) in AREAS.iter().enumerate() {
        let total = report.outcomes.iter().filter(|o| o.area == *area).count();
        let passed = report
            .outcomes
            .iter()
            .filter(|o| o.area == *area && o.status == Status::Pass)
            .count();
        let comma = if idx + 1 < AREAS.len() { "," } else { "" };
        out.push_str(&format!(
            "    {{\"name\": \"{}\", \"total\": {total}, \"passed\": {passed}}}{comma}\n",
            json_escape(area)
        ));
    }
    out.push_str("  ],\n");
    out.push_str("  \"scenarios\": [\n");
    for (idx, outcome) in report.outcomes.iter().enumerate() {
        out.push_str("    {\n");
        out.push_str(&format!(
            "      \"id\": \"{}\",\n",
            json_escape(&outcome.id)
        ));
        out.push_str(&format!(
            "      \"area\": \"{}\",\n",
            json_escape(&outcome.area)
        ));
        out.push_str(&format!(
            "      \"provenance\": {{\"kind\": \"{}\", \"source\": \"{}\"}},\n",
            outcome.provenance.kind.as_str(),
            json_escape(&outcome.provenance.source)
        ));
        out.push_str(&format!(
            "      \"status\": \"{}\",\n",
            outcome.status.as_str()
        ));
        out.push_str("      \"checks\": [");
        for (cidx, check) in outcome.checks.iter().enumerate() {
            if cidx > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!(
                "{{\"name\": \"{}\", \"status\": \"{}\", \"expected\": \"{}\", \"actual\": \"{}\"}}",
                json_escape(&check.name),
                if check.passed { "pass" } else { "fail" },
                json_escape(&check.expected),
                json_escape(&check.actual)
            ));
        }
        out.push_str("]\n");
        let comma = if idx + 1 < report.outcomes.len() {
            ","
        } else {
            ""
        };
        out.push_str(&format!("    }}{comma}\n"));
    }
    out.push_str("  ]\n");
    out.push_str("}\n");
    if out.len() > 256 * 1024 {
        return Err(format!("summary json {} > 256 KiB", out.len()));
    }
    Ok(out)
}

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            _ => out.push(ch),
        }
    }
    out
}
