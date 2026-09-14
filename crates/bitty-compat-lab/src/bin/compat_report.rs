#![forbid(unsafe_code)]

//! Emit the M1/M2 terminal compatibility matrix report (CTX-0404).
//!
//! Deterministic, offline, and bounded: verifies every `ci` row (corpus replay
//! or named-test presence), probes tool presence on `PATH`, and prints the
//! report JSON. Local (env-gated PTY) evidence is produced separately by
//! `tests/live_compat.rs`; see `scripts/compat-local.sh`.
//!
//! Usage:
//! ```text
//! cargo run -p bitty-compat-lab --bin compat_report --locked
//! cargo run -p bitty-compat-lab --bin compat_report --locked -- --out recording/compat-report.json
//! ```

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

fn usage() -> &'static str {
    "usage: compat_report [--out <path>]\n\n\
     Emits the deterministic M1/M2 compatibility matrix report as JSON.\n\
     Set BITTY_COMPAT_REVISION=<rev> to record the verified revision."
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

    let json = match bitty_compat_lab::report::generate_report_json() {
        Ok(json) => json,
        Err(err) => {
            eprintln!("compat_report failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    match out {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                if let Err(err) = fs::create_dir_all(parent) {
                    eprintln!("compat_report: cannot create {}: {err}", parent.display());
                    return ExitCode::FAILURE;
                }
            }
            if let Err(err) = fs::write(&path, &json) {
                eprintln!("compat_report: cannot write {}: {err}", path.display());
                return ExitCode::FAILURE;
            }
            println!(
                "compat_report: wrote {} ({} bytes)",
                path.display(),
                json.len()
            );
        }
        None => print!("{json}"),
    }
    ExitCode::SUCCESS
}
