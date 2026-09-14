//! Demo crate (status-drift clean fixture).
//!
//! OQ-018 is accepted per the canonical register (ipc-agent-rfc accepted
//! 2026-08-29, closes OQ-018). The Plugin Platform RFC is accepted and
//! closes OQ-011, OQ-012, OQ-013; OQ-014 is accepted via the
//! isolation-resource RFC. OQ-053 is accepted and closed.
#![forbid(unsafe_code)]

pub fn hello() -> &'static str {
    "clean"
}
