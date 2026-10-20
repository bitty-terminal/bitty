//! Hint annotation layer (UX-31, U-8 hint family).
//!
//! [`HintAnnotationLayer`] is the single batched annotation layer for a hint
//! session. It sits alongside the selection and IME layers at runtime
//! composition time: the runtime draws one batch for the whole session, never
//! one overlay per target. This module owns the batch data (target, kind,
//! workspace, screen rectangle, label, anchor, viewport), deterministic hit
//! testing for the annotated regions, and pruning; rasterization stays in
//! `bitty-render`, dispatch in [`crate::target_dispatch`].
//!
//! The layer is bounded ([`MAX_HINT_ANNOTATIONS`]), deterministic (input order
//! is preserved; a hit on overlapping regions resolves to the last painted
//! annotation), headless, and `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use crate::geometry::{Point, Rect};
use crate::target_provider::TargetKind;
use crate::targeting::{TargetRef, TargetRegistry, WorkspaceId};

/// Absolute cap on annotations per hint session.
pub const MAX_HINT_ANNOTATIONS: usize = 1024;

/// Maximum bytes of one hint label.
///
/// Allocator output is at most two ASCII characters; the bound stays generous
/// for policy-provided labels while keeping the batch text bounded.
pub const MAX_HINT_LABEL_BYTES: usize = 16;

/// One target/anchor request for a batched annotation.
///
/// The mediator issues the generation inside `target`; `workspace` carries the
/// owning workspace when the target is workspace-scoped, and `rect` is the
/// target's screen region in the annotation viewport's cell coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HintAnnotationSpec {
    /// Addressed target (generation handle from enumeration time).
    pub target: TargetRef,
    /// Owning workspace, when workspace-scoped.
    pub workspace: Option<WorkspaceId>,
    /// Screen region addressed by this annotation, in viewport cells.
    ///
    /// An empty rectangle is legal (for example a workspace with no visible
    /// geometry), but it is never hit-testable.
    pub rect: Rect,
}

/// Layer build failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnnotationLayerError {
    /// `specs`, `labels`, and `anchors` disagree in length.
    LengthMismatch {
        /// Number of target specs.
        targets: usize,
        /// Number of labels.
        labels: usize,
        /// Number of anchors.
        anchors: usize,
    },
    /// The session exceeds [`MAX_HINT_ANNOTATIONS`].
    TooManyAnnotations {
        /// Requested annotation count.
        requested: usize,
        /// Enforced cap.
        max: usize,
    },
    /// A label is empty, over [`MAX_HINT_LABEL_BYTES`], or contains
    /// whitespace/control characters.
    InvalidLabel {
        /// Rejected label.
        label: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// Two annotations carry the same label, so dispatch would be ambiguous.
    DuplicateLabel {
        /// Repeated label.
        label: String,
    },
}

impl std::fmt::Display for AnnotationLayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LengthMismatch {
                targets,
                labels,
                anchors,
            } => write!(
                f,
                "annotation layer length mismatch: {targets} targets, {labels} labels, {anchors} anchors"
            ),
            Self::TooManyAnnotations { requested, max } => write!(
                f,
                "too many hint annotations: requested {requested}, max {max}"
            ),
            Self::InvalidLabel { label, reason } => {
                write!(f, "invalid hint label '{label}': {reason}")
            }
            Self::DuplicateLabel { label } => {
                write!(f, "duplicate hint label '{label}'")
            }
        }
    }
}

impl std::error::Error for AnnotationLayerError {}

/// One batched hint entry: the target it addresses, its kind and workspace,
/// the screen region it occupies, the label drawn for it, and the
/// viewport-local cell anchor the label is drawn at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HintAnnotation {
    /// Addressed target (generation handle from enumeration time).
    pub target: TargetRef,
    /// Addressable surface kind.
    pub kind: TargetKind,
    /// Owning workspace, when workspace-scoped.
    pub workspace: Option<WorkspaceId>,
    /// Screen region addressed by the target, in viewport cells.
    pub rect: Rect,
    /// Hint label drawn at `anchor` (from [`crate::label_allocator`]).
    pub label: String,
    /// Viewport-local cell anchor for the label.
    pub anchor: Point,
}

