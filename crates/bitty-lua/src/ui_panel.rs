//! The closed `spec` table of `bitty.ui.panel.present` (bitty#2388, CTX-1909).
//!
//! Contract source: `specifications/panel-gui-content-presentation-candidate.md`
//! in `bitty-terminal-docs` (draft candidate, decision-ready), section
//! "Recommended API shape (candidate v2)".
//!
//! The record closes the descriptor at two fields, on purpose:
//!
//! | field | values | authority |
//! | --- | --- | --- |
//! | `placement` | `tiled`, `floating`, `fullscreen`, `scratchpad`; `band` invalid | hint only |
//! | `restore` | `previous` only | none (fixed) |
//!
//! There is deliberately no `mode`, `exit_key`, `owner`, `panel`,
//! coordinates, size, or surface handle: a Panel is a container, and
//! *placement* is the panel/user axis while the plugin only supplies
//! *content*. Anything else in the table is refused with the typed
//! `E_UI_COMPONENT_INVALID` instead of being ignored, because an ignored key
//! is a plugin author believing they declared an authority they do not hold.
//!
//! `placement` is recorded and never applied in v1: the panel keeps the
//! placement the user gave it. `restore` has exactly one value — the
//! displaced terminal is always restored — so it exists to make the default
//! explicit rather than to offer a choice.

use phodopus::Value;

use crate::host::{BridgeError, LuaValue, MarshallingLimits};
use crate::ui::component_invalid;

/// Top-level keys the closed `spec` table admits.
///
/// Exported so the refusal message and the parser read the same list: a key
/// outside it is a typed refusal, never a silently ignored field.
pub const UI_PANEL_SPEC_FIELDS: [&str; 2] = ["placement", "restore"];

/// Placements the record admits as a **hint** (`band` is not panel content).
pub const UI_PANEL_PLACEMENTS: [&str; 4] = ["tiled", "floating", "fullscreen", "scratchpad"];

/// The one accepted `restore` value.
pub const UI_PANEL_RESTORE_PREVIOUS: &str = "previous";

/// Release dispositions `bitty.ui.panel.release(handle, reason)` accepts.
///
/// `None` (Lua `nil`) defaults to [`UI_PANEL_RELEASE_DEFAULT_REASON`]. The set
/// is closed for the same reason `bitty.ui.overlay.release` closes its own:
/// the reason is recorded for the release audit row, so a free-form string
/// would either leak unbounded content into a diagnostic or silently become a
/// reason nobody can classify. Refusing it keeps the record honest.
pub const UI_PANEL_RELEASE_REASONS: [&str; 2] = ["released", "cancelled"];

/// Reason recorded when the caller supplies none.
pub const UI_PANEL_RELEASE_DEFAULT_REASON: &str = "released";

/// Maximum bytes of a caller-supplied release reason.
///
/// The closed set is checked before this bound matters; the bound exists so
/// the refusal path is bounded too (the message never echoes an unbounded
/// value).
pub const UI_PANEL_REASON_MAX_BYTES: usize = 64;

/// Maximum queued input events per presented layer (bitty#2388 D7/Bounds).
///
/// The queue is Core-owned and lives with the presented layer: overflow drops
/// the oldest event, counts it, and raises the poll result's sticky
/// `overflowed` flag. It is never a plugin-visible callback queue and never
/// grows past this bound, so a plugin that stops polling cannot exhaust host
/// memory (security invariant 7, "bounded inputs").
pub const UI_PANEL_INPUT_QUEUE_MAX: usize = 256;

/// Default `bitty.ui.panel.poll(handle)` drain size.
///
/// The record fixes a cheap default so one call cannot hand a plugin the whole
/// queue; a caller may ask for more up to [`UI_PANEL_INPUT_QUEUE_MAX`].
pub const UI_PANEL_INPUT_POLL_MAX_DEFAULT: usize = 64;

/// Maximum bytes of one queued input event's text (bitty#2388 "Bounds").
///
/// Shared with the focusable-overlay capture bound: one bounded key payload is
/// 4096 bytes, truncated at a `char` boundary before it is enqueued.
pub const UI_PANEL_INPUT_TEXT_MAX_BYTES: usize = 4096;

/// Bounded marshalling limits for the flat `spec` table.
///
/// The table is flat and carries at most two scalar fields, so a depth of one
/// level and a handful of nodes is the whole contract; anything deeper is a
/// table the record does not describe.
const PANEL_SPEC_LIMITS: MarshallingLimits = MarshallingLimits {
    max_depth: 2,
    max_nodes: 8,
    max_bytes: 512,
};

/// A placement the plugin may *ask for* as a hint (never an authority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UiPanelPlacement {
    /// The panel's own solver slot.
    Tiled,
    /// A bounded float over the workspace.
    Floating,
    /// Full-window layer.
    Fullscreen,
    /// Scratchpad slot.
    Scratchpad,
}

impl UiPanelPlacement {
    /// Parse one accepted spelling; `None` for anything else (including the
    /// record's explicit `band` refusal).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "tiled" => Some(Self::Tiled),
            "floating" => Some(Self::Floating),
            "fullscreen" => Some(Self::Fullscreen),
            "scratchpad" => Some(Self::Scratchpad),
            _ => None,
        }
    }
}

