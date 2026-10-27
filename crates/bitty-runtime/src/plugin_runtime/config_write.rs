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
//! - approval is **token-bound**: the backend renders the exact candidate
//!   user-layer bytes (including pinned-table seeds) and returns the changed
//!   keys plus a bounded exact diff; `preview` hashes that rendering into a
//!   one-shot token, and `set` re-renders and refuses unless the exact bytes
//!   are unchanged (`E_CONFIG_STALE` otherwise);
//! - calls, bytes, wall time, keys, and diff size are budgeted per generation;
//! - safe mode denies before any state is read;
//! - the durable write is delegated to a [`ConfigWriteBackend`] that only
//!   reports a path-free [`ConfigWriteFailure`], so plugin-visible diagnostics
//!   never contain host paths.
//!
//! **The host does not prove human intent.** A granted plugin can call
//! `preview` itself; the host enforces the grant plus a one-shot,
//! value-and-bytes-bound preview token. The two-press confirmation is a
//! dashboard UI contract, recorded separately in the plugin.
//!
//! The user-layer target, the strict loader, and the atomic replace live in
//! the application backend (`bitty`'s `UserLayerConfigWriter`); this module
//! owns validation, approval tokens, and bounds only.

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

/// Maximum keys one preview may list (pinned tables expand to their leaves).
pub const CONFIG_PREVIEW_MAX_KEYS: usize = 64;

/// Maximum exact rendered diff bytes one preview may carry.
///
/// A pinned table literal is bounded by the typed schema; 8 KiB leaves room
/// for the widest table plus the leaf line while staying far below the user
/// layer's 64 KiB file bound.
pub const CONFIG_PREVIEW_MAX_RENDERED_BYTES: usize = 8 * 1024;

/// Maximum diff lines one preview may carry.
pub const CONFIG_PREVIEW_MAX_LINES: usize = 128;

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

/// Approval-digest domain separator (versioned so the preimage can never be
/// confused with another SHA-256 use).
pub const CONFIG_SET_APPROVAL_DOMAIN: &str = "bitty-config-manage/v1";

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
    /// Stable kind label used in the approval preimage.
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
    // `history.capture.*` is deliberately absent: transcript/command capture
    // determines what terminal-derived content is retained, so enabling it is
    // not a presentation setting a plugin may flip. It stays on the explicit
    // CLI/user path (review B5).
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
        // P-3 permits this row. Its preview must state the exact resulting
        // policy (always prompt / never prompt) rather than a bare enum token,
        // because the setting changes whether closing can lose work.
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
    /// Both the backend and the approval digest use this one function, so the
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

/// One backend-rendered candidate: the exact bytes a commit would write.
///
/// The backend owns the user-layer rendering (including pinned-table seeds),
/// so the preview token binds what will actually be committed, not merely the
/// one leaf the plugin named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPreviewRendering {
    /// Every setting leaf whose value changes, sorted and deduplicated.
    ///
    /// A pinned-table seed lists the leaves the seed writes (including leaves
    /// outside the plugin allowlist, so the user sees the full effect).
    pub changed_keys: Vec<&'static str>,
    /// Exact inserted/replaced lines the user approves (bounded).
    pub rendered: String,
    /// SHA-256 of the exact candidate user-layer bytes.
    pub candidate_digest: String,
}

