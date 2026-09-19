//! Parser-throughput baseline (CTX-0576, M1-11).
//!
//! Deterministic, headless, bounded measurement of
//! [`bitty_vt::Parser::advance`] over representative VT/escape corpora. It
//! isolates the parser from terminal state (`State::apply`) and rendering so a
//! regression can be triaged to the parser stage, and it is the machine-checked
//! half of the compatibility-milestone "Performance guardrail": a committed
//! baseline number plus a generous regression threshold against plain-text
//! throughput.
//!
//! Budget reference: `docs/specifications/performance-budget-rfc.md#pb-6-throughput-floor`
//! (PB-6, ≥ 40 MB/s sustained parse-and-render on a Tier 1 reference machine).
//! The committed baseline is evidence, not a new budget: PB-6 remains an
//! architecture constraint until the RFC's reference hardware open item
//! closes.
//!
//! Evidence record: `crates/bitty-perf/baselines/parser-throughput.json`
//! (command, revision, machine class, per-corpus MB/s) and
//! `crates/bitty-perf/baselines/README.md` (repo-owned runbook).
//!
//! All corpora are bounded and deterministic: files are discovered by
//! `CARGO_MANIFEST_DIR`-anchored lexical order, each segment is capped at
//! [`MAX_SEGMENT_BYTES`], each `advance` call receives at most
//! [`MAX_CHUNK_BYTES`], the witness materializes at most [`MAX_ACTIONS`], and
//! there is no clock-dependent assertion beyond the documented regression
//! thresholds.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

use bitty_vt::Parser;

/// Schema version of the committed baseline artifact.
pub const PARSER_BASELINE_SCHEMA_VERSION: u32 = 1;

/// Repository-relative path of the committed baseline evidence artifact.
pub const PARSER_BASELINE_REL_PATH: &str = "crates/bitty-perf/baselines/parser-throughput.json";

/// Chunk size fed to `Parser::advance`, matching `bitty-pty::READ_CHUNK_SIZE`.
pub const MAX_CHUNK_BYTES: usize = 8 * 1024;

/// Maximum bytes of any single corpus segment (repeated to the sample size).
pub const MAX_SEGMENT_BYTES: usize = 64 * 1024;

/// Maximum actions materialized into a bounded `Vec` by the chunking-identity
/// witness. The throughput counter itself counts every decoded action without
/// materializing them (a `u64` counter accumulates nothing), so action totals
/// in the baseline are not truncated by this bound.
pub const MAX_ACTIONS: usize = 4096;

/// Bytes parsed per corpus in the reproducible release measurement.
pub const RELEASE_SAMPLE_BYTES: usize = 4 * 1024 * 1024;

/// Measured rounds per corpus in the release measurement (median reported).
pub const RELEASE_ROUNDS: usize = 5;

/// Bytes parsed per corpus in the bounded CI regression check.
pub const CI_SAMPLE_BYTES: usize = 512 * 1024;

/// Measured rounds per corpus in the bounded CI regression check.
pub const CI_ROUNDS: usize = 3;

/// Maximum corpora concatenated from `tests/compat/*/corpus/`.
pub const MAX_COMPAT_CORPORA: usize = 96;

/// Generous regression factor: a measured mixed/plain throughput ratio below
/// `baseline_ratio / REGRESSION_FACTOR` is a failure. It catches pathological
/// regressions (escape handling an order of magnitude slower) without flaking
/// on shared runners or debug-profile builds.
pub const REGRESSION_FACTOR: f64 = 4.0;

/// Absolute MB/s floor applied only to optimized builds; generously below the
/// committed baseline so a slower host class still passes.
pub const ABSOLUTE_FLOOR_MB_S: f64 = 10.0;

/// Identifier of the plain-text reference corpus.
pub const PLAIN_TEXT_ID: &str = "plain_text";

