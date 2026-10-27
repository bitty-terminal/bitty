//! Bounded configuration writes for the `config.manage` head (CTX-1613,
//! bitty#2393).
//!
//! OQ-D1 (resolved): configuration reads stay on the accepted
//! `runtime.inspect` query, so `config.manage` is a **write-only** head in a
//! new `config` family. A read grant never widens into a write and the write
//! grant never widens into a read.
//!
//! The write contract is deliberately narrow:
//!
//! - one allowlisted dotted key and one typed scalar value per call (no loose
//!   key/value table, no path, no argv, no URL);
//! - consent is hash-bound: `preview` renders the exact change and returns a
//!   SHA-256 digest; `set` accepts only a digest it issued for the same
//!   `(plugin, key, value)` triple in this generation, and consumes it;
//! - calls, bytes, and wall time are budgeted per generation;
//! - safe mode denies before any state is read;
//! - the durable write is delegated to a [`ConfigWriteBackend`] that only
//!   reports a path-free [`ConfigWriteFailure`], so plugin-visible diagnostics
//!   never contain host paths.
//!
//! The user-layer target, the strict loader, and the atomic replace live in
//! the application backend (`bitty`'s `UserLayerConfigWriter`); this module
//! owns validation, consent, and bounds only.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bitty_lua::{BridgeError, LuaValue};

/// Maximum dotted key bytes accepted by `config.manage`.
pub const CONFIG_SET_MAX_KEY_BYTES: usize = 64;

/// Maximum text value bytes accepted by `config.manage` (1 KiB).
pub const CONFIG_SET_MAX_VALUE_BYTES: usize = 1024;

/// Maximum rendered `key = value` bytes (escaped text can double).
pub const CONFIG_SET_MAX_RENDERED_BYTES: usize =
    CONFIG_SET_MAX_KEY_BYTES + (CONFIG_SET_MAX_VALUE_BYTES * 2) + 8;

/// Maximum `config.manage` calls per generation per second (preview + set).
pub const CONFIG_SET_CALLS_PER_SECOND: u32 = 16;

/// Maximum rendered bytes one generation may submit per second.
pub const CONFIG_SET_BYTES_PER_SECOND: usize = 4096;

/// Maximum rows any `config.manage` result may carry.
///
/// This slice returns exactly one row per call (a preview or an applied
/// change); the constant is the fixed ceiling future list methods must
/// respect and is enforced before every result is built.
pub const CONFIG_SET_MAX_ROWS: usize = 64;

/// Maximum outstanding preview digests retained per generation.
pub const CONFIG_SET_MAX_OUTSTANDING_PREVIEWS: usize = 8;

/// Host-side wall budget for the authorization path of one call.
///
/// The durable write itself is bounded by the call/byte budgets, not by a
/// deadline: an `fsync` cannot be cancelled safely, and refusing after the
/// rename would report a failed write that actually landed.
pub const CONFIG_SET_MAX_WALL_MS: u64 = 10;

/// Rate window for [`CONFIG_SET_CALLS_PER_SECOND`] and
/// [`CONFIG_SET_BYTES_PER_SECOND`].
pub const CONFIG_SET_RATE_WINDOW: Duration = Duration::from_secs(1);

/// Consent-digest domain separator (versioned so the preimage can never be
/// confused with another SHA-256 use).
pub const CONFIG_SET_DIGEST_DOMAIN: &str = "bitty-config-manage/v1";

/// Value kind of one writable setting (mirrors the typed config schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigValueKind {
    /// Free text (the strict loader validates enums/ranges).
    Text,
    /// Non-negative integer.
    Integer,
    /// Finite number.
    Number,
    /// Boolean.
    Bool,
}

impl ConfigValueKind {
    /// Stable kind label used in the consent preimage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Bool => "bool",
        }
    }
}

/// One allowlisted setting the plugin surface may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigSettingKey {
    /// Canonical dotted key (same spelling `config check` prints).
    pub key: &'static str,
    /// Accepted value kind.
    pub kind: ConfigValueKind,
}

