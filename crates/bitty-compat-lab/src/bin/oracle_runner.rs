#![forbid(unsafe_code)]

//! Differential M1 VT oracle runner (CTX-0573, Issue #1133).
//!
//! Replays every scenario in `tests/compat/oracle/scenarios/` through the
//! Bitty parser/state and diffs the observed state against its externally
//! derived expectation, printing one machine-readable summary. Bounded,
//! headless, deterministic: no clock, RNG, network, display, or host path.
//!
//! Usage:
//! ```text
//! cargo run -p bitty-compat-lab --bin oracle_runner --locked
//! cargo run -p bitty-compat-lab --bin oracle_runner --locked -- --out recording/oracle-summary.json
//! ```
//!
//! Exit code is `0` when every scenario passes, `1` when any scenario
//! diverges, and `2` on usage or load failure.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use bitty_compat_lab::oracle::{generate_summary_json, run_oracle};

fn usage() -> &'static str {
    "usage: oracle_runner [--out <path>]\n\n\
     Runs the M1 differential oracle corpus and emits a machine-readable\n\
     per-scenario summary as JSON. Exit 0 = all pass, 1 = divergence."
}

fn main() -> ExitCode {
    let mut out: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => match args.next() {
                Some(path) => out = Some(PathBuf::from(path)),
                None => {
                    eprintln!("--out requires a path\n{}", usage());
                    return ExitCode::from(2);
                }
            },
            "--help" | "-h" => {
                println!("{}", usage());
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument {other:?}\n{}", usage());
                return ExitCode::from(2);
            }
        }
    }

    let report = match run_oracle() {
        Ok(report) => report,
        Err(err) => {
            eprintln!("oracle_runner failed: {err}");
            return ExitCode::from(2);
        }
    };
    let json = match generate_summary_json(&report) {
        Ok(json) => json,
        Err(err) => {
            eprintln!("oracle_runner failed: {err}");
            return ExitCode::from(2);
        }
    };

    match out {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                if let Err(err) = fs::create_dir_all(parent) {
                    eprintln!("oracle_runner: cannot create {}: {err}", parent.display());
                    return ExitCode::FAILURE;
                }
            }
            if let Err(err) = fs::write(&path, &json) {
                eprintln!("oracle_runner: cannot write {}: {err}", path.display());
                return ExitCode::FAILURE;
            }
            println!(
                "oracle_runner: wrote {} ({} bytes)",
                path.display(),
                json.len()
            );
        }
        None => print!("{json}"),
    }

    if report.all_passed() {
        ExitCode::SUCCESS
    } else {
        for outcome in &report.outcomes {
            if outcome.status == bitty_compat_lab::oracle::Status::Fail {
                eprintln!(
                    "DIVERGENCE {} [{}] ({}):",
                    outcome.id, outcome.area, outcome.provenance.source
                );
                for check in outcome.checks.iter().filter(|c| !c.passed) {
                    eprintln!(
                        "  {}: expected {} got {}",
                        check.name, check.expected, check.actual
                    );
                }
            }
        }
        ExitCode::FAILURE
    }
}