/// A named corpus measured by the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorpusSpec {
    /// Stable machine identifier used in the baseline artifact.
    pub id: &'static str,
    /// Human description of what the segment exercises.
    pub description: &'static str,
}

/// The ordered corpus roster. The order is part of the artifact contract.
pub const CORPUS_SPECS: &[CorpusSpec] = &[
    CorpusSpec {
        id: PLAIN_TEXT_ID,
        description: "plain ASCII reference stream (bitty-vt seed 01), low escape density",
    },
    CorpusSpec {
        id: "mixed_seeds",
        description: "all bitty-vt seed families (SGR, cursor, DECSET, OSC, DCS, malformed)",
    },
    CorpusSpec {
        id: "compat_corpus",
        description: "tests/compat/*/corpus M1 surface corpora (modes, color, osc, mouse, tui)",
    },
    CorpusSpec {
        id: "escape_storm",
        description: "synthetic heavy escape density (SGR/DECSET/OSC/DCS/cursor) worst case",
    },
];

/// One measured corpus result.
#[derive(Debug, Clone)]
pub struct CorpusMeasurement {
    /// Stable corpus identifier.
    pub id: &'static str,
    /// Bytes parsed per measured round.
    pub bytes: usize,
    /// Decoded actions counted in the final round (exact; not truncated).
    pub actions: u64,
    /// Median throughput across measured rounds, in MiB/s.
    pub mb_s: f64,
}

/// Full measurement report.
#[derive(Debug, Clone)]
pub struct ParserThroughputReport {
    /// Per-corpus measurements, in [`CORPUS_SPECS`] order.
    pub corpora: Vec<CorpusMeasurement>,
    /// Bytes parsed per corpus.
    pub sample_bytes: usize,
    /// Measured rounds per corpus.
    pub rounds: usize,
    /// Wall-clock seconds spent in measured rounds across every corpus.
    pub elapsed_secs: f64,
}

impl ParserThroughputReport {
    /// Throughput for a corpus id, when measured.
    #[must_use]
    pub fn mb_s(&self, id: &str) -> Option<f64> {
        self.corpora.iter().find(|c| c.id == id).map(|c| c.mb_s)
    }

    /// Throughput ratio of `id` against the plain-text reference corpus.
    /// Returns `None` when either corpus is missing or the reference is zero.
    #[must_use]
    pub fn ratio_vs_plain(&self, id: &str) -> Option<f64> {
        let plain = self.mb_s(PLAIN_TEXT_ID)?;
        let value = self.mb_s(id)?;
        (plain > 0.0).then_some(value / plain)
    }

    /// Every non-plain corpus ratio against plain text, in roster order.
    #[must_use]
    pub fn escape_ratios(&self) -> Vec<(&'static str, f64)> {
        let mut out = Vec::new();
        for spec in CORPUS_SPECS {
            if spec.id == PLAIN_TEXT_ID {
                continue;
            }
            if let Some(ratio) = self.ratio_vs_plain(spec.id) {
                out.push((spec.id, ratio));
            }
        }
        out
    }
}

/// A committed baseline corpus value parsed from the artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct BaselineCorpus {
    /// Corpus identifier.
    pub id: String,
    /// Committed median throughput (MiB/s).
    pub mb_s: f64,
}

/// Parsed committed baseline.
#[derive(Debug, Clone)]
pub struct ParserBaseline {
    /// Committed per-corpus values.
    pub corpora: Vec<BaselineCorpus>,
    /// Regression factor carried by the artifact.
    pub regression_factor: f64,
    /// Absolute optimized-build floor carried by the artifact.
    pub absolute_floor_mb_s: f64,
}

impl ParserBaseline {
    /// Committed throughput for a corpus id.
    #[must_use]
    pub fn mb_s(&self, id: &str) -> Option<f64> {
        self.corpora.iter().find(|c| c.id == id).map(|c| c.mb_s)
    }

