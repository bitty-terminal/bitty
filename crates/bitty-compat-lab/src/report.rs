#![forbid(unsafe_code)]
//! M1/M2 terminal compatibility matrix report (CTX-0404).
//!
//! Machine-readable, deterministic report for the M1/M2 compatibility matrix:
//! shell startup/exit, tmux, nvim, ssh, general TUI, Unicode/CJK/IME, mouse,
//! clipboard (OSC 52 + kitty), kitty graphics, resize/reflow, scrollback, and
//! alternate screen. Every row declares its evidence status and method:
//!
//! - [`Status::Ci`] — deterministic check that runs in `cargo test`/`just check`.
//!   Backed by a checked-in corpus ([`Method::Corpus`]) replayed bounded,
//!   deterministic, and invariant-checked here, or by a named test that is
//!   verified present in its source file ([`Method::Test`]).
//! - [`Status::Local`] — environment-dependent check that only runs when the
//!   local tool or display exists and the operator opts in
//!   ([`Method::LocalPty`] with `BITTY_COMPAT_LIVE=1`, implemented by
//!   `tests/live_compat.rs`; [`Method::LiveDisplay`] with the `gui-tests`
//!   feature plus a live display, implemented by
//!   `crates/bitty-platform/tests/clipboard_live.rs`). CI never runs these
//!   and must not claim them.
//! - [`Status::Partial`] — bounded parser/admission evidence exists, but the
//!   full behavior (e.g. a protocol extension) is not implemented.
//! - [`Status::Gap`] — not covered; the row names the reason.
//!
//! The report is deterministic: sorted rows, no wall clock, no host paths, and
//! no network. The optional `BITTY_COMPAT_REVISION` environment variable is the
//! only externally supplied value; it is recorded as `"unknown"` when unset so
//! repeated runs on one checkout stay byte-identical.
//!
//! Differential reference dumps (`compare_all`) are out of this module; see
//! [`crate::compare`] and `tests/live_compat.rs` for the local leg.

use std::fmt::Write as _;
use std::path::PathBuf;

use crate::{MAX_ACTIONS, MAX_CORPUS_BYTES, actions_to_snapshot, parse_bounded, workspace_root};

/// Report schema version.
pub const REPORT_VERSION: u32 = 1;

/// Upper bound for the whole report JSON.
pub const MAX_REPORT_BYTES: usize = 64 * 1024;

/// Scope label recorded in the report.
pub const REPORT_SCOPE: &str = "M1/M2 terminal compatibility matrix (CTX-0404)";

/// Areas in priority order: areas named as M2→M7 hardening blockers first.
pub const AREAS: &[&str] = &[
    "shell-startup-exit",
    "tmux-rendering",
    "nvim-rendering-input",
    "ssh",
    "general-tui",
    "unicode-cjk-ime",
    "mouse-protocols",
    "clipboard",
    "graphics-protocols",
    "resize-reflow",
    "scrollback",
    "alternate-screen",
];

/// Tools probed on `PATH` for the environment section.
///
/// Sorted alphabetically; the report mirrors this order.
pub const PROBED_TOOLS: &[&str] = &[
    "bash", "chafa", "fish", "fzf", "htop", "nvim", "sh", "ssh", "tmux", "zsh",
];

/// Evidence status for one matrix row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// Deterministic check running in `cargo test` / `just check`.
    Ci,
    /// Environment-dependent check verified locally at a recorded revision.
    Local,
    /// Bounded partial evidence; full behavior not implemented yet.
    Partial,
    /// Not covered by any automated or recorded check.
    Gap,
}

impl Status {
    /// Lowercase wire form used in JSON and the reference page.
    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Ci => "ci",
            Status::Local => "local",
            Status::Partial => "partial",
            Status::Gap => "gap",
        }
    }
}

