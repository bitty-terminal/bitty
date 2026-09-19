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
//! tests/compat/oracle/README.md               # layout, provenance, how to extend
//! tests/compat/oracle/authority-cites.txt     # authority<TAB>cite index
//! tests/compat/oracle/scenarios/<id>.bin      # raw VT bytes for one scenario
//! tests/compat/oracle/scenarios/<id>.expected # expectation (authority derived)
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
//! tracking 1000/1002/1003 and encodings X10/SGR/UTF-8/urxvt (register plus
//! emitted bytes), mode 1007 alternate scroll, DECSCUSR cursor style,
//! alternate screen 1049/47, DECCKM cursor keys, and DSR/DA1 replies.
//!
//! ## Provenance
//!
//! Each `.expected` file carries structured provenance: an `authority:` line
//! naming one of the [`AUTHORITIES`] (the exact external source that defines
//! the exercised behavior) and a `cite:` line holding a **verbatim token** from
//! that source. The `oracle_citations_are_backed_by_their_authority` guard
//! requires every `authority`/`cite` pair to appear in the committed
//! `tests/compat/oracle/authority-cites.txt` index, which is derived from — and
//! re-verified against — the read-only reference snapshots and the canonical
//! specs. A free-form citation cannot enter the index, so a mis-citation (for
//! example citing `ctlseqs` for the `2026` synchronized-update mode, which
//! ctlseqs does not define) fails the guard instead of being rubber-stamped.
//! When a source is resolvable on the host, the guard additionally asserts the
//! token appears verbatim in it. See `tests/compat/oracle/README.md` for the
//! corpus-wide provenance table.
//!
//! Two engines execute scenarios: `state` (default) replays bytes through the
//! headless parser/state path; `runtime` drives `bitty-runtime` (OSC 10/11
//! query replies and mouse coordinate emission) and is executed by the
//! `bitty-runtime` oracle test, which reuses this crate's parser.

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

/// Summary schema version (bumped for structured engine/citation fields).
pub const SUMMARY_VERSION: u32 = 2;

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
    /// Authoritative specification citation.
    Spec,
    /// Reference implementation source (a captured/pinned upstream tree).
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

/// Where an authority source lives relative to the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityRoot {
    /// `$BITTY_WORKSPACE/recording/references/<path>` (read-only snapshot).
    UmbrellaReferences,
    /// `<repo>/<path>` (canonical docs submodule).
    RepoDocs,
}

/// One externally authoritative source the oracle may cite.
///
/// The `id` is the structured `authority:` value; `path` locates the source so
/// the citation guard can re-verify the `cite:` token verbatim when the source
/// is resolvable on the host.
#[derive(Debug, Clone, Copy)]
pub struct Authority {
    /// Stable authority id used in `.expected` files.
    pub id: &'static str,
    /// Spec or reference-implementation kind.
    pub kind: ProvenanceKind,
    /// Source path relative to [`Authority::root`].
    pub path: &'static str,
    /// Where the source lives.
    pub root: AuthorityRoot,
    /// Human-readable label recorded in the summary.
    pub label: &'static str,
}

