//! UI/GPU resource budget tier (UX-24, CTX-0671).
//!
//! Candidate implementation of U-6 (`budget` third). Nothing here is
//! normative, accepted, or verified: the tier presets, every cap, and the
//! refusal-vs-degradation rule below are candidate spellings that the UI
//! Runtime RFC accepts or rejects, never this module. The module is
//! English-only.
//!
//! What this module provides:
//!
//! - [`BudgetTier`] — the candidate preset tiers (`essential`,
//!   `standard`, `rich`) with [`ResourceBudget`] caps for node count,
//!   texture memory, blur surface area, and draw calls (UX-24 scope).
//! - [`ResourceUsage`] — measured demand in the same four dimensions,
//!   composable with [`ResourceUsage::saturating_add`] so per-surface
//!   accounting (tree nodes via [`tree_nodes`], canvas commands as one
//!   draw call each) sums to one admission input.
//! - Admission ([`ResourceBudget::admit`]): structural overuse (nodes,
//!   texture) is refused fail-closed — a partial surface would mislabel —
//!   while blur-only or draw-only overuse degrades to the capped usage
//!   ([`Admission::Degraded`]) because blur reduction and draw batching
//!   keep the surface complete. [`ResourceBudget::check`] is the strict
//!   refuse-on-any-overuse gate for callers that cannot degrade.
//!
//! Accounting granularity is per submission: the caller sums the usage
//! its submission needs and admits the total. No wall-clock time,
//! randomness, or platform handle participates; all types are bounded
//! and `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use std::fmt;

use crate::uitree::UiNode;

// ---------------------------------------------------------------------------
// Tier presets (candidate numbers)
// ---------------------------------------------------------------------------

/// Texture cap for [`BudgetTier::Essential`] (8 MiB).
pub const ESSENTIAL_TEXTURE_BYTES: u64 = 8 * 1024 * 1024;
/// Texture cap for [`BudgetTier::Standard`] (32 MiB).
pub const STANDARD_TEXTURE_BYTES: u64 = 32 * 1024 * 1024;
/// Texture cap for [`BudgetTier::Rich`] (64 MiB).
pub const RICH_TEXTURE_BYTES: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// Measured UI/GPU demand in the four budgeted dimensions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ResourceUsage {
    /// Retained tree nodes (see [`tree_nodes`]).
    pub nodes: usize,
    /// Texture memory in bytes (decoded images, glyph atlas pages).
    pub texture_bytes: u64,
    /// Blur surface area in pixels (radius-weighted area to composite).
    pub blur_area_px: u64,
    /// Draw calls to issue (one per canvas command, plus chrome).
    pub draw_calls: usize,
}

impl ResourceUsage {
    /// Builds a usage record from its four dimensions.
    #[must_use]
    pub const fn new(
        nodes: usize,
        texture_bytes: u64,
        blur_area_px: u64,
        draw_calls: usize,
    ) -> Self {
        Self {
            nodes,
            texture_bytes,
            blur_area_px,
            draw_calls,
        }
    }

    /// Zero demand.
    #[must_use]
    pub const fn zero() -> Self {
        Self::new(0, 0, 0, 0)
    }

    /// Draw-call demand for `commands` canvas commands (one each).
    #[must_use]
    pub const fn draws(commands: usize) -> Self {
        Self::new(0, 0, 0, commands)
    }

    /// Sums two usages, saturating instead of wrapping.
    #[must_use]
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            nodes: self.nodes.saturating_add(other.nodes),
            texture_bytes: self.texture_bytes.saturating_add(other.texture_bytes),
            blur_area_px: self.blur_area_px.saturating_add(other.blur_area_px),
            draw_calls: self.draw_calls.saturating_add(other.draw_calls),
        }
    }
}

/// Counts retained nodes in the subtree (including the root).
///
/// Per-tree accounting input for [`ResourceUsage::new`]: the node
/// dimension of a submission is the submitted tree's node count.
#[must_use]
pub fn tree_nodes(root: &UiNode) -> usize {
    root.count_nodes()
}

