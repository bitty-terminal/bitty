//! Candidate theme token contract (CTX-0612, UX-40).
//!
//! > Status: **draft candidate** — not **Accepted**, not **Verified**, and not
//! > normative. The accepted
//! > [RFC-0001 appearance configuration](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/rfcs/RFC-0001-appearance-configuration.md)
//! > already fixes the color value grammar, the per-key resolution order, the
//! > AC-1..AC-3 contrast rules, and the `bitty --safe` forced pair. This
//! > module only names the **candidate token vocabulary** from the
//! > [Theme Token Contract (Candidate)](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/theme-token-contract-candidate.md)
//! > record: which token names exist, who may define them, and how a token
//! > resolves when several sources claim it. Every token spelling below is a
//! > candidate spelling; acceptance happens through the appearance RFC's
//! > successor or the owner-pending UI Runtime RFC, never by using this
//! > module.
//!
//! # What this slice provides
//!
//! - [`CORE_TOKENS`] — the closed candidate Core inventory (`border.*`,
//!   `chrome.*`, `content.*`, `state.*`); adding a Core token is a contract
//!   change, and a plugin may not invent one.
//! - [`resolve`] — layered resolution, later wins: framework default, then
//!   theme preset, then plugin package (own `plugin.<name>.*` namespace
//!   only, never consulted for Core chrome/borders), then explicit user keys,
//!   then `bitty --safe` forced values for the outline pair. Core chrome
//!   structurally ignores the plugin layer, matching the accepted rule that
//!   no plugin sets Core-owned chrome at runtime.
//! - [`ResolvedTokens::layer_of`] — per-key attribution naming the winning
//!   [`TokenLayer`], so diagnostics report the winner instead of an opaque
//!   conflict.
//! - [`validate_contrast`] — the accepted AC-1 (focused `>= 3:1` vs
//!   background, enforced), AC-2 (focused `>= 3:1` vs idle, enforced), and
//!   AC-3 (idle `>= 1.5:1` vs background, advisory) rules over the resolved
//!   outline pair with the accepted relative-luminance computation. A
//!   violating preset pair is rejected fail-closed with a diagnostic naming
//!   the key and the resolved values.
//! - [`check_plugin_keys`] — the plugin boundary: a package named `N` may
//!   supply `plugin.N.*` only; `chrome.*`, `border.*`, `content.*`,
//!   `state.*`, another plugin's namespace, and `terminal.*` are rejected
//!   fail-closed.
//!
//! Terminal cell colors are never sourced from theme tokens (presentation
//! never becomes Terminal Truth): any `terminal.*` key is rejected, and the
//! resolved set structurally contains no terminal keys.

use std::collections::BTreeMap;
use std::fmt;

/// A theme token color: unpremultiplied `sRGB` RGBA bytes.
///
/// Accepted spelling is exactly `#RRGGBB` or `#RRGGBBAA` (RGBA byte order);
/// alpha defaults to `FF` when omitted. Named colors, `#RGB` shorthand,
/// `rgb()` syntax, gradients, and images are rejected fail-closed by
/// [`Self::parse`], mirroring the accepted RFC-0001 value grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenColor(pub [u8; 4]);

impl TokenColor {
    /// Builds a color from RGBA bytes.
    #[must_use]
    pub const fn from_rgba(rgba: [u8; 4]) -> Self {
        Self(rgba)
    }

    /// Parses a canonical `#RRGGBB` / `#RRGGBBAA` spelling.
    ///
    /// Returns `None` for any other grammar (missing `#`, wrong digit count,
    /// non-hex bytes, overlong input). Surrounding whitespace is trimmed like
    /// the accepted outline parser; overlong input is rejected, never
    /// truncated.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.len() > 9 {
            return None;
        }
        let body = trimmed.strip_prefix('#')?;
        let bytes = body.as_bytes();
        let hex_byte = |start: usize| -> Option<u8> {
            let hi = (*bytes.get(start)? as char).to_digit(16)?;
            let lo = (*bytes.get(start + 1)? as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        };
        let (r, g, b, a) = match bytes.len() {
            6 => (hex_byte(0)?, hex_byte(2)?, hex_byte(4)?, 0xFF),
            8 => (hex_byte(0)?, hex_byte(2)?, hex_byte(4)?, hex_byte(6)?),
            _ => return None,
        };
        Some(Self([r, g, b, a]))
    }

    /// Canonical spelling: `#RRGGBB` when opaque, `#RRGGBBAA` otherwise.
    #[must_use]
    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.0;
        if a == 0xFF {
            format!("#{r:02X}{g:02X}{b:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        }
    }

    /// Opaque RGB channels, dropping alpha.
    #[must_use]
    pub const fn rgb(self) -> [u8; 3] {
        [self.0[0], self.0[1], self.0[2]]
    }

    /// This color composited with straight-alpha src-over onto opaque `bg`.
    #[must_use]
    pub fn composited_over(self, bg: [u8; 3]) -> [u8; 3] {
        let [r, g, b, a] = self.0;
        let alpha = u16::from(a);
        let mix = |src: u8, dst: u8| -> u8 {
            let src = u16::from(src);
            let dst = u16::from(dst);
            (((src * alpha) + (dst * (255 - alpha)) + 127) / 255) as u8
        };
        [mix(r, bg[0]), mix(g, bg[1]), mix(b, bg[2])]
    }
}

