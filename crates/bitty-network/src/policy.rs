//! Policy vocabulary for the network shell.
//!
//! The capability and offline-first policy types are defined once in
//! `bitty-network-api`; this module re-exports them so implementation code
//! names policy through the shell while consumers keep depending on `-api`
//! only. Sealed like the rest of the shell: no enforcement code yet —
//! enforcement arrives with the socket follow-up.

pub use bitty_network_api::{NetworkCapability, OfflineFirst};
