//! Composition-root library surface (CTX-1361).
//!
//! The `bitty` binary is a thin composition root. This target exists only so
//! integration tests in `bitty-runtime` can exercise the real
//! `bitty extension add` staging implementation (and later, the production
//! component-host installer) instead of reimplementing it. It adds no
//! behavior to the binary and names the same modules.

#![forbid(unsafe_code)]
// The library is a test seam, not a second product surface: integration tests
// link the normal build, while the binary owns this crate's own unit tests.
// Compiling an empty harness here keeps `cargo clippy --all-targets` from
// building a partial module graph whose tests reference binary-only modules.
#![cfg(not(test))]

pub mod component;
pub mod component_bridge;
#[doc(hidden)]
pub mod component_host;
mod component_seed;
