//! Target dispatch bridge (UX-32, U-8 hint family).
//!
//! Selecting a hint label dispatches a typed command id; the targeting mechanism never
//! implements actions internally. [`TargetDispatcher`] binds each label in
//! a [`HintAnnotationLayer`](crate::annotation_layer::HintAnnotationLayer)
//! to a [`QualifiedCommand`](crate::panel::QualifiedCommand) owned by the
//! workspace command registry, and [`TargetDispatcher::dispatch`]
//! revalidates the bound target against the live
//! [`TargetRegistry`](crate::targeting::TargetRegistry) before
//! returning the command id for the router to execute.
//!
//! The dispatcher executes nothing: it has no `apply`/`execute` path, takes
//! no callbacks, and mutates neither the registry nor the layer. Unknown
//! labels and stale targets fail closed. Bounded
//! ([`MAX_TARGET_BINDINGS`]), deterministic, headless, and
//! `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use crate::annotation_layer::HintAnnotationLayer;
use crate::panel::QualifiedCommand;
use crate::targeting::{TargetError, TargetRef, TargetRegistry};

/// Absolute cap on label bindings per dispatcher.
pub const MAX_TARGET_BINDINGS: usize = 1024;

/// Dispatch failure. All variants fail closed: the caller drops the hint
/// session, never guesses a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// No binding carries this label.
    UnknownLabel(String),
    /// The bound target went stale (retired or re-registered).
    StaleTarget(String),
    /// The dispatcher is at [`MAX_TARGET_BINDINGS`].
    TooManyBindings {
        /// Enforced cap.
        max: usize,
        /// Live bindings.
        current: usize,
    },
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownLabel(label) => write!(f, "unknown hint label: '{label}'"),
            Self::StaleTarget(detail) => write!(f, "stale target: {detail}"),
            Self::TooManyBindings { max, current } => {
                write!(f, "too many target bindings: max {max}, current {current}")
            }
        }
    }
}

impl std::error::Error for DispatchError {}

impl From<TargetError> for DispatchError {
    fn from(err: TargetError) -> Self {
        match err {
            TargetError::UnknownTarget(detail) | TargetError::StaleTarget(detail) => {
                Self::StaleTarget(detail)
            }
            TargetError::TooManyTargets { .. } => Self::StaleTarget(err.to_string()),
        }
    }
}

/// Label-to-command bridge for one hint session. Owns no actions: each
/// binding pairs a label with the typed [`QualifiedCommand`] id the
/// workspace router executes after [`TargetDispatcher::dispatch`] returns
/// it.
#[derive(Clone, Debug, Default)]
pub struct TargetDispatcher {
    bindings: HashMap<String, (TargetRef, QualifiedCommand)>,
}