/// The closed set of authorities the oracle may cite.
///
/// One entry per exact source file, so a `cite:` token can be re-found
/// verbatim (never a whole tree, which would make the check vacuous). The set
/// is deliberately minimal: `xterm-ctlseqs` (the control-sequence reference),
/// its source `xterm-charproc` (semantics ctlseqs states only loosely), the
/// ghostty and kitty reference sources, and the two accepted RFCs. A source
/// absent here cannot be cited, so a mis-citation is a load error, not a
/// silent pass.
pub const AUTHORITIES: &[Authority] = &[
    Authority {
        id: "xterm-ctlseqs",
        kind: ProvenanceKind::Spec,
        path: "xterm/ctlseqs.txt",
        root: AuthorityRoot::UmbrellaReferences,
        label: "xterm patch #411 ctlseqs.txt (2026/08/23)",
    },
    Authority {
        id: "xterm-charproc",
        kind: ProvenanceKind::Capture,
        path: "xterm/charproc.c",
        root: AuthorityRoot::UmbrellaReferences,
        label: "xterm patch #411 charproc.c",
    },
    Authority {
        id: "ghostty-modes",
        kind: ProvenanceKind::Capture,
        path: "ghostty/src/terminal/modes.zig",
        root: AuthorityRoot::UmbrellaReferences,
        label: "ghostty src/terminal/modes.zig",
    },
    Authority {
        id: "ghostty-stream",
        kind: ProvenanceKind::Capture,
        path: "ghostty/src/terminal/stream.zig",
        root: AuthorityRoot::UmbrellaReferences,
        label: "ghostty src/terminal/stream.zig",
    },
    Authority {
        id: "ghostty-terminal",
        kind: ProvenanceKind::Capture,
        path: "ghostty/src/terminal/Terminal.zig",
        root: AuthorityRoot::UmbrellaReferences,
        label: "ghostty src/terminal/Terminal.zig",
    },
    Authority {
        id: "kitty-window",
        kind: ProvenanceKind::Capture,
        path: "kitty/kitty/window.py",
        root: AuthorityRoot::UmbrellaReferences,
        label: "kitty kitty/window.py",
    },
    Authority {
        id: "m1-rfc",
        kind: ProvenanceKind::Spec,
        path: "docs/specifications/compatibility-milestone-rfc.md",
        root: AuthorityRoot::RepoDocs,
        label: "M1 compatibility milestone RFC",
    },
    Authority {
        id: "text-rendering-rfc",
        kind: ProvenanceKind::Spec,
        path: "docs/specifications/text-rendering-rfc.md",
        root: AuthorityRoot::RepoDocs,
        label: "text rendering RFC",
    },
];

/// Look up an authority id.
#[must_use]
pub fn authority(id: &str) -> Option<&'static Authority> {
    AUTHORITIES.iter().find(|a| a.id == id)
}

/// One expectation's provenance: a structured authority plus the verbatim
/// token from that authority which supports the exercised behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// Spec or reference-implementation kind.
    pub kind: ProvenanceKind,
    /// Authority id (key into [`AUTHORITIES`]).
    pub authority: String,
    /// Human-readable authority label.
    pub source: String,
    /// Verbatim token from the authority that defines the behavior.
    pub cite: String,
}

/// Which execution engine runs a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// Headless `bitty-vt` parser -> `bitty-term-state` (default).
    State,
    /// `bitty-runtime` end-to-end (OSC query replies, mouse encoding).
    Runtime,
}

impl Engine {
    /// Lowercase wire form used in the summary JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Engine::State => "state",
            Engine::Runtime => "runtime",
        }
    }
}

/// One deterministic stimulus applied before a runtime scenario's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stimulus {
    /// Grant the `OSC 10`/`OSC 11` set gate (default deny).
    AllowOscColorSet,
    /// Move the pointer to the scenario's `at_cell` (0-based) and press left.
    MouseLeftPress,
    /// Move the pointer to the scenario's `at_cell` and release left.
    MouseLeftRelease,
}

impl Stimulus {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "allow-osc-color-set" => Some(Stimulus::AllowOscColorSet),
            "mouse-left-press" => Some(Stimulus::MouseLeftPress),
            "mouse-left-release" => Some(Stimulus::MouseLeftRelease),
            _ => None,
        }
    }
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
    /// Expected concatenated query-reply bytes, or `None` when unchecked.
    pub reply: Option<Vec<u8>>,
    /// Expected terminal-to-host input bytes (`emit:`), or `None` when
    /// unchecked. Runtime-engine only (mouse report emission).
    pub emit: Option<Vec<u8>>,
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
    /// Which engine executes the scenario.
    pub engine: Engine,
    /// Deterministic stimuli applied before the bytes (runtime engine).
    pub stimuli: Vec<Stimulus>,
    /// Pointer cell `(row, col)` for mouse stimuli, 0-based.
    pub at_cell: Option<(u16, u16)>,
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
    /// Execution engine.
    pub engine: Engine,
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
    let parsed = parse_expected(&id, text)?;
    Ok(Scenario {
        id,
        area: parsed.area,
        provenance: parsed.provenance,
        engine: parsed.engine,
        stimuli: parsed.stimuli,
        at_cell: parsed.at_cell,
        corpus,
        expected: parsed.expected,
    })
}