/// Scalar settings writable through `config.manage`.
///
/// Deliberate omissions:
///
/// - `terminal.shell` is a command string: a plugin must not repoint the
///   shell the next launch executes.
/// - `mod_key`/`mouse.right`/`mouse.middle` are canonicalizing input
///   aliases; the write path compares byte-exact values, so they stay
///   CLI-only.
/// - list-valued (`font.fallback`) and nullable
///   (`session.restore_on_startup`) settings need a richer argument shape
///   than one scalar and stay CLI-only.
/// - Profiles, the managed plugin ledger, and system paths are never
///   addressable here: there is no parameter for a target file.
pub const CONFIG_WRITABLE_SETTINGS: &[ConfigSettingKey] = &[
    ConfigSettingKey {
        key: "appearance.theme",
        kind: ConfigValueKind::Text,
    },
    ConfigSettingKey {
        key: "font.family",
        kind: ConfigValueKind::Text,
    },
    ConfigSettingKey {
        key: "font.size",
        kind: ConfigValueKind::Number,
    },
    ConfigSettingKey {
        key: "font.line_height",
        kind: ConfigValueKind::Number,
    },
    ConfigSettingKey {
        key: "font.letter_spacing",
        kind: ConfigValueKind::Number,
    },
    ConfigSettingKey {
        key: "window.opacity",
        kind: ConfigValueKind::Number,
    },
    ConfigSettingKey {
        key: "window.padding",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "window.radius_px",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "terminal.scrollback",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "terminal.scroll_lines_per_notch",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "terminal.scroll_pixels_per_notch",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "terminal.page_scroll_percent",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "terminal.cursor_style",
        kind: ConfigValueKind::Text,
    },
    ConfigSettingKey {
        key: "terminal.bell",
        kind: ConfigValueKind::Text,
    },
    ConfigSettingKey {
        key: "terminal.ambiguous_width",
        kind: ConfigValueKind::Text,
    },
    ConfigSettingKey {
        key: "selection.auto_copy",
        kind: ConfigValueKind::Bool,
    },
    ConfigSettingKey {
        key: "history.capture.transcript",
        kind: ConfigValueKind::Bool,
    },
    ConfigSettingKey {
        key: "history.capture.commands",
        kind: ConfigValueKind::Bool,
    },
    ConfigSettingKey {
        key: "layout.gaps_in",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "layout.gaps_out",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "layout.resize_step",
        kind: ConfigValueKind::Number,
    },
    ConfigSettingKey {
        key: "decoration.gaps_in",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "decoration.gaps_out",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "decoration.border",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "decoration.radius",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "decoration.content_inset",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "scrollbar.mode",
        kind: ConfigValueKind::Text,
    },
    ConfigSettingKey {
        key: "scrollbar.width",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "mouse.focus_follows_mouse",
        kind: ConfigValueKind::Bool,
    },
    ConfigSettingKey {
        key: "mouse.focus_follows_mouse_delay_ms",
        kind: ConfigValueKind::Integer,
    },
    ConfigSettingKey {
        key: "close_confirm",
        kind: ConfigValueKind::Text,
    },
    ConfigSettingKey {
        key: "appearance.animations.enabled",
        kind: ConfigValueKind::Bool,
    },
];

/// Resolve an allowlisted setting key.
#[must_use]
pub fn writable_setting(key: &str) -> Option<&'static ConfigSettingKey> {
    if key.is_empty() || key.len() > CONFIG_SET_MAX_KEY_BYTES {
        return None;
    }
    CONFIG_WRITABLE_SETTINGS
        .iter()
        .find(|setting| setting.key == key)
}

/// One typed configuration value.
///
/// The value is validated for shape here; semantic validation (theme
/// resolution, ranges, keymaps, image roots) is the strict loader's job in
/// the backend, before any byte is replaced.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigSettingValue {
    /// Free text.
    Text(String),
    /// Non-negative integer.
    Integer(u32),
    /// Finite `f32`.
    Number(f32),
    /// Boolean.
    Bool(bool),
}

