//! Resolved style cascade: seven-layer style resolution with attribution
//! (UX-20, CTX-0669).
//!
//! Candidate implementation of the U-4 `ResolvedStyle` cascade
//! (`bitty-terminal-docs/specifications/ui-runtime-candidate.md`)
//! (**Candidate**, owner-pending UI Runtime RFC). Nothing here is normative,
//! accepted, or verified: every layer name, rank, and rule below is a
//! candidate spelling that the UI Runtime RFC accepts or rejects, never this
//! module. The module is English-only.
//!
//! What this module provides:
//!
//! - [`StyleOrigin`] — the seven cascade layers in winning order: safety
//!   above user rule above workspace rule above user theme above plugin
//!   preference above plugin content theme above framework default. Rank
//!   decides: the highest-ranked layer claiming a key wins.
//! - [`StyleCascade`] — the per-layer key/value store over theme
//!   [`TokenColor`](crate::theme::TokenColor) values. The framework-default
//!   layer is pre-seeded from [`framework_default`](crate::theme::framework_default),
//!   so resolution over [`CORE_TOKENS`](crate::theme::CORE_TOKENS) is total.
//! - [`ResolvedStyle`] — the winning value plus the [`StyleOrigin`] that
//!   supplied it, so diagnostics report the winner instead of an opaque
//!   conflict.
//! - [`StyleError`] — the key-boundary rejections: unknown Core keys,
//!   `terminal.*` keys (presentation never becomes Terminal Truth), and
//!   malformed plugin-namespaced keys. The boundary mirrors the accepted
//!   theme contract; this cascade adds the rule layers above it.
//!
//! Relationship to panel rules: a rule accent from
//! [`PanelRule`](crate::panel_rules::PanelRule) joins this cascade at its
//! origin rank — `Safety` at [`StyleOrigin::Safety`], `User` at
//! [`StyleOrigin::UserRule`], `Workspace` at [`StyleOrigin::WorkspaceRule`]
//! — so rule-vs-theme precedence is one ordering, not two.
//!
//! All keys are bounded by the theme inventory plus the `plugin.` namespace
//! and `#![forbid(unsafe_code)]`. No wall-clock time, randomness, or
//! platform handle participates; multi-key reports use sorted-key order.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;

use crate::panel_rules::RuleOrigin;
use crate::theme::{CORE_TOKENS, TokenColor, framework_default};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to claim or resolve a style key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StyleError {
    /// A non-plugin-namespaced key outside the closed [`CORE_TOKENS`] set.
    UnknownKey {
        /// The rejected key.
        key: String,
    },
    /// Any `terminal.*` key: terminal cell colors come from the terminal
    /// palette and escape sequences, never from style resolution.
    TerminalKey {
        /// The rejected key.
        key: String,
    },
    /// A `plugin.`-namespaced key with an empty remainder (`plugin.` alone
    /// or `plugin..x`): the namespace must name an owner.
    BadPluginKey {
        /// The rejected key.
        key: String,
    },
}

impl fmt::Display for StyleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey { key } => write!(f, "unknown style key: {key}"),
            Self::TerminalKey { key } => write!(f, "terminal style key rejected: {key}"),
            Self::BadPluginKey { key } => write!(f, "malformed plugin style key: {key}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Origin
// ---------------------------------------------------------------------------

/// One cascade layer. Higher rank wins; the order is the UX-20 contract:
///
/// `safety > user rule > workspace rule > user theme > plugin preference >
/// plugin content theme > framework default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StyleOrigin {
    /// Built-in framework defaults (lowest rank, always total for Core).
    FrameworkDefault,
    /// Plugin content theme package values.
    PluginContentTheme,
    /// Plugin preference values (own chrome treatment knobs).
    PluginPreference,
    /// User-selected theme preset.
    UserTheme,
    /// Workspace-level rule effects.
    WorkspaceRule,
    /// User-level rule effects.
    UserRule,
    /// Safety policy (highest rank, never overridden).
    Safety,
}