    /// Committed ratio of `id` against the plain-text reference corpus.
    #[must_use]
    pub fn ratio_vs_plain(&self, id: &str) -> Option<f64> {
        let plain = self.mb_s(PLAIN_TEXT_ID)?;
        let value = self.mb_s(id)?;
        (plain > 0.0).then_some(value / plain)
    }
}

/// One regression check outcome.
#[derive(Debug, Clone)]
pub struct RegressionCheck {
    /// Check name (stable, machine-readable).
    pub name: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Human detail including the measured and committed values.
    pub detail: String,
}

/// Result of comparing a fresh measurement against the committed baseline.
#[derive(Debug, Clone)]
pub struct RegressionReport {
    /// Individual check outcomes.
    pub checks: Vec<RegressionCheck>,
}

impl RegressionReport {
    /// Whether every check passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// Render a multi-line summary for benches and CI logs.
    #[must_use]
    pub fn format_summary(&self) -> String {
        let mut out = String::new();
        for check in &self.checks {
            let verdict = if check.passed { "PASS" } else { "FAIL" };
            out.push_str(&format!("  {verdict} {} — {}\n", check.name, check.detail));
        }
        out
    }
}

/// Repository root: the crate's grandparent directory, derived from
/// `CARGO_MANIFEST_DIR` so worktrees, CI checkouts, and clones all work.
#[must_use]
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate directory must be <workspace>/crates/bitty-perf")
        .to_path_buf()
}

/// The committed baseline artifact, embedded at compile time.
#[must_use]
pub const fn committed_baseline_json() -> &'static str {
    include_str!("../baselines/parser-throughput.json")
}

/// Load the ordered, bounded corpus segments from the repository tree.
///
/// Every segment is derived from committed corpora: `bitty-vt` seeds and
/// `tests/compat/*/corpus/*.bin`. Discovery is lexical and deterministic; no
/// network, RNG, clock, display, or host path participates.
pub fn load_segments() -> Result<Vec<(CorpusSpec, Vec<u8>)>, String> {
    let root = workspace_root();
    let plain = read_one(&root.join("crates/bitty-vt/seeds/01-plain-text.bin"))?;
    let seeds = read_sorted_dir(&root.join("crates/bitty-vt/seeds"), usize::MAX)?;
    let compat = read_compat_corpora(&root.join("tests/compat"))?;

    let mut mixed = Vec::new();
    for (_, bytes) in &seeds {
        mixed.extend_from_slice(bytes);
    }
    let mut compat_bytes = Vec::new();
    for (_, bytes) in &compat {
        compat_bytes.extend_from_slice(bytes);
    }

    let segments = vec![
        (CORPUS_SPECS[0], repeat_to(&plain, MAX_SEGMENT_BYTES)),
        (CORPUS_SPECS[1], repeat_to(&mixed, MAX_SEGMENT_BYTES)),
        (CORPUS_SPECS[2], repeat_to(&compat_bytes, MAX_SEGMENT_BYTES)),
        (CORPUS_SPECS[3], escape_storm(MAX_SEGMENT_BYTES)),
    ];
    Ok(segments)
}

/// Run the measurement with explicit bounds. `sample_bytes` is parsed per
/// corpus per round; `rounds` measured rounds are reduced to a median.
pub fn measure(sample_bytes: usize, rounds: usize) -> Result<ParserThroughputReport, String> {
    if rounds == 0 {
        return Err("rounds must be > 0".to_string());
    }
    if sample_bytes < MAX_CHUNK_BYTES {
        return Err(format!("sample_bytes must be >= {MAX_CHUNK_BYTES}"));
    }
    let segments = load_segments()?;
    let mut corpora = Vec::with_capacity(segments.len());
    let mut elapsed_secs = 0.0;
    for (spec, segment) in segments {
        let (measurement, secs) = measure_segment(spec, &segment, sample_bytes, rounds);
        elapsed_secs += secs;
        corpora.push(measurement);
    }
    Ok(ParserThroughputReport {
        corpora,
        sample_bytes,
        rounds,
        elapsed_secs,
    })
}