/// How a row's evidence is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Checked-in corpus under `tests/compat/`, replayed by this module.
    Corpus {
        /// Workspace-relative corpus path.
        path: &'static str,
    },
    /// Named deterministic test verified present in its source file.
    Test {
        /// Workspace-relative test source file.
        file: &'static str,
        /// Test function name.
        name: &'static str,
    },
    /// Env-gated PTY scenario in `tests/live_compat.rs` (`BITTY_COMPAT_LIVE=1`).
    LocalPty {
        /// Scenario name; must match a live scenario test.
        scenario: &'static str,
    },
    /// Env-gated live-display scenario in
    /// `crates/bitty-platform/tests/clipboard_live.rs` (default-off
    /// `gui-tests` feature plus a reachable display; R-004, CTX-0641).
    LiveDisplay {
        /// Scenario prefix; every test named `live_{scenario}_*` in
        /// `clipboard_live.rs` belongs to it.
        scenario: &'static str,
    },
    /// No automated evidence; `reason` records why.
    Uncovered {
        /// Human-readable reason and follow-up hint.
        reason: &'static str,
    },
}

/// One area × scenario row of the M1/M2 compatibility matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// Area name from [`AREAS`].
    pub area: &'static str,
    /// Scenario within the area.
    pub scenario: &'static str,
    /// Evidence status.
    pub status: Status,
    /// Evidence method.
    pub method: Method,
    /// Short note shown in the reference page.
    pub note: &'static str,
}