/// One host-issued preview token: the approval a `set` must present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPreviewToken {
    /// SHA-256 hex of the approval preimage (plugin + change + rendering).
    pub digest: String,
    /// Exact rendered diff returned by the backend.
    pub rendered: String,
    /// Setting leaves the candidate changes.
    pub changed_keys: Vec<&'static str>,
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
    /// The candidate bytes moved between preview and commit (a profile or
    /// concurrent edit changed the seed); nothing was written.
    Stale,
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
            Self::Stale => "E_CONFIG_STALE",
            Self::Io => "E_CONFIG_IO",
        }
    }

    /// Stable error class for the bridge table.
    #[must_use]
    pub const fn class(self) -> &'static str {
        match self {
            Self::Invalid => "validation",
            Self::Unavailable | Self::Stale => "runtime",
            Self::Unsafe | Self::Io => "io",
        }
    }

    /// Stable path-free message.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Invalid => "the configuration change failed validation",
            Self::Unavailable => "no writable user configuration layer is available",
            Self::Unsafe => "the user configuration target is not a safe regular file",
            Self::Stale => "the configuration candidate changed since it was previewed",
            Self::Io => "the atomic user configuration replace failed",
        }
    }
}

/// Backend contract for one durable, validated user-layer change.
///
/// `preview` renders the exact candidate bytes without touching disk;
/// `apply` re-renders, refuses with [`ConfigWriteFailure::Stale`] unless the
/// candidate digest matches the approved one, and only then commits
/// atomically (temp + fsync + rename + directory fsync, owner-only mode,
/// rolling `.bak`). A returned error must leave the previous bytes untouched.
pub trait ConfigWriteBackend {
    /// Render the exact candidate for approval, changing nothing.
    ///
    /// # Errors
    ///
    /// Returns a path-free [`ConfigWriteFailure`].
    fn preview(
        &self,
        plugin_id: &str,
        change: &ConfigChange,
    ) -> Result<ConfigPreviewRendering, ConfigWriteFailure>;

    /// Apply one approved change; `Unchanged` means the value already matched.
    ///
    /// Re-renders and compares the candidate digest to `expected` before any
    /// write; a mismatch returns [`ConfigWriteFailure::Stale`].
    ///
    /// # Errors
    ///
    /// Returns a path-free [`ConfigWriteFailure`].
    fn apply(
        &self,
        plugin_id: &str,
        change: &ConfigChange,
        expected_candidate_digest: &str,
    ) -> Result<ConfigApplyOutcome, ConfigWriteFailure>;
}

/// Backend result for one change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigApplyOutcome {
    /// The user layer changed on disk.
    Applied,
    /// The requested value already matched; nothing was written.
    Unchanged,
}

/// Per-generation rate window and one-shot approval table.
#[derive(Debug)]
pub struct ConfigWriteGate {
    window_start: Instant,
    calls: u32,
    bytes: usize,
    issued: VecDeque<IssuedApproval>,
}

#[derive(Debug, Clone, PartialEq)]
struct IssuedApproval {
    digest: String,
    key: &'static str,
    value: ConfigSettingValue,
    candidate_digest: String,
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

    /// Remember one issued approval (bounded, drop-oldest).
    ///
    /// A repeated preview of the same `(key, value)` replaces the outstanding
    /// approval instead of stacking a second token: one exact change can only
    /// ever have one live approval.
    pub fn issue(
        &mut self,
        digest: String,
        key: &'static str,
        value: ConfigSettingValue,
        candidate_digest: String,
    ) {
        self.issued
            .retain(|issued| !(issued.key == key && issued.value == value));
        if self.issued.len() >= CONFIG_SET_MAX_OUTSTANDING_PREVIEWS {
            self.issued.pop_front();
        }
        self.issued.push_back(IssuedApproval {
            digest,
            key,
            value,
            candidate_digest,
        });
    }

    /// Consume the approval issued for exactly this `(key, value)` triple.
    ///
    /// Returns the approved candidate digest. One token authorizes one write:
    /// a second `set` with the same token is refused, so a stale row cannot be
    /// replayed.
    pub fn consume(
        &mut self,
        digest: &str,
        key: &str,
        value: &ConfigSettingValue,
    ) -> Option<String> {
        let index = self.issued.iter().position(|issued| {
            issued.digest == digest && issued.key == key && issued.value == *value
        })?;
        Some(self.issued.remove(index)?.candidate_digest)
    }
}