impl fmt::Display for TokenColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Focused `View` outline (candidate spelling).
pub const BORDER_FOCUSED: &str = "border.focused";
/// Idle `View` outline (candidate spelling).
pub const BORDER_IDLE: &str = "border.idle";
/// Panel content base background: the background the AC rules resolve against.
pub const CONTENT_BACKGROUND: &str = "content.background";

/// Closed candidate Core token inventory: the only `border.*`, `chrome.*`,
/// `content.*`, and `state.*` names a layer may supply. Adding a Core token
/// is a contract change; a plugin may not invent one under these namespaces.
pub const CORE_TOKENS: &[&str] = &[
    "border.focused",
    "border.idle",
    "chrome.bar.background",
    "chrome.bar.foreground",
    "chrome.bar.active",
    "chrome.rail.background",
    "chrome.rail.active",
    "chrome.tab.active",
    "chrome.tab.inactive",
    "chrome.notification",
    "content.background",
    "content.foreground",
    "content.accent",
    "content.muted",
    "state.error",
    "state.warning",
    "state.success",
];

/// Framework-default value for one Core token.
///
/// Every [`CORE_TOKENS`] name has a default, so resolution from empty layers
/// is total. The outline pair reuses the accepted Bitty Dark pair
/// (`#33CCFF` / `#595959AA`), which clears AC-1/AC-2 with the advisory AC-3
/// floor intact.
#[must_use]
pub fn framework_default(name: &str) -> Option<TokenColor> {
    let bytes: Option<[u8; 4]> = match name {
        "border.focused" => Some([0x33, 0xCC, 0xFF, 0xFF]),
        "border.idle" => Some([0x59, 0x59, 0x59, 0xAA]),
        "chrome.bar.background" => Some([0x1E, 0x1E, 0x2E, 0xFF]),
        "chrome.bar.foreground" => Some([0xCD, 0xD6, 0xF4, 0xFF]),
        "chrome.bar.active" => Some([0x89, 0xB4, 0xFA, 0xFF]),
        "chrome.rail.background" => Some([0x18, 0x18, 0x25, 0xFF]),
        "chrome.rail.active" => Some([0x89, 0xB4, 0xFA, 0xFF]),
        "chrome.tab.active" => Some([0x31, 0x32, 0x44, 0xFF]),
        "chrome.tab.inactive" => Some([0x18, 0x18, 0x25, 0xFF]),
        "chrome.notification" => Some([0xF9, 0xE2, 0xAF, 0xFF]),
        "content.background" => Some([0x1E, 0x1E, 0x2E, 0xFF]),
        "content.foreground" => Some([0xCD, 0xD6, 0xF4, 0xFF]),
        "content.accent" => Some([0x89, 0xB4, 0xFA, 0xFF]),
        "content.muted" => Some([0xBA, 0xC2, 0xDE, 0xFF]),
        "state.error" => Some([0xF3, 0x8B, 0xA8, 0xFF]),
        "state.warning" => Some([0xF9, 0xE2, 0xAF, 0xFF]),
        "state.success" => Some([0xA6, 0xE3, 0xA1, 0xFF]),
        _ => None,
    };
    bytes.map(TokenColor)
}

/// `bitty --safe` forced focused outline (accepted pair).
pub const SAFE_BORDER_FOCUSED: TokenColor = TokenColor([0xFF, 0xFF, 0xFF, 0xFF]);
/// `bitty --safe` forced idle outline (accepted pair).
pub const SAFE_BORDER_IDLE: TokenColor = TokenColor([0x80, 0x80, 0x80, 0xFF]);