/// Single batched annotation layer for one hint session.
///
/// The layer is the batch: `annotations` is the only per-target storage, so a
/// session of N targets is always exactly one layer, never N overlays. The
/// runtime composes this batch with the selection and IME layers; this crate
/// performs no drawing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HintAnnotationLayer {
    annotations: Vec<HintAnnotation>,
    viewport: Rect,
}

impl HintAnnotationLayer {
    /// Builds the session batch from plain target handles.
    ///
    /// Compatibility form for callers that only know the target and the label
    /// anchor: every entry carries no owning workspace and a one-cell screen
    /// region at its anchor. New code should call
    /// [`Self::build_anchored`] so the target's real screen rectangle and
    /// workspace travel with the annotation.
    ///
    /// # Errors
    ///
    /// [`AnnotationLayerError`] on length mismatch, overflow, or invalid or
    /// duplicate labels.
    pub fn build(
        targets: &[TargetRef],
        labels: &[String],
        anchors: &[Point],
        viewport: Rect,
    ) -> Result<Self, AnnotationLayerError> {
        let specs: Vec<HintAnnotationSpec> = targets
            .iter()
            .zip(anchors.iter())
            .map(|(target, anchor)| HintAnnotationSpec {
                target: *target,
                workspace: None,
                rect: Rect::new(anchor.x, anchor.y, 1, 1),
            })
            .collect();
        Self::build_anchored(&specs, labels, anchors, viewport)
    }

    /// Builds the session batch with full target anchors.
    ///
    /// `specs`, `labels`, and `anchors` must agree in length and the count must
    /// fit [`MAX_HINT_ANNOTATIONS`]; input order is preserved. Labels must be
    /// non-empty, bounded, whitespace/control-free, and unique so a typed label
    /// has exactly one target.
    ///
    /// # Errors
    ///
    /// [`AnnotationLayerError`] on length mismatch, overflow, or invalid or
    /// duplicate labels.
    pub fn build_anchored(
        specs: &[HintAnnotationSpec],
        labels: &[String],
        anchors: &[Point],
        viewport: Rect,
    ) -> Result<Self, AnnotationLayerError> {
        if specs.len() != labels.len() || specs.len() != anchors.len() {
            return Err(AnnotationLayerError::LengthMismatch {
                targets: specs.len(),
                labels: labels.len(),
                anchors: anchors.len(),
            });
        }
        if specs.len() > MAX_HINT_ANNOTATIONS {
            return Err(AnnotationLayerError::TooManyAnnotations {
                requested: specs.len(),
                max: MAX_HINT_ANNOTATIONS,
            });
        }
        let mut seen = HashSet::with_capacity(labels.len());
        let mut annotations = Vec::with_capacity(specs.len());
        for ((spec, label), anchor) in specs.iter().zip(labels.iter()).zip(anchors.iter()) {
            validate_label(label)?;
            if !seen.insert(label.as_str()) {
                return Err(AnnotationLayerError::DuplicateLabel {
                    label: label.clone(),
                });
            }
            annotations.push(HintAnnotation {
                target: spec.target,
                kind: TargetKind::of(&spec.target),
                workspace: spec.workspace,
                rect: spec.rect,
                label: label.clone(),
                anchor: *anchor,
            });
        }
        Ok(Self {
            annotations,
            viewport,
        })
    }