/// The bounded regression-check measurement used by CI (`cargo test`).
pub fn ci_report() -> Result<ParserThroughputReport, String> {
    measure(CI_SAMPLE_BYTES, CI_ROUNDS)
}

/// Parse the committed baseline artifact with a minimal, dependency-free
/// parser. The artifact carries flat corpus objects, so a brace split is
/// sufficient; the roster and positivity are validated by
/// [`committed_baseline`].
pub fn parse_baseline(json: &str) -> Result<ParserBaseline, String> {
    let mut corpora = Vec::new();
    for (index, chunk) in json.split('{').enumerate() {
        let Some(id) = json_string_after(chunk, "id") else {
            continue;
        };
        let Some(mb_s) = json_number_after(chunk, "mb_s") else {
            return Err(format!("corpus {id} (block {index}) missing mb_s"));
        };
        if !(mb_s.is_finite() && mb_s > 0.0) {
            return Err(format!("corpus {id} has non-positive mb_s {mb_s}"));
        }
        corpora.push(BaselineCorpus { id, mb_s });
    }
    if corpora.is_empty() {
        return Err("baseline has no corpora".to_string());
    }
    let regression_factor = json_number_after(json, "regression_factor")
        .ok_or_else(|| "baseline missing regression_factor".to_string())?;
    let absolute_floor_mb_s = json_number_after(json, "absolute_floor_mb_s")
        .ok_or_else(|| "baseline missing absolute_floor_mb_s".to_string())?;
    Ok(ParserBaseline {
        corpora,
        regression_factor,
        absolute_floor_mb_s,
    })
}

/// Parse and validate the embedded committed baseline.
pub fn committed_baseline() -> Result<ParserBaseline, String> {
    let baseline = parse_baseline(committed_baseline_json())?;
    let ids: Vec<&str> = baseline.corpora.iter().map(|c| c.id.as_str()).collect();
    let expected: Vec<&str> = CORPUS_SPECS.iter().map(|s| s.id).collect();
    if ids != expected {
        return Err(format!(
            "baseline corpus roster mismatch: {ids:?} != {expected:?}"
        ));
    }
    Ok(baseline)
}

/// Compare a fresh measurement against the committed baseline.
///
/// Two guards run:
/// - a per-escape-corpus ratio gate (`>= committed_ratio / factor`), the
///   milestone's "pathological regression versus plain text" guardrail;
/// - an absolute optimized-build floor that is skipped under debug builds.
pub fn regression_check(
    report: &ParserThroughputReport,
    baseline: &ParserBaseline,
) -> RegressionReport {
    let mut checks = Vec::new();
    for (id, measured) in report.escape_ratios() {
        let committed = baseline.ratio_vs_plain(id).unwrap_or(0.0);
        let floor = committed / baseline.regression_factor;
        let passed = measured >= floor;
        checks.push(RegressionCheck {
            name: format!("ratio {id}/plain_text"),
            passed,
            detail: format!(
                "measured {measured:.4} vs committed {committed:.4} (gate >= {floor:.4}, factor {:.1})",
                baseline.regression_factor
            ),
        });
    }

    // The reference corpus itself must not collapse to zero throughput.
    if let Some(plain) = report.mb_s(PLAIN_TEXT_ID) {
        let passed = plain.is_finite() && plain > 0.0;
        checks.push(RegressionCheck {
            name: format!("throughput {PLAIN_TEXT_ID}"),
            passed,
            detail: format!("{plain:.2} MiB/s (must be a positive finite number)"),
        });
    }

    if !cfg!(debug_assertions) {
        let measured = report.mb_s(CORPUS_SPECS[1].id).unwrap_or(0.0);
        let passed = measured >= baseline.absolute_floor_mb_s;
        checks.push(RegressionCheck {
            name: "absolute floor (optimized build)".to_string(),
            passed,
            detail: format!(
                "measured {measured:.2} MiB/s vs floor {:.2} MiB/s",
                baseline.absolute_floor_mb_s
            ),
        });
    }

    RegressionReport { checks }
}