// ---------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------

/// Budgeted caps in the four usage dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResourceBudget {
    /// Maximum retained tree nodes.
    pub max_nodes: usize,
    /// Maximum texture memory in bytes.
    pub max_texture_bytes: u64,
    /// Maximum blur surface area in pixels.
    pub max_blur_area_px: u64,
    /// Maximum draw calls.
    pub max_draw_calls: usize,
}

impl ResourceBudget {
    /// Builds explicit caps (custom tiers outside [`BudgetTier`]).
    #[must_use]
    pub const fn new(
        max_nodes: usize,
        max_texture_bytes: u64,
        max_blur_area_px: u64,
        max_draw_calls: usize,
    ) -> Self {
        Self {
            max_nodes,
            max_texture_bytes,
            max_blur_area_px,
            max_draw_calls,
        }
    }

    /// Strict gate: accepts only usage inside every cap.
    ///
    /// # Errors
    ///
    /// Returns the first [`Overrun`] in fixed dimension order
    /// (nodes, texture, blur, draws) so reports are deterministic.
    pub fn check(&self, usage: ResourceUsage) -> Result<(), Overrun> {
        if usage.nodes > self.max_nodes {
            return Err(Overrun {
                dimension: BudgetDimension::Nodes,
                found: usage.nodes as u64,
                cap: self.max_nodes as u64,
            });
        }
        if usage.texture_bytes > self.max_texture_bytes {
            return Err(Overrun {
                dimension: BudgetDimension::TextureBytes,
                found: usage.texture_bytes,
                cap: self.max_texture_bytes,
            });
        }
        if usage.blur_area_px > self.max_blur_area_px {
            return Err(Overrun {
                dimension: BudgetDimension::BlurAreaPx,
                found: usage.blur_area_px,
                cap: self.max_blur_area_px,
            });
        }
        if usage.draw_calls > self.max_draw_calls {
            return Err(Overrun {
                dimension: BudgetDimension::DrawCalls,
                found: usage.draw_calls as u64,
                cap: self.max_draw_calls as u64,
            });
        }
        Ok(())
    }

    /// Admits usage with the refusal-vs-degradation rule.
    ///
    /// Structural overuse (nodes or texture) refuses fail-closed with
    /// every overrun dimension listed; blur-only or draw-only overuse
    /// degrades to the capped usage, which stays complete (reduced blur,
    /// batched draws) instead of partial.
    #[must_use]
    pub fn admit(&self, usage: ResourceUsage) -> Admission {
        let mut structural = Vec::new();
        if usage.nodes > self.max_nodes {
            structural.push(Overrun {
                dimension: BudgetDimension::Nodes,
                found: usage.nodes as u64,
                cap: self.max_nodes as u64,
            });
        }
        if usage.texture_bytes > self.max_texture_bytes {
            structural.push(Overrun {
                dimension: BudgetDimension::TextureBytes,
                found: usage.texture_bytes,
                cap: self.max_texture_bytes,
            });
        }
        if !structural.is_empty() {
            return Admission::Refused(structural);
        }
        let degraded = ResourceUsage {
            nodes: usage.nodes,
            texture_bytes: usage.texture_bytes,
            blur_area_px: usage.blur_area_px.min(self.max_blur_area_px),
            draw_calls: usage.draw_calls.min(self.max_draw_calls),
        };
        if degraded == usage {
            Admission::Accepted
        } else {
            Admission::Degraded(degraded)
        }
    }
}

/// Candidate preset budget tiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BudgetTier {
    /// Small surfaces: 256 nodes, 8 MiB texture, no blur, 128 draws.
    Essential,
    /// Everyday terminal chrome: 1024 nodes, 32 MiB, 512x512 blur, 512
    /// draws. Default.
    #[default]
    Standard,
    /// Heavy effects scenes: 2048 nodes, 64 MiB, 1024x1024 blur, 1024
    /// draws.
    Rich,
}

