//! Core-owned placeholder scene for an unbound panel (OQ-084 Decision E rules
//! 5-6; bitty#2476 residual 4).
//!
//! A panel whose controller is missing, suspended, disabled, crashed,
//! mid-reload, denied, or incompatible keeps its container and presents a
//! placeholder. That surface belongs to Core, not to the plugin:
//!
//! - it is a **fixed** scene built from the constant table in
//!   [`bitty_ui::panel_recovery::PlaceholderScene`] — the same text for the same
//!   reason every time;
//! - it carries **no plugin text, style, or node tree**: this module accepts a
//!   [`PanelId`], a [`PlaceholderReason`], and a generation, and nothing else;
//! - it is admitted through [`RichBlock::new`], so `SCN-1` (nodes), `SCN-2`
//!   (depth), and `SCN-3` (text bytes) are enforced by the accepted scene gate
//!   rather than re-asserted here, and the shape is six nodes at depth three;
//! - it holds **no handle of any kind** (rule 6): the module imports the scene
//!   data model and the panel presentation types only — no PTY, no renderer, no
//!   plugin host, no callback. `tests/panel_placeholder_bounds.rs` pins that
//!   this file's imports stay that way.
//!
//! The block is rebuilt from the record's reason on demand instead of being
//! stored, so there is no placeholder scene a plugin could hold on to, mutate,
//! or replace.

#![forbid(unsafe_code)]

use bitty_rich::scene::{
    BlockAnchor, BlockId, RichBlock, SceneError, SceneNode, ScrollBehavior, StyledSpan,
};
use bitty_ui::panel::PanelId;
use bitty_ui::panel_recovery::{PlaceholderReason, PlaceholderScene};

/// Owner id stamped on every Core-owned placeholder block.
///
/// Zero is Core's own owner slot: pointer and wheel input are attributed to
/// plugin ids allocated from one upward, so no plugin can claim a placeholder
/// block as its own.
pub const PLACEHOLDER_BLOCK_OWNER: u64 = 0;

/// Creation stamp on a placeholder block.
///
/// Constant, not the wall clock: the placeholder path stays deterministic, so
/// two presentations of the same reason produce byte-identical blocks.
pub const PLACEHOLDER_BLOCK_CREATED_AT: u64 = 0;

/// Builds the Core-owned placeholder block for `reason`.
///
/// The scene is one column of three lines: the bold Core heading, the detail
/// line naming the reason, and a row of the two action labels (close, retry).
/// Nothing in the shape depends on the caller beyond the reason, and the block
/// is admitted by [`RichBlock::new`] against the accepted `SCN-1..3` bounds.
///
/// # Errors
/// [`SceneError`] only if the fixed table ever stops fitting the accepted scene
/// limits; the table is constant, so a refusal here is a Core defect rather
/// than a caller error.
pub fn core_placeholder_block(
    panel: PanelId,
    reason: PlaceholderReason,
    container_generation: u64,
) -> Result<RichBlock, SceneError> {
    let surface = PlaceholderScene::for_reason(reason);
    let action_labels: Vec<SceneNode> = surface
        .actions()
        .iter()
        .map(|action| SceneNode::Text(text(action.label(), false)))
        .collect();
    let content = SceneNode::Column(vec![
        SceneNode::Text(text(surface.heading(), true)),
        SceneNode::Text(text(surface.detail(), false)),
        SceneNode::Row(action_labels),
    ]);
    RichBlock::new(
        BlockId(panel.get()),
        BlockAnchor::Line(panel.get()),
        content,
        // The placeholder rides the container's content rect and takes no part
        // in terminal scroll layout.
        ScrollBehavior::Overlay,
        PLACEHOLDER_BLOCK_OWNER,
        container_generation,
        PLACEHOLDER_BLOCK_CREATED_AT,
    )
}

/// One plain scene text span with an explicit weight.
fn text(value: &str, bold: bool) -> StyledSpan {
    StyledSpan {
        text: value.to_owned(),
        bold,
        italic: false,
    }
}