/// Render a stable, human-readable report for the bench.
#[must_use]
pub fn format_report(report: &ParserThroughputReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "parser_throughput — PB-6 floor {} MiB/s (single core, {} B/corpus, {} rounds, headless)\n",
        crate::PB6_THROUGHPUT_MB_S,
        report.sample_bytes,
        report.rounds
    ));
    out.push_str(&format!(
        "bound: MAX_CHUNK_BYTES={MAX_CHUNK_BYTES}, MAX_SEGMENT_BYTES={MAX_SEGMENT_BYTES}, witness_MAX_ACTIONS={MAX_ACTIONS}\n"
    ));
    for measurement in &report.corpora {
        let ratio = report
            .ratio_vs_plain(measurement.id)
            .map(|r| format!("{r:.4}"))
            .unwrap_or_else(|| "n/a".to_string());
        out.push_str(&format!(
            "  {:<14} bytes={} actions={} median={:>8.2} MiB/s ratio_vs_plain={ratio}\n",
            measurement.id, measurement.bytes, measurement.actions, measurement.mb_s
        ));
    }
    out.push_str(&format!("  measured_elapsed={:.3}s\n", report.elapsed_secs));
    out
}

/// Serialize a report plus provenance into the committed baseline JSON shape.
///
/// `meta` values are supplied by the caller from the environment so no host
/// path, username, or machine identifier is embedded by this crate.
#[must_use]
pub fn baseline_json(report: &ParserThroughputReport, meta: &BaselineMeta) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"schema_version\": {PARSER_BASELINE_SCHEMA_VERSION},\n"
    ));
    out.push_str(&format!("  \"task\": \"{}\",\n", escape(&meta.task)));
    out.push_str(&format!("  \"issue\": {},\n", meta.issue));
    out.push_str(&format!(
        "  \"milestone_item\": \"{}\",\n",
        escape(&meta.milestone_item)
    ));
    out.push_str(&format!(
        "  \"captured_at\": \"{}\",\n",
        escape(&meta.captured_at)
    ));
    out.push_str(&format!(
        "  \"revision\": \"{}\",\n",
        escape(&meta.revision)
    ));
    out.push_str(&format!("  \"command\": \"{}\",\n", escape(&meta.command)));
    out.push_str(&format!("  \"profile\": \"{}\",\n", escape(&meta.profile)));
    out.push_str(&format!(
        "  \"toolchain\": \"{}\",\n",
        escape(&meta.toolchain)
    ));
    out.push_str(&format!("  \"os\": \"{}\",\n", escape(&meta.os)));
    out.push_str(&format!(
        "  \"machine_class\": \"{}\",\n",
        escape(&meta.machine_class)
    ));
    out.push_str(&format!(
        "  \"budget_ref\": \"{}\",\n",
        escape(&meta.budget_ref)
    ));
    out.push_str("  \"bounds\": {\n");
    out.push_str(&format!("    \"MAX_CHUNK_BYTES\": {MAX_CHUNK_BYTES},\n"));
    out.push_str(&format!(
        "    \"MAX_SEGMENT_BYTES\": {MAX_SEGMENT_BYTES},\n"
    ));
    out.push_str(&format!("    \"MAX_ACTIONS\": {MAX_ACTIONS},\n"));
    out.push_str(&format!("    \"sample_bytes\": {},\n", report.sample_bytes));
    out.push_str(&format!("    \"rounds\": {}\n", report.rounds));
    out.push_str("  },\n");
    out.push_str(&format!("  \"regression_factor\": {REGRESSION_FACTOR},\n"));
    out.push_str(&format!(
        "  \"absolute_floor_mb_s\": {ABSOLUTE_FLOOR_MB_S},\n"
    ));
    out.push_str("  \"corpora\": [\n");
    for (index, measurement) in report.corpora.iter().enumerate() {
        let description = CORPUS_SPECS
            .iter()
            .find(|s| s.id == measurement.id)
            .map_or("", |s| s.description);
        let ratio = report.ratio_vs_plain(measurement.id).unwrap_or(1.0);
        out.push_str("    {\n");
        out.push_str(&format!("      \"id\": \"{}\",\n", measurement.id));
        out.push_str(&format!(
            "      \"description\": \"{}\",\n",
            escape(description)
        ));
        out.push_str(&format!("      \"bytes\": {},\n", measurement.bytes));
        out.push_str(&format!("      \"actions\": {},\n", measurement.actions));
        out.push_str(&format!("      \"mb_s\": {:.2},\n", measurement.mb_s));
        out.push_str(&format!("      \"ratio_vs_plain\": {ratio:.4}\n"));
        if index + 1 < report.corpora.len() {
            out.push_str("    },\n");
        } else {
            out.push_str("    }\n");
        }
    }
    out.push_str("  ]\n");
    out.push_str("}\n");
    out
}