impl BudgetTier {
    /// Candidate vocabulary spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Essential => "essential",
            Self::Standard => "standard",
            Self::Rich => "rich",
        }
    }

    /// Parses a [`Self::as_str`] name. Case-sensitive; unknown inputs
    /// return `None` (no silent aliasing).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "essential" => Some(Self::Essential),
            "standard" => Some(Self::Standard),
            "rich" => Some(Self::Rich),
            _ => None,
        }
    }

    /// Caps for this tier (candidate numbers for the RFC to accept).
    #[must_use]
    pub const fn caps(self) -> ResourceBudget {
        match self {
            Self::Essential => ResourceBudget::new(256, ESSENTIAL_TEXTURE_BYTES, 0, 128),
            Self::Standard => ResourceBudget::new(1024, STANDARD_TEXTURE_BYTES, 512 * 512, 512),
            Self::Rich => ResourceBudget::new(2048, RICH_TEXTURE_BYTES, 1024 * 1024, 1024),
        }
    }
}

impl fmt::Display for BudgetTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Admission vocabulary
// ---------------------------------------------------------------------------

/// One budgeted dimension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BudgetDimension {
    /// Retained tree nodes.
    Nodes,
    /// Texture memory in bytes.
    TextureBytes,
    /// Blur surface area in pixels.
    BlurAreaPx,
    /// Draw calls.
    DrawCalls,
}

impl BudgetDimension {
    /// Candidate vocabulary spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nodes => "nodes",
            Self::TextureBytes => "texture-bytes",
            Self::BlurAreaPx => "blur-area-px",
            Self::DrawCalls => "draw-calls",
        }
    }
}

impl fmt::Display for BudgetDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One exceeded cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Overrun {
    /// The exceeded dimension.
    pub dimension: BudgetDimension,
    /// Measured demand.
    pub found: u64,
    /// The cap that was exceeded.
    pub cap: u64,
}

impl fmt::Display for Overrun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "budget overrun on {}: {} exceeds cap {}",
            self.dimension, self.found, self.cap
        )
    }
}

impl std::error::Error for Overrun {}

/// Admission outcome for a usage submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Inside every cap: render as submitted.
    Accepted,
    /// Blur/draw overuse only: render this capped usage instead (still
    /// complete, with reduced blur or batched draws).
    Degraded(ResourceUsage),
    /// Structural overuse (nodes or texture): render nothing new; the
    /// caller keeps the previous surface. Lists every structural
    /// overrun in fixed dimension order.
    Refused(Vec<Overrun>),
}

