//! Demo crate (status-drift violations fixture).
//!
//! OQ-018 remains open. The IPC/MCP wire protocol RFC has not landed.
//! The host tracks the `plugin-platform-rfc.md` contract
//! (`Proposed` / `draft`, `OQ-011..OQ-013`, `OQ-014`).
#![forbid(unsafe_code)]

pub fn hello() -> &'static str {
    "violations"
}
