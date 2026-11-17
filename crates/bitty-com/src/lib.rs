//! `bitty.com`: the console companion proxy for the GUI-subsystem `bitty.exe`.
//!
//! # Why this binary exists (bitty#3224)
//!
//! `bitty.exe` links with `windows_subsystem = "windows"` so a Start Menu or
//! Search launch never flashes a console window (bitty#2929). The cost is that
//! `CreateProcess` starts it as a GUI application: a command prompt, PowerShell,
//! Windows Terminal, or a Scoop shim does not block on it, `$LASTEXITCODE` stays
//! unset, and a CLI subcommand like `bitty init` appears to do nothing.
//!
//! Windows resolves `bitty` to `bitty.com` before `bitty.exe` (`CreateProcess`
//! searches the `%PATHEXT%` order, and `.COM` precedes `.EXE`), so shipping this
//! small console-subsystem binary next to the GUI one restores the classic
//! console-companion contract:
//!
//! - a **console** invocation (`bitty init`, `bitty --version`, `bitty doctor`,
//!   `bitty +list-themes`, an invalid flag) starts `bitty.exe` with inherited
//!   stdin/stdout/stderr, **waits**, and returns the child's exit code
//!   ([`Mode::WaitCli`]);
//! - a **desktop** invocation (bare `bitty`, `bitty --split v`, `bitty -- cat`)
//!   starts `bitty.exe` **detached** and returns 0 immediately ([`Mode::DetachGui`]).
//!
//! # Layout of this crate
//!
//! - [`grammar`] holds the argv classifier: the "is this invocation a console
//!   command?" decision. It is a deliberate, small duplicate of the real parser
//!   in `crates/bitty-terminal`, and `bitty-terminal`'s `com_parity_tests` run
//!   the same argv corpus through both so the two cannot drift silently.
//! - [`launch`] resolves the sibling executable from *this* process's own path
//!   and performs the spawn. All Win32-specific behavior lives behind
//!   `#[cfg(windows)]`; the non-Windows build is a working fallback (useful for
//!   manual testing on a Linux checkout), never a stub that fails to compile.
//!
//! # Safety
//!
//! No `unsafe` anywhere: the crate is `#![forbid(unsafe_code)]`, and the only
//! platform-specific API used is the safe `CommandExt::creation_flags`
//! extension from `std::os::windows::process` (Windows builds only) plus the two
//! `CreateProcess` flag constants this crate spells out in [`launch`].

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod grammar;
pub mod launch;

pub use grammar::{Mode, classify, classify_with_env, headless_requested_by_env};
pub use launch::run;
