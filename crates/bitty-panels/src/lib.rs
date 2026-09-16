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
//! Extraction of these panels to independent first-party packages remains
//! gated on the accepted OQ-053 verdicts:
//!
//! - [`ai_panel`]: split later (hybrid) — gated on the panel-provider contract
//!   (bitty-docs `CTX-0181`, OQ-058) and the `bitty-ai` surfaces
//!   (OQ-066/OQ-080/OQ-081); owning task `CTX-0402`.
//! - [`mail_panel`]: split later — gated on the panel-provider contract and
//!   the credential-source contract (OQ-054/OQ-055); owning task `CTX-0403`.
//!
//! Until those contracts land, this crate is the in-tree home that keeps the
//! experiences out of the microkernel crate while the bundled catalog,
//! manifests (bitty-plugin-host `bundled.rs`), capability strings, and wire
//! shapes stay unchanged. Panels recorded as Core or already split
//! (`browser-panel` per `CTX-0401`; `palette`/`statusline` residual Core
//! helpers per `CTX-0397`/`CTX-0398`; `project`, `shell-integration`, and the
//! workspace core per the OQ-053 decision) deliberately stay in
//! `bitty-runtime` in this phase.

mod scaffold;

pub mod ai_panel;
pub mod mail_panel;
