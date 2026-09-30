#![forbid(unsafe_code)]
//! Staged first-party panel experience implementations (CTX-0438 extraction wave).
//!
//! This crate owns the first-party panel experiences that were formerly
//! embedded in `bitty-runtime` and were moved out of the microkernel crate by
//! the 024 §17.3 extraction wave (research 001/002/009/013/014 red lines; the
//! 2026-09-16 architectural-drift review §4). Its modules consume only the
//! **accepted public Panel Runtime path** — `bitty_runtime::registry` plus
//! `bitty_ui` primitives — and hold application policy, not terminal
//! mechanism.
//!
//! # Boundary rule
//!
//! - `bitty-runtime` keeps mechanism (PTY, VT, grid, render seam, terminal
//!   registry, generic `PanelRegistry`/`PanelEventBus`, plugin host wiring).
//! - This crate keeps optional experience policy (capability strings, bounds,
//!   pure listing/filtering/validation helpers, tiled-layout assembly) and
//!   creates its panels through the public `create_panel` → `mount_panel`
//!   path with typed errors. No private channel, no first-party bypass, no
//!   parser/renderer/input hot path, and no grid mutation (only `Action`
//!   writes `State`).
//!
//! # Status (staging boundary, not a shipped split)
//!
//! `ai_panel` and `mail_panel` have been removed per CTX-0886 (Unix philosophy:
//! Core provides mechanism only). They will become independent crates
//! (`bitty-ai`, `bitty-mail`) like `bitty-network`, gated on the
//! panel-provider contract (OQ-058) and credential-source contract
//! (OQ-054/OQ-055).
//!
//! Panels recorded as Core or already split (`browser-panel` per `CTX-0401`;
//! `palette`/`statusline` residual Core helpers per `CTX-0397`/`CTX-0398`;
//! `project`, `shell-integration`, and the workspace core per the OQ-053
//! decision) deliberately stay in `bitty-runtime` in this phase.

mod scaffold;
