//! `bitty dev`: developer tracing, captures, dumps, and overlays (CTX-0174).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`dev` diagnostics section)
//! as refined by `docs/specifications/cli-contract-rfc.md` (`bitty dev` mixed
//! class, output envelope v1, exit codes 0-8) and the instrumentation scopes
//! owned by `docs/specifications/devtools-rfc.md`.
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty dev <verb> [subverb] [name] [flags]` where `<verb>` is one
//!   of `trace|capture|dump|overlay`:
//!   - `trace startup` — headless PB-1 startup tracing via
//!     `bitty-perf::startup::measure_headless_startup` (args parse, config,
//!     runtime create, layout, PTY spawn attempt, winit/wgpu/font probes,
//!     first bytes, first frame).
//!   - `trace latency [--iterations N]` — headless PB-4 key-to-screen tracing
//!     via `bitty-perf::latency::measure_latency` (bounded synthetic keys,
//!     echo model, stage breakdown, p50/p99/mean/max).
//!   - `capture [--layout single|split|stack|overlay]` — deterministic
//!     headless frame capture reusing the `--headless` smoke pattern: fresh
//!     [`Runtime`](bitty_runtime::Runtime), fixed synthetic VT corpus, one
//!     `tick`, then present stats plus a deterministic FNV-1a hash of the
//!     headless RGBA buffer (same layout plus same bytes is bit-identical).
//!   - `dump grid [--rows N] [--cols N]` — bounded grid text plus cursor via
//!     `bitty_runtime::inspect::grid_text_from_state` (the devtools trace
//!     path: `Runtime::tick` return plus inspect snapshots).
//!   - `dump scene` — scene dump: layout allocations, damage regions since
//!     the capture feed, present stats, and generation.
//!   - `dump atlas` — atlas dump: the captured snapshot rendered through a
//!     headless `GridRenderer` with a deterministic dev-only rasterizer,
//!     reporting draw-list fills/glyphs, atlas placements, texel bytes,
//!     atlas dimensions, and cache/atlas counters.
//!   - `overlay list` — renderer-overlay catalog (`damage|cells|glyphs|`
//!     `images|layout`) plus the paste banner, each with headless
//!     availability. GPU-bound overlays stay explicitly deferred per
//!     `cli.md`; listing them is informational, not a claim.
//!   - `overlay show <name>` — headless overlay proof where available:
//!     `damage` reports damage regions for the synthetic capture, `banner`
//!     proves the transient paste-banner paint pattern via a pending
//!     multi-line paste on a fresh headless runtime. Deferred overlays
//!     report `status: deferred` with the reason (exit 0, honest).
//! - Class: local only (no instance, safe-mode clean, no plugin VM). There
//!   are no runtime verbs in this slice: `--socket`/`--instance` are usage
//!   errors (exit 2), never silently ignored. Runtime traces over IPC remain
//!   future work owned by the DevTools protocol.
//! - `--format table` (default) is human output, not a machine contract.
//!   `--format json` / `--format jsonl` emit the versioned envelope (`v: 1`,
//!   `command: "dev"`, `ok`, `result`, plus `error` on failure) on stdout
//!   with diagnostics on stderr so JSON is never corrupted.
//! - `--` anywhere in `dev` mode is `UsageError` (exit 2): stray separators
//!   are never silently ignored.
//! - `bitty -- dev ...` runs a program named `dev` (escape hatch); the word
//!   `dev` as first positional is always this subcommand. A program literally
//!   named `dev` needs `bitty run -- dev ...` (or legacy `bitty -- dev ...`).
//! - `--help` (anywhere after `dev`, or `bitty --help dev`) prints help to
//!   stdout, exit 0, and never builds a runtime.
//!
//! # Read-only surface (no new authority)
//!
//! - Tracing reuses `bitty-perf` startup/latency modules (same budgets PB-1
//!   `100/200 ms`, PB-4 `8/15 ms`); no new measurement code is invented.
//! - Captures/dumps reuse the headless smoke pattern (`Runtime::with_defaults`
//!   plus `Surface::headless`, deterministic synthetic bytes, `tick`): no
//!   display server, window, adapter, or font file is contacted.
//! - The atlas rasterizer below (`DevRasterizer`) is dev-only and
//!   deterministic (fixed 8x6 coverage derived from the character code); it
//!   never touches the platform font stack, so dumps are byte-identical
//!   across hosts.
//! - Terminal bytes are untrusted input; the corpus is a fixed internal
//!   constant, and every numeric flag is bounded before any runtime is built.
//!
//! # Exit codes (stable taxonomy)
//!
//! - `0` success (including deferred overlay reports: informational, not a
//!   failure).
//! - `2` usage error (missing/unknown verb or subverb, extra positional,
//!   unknown flag, bad `--format`/`--iterations`/`--rows`/`--cols`/
//!   `--layout`, misplaced verb-specific flag, stray `--`, `--socket` or
//!   `--instance` on a local command, unknown overlay name).
//! - `1` generic (unexpected failure after parsing, e.g. the headless runtime
//!   or renderer failed to build; never used for the usage paths above).

#![forbid(unsafe_code)]

use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success.
pub const EXIT_OK: i32 = 0;
/// Generic failure (unexpected post-parse failure).
pub const EXIT_GENERIC: i32 = 1;
/// CLI usage error.
pub const EXIT_USAGE: i32 = 2;

// ---------------------------------------------------------------------------
// Bounds (fail closed with exit 2 before any runtime is built)
// ---------------------------------------------------------------------------

/// Maximum bytes for a verb/subverb/name token.
pub const MAX_DEV_TOKEN_LEN: usize = 32;
/// Maximum bytes for a `--format` value.
pub const MAX_DEV_FORMAT_LEN: usize = 16;
/// Maximum bytes for a `--layout` spec.
pub const MAX_DEV_LAYOUT_LEN: usize = 64;
/// Default latency iterations (fast enough for interactive use).
pub const DEFAULT_LATENCY_ITERATIONS: usize = 20;
/// Maximum latency iterations (bounded trace time).
pub const MAX_LATENCY_ITERATIONS: usize = 1000;
/// Default/maximum grid dump rows (mirrors `INSPECT_MAX_ROWS`).
pub const DEFAULT_DUMP_ROWS: usize = 64;
/// Maximum grid dump rows.
pub const MAX_DUMP_ROWS: usize = 64;
/// Default/maximum grid dump columns (mirrors `INSPECT_MAX_COLS`).
pub const DEFAULT_DUMP_COLS: usize = 256;
/// Maximum grid dump columns.
pub const MAX_DUMP_COLS: usize = 256;

/// Deterministic synthetic VT corpus for captures and dumps.
///
/// Identical to the `--headless` smoke payload so dev captures stay comparable
/// with the CI smoke proof: printable text, SGR color, OSC title, erase.
/// Fixed internal constant — never user input, no wall clock, no font file.
///
/// CTX-0234: rows 1..=6 are full-width marker rows (80 cells each) so
/// layout captures stay distinguishable without relying on primary-grid
/// duplication (session-less unfocused tiles present erased): split shows
/// markers on the left tile only, overlay blanks rows 5..=6 at cols 5..=24,
/// stack/single cover the grid. Sparse first-row-only bytes rendered
/// split and single bit-identical.
pub const DEV_SYNTHETIC_CORPUS: &[u8] =
    b"bitty headless smoke \x1b[31mred\x1b[0m \x1b]0;bitty-smoke\x07\r\n\
    11111111111111111111111111111111111111111111111111111111111111111111111111111111\r\n\
    22222222222222222222222222222222222222222222222222222222222222222222222222222222\r\n\
    33333333333333333333333333333333333333333333333333333333333333333333333333333333\r\n\
    44444444444444444444444444444444444444444444444444444444444444444444444444444444\r\n\
    55555555555555555555555555555555555555555555555555555555555555555555555555555555\r\n\
    66666666666666666666666666666666666666666666666666666666666666666666666666666666\r\n";

/// Synthetic multi-line paste that trips the paste inspection gate (embedded
/// newline) so `overlay show banner` can prove the transient banner paint
/// pattern on a fresh headless runtime without delivering any bytes.
pub const DEV_BANNER_PASTE: &str = "bitty overlay proof\nline2";

// ---------------------------------------------------------------------------
// Format
// ---------------------------------------------------------------------------

/// Output shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevFormat {
    /// Human table (not a machine contract).
    Table,
    /// Single versioned JSON envelope.
    Json,
    /// Same envelope, single line.
    Jsonl,
}

impl DevFormat {
    /// Parse `--format` (`None` means table default).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(Self::Table),
            Some(v) => {
                if v.len() > MAX_DEV_FORMAT_LEN || v.contains('\0') {
                    return Err(format!(
                        "bitty dev: unknown --format {v:?} (want table|json|jsonl)"
                    ));
                }
                match v.trim().to_ascii_lowercase().as_str() {
                    "table" => Ok(Self::Table),
                    "json" => Ok(Self::Json),
                    "jsonl" => Ok(Self::Jsonl),
                    other => Err(format!(
                        "bitty dev: unknown --format {other:?} (want table|json|jsonl)"
                    )),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Capture layout composition (mirrors the `--headless` layout proof).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureLayout {
    /// Single leaf.
    Single,
    /// Horizontal split (two leaves).
    Split,
    /// Stack (two leaves, last on top).
    Stack,
    /// Overlay (base plus floating leaf).
    Overlay,
}

impl CaptureLayout {
    /// Parses a `--layout` spec (case-insensitive, surrounding whitespace ignored).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "single" => Some(Self::Single),
            "split" => Some(Self::Split),
            "stack" => Some(Self::Stack),
            "overlay" => Some(Self::Overlay),
            _ => None,
        }
    }

    /// Stable name for output.
    pub fn name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Split => "split",
            Self::Stack => "stack",
            Self::Overlay => "overlay",
        }
    }
}

/// Overlay catalog entry name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayName {
    /// Damage-region overlay (headless-available).
    Damage,
    /// Cell-grid overlay (deferred: renderer architecture).
    Cells,
    /// Glyph overlay (deferred: renderer architecture).
    Glyphs,
    /// Image overlay (deferred: renderer architecture).
    Images,
    /// Layout overlay (deferred: renderer architecture).
    Layout,
    /// Transient paste banner (headless-available via pending paste).
    Banner,
}

impl OverlayName {
    /// Parses an overlay name (case-insensitive).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "damage" => Some(Self::Damage),
            "cells" => Some(Self::Cells),
            "glyphs" => Some(Self::Glyphs),
            "images" => Some(Self::Images),
            "layout" => Some(Self::Layout),
            "banner" => Some(Self::Banner),
            _ => None,
        }
    }

    /// Stable name for output.
    pub fn name(self) -> &'static str {
        match self {
            Self::Damage => "damage",
            Self::Cells => "cells",
            Self::Glyphs => "glyphs",
            Self::Images => "images",
            Self::Layout => "layout",
            Self::Banner => "banner",
        }
    }

    /// One-line description for the catalog.
    pub fn description(self) -> &'static str {
        match self {
            Self::Damage => "damaged grid regions for the last frame",
            Self::Cells => "cell-grid overlay (renderer architecture)",
            Self::Glyphs => "glyph overlay (renderer architecture)",
            Self::Images => "image overlay (renderer architecture)",
            Self::Layout => "layout overlay (renderer architecture)",
            Self::Banner => "transient paste-confirmation banner",
        }
    }

    /// Headless availability: `None` means provable headless, `Some(reason)`
    /// means explicitly deferred per `cli.md`.
    pub fn deferred_reason(self) -> Option<&'static str> {
        match self {
            Self::Damage | Self::Banner => None,
            Self::Cells | Self::Glyphs | Self::Images | Self::Layout => {
                Some("deferred: renderer overlays depend on renderer architecture (cli.md)")
            }
        }
    }

    /// All catalog entries in stable order.
    pub fn all() -> [Self; 6] {
        [
            Self::Damage,
            Self::Cells,
            Self::Glyphs,
            Self::Images,
            Self::Layout,
            Self::Banner,
        ]
    }
}

/// Validated `bitty dev` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevRequest {
    /// `trace startup`.
    TraceStartup,
    /// `trace latency [--iterations N]`.
    TraceLatency {
        /// Bounded sample count (`1..=MAX_LATENCY_ITERATIONS`).
        iterations: usize,
    },
    /// `capture [--layout SPEC]`.
    Capture {
        /// Layout composition under capture.
        layout: CaptureLayout,
    },
    /// `dump grid [--rows N] [--cols N]`.
    DumpGrid {
        /// Bounded row cap.
        rows: usize,
        /// Bounded column cap.
        cols: usize,
    },
    /// `dump scene`.
    DumpScene,
    /// `dump atlas`.
    DumpAtlas,
    /// `overlay list`.
    OverlayList,
    /// `overlay show <name>`.
    OverlayShow {
        /// Catalog entry to prove.
        name: OverlayName,
    },
}

impl DevRequest {
    /// Verb token for output (`trace|capture|dump|overlay`).
    pub fn verb(&self) -> &'static str {
        match self {
            Self::TraceStartup | Self::TraceLatency { .. } => "trace",
            Self::Capture { .. } => "capture",
            Self::DumpGrid { .. } | Self::DumpScene | Self::DumpAtlas => "dump",
            Self::OverlayList | Self::OverlayShow { .. } => "overlay",
        }
    }

    /// Subverb detail for output (`startup|latency|<layout>|grid|...`).
    pub fn detail(&self) -> String {
        match self {
            Self::TraceStartup => "startup".to_string(),
            Self::TraceLatency { iterations } => format!("latency iterations={iterations}"),
            Self::Capture { layout } => layout.name().to_string(),
            Self::DumpGrid { rows, cols } => format!("grid rows={rows} cols={cols}"),
            Self::DumpScene => "scene".to_string(),
            Self::DumpAtlas => "atlas".to_string(),
            Self::OverlayList => "list".to_string(),
            Self::OverlayShow { name } => format!("show {}", name.name()),
        }
    }
}

/// Validated output options for `bitty dev`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevOptions {
    /// Output shape.
    pub format: DevFormat,
    /// Accepted for parity (`NO_COLOR` also honored); tables are plain text.
    pub no_color: bool,
}

// ---------------------------------------------------------------------------
// Parse errors, usage, help
// ---------------------------------------------------------------------------

/// `bitty dev` parse outcome: help, or a fail-closed usage diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevParseError {
    /// `-h`/`--help` anywhere after `dev`.
    Help,
    /// Usage error diagnostic (stderr only, exit [`EXIT_USAGE`]).
    Usage(String),
}

impl DevParseError {
    /// Stderr diagnostic for the error.
    pub fn message(&self) -> String {
        match self {
            Self::Help => dev_help_text(),
            Self::Usage(diagnostic) => diagnostic.clone(),
        }
    }
}

/// Short usage for stderr (fail-closed exit 2 trailer).
#[must_use]
pub fn dev_usage() -> String {
    "usage: bitty dev <trace|capture|dump|overlay> [args] [--format table|json|jsonl] [--no-color]\n       bitty dev trace <startup|latency> [--iterations N]\n       bitty dev capture [--layout single|split|stack|overlay]\n       bitty dev dump <grid|scene|atlas> [--rows N] [--cols N]\n       bitty dev overlay <list|show <damage|cells|glyphs|images|layout|banner>>\n\nverbs:\n  trace     headless PB-1 startup / PB-4 latency tracing (bitty-perf, local)\n  capture   deterministic headless frame capture (stats + RGBA hash, local)\n  dump      grid text / scene / atlas dumps from a headless capture (local)\n  overlay   renderer-overlay catalog and headless proofs (local; GPU-bound entries deferred)"
        .to_string()
}

/// Full help for `bitty dev --help` (stdout, exit 0).
#[must_use]
pub fn dev_help_text() -> String {
    "bitty dev — developer tracing, captures, dumps, and overlays (local)\n\
     \n\
     Usage: bitty dev <trace|capture|dump|overlay> [args] [--format table|json|jsonl] [--no-color]\n\
     \n\
     Verbs (all local class: no instance, safe-mode clean, no plugin VM):\n  \
       trace startup                 Headless PB-1 startup phases (bitty-perf).\n  \
       trace latency [--iterations N]  Headless PB-4 key-to-screen trace\n  \
                                     (default 20, range 1..=1000).\n  \
       capture [--layout SPEC]       Deterministic headless frame capture:\n  \
                                     SPEC = single|split|stack|overlay\n  \
                                     (default single). Reports present stats,\n  \
                                     cold-queue counters, and an FNV-1a hash\n  \
                                     of the headless RGBA buffer.\n  \
       dump grid [--rows N] [--cols N]  Bounded grid text plus cursor\n  \
                                     (defaults rows=64 cols=256, capped at\n  \
                                     64x256 like the inspect path).\n  \
       dump scene                    Layout allocations, damage regions,\n  \
                                     present stats, and generation.\n  \
       dump atlas                    Headless GridRenderer atlas report:\n  \
                                     fills/glyphs, placements, texel bytes,\n  \
                                     dimensions, cache/atlas counters.\n  \
       overlay list                  Overlay catalog with headless\n  \
                                     availability (GPU-bound entries deferred).\n  \
       overlay show <name>           Headless overlay proof where available\n  \
                                     (damage, banner); deferred entries report\n  \
                                     status deferred with the reason.\n\
     \n\
     Options:\n  \
       --format SHAPE  table (default, human, not a contract) | json | jsonl (envelope v1)\n  \
       --no-color      Accepted for parity; tables carry no ANSI coloring.\n  \
       --iterations N  trace latency sample count (1..=1000).\n  \
       --rows N        dump grid row cap (1..=64).\n  \
       --cols N        dump grid column cap (1..=256).\n  \
       --layout SPEC   capture composition (single|split|stack|overlay).\n  \
       -h, --help      Print this help and exit (never builds a runtime).\n\
     \n\
     Local-only: --socket/--instance are rejected (exit 2). Runtime traces\n  \
       over IPC are future DevTools-protocol work, not invented here.\n\
     \n\
     Output contract:\n  \
       Stdout carries the result; stderr carries diagnostics. JSON/JSONL use\n  \
       envelope {\"v\":1,\"command\":\"dev\",\"ok\":true,\"result\":{\"verb\":...}}.\n  \
       Usage errors (exit 2) go to stderr with no stdout envelope. Generic\n  \
       post-parse failures emit ok:false envelopes (exit 1).\n\
     \n\
     Exit codes:\n  \
       0 success (deferred overlay reports are informational)\n  \
       2 usage error (missing/unknown verb, extra arg, bad flag value, stray --)\n  \
       1 generic error (headless runtime/renderer failed after parsing)\n\
     \n\
     Examples:\n  \
       bitty dev trace startup\n  \
       bitty dev trace latency --iterations 50 --format json\n  \
       bitty dev capture --layout split\n  \
       bitty dev dump grid --format json\n  \
       bitty dev overlay list\n"
    .to_string()
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Validates a verb/subverb/name token shape (fail closed before dispatch).
fn validate_token(token: &str, what: &str) -> Result<(), DevParseError> {
    if token.is_empty() || token.len() > MAX_DEV_TOKEN_LEN || token.contains('\0') {
        return Err(DevParseError::Usage(format!(
            "bitty dev: invalid {what} {token:?} (want 1..={MAX_DEV_TOKEN_LEN} bytes, no NUL)\n{}",
            dev_usage()
        )));
    }
    let ok = token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(DevParseError::Usage(format!(
            "bitty dev: invalid {what} {token:?} (want ^[a-z0-9_-]+$, case-insensitive)\n{}",
            dev_usage()
        )));
    }
    Ok(())
}

/// Parses a bounded `1..=max` integer flag value.
fn parse_bounded_int(
    raw: &str,
    flag: &str,
    max: usize,
    where_: &str,
) -> Result<usize, DevParseError> {
    if raw.is_empty() || raw.len() > 8 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DevParseError::Usage(format!(
            "bitty dev: --{flag} must be an integer in 1..={max} (got {raw:?}, {where_})\n{}",
            dev_usage()
        )));
    }
    match raw.parse::<usize>() {
        Ok(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(DevParseError::Usage(format!(
            "bitty dev: --{flag} must be an integer in 1..={max} (got {raw:?}, {where_})\n{}",
            dev_usage()
        ))),
    }
}

/// Rejects `--socket`/`--instance` (local-only command) for any spelling.
fn reject_remote_flag(token: &str) -> Result<(), DevParseError> {
    let stem = token.split('=').next().unwrap_or(token);
    if stem == "--socket" || stem == "--instance" {
        return Err(DevParseError::Usage(format!(
            "bitty dev: {stem} is rejected (dev is local-only: no instance, no IPC)\n{}",
            dev_usage()
        )));
    }
    Ok(())
}

/// Parses raw tokens after the `dev` word into a validated request.
///
/// - `-h`/`--help` anywhere is [`DevParseError::Help`].
/// - Every other failure is [`DevParseError::Usage`] (exit 2, stderr only).
pub fn parse_dev_request(tokens: &[String]) -> Result<(DevRequest, DevOptions), DevParseError> {
    let mut verb: Option<String> = None;
    let mut sub: Option<String> = None;
    let mut name: Option<String> = None;
    let mut extra: Vec<String> = Vec::new();
    let mut format_raw: Option<String> = None;
    let mut no_color = false;
    let mut iterations_raw: Option<String> = None;
    let mut rows_raw: Option<String> = None;
    let mut cols_raw: Option<String> = None;
    let mut layout_raw: Option<String> = None;

    let mut i = 0usize;
    while i < tokens.len() {
        let token = &tokens[i];
        if token.contains('\0') {
            return Err(DevParseError::Usage(format!(
                "bitty dev: argument must not contain NUL\n{}",
                dev_usage()
            )));
        }
        // Stray separator is a usage error (never silently ignored).
        if token == "--" {
            return Err(DevParseError::Usage(format!(
                "bitty dev: unexpected `--` (dev takes no pass-through separator)\n{}",
                dev_usage()
            )));
        }
        // Help anywhere wins (never needs a runtime).
        if token == "-h" || token == "--help" {
            return Err(DevParseError::Help);
        }
        // `--flag=value` forms first.
        if let Some(val) = token.strip_prefix("--format=") {
            reject_remote_flag(token)?;
            format_raw = Some(val.to_string());
            i += 1;
            continue;
        }
        if let Some(val) = token.strip_prefix("--iterations=") {
            iterations_raw = Some(val.to_string());
            i += 1;
            continue;
        }
        if let Some(val) = token.strip_prefix("--rows=") {
            rows_raw = Some(val.to_string());
            i += 1;
            continue;
        }
        if let Some(val) = token.strip_prefix("--cols=") {
            cols_raw = Some(val.to_string());
            i += 1;
            continue;
        }
        if let Some(val) = token.strip_prefix("--layout=") {
            if val.len() > MAX_DEV_LAYOUT_LEN {
                return Err(DevParseError::Usage(format!(
                    "bitty dev: --layout must be 1..={MAX_DEV_LAYOUT_LEN} bytes (got {})\n{}",
                    val.len(),
                    dev_usage()
                )));
            }
            layout_raw = Some(val.to_string());
            i += 1;
            continue;
        }
        // Remote-target flags are rejected in every spelling (local-only).
        if token.starts_with("--socket") || token.starts_with("--instance") {
            reject_remote_flag(token)?;
            // `--socket`/`--instance` bare (space form) still consume nothing:
            // the value, if any, is reported as an extra positional below so
            // the diagnostic stays fail-closed without guessing intent.
            return Err(DevParseError::Usage(format!(
                "bitty dev: {token:?} is rejected (dev is local-only: no instance, no IPC)\n{}",
                dev_usage()
            )));
        }
        match token.as_str() {
            "--format" => {
                if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                    format_raw = Some(tokens[i + 1].clone());
                    i += 2;
                } else {
                    return Err(DevParseError::Usage(format!(
                        "bitty dev: --format needs a value (table|json|jsonl)\n{}",
                        dev_usage()
                    )));
                }
            }
            "--no-color" => {
                no_color = true;
                i += 1;
            }
            "--iterations" => {
                if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                    iterations_raw = Some(tokens[i + 1].clone());
                    i += 2;
                } else {
                    return Err(DevParseError::Usage(format!(
                        "bitty dev: --iterations needs a value (1..={MAX_LATENCY_ITERATIONS})\n{}",
                        dev_usage()
                    )));
                }
            }
            "--rows" => {
                if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                    rows_raw = Some(tokens[i + 1].clone());
                    i += 2;
                } else {
                    return Err(DevParseError::Usage(format!(
                        "bitty dev: --rows needs a value (1..={MAX_DUMP_ROWS})\n{}",
                        dev_usage()
                    )));
                }
            }
            "--cols" => {
                if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                    cols_raw = Some(tokens[i + 1].clone());
                    i += 2;
                } else {
                    return Err(DevParseError::Usage(format!(
                        "bitty dev: --cols needs a value (1..={MAX_DUMP_COLS})\n{}",
                        dev_usage()
                    )));
                }
            }
            "--layout" => {
                if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                    let val = tokens[i + 1].clone();
                    if val.len() > MAX_DEV_LAYOUT_LEN {
                        return Err(DevParseError::Usage(format!(
                            "bitty dev: --layout must be 1..={MAX_DEV_LAYOUT_LEN} bytes (got {})\n{}",
                            val.len(),
                            dev_usage()
                        )));
                    }
                    layout_raw = Some(val);
                    i += 2;
                } else {
                    return Err(DevParseError::Usage(format!(
                        "bitty dev: --layout needs a value (single|split|stack|overlay)\n{}",
                        dev_usage()
                    )));
                }
            }
            other if other.starts_with('-') => {
                return Err(DevParseError::Usage(format!(
                    "bitty dev: unknown flag {other:?} (want --format|--no-color|--iterations|--rows|--cols|--layout)\n{}",
                    dev_usage()
                )));
            }
            _ => {
                if verb.is_none() {
                    validate_token(token, "verb")?;
                    verb = Some(token.clone());
                } else if sub.is_none() {
                    validate_token(token, "subverb")?;
                    sub = Some(token.clone());
                } else if name.is_none() {
                    validate_token(token, "name")?;
                    name = Some(token.clone());
                } else {
                    extra.push(token.clone());
                }
                i += 1;
            }
        }
    }

    if !extra.is_empty() {
        return Err(DevParseError::Usage(format!(
            "bitty dev: unexpected argument '{}'\n{}",
            extra[0],
            dev_usage()
        )));
    }

    let format = DevFormat::parse(format_raw.as_deref())
        .map_err(|message| DevParseError::Usage(format!("{message}\n{}", dev_usage())))?;
    let options = DevOptions { format, no_color };

    let verb_raw = verb.ok_or_else(|| {
        DevParseError::Usage(format!(
            "bitty dev: missing <verb> (want trace|capture|dump|overlay)\n{}",
            dev_usage()
        ))
    })?;
    let no_verb_flags = |flag: &str| -> DevParseError {
        DevParseError::Usage(format!(
            "bitty dev: --{flag} applies only to its owning verb (see `bitty dev --help`)\n{}",
            dev_usage()
        ))
    };

    match verb_raw.trim().to_ascii_lowercase().as_str() {
        "trace" => {
            if rows_raw.is_some() {
                return Err(no_verb_flags("rows"));
            }
            if cols_raw.is_some() {
                return Err(no_verb_flags("cols"));
            }
            if layout_raw.is_some() {
                return Err(no_verb_flags("layout"));
            }
            let sub_raw = sub.ok_or_else(|| {
                DevParseError::Usage(format!(
                    "bitty dev trace: missing <subverb> (want startup|latency)\n{}",
                    dev_usage()
                ))
            })?;
            if name.is_some() {
                return Err(DevParseError::Usage(format!(
                    "bitty dev trace: unexpected argument '{}'\n{}",
                    name.unwrap_or_default(),
                    dev_usage()
                )));
            }
            match sub_raw.trim().to_ascii_lowercase().as_str() {
                "startup" => {
                    if iterations_raw.is_some() {
                        return Err(DevParseError::Usage(format!(
                            "bitty dev: --iterations applies only to `trace latency`\n{}",
                            dev_usage()
                        )));
                    }
                    Ok((DevRequest::TraceStartup, options))
                }
                "latency" => {
                    let iterations = match iterations_raw.as_deref() {
                        None => DEFAULT_LATENCY_ITERATIONS,
                        Some(raw) => parse_bounded_int(
                            raw,
                            "iterations",
                            MAX_LATENCY_ITERATIONS,
                            "for `trace latency`",
                        )?,
                    };
                    Ok((DevRequest::TraceLatency { iterations }, options))
                }
                other => Err(DevParseError::Usage(format!(
                    "bitty dev trace: unknown subverb {other:?} (want startup|latency)\n{}",
                    dev_usage()
                ))),
            }
        }
        "capture" => {
            if sub.is_some() {
                return Err(DevParseError::Usage(format!(
                    "bitty dev capture: unexpected argument '{}'\n{}",
                    sub.unwrap_or_default(),
                    dev_usage()
                )));
            }
            if iterations_raw.is_some() {
                return Err(no_verb_flags("iterations"));
            }
            if rows_raw.is_some() {
                return Err(no_verb_flags("rows"));
            }
            if cols_raw.is_some() {
                return Err(no_verb_flags("cols"));
            }
            let layout = match layout_raw.as_deref() {
                None => CaptureLayout::Single,
                Some(raw) => CaptureLayout::parse(raw).ok_or_else(|| {
                    DevParseError::Usage(format!(
                        "bitty dev capture: unknown --layout {raw:?} (want single|split|stack|overlay)\n{}",
                        dev_usage()
                    ))
                })?,
            };
            Ok((DevRequest::Capture { layout }, options))
        }
        "dump" => {
            if iterations_raw.is_some() {
                return Err(no_verb_flags("iterations"));
            }
            if layout_raw.is_some() {
                return Err(no_verb_flags("layout"));
            }
            let sub_raw = sub.ok_or_else(|| {
                DevParseError::Usage(format!(
                    "bitty dev dump: missing <subverb> (want grid|scene|atlas)\n{}",
                    dev_usage()
                ))
            })?;
            match sub_raw.trim().to_ascii_lowercase().as_str() {
                "grid" => {
                    if name.is_some() {
                        return Err(DevParseError::Usage(format!(
                            "bitty dev dump grid: unexpected argument '{}'\n{}",
                            name.unwrap_or_default(),
                            dev_usage()
                        )));
                    }
                    let rows = match rows_raw.as_deref() {
                        None => DEFAULT_DUMP_ROWS,
                        Some(raw) => {
                            parse_bounded_int(raw, "rows", MAX_DUMP_ROWS, "for `dump grid`")?
                        }
                    };
                    let cols = match cols_raw.as_deref() {
                        None => DEFAULT_DUMP_COLS,
                        Some(raw) => {
                            parse_bounded_int(raw, "cols", MAX_DUMP_COLS, "for `dump grid`")?
                        }
                    };
                    Ok((DevRequest::DumpGrid { rows, cols }, options))
                }
                "scene" => {
                    if name.is_some() {
                        return Err(DevParseError::Usage(format!(
                            "bitty dev dump scene: unexpected argument '{}'\n{}",
                            name.unwrap_or_default(),
                            dev_usage()
                        )));
                    }
                    if rows_raw.is_some() {
                        return Err(no_verb_flags("rows"));
                    }
                    if cols_raw.is_some() {
                        return Err(no_verb_flags("cols"));
                    }
                    Ok((DevRequest::DumpScene, options))
                }
                "atlas" => {
                    if name.is_some() {
                        return Err(DevParseError::Usage(format!(
                            "bitty dev dump atlas: unexpected argument '{}'\n{}",
                            name.unwrap_or_default(),
                            dev_usage()
                        )));
                    }
                    if rows_raw.is_some() {
                        return Err(no_verb_flags("rows"));
                    }
                    if cols_raw.is_some() {
                        return Err(no_verb_flags("cols"));
                    }
                    Ok((DevRequest::DumpAtlas, options))
                }
                other => Err(DevParseError::Usage(format!(
                    "bitty dev dump: unknown subverb {other:?} (want grid|scene|atlas)\n{}",
                    dev_usage()
                ))),
            }
        }
        "overlay" => {
            if iterations_raw.is_some() {
                return Err(no_verb_flags("iterations"));
            }
            if rows_raw.is_some() {
                return Err(no_verb_flags("rows"));
            }
            if cols_raw.is_some() {
                return Err(no_verb_flags("cols"));
            }
            if layout_raw.is_some() {
                return Err(no_verb_flags("layout"));
            }
            let sub_raw = sub.ok_or_else(|| {
                DevParseError::Usage(format!(
                    "bitty dev overlay: missing <subverb> (want list|show)\n{}",
                    dev_usage()
                ))
            })?;
            match sub_raw.trim().to_ascii_lowercase().as_str() {
                "list" => {
                    if name.is_some() {
                        return Err(DevParseError::Usage(format!(
                            "bitty dev overlay list: unexpected argument '{}'\n{}",
                            name.unwrap_or_default(),
                            dev_usage()
                        )));
                    }
                    Ok((DevRequest::OverlayList, options))
                }
                "show" => {
                    let name_raw = name.ok_or_else(|| {
                        DevParseError::Usage(format!(
                            "bitty dev overlay show: missing <name> (want damage|cells|glyphs|images|layout|banner)\n{}",
                            dev_usage()
                        ))
                    })?;
                    let overlay = OverlayName::parse(&name_raw).ok_or_else(|| {
                        DevParseError::Usage(format!(
                            "bitty dev overlay show: unknown overlay {name_raw:?} (want damage|cells|glyphs|images|layout|banner)\n{}",
                            dev_usage()
                        ))
                    })?;
                    Ok((DevRequest::OverlayShow { name: overlay }, options))
                }
                other => Err(DevParseError::Usage(format!(
                    "bitty dev overlay: unknown subverb {other:?} (want list|show)\n{}",
                    dev_usage()
                ))),
            }
        }
        other => Err(DevParseError::Usage(format!(
            "bitty dev: unknown verb {other:?} (want trace|capture|dump|overlay)\n{}",
            dev_usage()
        ))),
    }
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

/// Escape a string for embedding in JSON output (control bytes safe).
#[must_use]
pub fn json_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Deterministic FNV-1a 64-bit hash rendered as 16 lowercase hex digits.
///
/// Used to fingerprint headless RGBA buffers so captures prove determinism
/// (same layout plus same bytes is bit-identical) without writing files.
#[must_use]
pub fn fnv1a_hex(bytes: &[u8]) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// Renders the success envelope around an already-serialized result object.
#[must_use]
pub fn format_success_envelope(verb: &str, detail: &str, result_json: &str) -> String {
    format!(
        "{{\"v\":1,\"command\":\"dev\",\"ok\":true,\"result\":{{\"verb\":\"{}\",\"detail\":\"{}\",{}}}}}",
        json_escape(verb),
        json_escape(detail),
        result_json
    )
}

/// Renders the failure envelope for post-parse generic failures.
#[must_use]
pub fn format_error_envelope(verb: &str, detail: &str, code: &str, message: &str) -> String {
    format!(
        "{{\"v\":1,\"command\":\"dev\",\"ok\":false,\"error\":{{\"class\":\"Internal\",\"code\":\"{}\",\"message\":\"{}\"}},\"result\":{{\"verb\":\"{}\",\"detail\":\"{}\"}}}}",
        json_escape(code),
        json_escape(message),
        json_escape(verb),
        json_escape(detail)
    )
}

// ---------------------------------------------------------------------------
// Headless capture helpers (reuse the `--headless` smoke pattern)
// ---------------------------------------------------------------------------

/// Builds the capture layout composition (mirrors the layout proof in
/// `main.rs`: same bytes plus same layout is deterministic).
fn build_capture_layout(layout: CaptureLayout) -> bitty_runtime::LayoutNode {
    use bitty_runtime::{LayoutNode, SplitAxis, UiRect, View, ViewId};
    match layout {
        CaptureLayout::Single => LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        CaptureLayout::Split => LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        ),
        CaptureLayout::Stack => LayoutNode::stack(vec![
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        ]),
        CaptureLayout::Overlay => LayoutNode::overlay(
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 20, 10)),
            UiRect::new(5, 5, 20, 10),
        ),
    }
}

/// Deterministic headless capture: fresh runtime, fixed corpus, one tick.
struct Capture {
    /// Frame counter presented.
    frame: u64,
    /// Fill rectangles in the presented draw list.
    fills: usize,
    /// Glyph instances in the presented draw list.
    glyphs: usize,
    /// Snapshot generation presented.
    generation: u64,
    /// Headless RGBA byte length.
    rgba_len: usize,
    /// FNV-1a hash of the headless RGBA buffer.
    rgba_hash: String,
    /// Surface extent (`WxH` pixels).
    extent: String,
    /// Cold-queue length at capture time.
    cold_len: usize,
    /// Cold-queue capacity.
    cold_cap: usize,
    /// Cold-queue dropped counter.
    cold_dropped: u64,
    /// Whether the runtime is headless (always true here).
    headless: bool,
}

/// Runs one deterministic capture with `layout` (errors only when the
/// headless runtime fails to build, which is a generic failure).
fn run_capture(layout: CaptureLayout) -> Result<Capture, String> {
    let mut runtime = bitty_runtime::Runtime::with_defaults()
        .map_err(|err| format!("bitty dev: headless runtime failed: {err}"))?;
    runtime.set_layout(build_capture_layout(layout));
    runtime.handle_pty_bytes(DEV_SYNTHETIC_CORPUS);
    let cold_len = runtime.cold_queue_len();
    let cold_cap = runtime.cold_queue_capacity();
    let cold_dropped = runtime.cold_queue_dropped();
    let stats = runtime
        .tick()
        .ok_or_else(|| "bitty dev: capture tick produced no present (idle)".to_string())?;
    let rgba = runtime
        .headless_rgba()
        .ok_or_else(|| "bitty dev: capture produced no headless RGBA".to_string())?;
    let extent = runtime
        .surface_extent()
        .map(|e| format!("{}x{}", e.width(), e.height()))
        .unwrap_or_else(|| "(none)".to_string());
    Ok(Capture {
        frame: stats.frame,
        fills: stats.fills,
        glyphs: stats.glyphs,
        generation: stats.generation,
        rgba_len: rgba.len(),
        rgba_hash: fnv1a_hex(&rgba),
        extent,
        cold_len,
        cold_cap,
        cold_dropped,
        headless: runtime.is_headless(),
    })
}

// ---------------------------------------------------------------------------
// Dev-only deterministic rasterizer (atlas dumps without a font stack)
// ---------------------------------------------------------------------------

/// Deterministic dev-only rasterizer for `dump atlas`.
///
/// Fixed 8x6 coverage derived from the character code (never the platform
/// font stack), so atlas dumps are byte-identical across hosts. Whitespace
/// rasterizes to a cacheable blank (`Ok(None)`), exactly like the render
/// test seam. Labeled dev-only: production rendering keeps its own path.
struct DevRasterizer {
    next_id: u64,
}

impl DevRasterizer {
    fn new() -> Self {
        Self { next_id: 0 }
    }

    fn bitmap_for(character: char) -> bitty_render::GlyphBitmap {
        use bitty_render::glyph::{BitmapFormat, GlyphBitmap, GlyphMetrics};
        let code = u32::from(character) as usize;
        let width: i32 = i32::try_from(code % 3 + 6).unwrap_or(8);
        let height: i32 = 6;
        let data: Vec<u8> = (0..(width as usize) * (height as usize) * 3)
            .map(|i| (0x30 + ((code + i) % 0x50)) as u8)
            .collect();
        GlyphBitmap::try_new(
            GlyphMetrics {
                left: 0,
                top: 1,
                width,
                height,
                advance: [width, 0],
            },
            BitmapFormat::Rgb,
            data,
        )
        .expect("dev rasterizer bitmap dimensions are valid")
    }
}

impl bitty_render::GlyphRasterizer for DevRasterizer {
    fn load_font(
        &mut self,
        _query: &bitty_render::FontQuery,
    ) -> Result<bitty_render::FontId, bitty_render::RenderError> {
        Ok(bitty_render::FontId::next(&mut self.next_id))
    }

    fn rasterize(
        &mut self,
        key: bitty_render::RasterKey,
    ) -> Result<Option<bitty_render::GlyphBitmap>, bitty_render::RenderError> {
        if key.character == ' ' || key.character == '\t' {
            return Ok(None);
        }
        Ok(Some(Self::bitmap_for(key.character)))
    }
}

// ---------------------------------------------------------------------------
// Executors (each returns table text plus the JSON result object)
// ---------------------------------------------------------------------------

/// Executor output: human table plus the pre-serialized JSON result object.
struct DevOutput {
    table: String,
    result_json: String,
}

fn trace_startup_output() -> DevOutput {
    let report = bitty_perf::startup::measure_headless_startup();
    let table = report.format_timeline();
    let mut result = String::with_capacity(1024);
    let _ = write!(
        result,
        "\"total_ms\":{:.3},\"budgets\":{{\"p50_ms\":{},\"p99_ms\":{}}},\"verdict\":\"{}\",\"headless_fallback\":{},\"real_window\":{},\"first_frame_presented\":{},\"phases\":[",
        report.total_ms(),
        bitty_perf::PB1_STARTUP_MS_P50,
        bitty_perf::PB1_STARTUP_MS_P99,
        if report.meets_p50() {
            "pass_p50"
        } else if report.meets_p99() {
            "pass_p99"
        } else {
            "above_budget"
        },
        report.headless_fallback,
        report.is_real_window,
        report.first_frame_presented
    );
    for (i, phase) in report.phases.iter().enumerate() {
        if i > 0 {
            result.push(',');
        }
        let (status, detail) = match &phase.status {
            bitty_perf::startup::PhaseStatus::Success => ("ok", String::new()),
            bitty_perf::startup::PhaseStatus::Skipped(reason) => ("skipped", (*reason).to_string()),
            bitty_perf::startup::PhaseStatus::Unavailable(detail) => {
                ("unavailable", detail.clone())
            }
            bitty_perf::startup::PhaseStatus::Failed(detail) => ("failed", detail.clone()),
        };
        let _ = write!(
            result,
            "{{\"name\":\"{}\",\"elapsed_ms\":{:.3},\"since_start_ms\":{:.3},\"status\":\"{}\",\"detail\":\"{}\"}}",
            json_escape(phase.name),
            phase.elapsed.as_secs_f64() * 1000.0,
            phase.since_start.as_secs_f64() * 1000.0,
            status,
            json_escape(&detail)
        );
    }
    result.push(']');
    if let Some(frame) = report.first_frame_stats {
        let _ = write!(
            result,
            ",\"first_frame\":{{\"frame\":{},\"fills\":{},\"glyphs\":{},\"headless\":{},\"generation\":{}}}",
            frame.frame, frame.fills, frame.glyphs, frame.headless, frame.generation
        );
    }
    DevOutput {
        table,
        result_json: result,
    }
}

fn trace_latency_output(iterations: usize) -> DevOutput {
    let report = bitty_perf::latency::measure_latency(iterations);
    let table = report.format_summary();
    let result = format!(
        "\"iterations\":{},\"samples\":{},\"p50_ms\":{:.3},\"p99_ms\":{:.3},\"mean_ms\":{:.3},\"max_ms\":{:.3},\"budgets\":{{\"p50_ms\":{},\"p99_ms\":{}}},\"verdict\":\"{}\",\"headless\":{},\"idle_misses\":{}",
        iterations,
        report.samples.len(),
        report.p50_ms,
        report.p99_ms,
        report.mean_ms,
        report.max_ms,
        bitty_perf::PB4_LATENCY_MS_P50,
        bitty_perf::PB4_LATENCY_MS_P99,
        if report.meets_p50() {
            "pass_p50"
        } else if report.meets_p99() {
            "pass_p99"
        } else {
            "above_budget"
        },
        report.headless,
        report.idle_misses
    );
    DevOutput {
        table,
        result_json: result,
    }
}

fn capture_output(layout: CaptureLayout) -> Result<DevOutput, String> {
    let capture = run_capture(layout)?;
    let table = format!(
        "bitty dev capture — layout={} headless={}\n  frame={} fills={} glyphs={} generation={}\n  surface: extent={} rgba_len={} rgba_hash={}\n  cold-queue: len={} cap={} dropped={}\n",
        layout.name(),
        capture.headless,
        capture.frame,
        capture.fills,
        capture.glyphs,
        capture.generation,
        capture.extent,
        capture.rgba_len,
        capture.rgba_hash,
        capture.cold_len,
        capture.cold_cap,
        capture.cold_dropped
    );
    let result = format!(
        "\"layout\":\"{}\",\"headless\":{},\"frame\":{},\"fills\":{},\"glyphs\":{},\"generation\":{},\"extent\":\"{}\",\"rgba_len\":{},\"rgba_hash\":\"{}\",\"cold_len\":{},\"cold_cap\":{},\"cold_dropped\":{}",
        layout.name(),
        capture.headless,
        capture.frame,
        capture.fills,
        capture.glyphs,
        capture.generation,
        json_escape(&capture.extent),
        capture.rgba_len,
        capture.rgba_hash,
        capture.cold_len,
        capture.cold_cap,
        capture.cold_dropped
    );
    Ok(DevOutput {
        table,
        result_json: result,
    })
}

fn dump_grid_output(rows: usize, cols: usize) -> Result<DevOutput, String> {
    let mut runtime = bitty_runtime::Runtime::with_defaults()
        .map_err(|err| format!("bitty dev: headless runtime failed: {err}"))?;
    runtime.handle_pty_bytes(DEV_SYNTHETIC_CORPUS);
    let _ = runtime.tick();
    let snapshot = bitty_runtime::inspect::grid_text_from_state(runtime.state(), rows, cols);
    let mut table = format!(
        "bitty dev dump grid — rows={} cols={} generation={} cursor={}:{} visible={}\n",
        snapshot.rows,
        snapshot.cols,
        snapshot.generation,
        snapshot.cursor_row,
        snapshot.cursor_col,
        snapshot.cursor_visible
    );
    for line in &snapshot.lines {
        table.push_str(line);
        table.push('\n');
    }
    let mut result = format!(
        "\"rows\":{},\"cols\":{},\"generation\":{},\"cursor_row\":{},\"cursor_col\":{},\"cursor_visible\":{},\"lines\":[",
        snapshot.rows,
        snapshot.cols,
        snapshot.generation,
        snapshot.cursor_row,
        snapshot.cursor_col,
        snapshot.cursor_visible
    );
    for (i, line) in snapshot.lines.iter().enumerate() {
        if i > 0 {
            result.push(',');
        }
        let _ = write!(result, "\"{}\"", json_escape(line));
    }
    result.push(']');
    Ok(DevOutput {
        table,
        result_json: result,
    })
}

/// Formats one damage region for table and JSON output.
fn format_region(region: &bitty_term_state::DamagedRegion) -> String {
    match region {
        bitty_term_state::DamagedRegion::Grid(rect) => format!(
            "grid {}:{}..{}:{}",
            rect.top, rect.left, rect.bottom, rect.right
        ),
        bitty_term_state::DamagedRegion::Scrollback {
            first_line_id,
            count,
        } => {
            format!("scrollback first_line_id={first_line_id} count={count}")
        }
    }
}

fn dump_scene_output() -> Result<DevOutput, String> {
    let mut runtime = bitty_runtime::Runtime::with_defaults()
        .map_err(|err| format!("bitty dev: headless runtime failed: {err}"))?;
    let generation_before = runtime.state().generation();
    runtime.handle_pty_bytes(DEV_SYNTHETIC_CORPUS);
    let stats = runtime
        .tick()
        .ok_or_else(|| "bitty dev: scene tick produced no present (idle)".to_string())?;
    let regions = runtime.state().damage_since(generation_before);
    let allocations = runtime.layout_allocations();
    let container = runtime.container();

    let mut table = format!(
        "bitty dev dump scene — generation_before={generation_before} generation={} frame={} fills={} glyphs={}\n",
        stats.generation, stats.frame, stats.fills, stats.glyphs
    );
    let _ = writeln!(
        table,
        "  container: x={} y={} w={} h={} leafs={}",
        container.x,
        container.y,
        container.width,
        container.height,
        allocations.len()
    );
    for (id, rect) in &allocations {
        let _ = writeln!(
            table,
            "  leaf {id:?}: x={} y={} w={} h={}",
            rect.x, rect.y, rect.width, rect.height
        );
    }
    let _ = writeln!(table, "  damage regions: {}", regions.len());
    for region in &regions {
        let _ = writeln!(table, "    {}", format_region(region));
    }

    let mut result = format!(
        "\"generation_before\":{generation_before},\"generation\":{},\"frame\":{},\"fills\":{},\"glyphs\":{},\"container\":{{\"x\":{},\"y\":{},\"w\":{},\"h\":{}}},\"allocations\":[",
        stats.generation,
        stats.frame,
        stats.fills,
        stats.glyphs,
        container.x,
        container.y,
        container.width,
        container.height
    );
    for (i, (id, rect)) in allocations.iter().enumerate() {
        if i > 0 {
            result.push(',');
        }
        let _ = write!(
            result,
            "{{\"view\":\"{id:?}\",\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
            rect.x, rect.y, rect.width, rect.height
        );
    }
    result.push_str("],\"damage\":[");
    for (i, region) in regions.iter().enumerate() {
        if i > 0 {
            result.push(',');
        }
        let _ = write!(result, "\"{}\"", json_escape(&format_region(region)));
    }
    result.push(']');
    Ok(DevOutput {
        table,
        result_json: result,
    })
}

fn dump_atlas_output() -> Result<DevOutput, String> {
    use bitty_render::{CellMetrics, FontQuery, FontStyle, GridRenderer};
    let mut runtime = bitty_runtime::Runtime::with_defaults()
        .map_err(|err| format!("bitty dev: headless runtime failed: {err}"))?;
    runtime.handle_pty_bytes(DEV_SYNTHETIC_CORPUS);
    let _ = runtime.tick();
    let snapshot = runtime.snapshot();
    let rows = u16::try_from(snapshot.height).unwrap_or(u16::MAX);
    let cols = u16::try_from(snapshot.width).unwrap_or(u16::MAX);
    let damage = bitty_term_state::Damage {
        generation: snapshot.generation,
        regions: vec![bitty_term_state::DamagedRegion::Grid(
            bitty_term_state::DamageRect::full(rows, cols),
        )]
        .into_boxed_slice(),
    };
    let query = FontQuery {
        family: "Bitty Dev Mono".to_string(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    let cell = CellMetrics::new(8, 16)
        .map_err(|err| format!("bitty dev: dev cell metrics invalid: {err}"))?;
    let mut renderer = GridRenderer::new(DevRasterizer::new(), &query, cell)
        .map_err(|err| format!("bitty dev: dev renderer failed: {err}"))?;
    let list = renderer
        .render(&snapshot, &damage)
        .map_err(|err| format!("bitty dev: dev render failed: {err}"))?;
    let dims = renderer.atlas_dims();
    let texels_len = renderer.atlas_texels().len();
    let placements = renderer.atlas_placements();
    let (cache_hits, cache_misses) = renderer.cache_stats();
    let (atlas_hits, atlas_misses, atlas_evictions, atlas_inline) = renderer.atlas_stats();
    let counters = renderer.counters();

    let table = format!(
        "bitty dev dump atlas — rasterizer=dev-deterministic headless\n  draw-list: fills={} glyphs={} generation={}\n  atlas: placements={placements} texels_len={texels_len} dims={}x{}\n  cache: hits={cache_hits} misses={cache_misses}\n  atlas counters: hits={atlas_hits} misses={atlas_misses} evictions={atlas_evictions} inline_fallbacks={atlas_inline} frames_planned={}\n",
        list.fills.len(),
        list.glyphs.len(),
        list.generation,
        dims.width,
        dims.height,
        counters.frames_planned
    );
    let result = format!(
        "\"rasterizer\":\"dev-deterministic\",\"fills\":{},\"glyphs\":{},\"generation\":{},\"placements\":{placements},\"texels_len\":{texels_len},\"dims\":{{\"w\":{},\"h\":{}}},\"cache\":{{\"hits\":{cache_hits},\"misses\":{cache_misses}}},\"atlas\":{{\"hits\":{atlas_hits},\"misses\":{atlas_misses},\"evictions\":{atlas_evictions},\"inline_fallbacks\":{atlas_inline}}},\"frames_planned\":{}",
        list.fills.len(),
        list.glyphs.len(),
        list.generation,
        dims.width,
        dims.height,
        counters.frames_planned
    );
    Ok(DevOutput {
        table,
        result_json: result,
    })
}

fn overlay_list_output() -> DevOutput {
    let mut table = String::from("bitty dev overlay list — renderer overlays\n");
    for overlay in OverlayName::all() {
        let status = match overlay.deferred_reason() {
            None => "available-headless",
            Some(_) => "deferred",
        };
        let _ = writeln!(
            table,
            "  {:<8} {:<20} {}",
            overlay.name(),
            status,
            overlay.description()
        );
    }
    let mut result = String::from("\"overlays\":[");
    for (i, overlay) in OverlayName::all().iter().enumerate() {
        if i > 0 {
            result.push(',');
        }
        match overlay.deferred_reason() {
            None => {
                let _ = write!(
                    result,
                    "{{\"name\":\"{}\",\"status\":\"available-headless\",\"description\":\"{}\"}}",
                    overlay.name(),
                    json_escape(overlay.description())
                );
            }
            Some(reason) => {
                let _ = write!(
                    result,
                    "{{\"name\":\"{}\",\"status\":\"deferred\",\"reason\":\"{}\",\"description\":\"{}\"}}",
                    overlay.name(),
                    json_escape(reason),
                    json_escape(overlay.description())
                );
            }
        }
    }
    result.push(']');
    DevOutput {
        table,
        result_json: result,
    }
}

fn overlay_show_output(name: OverlayName) -> Result<DevOutput, String> {
    match name {
        OverlayName::Damage => {
            let mut runtime = bitty_runtime::Runtime::with_defaults()
                .map_err(|err| format!("bitty dev: headless runtime failed: {err}"))?;
            let generation_before = runtime.state().generation();
            runtime.handle_pty_bytes(DEV_SYNTHETIC_CORPUS);
            let stats = runtime.tick().ok_or_else(|| {
                "bitty dev: overlay damage tick produced no present (idle)".to_string()
            })?;
            let regions = runtime.state().damage_since(generation_before);
            let mut table = format!(
                "bitty dev overlay show damage — regions={} frame={} fills={} glyphs={}\n",
                regions.len(),
                stats.frame,
                stats.fills,
                stats.glyphs
            );
            for region in &regions {
                let _ = writeln!(table, "  {}", format_region(region));
            }
            let mut result = format!(
                "\"name\":\"damage\",\"status\":\"available-headless\",\"frame\":{},\"fills\":{},\"glyphs\":{},\"regions\":[",
                stats.frame, stats.fills, stats.glyphs
            );
            for (i, region) in regions.iter().enumerate() {
                if i > 0 {
                    result.push(',');
                }
                let _ = write!(result, "\"{}\"", json_escape(&format_region(region)));
            }
            result.push(']');
            Ok(DevOutput {
                table,
                result_json: result,
            })
        }
        OverlayName::Banner => {
            // Prove the transient paste-banner paint pattern (CTX-0192): a
            // pending multi-line paste on a fresh headless runtime carries
            // banner text, exactly what `tick` paints as the compact pill.
            let mut runtime = bitty_runtime::Runtime::with_defaults()
                .map_err(|err| format!("bitty dev: headless runtime failed: {err}"))?;
            let pending = runtime.request_paste(DEV_BANNER_PASTE.to_string());
            let banner = runtime.paste_banner_text().ok_or_else(|| {
                "bitty dev: overlay banner proof produced no banner text".to_string()
            })?;
            let collapsed = runtime.paste_banner_collapsed_at(std::time::Instant::now());
            let table = format!(
                "bitty dev overlay show banner — pending={pending} has_banner=true collapsed={collapsed:?}\n  banner: {banner}\n"
            );
            let result = format!(
                "\"name\":\"banner\",\"status\":\"available-headless\",\"pending\":{pending},\"collapsed\":{},\"banner\":\"{}\"",
                collapsed.unwrap_or(false),
                json_escape(&banner)
            );
            Ok(DevOutput {
                table,
                result_json: result,
            })
        }
        deferred => {
            let reason = deferred
                .deferred_reason()
                .unwrap_or("deferred: renderer architecture");
            let table = format!(
                "bitty dev overlay show {} — status=deferred\n  reason: {reason}\n",
                deferred.name()
            );
            let result = format!(
                "\"name\":\"{}\",\"status\":\"deferred\",\"reason\":\"{}\"",
                deferred.name(),
                json_escape(reason)
            );
            Ok(DevOutput {
                table,
                result_json: result,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Runs a validated `bitty dev` request; returns the process exit code.
///
/// - Table goes to stdout for humans; JSON/JSONL emit the versioned envelope
///   (`v: 1`, `command: "dev"`) on stdout with diagnostics on stderr.
/// - Post-parse failures emit ok:false envelopes for json/jsonl (exit 1) and
///   stderr-only diagnostics for table.
pub fn run_dev(request: &DevRequest, options: &DevOptions) -> i32 {
    let outcome: Result<DevOutput, String> = match request {
        DevRequest::TraceStartup => Ok(trace_startup_output()),
        DevRequest::TraceLatency { iterations } => Ok(trace_latency_output(*iterations)),
        DevRequest::Capture { layout } => capture_output(*layout),
        DevRequest::DumpGrid { rows, cols } => dump_grid_output(*rows, *cols),
        DevRequest::DumpScene => dump_scene_output(),
        DevRequest::DumpAtlas => dump_atlas_output(),
        DevRequest::OverlayList => Ok(overlay_list_output()),
        DevRequest::OverlayShow { name } => overlay_show_output(*name),
    };
    match outcome {
        Ok(output) => {
            match options.format {
                DevFormat::Table => print!("{}", output.table),
                DevFormat::Json | DevFormat::Jsonl => println!(
                    "{}",
                    format_success_envelope(request.verb(), &request.detail(), &output.result_json)
                ),
            }
            EXIT_OK
        }
        Err(message) => {
            match options.format {
                DevFormat::Table => eprintln!("{message}"),
                DevFormat::Json | DevFormat::Jsonl => {
                    eprintln!("{message}");
                    println!(
                        "{}",
                        format_error_envelope(
                            request.verb(),
                            &request.detail(),
                            "Internal",
                            &message
                        )
                    );
                }
            }
            EXIT_GENERIC
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (pure parser plus bounded helpers; binary dispatch is covered by
// `crates/bitty-app/tests/cli_dev.rs`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn parse(items: &[&str]) -> Result<(DevRequest, DevOptions), DevParseError> {
        parse_dev_request(&words(items))
    }

    #[test]
    fn help_anywhere_is_help() {
        assert_eq!(parse(&["--help"]), Err(DevParseError::Help));
        assert_eq!(parse(&["trace", "--help"]), Err(DevParseError::Help));
        assert_eq!(parse(&["-h"]), Err(DevParseError::Help));
        assert_eq!(
            parse(&["dump", "grid", "--rows", "4", "--help"]),
            Err(DevParseError::Help)
        );
    }

    #[test]
    fn missing_verb_fails_closed() {
        let err = parse(&[]).unwrap_err();
        match err {
            DevParseError::Usage(message) => assert!(message.contains("missing <verb>")),
            DevParseError::Help => panic!("want usage"),
        }
    }

    #[test]
    fn unknown_verb_names_valid_set() {
        let err = parse(&["frob"]).unwrap_err();
        match err {
            DevParseError::Usage(message) => {
                assert!(message.contains("unknown verb"));
                assert!(message.contains("trace|capture|dump|overlay"));
            }
            DevParseError::Help => panic!("want usage"),
        }
    }

    #[test]
    fn trace_verbs_parse() {
        let (request, options) = parse(&["trace", "startup"]).unwrap();
        assert_eq!(request, DevRequest::TraceStartup);
        assert_eq!(options.format, DevFormat::Table);
        let (request, _) = parse(&["trace", "latency"]).unwrap();
        assert_eq!(
            request,
            DevRequest::TraceLatency {
                iterations: DEFAULT_LATENCY_ITERATIONS
            }
        );
        let (request, options) = parse(&["trace", "latency", "--iterations", "7"]).unwrap();
        assert_eq!(request, DevRequest::TraceLatency { iterations: 7 });
        assert_eq!(options.format, DevFormat::Table);
        let (request, _) = parse(&["trace", "latency", "--iterations=50"]).unwrap();
        assert_eq!(request, DevRequest::TraceLatency { iterations: 50 });
    }

    #[test]
    fn trace_missing_or_unknown_subverb_fails_closed() {
        let err = parse(&["trace"]).unwrap_err();
        match err {
            DevParseError::Usage(message) => assert!(message.contains("missing <subverb>")),
            DevParseError::Help => panic!("want usage"),
        }
        let err = parse(&["trace", "nope"]).unwrap_err();
        match err {
            DevParseError::Usage(message) => assert!(message.contains("unknown subverb")),
            DevParseError::Help => panic!("want usage"),
        }
        assert!(parse(&["trace", "startup", "extra"]).is_err());
    }

    #[test]
    fn iterations_bounds_are_enforced() {
        assert!(parse(&["trace", "latency", "--iterations", "0"]).is_err());
        assert!(parse(&["trace", "latency", "--iterations", "1001"]).is_err());
        assert!(parse(&["trace", "latency", "--iterations", "abc"]).is_err());
        assert!(parse(&["trace", "latency", "--iterations"]).is_err());
        assert!(parse(&["trace", "latency", "--iterations", "1000"]).is_ok());
        // Iterations belong to latency only.
        assert!(parse(&["trace", "startup", "--iterations", "5"]).is_err());
        assert!(parse(&["capture", "--iterations", "5"]).is_err());
        assert!(parse(&["dump", "scene", "--iterations", "5"]).is_err());
    }

    #[test]
    fn capture_layouts_parse_with_default_single() {
        let (request, _) = parse(&["capture"]).unwrap();
        assert_eq!(
            request,
            DevRequest::Capture {
                layout: CaptureLayout::Single
            }
        );
        for (word, layout) in [
            ("single", CaptureLayout::Single),
            ("split", CaptureLayout::Split),
            ("stack", CaptureLayout::Stack),
            ("overlay", CaptureLayout::Overlay),
            ("SPLIT", CaptureLayout::Split),
        ] {
            let (request, _) = parse(&["capture", "--layout", word]).unwrap();
            assert_eq!(request, DevRequest::Capture { layout });
        }
        assert!(parse(&["capture", "--layout", "diagonal"]).is_err());
        assert!(parse(&["capture", "grid"]).is_err());
        assert!(parse(&["capture", "--rows", "4"]).is_err());
    }

    #[test]
    fn dump_verbs_parse_with_grid_bounds() {
        let (request, _) = parse(&["dump", "grid"]).unwrap();
        assert_eq!(
            request,
            DevRequest::DumpGrid {
                rows: DEFAULT_DUMP_ROWS,
                cols: DEFAULT_DUMP_COLS
            }
        );
        let (request, _) = parse(&["dump", "grid", "--rows", "4", "--cols", "10"]).unwrap();
        assert_eq!(request, DevRequest::DumpGrid { rows: 4, cols: 10 });
        let (request, _) = parse(&["dump", "scene"]).unwrap();
        assert_eq!(request, DevRequest::DumpScene);
        let (request, _) = parse(&["dump", "atlas"]).unwrap();
        assert_eq!(request, DevRequest::DumpAtlas);
        assert!(parse(&["dump"]).is_err());
        assert!(parse(&["dump", "nope"]).is_err());
        assert!(parse(&["dump", "grid", "--rows", "0"]).is_err());
        assert!(parse(&["dump", "grid", "--cols", "257"]).is_err());
        assert!(parse(&["dump", "scene", "--rows", "4"]).is_err());
        assert!(parse(&["dump", "atlas", "--cols", "4"]).is_err());
        assert!(parse(&["dump", "grid", "--layout", "split"]).is_err());
    }

    #[test]
    fn overlay_verbs_parse() {
        let (request, _) = parse(&["overlay", "list"]).unwrap();
        assert_eq!(request, DevRequest::OverlayList);
        for name in ["damage", "cells", "glyphs", "images", "layout", "banner"] {
            let (request, _) = parse(&["overlay", "show", name]).unwrap();
            assert_eq!(
                request,
                DevRequest::OverlayShow {
                    name: OverlayName::parse(name).unwrap()
                }
            );
        }
        assert!(parse(&["overlay"]).is_err());
        assert!(parse(&["overlay", "show"]).is_err());
        assert!(parse(&["overlay", "show", "nope"]).is_err());
        assert!(parse(&["overlay", "list", "damage"]).is_err());
    }

    #[test]
    fn format_shapes_parse_and_reject() {
        let (_, options) = parse(&["trace", "startup", "--format", "json"]).unwrap();
        assert_eq!(options.format, DevFormat::Json);
        let (_, options) = parse(&["--format=jsonl", "capture"]).unwrap();
        assert_eq!(options.format, DevFormat::Jsonl);
        assert!(parse(&["capture", "--format", "yaml"]).is_err());
        assert!(parse(&["capture", "--format"]).is_err());
    }

    #[test]
    fn remote_target_flags_are_rejected() {
        for args in [
            vec!["capture", "--socket", "/tmp/a.sock"],
            vec!["capture", "--socket=/tmp/a.sock"],
            vec!["--socket", "/tmp/a.sock", "capture"],
            vec!["dump", "grid", "--instance", "i:1"],
            vec!["dump", "grid", "--instance=i:1"],
            vec!["trace", "startup", "--instance", "x"],
        ] {
            let err = parse(&args).unwrap_err();
            match err {
                DevParseError::Usage(message) => assert!(
                    message.contains("local-only"),
                    "want local-only diagnostic, got {message:?}"
                ),
                DevParseError::Help => panic!("want usage"),
            }
        }
    }

    #[test]
    fn stray_separator_and_unknown_flags_fail_closed() {
        assert!(parse(&["capture", "--"]).is_err());
        assert!(parse(&["--", "capture"]).is_err());
        assert!(parse(&["capture", "--frobnicate"]).is_err());
        assert!(parse(&["capture", "-x"]).is_err());
    }

    #[test]
    fn extra_positionals_fail_closed() {
        assert!(parse(&["capture", "a", "b", "c", "d"]).is_err());
        assert!(parse(&["overlay", "show", "damage", "extra"]).is_err());
    }

    #[test]
    fn token_shapes_are_bounded() {
        let long = "a".repeat(MAX_DEV_TOKEN_LEN + 1);
        assert!(parse(&[&long]).is_err());
        assert!(parse(&["trace;startup"]).is_err());
        assert!(parse(&["trace\x00startup"]).is_err());
    }

    #[test]
    fn json_escape_covers_controls() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("a\nb\rc\td"), "a\\nb\\rc\\td");
        assert_eq!(json_escape("\u{0}\u{1f}"), "\\u0000\\u001f");
    }

    #[test]
    fn fnv1a_is_deterministic_and_sensitive() {
        assert_eq!(fnv1a_hex(b"abc"), fnv1a_hex(b"abc"));
        assert_ne!(fnv1a_hex(b"abc"), fnv1a_hex(b"abd"));
        assert_eq!(fnv1a_hex(b"").len(), 16);
    }

    #[test]
    fn dev_format_parses_shapes() {
        assert_eq!(DevFormat::parse(None).unwrap(), DevFormat::Table);
        assert_eq!(DevFormat::parse(Some("json")).unwrap(), DevFormat::Json);
        assert!(DevFormat::parse(Some("yaml")).is_err());
    }

    #[test]
    fn overlay_catalog_marks_gpu_entries_deferred() {
        assert!(OverlayName::Damage.deferred_reason().is_none());
        assert!(OverlayName::Banner.deferred_reason().is_none());
        for deferred in [
            OverlayName::Cells,
            OverlayName::Glyphs,
            OverlayName::Images,
            OverlayName::Layout,
        ] {
            assert!(
                deferred.deferred_reason().is_some(),
                "{} must stay deferred",
                deferred.name()
            );
        }
        assert_eq!(OverlayName::all().len(), 6);
    }

    #[test]
    fn capture_layouts_build() {
        for layout in [
            CaptureLayout::Single,
            CaptureLayout::Split,
            CaptureLayout::Stack,
            CaptureLayout::Overlay,
        ] {
            let node = build_capture_layout(layout);
            assert!(!node.leaf_ids().is_empty(), "{layout:?} needs leaves");
        }
    }

    #[test]
    fn usage_and_help_name_verbs() {
        let usage = dev_usage();
        for token in ["trace", "capture", "dump", "overlay"] {
            assert!(usage.contains(token), "usage must name {token}");
        }
        let help = dev_help_text();
        for token in [
            "trace startup",
            "trace latency",
            "dump grid",
            "overlay list",
        ] {
            assert!(help.contains(token), "help must name {token}");
        }
    }
}