/// Provenance metadata supplied to [`baseline_json`].
#[derive(Debug, Clone, Default)]
pub struct BaselineMeta {
    /// Owning CarryCtx task.
    pub task: String,
    /// Owning GitHub issue number.
    pub issue: u64,
    /// M1 backlog item identifier.
    pub milestone_item: String,
    /// Capture date (`YYYY-MM-DD`).
    pub captured_at: String,
    /// Git revision the baseline was captured at.
    pub revision: String,
    /// Exact measurement command.
    pub command: String,
    /// Cargo profile used.
    pub profile: String,
    /// Rust toolchain.
    pub toolchain: String,
    /// Operating system description.
    pub os: String,
    /// Machine class description.
    pub machine_class: String,
    /// Budget reference anchor.
    pub budget_ref: String,
}

fn measure_segment(
    spec: CorpusSpec,
    segment: &[u8],
    sample_bytes: usize,
    rounds: usize,
) -> (CorpusMeasurement, f64) {
    assert!(
        segment.len() <= MAX_SEGMENT_BYTES,
        "segment exceeds MAX_SEGMENT_BYTES"
    );
    assert_single_chunk_identity(segment);

    let buffer = repeat_to(segment, sample_bytes);
    // Warmup: caches and allocator settle before measurement.
    let _ = black_box(parse_stream(black_box(&buffer)));

    let mut samples = Vec::with_capacity(rounds);
    let mut actions = 0u64;
    let mut elapsed_secs = 0.0;
    for _ in 0..rounds {
        let start = Instant::now();
        actions = parse_stream(black_box(&buffer));
        let secs = start.elapsed().as_secs_f64().max(1e-9);
        elapsed_secs += secs;
        samples.push(buffer.len() as f64 / (1024.0 * 1024.0) / secs);
    }
    samples.sort_by(f64::total_cmp);
    let median = samples[samples.len() / 2];
    (
        CorpusMeasurement {
            id: spec.id,
            bytes: buffer.len(),
            actions,
            mb_s: median,
        },
        elapsed_secs,
    )
}

/// Parse a whole buffer in PTY-sized chunks through one parser, counting every
/// decoded action (no materialization, so the count is exact).
fn parse_stream(buffer: &[u8]) -> u64 {
    let mut parser = Parser::new();
    let mut actions = 0u64;
    for chunk in buffer.chunks(MAX_CHUNK_BYTES) {
        parser.advance(black_box(chunk), |_| actions += 1);
    }
    actions
}

