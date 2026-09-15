//! Fragment-to-`RichBlock` projection (CTX-0439, text-first).
//!
//! The ingestion transport
//! ([`FragmentIngestService`](bitty_ipc::FragmentIngestService)) queues
//! bounded text chunks cut from one damage generation; the render side needs
//! scene blocks, not chunks. This module is that mapping: drained
//! [`RichFragment`](bitty_ipc::RichFragment)s project to one [`RichBlock`]
//! carrying the generation's text.
//!
//! # Generation/seq discipline (defined here, minimal)
//!
//! - One projection consumes fragments of exactly one `(terminal_id,
//!   generation)`; mixed terminals or generations fail closed (the caller
//!   groups the drain first).
//! - `seq` values must be unique (ingest dedups per queue, projection
//!   re-checks the slice); duplicates fail closed.
//! - Input order is normalized: the projection sorts by `seq` ascending,
//!   so drain order never affects the block.
//! - Gaps are tolerated and reported, not failed: `complete` is true only
//!   when the seqs form exactly `0..len`. A gapped block renders what it
//!   has; waiting for missing chunks is the caller's policy.
//! - Joined text must fit `scene::SCENE_MAX_TEXT_BYTES_PER_BLOCK`
//!   (256 KiB); overflow fails closed, never truncated (the transport
//!   already truncated+flagged per fragment — the projection must not
//!   silently drop producer bytes a second time).
//! - The block id is caller-supplied (projection allocates no ids), the
//!   anchor keys on projection order (`BlockAnchor::Zone(first_seq)`: an
//!   ordinal-identity anchor in the `blocks.rs` spirit, never a grid row),
//!   and `created_at` is caller-supplied (projection owns no clock).
//! - Line anchoring stays sequel work: fragments carry zone attribution,
//!   not rows. The block content is plain text; zone/line re-anchoring
//!   happens when the render side maps the block.
//! - Trust: fragments are untrusted observation data (`is_untrusted_surface`
//!   enforced per fragment); the projected block inherits that status by
//!   construction — callers must treat it as untrusted.
//!
//! The module is pure, bounded, headless, and `forbid(unsafe)`: no I/O, no
//! clock, no workspace dependency beyond the existing `bitty-ipc` edge. No
//! network, no new external crates.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use bitty_ipc::{IpcError, RichFragment};

use crate::scene::{
    BlockAnchor, BlockId, RichBlock, SCENE_MAX_TEXT_BYTES_PER_BLOCK, SceneError, SceneNode,
    ScrollBehavior, StyledSpan,
};

/// Projected scene block plus the discipline evidence for one generation.
#[derive(Debug, Clone)]
pub struct ProjectedBlock {
    /// Scene block carrying the generation's joined text.
    pub block: RichBlock,
    /// Terminal the fragments were cut from.
    pub terminal_id: String,
    /// Damage generation the fragments were cut from.
    pub generation: u64,
    /// First `seq` covered (anchor order).
    pub seq_start: u64,
    /// Last `seq` covered (anchor order).
    pub seq_end: u64,
    /// True only when seqs form exactly `0..len` (no gaps).
    pub complete: bool,
    /// True when any input fragment was transport-truncated.
    pub truncated: bool,
}

/// Projection failure (fail-closed, typed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    /// No fragments supplied.
    Empty,
    /// Fragments span more than one terminal.
    MixedTerminal {
        /// First terminal observed.
        first: String,
        /// Conflicting terminal observed.
        other: String,
    },
    /// Fragments span more than one generation.
    MixedGeneration {
        /// Owning terminal.
        terminal: String,
        /// First generation observed.
        first: u64,
        /// Conflicting generation observed.
        other: u64,
    },
    /// Two fragments share one `seq`.
    DuplicateSeq {
        /// Owning terminal.
        terminal: String,
        /// Owning generation.
        generation: u64,
        /// Duplicated chunk index.
        seq: u64,
    },
    /// A fragment violates its DTO budgets.
    InvalidFragment(IpcError),
    /// Joined text exceeds the per-block budget.
    TextTooLarge {
        /// Joined bytes.
        bytes: usize,
        /// Cap (`SCENE_MAX_TEXT_BYTES_PER_BLOCK`).
        cap: usize,
    },
    /// Scene construction rejected the content (defensive; unreachable
    /// after the text check — a single text node cannot breach SCN-1/2).
    Scene(SceneError),
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "no fragments to project"),
            Self::MixedTerminal { first, other } => {
                write!(f, "fragments span terminals '{first}' and '{other}'")
            }
            Self::MixedGeneration {
                terminal,
                first,
                other,
            } => write!(
                f,
                "fragments for '{terminal}' span generations {first} and {other}"
            ),
            Self::DuplicateSeq {
                terminal,
                generation,
                seq,
            } => write!(
                f,
                "duplicate seq {seq} for '{terminal}' generation {generation}"
            ),
            Self::InvalidFragment(error) => write!(f, "invalid fragment: {error}"),
            Self::TextTooLarge { bytes, cap } => {
                write!(f, "projected text too large: {bytes} > {cap}")
            }
            Self::Scene(error) => write!(f, "scene rejected projection: {error}"),
        }
    }
}

