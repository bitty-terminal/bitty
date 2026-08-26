//! `bitty-pty`: owned PTY/process-lifecycle crate with explicit
//! backpressure.
//!
//! The crate implements the PTY row of the Core Workspace Topology
//! (ADR-0003): process lifecycle (spawn/shutdown), resize, and I/O with
//! bounded buffering. It has **no workspace-crate dependencies** by contract,
//! and exactly one third-party dependency (`portable-pty`) by the accepted
//! upstream decision of ADR-0004.
//!
//! # Upstream boundary (ADR-0004 "Wrap" row)
//!
//! `portable-pty` (~0.9) is **wrapped, never adopted**: its types never
//! appear anywhere in this crate's public API, and every upstream failure is
//! flattened into the owned [`PtyError`]. Per ADR-0004's fallback rule, if
//! `portable-pty` becomes unmaintained for more than twelve months while on
//! this hot path it must be replaced by an owned fork extracted under rule 3
//! of that decision (vendored under `vendor/`, named maintenance owner);
//! only this crate's internal platform modules would need mechanical
//! changes, because no caller can observe upstream today.
//!
//! # Security defaults
//!
//! - **No shell interpolation.** [`PtyBuilder`] spawns the program from a
//!   direct argv vector; there is no code path that routes through a shell.
//! - **Minimal child environment.** The inherited environment is stripped;
//!   children receive only explicitly allowlisted entries plus a default
//!   `TERM=xterm-256color` (see [`builder`] for the exact rules and limits).
//! - **fd hygiene.** On Unix the wrapped layer opens both PTY descriptors
//!   close-on-exec, resets inherited signal dispositions, and starts the
//!   child in a fresh session with the PTY as controlling terminal.
//! - **Bounded output buffering.** See below; unbounded parsing or buffering
//!   of untrusted PTY bytes is forbidden by the security corpus.
//!
//! # Backpressure
//!
//! Output flows kernel → pump thread → bounded channel → consumer:
//!
//! - reads are chunked at [`READ_CHUNK_SIZE`] bytes;
//! - the channel holds at most [`CHANNEL_CAPACITY_CHUNKS`] chunks, so the
//!   hard in-crate buffer bound is [`MAX_BUFFERED_BYTES`] (128 KiB);
//! - when the consumer stalls, the channel fills, the pump blocks, the
//!   kernel PTY buffer fills, and the child's writes block — end-to-end
//!   backpressure with zero data loss and zero memory growth.
//!
//! # Platform support
//!
//! Unix (Linux CI target) is implemented. Windows ConPTY sits behind a
//! compile seam that compiles the full public API but returns
//! [`PtyError::Unsupported`] at runtime until the Tier-1 Windows slice
//! lands; other platforms fail to compile rather than silently misbehave.
//!
//! # Example
//!
//! ```
//! use bitty_pty::PtyBuilder;
//!
//! # #[cfg(unix)]
//! # fn run() -> Result<(), bitty_pty::PtyError> {
//! let mut pty = PtyBuilder::new("/bin/cat")
//!     .arg("-A")
//!     .env("LANG", "C")
//!     .size(120, 40)
//!     .spawn()?;
//!
//! assert_eq!(pty.size()?, (120, 40));
//! pty.resize(80, 24)?;
//! assert_eq!(pty.size()?, (80, 24));
//!
//! let mut reader = pty.take_reader()?;
//! let mut writer = pty.take_writer()?;
//! use std::io::Write;
//! writer.write_all(b"ping\n")?;
//! // `cat` echoes the line back through the bounded channel.
//! if let Some(chunk) = reader.recv() {
//!     assert!(chunk.windows(4).any(|w| w == b"ping"));
//! }
//! drop(writer);          // graceful: sends EOF to the child
//! let status = pty.wait()?; // then reap it
//! assert!(status.is_success());
//! # Ok(())
//! # }
//! # #[cfg(unix)]
//! # run().unwrap();
//! ```

#![forbid(unsafe_code)]

mod builder;
mod error;
mod platform;
mod pty;
mod reader;
mod writer;

pub use builder::DEFAULT_COLS;
pub use builder::DEFAULT_ROWS;
pub use builder::DEFAULT_TERM;
pub use builder::MAX_ARGS;
pub use builder::MAX_ARGV_BYTES;
pub use builder::MAX_ENV_ENTRIES;
pub use builder::MAX_ENV_VALUE_BYTES;
pub use builder::PtyBuilder;
pub use error::PtyError;
pub use platform::ExitStatus;
pub use pty::Pty;
pub use reader::CHANNEL_CAPACITY_CHUNKS;
pub use reader::MAX_BUFFERED_BYTES;
pub use reader::PtyReader;
pub use reader::READ_CHUNK_SIZE;
pub use writer::PtyWriter;
