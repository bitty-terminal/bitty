//! The panel generation edge: the narrow port through which the plugin
//! generation boundary reaches the panel host (bitty#2476 residuals 2 and 4).
//!
//! # Why a port and not a handle
//!
//! `PanelRuntime` is app-owned: the composition root constructs it, installs it
//! into the plugin session's `bitty.panel.create` backend and the `SceneHost`,
//! and keeps the same `Rc` for the present loop
//! (`crates/bitty-terminal/src/plugin_runtime.rs`). The plugin runtime therefore
//! cannot name a registry, and the production `rebind_panel` and
//! `present_placeholder` had no caller — the state residual 2 and residual 4
//! were merged in.
//!
//! This module is the seam that closes that: a two-method trait the app
//! implements over its own panel host and installs with
//! [`PluginRuntime::set_panel_generation_port`](super::PluginRuntime::set_panel_generation_port),
//! following the `SceneHost` precedent (also an app-owned installer passed into
//! the plugin runtime at the composition root).
//!
//! The port deliberately exposes **two questions and nothing else**: *bind this
//! successor onto the containers my predecessor still owns*, and *release this
//! ended generation and present the placeholder on the containers it left
//! unbound*. It hands over no registry, no renderer, no scene, no capability
//! token from presentation data, and no panel identifier the plugin can choose.
//! The plugin runtime decides *when* an edge runs and *which* owner and
//! generation it is; the app decides *which containers that names*, because
//! only it holds the panel records.
//!
//! # Fail-closed contract
//!
//! An implementation must move nothing on a refusal. A refused
//! [`PanelGenerationPort::bind_successor`] leaves the panel's binding, lease
//! tenure, view attachment, grant rows, and retired-ceiling tombstone exactly
//! as they were, so the panel ends **unbound** with every verb refused — never
//! partially rebound, and never with a widened grant set. That is the bind's own
//! contract (`PanelRegistry::rebind_panel` /
//! [`PanelRuntime::rebind_panel`](crate::registry::PanelRuntime::rebind_panel),
//! OQ-084 D.2), and the reference implementation in `bitty-terminal` satisfies it
//! by delegating rather than by reconstructing it.
//!
//! A refused placeholder presentation likewise changes nothing: the reason is
//! refused before any panel state moves, so the container keeps whatever state
//! it held.
//!
//! # Ordering obligations this port does not enforce
//!
//! OQ-084 D.2 leaves phase-1 steps 3 (capture release and overlay-capture
//! revoke) and 5 (in-flight verb cancellation) to the plugin runtime's
//! generation edge. [`PluginRuntime`](super::PluginRuntime) discharges them
//! before it calls the port: `dispose_inner` and `suspend_inner` run the
//! generation's content release (`clear_ui_blocks`) first, and `reload_inner`
//! enters `dispose_inner` before it activates a successor — so a bind the port
//! is asked to perform is never asked for a predecessor whose edge has not run.
//! What the port may rely on:
//!
//! 1. [`PanelGenerationPort::release_generation`] is called at a terminal edge
//!    (dispose, unload, disable), after the generation's own content release,
//!    and before any successor exists.
//! 2. [`PanelGenerationPort::bind_successor`] is called after that release
//!    returned and after the successor generation activated.
//! 3. The release completes before any placeholder becomes visible: the
//!    reference implementation reaches it through the registry's
//!    `present_placeholder`, which runs the unbind release and only then writes
//!    the placeholder (OQ-084 A.2).
//!
//! # Optional by construction
//!
//! The port is `None` until the app installs one, so runtime tests and headless
//! builds behave exactly as before: the edge releases nothing and presents
//! nothing, and only the generation's own content release runs.

use std::collections::BTreeSet;

use bitty_plugin_host::PluginId;
use bitty_ui::panel_recovery::PlaceholderReason;

use crate::registry::{Generation, PanelError, PanelId};

/// A plugin activation generation.
///
/// This is the counter `PluginRuntime` advances per activation and stamps on
/// the generation's services as `ui_epoch` — the *controller* generation OQ-084
/// distinguishes from the panel allocation epoch. It is a `u32` on the plugin
/// side and is never the panel registry's own counter.
pub type PluginGeneration = u32;