/// The M1/M2 matrix rows, ordered by [`AREAS`] priority then scenario name.
pub const ROWS: &[Row] = &[
    // shell-startup-exit
    Row {
        area: "shell-startup-exit",
        scenario: "zsh/fish prompt marks OSC 133 A/B/C with OSC 7 cwd and OSC 8 hyperlink",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/shell/corpus/02-dogfooding-shell-osc133-osc7-fish.bin",
        },
        note: "prompt/input/output marks and cwd reporting stay bounded and deterministic",
    },
    Row {
        area: "shell-startup-exit",
        scenario: "command exit status OSC 133 D including non-zero and signal-style codes",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/shell/corpus/04-shell-startup-exit.bin",
        },
        note: "exit_code parsed for success, failure, and signal-style values",
    },
    Row {
        area: "shell-startup-exit",
        scenario: "interactive shell startup, echo, and clean exit over a real PTY",
        status: Status::Local,
        method: Method::LocalPty { scenario: "shell" },
        note: "bash --noprofile --norc -i; env-gated, never claimed in CI",
    },
    Row {
        area: "shell-startup-exit",
        scenario: "automatic shell-integration installation for bash/zsh/fish",
        status: Status::Gap,
        method: Method::Uncovered {
            reason: "no shell-integration installer exists yet; needs an owning task",
        },
        note: "prompt marks are exercised from corpora, not emitted by an installed hook",
    },
    // tmux-rendering
    Row {
        area: "tmux-rendering",
        scenario: "pane borders (U+2502), status bar SGR, and scroll regions",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin",
        },
        note: "synthetic tmux-shaped stream; real tmux is the local leg below",
    },
    Row {
        area: "tmux-rendering",
        scenario: "real tmux session render capture over a PTY",
        status: Status::Local,
        method: Method::LocalPty { scenario: "tmux" },
        note: "unique private tmux socket, attach/detach, server killed by the test",
    },
    Row {
        area: "tmux-rendering",
        scenario: "tmux DCS passthrough (ESC Ptmux;)",
        status: Status::Gap,
        method: Method::Uncovered {
            reason: "DCS strings are recorded as unknown/inert; no tmux passthrough unwrap",
        },
        note: "blocks nested protocol probing through tmux; near-term follow-up",
    },
    // nvim-rendering-input
    Row {
        area: "nvim-rendering-input",
        scenario: "alternate-screen fullscreen, scroll region, and statusline",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin",
        },
        note: "1049h/1049l plus scroll region replay with invariants",
    },
    Row {
        area: "nvim-rendering-input",
        scenario: "truecolor and curly underline SGR (SGR 4:3) for diagnostics UI",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/vt/corpus/02-sgr-underline.bin",
        },
        note: "undercurl style is parsed and preserved in cell attributes",
    },
    Row {
        area: "nvim-rendering-input",
        scenario: "kitty keyboard protocol (CSI u, 7727 progressive) and bracketed paste",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/keyboard/corpus/03-dogfooding-kitty-keyboard-bracketed.bin",
        },
        note: "input-mode negotiation only; key encoding to the app is not exercised",
    },
    Row {
        area: "nvim-rendering-input",
        scenario: "real nvim session render capture over a PTY",
        status: Status::Local,
        method: Method::LocalPty { scenario: "nvim" },
        note: "nvim --clean driven with ihello/:qa! under a PTY",
    },
    // ssh
    Row {
        area: "ssh",
        scenario: "remote session title (OSC 0) and echo over a synthetic stream",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/tui/corpus/03-dogfooding-nvim-tmux-fzf-htop-ssh.bin",
        },
        note: "title and output are byte-level evidence only",
    },
    Row {
        area: "ssh",
        scenario: "real ssh transport to a remote host",
        status: Status::Gap,
        method: Method::Uncovered {
            reason: "requires a remote endpoint; automation must not dial out (no network in tests)",
        },
        note: "manual-smoke checklist only; no recorded local evidence yet",
    },
    // general-tui
    Row {
        area: "general-tui",
        scenario: "htop/fzf alternate-screen list with 32m color bars",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/tui/corpus/02-htop-fzf.bin",
        },
        note: "47h/47l legacy alternate screen replay",
    },
    Row {
        area: "general-tui",
        scenario: "real fzf render capture over a PTY",
        status: Status::Local,
        method: Method::LocalPty { scenario: "fzf" },
        note: "piped candidate list, full-screen fzf, ESC to quit",
    },
    Row {
        area: "general-tui",
        scenario: "real htop render capture over a PTY",
        status: Status::Local,
        method: Method::LocalPty { scenario: "htop" },
        note: "q to quit; bounded capture and replay",
    },
    // unicode-cjk-ime
    Row {
        area: "unicode-cjk-ime",
        scenario: "wide CJK, emoji ZWJ, combining marks, and zero-width width invariants",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/unicode/corpus/09-dogfooding-ime-unicode-dpi.bin",
        },
        note: "wide cells occupy two columns with a non-orphan spacer",
    },
    Row {
        area: "unicode-cjk-ime",
        scenario: "ambiguous-width policy (single-width primary table)",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/unicode/corpus/05-ambiguous.bin",
        },
        note: "documented policy; no terminal-dependent ambiguity switch",
    },
    Row {
        area: "unicode-cjk-ime",
        scenario: "invalid UTF-8 replacement (U+FFFD) stays bounded",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/unicode/corpus/07-invalid-utf8.bin",
        },
        note: "one replacement cell, deterministic under byte-by-byte re-parse",
    },
    Row {
        area: "unicode-cjk-ime",
        scenario: "real IME composition events on a live input method",
        status: Status::Gap,
        method: Method::Uncovered {
            reason: "requires a live input method; compat-lab replays bytes and cannot drive an IME",
        },
        note: "width behavior for precomposed text is covered; composition itself is not",
    },
    // mouse-protocols
    Row {
        area: "mouse-protocols",
        scenario: "SGR 1006 with normal/button/any-motion tracking modes",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/mouse/corpus/03-dogfooding-mouse-resize-sgr.bin",
        },
        note: "1000/1002/1003/1006 mode set/reset replay plus SGR reports",
    },
    Row {
        area: "mouse-protocols",
        scenario: "legacy encodings: UTF-8 1005 and urxvt 1015",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/mouse/corpus/04-legacy-coordinate-encodings.bin",
        },
        note: "mode negotiation parse; report decoding is input-side and not covered",
    },
    Row {
        area: "mouse-protocols",
        scenario: "interactive mouse click/drag/scroll in tmux and nvim",
        status: Status::Gap,
        method: Method::Uncovered {
            reason: "scripted live-app mouse interaction is not automated; manual-smoke only",
        },
        note: "no recorded local evidence yet",
    },
    // clipboard
    Row {
        area: "clipboard",
        scenario: "OSC 52 query vs write with base64 payload, bounded",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/osc/corpus/03-dogfooding-osc7-8-52-title.bin",
        },
        note: "query/write distinguished; read policy remains deny-by-default",
    },
    Row {
        area: "clipboard",
        scenario: "kitty clipboard extension (OSC 5522)",
        status: Status::Partial,
        method: Method::Corpus {
            path: "tests/compat/osc/corpus/04-kitty-clipboard-5522.bin",
        },
        note: "parses as bounded inert unknown OSC; extension not implemented (gap)",
    },
    Row {
        area: "clipboard",
        scenario: "system clipboard sync policy (headless simulation)",
        status: Status::Ci,
        method: Method::Test {
            file: "crates/bitty-platform/tests/clipboard_sync.rs",
            name: "headless_set_syncs_clipboard_and_primary",
        },
        note: "policy simulation only; OS clipboard backends are not exercised (R-004 Open)",
    },
    Row {
        area: "clipboard",
        scenario: "OS-level clipboard roundtrip on a live display",
        status: Status::Local,
        method: Method::LiveDisplay {
            scenario: "clipboard",
        },
        note: "operator-run per Tier-1 display (X11/Wayland/macOS/Windows) with gui-tests; never claimed in CI",
    },
    // graphics-protocols
    Row {
        area: "graphics-protocols",
        scenario: "kitty graphics single-chunk APC G admission (f=32 2x2 RGBA)",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/graphics/corpus/01-kitty-image-single.bin",
        },
        note: "parser emission bounded; placement/paint is runtime-owned",
    },
    Row {
        area: "graphics-protocols",
        scenario: "kitty graphics chunked reassembly (m=1/m=0, chafa shape)",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/graphics/corpus/02-kitty-image-chunked.bin",
        },
        note: "multi-chunk stream reassembled under the ledger cap",
    },
    Row {
        area: "graphics-protocols",
        scenario: "runtime image placement and paint (headless)",
        status: Status::Ci,
        method: Method::Test {
            file: "crates/bitty-runtime/tests/kitty_images_present.rs",
            name: "display_paints_image_pixels_topmost",
        },
        note: "headless compositor paint; no GPU/display required",
    },
    Row {
        area: "graphics-protocols",
        scenario: "real chafa --format=kitty capture over a PTY",
        status: Status::Local,
        method: Method::LocalPty { scenario: "chafa" },
        note: "generated 2x2 PNG; capture must contain APC G and emit a graphics action",
    },
    // resize-reflow
    Row {
        area: "resize-reflow",
        scenario: "reflow with scroll region and erase on width change",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/resize/corpus/01-resize-reflow.bin",
        },
        note: "scroll region 2;10r with 5S/3T stays bounded",
    },
    Row {
        area: "resize-reflow",
        scenario: "resize during alternate screen (800x600 -> 100x37 @8x16)",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/resize/corpus/02-dogfooding-resize-dpi-alt-screen.bin",
        },
        note: "alt-screen content and geometry updates replay deterministically",
    },
    Row {
        area: "resize-reflow",
        scenario: "SIGWINCH-driven resize over a real PTY",
        status: Status::Local,
        method: Method::LocalPty { scenario: "resize" },
        note: "PTY 80x24 -> 120x40 with stty size assertion",
    },
    // scrollback
    Row {
        area: "scrollback",
        scenario: "scrollback retention and viewport under 30 lines + CSI 3S",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/scrollback/corpus/01-scrollback-basic.bin",
        },
        note: "retention stays under the configured state cap",
    },
    Row {
        area: "scrollback",
        scenario: "alternate-screen isolation of primary scrollback (1049h/1049l)",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/scrollback/corpus/02-scrollback-alt-screen.bin",
        },
        note: "primary buffer restored without alt-screen leakage",
    },
    Row {
        area: "scrollback",
        scenario: "runtime scrollback cap and resize retention",
        status: Status::Ci,
        method: Method::Test {
            file: "crates/bitty-runtime/tests/scrollback_cap.rs",
            name: "configured_scrollback_cap_bounds_retained_lines",
        },
        note: "runtime-level cap; resize retention covered by resize_scrollback.rs",
    },
    Row {
        area: "scrollback",
        scenario: "scrollback search and scrollbar interaction",
        status: Status::Ci,
        method: Method::Test {
            file: "crates/bitty-runtime/tests/scrollback_search_selection_persistence.rs",
            name: "search_finds_in_scrollback_and_live_grid_headless",
        },
        note: "search spans history and live grid headlessly",
    },
    // alternate-screen
    Row {
        area: "alternate-screen",
        scenario: "1049 enter/exit with cursor save and no orphan spacer",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/tui/corpus/01-nvim-tmux.bin",
        },
        note: "alt-screen enter/exit replay with invariant checks",
    },
    Row {
        area: "alternate-screen",
        scenario: "legacy 47 alternate-screen enter/exit",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/tui/corpus/02-htop-fzf.bin",
        },
        note: "legacy 47h/47l replay plus SGR",
    },
    Row {
        area: "alternate-screen",
        scenario: "alternate screen with mouse/keyboard mode interaction",
        status: Status::Ci,
        method: Method::Corpus {
            path: "tests/compat/mouse/corpus/03-dogfooding-mouse-resize-sgr.bin",
        },
        note: "mode toggles during alt-screen replay stay bounded",
    },
];

