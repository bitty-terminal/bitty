//! Typical-session memory + growth — PB-3 (CTX-0676, PERF-04).
//!
//! Bounded, headless, synthetic anchor for
//! `performance-budget-rfc.md#pb-3-typical-session-memory`: 8 tabs after a
//! mixed session must stay within 250 MB, and closing back to one tab must
//! reclaim to within 15 % of the pre-open baseline.
//!
//! ## Measurement contract
//!
//! - **Synthetic proxy, stated honestly.** A real 4 h mixed session on Tier 1
//!   hardware is out of scope for a bounded bench; this module opens 8 real
//!   [`bitty_term_state::State`] tabs, feeds each a fixed deterministic
//!   workload ([`BYTES_PER_TAB`] bytes of mixed SGR/cursor/OSC/plain output
//!   in [`CHUNK_BYTES`] chunks), samples self RSS, then drops 7 tabs and
//!   samples again. It proves the harness and records a regression anchor,
//!   not budget compliance. The 8-tab real-session measurement stays
//!   deferred per issue #1058.
//! - **Real RSS, never fabricated.** Samples come from `/proc/self/status`
//!   (`VmRSS`, Linux). When unreadable the report is `Unavailable` with a
//!   reason and `--write-baseline` refuses to write (exit 2).
//! - **Bounded.** 8 tabs x 64 KiB, 8 KiB chunks; no display, no PTY, no
//!   network, no clock-dependent assertion.
//! - **Host context from the environment.** OS/arch/CPU/memory via
//!   [`real_window::HostContext`]; provenance via [`real_window::BaselineMeta`];
//!   no checkout path, username, or hostname is embedded.
//!
//! `#![forbid(unsafe_code)]`; the RSS probe is Linux-gated and degrades to
//! `Unavailable` elsewhere.
//!
//! Budget reference:
//! `docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory`.
//! Runbook: `crates/bitty-perf/baselines/typical-session-evidence.md`.

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use bitty_term_state::State;
use bitty_vt::Parser;

use super::real_window::{BaselineMeta, HostContext};

/// Schema version of the committed `pb-typical-session.json` artifact.
pub const TYPICAL_SESSION_SCHEMA_VERSION: u32 = 1;
/// Committed PB-3 evidence artifact, relative to the repository root.
pub const TYPICAL_SESSION_BASELINE_REL_PATH: &str =
    "crates/bitty-perf/baselines/pb-typical-session.json";

/// Tabs opened in the synthetic session (the PB-3 8-tab shape).
pub const TABS: usize = 8;
/// Tabs retained after the close step (reclaim target).
pub const TABS_AFTER_CLOSE: usize = 1;
/// Deterministic workload bytes fed per tab.
pub const BYTES_PER_TAB: usize = 64 * 1024;
/// Chunk size fed to `Parser::advance` (matches `bitty-pty::READ_CHUNK_SIZE`).
pub const CHUNK_BYTES: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// Synthetic workload (deterministic, fixed, bounded)
// ---------------------------------------------------------------------------

/// One deterministic workload line: shell prompt + SGR color + cursor motion +
/// plain text. Repeated to [`BYTES_PER_TAB`] bytes per tab.
const WORKLOAD_LINE: &[u8] = b"\x1b[32m$\x1b[0m cargo test --workspace \x1b[1mok 42 passed\x1b[0m\n\x1b[1;34m~/bitty\x1b[0m \x1b[4mgit log --oneline\x1b[24m abc1234 fix parser edge\nplain output line 0123456789 abcdefghijklmnopqrstuvwxyz\n\x1b]0;bitty-tab\x07\x1b[2K\x1b[1Grefreshed status line\n";

/// Build the fixed per-tab workload (`len` bytes, line-repeated).
#[must_use]
pub fn workload_bytes(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let take = (len - out.len()).min(WORKLOAD_LINE.len());
        out.extend_from_slice(&WORKLOAD_LINE[..take]);
    }
    out
}

// ---------------------------------------------------------------------------
// Pure metric math (unit-testable, no display)
// ---------------------------------------------------------------------------

/// Growth in percent from `before_mb` to `after_mb` (`None` when `before` is
/// non-positive).
#[must_use]
pub fn growth_pct(before_mb: f64, after_mb: f64) -> Option<f64> {
    if before_mb <= 0.0 {
        return None;
    }
    Some((after_mb - before_mb) / before_mb * 100.0)
}

