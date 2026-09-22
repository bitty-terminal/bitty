//! Lua panel/workspace API descriptors (UX-11, issue #1017).
//!
//! Candidate implementation (**Candidate**, owner-pending Panel RFC;
//! spellings, scopes, and the API version stay `[BLOCKED: OQ-056]`, and
//! the capability model refines `CW-22`). Nothing here is normative,
//! accepted, or verified: every spelling, scope, and event name below is a
//! candidate the owning RFC accepts or rejects, never this module. The
//! module is English-only.
//!
//! What this module provides:
//!
//! - [`LuaCapability`] — the four candidate capabilities gating panel and
//!   workspace mutation (`move`, `resize`, `float`, `workspace`).
//! - [`PANEL_LUA_COMMANDS`] — the candidate command spelling table
//!   (`bitty.panel:*` / `bitty.workspace:*`) with the capability and
//!   [`ApiScope`] each spelling needs. Spellings satisfy the
//!   `owner.name:command` grammar (see
//!   [`QualifiedCommand`](crate::panel::QualifiedCommand)) and are
//!   validated by [`validate_spellings`].
//! - [`PANEL_LUA_QUERIES`] / [`PANEL_LUA_EVENTS`] — the candidate query
//!   and event name tables. Queries read; events notify; neither mutates.
//! - [`CapabilityGate`] — the fail-closed capability check: a call is
//!   allowed only when every capability its spelling needs was granted.
//!
//! Deliberately no Lua binding: with spellings undecided (`OQ-056`), this
//! crate owns the descriptor tables the future `bitty-lua` surface
//! validates against, never the binding itself. Handles crossing into Lua
//! stay [`OpaquePanelHandle`](crate::panel_identity::OpaquePanelHandle):
//! equality-only, resolved registry-side.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, render, platform, PTY, or plugin handle participates.

#![forbid(unsafe_code)]

use std::fmt;

use crate::panel::QualifiedCommand;

// ---------------------------------------------------------------------------
// ApiVersion: undecided, as data
// ---------------------------------------------------------------------------

/// Candidate API versions for the Lua panel surface (decision: `OQ-056`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanelApiVersion {
    /// The single candidate spelling this module describes.
    V1,
}

impl PanelApiVersion {
    /// Canonical version string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
        }
    }
}

impl fmt::Display for PanelApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// LuaCapability / ApiScope
// ---------------------------------------------------------------------------

/// Candidate capability granting one panel/workspace mutation family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LuaCapability {
    /// Move panels between slots, layers, and workspaces.
    MovePanel,
    /// Resize tiled and floating panels.
    ResizePanel,
    /// Toggle the floating presentation of a panel.
    FloatPanel,
    /// Switch, open, and close workspaces.
    SwitchWorkspace,
}

impl LuaCapability {
    /// Canonical kebab-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MovePanel => "move",
            Self::ResizePanel => "resize",
            Self::FloatPanel => "float",
            Self::SwitchWorkspace => "workspace",
        }
    }
}

impl fmt::Display for LuaCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Candidate scope a spelling operates in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApiScope {
    /// One panel (takes an opaque handle).
    Panel,
    /// One workspace (takes a workspace index).
    Workspace,
}

impl ApiScope {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::Workspace => "workspace",
        }
    }
}

impl fmt::Display for ApiScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Descriptor tables
// ---------------------------------------------------------------------------

/// One candidate command spelling with its gate and scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PanelLuaCommand {
    /// Candidate `owner.name:command` spelling.
    pub spelling: &'static str,
    /// Capability the call needs granted.
    pub capability: LuaCapability,
    /// Scope the call operates in.
    pub scope: ApiScope,
}

/// Candidate command spellings (decision: `OQ-056`).
pub const PANEL_LUA_COMMANDS: [PanelLuaCommand; 6] = [
    PanelLuaCommand {
        spelling: "bitty.panel:move",
        capability: LuaCapability::MovePanel,
        scope: ApiScope::Panel,
    },
    PanelLuaCommand {
        spelling: "bitty.panel:resize",
        capability: LuaCapability::ResizePanel,
        scope: ApiScope::Panel,
    },
    PanelLuaCommand {
        spelling: "bitty.panel:float-toggle",
        capability: LuaCapability::FloatPanel,
        scope: ApiScope::Panel,
    },
    PanelLuaCommand {
        spelling: "bitty.workspace:switch",
        capability: LuaCapability::SwitchWorkspace,
        scope: ApiScope::Workspace,
    },
    PanelLuaCommand {
        spelling: "bitty.workspace:move-panel-to",
        capability: LuaCapability::MovePanel,
        scope: ApiScope::Workspace,
    },
    PanelLuaCommand {
        spelling: "bitty.workspace:close",
        capability: LuaCapability::SwitchWorkspace,
        scope: ApiScope::Workspace,
    },
];

/// One candidate query name (reads only, never gated by mutation caps).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PanelLuaQuery {
    /// Candidate query name.
    pub name: &'static str,
    /// Scope the query reads.
    pub scope: ApiScope,
}