    /// Number of annotations in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.annotations.len()
    }

    /// True when the batch holds no annotation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.annotations.is_empty()
    }

    /// Viewport the anchors and rectangles are local to.
    #[must_use]
    pub fn viewport(&self) -> Rect {
        self.viewport
    }

    /// All annotations in build (paint) order.
    #[must_use]
    pub fn annotations(&self) -> &[HintAnnotation] {
        &self.annotations
    }

    /// Finds the annotation carrying `label`.
    #[must_use]
    pub fn annotation_for_label(&self, label: &str) -> Option<&HintAnnotation> {
        self.annotations
            .iter()
            .find(|annotation| annotation.label == label)
    }

    /// True when `label` is present in the batch.
    #[must_use]
    pub fn contains_label(&self, label: &str) -> bool {
        self.annotation_for_label(label).is_some()
    }

    /// Deterministic hit test over the annotated screen regions.
    ///
    /// Returns the annotation whose `rect` contains `point`. Annotations are
    /// painted in build order, so when regions overlap the **last** painted
    /// annotation wins, matching the compositor's topmost-first pointer rule.
    /// Empty rectangles never win and out-of-viewport points still hit a
    /// rectangle when it contains them; callers that need viewport clipping
    /// check [`Self::viewport`] first.
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<&HintAnnotation> {
        self.annotations
            .iter()
            .rev()
            .find(|annotation| !annotation.rect.is_empty() && annotation.rect.contains_point(point))
    }

    /// Drops the annotation carrying `label`, returning whether it existed.
    ///
    /// This is the mutation path used when a target disappears or is revoked:
    /// the label stops being visible and hit-testable immediately, and a
    /// dispatcher that follows [`crate::target_dispatch::TargetDispatcher::retain_presented`]
    /// drops its binding on the next presentation sync.
    pub fn drop_annotation(&mut self, label: &str) -> bool {
        let before = self.annotations.len();
        self.annotations
            .retain(|annotation| annotation.label != label);
        self.annotations.len() != before
    }

    /// Drops every annotation whose target no longer resolves in `registry`.
    ///
    /// Returns the number of annotations removed. Retired, re-registered, and
    /// never-registered handles all fail closed and are removed together.
    pub fn retain_live(&mut self, registry: &TargetRegistry) -> usize {
        let before = self.annotations.len();
        self.annotations
            .retain(|annotation| registry.resolve(&annotation.target).is_ok());
        before - self.annotations.len()
    }

    /// Drops all annotations, ending the session.
    pub fn clear(&mut self) {
        self.annotations.clear();
    }
}

