#![forbid(unsafe_code)]
//! Compat lab owner crate — re-exports headless bounded harness and corpora.
//!
//! The canonical harness source remains `tests/compat/harness.rs` at the
//! workspace root. This crate provides a workspace-member integration point
//! so `cargo test -p bitty-compat-lab --test harness` and `cargo test --workspace`
//! exercise the lab without relying on workspace-root `tests/` discovery
//! (workspace root is virtual and does not auto-discover `tests/**`).

/// Re-export the canonical harness via a `#[path]` module so the source
/// of truth stays `tests/compat/harness.rs`. Using `#[path]` escapes the
/// crate dir without violating Cargo's `[[test]] path must be inside package`
/// rule; `mod` paths are allowed to reference outside via relative `#[path]`.
#[path = "../../../tests/compat/harness.rs"]
pub mod harness;

pub use harness::{
    MAX_ACTIONS, MAX_CORPORA_PER_CATEGORY, MAX_CORPUS_BYTES, MAX_OSC_BYTES, actions_to_snapshot,
    diff_snapshots, list_corpus, parse_bounded,
};

pub mod compare;
pub mod matrix;