/// Base of the controller-generation domain inside the panel registry's
/// [`Generation`] space.
///
/// The panel registry's `Generation` names two unrelated counters: the
/// *allocation epoch* it mints for every panel (and re-mints on disposal), and
/// the *controller* generation a bind writes. OQ-084 A.3 requires they never be
/// compared, but the registry refuses a bind that presents the allocation epoch
/// as its successor (`PanelError::ControllerConflict`, B.1/B.2) precisely
/// because a value that is both would collapse the two facts. A raw plugin
/// generation is a small integer, so it collides with an allocation epoch
/// whenever the two counters happen to be equal — the second activation of a
/// plugin whose first panel was the registry's first allocation is the ordinary
/// case, not a corner.
///
/// [`controller_generation`] therefore places every controller generation in a
/// domain no allocation epoch can reach: the counter above `u32::MAX`. The
/// mapping is total and injective, so two plugin generations never name one
/// controller generation and a controller generation never names an allocation
/// epoch.
pub const CONTROLLER_GENERATION_BASE: u64 = u32::MAX as u64 + 1;

/// Maps a plugin activation generation into the panel registry's controller
/// generation domain.
///
/// This is the only conversion between the two counters in the tree; the port's
/// caller and its implementation must both use it, so a panel record, a grant
/// row, and a retired-ceiling tombstone keyed by the result can never be
/// confused with the panel's allocation epoch (see
/// [`CONTROLLER_GENERATION_BASE`]).
#[must_use]
pub const fn controller_generation(plugin_generation: PluginGeneration) -> Generation {
    Generation(CONTROLLER_GENERATION_BASE + plugin_generation as u64)
}

/// The unbind edge of one plugin generation (bitty#2476 residuals 4 and 5).
///
/// The generation ended with no successor: it was disposed, unloaded, disabled,
/// or it failed. The port releases the containers it still drives and presents
/// the Decision E placeholder with `reason`.
#[derive(Clone, Copy, Debug)]
pub struct PanelEdgeRelease<'a> {
    /// The plugin whose generation ended.
    pub owner: &'a PluginId,
    /// The activation generation that ended.
    pub ended: PluginGeneration,
    /// The closed Decision E reason the placeholder names. Taken from
    /// [`PlaceholderReason`] rather than a free string, so a caller cannot hand
    /// the registry a token outside the vocabulary.
    pub reason: PlaceholderReason,
}

/// The successor bind of one plugin generation (bitty#2476 residual 2).
///
/// The predecessor's edge has already run: its content, capture session, and
/// parked command are released, and the panel containers it drove are still
/// standing. The port binds the successor onto them without disposing them and
/// without re-minting a `PanelId` (OQ-084 A.1, A.4).
#[derive(Clone, Copy, Debug)]
pub struct PanelEdgeBind<'a> {
    /// The plugin that owns both generations. The at-most-one-controller rule
    /// and the same-owner rule of OQ-084 A.2 mean a bind never crosses owners
    /// (B.6 is undecided).
    pub owner: &'a PluginId,
    /// The activation generation whose edge just ran.
    pub predecessor: PluginGeneration,
    /// The activation generation that just activated.
    pub successor: PluginGeneration,
    /// The successor's **own** resolution — the panel capability families this
    /// generation was granted at activation (D.1's `R`). Never the
    /// predecessor's set and never a union of the two.
    pub requested: &'a BTreeSet<String>,
    /// The permission-diff consents granted **for this binding** (D.1's `C`).
    /// Empty when no consent surface answered, which denies every widening
    /// (PGO-17) rather than granting it.
    pub consented: &'a BTreeSet<String>,
}

/// What one edge call did.
///
/// One report per edge, replaced at the next edge: the edge never accumulates a
/// row per reload (OQ-084 A.2 forbids that growth on the registry side, and
/// this value is not retained beyond the plugin runtime's single most recent
/// one).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PanelEdgeReport {
    /// Containers the edge attempted. Bounded by the panel registry's live
    /// panel count, which is itself bounded by `max_panels_per_window`.
    pub attempted: usize,
    /// Containers the edge moved: bound to the successor, or left unbound with
    /// the placeholder presented.
    pub committed: Vec<PanelId>,
    /// Containers the edge refused, with the typed refusal and nothing moved.
    pub refused: Vec<(PanelId, PanelError)>,
}