/// Validates one hint label.
fn validate_label(label: &str) -> Result<(), AnnotationLayerError> {
    if label.is_empty() {
        return Err(AnnotationLayerError::InvalidLabel {
            label: label.to_string(),
            reason: "label must not be empty",
        });
    }
    if label.len() > MAX_HINT_LABEL_BYTES {
        return Err(AnnotationLayerError::InvalidLabel {
            label: label.to_string(),
            reason: "label exceeds the byte bound",
        });
    }
    if label
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(AnnotationLayerError::InvalidLabel {
            label: label.to_string(),
            reason: "label must not contain whitespace or control characters",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targeting::{LinkId, TargetRegistry};

    fn session(count: usize) -> (Vec<TargetRef>, Vec<String>, Vec<Point>) {
        let mut registry = TargetRegistry::new();
        let mut targets = Vec::with_capacity(count);
        for raw in 0..(count as u64) {
            let link = registry.insert_link(LinkId::new(raw)).expect("link");
            targets.push(TargetRef::Link(link));
        }
        let labels = (0..count).map(|i| format!("l{i}")).collect();
        let anchors = (0..count)
            .map(|i| Point::new((i % 80) as u16, (i / 80) as u16))
            .collect();
        (targets, labels, anchors)
    }

    fn anchored_session() -> (HintAnnotationLayer, TargetRegistry) {
        let mut registry = TargetRegistry::new();
        let panel = registry
            .insert_panel(crate::panel::PanelId::new(7))
            .expect("panel");
        let workspace = registry
            .insert_workspace(WorkspaceId::new(3))
            .expect("workspace");
        let specs = [
            HintAnnotationSpec {
                target: TargetRef::Panel(panel),
                workspace: Some(WorkspaceId::new(3)),
                rect: Rect::new(2, 1, 10, 5),
            },
            HintAnnotationSpec {
                target: TargetRef::Workspace(workspace),
                workspace: Some(WorkspaceId::new(3)),
                rect: Rect::new(20, 0, 8, 8),
            },
        ];
        let labels = vec!["a".to_string(), "s".to_string()];
        let anchors = vec![Point::new(3, 2), Point::new(21, 1)];
        let layer =
            HintAnnotationLayer::build_anchored(&specs, &labels, &anchors, Rect::new(0, 0, 80, 24))
                .expect("layer");
        (layer, registry)
    }

    #[test]
    fn one_layer_batches_many_targets() {
        let (targets, labels, anchors) = session(64);
        let layer =
            HintAnnotationLayer::build(&targets, &labels, &anchors, Rect::new(0, 0, 80, 24))
                .expect("layer");
        // Exactly one layer holds all 64 targets: no per-target overlay.
        assert_eq!(layer.len(), 64);
        assert!(!layer.is_empty());
        assert_eq!(layer.viewport(), Rect::new(0, 0, 80, 24));
        assert_eq!(layer.annotations().len(), 64);
        assert_eq!(
            layer.annotation_for_label("l7").expect("label").anchor,
            Point::new(7, 0)
        );
        assert!(layer.annotation_for_label("missing").is_none());
    }

    #[test]
    fn input_order_preserved() {
        let (targets, labels, anchors) = session(8);
        let layer =
            HintAnnotationLayer::build(&targets, &labels, &anchors, Rect::zero()).expect("layer");
        for (index, annotation) in layer.annotations().iter().enumerate() {
            assert_eq!(annotation.target, targets[index]);
            assert_eq!(annotation.label, labels[index]);
            assert_eq!(annotation.anchor, anchors[index]);
        }
    }

    #[test]
    fn length_mismatch_fails_closed() {
        let (targets, labels, anchors) = session(3);
        let err = HintAnnotationLayer::build(&targets[..2], &labels, &anchors, Rect::zero())
            .expect_err("mismatch must fail");
        assert!(matches!(err, AnnotationLayerError::LengthMismatch { .. }));
    }

    #[test]
    fn cap_enforced() {
        let (targets, labels, anchors) = session(8);
        let many_targets = vec![targets[0]; MAX_HINT_ANNOTATIONS + 1];
        let many_labels = vec![labels[0].clone(); MAX_HINT_ANNOTATIONS + 1];
        let many_anchors = vec![anchors[0]; MAX_HINT_ANNOTATIONS + 1];
        let err =
            HintAnnotationLayer::build(&many_targets, &many_labels, &many_anchors, Rect::zero())
                .expect_err("cap must hold");
        assert!(matches!(
            err,
            AnnotationLayerError::TooManyAnnotations { .. }
        ));
    }

    #[test]
    fn clear_ends_session() {
        let (targets, labels, anchors) = session(4);
        let mut layer =
            HintAnnotationLayer::build(&targets, &labels, &anchors, Rect::zero()).expect("layer");
        layer.clear();
        assert!(layer.is_empty());
        assert_eq!(layer.len(), 0);
    }

    #[test]
    fn anchored_fields_round_trip() {
        let (layer, _registry) = anchored_session();
        let panel = layer.annotation_for_label("a").expect("panel annotation");
        assert_eq!(panel.kind, TargetKind::Panel);
        assert_eq!(panel.workspace, Some(WorkspaceId::new(3)));
        assert_eq!(panel.rect, Rect::new(2, 1, 10, 5));
        let workspace = layer
            .annotation_for_label("s")
            .expect("workspace annotation");
        assert_eq!(workspace.kind, TargetKind::Workspace);
        assert_eq!(workspace.rect, Rect::new(20, 0, 8, 8));
    }

    #[test]
    fn hit_test_round_trips_region_to_label() {
        let (layer, _registry) = anchored_session();
        assert_eq!(
            layer.hit_test(Point::new(2, 1)).map(|a| a.label.as_str()),
            Some("a")
        );
        assert_eq!(
            layer.hit_test(Point::new(11, 5)).map(|a| a.label.as_str()),
            Some("a")
        );
        assert_eq!(
            layer.hit_test(Point::new(20, 0)).map(|a| a.label.as_str()),
            Some("s")
        );
        assert!(layer.hit_test(Point::new(0, 0)).is_none());
        assert!(layer.hit_test(Point::new(12, 1)).is_none());
    }

    #[test]
    fn overlapping_regions_resolve_last_painted() {
        let mut registry = TargetRegistry::new();
        let first = registry
            .insert_panel(crate::panel::PanelId::new(1))
            .expect("panel");
        let second = registry
            .insert_panel(crate::panel::PanelId::new(2))
            .expect("panel");
        let specs = [
            HintAnnotationSpec {
                target: TargetRef::Panel(first),
                workspace: None,
                rect: Rect::new(0, 0, 10, 10),
            },
            HintAnnotationSpec {
                target: TargetRef::Panel(second),
                workspace: None,
                rect: Rect::new(5, 5, 10, 10),
            },
        ];
        let labels = vec!["a".to_string(), "s".to_string()];
        let anchors = vec![Point::new(0, 0), Point::new(5, 5)];
        let layer = HintAnnotationLayer::build_anchored(&specs, &labels, &anchors, Rect::zero())
            .expect("layer");
        assert_eq!(
            layer.hit_test(Point::new(7, 7)).map(|a| a.label.as_str()),
            Some("s")
        );
        assert_eq!(
            layer.hit_test(Point::new(1, 1)).map(|a| a.label.as_str()),
            Some("a")
        );
    }

    #[test]
    fn duplicate_label_fails_closed() {
        let (targets, labels, anchors) = session(2);
        let duplicate = vec![labels[0].clone(), labels[0].clone()];
        let err = HintAnnotationLayer::build(&targets, &duplicate, &anchors, Rect::zero())
            .expect_err("duplicate labels must fail");
        assert!(matches!(err, AnnotationLayerError::DuplicateLabel { .. }));
    }

    #[test]
    fn invalid_labels_fail_closed() {
        let (targets, _labels, anchors) = session(1);
        for bad in [
            "",
            "has space",
            "line\nbreak",
            &"x".repeat(MAX_HINT_LABEL_BYTES + 1),
        ] {
            let err =
                HintAnnotationLayer::build(&targets, &[bad.to_string()], &anchors, Rect::zero())
                    .expect_err("invalid label must fail");
            assert!(matches!(err, AnnotationLayerError::InvalidLabel { .. }));
        }
        // The exact byte bound is accepted.
        HintAnnotationLayer::build(
            &targets,
            &["x".repeat(MAX_HINT_LABEL_BYTES)],
            &anchors,
            Rect::zero(),
        )
        .expect("boundary label accepted");
    }

    #[test]
    fn drop_annotation_removes_visibility_and_hit() {
        let (mut layer, _registry) = anchored_session();
        assert!(layer.drop_annotation("a"));
        assert!(!layer.drop_annotation("a"));
        assert!(!layer.contains_label("a"));
        assert!(layer.hit_test(Point::new(2, 1)).is_none());
        // The sibling annotation is untouched.
        assert!(layer.contains_label("s"));
    }

    #[test]
    fn retain_live_removes_stale_targets() {
        let (mut layer, mut registry) = anchored_session();
        assert!(registry.retire_workspace(WorkspaceId::new(3)));
        assert_eq!(layer.retain_live(&registry), 1);
        assert!(!layer.contains_label("s"));
        assert!(layer.contains_label("a"));
    }

    #[test]
    fn empty_rect_never_wins() {
        let mut registry = TargetRegistry::new();
        let panel = registry
            .insert_panel(crate::panel::PanelId::new(1))
            .expect("panel");
        let specs = [HintAnnotationSpec {
            target: TargetRef::Panel(panel),
            workspace: None,
            rect: Rect::zero(),
        }];
        let layer = HintAnnotationLayer::build_anchored(
            &specs,
            &["a".to_string()],
            &[Point::new(0, 0)],
            Rect::zero(),
        )
        .expect("layer");
        assert!(layer.hit_test(Point::new(0, 0)).is_none());
    }
}