impl TargetDispatcher {
    /// Creates an empty dispatcher.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bindings: HashMap::new(),
        }
    }

    /// Binds every annotation in `layer` to the command in `commands`
    /// (positional pairing). Lengths must agree; the dispatcher stores the
    /// typed ids only — no validation beyond that, no execution.
    pub fn bind_layer(
        &mut self,
        layer: &HintAnnotationLayer,
        commands: &[QualifiedCommand],
    ) -> Result<(), DispatchError> {
        if layer.len() != commands.len() {
            return Err(DispatchError::UnknownLabel(format!(
                "layer holds {} annotations but {} commands were supplied",
                layer.len(),
                commands.len()
            )));
        }
        if self.bindings.len() + layer.len() > MAX_TARGET_BINDINGS {
            return Err(DispatchError::TooManyBindings {
                max: MAX_TARGET_BINDINGS,
                current: self.bindings.len(),
            });
        }
        for (annotation, command) in layer.annotations().iter().zip(commands.iter()) {
            self.bindings.insert(
                annotation.label.clone(),
                (annotation.target, command.clone()),
            );
        }
        Ok(())
    }

    /// Number of bound labels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// True when no label is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Resolves `label` to the typed command id for the workspace router.
    /// Revalidates the bound target against `registry` first: unknown
    /// labels and stale targets fail closed. Executes nothing.
    pub fn dispatch(
        &self,
        label: &str,
        registry: &TargetRegistry,
    ) -> Result<QualifiedCommand, DispatchError> {
        let (target, command) = self
            .bindings
            .get(label)
            .ok_or_else(|| DispatchError::UnknownLabel(label.to_string()))?;
        registry.resolve(target).map_err(DispatchError::from)?;
        Ok(command.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Point, Rect};
    use crate::panel::CommandRegistry;
    use crate::panel::PanelId;
    use crate::targeting::LinkId;

    fn command(raw: &str) -> QualifiedCommand {
        QualifiedCommand::parse(raw).expect("command parses")
    }

    fn layer_two(registry: &mut TargetRegistry) -> HintAnnotationLayer {
        let first = registry.insert_link(LinkId::new(1)).expect("link");
        let second = registry.insert_panel(PanelId::new(2)).expect("panel");
        HintAnnotationLayer::build(
            &[TargetRef::Link(first), TargetRef::Panel(second)],
            &["a".to_string(), "g".to_string()],
            &[Point::new(1, 0), Point::new(70, 0)],
            Rect::new(0, 0, 80, 24),
        )
        .expect("layer")
    }

    #[test]
    fn dispatch_returns_typed_command_id() {
        let mut registry = TargetRegistry::new();
        let layer = layer_two(&mut registry);
        let mut dispatcher = TargetDispatcher::new();
        assert!(dispatcher.is_empty());
        dispatcher
            .bind_layer(
                &layer,
                &[
                    command("bitty.workspace:focus-panel"),
                    command("bitty.links:open-link"),
                ],
            )
            .expect("bind");
        assert_eq!(dispatcher.len(), 2);
        assert_eq!(
            dispatcher
                .dispatch("a", &registry)
                .expect("dispatch")
                .as_str(),
            "bitty.workspace:focus-panel"
        );
        assert_eq!(
            dispatcher
                .dispatch("g", &registry)
                .expect("dispatch")
                .as_str(),
            "bitty.links:open-link"
        );
    }

    #[test]
    fn unknown_label_fails_closed() {
        let mut registry = TargetRegistry::new();
        let layer = layer_two(&mut registry);
        let mut dispatcher = TargetDispatcher::new();
        dispatcher
            .bind_layer(
                &layer,
                &[
                    command("bitty.workspace:focus-panel"),
                    command("bitty.links:open-link"),
                ],
            )
            .expect("bind");
        assert!(matches!(
            dispatcher.dispatch("zz", &registry),
            Err(DispatchError::UnknownLabel(_))
        ));
        assert!(matches!(
            dispatcher.dispatch("", &registry),
            Err(DispatchError::UnknownLabel(_))
        ));
    }

    #[test]
    fn stale_target_fails_closed_despite_valid_label() {
        let mut registry = TargetRegistry::new();
        let layer = layer_two(&mut registry);
        let mut dispatcher = TargetDispatcher::new();
        dispatcher
            .bind_layer(
                &layer,
                &[
                    command("bitty.workspace:focus-panel"),
                    command("bitty.links:open-link"),
                ],
            )
            .expect("bind");
        assert!(registry.retire_link(LinkId::new(1)));
        assert!(matches!(
            dispatcher.dispatch("a", &registry),
            Err(DispatchError::StaleTarget(_))
        ));
        // The sibling binding is unaffected.
        assert!(dispatcher.dispatch("g", &registry).is_ok());
    }

    #[test]
    fn dispatch_executes_nothing_and_mutates_nothing() {
        let mut registry = TargetRegistry::new();
        let layer = layer_two(&mut registry);
        let mut dispatcher = TargetDispatcher::new();
        dispatcher
            .bind_layer(
                &layer,
                &[
                    command("bitty.workspace:focus-panel"),
                    command("bitty.links:open-link"),
                ],
            )
            .expect("bind");
        let before = registry.len();
        let annotation = layer.annotation_for_label("a").expect("label");
        let resolved = registry.resolve(&annotation.target).expect("live");
        assert_eq!(resolved, annotation.target);
        // Dispatch returns the id; the command registry owns execution.
        let mut commands = CommandRegistry::new();
        commands
            .register(PanelId::new(2), "bitty.links:open-link")
            .expect("register");
        let id = dispatcher.dispatch("g", &registry).expect("dispatch");
        assert_eq!(commands.owner_of(id.as_str()), Some(PanelId::new(2)));
        assert_eq!(registry.len(), before);
    }

    #[test]
    fn length_mismatch_rejected() {
        let mut registry = TargetRegistry::new();
        let layer = layer_two(&mut registry);
        let mut dispatcher = TargetDispatcher::new();
        assert!(matches!(
            dispatcher.bind_layer(&layer, &[command("bitty.workspace:focus-panel")]),
            Err(DispatchError::UnknownLabel(_))
        ));
        assert!(dispatcher.is_empty());
    }
}
