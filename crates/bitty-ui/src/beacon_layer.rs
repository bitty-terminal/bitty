//! Beacon annotation layer (UX-31, U-8 Beacon family).
//!
//! [`BeaconAnnotationLayer`] is the single batched GPU annotation layer for
//! a hint session. It sits alongside the selection and IME layers at
//! runtime composition time: the runtime draws one batch for the whole
//! session, never one overlay per target. This module owns only the batch
//! data (target, label, anchor, viewport); rasterization stays in
//! `bitty-render`, dispatch in [`crate::beacon_dispatch`].
//!
//! Bounded ([`MAX_BEACON_ANNOTATIONS`]), deterministic (input order is
//! preserved), headless, and `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use crate::beacon_target::TargetRef;
use crate::geometry::{Point, Rect};

/// Absolute cap on annotations per hint session.
pub const MAX_BEACON_ANNOTATIONS: usize = 1024;

/// Layer build failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnnotationLayerError {
    /// `targets`, `labels`, and `anchors` disagree in length.
    LengthMismatch {
        /// Number of targets.
        targets: usize,
        /// Number of labels.
        labels: usize,
        /// Number of anchors.
        anchors: usize,
    },
    /// The session exceeds [`MAX_BEACON_ANNOTATIONS`].
    TooManyAnnotations {
        /// Requested annotation count.
        requested: usize,
        /// Enforced cap.
        max: usize,
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
                "beacon layer length mismatch: {targets} targets, {labels} labels, {anchors} anchors"
            ),
            Self::TooManyAnnotations { requested, max } => write!(
                f,
                "too many beacon annotations: requested {requested}, max {max}"
            ),
        }
    }
}

impl std::error::Error for AnnotationLayerError {}

/// One batched hint entry: the target it addresses, the label drawn for
/// it, and the viewport-local cell anchor the label is drawn at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeaconAnnotation {
    /// Addressed target (generation handle from enumeration time).
    pub target: TargetRef,
    /// Hint label drawn at `anchor` (from [`crate::beacon_label`]).
    pub label: String,
    /// Viewport-local cell anchor for the label.
    pub anchor: Point,
}

/// Single batched annotation layer for one hint session.
///
/// The layer is the batch: `annotations` is the only per-target storage,
/// so a session of N targets is always exactly one layer, never N
/// overlays. The runtime composes this batch with the selection and IME
/// layers; this crate performs no drawing.
#[derive(Clone, Debug, Default)]
pub struct BeaconAnnotationLayer {
    annotations: Vec<BeaconAnnotation>,
    viewport: Rect,
}

impl BeaconAnnotationLayer {
    /// Builds the session batch. `targets`, `labels`, and `anchors` must
    /// agree in length and the count must fit
    /// [`MAX_BEACON_ANNOTATIONS`]; input order is preserved.
    pub fn build(
        targets: &[TargetRef],
        labels: &[String],
        anchors: &[Point],
        viewport: Rect,
    ) -> Result<Self, AnnotationLayerError> {
        if targets.len() != labels.len() || targets.len() != anchors.len() {
            return Err(AnnotationLayerError::LengthMismatch {
                targets: targets.len(),
                labels: labels.len(),
                anchors: anchors.len(),
            });
        }
        if targets.len() > MAX_BEACON_ANNOTATIONS {
            return Err(AnnotationLayerError::TooManyAnnotations {
                requested: targets.len(),
                max: MAX_BEACON_ANNOTATIONS,
            });
        }
        let annotations = targets
            .iter()
            .zip(labels.iter())
            .zip(anchors.iter())
            .map(|((target, label), anchor)| BeaconAnnotation {
                target: *target,
                label: label.clone(),
                anchor: *anchor,
            })
            .collect();
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

    /// Viewport the anchors are local to.
    #[must_use]
    pub fn viewport(&self) -> Rect {
        self.viewport
    }

    /// All annotations in build order.
    #[must_use]
    pub fn annotations(&self) -> &[BeaconAnnotation] {
        &self.annotations
    }

    /// Finds the annotation carrying `label`.
    #[must_use]
    pub fn annotation_for_label(&self, label: &str) -> Option<&BeaconAnnotation> {
        self.annotations
            .iter()
            .find(|annotation| annotation.label == label)
    }

    /// Drops all annotations, ending the session.
    pub fn clear(&mut self) {
        self.annotations.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beacon_target::{LinkId, TargetRegistry};

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

    #[test]
    fn one_layer_batches_many_targets() {
        let (targets, labels, anchors) = session(64);
        let layer =
            BeaconAnnotationLayer::build(&targets, &labels, &anchors, Rect::new(0, 0, 80, 24))
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
            BeaconAnnotationLayer::build(&targets, &labels, &anchors, Rect::zero()).expect("layer");
        for (index, annotation) in layer.annotations().iter().enumerate() {
            assert_eq!(annotation.target, targets[index]);
            assert_eq!(annotation.label, labels[index]);
            assert_eq!(annotation.anchor, anchors[index]);
        }
    }

    #[test]
    fn length_mismatch_fails_closed() {
        let (targets, labels, anchors) = session(3);
        let err = BeaconAnnotationLayer::build(&targets[..2], &labels, &anchors, Rect::zero())
            .expect_err("mismatch must fail");
        assert!(matches!(err, AnnotationLayerError::LengthMismatch { .. }));
    }

    #[test]
    fn cap_enforced() {
        let (targets, labels, anchors) = session(8);
        let many_targets = vec![targets[0]; MAX_BEACON_ANNOTATIONS + 1];
        let many_labels = vec![labels[0].clone(); MAX_BEACON_ANNOTATIONS + 1];
        let many_anchors = vec![anchors[0]; MAX_BEACON_ANNOTATIONS + 1];
        let err =
            BeaconAnnotationLayer::build(&many_targets, &many_labels, &many_anchors, Rect::zero())
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
            BeaconAnnotationLayer::build(&targets, &labels, &anchors, Rect::zero()).expect("layer");
        layer.clear();
        assert!(layer.is_empty());
        assert_eq!(layer.len(), 0);
    }
}