/// Look up `name` on `PATH`, returning the first executable file.
///
/// Portable, allocation-bounded, and read-only: no process is spawned.
pub fn tool_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Verify a checked-in corpus: bounded, deterministic, invariant-clean.
///
/// Returns `(bytes_len, actions_len, state_hash)`.
fn verify_corpus(path_rel: &str) -> Result<(usize, usize, u64), String> {
    let path = workspace_root().join(path_rel);
    let bytes = std::fs::read(&path).map_err(|e| format!("corpus {path_rel:?} unreadable: {e}"))?;
    if bytes.len() > MAX_CORPUS_BYTES {
        return Err(format!(
            "corpus {path_rel:?} len {} > MAX_CORPUS_BYTES {MAX_CORPUS_BYTES}",
            bytes.len()
        ));
    }
    let actions = parse_bounded(&bytes);
    if actions.len() > MAX_ACTIONS {
        return Err(format!(
            "corpus {path_rel:?} actions {} > MAX_ACTIONS {MAX_ACTIONS}",
            actions.len()
        ));
    }
    let snapshot = actions_to_snapshot(&actions);
    if snapshot.width != bitty_term_state::GRID_COLUMNS
        || snapshot.height != bitty_term_state::GRID_ROWS
    {
        return Err(format!(
            "corpus {path_rel:?} snapshot {}x{} != canonical grid",
            snapshot.width, snapshot.height
        ));
    }
    let mut state = bitty_term_state::State::new();
    for action in &actions {
        state.apply(action);
    }
    state
        .check_invariants()
        .map_err(|e| format!("corpus {path_rel:?} invariant violation: {e:?}"))?;
    let hash = state.state_hash();
    let mut replay = bitty_term_state::State::new();
    for action in &parse_bounded(&bytes) {
        replay.apply(action);
    }
    if replay.state_hash() != hash {
        return Err(format!("corpus {path_rel:?} state_hash diverged on replay"));
    }
    Ok((bytes.len(), actions.len(), hash))
}