impl StyleOrigin {
    /// Priority rank: higher wins.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::FrameworkDefault => 0,
            Self::PluginContentTheme => 1,
            Self::PluginPreference => 2,
            Self::UserTheme => 3,
            Self::WorkspaceRule => 4,
            Self::UserRule => 5,
            Self::Safety => 6,
        }
    }

    /// All origins from lowest to highest rank.
    #[must_use]
    pub const fn ordered() -> [Self; 7] {
        [
            Self::FrameworkDefault,
            Self::PluginContentTheme,
            Self::PluginPreference,
            Self::UserTheme,
            Self::WorkspaceRule,
            Self::UserRule,
            Self::Safety,
        ]
    }

    /// Stable lowercase label for diagnostics and attribution traces.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FrameworkDefault => "framework-default",
            Self::PluginContentTheme => "plugin-content-theme",
            Self::PluginPreference => "plugin-preference",
            Self::UserTheme => "user-theme",
            Self::WorkspaceRule => "workspace-rule",
            Self::UserRule => "user-rule",
            Self::Safety => "safety",
        }
    }

    /// Parses a canonical [`Self::as_str`] name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "framework-default" => Some(Self::FrameworkDefault),
            "plugin-content-theme" => Some(Self::PluginContentTheme),
            "plugin-preference" => Some(Self::PluginPreference),
            "user-theme" => Some(Self::UserTheme),
            "workspace-rule" => Some(Self::WorkspaceRule),
            "user-rule" => Some(Self::UserRule),
            "safety" => Some(Self::Safety),
            _ => None,
        }
    }

    /// Maps a panel-rule origin onto its cascade rank: a rule accent
    /// joins the cascade at the layer its origin names, so rule-vs-theme
    /// precedence is one ordering, not two.
    #[must_use]
    pub const fn for_rule_origin(origin: RuleOrigin) -> Self {
        match origin {
            RuleOrigin::Workspace => Self::WorkspaceRule,
            RuleOrigin::User => Self::UserRule,
            RuleOrigin::Safety => Self::Safety,
        }
    }
}

impl fmt::Display for StyleOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Resolved value
// ---------------------------------------------------------------------------

/// One resolved style: the winning value plus the layer that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedStyle {
    /// The winning color value.
    pub value: TokenColor,
    /// The layer that supplied it (higher rank wins).
    pub origin: StyleOrigin,
}

// ---------------------------------------------------------------------------
// Cascade
// ---------------------------------------------------------------------------

/// Validates a style key against the theme boundary.
///
/// Accepts closed [`CORE_TOKENS`] names and well-formed `plugin.<owner>...`
/// names. Rejects `terminal.*` keys, unknown Core names, and empty plugin
/// remainders.
fn check_key(key: &str) -> Result<(), StyleError> {
    if key.starts_with("terminal.") || key == "terminal" {
        return Err(StyleError::TerminalKey {
            key: key.to_owned(),
        });
    }
    if let Some(rest) = key.strip_prefix("plugin.") {
        if rest.is_empty() || rest.starts_with('.') {
            return Err(StyleError::BadPluginKey {
                key: key.to_owned(),
            });
        }
        return Ok(());
    }
    if CORE_TOKENS.contains(&key) {
        return Ok(());
    }
    Err(StyleError::UnknownKey {
        key: key.to_owned(),
    })
}

/// The seven-layer style cascade.
///
/// The framework-default layer is pre-seeded at construction, so every
/// [`CORE_TOKENS`] key resolves even when no other layer claims it.
/// Higher layers override lower ones per key; attribution always names the
/// winning origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleCascade {
    layers: BTreeMap<StyleOrigin, BTreeMap<String, TokenColor>>,
}

impl StyleCascade {
    /// Builds a cascade pre-seeded with framework defaults for every Core
    /// token. Infallible by construction: every Core token has a default.
    #[must_use]
    pub fn new() -> Self {
        let mut defaults = BTreeMap::new();
        for name in CORE_TOKENS {
            if let Some(value) = framework_default(name) {
                defaults.insert((*name).to_owned(), value);
            }
        }
        let mut layers = BTreeMap::new();
        layers.insert(StyleOrigin::FrameworkDefault, defaults);
        Self { layers }
    }

    /// Claims a key at one layer, validating the key boundary fail-closed.
    pub fn set(
        &mut self,
        origin: StyleOrigin,
        key: &str,
        value: TokenColor,
    ) -> Result<(), StyleError> {
        // The framework-default layer is seeded, not written: callers claim
        // the six live layers above it.
        if origin == StyleOrigin::FrameworkDefault {
            return Err(StyleError::UnknownKey {
                key: key.to_owned(),
            });
        }
        check_key(key)?;
        self.layers
            .entry(origin)
            .or_default()
            .insert(key.to_owned(), value);
        Ok(())
    }

    /// Resolves one key to its winning value and origin, or `None` when no
    /// layer claims it.
    #[must_use]
    pub fn resolve(&self, key: &str) -> Option<ResolvedStyle> {
        let mut winner: Option<ResolvedStyle> = None;
        for origin in StyleOrigin::ordered() {
            let layer = self.layers.get(&origin);
            let mut value = None;
            if let Some(entries) = layer {
                value = entries.get(key).copied();
            }
            if let Some(color) = value {
                winner = Some(ResolvedStyle {
                    value: color,
                    origin,
                });
            }
        }
        winner
    }