impl ConfigSettingValue {
    /// Build a text value under the shared bounds.
    pub fn text(raw: &str) -> Result<Self, BridgeError> {
        if raw.len() > CONFIG_SET_MAX_VALUE_BYTES {
            return Err(config_value_error("config value exceeds 1024 bytes"));
        }
        if raw.trim().is_empty() {
            return Err(config_value_error("config value must be non-empty"));
        }
        if raw.chars().any(char::is_control) {
            return Err(config_value_error(
                "config value must not contain control characters",
            ));
        }
        Ok(Self::Text(raw.to_string()))
    }

    /// Build an integer value (`u32`, matching the typed schema).
    pub fn integer(raw: i64) -> Result<Self, BridgeError> {
        u32::try_from(raw)
            .map(Self::Integer)
            .map_err(|_| config_value_error("config value is not a non-negative 32-bit integer"))
    }

    /// Build a finite number value.
    pub fn number(raw: f64) -> Result<Self, BridgeError> {
        #[allow(clippy::cast_possible_truncation)]
        let value = raw as f32;
        if !value.is_finite() {
            return Err(config_value_error("config value is not a finite number"));
        }
        Ok(Self::Number(value))
    }

    /// Build a boolean value.
    #[must_use]
    pub const fn boolean(raw: bool) -> Self {
        Self::Bool(raw)
    }

    /// Parse one Lua scalar into the setting's value kind.
    ///
    /// Tables, functions, `nil`, and mismatched scalar types are refused
    /// before any allowlist lookup or host call.
    pub fn from_lua(kind: ConfigValueKind, raw: &LuaValue) -> Result<Self, BridgeError> {
        match (kind, raw) {
            (ConfigValueKind::Text, LuaValue::String(text)) => Self::text(text),
            (ConfigValueKind::Integer, LuaValue::Integer(value)) => Self::integer(*value),
            (ConfigValueKind::Number, LuaValue::Integer(value)) => Self::number(*value as f64),
            (ConfigValueKind::Number, LuaValue::Number(value)) => Self::number(*value),
            (ConfigValueKind::Bool, LuaValue::Bool(value)) => Ok(Self::boolean(*value)),
            _ => Err(config_value_error(
                "config value has the wrong type for this setting",
            )),
        }
    }

    /// The setting kind this value satisfies.
    #[must_use]
    pub const fn kind(&self) -> ConfigValueKind {
        match self {
            Self::Text(_) => ConfigValueKind::Text,
            Self::Integer(_) => ConfigValueKind::Integer,
            Self::Number(_) => ConfigValueKind::Number,
            Self::Bool(_) => ConfigValueKind::Bool,
        }
    }

    /// Render the exact Lua literal the backend writes and the user approves.
    ///
    /// Both the backend and the consent digest use this one function, so the
    /// approved rendering and the persisted bytes cannot diverge.
    #[must_use]
    pub fn render_literal(&self) -> String {
        match self {
            Self::Text(text) => escape_lua_string(text),
            Self::Integer(value) => value.to_string(),
            Self::Number(value) => format!("{value}"),
            Self::Bool(value) => value.to_string(),
        }
    }
}

/// Quote one Lua short string (`"..."`), escaping quotes, backslashes, and
/// controls. Values are already control-free; the escape stays here so the
/// literal can never break out of the string.
fn escape_lua_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other if other.is_control() => {
                let code = u32::from(other);
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{{{code:x}}}");
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// One validated, rendered configuration change.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigChange {
    setting: &'static ConfigSettingKey,
    value: ConfigSettingValue,
    /// `key = literal`, byte-identical to what the backend persists.
    rendered: String,
}