/// Determinism witness: the bounded prefix parsed in one call and the same
/// prefix parsed byte-by-byte through a single parser must decode identically
/// (chunking invariance).
fn assert_single_chunk_identity(segment: &[u8]) {
    let prefix = &segment[..segment.len().min(MAX_CHUNK_BYTES)];
    let mut whole = Vec::new();
    Parser::new().advance(prefix, |a| {
        if whole.len() < MAX_ACTIONS {
            whole.push(a);
        }
    });
    let mut per_byte = Vec::new();
    let mut parser = Parser::new();
    for byte in prefix.iter().copied() {
        parser.advance(&[byte], |a| {
            if per_byte.len() < MAX_ACTIONS {
                per_byte.push(a);
            }
        });
    }
    assert_eq!(
        whole, per_byte,
        "chunking invariance violated on a corpus prefix"
    );
    assert!(whole.len() <= MAX_ACTIONS && per_byte.len() <= MAX_ACTIONS);
}

fn escape_storm(len: usize) -> Vec<u8> {
    const CHUNK: &[u8] = b"\x1b[31;1mHello \x1b[0m\x1b[2J\x1b[H\x1b[?25h world \x1b[38;2;255;128;0m!\n\x1b]0;Bitty\x07\x1bP0;fake|DCS\x1b\\";
    repeat_to(CHUNK, len)
}

fn repeat_to(source: &[u8], len: usize) -> Vec<u8> {
    assert!(!source.is_empty(), "source corpus must not be empty");
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let take = source.len().min(len - out.len());
        out.extend_from_slice(&source[..take]);
    }
    out
}

fn read_one(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// Read every `*.bin` under `dir` in lexical order, capped at `max`.
fn read_sorted_dir(dir: &Path, max: usize) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    let mut paths = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("bin") {
            paths.push(path);
        }
    }
    paths.sort();
    paths.truncate(max);
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        out.push((path.clone(), read_one(&path)?));
    }
    Ok(out)
}

/// Concatenate every `tests/compat/<category>/corpus/*.bin` in lexical order,
/// bounded to [`MAX_COMPAT_CORPORA`] files and [`MAX_CHUNK_BYTES`] per file.
fn read_compat_corpora(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    let mut categories = Vec::new();
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("read_dir {}: {e}", root.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            categories.push(path);
        }
    }
    categories.sort();

    let mut out = Vec::new();
    for category in categories {
        let corpus = category.join("corpus");
        if !corpus.is_dir() {
            continue;
        }
        let files = read_sorted_dir(&corpus, MAX_COMPAT_CORPORA.saturating_sub(out.len()))?;
        for (path, bytes) in files {
            if bytes.len() > MAX_CHUNK_BYTES {
                return Err(format!(
                    "{} exceeds MAX_CHUNK_BYTES ({})",
                    path.display(),
                    bytes.len()
                ));
            }
            out.push((path, bytes));
            if out.len() >= MAX_COMPAT_CORPORA {
                return Ok(out);
            }
        }
    }
    if out.is_empty() {
        return Err(format!("no compat corpora under {}", root.display()));
    }
    Ok(out)
}

fn find_key<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let pos = json.find(&needle)?;
    let rest = &json[pos + needle.len()..];
    let colon = rest.find(':')?;
    Some(&rest[colon + 1..])
}

