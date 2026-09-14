#![forbid(unsafe_code)]
//! Compat lab owner crate — re-exports headless bounded harness and corpora.
//!
//! The canonical harness source remains `tests/compat/harness.rs` at the
//! workspace root. This crate provides a workspace-member integration point
//! so `cargo test -p bitty-compat-lab --test harness` and `cargo test --workspace`
//! exercise the lab without relying on workspace-root `tests/` discovery
//! (workspace root is virtual and does not auto-discover `tests/**`).

use std::ffi::OsStr;
use std::path::PathBuf;

/// Repository root of this `bitty` checkout: the crate's grandparent
/// directory (`<repo>/crates/<crate>`), derived from `CARGO_MANIFEST_DIR`.
///
/// Never hardcode a checkout path: the same crate is built from worktrees,
/// CI checkouts, and clones under arbitrary roots.
pub fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("crate directory must be <workspace>/crates/<crate>")
        .to_path_buf()
}

/// Umbrella workspace root for the multi-repository layout: `$BITTY_WORKSPACE`
/// when set to a non-empty value, otherwise the parent directory of
/// [`workspace_root`] (the umbrella layout that holds the sibling
/// repositories). Derived from the crate location or the environment — never
/// an absolute literal, so the crate builds and tests anywhere.
pub fn umbrella_root() -> Option<PathBuf> {
    umbrella_root_from(std::env::var_os("BITTY_WORKSPACE").as_deref())
}

/// Pure derivation behind [`umbrella_root`]: a non-empty explicit value wins,
/// otherwise the repository's parent directory. Kept separate from the
/// environment lookup so tests can cover both branches without mutating
/// process-global state (`std::env::set_var` is `unsafe` and this crate is
/// `forbid(unsafe_code)`).
pub fn umbrella_root_from(env_value: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(value) = env_value.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(value));
    }
    workspace_root().parent().map(PathBuf::from)
}

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
pub mod report;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_root_is_the_repo_containing_this_crate() {
        let root = workspace_root();
        assert!(root.join("crates/bitty-compat-lab/Cargo.toml").is_file());
        assert!(root.join("tests/compat").is_dir());
    }

    #[test]
    fn umbrella_root_falls_back_to_the_repo_parent() {
        let expected = workspace_root().parent().map(|p| p.to_path_buf());
        assert!(expected.is_some(), "repo root must have a parent");
        assert_eq!(umbrella_root_from(None), expected);
        assert_eq!(umbrella_root_from(Some(OsStr::new(""))), expected);
    }

    #[test]
    fn umbrella_root_honors_a_non_empty_env_override() {
        let synthetic = PathBuf::from("synthetic-umbrella-root");
        assert_eq!(
            umbrella_root_from(Some(synthetic.as_os_str())),
            Some(synthetic.clone())
        );
    }
}