/// How far `after_close_mb` sits above `before_mb`, in percent (`None` when
/// `before` is non-positive). PB-3 reclaim requires this to be within
/// [`crate::PB3_RECLAIM_PCT`].
#[must_use]
pub fn reclaim_within_pct(before_mb: f64, after_close_mb: f64) -> Option<f64> {
    growth_pct(before_mb, after_close_mb)
}

/// `true` when an 8-tab RSS sample is within the PB-3 typical-session budget.
#[must_use]
pub fn meets_pb3(open_mb: f64) -> bool {
    open_mb <= crate::PB3_TYPICAL_RSS_MB as f64
}

/// `true` when the post-close RSS is within the reclaim budget above baseline.
#[must_use]
pub fn meets_reclaim(before_mb: f64, after_close_mb: f64) -> bool {
    reclaim_within_pct(before_mb, after_close_mb)
        .is_some_and(|pct| pct <= crate::PB3_RECLAIM_PCT as f64)
}

// ---------------------------------------------------------------------------
// Self-RSS probe (Linux `/proc/self/status`, else unavailable)
// ---------------------------------------------------------------------------

/// Read this process's RSS in MB. `None` when unreadable (non-Linux or
/// restricted `/proc`).
#[must_use]
pub fn self_rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024.0)
}

// ---------------------------------------------------------------------------
// Session report
// ---------------------------------------------------------------------------

/// Bounded synthetic 8-tab session memory report.
#[derive(Debug, Clone, Default)]
pub struct TypicalSessionReport {
    /// Tabs opened (always [`TABS`]).
    pub tabs: usize,
    /// Workload bytes fed per tab.
    pub bytes_per_tab: usize,
    /// Total workload bytes across all tabs.
    pub total_bytes: usize,
    /// Self RSS before opening tabs, in MB (`None` when unreadable).
    pub rss_before_mb: Option<f64>,
    /// Self RSS with all tabs open, in MB.
    pub rss_open_mb: Option<f64>,
    /// Self RSS after closing back to [`TABS_AFTER_CLOSE`], in MB.
    pub rss_close_mb: Option<f64>,
    /// Why the measurement is unavailable (`None` when measured).
    pub reason: Option<String>,
    /// Wall time of the whole measurement.
    pub elapsed: Duration,
}

impl TypicalSessionReport {
    /// `true` when any RSS sample is missing.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        self.rss_before_mb.is_none() || self.rss_open_mb.is_none() || self.rss_close_mb.is_none()
    }

    /// Absolute growth from baseline to 8-tab open, in MB.
    #[must_use]
    pub fn growth_mb(&self) -> Option<f64> {
        Some(self.rss_open_mb? - self.rss_before_mb?)
    }

    /// Relative growth from baseline to 8-tab open, in percent.
    #[must_use]
    pub fn growth_pct(&self) -> Option<f64> {
        growth_pct(self.rss_before_mb?, self.rss_open_mb?)
    }

    /// How far the post-close RSS sits above baseline, in percent.
    #[must_use]
    pub fn reclaim_within_pct(&self) -> Option<f64> {
        reclaim_within_pct(self.rss_before_mb?, self.rss_close_mb?)
    }

    /// `true` when the 8-tab RSS is within the PB-3 budget.
    #[must_use]
    pub fn meets_open_budget(&self) -> bool {
        self.rss_open_mb.is_some_and(meets_pb3)
    }

    /// `true` when the post-close RSS reclaimed within the reclaim budget.
    #[must_use]
    pub fn meets_reclaim(&self) -> bool {
        match (self.rss_before_mb, self.rss_close_mb) {
            (Some(before), Some(close)) => meets_reclaim(before, close),
            _ => false,
        }
    }

    /// Human summary for bench output and evidence docs.
    #[must_use]
    pub fn format_summary(&self) -> String {
        if self.is_unavailable() {
            return format!(
                "typical-session — UNMEASURED ({}); tabs={} x {} KiB ({} KiB total)",
                self.reason.as_deref().unwrap_or("no RSS sample"),
                self.tabs,
                self.bytes_per_tab / 1024,
                self.total_bytes / 1024,
            );
        }
        let before = self.rss_before_mb.unwrap_or(0.0);
        let open = self.rss_open_mb.unwrap_or(0.0);
        let close = self.rss_close_mb.unwrap_or(0.0);
        let open_verdict = if self.meets_open_budget() {
            "PASS"
        } else {
            "ABOVE_BUDGET"
        };
        let reclaim_verdict = if self.meets_reclaim() {
            "PASS"
        } else {
            "ABOVE_BUDGET"
        };
        format!(
            "typical-session — {open_verdict} open {open:.2} MB vs PB-3 budget {} MB (baseline {before:.2} MB, growth {:+.2} MB / {:+.2}%); {reclaim_verdict} reclaim {close:.2} MB ({:+.2}% vs baseline, budget within {}%); tabs={} x {} KiB, {:.1?} wall",
            crate::PB3_TYPICAL_RSS_MB,
            self.growth_mb().unwrap_or(0.0),
            self.growth_pct().unwrap_or(0.0),
            self.reclaim_within_pct().unwrap_or(0.0),
            crate::PB3_RECLAIM_PCT,
            self.tabs,
            self.bytes_per_tab / 1024,
            self.elapsed,
        )
    }
}