/// Degradation target for a plugin content token that violates contrast: a
/// plugin's content is not trusted to satisfy a contrast obligation it cannot
/// verify, so a violating value is reported and replaced with this compliant
/// default instead of failing the panel.
pub const PLUGIN_TOKEN_FALLBACK: TokenColor = TokenColor([0xCD, 0xD6, 0xF4, 0xFF]);

/// AC-1 floor: focused outline `>= 3:1` against the background (enforced).
pub const AC1_FOCUSED_VS_BACKGROUND_MIN: f64 = 3.0;
/// AC-2 floor: focused outline `>= 3:1` against idle (enforced).
pub const AC2_FOCUSED_VS_IDLE_MIN: f64 = 3.0;
/// AC-3 floor: idle outline `>= 1.5:1` against the background (advisory).
pub const AC3_IDLE_VS_BACKGROUND_MIN: f64 = 1.5;

/// One layer of the candidate resolution order, later wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TokenLayer {
    /// Built-in framework defaults ([`framework_default`]).
    FrameworkDefault,
    /// Theme preset token values selected by `appearance.theme`.
    ThemePreset,
    /// Plugin theme package values, own `plugin.<name>.*` namespace only.
    PluginPackage,
    /// Explicit user keys under the accepted appearance surface.
    UserKeys,
    /// `bitty --safe` forced values for the keys it governs.
    SafeForced,
}

impl TokenLayer {
    /// Stable lowercase label for diagnostics and attribution traces.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FrameworkDefault => "framework-default",
            Self::ThemePreset => "theme-preset",
            Self::PluginPackage => "plugin-package",
            Self::UserKeys => "user-keys",
            Self::SafeForced => "safe-forced",
        }
    }
}

impl fmt::Display for TokenLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One resolved token: the winning value plus the layer that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedToken {
    /// The winning color value.
    pub value: TokenColor,
    /// The layer that supplied it (later layers win).
    pub layer: TokenLayer,
}

/// The full resolved token set in deterministic (sorted-key) order.
///
/// Always contains every [`CORE_TOKENS`] entry plus any plugin-namespaced
/// tokens claimed by the preset, plugin package, or user layers. Never
/// contains `terminal.*` keys: terminal cell colors come from the terminal
/// palette and escape sequences, never from theme tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTokens {
    entries: BTreeMap<String, ResolvedToken>,
}

impl ResolvedTokens {
    /// Looks up one resolved token by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<ResolvedToken> {
        self.entries.get(name).copied()
    }

    /// Per-key attribution: which layer won `name`, if present.
    #[must_use]
    pub fn layer_of(&self, name: &str) -> Option<TokenLayer> {
        self.entries.get(name).map(|entry| entry.layer)
    }

    /// True when `name` has a resolved value.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Number of resolved tokens.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no token resolved (only reachable from an empty build).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Deterministic iteration in sorted-key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, ResolvedToken)> {
        self.entries
            .iter()
            .map(|(key, entry)| (key.as_str(), *entry))
    }

    /// Resolved focused outline (`border.focused`; always present).
    #[must_use]
    pub fn focused(&self) -> ResolvedToken {
        self.entries[BORDER_FOCUSED]
    }

    /// Resolved idle outline (`border.idle`; always present).
    #[must_use]
    pub fn idle(&self) -> ResolvedToken {
        self.entries[BORDER_IDLE]
    }

    /// Resolved content background (`content.background`; always present).
    #[must_use]
    pub fn content_background(&self) -> ResolvedToken {
        self.entries[CONTENT_BACKGROUND]
    }
}