/// Number of outstanding approvals (tests/diagnostics).
#[must_use]
pub fn outstanding_previews(gate: &ConfigWriteGate) -> usize {
    gate.issued.len()
}

/// Build the versioned approval preimage for one rendered change.
#[must_use]
pub fn approval_preimage(
    plugin_id: &str,
    change: &ConfigChange,
    rendering: &ConfigPreviewRendering,
) -> String {
    format!(
        "{CONFIG_SET_APPROVAL_DOMAIN}\0{plugin_id}\0{}\0{}\0{}\0{}\0{}",
        change.key(),
        change.value().kind().as_str(),
        change.rendered(),
        rendering.candidate_digest,
        rendering.changed_keys.join("\n")
    )
}

/// SHA-256 approval digest over [`approval_preimage`].
#[must_use]
pub fn approval_digest(
    plugin_id: &str,
    change: &ConfigChange,
    rendering: &ConfigPreviewRendering,
) -> String {
    bitty_package::integrity::sha256_hex(approval_preimage(plugin_id, change, rendering).as_bytes())
}

/// SHA-256 of the exact candidate user-layer bytes.
///
/// Backends hash the rendered candidate with this helper so the runtime and
/// the tests share one definition.
#[must_use]
pub fn candidate_digest(bytes: &[u8]) -> String {
    bitty_package::integrity::sha256_hex(bytes)
}

/// Map one backend failure to the plugin-visible typed error.
///
/// Only the path-free class and message cross; the backend's own diagnostic
/// (which may name a host path) stays in the host log.
#[must_use]
pub fn config_backend_error(failure: ConfigWriteFailure, key: &str) -> BridgeError {
    BridgeError::new(
        failure.class(),
        failure.code(),
        format!("{} (key '{key}')", failure.message()),
    )
}