impl ConfigChange {
    /// Build one rendered change from an allowlisted setting and typed value.
    #[must_use]
    pub fn new(setting: &'static ConfigSettingKey, value: ConfigSettingValue) -> Self {
        let rendered = format!("{} = {}", setting.key, value.render_literal());
        Self {
            setting,
            value,
            rendered,
        }
    }

    /// Canonical dotted key.
    #[must_use]
    pub fn key(&self) -> &'static str {
        self.setting.key
    }

    /// Setting metadata (key + kind).
    #[must_use]
    pub fn setting(&self) -> &'static ConfigSettingKey {
        self.setting
    }

    /// Typed value.
    #[must_use]
    pub fn value(&self) -> &ConfigSettingValue {
        &self.value
    }

    /// `key = literal` rendering shown to the user and persisted.
    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }
}

/// One issued preview: the exact change the user is asked to approve.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigPreview {
    /// SHA-256 hex of the consent preimage.
    pub digest: String,
    /// `key = literal` rendering.
    pub rendered: String,
}

/// Result of one accepted `config.set`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSetOutcome {
    /// Whether the durable user layer changed (`false` for a no-op).
    pub applied: bool,
    /// Reload marker: `auto` means the app's config watcher re-reads the
    /// user layer on the next tick; `none` means nothing changed.
    pub reload: &'static str,
}

/// Why a backend refused a write. Carries no message and no path: the
/// plugin-visible diagnostic is assembled host-side from this class only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigWriteFailure {
    /// The strict loader rejected the candidate (schema/enum/range/keymaps).
    Invalid,
    /// The backend has no writable user layer (read-only host, safe mode,
    /// or `--config` outside the user root).
    Unavailable,
    /// The target is not a regular owner-controlled file (symlink, FIFO,
    /// directory, foreign owner), or the write would leave the user layer.
    Unsafe,
    /// Atomic replace failed; the previous bytes are intact.
    Io,
}

impl ConfigWriteFailure {
    /// Stable path-free error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Invalid => "E_CONFIG_INVALID",
            Self::Unavailable => "E_CONFIG_UNAVAILABLE",
            Self::Unsafe => "E_CONFIG_UNSAFE",
            Self::Io => "E_CONFIG_IO",
        }
    }

    /// Stable path-free message.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Invalid => "the configuration change failed validation",
            Self::Unavailable => "no writable user configuration layer is available",
            Self::Unsafe => "the user configuration target is not a safe regular file",
            Self::Io => "the atomic user configuration replace failed",
        }
    }
}

/// Backend contract for one durable, validated user-layer change.
///
/// Implementations must re-validate the change through the strict loader and
/// commit atomically (temp + fsync + rename + directory fsync, owner-only
/// mode, `.bak` of the previous bytes). A returned error must leave the
/// previous bytes untouched.
pub trait ConfigWriteBackend {
    /// Apply one change; `Unchanged` means the value already matched.
    ///
    /// # Errors
    ///
    /// Returns a path-free [`ConfigWriteFailure`].
    fn apply(&self, change: &ConfigChange) -> Result<ConfigApplyOutcome, ConfigWriteFailure>;
}

/// Backend result for one change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigApplyOutcome {
    /// The user layer changed on disk.
    Applied,
    /// The requested value already matched; nothing was written.
    Unchanged,
}

/// Per-generation rate window and consent-preview table.
#[derive(Debug)]
pub struct ConfigWriteGate {
    window_start: Instant,
    calls: u32,
    bytes: usize,
    issued: VecDeque<IssuedPreview>,
}

#[derive(Debug, Clone, PartialEq)]
struct IssuedPreview {
    digest: String,
    key: &'static str,
    value: ConfigSettingValue,
}

impl ConfigWriteGate {
    /// Create an empty gate whose first call starts the window.
    #[must_use]
    pub fn new(window_start: Instant) -> Self {
        Self {
            window_start,
            calls: 0,
            bytes: 0,
            issued: VecDeque::new(),
        }
    }

