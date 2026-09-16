#![forbid(unsafe_code)]
//! `bitty-vm` entry point: argument handling lives in `bitty_test_vm::cli`.

use std::process::ExitCode;

fn main() -> ExitCode {
    bitty_test_vm::cli::run(std::env::args().skip(1))
}
