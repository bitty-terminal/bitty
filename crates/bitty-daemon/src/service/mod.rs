//! The `bittyd` service surface: scopes, dispatch, and the local server loop.
//!
//! This module is the implementation of bitty#2607: the `bitty-ipc` v1
//! transport meets the accepted [`bittyd` Session-Host Contract]. It is built
//! in reviewable layers:
//!
//! | Layer | Responsibility |
//! | --- | --- |
//! | [`scopes`] | The `bittyd`-owned `session.*` / `daemon.*` scope registry, the frozen method catalog, and the deny-by-default resolver |
//!
//! Every refusal is a static code with static counts and bounds. No caller
//! bytes, path, token, bearer, terminal text, or another session's fields are
//! echoed, and a missing scope is decided before any target lookup.
//!
//! [`bittyd` Session-Host Contract]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md

pub mod scopes;

pub use scopes::{
    IdempotencyClass, MethodResolution, ScopeRequirement, ServiceMethod, ServiceScope,
    ServiceScopeSet, resolve,
};