    /// Charge one call and its rendered bytes.
    ///
    /// Returns `false` without charging when the one-second window is
    /// exhausted; nothing is clamped and no state changes.
    pub fn charge(&mut self, now: Instant, bytes: usize) -> bool {
        if now.duration_since(self.window_start) >= CONFIG_SET_RATE_WINDOW {
            self.window_start = now;
            self.calls = 0;
            self.bytes = 0;
        }
        let next_bytes = self.bytes.saturating_add(bytes);
        if self.calls >= CONFIG_SET_CALLS_PER_SECOND || next_bytes > CONFIG_SET_BYTES_PER_SECOND {
            return false;
        }
        self.calls += 1;
        self.bytes = next_bytes;
        true
    }

    /// Remember one issued preview (bounded, drop-oldest).
    pub fn issue(&mut self, digest: String, key: &'static str, value: ConfigSettingValue) {
        if self.issued.len() >= CONFIG_SET_MAX_OUTSTANDING_PREVIEWS {
            self.issued.pop_front();
        }
        self.issued.push_back(IssuedPreview { digest, key, value });
    }

    /// Consume the digest issued for exactly this `(key, value)` triple.
    ///
    /// One digest authorizes one write: a second `set` with the same token is
    /// refused, so a stale row cannot be replayed.
    pub fn consume(&mut self, digest: &str, key: &str, value: &ConfigSettingValue) -> bool {
        let Some(index) = self.issued.iter().position(|issued| {
            issued.digest == digest && issued.key == key && issued.value == *value
        }) else {
            return false;
        };
        self.issued.remove(index);
        true
    }
}

/// Number of outstanding previews (tests/diagnostics).
#[must_use]
pub fn outstanding_previews(gate: &ConfigWriteGate) -> usize {
    gate.issued.len()
}

/// Build the versioned consent preimage for one change.
#[must_use]
pub fn consent_preimage(plugin_id: &str, change: &ConfigChange) -> String {
    format!(
        "{CONFIG_SET_DIGEST_DOMAIN}\0{plugin_id}\0{}\0{}\0{}",
        change.key(),
        change.value().kind().as_str(),
        change.rendered()
    )
}

/// SHA-256 consent digest over [`consent_preimage`].
#[must_use]
pub fn consent_digest(plugin_id: &str, change: &ConfigChange) -> String {
    bitty_package::integrity::sha256_hex(consent_preimage(plugin_id, change).as_bytes())
}

/// Validate the one-row result bound before a result is built.
pub fn check_result_rows(rows: usize) -> Result<(), BridgeError> {
    if rows == 0 || rows > CONFIG_SET_MAX_ROWS {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_LIMIT",
            "config.manage result exceeds its row bound",
        ));
    }
    Ok(())
}

/// Typed value-shape error (never echoes the rejected value).
#[must_use]
pub fn config_value_error(message: &str) -> BridgeError {
    BridgeError::new("validation", "E_CONFIG_VALUE", message)
}