/// Run the bounded synthetic 8-tab session and return the report.
///
/// Headless; no PTY, no display. Every RSS sample is real (`/proc/self`);
/// when the probe fails the report is `Unavailable`, never fabricated.
#[must_use]
pub fn measure_typical_session() -> TypicalSessionReport {
    measure_typical_session_with(TABS, BYTES_PER_TAB)
}

/// Measurement variant with explicit bounds (used by the bench and tests).
/// `tabs` is clamped to 1..=16, `bytes_per_tab` to 1 KiB..=1 MiB.
#[must_use]
pub fn measure_typical_session_with(tabs: usize, bytes_per_tab: usize) -> TypicalSessionReport {
    let t0 = Instant::now();
    let tabs = tabs.clamp(1, 16);
    let bytes_per_tab = bytes_per_tab.clamp(1024, 1024 * 1024);
    let total_bytes = tabs * bytes_per_tab;
    let workload = workload_bytes(bytes_per_tab);

    let unavailable = |reason: &str| TypicalSessionReport {
        tabs,
        bytes_per_tab,
        total_bytes,
        reason: Some(reason.to_string()),
        elapsed: t0.elapsed(),
        ..TypicalSessionReport::default()
    };

    let Some(before) = self_rss_mb() else {
        return unavailable("self RSS unreadable (/proc/self/status VmRSS missing)");
    };

    // Open: one real State + Parser per tab, workload fed in 8 KiB chunks.
    let mut states: Vec<State> = Vec::with_capacity(tabs);
    for _ in 0..tabs {
        let mut state = State::new();
        let mut parser = Parser::new();
        for chunk in workload.chunks(CHUNK_BYTES) {
            let mut actions = Vec::new();
            parser.advance(black_box(chunk), |a| actions.push(a));
            for action in actions.drain(..) {
                black_box(state.apply(black_box(&action)));
            }
        }
        black_box(state.snapshot());
        states.push(state);
    }
    let Some(open) = self_rss_mb() else {
        return unavailable("self RSS unreadable after open");
    };

    // Close: retain one tab, drop the rest, then sample.
    states.truncate(TABS_AFTER_CLOSE.min(states.len()));
    let Some(close) = self_rss_mb() else {
        return unavailable("self RSS unreadable after close");
    };
    drop(states);

    TypicalSessionReport {
        tabs,
        bytes_per_tab,
        total_bytes,
        rss_before_mb: Some(before),
        rss_open_mb: Some(open),
        rss_close_mb: Some(close),
        reason: None,
        elapsed: t0.elapsed(),
    }
}

// ---------------------------------------------------------------------------
// Committed baseline artifact
// ---------------------------------------------------------------------------

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The committed evidence artifact, embedded at compile time.
#[must_use]
pub const fn committed_session_json() -> &'static str {
    include_str!("../baselines/pb-typical-session.json")
}

