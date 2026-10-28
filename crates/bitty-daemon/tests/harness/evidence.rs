//! Durable per-vector evidence files.
//!
//! One file per (cell, suite) under `$CARGO_TARGET_DIR/conformance/` or the
//! workspace `target/conformance/`. The record contains no host path, username,
//! socket path, port, or token: only the cell, version, commit, instance,
//! advertised capabilities and limits, and per-vector status.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::harness::corpus::DriverKind;
use crate::harness::runner::{RowStatus, ServiceFacts, VectorOutcome};

/// Evidence schema.
pub const SCHEMA: &str = "bittyd-conformance-evidence/1";

/// Provenance for one evidence file.
pub struct Evidence<'a> {
    pub suite: &'a str,
    pub label: &'a str,
    pub facts: &'a ServiceFacts,
    pub corpus_contract: &'a str,
    pub rows: &'a [VectorOutcome],
}

/// Render the evidence document.
pub fn render(evidence: &Evidence<'_>) -> Value {
    let mut rows = Vec::with_capacity(evidence.rows.len());
    let mut passed = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let mut known = 0;
    for row in evidence.rows {
        match row.status {
            RowStatus::Pass => passed += 1,
            RowStatus::Skip => skipped += 1,
            RowStatus::Fail => failed += 1,
            RowStatus::KnownDefect => known += 1,
        }
        rows.push(json!({
            "id": row.id,
            "title": row.title,
            "area": row.area,
            "decisions": row.decisions,
            "driver": row.driver.as_str(),
            "status": row.status.as_str(),
            "skip_reason": row.skip_reason,
            "deferred": row.deferred,
            "known_defect": row.known_defect,
            "instance_id": row.instance_id,
            "steps": row.steps,
            "signature": row.signature,
            "failure": row.failure,
        }));
    }
    json!({
        "schema": SCHEMA,
        "suite": evidence.suite,
        "label": evidence.label,
        "cell": evidence.facts.cell,
        "version": evidence.facts.version,
        "commit": commit(),
        "corpus_contract": evidence.corpus_contract,
        "corpus_schema": crate::harness::corpus::SCHEMA,
        "frozen": {"session.attach.request_role": "attach_role"},
        "instance_id": evidence.facts.instance_id,
        "capabilities": evidence.facts.capabilities,
        "limits": evidence.facts.limits,
        "counts": {
            "pass": passed,
            "skip": skipped,
            "fail": failed,
            "known_defect": known,
            "total": evidence.rows.len(),
        },
        "vectors": rows,
    })
}

/// Write the evidence document and return its path (for the CI artifact).
///
/// # Errors
///
/// An I/O error message without the path, so a failure log stays free of host
/// locations.
pub fn write(evidence: &Evidence<'_>) -> Result<PathBuf, String> {
    let directory = directory();
    std::fs::create_dir_all(&directory)
        .map_err(|_| "cannot create evidence directory".to_owned())?;
    let path = directory.join(format!("{}.{}.json", evidence.facts.cell, evidence.suite));
    let mut text = serde_json::to_string_pretty(&render(evidence))
        .map_err(|_| "cannot render evidence".to_owned())?;
    text.push('\n');
    std::fs::write(&path, text).map_err(|_| "cannot write evidence file".to_owned())?;
    Ok(path)
}

/// The conformance evidence directory.
pub fn directory() -> PathBuf {
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        if !target.is_empty() {
            return PathBuf::from(target).join("conformance");
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("conformance")
}

fn commit() -> String {
    if let Ok(sha) = std::env::var("GITHUB_SHA") {
        if !sha.is_empty() {
            return sha;
        }
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(["rev-parse", "HEAD"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
        _ => "unknown".to_owned(),
    }
}

/// Render the parity document.
pub fn render_parity(compared: usize, divergences: &[String], skipped: &[String]) -> Value {
    json!({
        "schema": SCHEMA,
        "suite": "parity",
        "label": "fake-vs-real",
        "cell": cell(),
        "version": bitty_daemon::CRATE_VERSION,
        "commit": commit(),
        "compared": compared,
        "divergences": divergences,
        "not_compared": skipped,
        "status": if divergences.is_empty() { "pass" } else { "fail" },
    })
}

/// Write the parity document.
///
/// # Errors
///
/// An I/O error message without the path.
pub fn write_parity(
    compared: usize,
    divergences: &[String],
    skipped: &[String],
) -> Result<PathBuf, String> {
    let directory = directory();
    std::fs::create_dir_all(&directory)
        .map_err(|_| "cannot create evidence directory".to_owned())?;
    let path = directory.join(format!("{}.parity.json", cell()));
    let mut text = serde_json::to_string_pretty(&render_parity(compared, divergences, skipped))
        .map_err(|_| "cannot render parity evidence".to_owned())?;
    text.push('\n');
    std::fs::write(&path, text).map_err(|_| "cannot write parity evidence".to_owned())?;
    Ok(path)
}

/// The platform cell string.
pub fn cell() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The evidence label for a driver.
pub const fn driver_label(driver: DriverKind) -> &'static str {
    match driver {
        DriverKind::Fake => "candidate-not-bittyd",
        DriverKind::Real => "bittyd-real-service",
    }
}