/// No writable user layer: same class as a missing backend.
#[must_use]
pub fn config_unavailable() -> BridgeError {
    BridgeError::new(
        ConfigWriteFailure::Unavailable.class(),
        ConfigWriteFailure::Unavailable.code(),
        ConfigWriteFailure::Unavailable.message(),
    )
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
            // Review B5: capture retention is not a plugin presentation
            // setting; it stays on the explicit CLI/user path.
            "history.capture.transcript",
            "history.capture.commands",
        ] {
            assert!(
                writable_setting(forbidden).is_none(),
                "{forbidden} must stay CLI-only"
            );
        }
        // `close_confirm` is allowed by P-3, but the preview must disclose the
        // resulting policy (see the allowlist comment).
        assert!(writable_setting("close_confirm").is_some());
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

    fn rendering(candidate: &str, keys: &[&'static str]) -> ConfigPreviewRendering {
        ConfigPreviewRendering {
            changed_keys: keys.to_vec(),
            rendered: format!("rendered:{candidate}"),
            candidate_digest: bitty_package::integrity::sha256_hex(candidate.as_bytes()),
        }
    }

    #[test]
    fn approval_digest_binds_plugin_key_value_and_exact_candidate() {
        let theme = setting("appearance.theme");
        let change =
            ConfigChange::new(theme, ConfigSettingValue::text("catppuccin").expect("text"));
        let approved = rendering(
            "return { appearance = { theme = \"catppuccin\" } }",
            &["appearance.theme"],
        );
        let digest = approval_digest("bitty-terminal.dashboard", &change, &approved);
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            digest,
            approval_digest("bitty-terminal.dashboard", &change, &approved)
        );
        assert_ne!(
            digest,
            approval_digest("other.plugin", &change, &approved),
            "plugin identity is bound"
        );

        // The exact candidate bytes are bound: a profile edit that changes a
        // pinned seed produces a different approval, even for the same leaf.
        let moved_seed = rendering(
            "return { appearance = { theme = \"catppuccin\" }, terminal = { shell = \"/bin/fish\" } }",
            &["appearance.theme", "terminal.shell"],
        );
        assert_ne!(
            digest,
            approval_digest("bitty-terminal.dashboard", &change, &moved_seed),
            "a different pinned seed invalidates the approval"
        );
        assert_ne!(
            approval_preimage("p", &change, &approved),
            approval_preimage("p", &change, &moved_seed),
            "the preimage carries the candidate digest and changed keys"
        );

        // The changed-key list is part of the binding (seed visibility).
        let hidden_keys = rendering("return { appearance = { theme = \"catppuccin\" } }", &[]);
        assert_ne!(
            digest,
            approval_digest("bitty-terminal.dashboard", &change, &hidden_keys)
        );

        let other_value =
            ConfigChange::new(theme, ConfigSettingValue::text("dracula").expect("text"));
        assert_ne!(
            digest,
            approval_digest("bitty-terminal.dashboard", &other_value, &approved)
        );

        let other_key = ConfigChange::new(
            setting("font.family"),
            ConfigSettingValue::text("catppuccin").expect("text"),
        );
        assert_ne!(
            approval_digest("bitty-terminal.dashboard", &other_key, &approved),
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
            approval_preimage("p", &text_one, &approved),
            approval_preimage("p", &number_one, &approved)
        );

        // The preimage carries the rendered change, not a plugin-supplied
        // spelling: whitespace/quoting variants produce different digests.
        let padded = ConfigChange::new(
            theme,
            ConfigSettingValue::text(" catppuccin").expect("text"),
        );
        assert_ne!(
            digest,
            approval_digest("bitty-terminal.dashboard", &padded, &approved)
        );
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
    fn approvals_are_one_shot_unique_and_bounded() {
        let start = Instant::now();
        let mut gate = ConfigWriteGate::new(start);
        let value = ConfigSettingValue::text("catppuccin").expect("text");
        let change = ConfigChange::new(setting("appearance.theme"), value.clone());
        let approved = rendering("candidate-one", &["appearance.theme"]);
        let digest = approval_digest("p", &change, &approved);
        gate.issue(
            digest.clone(),
            change.key(),
            value.clone(),
            approved.candidate_digest.clone(),
        );
        assert_eq!(outstanding_previews(&gate), 1);
        // Re-previewing the same exact change replaces the approval instead of
        // stacking a second token (one live approval per change).
        gate.issue(
            digest.clone(),
            change.key(),
            value.clone(),
            approved.candidate_digest.clone(),
        );
        assert_eq!(outstanding_previews(&gate), 1, "no duplicate approvals");
        assert_eq!(
            gate.consume(&digest, change.key(), &value),
            Some(approved.candidate_digest.clone())
        );
        assert!(
            gate.consume(&digest, change.key(), &value).is_none(),
            "tokens are one-shot"
        );
        assert_eq!(outstanding_previews(&gate), 0);

        for index in 0..CONFIG_SET_MAX_OUTSTANDING_PREVIEWS + 4 {
            let value = ConfigSettingValue::text(&format!("theme{index}")).expect("text");
            let change = ConfigChange::new(setting("appearance.theme"), value.clone());
            let approved = rendering(&format!("candidate-{index}"), &["appearance.theme"]);
            gate.issue(
                approval_digest("p", &change, &approved),
                change.key(),
                value,
                approved.candidate_digest,
            );
        }
        assert_eq!(
            outstanding_previews(&gate),
            CONFIG_SET_MAX_OUTSTANDING_PREVIEWS,
            "old approvals drop instead of growing the table"
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
            ConfigWriteFailure::Stale,
            ConfigWriteFailure::Io,
        ] {
            assert!(!failure.message().contains('/'));
            assert!(!failure.message().contains("~"));
            assert!(failure.code().starts_with("E_CONFIG_"));
            assert!(!failure.class().is_empty());
        }
        assert_eq!(ConfigWriteFailure::Stale.code(), "E_CONFIG_STALE");
        assert_eq!(config_unavailable().code, "E_CONFIG_UNAVAILABLE");
    }
}