/// Typed unknown-key error (never echoes the rejected key).
#[must_use]
pub fn config_key_error() -> BridgeError {
    BridgeError::new(
        "validation",
        "E_CONFIG_KEY",
        "config path is not in the config.manage allowlist",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(key: &str) -> &'static ConfigSettingKey {
        writable_setting(key).expect("allowlisted")
    }

    #[test]
    fn allowlist_is_unique_and_bounded() {
        let mut keys: Vec<&str> = CONFIG_WRITABLE_SETTINGS.iter().map(|s| s.key).collect();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), total, "allowlist keys must be unique");
        assert!(total <= CONFIG_SET_MAX_ROWS);
        for key in &keys {
            assert!(!key.is_empty());
            assert!(key.len() <= CONFIG_SET_MAX_KEY_BYTES, "{key} too long");
            assert!(
                key.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'.'
                    || b == b'_'),
                "{key} must stay dotted lowercase"
            );
        }
        // Storage/input-adjacent keys are deliberately outside the surface.
        for forbidden in [
            "terminal.shell",
            "mod_key",
            "mouse.right",
            "mouse.middle",
            "font.fallback",
            "session.restore_on_startup",
        ] {
            assert!(
                writable_setting(forbidden).is_none(),
                "{forbidden} must stay CLI-only"
            );
        }
    }

    #[test]
    fn unknown_and_oversized_keys_refuse_without_echo() {
        assert!(writable_setting("").is_none());
        assert!(writable_setting("appearance").is_none());
        assert!(writable_setting("appearance.theme ").is_none());
        assert!(writable_setting("Appearance.Theme").is_none());
        assert!(writable_setting("../init").is_none());
        assert!(writable_setting("/etc/bitty/init.lua").is_none());
        let oversized = "a".repeat(CONFIG_SET_MAX_KEY_BYTES + 1);
        assert!(writable_setting(&oversized).is_none());
        let error = config_key_error();
        assert_eq!(error.code, "E_CONFIG_KEY");
        assert!(!error.message.contains('/'));
    }

    #[test]
    fn values_are_bounded_and_typed() {
        assert!(ConfigSettingValue::text("").is_err());
        assert!(ConfigSettingValue::text("   ").is_err());
        assert!(ConfigSettingValue::text("line\nbreak").is_err());
        assert!(ConfigSettingValue::text(&"x".repeat(CONFIG_SET_MAX_VALUE_BYTES)).is_ok());
        assert!(ConfigSettingValue::text(&"x".repeat(CONFIG_SET_MAX_VALUE_BYTES + 1)).is_err());
        assert!(ConfigSettingValue::integer(-1).is_err());
        assert!(ConfigSettingValue::integer(i64::from(u32::MAX)).is_ok());
        assert!(ConfigSettingValue::number(f64::NAN).is_err());
        assert!(ConfigSettingValue::number(f64::INFINITY).is_err());
        assert_eq!(
            ConfigSettingValue::number(1.2)
                .expect("finite")
                .render_literal(),
            "1.2"
        );
        // A hostile-looking text value remains text: it never becomes code,
        // and the literal quotes/escapes it.
        let hostile = ConfigSettingValue::text("\"; os.execute('id'); --").expect("text");
        assert_eq!(hostile.render_literal(), "\"\\\"; os.execute('id'); --\"");
        // Wrong scalar types are refused before any host call.
        assert!(
            ConfigSettingValue::from_lua(ConfigValueKind::Bool, &LuaValue::Integer(1)).is_err()
        );
        assert!(
            ConfigSettingValue::from_lua(ConfigValueKind::Text, &LuaValue::Table(Vec::new()))
                .is_err()
        );
        assert!(
            ConfigSettingValue::from_lua(ConfigValueKind::Number, &LuaValue::Integer(2)).is_ok()
        );
    }

    #[test]
    fn rendered_change_is_key_equals_literal() {
        let change = ConfigChange::new(
            setting("appearance.theme"),
            ConfigSettingValue::text("catppuccin").expect("text"),
        );
        assert_eq!(change.rendered(), "appearance.theme = \"catppuccin\"");
        assert_eq!(change.key(), "appearance.theme");
        assert_eq!(change.value().kind(), ConfigValueKind::Text);
    }

    #[test]
    fn consent_digest_binds_plugin_key_value_and_rendering() {
        let theme = setting("appearance.theme");
        let change =
            ConfigChange::new(theme, ConfigSettingValue::text("catppuccin").expect("text"));
        let digest = consent_digest("bitty-terminal.dashboard", &change);
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(digest, consent_digest("bitty-terminal.dashboard", &change));
        assert_ne!(digest, consent_digest("other.plugin", &change));

        let other_value =
            ConfigChange::new(theme, ConfigSettingValue::text("dracula").expect("text"));
        assert_ne!(
            digest,
            consent_digest("bitty-terminal.dashboard", &other_value)
        );

        let other_key = ConfigChange::new(
            setting("font.family"),
            ConfigSettingValue::text("catppuccin").expect("text"),
        );
        assert_ne!(
            consent_digest("bitty-terminal.dashboard", &other_key),
            digest
        );

        // Kind is part of the preimage: `1` text and `1` number differ even
        // though a naive rendering could look similar.
        let text_one = ConfigChange::new(
            setting("font.family"),
            ConfigSettingValue::text("1").expect("text"),
        );
        let number_one = ConfigChange::new(
            setting("font.size"),
            ConfigSettingValue::number(1.0).expect("number"),
        );
        assert_ne!(
            consent_preimage("p", &text_one),
            consent_preimage("p", &number_one)
        );

        // The preimage carries the rendered change, not a plugin-supplied
        // spelling: whitespace/quoting variants produce different digests.
        let padded = ConfigChange::new(
            theme,
            ConfigSettingValue::text(" catppuccin").expect("text"),
        );
        assert_ne!(digest, consent_digest("bitty-terminal.dashboard", &padded));
    }

    #[test]
    fn gate_enforces_call_and_byte_budgets_per_window() {
        let start = Instant::now();
        let mut gate = ConfigWriteGate::new(start);
        for _ in 0..CONFIG_SET_CALLS_PER_SECOND {
            assert!(gate.charge(start, 1));
        }
        assert!(!gate.charge(start, 1), "17th call in the window refuses");
        // The next window resets both counters.
        assert!(gate.charge(start + CONFIG_SET_RATE_WINDOW, 1));

        let mut bytes_gate = ConfigWriteGate::new(start);
        assert!(bytes_gate.charge(start, CONFIG_SET_BYTES_PER_SECOND));
        assert!(
            !bytes_gate.charge(start, 1),
            "byte window refuses oversubscription"
        );
        assert!(
            bytes_gate.charge(start + CONFIG_SET_RATE_WINDOW, 1),
            "a fresh window charges again"
        );
    }

    #[test]
    fn previews_are_one_shot_and_bounded() {
        let start = Instant::now();
        let mut gate = ConfigWriteGate::new(start);
        let value = ConfigSettingValue::text("catppuccin").expect("text");
        let change = ConfigChange::new(setting("appearance.theme"), value.clone());
        let digest = consent_digest("p", &change);
        gate.issue(digest.clone(), change.key(), value.clone());
        assert_eq!(outstanding_previews(&gate), 1);
        assert!(gate.consume(&digest, change.key(), &value));
        assert!(
            !gate.consume(&digest, change.key(), &value),
            "tokens are one-shot"
        );
        assert_eq!(outstanding_previews(&gate), 0);

        for index in 0..CONFIG_SET_MAX_OUTSTANDING_PREVIEWS + 4 {
            let value = ConfigSettingValue::text(&format!("theme{index}")).expect("text");
            let change = ConfigChange::new(setting("appearance.theme"), value.clone());
            gate.issue(consent_digest("p", &change), change.key(), value);
        }
        assert_eq!(
            outstanding_previews(&gate),
            CONFIG_SET_MAX_OUTSTANDING_PREVIEWS,
            "old previews drop instead of growing the table"
        );
    }

    #[test]
    fn result_rows_are_bounded() {
        assert!(check_result_rows(1).is_ok());
        assert!(check_result_rows(CONFIG_SET_MAX_ROWS).is_ok());
        assert!(check_result_rows(0).is_err());
        assert!(check_result_rows(CONFIG_SET_MAX_ROWS + 1).is_err());
    }

    #[test]
    fn failure_codes_are_path_free() {
        for failure in [
            ConfigWriteFailure::Invalid,
            ConfigWriteFailure::Unavailable,
            ConfigWriteFailure::Unsafe,
            ConfigWriteFailure::Io,
        ] {
            assert!(!failure.message().contains('/'));
            assert!(!failure.message().contains("~"));
            assert!(failure.code().starts_with("E_CONFIG_"));
        }
    }
}
