//! `ConfigPlan` — the declarative data returned from Lua evaluation.
//!
//! Pipeline position (candidate, per RFC): `Lua -> ConfigPlan -> typed
//! validation -> merge -> diff -> reconcile`. The plan itself is side-effect
//! free; evaluation that produced it never mutated live terminal state. Rust
//! owns everything after this point.
//!
//! # Draft status
//!
//! The typed schema here is a candidate synthesis of the
//! `lua-and-xdg` topic and the configuration-model RFC (both `draft`).
//! Fields, merge classes, and layer ordering are not normative and will
//! move with the RFC.

use crate::error::ConfigError;
use crate::types::{
    AppearanceConfig, FontConfig, KeymapEntry, PluginSpec, TerminalConfig, WindowConfig,
};

/// Current schema version is owned by [`crate::migration`].
pub use crate::migration::CURRENT_SCHEMA_VERSION;

/// The declarative plan returned from a configuration module.
///
/// Each field is `Option` because a single layer may declare only the
/// subset it cares about; merging fills the rest from lower-precedence
/// layers and core defaults.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ConfigPlan {
    /// Schema version of this plan. If absent, assumed `0` for migration.
    pub schema_version: Option<u32>,
    /// Font configuration.
    pub font: Option<FontConfig>,
    /// Window configuration.
    pub window: Option<WindowConfig>,
    /// Terminal configuration.
    pub terminal: Option<TerminalConfig>,
    /// Appearance configuration.
    pub appearance: Option<AppearanceConfig>,
    /// Key mappings (full set for this layer).
    pub keymaps: Option<Vec<KeymapEntry>>,
    /// Plugin set (full set for this layer).
    pub plugins: Option<Vec<PluginSpec>>,
    /// Profile name this plan declares itself as (for `extends` sources).
    pub profile_name: Option<String>,
    /// Single-parent `extends` target; cycle detection is enforced.
    pub extends: Option<String>,
    /// Undeclared / unknown fields captured for strict validation.
    ///
    /// The RFC requires that undeclared fields fail validation rather than
    /// merging implicitly. Capturing them here lets validation surface the
    /// offending source.
    pub undeclared_fields: Vec<String>,
}

impl ConfigPlan {
    /// Create an empty plan at the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: Some(CURRENT_SCHEMA_VERSION),
            ..Default::default()
        }
    }

    /// Convenience builder: set font.
    #[must_use]
    pub fn with_font(mut self, font: FontConfig) -> Self {
        self.font = Some(font);
        self
    }

    /// Convenience builder: set window.
    #[must_use]
    pub fn with_window(mut self, window: WindowConfig) -> Self {
        self.window = Some(window);
        self
    }

    /// Convenience builder: set terminal.
    #[must_use]
    pub fn with_terminal(mut self, terminal: TerminalConfig) -> Self {
        self.terminal = Some(terminal);
        self
    }

    /// Convenience builder: set appearance.
    #[must_use]
    pub fn with_appearance(mut self, appearance: AppearanceConfig) -> Self {
        self.appearance = Some(appearance);
        self
    }

    /// The effective schema version (`0` if missing).
    #[must_use]
    pub fn effective_schema_version(&self) -> u32 {
        self.schema_version.unwrap_or(0)
    }

    /// Validate present fields and reject any undeclared fields.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(v) = self.schema_version {
            if v > CURRENT_SCHEMA_VERSION {
                return Err(ConfigError::SchemaVersionUnsupported {
                    found: v,
                    supported: CURRENT_SCHEMA_VERSION,
                });
            }
        }
        if !self.undeclared_fields.is_empty() {
            return Err(ConfigError::UndeclaredField {
                field: self.undeclared_fields[0].clone(),
                source: None,
            });
        }
        if let Some(f) = &self.font {
            f.validate()?;
        }
        if let Some(w) = &self.window {
            w.validate()?;
        }
        if let Some(t) = &self.terminal {
            t.validate()?;
        }
        if let Some(a) = &self.appearance {
            a.validate()?;
        }
        if let Some(kms) = &self.keymaps {
            if kms.len() > crate::types::MAX_KEYMAPS {
                return Err(ConfigError::validation(
                    "keymaps",
                    format!("must contain <= {} entries", crate::types::MAX_KEYMAPS),
                ));
            }
            for km in kms {
                km.validate()?;
            }
        }
        if let Some(ps) = &self.plugins {
            if ps.len() > crate::types::MAX_PLUGINS {
                return Err(ConfigError::validation(
                    "plugins",
                    format!("must contain <= {} entries", crate::types::MAX_PLUGINS),
                ));
            }
            for p in ps {
                p.validate()?;
            }
        }
        if let Some(ext) = &self.extends {
            if ext.trim().is_empty() {
                return Err(ConfigError::validation(
                    "extends",
                    "must be non-empty when present",
                ));
            }
        }
        Ok(())
    }

    /// Whether this plan is empty (no typed fields set).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.font.is_none()
            && self.window.is_none()
            && self.terminal.is_none()
            && self.appearance.is_none()
            && self.keymaps.is_none()
            && self.plugins.is_none()
            && self.extends.is_none()
            && self.profile_name.is_none()
    }
}