/// The restore half of the descriptor. One value by contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UiPanelRestore {
    /// Put the displaced terminal back (the only accepted behaviour).
    #[default]
    Previous,
}

/// The validated, closed `spec` table of `bitty.ui.panel.present`.
///
/// Absent fields keep their defaults: no placement hint, and `restore =
/// "previous"`. A caller may omit the table entirely — that is the same as
/// `{}`, which is the v1 behaviour rather than a second contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiPanelSpec {
    /// Placement hint, recorded and never applied in v1.
    pub placement: Option<UiPanelPlacement>,
    /// Restore behaviour (fixed to [`UiPanelRestore::Previous`]).
    pub restore: UiPanelRestore,
}

impl UiPanelSpec {
    /// A spec with no hint and the fixed restore.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            placement: None,
            restore: UiPanelRestore::Previous,
        }
    }
}

/// Read one `spec` argument into the closed model.
///
/// `nil` yields the default spec (no hint, fixed restore). Any other value
/// must be a flat table whose keys are all in [`UI_PANEL_SPEC_FIELDS`];
/// unknown or extra keys, an unreadable value, a `placement` outside
/// [`UI_PANEL_PLACEMENTS`] (including `band`), and a `restore` other than
/// `previous` are all typed `E_UI_COMPONENT_INVALID` refusals that change
/// nothing.
///
/// # Errors
///
/// [`BridgeError`] with `validation` class and the `E_UI_COMPONENT_INVALID`
/// code; the message names the offending key or value, never its content
/// beyond a bounded echo.
pub fn read_panel_spec(value: Value<'_>) -> Result<UiPanelSpec, BridgeError> {
    let marshalled = LuaValue::from_lua(value, PANEL_SPEC_LIMITS).map_err(|error| {
        component_invalid(format!("ui.panel spec is not bounded data: {error}"))
    })?;
    let pairs = match &marshalled {
        LuaValue::Nil => return Ok(UiPanelSpec::new()),
        LuaValue::Table(pairs) => pairs,
        _ => {
            return Err(component_invalid(
                "ui.panel spec must be a table (or nil for the defaults)",
            ));
        }
    };
    let mut spec = UiPanelSpec::new();
    for (key, value) in pairs {
        let LuaValue::String(key) = key else {
            return Err(component_invalid(format!(
                "ui.panel spec keys must be strings, one of {:?}",
                UI_PANEL_SPEC_FIELDS
            )));
        };
        match key.as_str() {
            "placement" => spec.placement = read_placement(value)?,
            "restore" => spec.restore = read_restore(value)?,
            other => {
                return Err(component_invalid(format!(
                    "ui.panel spec does not accept '{other}'; the closed set is {:?}",
                    UI_PANEL_SPEC_FIELDS
                )));
            }
        }
    }
    Ok(spec)
}

/// Read the placement hint; `nil` means "no hint".
fn read_placement(value: &LuaValue) -> Result<Option<UiPanelPlacement>, BridgeError> {
    match value {
        LuaValue::Nil => Ok(None),
        LuaValue::String(raw) => UiPanelPlacement::parse(raw).map(Some).ok_or_else(|| {
            component_invalid(format!(
                "ui.panel spec placement '{raw}' is not one of {:?} (band is not panel content)",
                UI_PANEL_PLACEMENTS
            ))
        }),
        _ => Err(component_invalid(
            "ui.panel spec placement must be a string",
        )),
    }
}

/// Read the restore field; only `previous` is accepted.
fn read_restore(value: &LuaValue) -> Result<UiPanelRestore, BridgeError> {
    match value {
        LuaValue::Nil => Ok(UiPanelRestore::Previous),
        LuaValue::String(raw) if raw == UI_PANEL_RESTORE_PREVIOUS => Ok(UiPanelRestore::Previous),
        LuaValue::String(raw) => Err(component_invalid(format!(
            "ui.panel spec restore '{raw}' is not accepted; the only value is '{UI_PANEL_RESTORE_PREVIOUS}'"
        ))),
        _ => Err(component_invalid("ui.panel spec restore must be a string")),
    }
}

/// Validate a caller-supplied release disposition.
///
/// `None` (Lua `nil`) is the documented default; any other value must be in
/// [`UI_PANEL_RELEASE_REASONS`]. The check runs before the host call, so a
/// refused reason leaves the presented layer exactly as it was.
///
/// # Errors
///
/// [`BridgeError`] with `validation` class and the `E_UI_COMPONENT_INVALID`
/// code.
pub fn read_panel_release_reason(reason: Option<&str>) -> Result<&str, BridgeError> {
    match reason {
        None => Ok(UI_PANEL_RELEASE_DEFAULT_REASON),
        Some(raw) if UI_PANEL_RELEASE_REASONS.contains(&raw) => Ok(raw),
        Some(raw) if raw.len() <= UI_PANEL_REASON_MAX_BYTES => Err(component_invalid(format!(
            "ui.panel.release reason '{raw}' is not one of {:?}",
            UI_PANEL_RELEASE_REASONS
        ))),
        Some(_) => Err(component_invalid(format!(
            "ui.panel.release reason exceeds {UI_PANEL_REASON_MAX_BYTES} bytes and is not one of {:?}",
            UI_PANEL_RELEASE_REASONS
        ))),
    }
}
