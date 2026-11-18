//! `bitty.com` entry point: delegate every decision to [`bitty_com`].
//!
//! This binary stays a console-subsystem executable (the default on Windows),
//! which is exactly what makes a shell invocation block on it.

#![forbid(unsafe_code)]

fn main() {
    std::process::exit(bitty_com::run());
}