impl PanelEdgeReport {
    /// Containers the edge refused.
    #[must_use]
    pub fn refused_count(&self) -> usize {
        self.refused.len()
    }

    /// Whether the edge found no container to act on. A generation that never
    /// bound a controller leaves the panel host untouched, which is not a
    /// failure: `bitty.panel.create` containers carry no owner until a bind
    /// writes one (OQ-084 follow-up (e)).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.attempted == 0
    }
}

/// The panel host, as the plugin generation edge can reach it.
///
/// Two methods, one per edge, and no state: the implementation owns the panel
/// host and answers for the containers a generation drives. See the
/// [module documentation](self) for the ordering the caller guarantees and the
/// fail-closed contract an implementation must honour.
///
/// Implementations are installed through
/// [`PluginRuntime::set_panel_generation_port`](super::PluginRuntime::set_panel_generation_port)
/// and are `!Send` like every other panel handle, so the edge runs on the
/// runtime's owning thread and no panel mutation crosses a thread boundary.
pub trait PanelGenerationPort {
    /// Releases the ended generation on every container it still drives and
    /// presents the placeholder naming `edge.reason` on each.
    ///
    /// The release covers the generation's panel-scoped grant row (after
    /// writing the retired-ceiling tombstone), its queued bus events, the panel
    /// commands it registered, and its attributed overlays; the placeholder
    /// becomes visible only after that release completed. A container whose
    /// presentation then fails is reported in
    /// [`PanelEdgeReport::refused`] and is left unbound.
    fn release_generation(&self, edge: PanelEdgeRelease<'_>) -> PanelEdgeReport;

    /// Binds `edge.successor` onto every container `edge.predecessor` still
    /// holds, keeping the container, its `PanelId`, its attachment, its title,
    /// its geometry, and its workspace membership (OQ-084 A.4, D.3).
    ///
    /// The install is `(R ∩ P) ∪ C` from `edge.requested`, the predecessor's
    /// captured ceiling `P`, and `edge.consented`; the binding, the lease
    /// reset, and the tombstone drop then happen in D.2 phase 2's order. A
    /// refusal moves nothing.
    fn bind_successor(&self, edge: PanelEdgeBind<'_>) -> PanelEdgeReport;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The controller-generation domain is disjoint from every allocation epoch
    /// a `u64` registry counter can mint before it exhausts, and injective over
    /// the plugin generation space.
    ///
    /// Fails for a pass-through mapping (`Generation(u64::from(g))`), which
    /// makes the ordinary reload of the first panel a `ControllerConflict`, and
    /// for any mapping that folds two plugin generations onto one value.
    #[test]
    fn controller_generations_never_name_an_allocation_epoch() {
        assert_eq!(controller_generation(0).get(), CONTROLLER_GENERATION_BASE);
        assert_eq!(
            controller_generation(1).get(),
            CONTROLLER_GENERATION_BASE + 1
        );
        assert_eq!(
            controller_generation(u32::MAX).get(),
            CONTROLLER_GENERATION_BASE + u64::from(u32::MAX)
        );
        // The registry starts its counter at `Generation::INITIAL` and mints one
        // value per allocation and per disposal, all strictly below the base
        // until the `u64` counter would need more than `u32::MAX` steps.
        assert!(controller_generation(0) > Generation::INITIAL);
        assert!(
            controller_generation(0).get() > u64::from(u32::MAX),
            "no allocation epoch the u32-bounded panel count can reach is in this domain"
        );
        // Injective: distinct plugin generations never share a controller value.
        let mut seen = BTreeSet::new();
        for generation in [0_u32, 1, 2, 7, 4096, u32::MAX - 1, u32::MAX] {
            assert!(
                seen.insert(controller_generation(generation)),
                "distinct plugin generations map to distinct controller generations"
            );
        }
    }
}