impl Admission {
    /// Whether the submission may render (accepted or degraded).
    #[must_use]
    pub const fn is_admitted(&self) -> bool {
        match self {
            Self::Accepted | Self::Degraded(_) => true,
            Self::Refused(_) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uitree::{UiNodeId, UiNodeKind};

    fn budget() -> ResourceBudget {
        BudgetTier::Standard.caps()
    }

    #[test]
    fn tier_spellings_round_trip() {
        for tier in [
            BudgetTier::Essential,
            BudgetTier::Standard,
            BudgetTier::Rich,
        ] {
            assert_eq!(BudgetTier::parse(tier.as_str()), Some(tier));
        }
        assert_eq!(BudgetTier::parse("EXTRA"), None);
        assert_eq!(BudgetTier::parse(""), None);
        assert_eq!(BudgetTier::default(), BudgetTier::Standard);
    }

    #[test]
    fn tiers_order_essential_below_standard_below_rich() {
        let caps = [
            BudgetTier::Essential.caps(),
            BudgetTier::Standard.caps(),
            BudgetTier::Rich.caps(),
        ];
        assert!(caps[0].max_nodes < caps[1].max_nodes);
        assert!(caps[1].max_nodes < caps[2].max_nodes);
        assert!(caps[0].max_texture_bytes < caps[1].max_texture_bytes);
        assert!(caps[1].max_texture_bytes < caps[2].max_texture_bytes);
        assert_eq!(caps[0].max_blur_area_px, 0);
    }

    #[test]
    fn inside_budget_is_accepted() {
        let usage = ResourceUsage::new(64, 1024, 1024, 32);
        assert_eq!(budget().admit(usage), Admission::Accepted);
        budget().check(usage).expect("inside every cap");
    }

    #[test]
    fn blur_only_overuse_degrades_to_cap() {
        let over = budget().max_blur_area_px.saturating_add(1);
        let usage = ResourceUsage::new(8, 0, over, 4);
        let admitted = budget().admit(usage);
        let expected = ResourceUsage::new(8, 0, budget().max_blur_area_px, 4);
        assert_eq!(admitted, Admission::Degraded(expected));
        assert!(admitted.is_admitted());
    }

    #[test]
    fn draw_only_overuse_degrades_to_cap() {
        let usage = ResourceUsage::new(8, 0, 0, budget().max_draw_calls.saturating_add(10));
        let admitted = budget().admit(usage);
        let expected = ResourceUsage::new(8, 0, 0, budget().max_draw_calls);
        assert_eq!(admitted, Admission::Degraded(expected));
    }

    #[test]
    fn node_overuse_refuses_fail_closed() {
        let usage = ResourceUsage::new(
            budget().max_nodes.saturating_add(1),
            0,
            budget().max_blur_area_px.saturating_add(1),
            0,
        );
        let admitted = budget().admit(usage);
        match admitted {
            Admission::Refused(overruns) => {
                assert_eq!(overruns.len(), 1);
                assert_eq!(overruns[0].dimension, BudgetDimension::Nodes);
            }
            other => panic!("node overuse must refuse, got {other:?}"),
        }
        assert!(!budget().admit(usage).is_admitted());
        let err = budget().check(usage).expect_err("over cap must fail");
        assert_eq!(err.dimension, BudgetDimension::Nodes);
    }

    #[test]
    fn texture_overuse_refuses_with_both_structural_overruns() {
        let usage = ResourceUsage::new(
            budget().max_nodes.saturating_add(2),
            budget().max_texture_bytes.saturating_add(1),
            0,
            0,
        );
        match budget().admit(usage) {
            Admission::Refused(overruns) => {
                assert_eq!(overruns.len(), 2);
                assert_eq!(overruns[0].dimension, BudgetDimension::Nodes);
                assert_eq!(overruns[1].dimension, BudgetDimension::TextureBytes);
            }
            other => panic!("structural overuse must refuse, got {other:?}"),
        }
    }

    #[test]
    fn usage_sums_saturate_and_tree_nodes_counts_root() {
        let root = UiNode::new(
            UiNodeId::new(1),
            UiNodeKind::Box,
            vec![UiNode::leaf(
                UiNodeId::new(2),
                UiNodeKind::Text("hi".to_string()),
            )],
        );
        assert_eq!(tree_nodes(&root), 2);
        let total = ResourceUsage::new(2, 0, 0, 0).saturating_add(ResourceUsage::draws(5));
        assert_eq!(total, ResourceUsage::new(2, 0, 0, 5));
        let saturated = ResourceUsage::new(usize::MAX, u64::MAX, u64::MAX, usize::MAX)
            .saturating_add(ResourceUsage::new(1, 1, 1, 1));
        assert_eq!(saturated.nodes, usize::MAX);
        assert_eq!(saturated.texture_bytes, u64::MAX);
    }

    #[test]
    fn overrun_display_is_human_readable() {
        let err = Overrun {
            dimension: BudgetDimension::DrawCalls,
            found: 600,
            cap: 512,
        };
        assert_eq!(
            err.to_string(),
            "budget overrun on draw-calls: 600 exceeds cap 512"
        );
    }
}