/// Fail-closed token error; every variant names the offending key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// A non-plugin-namespaced key outside the closed [`CORE_TOKENS`] set.
    /// A plugin may not invent Core tokens.
    UnknownToken {
        /// The offending key.
        key: String,
        /// The layer that supplied it.
        layer: TokenLayer,
    },
    /// A `terminal.*` key: terminal truth is never tokenized.
    TerminalNamespace {
        /// The offending key.
        key: String,
        /// The layer that supplied it.
        layer: TokenLayer,
    },
    /// A plugin package key outside its own `plugin.<name>.*` namespace
    /// (chrome, borders, another plugin's tokens, or Core content/state).
    OutOfNamespace {
        /// The offending key.
        key: String,
        /// The plugin package that supplied it.
        plugin: String,
    },
    /// A value outside the accepted `#RRGGBB[AA]` grammar.
    BadValue {
        /// The offending key.
        key: String,
        /// The rejected raw spelling.
        raw: String,
        /// The layer that supplied it.
        layer: TokenLayer,
    },
    /// One layer supplying the same key twice (conflict is not silent).
    DuplicateKey {
        /// The offending key.
        key: String,
        /// The layer that supplied it twice.
        layer: TokenLayer,
    },
    /// An enforced contrast rule (AC-1/AC-2) violated by the resolved pair.
    Contrast {
        /// `"AC-1"` or `"AC-2"`.
        rule: &'static str,
        /// Human-readable diagnostic naming the keys and resolved values.
        detail: String,
    },
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownToken { key, layer } => write!(
                f,
                "unknown Core token '{key}' from layer '{layer}': \
                 Core tokens are closed; a plugin may not invent one"
            ),
            Self::TerminalNamespace { key, layer } => write!(
                f,
                "terminal key '{key}' from layer '{layer}' is not a theme token: \
                 terminal cell colors come from the palette, never from tokens"
            ),
            Self::OutOfNamespace { key, plugin } => write!(
                f,
                "plugin '{plugin}' may not supply '{key}': a plugin theme package \
                 may claim only its own 'plugin.{plugin}.*' namespace"
            ),
            Self::BadValue { key, raw, layer } => write!(
                f,
                "token '{key}' from layer '{layer}' rejects value '{raw}': \
                 expected '#RRGGBB' or '#RRGGBBAA'"
            ),
            Self::DuplicateKey { key, layer } => write!(
                f,
                "token '{key}' supplied twice in layer '{layer}': \
                 one layer must claim a key at most once"
            ),
            Self::Contrast { rule, detail } => {
                write!(f, "contrast {rule} violation: {detail}")
            }
        }
    }
}

impl std::error::Error for TokenError {}

/// Advisory contrast note (currently AC-3 only): reported, never fail-closed.
#[derive(Debug, Clone, PartialEq)]
pub struct ContrastNote {
    /// `"AC-3"`.
    pub rule: &'static str,
    /// Human-readable diagnostic naming the keys and resolved values.
    pub detail: String,
    /// Measured contrast ratio.
    pub ratio: f64,
    /// Floor the measurement is advisory against.
    pub minimum: f64,
}

impl fmt::Display for ContrastNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "contrast {} advisory: {} (measured {:.2}:1, floor {:.2}:1)",
            self.rule, self.detail, self.ratio, self.minimum
        )
    }
}

/// WCAG 2.1 relative luminance of an opaque `sRGB` byte triple (accepted
/// computation, shared with the outline and preset validation).
#[must_use]
pub fn relative_luminance(rgb: [u8; 3]) -> f64 {
    let channel = |c: u8| -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.212_6 * channel(rgb[0]) + 0.715_2 * channel(rgb[1]) + 0.072_2 * channel(rgb[2])
}

/// WCAG 2.1 contrast ratio between two opaque colors (`>= 1.0`).
#[must_use]
pub fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// True for `terminal.*` keys, which are never theme tokens.
#[must_use]
pub fn is_terminal_key(name: &str) -> bool {
    name == "terminal" || name.starts_with("terminal.")
}

/// True for well-formed `plugin.<name>.<token>` names (all segments
/// non-empty).
fn is_plugin_key(name: &str) -> bool {
    let mut parts = name.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("plugin"), Some(ns), Some(_), None) => !ns.is_empty(),
        // Deeper plugin token paths (`plugin.<name>.a.b`) stay allowed: the
        // plugin owns everything under its namespace.
        (Some("plugin"), Some(ns), Some(_), Some(_)) => !ns.is_empty() && !name.contains(".."),
        _ => false,
    }
}

/// Plugin namespace embedded in a well-formed plugin key.
fn plugin_namespace(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("plugin.")?;
    let end = rest.find('.')?;
    let ns = &rest[..end];
    (!ns.is_empty()).then_some(ns)
}

fn check_key_for_layer(name: &str, layer: TokenLayer) -> Result<(), TokenError> {
    if is_terminal_key(name) {
        return Err(TokenError::TerminalNamespace {
            key: name.to_owned(),
            layer,
        });
    }
    if framework_default(name).is_some() || is_plugin_key(name) {
        Ok(())
    } else {
        Err(TokenError::UnknownToken {
            key: name.to_owned(),
            layer,
        })
    }
}