/// Serialize a measured report plus provenance into the committed shape.
/// Returns `Err` when the report is unavailable — the bench refuses to write
/// fabricated numbers.
pub fn baseline_json(report: &TypicalSessionReport, meta: &BaselineMeta) -> Result<String, String> {
    if report.is_unavailable() {
        return Err("refusing to serialize an unmeasured typical-session report".to_string());
    }
    let host = HostContext::capture();
    let (before, open, close) = (
        report.rss_before_mb.unwrap_or(0.0),
        report.rss_open_mb.unwrap_or(0.0),
        report.rss_close_mb.unwrap_or(0.0),
    );
    let issues: Vec<String> = meta.issues.iter().map(u64::to_string).collect();
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"schema_version\": {TYPICAL_SESSION_SCHEMA_VERSION},\n"
    ));
    out.push_str(&format!("  \"task\": \"{}\",\n", escape(&meta.task)));
    out.push_str(&format!("  \"issues\": [{}],\n", issues.join(", ")));
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
    out.push_str(
        "  \"budget_ref\": \"docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory\",\n",
    );
    out.push_str("  \"host_context\": {\n");
    out.push_str(&format!("    \"os\": \"{}\",\n", escape(&host.os)));
    out.push_str(&format!("    \"arch\": \"{}\",\n", escape(&host.arch)));
    out.push_str(&format!(
        "    \"toolchain\": \"{}\",\n",
        escape(&host.toolchain)
    ));
    out.push_str(&format!("    \"cpus\": {},\n", host.cpus));
    match host.total_memory_mb {
        Some(mb) => out.push_str(&format!("    \"total_memory_mb\": {mb}\n")),
        None => out.push_str("    \"total_memory_mb\": null\n"),
    }
    out.push_str("  },\n");
    out.push_str("  \"bounds\": {\n");
    out.push_str(&format!("    \"tabs\": {},\n", report.tabs));
    out.push_str(&format!("    \"tabs_after_close\": {TABS_AFTER_CLOSE},\n"));
    out.push_str(&format!(
        "    \"bytes_per_tab\": {},\n",
        report.bytes_per_tab
    ));
    out.push_str(&format!("    \"total_bytes\": {},\n", report.total_bytes));
    out.push_str(&format!("    \"chunk_bytes\": {CHUNK_BYTES}\n"));
    out.push_str("  },\n");
    out.push_str("  \"session\": {\n");
    out.push_str("    \"status\": \"measured\",\n");
    out.push_str(&format!("    \"rss_before_mb\": {before:.3},\n"));
    out.push_str(&format!("    \"rss_open_mb\": {open:.3},\n"));
    out.push_str(&format!("    \"rss_close_mb\": {close:.3},\n"));
    out.push_str(&format!(
        "    \"growth_mb\": {:.3},\n",
        report.growth_mb().unwrap_or(0.0)
    ));
    out.push_str(&format!(
        "    \"growth_pct\": {:.3},\n",
        report.growth_pct().unwrap_or(0.0)
    ));
    out.push_str(&format!(
        "    \"reclaim_within_pct\": {:.3},\n",
        report.reclaim_within_pct().unwrap_or(0.0)
    ));
    out.push_str(&format!(
        "    \"budget_mb\": {},\n",
        crate::PB3_TYPICAL_RSS_MB
    ));
    out.push_str(&format!(
        "    \"reclaim_budget_pct\": {},\n",
        crate::PB3_RECLAIM_PCT
    ));
    out.push_str(&format!(
        "    \"meets_open_budget\": {},\n",
        report.meets_open_budget()
    ));
    out.push_str(&format!(
        "    \"meets_reclaim\": {}\n",
        report.meets_reclaim()
    ));
    out.push_str("  }\n}\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_is_deterministic_and_bounded() {
        let a = workload_bytes(4096);
        let b = workload_bytes(4096);
        assert_eq!(a, b);
        assert_eq!(a.len(), 4096);
        assert_eq!(workload_bytes(BYTES_PER_TAB).len(), BYTES_PER_TAB);
    }

    #[test]
    fn growth_math_holds() {
        assert!((growth_pct(100.0, 110.0).unwrap() - 10.0).abs() < 1e-9);
        assert!((reclaim_within_pct(100.0, 112.0).unwrap() - 12.0).abs() < 1e-9);
        assert!(growth_pct(0.0, 10.0).is_none());
        assert!(growth_pct(-1.0, 10.0).is_none());
    }

    #[test]
    fn budget_predicates_hold() {
        assert!(meets_pb3(250.0));
        assert!(!meets_pb3(250.001));
        assert!(meets_reclaim(100.0, 115.0));
        assert!(!meets_reclaim(100.0, 115.001));
    }

    #[test]
    fn unavailable_report_never_claims_measured() {
        let report = TypicalSessionReport::default();
        assert!(report.is_unavailable());
        assert!(!report.meets_open_budget());
        assert!(!report.meets_reclaim());
        assert!(report.format_summary().contains("UNMEASURED"));
        let meta = BaselineMeta {
            task: "test".to_string(),
            ..BaselineMeta::default()
        };
        assert!(baseline_json(&report, &meta).is_err());
    }
}
