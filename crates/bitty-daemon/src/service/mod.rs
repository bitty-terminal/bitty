//! The `bittyd` service surface: scopes, dispatch, and the local server loop.
//!
//! This module is the implementation of bitty#2607: the `bitty-ipc` v1
//! transport meets the accepted [`bittyd` Session-Host Contract]. It is built
//! in reviewable layers:
//!
//! | Layer | Responsibility |
//! | --- | --- |
//! | [`scopes`] | The `bittyd`-owned `session.*` / `daemon.*` scope registry, the frozen method catalog, and the deny-by-default resolver |
//! | [`protocol`] | Per-connection state: client id, effective scopes, leases, and pending control-token delivery |
//! | [`grants`] | Host-consent and observer grants: mint, single-use consume, TTL, revoke |
//! | [`wire`] | The bounded `bitty-ipc` v1 envelopes and the static error rendering |
//! | [`dispatch`] | The eleven-stage authorization order and the closed refusal mapping |
//! | [`frame`] | Per-frame `SCM_CREDENTIALS` validation on Linux and the bounded frame reader |
//! | [`server`] | Endpoint admission, connection cap, rate and payload limits, and shutdown |
//!
//! Every refusal is a static code with static counts and bounds. No caller
//! bytes, path, token, bearer, terminal text, or another session's fields are
//! echoed, and a missing scope is decided before any target lookup.
//!
//! [`bittyd` Session-Host Contract]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md

pub mod dispatch;
#[cfg(unix)]
pub mod frame;
pub mod grants;
pub mod protocol;
pub mod scopes;
pub mod server;
pub mod wire;

pub use dispatch::{Refusal, Service};
pub use grants::{GrantId, GrantKind, GrantStore};
pub use protocol::Connection;
pub use scopes::{
    IdempotencyClass, MethodResolution, ScopeRequirement, ServiceMethod, ServiceScope,
    ServiceScopeSet, resolve,
};
pub use server::{ServerConfig, ServerError, ServerOutcome, run};