impl std::error::Error for ProjectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidFragment(error) => Some(error),
            Self::Scene(error) => Some(error),
            _ => None,
        }
    }
}

/// Project one generation's fragments to a scene block (text-first).
///
/// Sorts by `seq`, joins text verbatim (producers cut contiguous chunks),
/// and builds a single-text-span [`RichBlock`]. See the module docs for the
/// discipline contract.
///
/// # Errors
///
/// Returns the [`ProjectionError`] variant matching the first discipline
/// violation (empty, mixed terminal/generation, duplicate seq, invalid
/// fragment, over-budget text, scene rejection).
pub fn project_fragments(
    block_id: BlockId,
    owner: u64,
    created_at: u64,
    fragments: &[RichFragment],
) -> Result<ProjectedBlock, ProjectionError> {
    let first = fragments.first().ok_or(ProjectionError::Empty)?;
    for fragment in fragments {
        fragment
            .validate()
            .map_err(ProjectionError::InvalidFragment)?;
        if fragment.terminal_id != first.terminal_id {
            return Err(ProjectionError::MixedTerminal {
                first: first.terminal_id.clone(),
                other: fragment.terminal_id.clone(),
            });
        }
        if fragment.generation != first.generation {
            return Err(ProjectionError::MixedGeneration {
                terminal: first.terminal_id.clone(),
                first: first.generation,
                other: fragment.generation,
            });
        }
    }
    let mut seen = BTreeSet::new();
    for fragment in fragments {
        if !seen.insert(fragment.seq) {
            return Err(ProjectionError::DuplicateSeq {
                terminal: first.terminal_id.clone(),
                generation: first.generation,
                seq: fragment.seq,
            });
        }
    }
    let mut ordered: Vec<&RichFragment> = fragments.iter().collect();
    ordered.sort_by_key(|fragment| fragment.seq);
    let total: usize = ordered.iter().map(|fragment| fragment.text.len()).sum();
    if total > SCENE_MAX_TEXT_BYTES_PER_BLOCK {
        return Err(ProjectionError::TextTooLarge {
            bytes: total,
            cap: SCENE_MAX_TEXT_BYTES_PER_BLOCK,
        });
    }
    let mut text = String::with_capacity(total);
    for fragment in &ordered {
        text.push_str(&fragment.text);
    }
    let complete = ordered
        .iter()
        .enumerate()
        .all(|(index, fragment)| fragment.seq == index as u64);
    let truncated = ordered.iter().any(|fragment| fragment.truncated);
    let content = SceneNode::Text(StyledSpan {
        text,
        bold: false,
        italic: false,
    });
    let block = RichBlock::new(
        block_id,
        BlockAnchor::Zone(ordered.first().map_or(0, |fragment| fragment.seq)),
        content,
        ScrollBehavior::Inline,
        owner,
        first.generation,
        created_at,
    )
    .map_err(ProjectionError::Scene)?;
    Ok(ProjectedBlock {
        block,
        terminal_id: first.terminal_id.clone(),
        generation: first.generation,
        seq_start: ordered.first().map_or(0, |fragment| fragment.seq),
        seq_end: ordered.last().map_or(0, |fragment| fragment.seq),
        complete,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_ipc::MAX_FRAGMENT_TEXT_BYTES;

    fn fragment(terminal_id: &str, generation: u64, seq: u64, text: &str) -> RichFragment {
        RichFragment {
            terminal_id: terminal_id.to_owned(),
            generation,
            seq,
            zone: None,
            text: text.to_owned(),
            truncated: false,
            is_untrusted_surface: true,
        }
    }

    #[test]
    fn budgets_match_accepted_contracts() {
        assert_eq!(SCENE_MAX_TEXT_BYTES_PER_BLOCK, 256 * 1024);
        assert_eq!(MAX_FRAGMENT_TEXT_BYTES, 16 * 1024);
    }

    #[test]
    fn projects_single_generation_in_seq_order() {
        let fragments = vec![
            fragment("t:31", 7, 2, "c"),
            fragment("t:31", 7, 0, "a"),
            fragment("t:31", 7, 1, "b"),
        ];
        let projected = project_fragments(BlockId(11), 3, 99, &fragments).expect("projects");
        assert_eq!(projected.terminal_id, "t:31");
        assert_eq!(projected.generation, 7);
        assert_eq!(projected.block.generation(), 7);
        assert_eq!(projected.block.text_bytes(), 3);
        assert_eq!((projected.seq_start, projected.seq_end), (0, 2));
        assert!(projected.complete);
        assert!(!projected.truncated);
    }

    #[test]
    fn empty_fails_closed() {
        let error = project_fragments(BlockId(1), 0, 0, &[]).expect_err("empty must fail");
        assert_eq!(error, ProjectionError::Empty);
    }

    #[test]
    fn mixed_terminal_fails_closed() {
        let fragments = vec![fragment("t:31", 7, 0, "a"), fragment("t:32", 7, 1, "b")];
        let error = project_fragments(BlockId(1), 0, 0, &fragments).expect_err("mixed must fail");
        assert!(
            matches!(error, ProjectionError::MixedTerminal { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn mixed_generation_fails_closed() {
        let fragments = vec![fragment("t:31", 7, 0, "a"), fragment("t:31", 8, 1, "b")];
        let error = project_fragments(BlockId(1), 0, 0, &fragments).expect_err("mixed must fail");
        assert!(
            matches!(error, ProjectionError::MixedGeneration { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn duplicate_seq_fails_closed() {
        let fragments = vec![fragment("t:31", 7, 0, "a"), fragment("t:31", 7, 0, "b")];
        let error =
            project_fragments(BlockId(1), 0, 0, &fragments).expect_err("duplicate must fail");
        assert!(
            matches!(error, ProjectionError::DuplicateSeq { seq: 0, .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn gap_marks_incomplete_not_failure() {
        let fragments = vec![fragment("t:31", 7, 0, "a"), fragment("t:31", 7, 2, "c")];
        let projected = project_fragments(BlockId(1), 0, 0, &fragments).expect("gaps tolerate");
        assert!(!projected.complete);
        assert_eq!(projected.block.text_bytes(), 2);
        assert_eq!((projected.seq_start, projected.seq_end), (0, 2));
    }

    #[test]
    fn truncated_flag_propagates() {
        let mut flagged = fragment("t:31", 7, 1, "b");
        flagged.truncated = true;
        let fragments = vec![fragment("t:31", 7, 0, "a"), flagged];
        let projected = project_fragments(BlockId(1), 0, 0, &fragments).expect("projects");
        assert!(projected.truncated);
        assert!(projected.complete);
    }

    #[test]
    fn over_block_text_budget_fails_closed() {
        let chunk = "x".repeat(MAX_FRAGMENT_TEXT_BYTES);
        let fragments: Vec<RichFragment> = (0..17)
            .map(|seq| RichFragment {
                terminal_id: "t:31".to_owned(),
                generation: 7,
                seq,
                zone: None,
                text: chunk.clone(),
                truncated: true,
                is_untrusted_surface: true,
            })
            .collect();
        let error =
            project_fragments(BlockId(1), 0, 0, &fragments).expect_err("over-budget must fail");
        assert_eq!(
            error,
            ProjectionError::TextTooLarge {
                bytes: 17 * MAX_FRAGMENT_TEXT_BYTES,
                cap: SCENE_MAX_TEXT_BYTES_PER_BLOCK,
            }
        );
    }

    #[test]
    fn invalid_fragment_fails_closed() {
        let mut bad = fragment("t:31", 7, 0, "a\0b");
        bad.text.push('\0');
        let error = project_fragments(BlockId(1), 0, 0, &[bad]).expect_err("NUL must fail");
        assert!(
            matches!(error, ProjectionError::InvalidFragment(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn untrusted_label_is_required() {
        let mut bad = fragment("t:31", 7, 0, "a");
        bad.is_untrusted_surface = false;
        let error = project_fragments(BlockId(1), 0, 0, &[bad]).expect_err("label must hold");
        assert!(
            matches!(error, ProjectionError::InvalidFragment(_)),
            "got {error:?}"
        );
    }
}