/// Candidate query names (decision: `OQ-056`).
pub const PANEL_LUA_QUERIES: [PanelLuaQuery; 3] = [
    PanelLuaQuery {
        name: "list-panels",
        scope: ApiScope::Workspace,
    },
    PanelLuaQuery {
        name: "active-panel",
        scope: ApiScope::Workspace,
    },
    PanelLuaQuery {
        name: "panel-geometry",
        scope: ApiScope::Panel,
    },
];

/// One candidate event name (notifies only, never mutates).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PanelLuaEvent {
    /// Candidate event name.
    pub name: &'static str,
    /// Scope the event reports.
    pub scope: ApiScope,
}

/// Candidate event names (decision: `OQ-056`).
pub const PANEL_LUA_EVENTS: [PanelLuaEvent; 4] = [
    PanelLuaEvent {
        name: "panel-opened",
        scope: ApiScope::Panel,
    },
    PanelLuaEvent {
        name: "panel-closed",
        scope: ApiScope::Panel,
    },
    PanelLuaEvent {
        name: "panel-moved",
        scope: ApiScope::Panel,
    },
    PanelLuaEvent {
        name: "workspace-switched",
        scope: ApiScope::Workspace,
    },
];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to validate the Lua panel surface descriptors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LuaApiError {
    /// A table spelling fails the `owner.name:command` grammar.
    BadSpelling {
        /// The rejected spelling.
        spelling: String,
    },
    /// A spelling appears twice in the command table.
    DuplicateSpelling {
        /// The duplicated spelling.
        spelling: String,
    },
    /// The capability was not granted for this spelling.
    CapabilityDenied {
        /// The refused spelling.
        spelling: String,
        /// The missing capability.
        capability: LuaCapability,
    },
    /// No table entry names this spelling.
    UnknownSpelling {
        /// The unrecognized spelling.
        spelling: String,
    },
}

impl fmt::Display for LuaApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadSpelling { spelling } => {
                write!(f, "bad lua spelling: {spelling}")
            }
            Self::DuplicateSpelling { spelling } => {
                write!(f, "duplicate lua spelling: {spelling}")
            }
            Self::CapabilityDenied {
                spelling,
                capability,
            } => write!(f, "capability '{capability}' denied for '{spelling}'"),
            Self::UnknownSpelling { spelling } => {
                write!(f, "unknown lua spelling: {spelling}")
            }
        }
    }
}

impl std::error::Error for LuaApiError {}

// ---------------------------------------------------------------------------
// validate_spellings / CapabilityGate
// ---------------------------------------------------------------------------

/// Validates the candidate command table: every spelling parses as
/// [`QualifiedCommand`] and no spelling repeats.
///
/// Fails closed on the first defect; a table that validates here still
/// needs the `OQ-056` owner decision before any binding ships.
pub fn validate_spellings() -> Result<(), LuaApiError> {
    let mut seen: Vec<&str> = Vec::with_capacity(PANEL_LUA_COMMANDS.len());
    for cmd in &PANEL_LUA_COMMANDS {
        QualifiedCommand::parse(cmd.spelling).map_err(|_| LuaApiError::BadSpelling {
            spelling: cmd.spelling.to_owned(),
        })?;
        if seen.contains(&cmd.spelling) {
            return Err(LuaApiError::DuplicateSpelling {
                spelling: cmd.spelling.to_owned(),
            });
        }
        seen.push(cmd.spelling);
    }
    Ok(())
}

/// Looks up the descriptor for `spelling`.
pub fn lookup_command(spelling: &str) -> Result<PanelLuaCommand, LuaApiError> {
    PANEL_LUA_COMMANDS
        .iter()
        .copied()
        .find(|c| c.spelling == spelling)
        .ok_or_else(|| LuaApiError::UnknownSpelling {
            spelling: spelling.to_owned(),
        })
}

/// Fail-closed capability check over the candidate command table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityGate {
    granted: Vec<LuaCapability>,
}

impl CapabilityGate {
    /// Creates a gate granting exactly `granted`.
    #[must_use]
    pub fn with_grants(granted: &[LuaCapability]) -> Self {
        let mut unique = Vec::with_capacity(granted.len());
        for cap in granted {
            if !unique.contains(cap) {
                unique.push(*cap);
            }
        }
        Self { granted: unique }
    }

    /// Whether `spelling` may run: known spelling with its capability
    /// granted. Unknown spellings and missing capabilities deny alike.
    pub fn allows(&self, spelling: &str) -> Result<PanelLuaCommand, LuaApiError> {
        let cmd = lookup_command(spelling)?;
        if self.granted.contains(&cmd.capability) {
            Ok(cmd)
        } else {
            Err(LuaApiError::CapabilityDenied {
                spelling: spelling.to_owned(),
                capability: cmd.capability,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_parses_and_has_no_duplicates() {
        validate_spellings().unwrap();
    }

    #[test]
    fn gate_denies_ungranted_and_unknown() {
        let gate = CapabilityGate::with_grants(&[LuaCapability::MovePanel]);
        assert!(gate.allows("bitty.panel:move").is_ok());
        assert!(gate.allows("bitty.panel:resize").is_err());
        assert!(gate.allows("bitty.panel:nope").is_err());
    }
}