fn parse_layer(
    pairs: &[(&str, &str)],
    layer: TokenLayer,
    out: &mut BTreeMap<String, ResolvedToken>,
) -> Result<(), TokenError> {
    let mut seen: Vec<&str> = Vec::with_capacity(pairs.len());
    for (name, raw) in pairs {
        if seen.contains(name) {
            return Err(TokenError::DuplicateKey {
                key: (*name).to_owned(),
                layer,
            });
        }
        seen.push(name);
        check_key_for_layer(name, layer)?;
        let value = TokenColor::parse(raw).ok_or_else(|| TokenError::BadValue {
            key: (*name).to_owned(),
            raw: (*raw).to_owned(),
            layer,
        })?;
        out.insert((*name).to_owned(), ResolvedToken { value, layer });
    }
    Ok(())
}

/// Validates one plugin theme package's keys against the plugin boundary.
///
/// A package named `plugin` may claim exactly its own `plugin.<name>.*`
/// namespace: `chrome.*`, `border.*`, `content.*`, `state.*`,
/// `plugin.<other>.*`, and `terminal.*` keys are rejected fail-closed with a
/// diagnostic naming the key. Values are grammar-checked here as well so a
/// malformed package fails before it can shadow anything.
pub fn check_plugin_keys(plugin: &str, keys: &[(&str, &str)]) -> Result<(), TokenError> {
    if plugin.is_empty() || plugin.contains('.') {
        return Err(TokenError::OutOfNamespace {
            key: format!("plugin.{plugin}"),
            plugin: plugin.to_owned(),
        });
    }
    let mut seen: Vec<&str> = Vec::with_capacity(keys.len());
    for (name, raw) in keys {
        if seen.contains(name) {
            return Err(TokenError::DuplicateKey {
                key: (*name).to_owned(),
                layer: TokenLayer::PluginPackage,
            });
        }
        seen.push(name);
        if is_terminal_key(name) {
            return Err(TokenError::TerminalNamespace {
                key: (*name).to_owned(),
                layer: TokenLayer::PluginPackage,
            });
        }
        let owned = plugin_namespace(name).is_some_and(|ns| ns == plugin);
        if !owned {
            return Err(TokenError::OutOfNamespace {
                key: (*name).to_owned(),
                plugin: plugin.to_owned(),
            });
        }
        if TokenColor::parse(raw).is_none() {
            return Err(TokenError::BadValue {
                key: (*name).to_owned(),
                raw: (*raw).to_owned(),
                layer: TokenLayer::PluginPackage,
            });
        }
    }
    Ok(())
}

/// Resolves the full token set across the candidate layers (later wins).
///
/// - `preset`: theme preset token values selected by `appearance.theme`.
/// - `plugin`: optional plugin theme package as `(name, values)`; only its
///   own `plugin.<name>.*` namespace is accepted, and Core chrome/borders
///   structurally never consult it.
/// - `user`: explicit user keys under the accepted appearance surface.
/// - `safe`: `bitty --safe`; when true the forced outline pair
///   ([`SAFE_BORDER_FOCUSED`] / [`SAFE_BORDER_IDLE`]) ignores user and preset
///   values for `border.focused` / `border.idle` and wins with
///   [`TokenLayer::SafeForced`] attribution.
///
/// Fails closed on unknown Core tokens, `terminal.*` keys, out-of-namespace
/// plugin keys, malformed values, and keys claimed twice in one layer.
/// Contrast is a separate step ([`validate_contrast`]); use
/// [`resolve_validated`] for the composed fail-closed path.
pub fn resolve(
    preset: &[(&str, &str)],
    plugin: Option<(&str, &[(&str, &str)])>,
    user: &[(&str, &str)],
    safe: bool,
) -> Result<ResolvedTokens, TokenError> {
    let mut entries: BTreeMap<String, ResolvedToken> = BTreeMap::new();
    for name in CORE_TOKENS {
        if let Some(value) = framework_default(name) {
            entries.insert(
                (*name).to_owned(),
                ResolvedToken {
                    value,
                    layer: TokenLayer::FrameworkDefault,
                },
            );
        }
    }
    parse_layer(preset, TokenLayer::ThemePreset, &mut entries)?;
    if let Some((name, values)) = plugin {
        check_plugin_keys(name, values)?;
        parse_layer(values, TokenLayer::PluginPackage, &mut entries)?;
    }
    parse_layer(user, TokenLayer::UserKeys, &mut entries)?;
    if safe {
        for (name, value) in [
            (BORDER_FOCUSED, SAFE_BORDER_FOCUSED),
            (BORDER_IDLE, SAFE_BORDER_IDLE),
        ] {
            entries.insert(
                name.to_owned(),
                ResolvedToken {
                    value,
                    layer: TokenLayer::SafeForced,
                },
            );
        }
    }
    Ok(ResolvedTokens { entries })
}