/// Parsed `.expected` file, minus the raw corpus.
struct ParsedExpected {
    area: String,
    provenance: Provenance,
    engine: Engine,
    stimuli: Vec<Stimulus>,
    at_cell: Option<(u16, u16)>,
    expected: Expected,
}

fn parse_expected(id: &str, text: &str) -> Result<ParsedExpected, String> {
    let mut area: Option<String> = None;
    let mut provenance: Option<Provenance> = None;
    let mut engine = Engine::State;
    let mut stimuli: Vec<Stimulus> = Vec::new();
    let mut at_cell: Option<(u16, u16)> = None;
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
            "authority" => {
                let found = authority(value)
                    .ok_or_else(|| format!("{id}:{}: unknown authority {value:?}", lineno + 1))?;
                provenance = Some(Provenance {
                    kind: found.kind,
                    authority: found.id.to_string(),
                    source: found.label.to_string(),
                    cite: String::new(),
                });
            }
            "cite" => {
                let p = provenance
                    .as_mut()
                    .ok_or_else(|| format!("{id}:{}: cite before authority", lineno + 1))?;
                p.cite = value.to_string();
            }
            "engine" => {
                engine = match value {
                    "state" => Engine::State,
                    "runtime" => Engine::Runtime,
                    other => {
                        return Err(format!(
                            "{id}:{}: engine expects state|runtime, got {other:?}",
                            lineno + 1
                        ));
                    }
                };
            }
            "stimulus" => {
                let parsed = Stimulus::parse(value)
                    .ok_or_else(|| format!("{id}:{}: unknown stimulus {value:?}", lineno + 1))?;
                stimuli.push(parsed);
            }
            "at_cell" => {
                let (r, c) = value
                    .split_once(',')
                    .ok_or_else(|| format!("{id}:{}: at_cell needs `ROW,COL`", lineno + 1))?;
                let r: u16 = r
                    .trim()
                    .parse()
                    .map_err(|_| format!("{id}:{}: bad at_cell row", lineno + 1))?;
                let c: u16 = c
                    .trim()
                    .parse()
                    .map_err(|_| format!("{id}:{}: bad at_cell col", lineno + 1))?;
                at_cell = Some((r, c));
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
            "emit" => expected.emit = Some(unescape_bytes(value)),
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
    let provenance = provenance.ok_or_else(|| format!("{id}: missing `authority:` line"))?;
    if provenance.cite.trim().is_empty() {
        return Err(format!("{id}: missing `cite:` line"));
    }
    if engine == Engine::Runtime {
        let uses_stimulus = stimuli
            .iter()
            .any(|s| matches!(s, Stimulus::MouseLeftPress | Stimulus::MouseLeftRelease));
        if uses_stimulus && at_cell.is_none() {
            return Err(format!(
                "{id}: mouse stimulus requires an `at_cell: ROW,COL` line"
            ));
        }
    } else if !stimuli.is_empty() || at_cell.is_some() {
        return Err(format!(
            "{id}: stimuli and at_cell are runtime-engine only (add `engine: runtime`)"
        ));
    }
    Ok(ParsedExpected {
        area,
        provenance,
        engine,
        stimuli,
        at_cell,
        expected,
    })
}

/// Run the full oracle corpus, sorted and bounded.
///
/// Citations are verified first (against the committed index and, when
/// resolvable, the authority sources) so a mis-citation fails the run rather
/// than passing as a green scenario.
pub fn run_oracle() -> Result<OracleReport, String> {
    let scenarios = load_scenarios()?;
    verify_citations(&scenarios)?;
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

/// Observed values one engine extracts for the shared checks.
struct Observation {
    snapshot: Snapshot,
    actions: Vec<TerminalAction>,
    alt_screen: bool,
    /// Query replies emitted by the engine (concatenated).
    reply: Vec<u8>,
    /// Input bytes the terminal emitted toward the host (mouse reports).
    emit: Vec<u8>,
}

/// Execute one scenario against the Bitty build and diff against its oracle.
#[must_use]
pub fn run_scenario(scenario: &Scenario) -> ScenarioOutcome {
    let mut checks: Vec<CheckResult> = Vec::new();

    let observation = match scenario.engine {
        Engine::State => observe_state_engine(scenario),
        Engine::Runtime => observe_runtime_engine(scenario),
    };

    check_grid(&scenario.expected, &observation.snapshot, &mut checks);
    check_cursor(&scenario.expected, &observation.snapshot, &mut checks);
    check_modes(
        &scenario.expected,
        &observation.snapshot.modes,
        observation.alt_screen,
        &mut checks,
    );
    check_cursor_style(&scenario.expected, &observation.snapshot, &mut checks);
    check_title(&scenario.expected, &observation.snapshot, &mut checks);
    check_actions(&scenario.expected, &observation.actions, &mut checks);
    check_reply(&scenario.expected, &observation.reply, &mut checks);
    check_emit(&scenario.expected, &observation.emit, &mut checks);

    let status = if checks.iter().all(|c| c.passed) {
        Status::Pass
    } else {
        Status::Fail
    };
    ScenarioOutcome {
        id: scenario.id.clone(),
        area: scenario.area.clone(),
        engine: scenario.engine,
        provenance: scenario.provenance.clone(),
        status,
        checks,
    }
}

/// Headless `bitty-vt` parser -> `bitty-term-state` observation (bounded).
fn observe_state_engine(scenario: &Scenario) -> Observation {
    let actions = std::panic::catch_unwind(|| crate::parse_bounded(&scenario.corpus))
        .expect("parse_bounded must not panic on a bounded corpus");
    let snapshot = crate::actions_to_snapshot(&actions);

    let mut state = bitty_term_state::State::new();
    for action in &actions {
        state.apply(action);
    }
    let reply: Vec<u8> = state
        .take_replies()
        .iter()
        .flat_map(|r| r.iter().copied())
        .collect();
    debug_assert!(
        state.check_invariants().is_ok(),
        "state engine left the terminal invariant set dirty"
    );
    Observation {
        snapshot,
        actions,
        alt_screen: state.alt_screen_active(),
        reply,
        emit: Vec::new(),
    }
}

/// `bitty-runtime` engine: real app path for replies and mouse emission.
///
/// Deterministic and headless: no PTY, network, clock, or display; the
/// runtime's `Instant::now` seam is only used for click counting, which the
/// stimuli do not exercise.
fn observe_runtime_engine(scenario: &Scenario) -> Observation {
    use bitty_runtime::Runtime;

    let config = bitty_runtime::RuntimeConfig {
        theme_resolved: true,
        ..bitty_runtime::RuntimeConfig::default()
    };
    let mut rt = Runtime::new(config).expect("headless runtime must build");

    // Phase 1 — stimuli that gate later bytes (must precede the corpus).
    for stimulus in &scenario.stimuli {
        if *stimulus == Stimulus::AllowOscColorSet {
            rt.set_osc_color_set_allowed(true);
        }
    }

    rt.handle_pty_bytes(&scenario.corpus);

    // Phase 2 — input stimuli, applied after the corpus so the tracking and
    // encoding modes are live when the pointer event is encoded. One captured
    // base instant feeds the virtual-clock seam for every event, so the
    // click-tracking timestamp is fixed for the scenario and the emitted bytes
    // cannot depend on wall time.
    let base = std::time::Instant::now();
    for stimulus in &scenario.stimuli {
        match stimulus {
            Stimulus::AllowOscColorSet => {}
            Stimulus::MouseLeftPress | Stimulus::MouseLeftRelease => {
                let (row, col) = scenario
                    .at_cell
                    .expect("runtime mouse stimulus requires at_cell");
                let pos = runtime_cell_center(&rt, row, col);
                rt.handle_cursor_moved(pos);
                rt.drain_pending_input();
                let state = if *stimulus == Stimulus::MouseLeftPress {
                    bitty_platform::PressState::Pressed
                } else {
                    bitty_platform::PressState::Released
                };
                rt.handle_mouse_input_at(
                    bitty_platform::MouseEvent::new(bitty_platform::MouseButton::Left, state),
                    base,
                );
            }
        }
    }
    let reply: Vec<u8> = rt
        .take_replies()
        .iter()
        .flat_map(|r| r.iter().copied())
        .collect();
    let emit = rt.pending_input().to_vec();
    let snapshot = rt.snapshot();
    let alt_screen = rt.state().alt_screen_active();
    let actions = crate::parse_bounded(&scenario.corpus);
    Observation {
        snapshot,
        actions,
        alt_screen,
        reply,
        emit,
    }
}

/// A physical pointer position whose `Runtime::cursor_to_cell` maps to
/// `(row, col)`.
///
/// Inverts the runtime's own public cell mapping by a bounded 1px scan rather
/// than re-deriving decoration/DPI/gap math here, so the stimulus stays
/// correct if layout constants change. The scan range is derived from the
/// configured grid and decoration — no clock or randomness.
fn runtime_cell_center(
    rt: &bitty_runtime::Runtime,
    row: u16,
    col: u16,
) -> bitty_platform::CursorPosition {
    let cfg = rt.config();
    let cell_w = f64::from(cfg.cell_width.max(1));
    let cell_h = f64::from(cfg.cell_height.max(1));
    let max_x = (cfg.cols as f64 + 4.0) * cell_w + 256.0;
    let max_y = (cfg.rows as f64 + 4.0) * cell_h + 256.0;
    let x = probe_axis(
        |x| {
            rt.cursor_to_cell(bitty_platform::CursorPosition { x, y: 0.0 })
                .col
        },
        col,
        max_x,
    );
    let y = probe_axis(
        |y| {
            rt.cursor_to_cell(bitty_platform::CursorPosition { x: 0.0, y })
                .row
        },
        row,
        max_y,
    );
    bitty_platform::CursorPosition { x, y }
}

/// Midpoint of the first pixel band on one axis whose cell mapping equals
/// `target` (bounded scan up to `max`).
fn probe_axis(mut map: impl FnMut(f64) -> u16, target: u16, max: f64) -> f64 {
    let mut start = None;
    let mut end = max;
    let mut x = 0.0f64;
    while x <= max {
        let cell = map(x);
        if cell == target {
            if start.is_none() {
                start = Some(x);
            }
        } else if start.is_some() {
            end = x;
            break;
        }
        x += 1.0;
    }
    match start {
        Some(lo) => (lo + end) / 2.0,
        None => max / 2.0,
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

fn check_emit(expected: &Expected, emit: &[u8], checks: &mut Vec<CheckResult>) {
    if let Some(want) = &expected.emit {
        push_check(
            checks,
            "emit",
            emit == want.as_slice(),
            escape_bytes(want),
            escape_bytes(emit),
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
            "      \"engine\": \"{}\",\n",
            outcome.engine.as_str()
        ));
        out.push_str(&format!(
            "      \"provenance\": {{\"kind\": \"{}\", \"authority\": \"{}\", \"source\": \"{}\", \"cite\": \"{}\"}},\n",
            outcome.provenance.kind.as_str(),
            json_escape(&outcome.provenance.authority),
            json_escape(&outcome.provenance.source),
            json_escape(&outcome.provenance.cite)
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

/// Repo-relative path of the committed citation index.
pub const CITATION_INDEX_REL: &str = "tests/compat/oracle/authority-cites.txt";

/// One index entry: the claimed `authority` + verbatim `cite` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiteEntry {
    /// Authority id.
    pub authority: String,
    /// Verbatim token from the authority source.
    pub cite: String,
}

/// Load and validate the citation index.
///
/// Format: one `authority<TAB>cite` pair per non-comment, non-blank line. A
/// malformed line, an unknown authority, a duplicate pair, or an empty field
/// is an error, so the index cannot silently drift.
pub fn load_citation_index() -> Result<Vec<CiteEntry>, String> {
    let path = crate::workspace_root().join(CITATION_INDEX_REL);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read citation index {}: {e}", path.display()))?;
    let mut out: Vec<CiteEntry> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (authority, cite) = line
            .split_once('\t')
            .ok_or_else(|| format!("citation index:{}: needs `authority<TAB>cite`", lineno + 1))?;
        let authority = authority.trim();
        let cite = cite.trim();
        if authority.is_empty() || cite.is_empty() {
            return Err(format!(
                "citation index:{}: empty authority or cite field",
                lineno + 1
            ));
        }
        if self::authority(authority).is_none() {
            return Err(format!(
                "citation index:{}: unknown authority {authority:?}",
                lineno + 1
            ));
        }
        let key = (authority.to_string(), cite.to_string());
        if !seen.insert(key.clone()) {
            return Err(format!(
                "citation index:{}: duplicate pair {authority:?} / {cite:?}",
                lineno + 1
            ));
        }
        out.push(CiteEntry {
            authority: key.0,
            cite: key.1,
        });
    }
    if out.is_empty() {
        return Err("citation index is empty".to_string());
    }
    Ok(out)
}

/// Resolve an authority's source file on this host, if present.
///
/// Returns `None` when the read-only snapshot / docs submodule is absent (for
/// example a bare CI checkout without the submodule); the caller then relies on
/// the committed index alone.
#[must_use]
pub fn resolve_authority_source(authority: &Authority) -> Option<PathBuf> {
    match authority.root {
        AuthorityRoot::UmbrellaReferences => crate::umbrella_root()
            .map(|root| root.join("recording/references").join(authority.path)),
        AuthorityRoot::RepoDocs => Some(crate::workspace_root().join(authority.path)),
    }
    .filter(|p| p.exists())
}

/// Verify one `authority`/`cite` pair against the committed index and, when
/// resolvable, the authority source file.
///
/// # Errors
///
/// Returns a message when the pair is absent from the index, or when the
/// authority source is resolvable and does not contain the token verbatim
/// (a mis-citation: the cited sentence does not support the behavior).
pub fn verify_citation(index: &[CiteEntry], authority_id: &str, cite: &str) -> Result<(), String> {
    let entry_ok = index
        .iter()
        .any(|e| e.authority == authority_id && e.cite == cite);
    if !entry_ok {
        return Err(format!(
            "citation not in {CITATION_INDEX_REL}: authority {authority_id:?} / cite {cite:?}"
        ));
    }
    let Some(auth) = authority(authority_id) else {
        return Err(format!("unknown authority {authority_id:?}"));
    };
    if let Some(path) = resolve_authority_source(auth) {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read authority source {}: {e}", path.display()))?;
        if !text.contains(cite) {
            return Err(format!(
                "cite {cite:?} is not present verbatim in {} ({}): the cited source does not \
                 support the exercised behavior",
                auth.label,
                path.display()
            ));
        }
    }
    Ok(())
}

/// Verify every scenario's structured provenance against the index/sources.
pub fn verify_citations(scenarios: &[Scenario]) -> Result<(), String> {
    let index = load_citation_index()?;
    for scenario in scenarios {
        verify_citation(
            &index,
            &scenario.provenance.authority,
            &scenario.provenance.cite,
        )
        .map_err(|e| format!("{}: {e}", scenario.id))?;
    }
    Ok(())
}