fn json_string_after(json: &str, key: &str) -> Option<String> {
    let after = find_key(json, key)?.trim_start();
    let mut chars = after.chars();
    if chars.next()? != '"' {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            out.push(match ch {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

fn json_number_after(json: &str, key: &str) -> Option<f64> {
    let after = find_key(json, key)?.trim_start();
    let end = after
        .find(|c: char| {
            !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
        })
        .unwrap_or(after.len());
    if end == 0 {
        return None;
    }
    after[..end].parse::<f64>().ok()
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_baseline_json() -> String {
        baseline_json(
            &ParserThroughputReport {
                corpora: vec![
                    CorpusMeasurement {
                        id: PLAIN_TEXT_ID,
                        bytes: 1024,
                        actions: 10,
                        mb_s: 100.0,
                    },
                    CorpusMeasurement {
                        id: CORPUS_SPECS[1].id,
                        bytes: 1024,
                        actions: 10,
                        mb_s: 25.0,
                    },
                ],
                sample_bytes: 1024,
                rounds: 1,
                elapsed_secs: 0.01,
            },
            &BaselineMeta {
                task: "CTX-0000".to_string(),
                ..BaselineMeta::default()
            },
        )
    }

    #[test]
    fn baseline_round_trips_through_the_minimal_parser() {
        let json = sample_baseline_json();
        let parsed = parse_baseline(&json).expect("baseline must parse");
        assert_eq!(parsed.corpora.len(), 2);
        assert_eq!(parsed.corpora[0].id, PLAIN_TEXT_ID);
        assert!((parsed.corpora[0].mb_s - 100.0).abs() < 1e-9);
        assert!((parsed.corpora[1].mb_s - 25.0).abs() < 1e-9);
        assert!((parsed.regression_factor - REGRESSION_FACTOR).abs() < 1e-9);
    }

    #[test]
    fn committed_baseline_roster_matches_the_module() {
        let baseline = committed_baseline().expect("committed baseline valid");
        let ids: Vec<&str> = baseline.corpora.iter().map(|c| c.id.as_str()).collect();
        let expected: Vec<&str> = CORPUS_SPECS.iter().map(|s| s.id).collect();
        assert_eq!(ids, expected);
    }

    #[test]
    fn ratio_gate_fails_a_pathological_regression() {
        let baseline = ParserBaseline {
            corpora: vec![
                BaselineCorpus {
                    id: PLAIN_TEXT_ID.to_string(),
                    mb_s: 100.0,
                },
                BaselineCorpus {
                    id: CORPUS_SPECS[1].id.to_string(),
                    mb_s: 25.0,
                },
            ],
            regression_factor: REGRESSION_FACTOR,
            absolute_floor_mb_s: ABSOLUTE_FLOOR_MB_S,
        };
        let healthy = ParserThroughputReport {
            corpora: vec![
                CorpusMeasurement {
                    id: PLAIN_TEXT_ID,
                    bytes: 1,
                    actions: 1,
                    mb_s: 100.0,
                },
                CorpusMeasurement {
                    id: CORPUS_SPECS[1].id,
                    bytes: 1,
                    actions: 1,
                    mb_s: 25.0,
                },
            ],
            sample_bytes: 1,
            rounds: 1,
            elapsed_secs: 0.0,
        };
        assert!(regression_check(&healthy, &baseline).all_passed());

        let regressed = ParserThroughputReport {
            corpora: vec![
                CorpusMeasurement {
                    id: PLAIN_TEXT_ID,
                    bytes: 1,
                    actions: 1,
                    mb_s: 100.0,
                },
                CorpusMeasurement {
                    id: CORPUS_SPECS[1].id,
                    bytes: 1,
                    actions: 1,
                    mb_s: 1.0,
                },
            ],
            sample_bytes: 1,
            rounds: 1,
            elapsed_secs: 0.0,
        };
        let report = regression_check(&regressed, &baseline);
        assert!(!report.all_passed(), "a 25x regression must fail");
        assert!(report.format_summary().contains("FAIL"));
    }

    #[test]
    fn segments_are_bounded_and_deterministic() {
        let first = load_segments().expect("segments load");
        let second = load_segments().expect("segments load");
        assert_eq!(first.len(), CORPUS_SPECS.len());
        for ((spec_a, a), (spec_b, b)) in first.iter().zip(second.iter()) {
            assert_eq!(spec_a.id, spec_b.id);
            assert_eq!(a, b, "corpus {} must be deterministic", spec_a.id);
            assert!(!a.is_empty());
            assert!(a.len() <= MAX_SEGMENT_BYTES);
        }
    }
}