/// Validates the resolved outline pair against AC-1/AC-2/AC-3.
///
/// AC-1 (focused vs background) and AC-2 (focused vs idle) violations are
/// rejected fail-closed with a diagnostic naming the rule, the keys, and the
/// resolved values. AC-3 (idle vs background) is advisory: a violation is
/// returned as a [`ContrastNote`] without failing. Translucent outlines are
/// composited over the resolved background first, matching the accepted
/// outline computation.
pub fn validate_contrast(tokens: &ResolvedTokens) -> Result<Vec<ContrastNote>, TokenError> {
    let background = tokens.content_background().value.rgb();
    let focused = tokens.focused().value;
    let idle = tokens.idle().value;
    let focused_flat = focused.composited_over(background);
    let idle_flat = idle.composited_over(background);

    let ac1 = contrast_ratio(focused_flat, background);
    if ac1 < AC1_FOCUSED_VS_BACKGROUND_MIN {
        return Err(TokenError::Contrast {
            rule: "AC-1",
            detail: format!(
                "'{BORDER_FOCUSED}' ({}) vs '{CONTENT_BACKGROUND}' ({}) \
                 measures {ac1:.2}:1, below {AC1_FOCUSED_VS_BACKGROUND_MIN:.1}:1",
                focused,
                tokens.content_background().value,
            ),
        });
    }

    let ac2 = contrast_ratio(focused_flat, idle_flat);
    if ac2 < AC2_FOCUSED_VS_IDLE_MIN {
        return Err(TokenError::Contrast {
            rule: "AC-2",
            detail: format!(
                "'{BORDER_FOCUSED}' ({focused}) vs '{BORDER_IDLE}' ({idle}) \
                 measures {ac2:.2}:1, below {AC2_FOCUSED_VS_IDLE_MIN:.1}:1",
                focused = focused,
                idle = idle,
            ),
        });
    }

    let mut notes = Vec::new();
    let ac3 = contrast_ratio(idle_flat, background);
    if ac3 < AC3_IDLE_VS_BACKGROUND_MIN {
        notes.push(ContrastNote {
            rule: "AC-3",
            detail: format!(
                "'{BORDER_IDLE}' ({idle}) vs '{CONTENT_BACKGROUND}' ({}) \
                 measures {ac3:.2}:1",
                tokens.content_background().value,
            ),
            ratio: ac3,
            minimum: AC3_IDLE_VS_BACKGROUND_MIN,
        });
    }
    Ok(notes)
}

/// Resolves and then contrast-validates (AC-1/AC-2 enforced, AC-3 advisory).
///
/// Returns the resolved set plus any advisory notes. This is the
/// `ConfigPlan`-validation-shaped path: a theme preset that produces a
/// violating pair is rejected here with a diagnostic naming the key and the
/// resolved values.
pub fn resolve_validated(
    preset: &[(&str, &str)],
    plugin: Option<(&str, &[(&str, &str)])>,
    user: &[(&str, &str)],
    safe: bool,
) -> Result<(ResolvedTokens, Vec<ContrastNote>), TokenError> {
    let tokens = resolve(preset, plugin, user, safe)?;
    let notes = validate_contrast(&tokens)?;
    Ok((tokens, notes))
}

/// Advisory contrast check for one plugin content token.
///
/// A plugin-supplied token that produces a violating pair inside the plugin's
/// own content surface is reported (returned note) and degraded to
/// [`PLUGIN_TOKEN_FALLBACK`] rather than failing the panel. The candidate
/// floor is the content-text floor only as an advisory signal: a low ratio
/// means "replaced with the fallback", never an error.
#[must_use]
pub fn degrade_plugin_token(
    name: &str,
    value: TokenColor,
    background: TokenColor,
) -> (TokenColor, Option<ContrastNote>) {
    let ratio = contrast_ratio(value.composited_over(background.rgb()), background.rgb());
    if ratio < AC3_IDLE_VS_BACKGROUND_MIN {
        let note = ContrastNote {
            rule: "AC-3",
            detail: format!(
                "plugin token '{name}' ({value}) vs panel background ({background}) \
                 measures {ratio:.2}:1; degraded to the compliant default \
                 ({PLUGIN_TOKEN_FALLBACK})"
            ),
            ratio,
            minimum: AC3_IDLE_VS_BACKGROUND_MIN,
        };
        (PLUGIN_TOKEN_FALLBACK, Some(note))
    } else {
        (value, None)
    }
}