    /// Resolves every claimed key (plus all pre-seeded Core tokens) in
    /// sorted-key order.
    #[must_use]
    pub fn resolve_all(&self) -> BTreeMap<String, ResolvedStyle> {
        let mut keys: Vec<&String> = Vec::new();
        for entries in self.layers.values() {
            for key in entries.keys() {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        let mut out = BTreeMap::new();
        for key in keys {
            if let Some(resolved) = self.resolve(key) {
                out.insert(key.clone(), resolved);
            }
        }
        out
    }

    /// Names the winning origin for one key, or `None` when unclaimed.
    #[must_use]
    pub fn attribution(&self, key: &str) -> Option<StyleOrigin> {
        self.resolve(key).map(|r| r.origin)
    }
}

impl Default for StyleCascade {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const RED: TokenColor = TokenColor([0xFF, 0x00, 0x00, 0xFF]);
    const GREEN: TokenColor = TokenColor([0x00, 0xFF, 0x00, 0xFF]);
    const BLUE: TokenColor = TokenColor([0x00, 0x00, 0xFF, 0xFF]);

    #[test]
    fn origin_order_is_the_ux20_contract() {
        let mut ranks: Vec<u8> = StyleOrigin::ordered().iter().map(|o| o.rank()).collect();
        let sorted = {
            let mut copy = ranks.clone();
            copy.sort();
            copy
        };
        assert_eq!(ranks, sorted, "ordered() must run lowest to highest");
        assert!(StyleOrigin::Safety.rank() > StyleOrigin::UserRule.rank());
        assert!(StyleOrigin::UserRule.rank() > StyleOrigin::WorkspaceRule.rank());
        assert!(StyleOrigin::WorkspaceRule.rank() > StyleOrigin::UserTheme.rank());
        assert!(StyleOrigin::UserTheme.rank() > StyleOrigin::PluginPreference.rank());
        assert!(StyleOrigin::PluginPreference.rank() > StyleOrigin::PluginContentTheme.rank());
        assert!(StyleOrigin::PluginContentTheme.rank() > StyleOrigin::FrameworkDefault.rank());
        ranks.clear();
    }

    #[test]
    fn highest_claimant_wins_with_attribution() {
        let mut cascade = StyleCascade::new();
        cascade
            .set(StyleOrigin::PluginContentTheme, "content.accent", BLUE)
            .expect("valid key");
        cascade
            .set(StyleOrigin::PluginPreference, "content.accent", GREEN)
            .expect("valid key");
        cascade
            .set(StyleOrigin::UserTheme, "content.accent", RED)
            .expect("valid key");
        let resolved = cascade.resolve("content.accent").expect("claimed");
        assert_eq!(resolved.value, RED);
        assert_eq!(resolved.origin, StyleOrigin::UserTheme);
        assert_eq!(
            cascade.attribution("content.accent"),
            Some(StyleOrigin::UserTheme)
        );
        // Rules beat themes; safety beats rules.
        cascade
            .set(StyleOrigin::WorkspaceRule, "content.accent", GREEN)
            .expect("valid key");
        assert_eq!(
            cascade.attribution("content.accent"),
            Some(StyleOrigin::WorkspaceRule)
        );
        cascade
            .set(StyleOrigin::Safety, "content.accent", BLUE)
            .expect("valid key");
        let safe = cascade.resolve("content.accent").expect("claimed");
        assert_eq!(safe.value, BLUE);
        assert_eq!(safe.origin, StyleOrigin::Safety);
    }

    #[test]
    fn framework_defaults_keep_core_total() {
        let cascade = StyleCascade::new();
        for name in CORE_TOKENS {
            let resolved = cascade.resolve(name);
            assert!(resolved.is_some(), "core key {name} must resolve");
            assert_eq!(
                resolved.expect("checked").origin,
                StyleOrigin::FrameworkDefault
            );
        }
        assert_eq!(cascade.resolve("not.a.key"), None);
        // resolve_all covers every Core token at minimum.
        assert!(cascade.resolve_all().len() >= CORE_TOKENS.len());
    }

    #[test]
    fn key_boundary_rejects_terminal_unknown_and_bad_plugin() {
        let mut cascade = StyleCascade::new();
        assert!(matches!(
            cascade.set(StyleOrigin::UserTheme, "terminal.foreground", RED),
            Err(StyleError::TerminalKey { .. })
        ));
        assert!(matches!(
            cascade.set(StyleOrigin::UserTheme, "chrome.unknown", RED),
            Err(StyleError::UnknownKey { .. })
        ));
        assert!(matches!(
            cascade.set(StyleOrigin::UserTheme, "plugin.", RED),
            Err(StyleError::BadPluginKey { .. })
        ));
        cascade
            .set(StyleOrigin::PluginContentTheme, "plugin.notes.accent", RED)
            .expect("namespaced plugin key is accepted");
        assert_eq!(
            cascade.attribution("plugin.notes.accent"),
            Some(StyleOrigin::PluginContentTheme)
        );
    }

    #[test]
    fn origin_names_round_trip() {
        for origin in StyleOrigin::ordered() {
            assert_eq!(StyleOrigin::parse(origin.as_str()), Some(origin));
        }
        assert_eq!(StyleOrigin::parse("user"), None);
    }
}