/// Verify that a named test function exists in its source file.
fn verify_test_present(file_rel: &str, name: &str) -> Result<(), String> {
    let path = workspace_root().join(file_rel);
    let source = std::fs::read_to_string(&path)
        .map_err(|e| format!("test file {file_rel:?} unreadable: {e}"))?;
    let needle = format!("fn {name}(");
    if source.contains(&needle) {
        Ok(())
    } else {
        Err(format!("test {file_rel:?}::{name} not found"))
    }
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

fn method_name(method: Method) -> &'static str {
    match method {
        Method::Corpus { .. } => "corpus",
        Method::Test { .. } => "test",
        Method::LocalPty { .. } => "local-pty",
        Method::LiveDisplay { .. } => "live-display",
        Method::Uncovered { .. } => "none",
    }
}

/// Generate the machine-readable M1/M2 compatibility report.
///
/// Deterministic: repeated calls on one checkout return identical bytes. Every
/// `ci` row is verified here (corpus replay or test presence); a broken row is
/// an error, never a silent `ci` claim.
pub fn generate_report_json() -> Result<String, String> {
    // Validate the declared shape before emitting anything.
    let mut seen_areas = std::collections::BTreeSet::new();
    for area in AREAS {
        if !seen_areas.insert(*area) {
            return Err(format!("duplicate area {area:?}"));
        }
    }
    for row in ROWS {
        if !seen_areas.contains(row.area) {
            return Err(format!("row with unknown area {:?}", row.area));
        }
        match (row.status, row.method) {
            (Status::Ci, Method::Corpus { .. } | Method::Test { .. }) => {}
            (Status::Local, Method::LocalPty { .. } | Method::LiveDisplay { .. }) => {}
            (Status::Partial, Method::Corpus { .. }) => {}
            (Status::Gap, Method::Uncovered { .. }) => {}
            (status, method) => {
                return Err(format!(
                    "row {:?}/{:?} has inconsistent status {} and method {}",
                    row.area,
                    row.scenario,
                    status.as_str(),
                    method_name(method)
                ));
            }
        }
    }

    let revision = std::env::var("BITTY_COMPAT_REVISION").unwrap_or_else(|_| "unknown".to_string());
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"schema_version\": {REPORT_VERSION},\n"));
    out.push_str("  \"generator\": \"bitty-compat-lab compat_report\",\n");
    out.push_str(&format!(
        "  \"scope\": \"{}\",\n",
        json_escape(REPORT_SCOPE)
    ));
    out.push_str(&format!(
        "  \"revision\": \"{}\",\n",
        json_escape(&revision)
    ));
    out.push_str("  \"bounds\": {\n");
    out.push_str(&format!("    \"MAX_CORPUS_BYTES\": {MAX_CORPUS_BYTES},\n"));
    out.push_str(&format!("    \"MAX_ACTIONS\": {MAX_ACTIONS},\n"));
    out.push_str(&format!("    \"MAX_REPORT_BYTES\": {MAX_REPORT_BYTES}\n"));
    out.push_str("  },\n");

    // Environment probe: presence only, no execution, no host paths.
    out.push_str("  \"environment\": {\n");
    for (idx, tool) in PROBED_TOOLS.iter().enumerate() {
        let comma = if idx + 1 < PROBED_TOOLS.len() {
            ","
        } else {
            ""
        };
        out.push_str(&format!(
            "    \"{tool}\": {}{comma}\n",
            tool_path(tool).is_some()
        ));
    }
    out.push_str("  },\n");

    // Areas with row counts, in priority order.
    out.push_str("  \"areas\": [\n");
    for (idx, area) in AREAS.iter().enumerate() {
        let count = ROWS.iter().filter(|row| row.area == *area).count();
        let comma = if idx + 1 < AREAS.len() { "," } else { "" };
        out.push_str(&format!(
            "    {{\"name\": \"{name}\", \"priority\": {priority}, \"rows\": {count}}}{comma}\n",
            name = json_escape(area),
            priority = idx + 1,
        ));
    }
    out.push_str("  ],\n");

    // Rows in declared order (priority by area, scenario order within area).
    let mut summary = [0usize; 4];
    out.push_str("  \"rows\": [\n");
    for (idx, row) in ROWS.iter().enumerate() {
        let (method, evidence, check) = match row.method {
            Method::Corpus { path } => {
                let (bytes_len, actions_len, hash) = verify_corpus(path)?;
                let evidence = format!(
                    "{{\"kind\": \"corpus\", \"path\": \"{}\", \"bytes_len\": {bytes_len}, \"actions_len\": {actions_len}, \"state_hash\": \"{hash:016x}\", \"check\": \"bounded-deterministic-invariants\"}}",
                    json_escape(path)
                );
                ("corpus", evidence, "bounded-deterministic-invariants")
            }
            Method::Test { file, name } => {
                verify_test_present(file, name)?;
                let evidence = format!(
                    "{{\"kind\": \"test\", \"file\": \"{}\", \"name\": \"{}\", \"check\": \"test-present\"}}",
                    json_escape(file),
                    json_escape(name)
                );
                ("test", evidence, "test-present")
            }
            Method::LocalPty { scenario } => {
                let evidence = format!(
                    "{{\"kind\": \"local-pty\", \"scenario\": \"{}\", \"check\": \"env-gated-BITTY_COMPAT_LIVE\"}}",
                    json_escape(scenario)
                );
                ("local-pty", evidence, "env-gated-BITTY_COMPAT_LIVE")
            }
            Method::LiveDisplay { scenario } => {
                let evidence = format!(
                    "{{\"kind\": \"live-display\", \"scenario\": \"{}\", \"check\": \"env-gated-gui-tests\"}}",
                    json_escape(scenario)
                );
                ("live-display", evidence, "env-gated-gui-tests")
            }
            Method::Uncovered { reason } => {
                let evidence = format!(
                    "{{\"kind\": \"none\", \"reason\": \"{}\"}}",
                    json_escape(reason)
                );
                ("none", evidence, "uncovered")
            }
        };
        summary[match row.status {
            Status::Ci => 0,
            Status::Local => 1,
            Status::Partial => 2,
            Status::Gap => 3,
        }] += 1;
        let comma = if idx + 1 < ROWS.len() { "," } else { "" };
        out.push_str("    {\n");
        out.push_str(&format!("      \"area\": \"{}\",\n", json_escape(row.area)));
        out.push_str(&format!(
            "      \"scenario\": \"{}\",\n",
            json_escape(row.scenario)
        ));
        out.push_str(&format!("      \"status\": \"{}\",\n", row.status.as_str()));
        out.push_str(&format!("      \"method\": \"{method}\",\n"));
        out.push_str(&format!("      \"evidence\": {evidence},\n"));
        out.push_str(&format!(
            "      \"evidence_check\": \"{}\",\n",
            json_escape(check)
        ));
        out.push_str(&format!("      \"note\": \"{}\"\n", json_escape(row.note)));
        out.push_str(&format!("    }}{comma}\n"));
    }
    out.push_str("  ],\n");
    out.push_str("  \"summary\": {\n");
    out.push_str(&format!("    \"ci\": {},\n", summary[0]));
    out.push_str(&format!("    \"local\": {},\n", summary[1]));
    out.push_str(&format!("    \"partial\": {},\n", summary[2]));
    out.push_str(&format!("    \"gap\": {}\n", summary[3]));
    out.push_str("  }\n");
    out.push_str("}\n");

    if out.len() > MAX_REPORT_BYTES {
        return Err(format!(
            "report json {} > MAX_REPORT_BYTES {MAX_REPORT_BYTES}",
            out.len()
        ));
    }
    Ok(out)
}