/// Candidate layer stack and precedence (from `lua-and-xdg.md` and the RFC
/// section "Layers, merge, and attribution").
///
/// Precedence is strictly ordered; the later item wins per declared merge
/// class, never by load-order accident. Core defaults are the lowest, CLI
/// the highest.
///
/// Full stack (inclusive of policy split):
///
/// `CoreDefaults < SystemDefaults < SystemPolicy(non-overridable) <
/// Distribution < Profile < User < TrustedLocal < Cli`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LayerKind {
    /// Built-in minimal configuration; used for `--safe` and first-start
    /// fallback per R-009.
    CoreDefaults,
    /// System defaults (e.g. `/etc/xdg/bitty/defaults.lua`). Trusted only
    /// after source verification.
    SystemDefaults,
    /// System policy entries that are non-overridable; distinct from
    /// `SystemDefaults` per the trust table.
    SystemPolicy,
    /// Distribution layer.
    Distribution,
    /// Profile layer (`extends` chain).
    Profile,
    /// User configuration (`$XDG_CONFIG_HOME/bitty/init.lua`).
    User,
    /// Trusted local override (project-scoped, hash-bound consent).
    TrustedLocal,
    /// CLI overrides.
    Cli,
}

impl LayerKind {
    /// Precedence rank; higher wins.
    #[must_use]
    pub fn precedence(self) -> u8 {
        match self {
            Self::CoreDefaults => 0,
            Self::SystemDefaults => 10,
            Self::SystemPolicy => 20,
            Self::Distribution => 30,
            Self::Profile => 40,
            Self::User => 50,
            Self::TrustedLocal => 60,
            Self::Cli => 70,
        }
    }

    /// Whether this layer is non-overridable policy.
    #[must_use]
    pub fn is_policy(self) -> bool {
        matches!(self, Self::SystemPolicy)
    }

    /// Human-readable label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::CoreDefaults => "core-defaults",
            Self::SystemDefaults => "system-defaults",
            Self::SystemPolicy => "system-policy",
            Self::Distribution => "distribution",
            Self::Profile => "profile",
            Self::User => "user",
            Self::TrustedLocal => "trusted-local",
            Self::Cli => "cli",
        }
    }
}

impl std::fmt::Display for LayerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Source attribution for a single plan layer.
///
/// The RFC requires that source attribution survive merging so every
/// effective value can answer "which file, which layer". This struct is
/// deliberately small and cloneable for headless tests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConfigSource {
    /// Which layer this plan came from.
    pub layer: LayerKind,
    /// File path or identifier (e.g. `"/home/alice/.config/bitty/init.lua"`).
    /// `None` for in-memory or core defaults.
    pub path: Option<String>,
    /// Optional line/column hint for diagnostics.
    pub line: Option<u32>,
}

impl ConfigSource {
    /// Create a source.
    pub fn new(layer: LayerKind, path: Option<impl Into<String>>) -> Self {
        Self {
            layer,
            path: path.map(Into::into),
            line: None,
        }
    }

    /// With line.
    #[must_use]
    pub fn with_line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }

    /// Human-readable description for diagnostics, e.g. `user:/path/init.lua:12`.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut s = self.layer.label().to_string();
        if let Some(p) = &self.path {
            s.push(':');
            s.push_str(p);
            if let Some(l) = self.line {
                s.push(':');
                s.push_str(&l.to_string());
            }
        }
        s
    }
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// A single layer's plan together with its source.
#[derive(Debug, Clone, PartialEq)]
pub struct LayeredPlan {
    /// Where this plan came from.
    pub source: ConfigSource,
    /// The declarative plan for this layer.
    pub plan: ConfigPlan,
}

impl LayeredPlan {
    /// Create a layered plan.
    pub fn new(source: ConfigSource, plan: ConfigPlan) -> Self {
        Self { source, plan }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_validate_rejects_undeclared() {
        let mut p = ConfigPlan::new();
        p.undeclared_fields.push("unknown_field".into());
        assert!(p.validate().is_err());
    }

    #[test]
    fn layer_precedence_order() {
        assert!(LayerKind::CoreDefaults.precedence() < LayerKind::SystemDefaults.precedence());
        assert!(LayerKind::SystemDefaults.precedence() < LayerKind::SystemPolicy.precedence());
        assert!(LayerKind::SystemPolicy.precedence() < LayerKind::Distribution.precedence());
        assert!(LayerKind::Distribution.precedence() < LayerKind::Profile.precedence());
        assert!(LayerKind::Profile.precedence() < LayerKind::User.precedence());
        assert!(LayerKind::User.precedence() < LayerKind::TrustedLocal.precedence());
        assert!(LayerKind::TrustedLocal.precedence() < LayerKind::Cli.precedence());
    }

    #[test]
    fn source_describe() {
        let s = ConfigSource::new(LayerKind::User, Some("/home/a/.config/bitty/init.lua"))
            .with_line(42);
        assert_eq!(s.describe(), "user:/home/a/.config/bitty/init.lua:42");
        let s2 = ConfigSource::new(LayerKind::CoreDefaults, None::<String>);
        assert_eq!(s2.describe(), "core-defaults");
    }

    #[test]
    fn plan_default_is_empty_and_valid() {
        let p = ConfigPlan::default();
        assert!(p.is_empty());
        // default has no schema_version, so effective 0 —still valid
        p.validate().expect("empty plan must be valid");
    }

    #[test]
    fn plan_new_sets_current_version() {
        let p = ConfigPlan::new();
        assert_eq!(p.effective_schema_version(), CURRENT_SCHEMA_VERSION);
        p.validate().expect("new plan valid");
    }

    #[test]
    fn schema_version_unsupported() {
        let p = ConfigPlan {
            schema_version: Some(9999),
            ..Default::default()
        };
        assert!(p.validate().is_err());
    }
}
