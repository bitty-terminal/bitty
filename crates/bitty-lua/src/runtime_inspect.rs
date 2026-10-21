//! Typed, bounded read-only runtime inspection for `bitty.runtime.inspect()`
//! (issue #2396).
//!
//! The surface is one argument-free call returning a plain snapshot table with
//! five sections: diagnostics, plugins, settings, extensions, and profiles.
//! Every section either carries bounded rows or a typed
//! `unavailable: <reason>` marker; a failed source is never rendered as an
//! empty list.
//!
//! # Authority
//!
//! This module owns data shaping only. The bridge rejects arguments before the
//! host is reached ([`crate::host::HostServices::runtime_inspect`]), the host
//! checks the `runtime.inspect` grant, and the application builds the snapshot
//! from already-resolved configuration and recorded state. No call path opens a
//! file, spawns a process, reads the environment, or touches the network: every
//! path in the payload is a display string.
//!
//! # Bounds (fail-closed)
//!
//! | Dimension            | Bound | Enforcement |
//! |----------------------|-------|-------------|
//! | aggregate payload    | 64 KiB | [`RuntimeInspectSnapshot::enforce_budget`] drops tail rows and marks `truncated`; [`Self::fits_budget`] is the bridge's final gate |
//! | rows per section     | 128 plugins / 256 settings / 64 extensions / 64 profiles | the application builder truncates and sets `truncated` |
//! | path display string  | 240 bytes | [`bounded_display`] at conversion time |
//! | other display string | 96 bytes | [`bounded_display`] at conversion time |
//! | calls per generation | 16 / s | host call budget |
//! | wall time per call   | 10 ms | host call budget |
//!
//! The aggregate bound measures the canonical JSON encoding of the returned
//! table (the same strings the Lua conversion emits, plus structural bytes).
//! Truncation is deterministic and visible; a silently oversized payload is
//! never returned.

use crate::host::LuaValue;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Maximum aggregate snapshot payload (canonical encoded bytes).
pub const RUNTIME_INSPECT_MAX_BYTES: usize = 64 * 1024;

/// Maximum plugin rows per snapshot.
pub const RUNTIME_INSPECT_MAX_PLUGIN_ROWS: usize = 128;

/// Maximum settings rows per snapshot.
pub const RUNTIME_INSPECT_MAX_SETTING_ROWS: usize = 256;

/// Maximum extension rows per snapshot.
pub const RUNTIME_INSPECT_MAX_EXTENSION_ROWS: usize = 64;

/// Maximum profile rows per snapshot.
pub const RUNTIME_INSPECT_MAX_PROFILE_ROWS: usize = 64;

/// Maximum bytes of one display path.
pub const RUNTIME_INSPECT_MAX_PATH_BYTES: usize = 240;

/// Maximum bytes of one non-path display string.
pub const RUNTIME_INSPECT_MAX_STRING_BYTES: usize = 96;

/// Maximum accepted calls per second per plugin generation.
pub const RUNTIME_INSPECT_CALLS_PER_SECOND: u32 = 16;

/// Maximum wall time one call may consume, in milliseconds.
pub const RUNTIME_INSPECT_MAX_WALL_MS: u64 = 10;

/// Reason reported by a section whose source could not be read.
pub const RUNTIME_INSPECT_UNAVAILABLE: &str = "source unavailable";

/// Reason reported when the snapshot exceeds the aggregate payload bound.
pub const RUNTIME_INSPECT_OVER_BUDGET: &str = "payload exceeds 64 KiB";

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

/// One section: either typed content or a typed per-section failure.
///
/// The `unavailable` string is host-authored, bounded, and never contains
/// credentials or terminal content; it names the failing source class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Section<T> {
    /// Section content.
    Available(T),
    /// Section source failure (rendered as `{ unavailable = <reason> }`).
    Unavailable(String),
}

impl<T> Default for Section<T> {
    fn default() -> Self {
        Self::Unavailable(RUNTIME_INSPECT_UNAVAILABLE.to_string())
    }
}

impl<T> Section<T> {
    /// Build an available section.
    #[must_use]
    pub fn available(value: T) -> Self {
        Self::Available(value)
    }

    /// Build an unavailable section; the reason is bounded at conversion.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable(reason.into())
    }

    /// Whether this section failed.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// Bounded rows plus the deterministic truncation marker.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InspectRows<T> {
    /// Rows in their documented order.
    pub rows: Vec<T>,
    /// Whether tail rows were dropped by a bound.
    pub truncated: bool,
}

impl<T> InspectRows<T> {
    /// Build rows without a truncation marker.
    #[must_use]
    pub fn new(rows: Vec<T>) -> Self {
        Self {
            rows,
            truncated: false,
        }
    }

    /// Cap the row count at `max`, setting `truncated` when rows were cut.
    pub fn cap(&mut self, max: usize) {
        if self.rows.len() > max {
            self.rows.truncate(max);
            self.truncated = true;
        }
    }

    /// Drop the tail row; returns whether one existed.
    fn pop_tail(&mut self) -> bool {
        if self.rows.pop().is_some() {
            self.truncated = true;
            true
        } else {
            false
        }
    }
}

/// Source layer that selected the active profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSource {
    /// `--profile`.
    Cli,
    /// `BITTY_PROFILE`.
    Env,
    /// No profile selected.
    None,
}

impl ProfileSource {
    /// Stable label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Env => "env",
            Self::None => "none",
        }
    }
}

/// Source layer that won the active theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSource {
    /// `--theme`.
    Cli,
    /// User config file.
    File,
    /// Named profile layer.
    Profile,
    /// Built-in default.
    Default,
}

impl ThemeSource {
    /// Stable label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::File => "file",
            Self::Profile => "profile",
            Self::Default => "default",
        }
    }
}

/// Source layer that supplied one plugin setting value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    /// Shipped manifest default.
    Default,
    /// Shared host source (the old home).
    Host,
    /// Per-plugin user file.
    User,
}

impl SettingSource {
    /// Stable label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Host => "host",
            Self::User => "user",
        }
    }
}

/// Last recorded extension activation transition.
///
/// This is a recorded label, never a liveness probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionActivation {
    /// Nothing recorded yet.
    Installed,
    /// A spawn was recorded.
    Active,
    /// A stop was recorded.
    Stopped,
    /// A crash was recorded.
    Crashed,
    /// A refusal was recorded with its stable class.
    Denied(String),
}

impl ExtensionActivation {
    /// Stable label (`denied:<class>` for refusals).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Installed => "installed".to_string(),
            Self::Active => "active".to_string(),
            Self::Stopped => "stopped".to_string(),
            Self::Crashed => "crashed".to_string(),
            Self::Denied(class) => format!(
                "denied:{}",
                bounded_display(class, RUNTIME_INSPECT_MAX_STRING_BYTES)
            ),
        }
    }

    /// Trust derived from the recorded transition.
    ///
    /// A successful spawn family (active/stopped/crashed) implies the
    /// executable passed resolution and digest verification. A recorded
    /// `component_untrusted` denial is untrusted; nothing recorded (or any
    /// other refusal) stays unverified.
    #[must_use]
    pub fn trust(&self) -> ExtensionTrust {
        match self {
            Self::Active | Self::Stopped | Self::Crashed => ExtensionTrust::Verified,
            Self::Denied(class) if class == "component_untrusted" => ExtensionTrust::Untrusted,
            Self::Installed | Self::Denied(_) => ExtensionTrust::Unverified,
        }
    }
}

/// Trust label derived from the last recorded activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionTrust {
    /// No verified activation record.
    Unverified,
    /// A successful spawn family was recorded.
    Verified,
    /// A `component_untrusted` denial was recorded.
    Untrusted,
}

impl ExtensionTrust {
    /// Stable label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::Verified => "verified",
            Self::Untrusted => "untrusted",
        }
    }
}

/// Decoration geometry (logical pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecorationGeometry {
    /// Gap between adjacent views.
    pub gaps_in: u32,
    /// Gap between the tiling area and the window edge.
    pub gaps_out: u32,
    /// Frame border thickness.
    pub border: u32,
    /// Frame corner radius.
    pub radius: u32,
    /// Inner padding between frame and content.
    pub content_inset: u32,
}

/// Diagnostics section content.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeDiagnostics {
    /// Running application version.
    pub app_version: String,
    /// Loaded config file (display path).
    pub config_file: Option<String>,
    /// Config directory (display path).
    pub config_dir: Option<String>,
    /// Managed plugin ledger (display path).
    pub plugin_ledger: Option<String>,
    /// Plugin package store (display path).
    pub plugin_store: Option<String>,
    /// Extension user install root (display path).
    pub extension_user_root: Option<String>,
    /// Extension system install root (display path).
    pub extension_system_root: Option<String>,
    /// Extension activation state root (display path).
    pub extension_state_root: Option<String>,
    /// Active profile name.
    pub profile: Option<String>,
    /// Layer that selected the profile.
    pub profile_source: ProfileSource,
    /// Active theme name.
    pub theme: String,
    /// Layer that selected the theme.
    pub theme_source: ThemeSource,
    /// Effective font family.
    pub font_family: String,
    /// Effective font size in points.
    pub font_size: f64,
    /// Effective decoration geometry.
    pub decoration: DecorationGeometry,
}

/// One plugin row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInspectRow {
    /// Owner-qualified plugin id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Manifest version.
    pub version: String,
    /// Provenance label (`bundled`, `registry`, `git`, `local-path`).
    pub source: String,
    /// Recorded/desired enabled state.
    pub enabled: bool,
    /// Whether the managed ledger records this id.
    pub recorded: bool,
}

/// One declared setting row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingInspectRow {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Declared setting key.
    pub key: String,
    /// Current effective value as a bounded display string.
    pub current: String,
    /// Shipped manifest default as a bounded display string.
    pub default: String,
    /// Layer that supplied `current`.
    pub source: SettingSource,
}

/// One installed extension version row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionInspectRow {
    /// Component name.
    pub name: String,
    /// Installed version.
    pub version: String,
    /// Install tier (`user` or `system`).
    pub tier: String,
    /// Whether the installed version supports Core's wire protocol.
    pub abi_compatible: bool,
    /// Descriptor digest (recorded, never recomputed on the call path).
    pub sha256: String,
    /// Executable path (display string).
    pub executable_path: String,
    /// Last recorded activation transition.
    pub activation: ExtensionActivation,
    /// Trust derived from the activation record.
    pub trust: ExtensionTrust,
}

/// One profile row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileInspectRow {
    /// Profile name.
    pub name: String,
    /// Whether this profile was selected at startup.
    pub active: bool,
}

/// Complete `bitty.runtime.inspect()` payload.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RuntimeInspectSnapshot {
    /// Application and path diagnostics.
    pub diagnostics: Section<RuntimeDiagnostics>,
    /// Bundled and installed plugin rows.
    pub plugins: Section<InspectRows<PluginInspectRow>>,
    /// Declared plugin settings rows.
    pub settings: Section<InspectRows<SettingInspectRow>>,
    /// Installed extension version rows.
    pub extensions: Section<InspectRows<ExtensionInspectRow>>,
    /// Profile rows.
    pub profiles: Section<InspectRows<ProfileInspectRow>>,
}

impl RuntimeInspectSnapshot {
    /// Canonical encoded payload length in bytes.
    ///
    /// This is the bound the aggregate 64 KiB ceiling measures. Field order and
    /// escaping are deterministic, so the value is stable across calls.
    #[must_use]
    pub fn canonical_len(&self) -> usize {
        let mut out = String::new();
        self.write_json(&mut out);
        out.len()
    }

    /// Whether the snapshot is inside the aggregate payload bound.
    #[must_use]
    pub fn fits_budget(&self) -> bool {
        self.canonical_len() <= RUNTIME_INSPECT_MAX_BYTES
    }

    /// Drop tail rows until the payload fits the aggregate bound.
    ///
    /// Returns whether anything was dropped. Drop order is settings, plugins,
    /// extensions, then profiles (the sections with the least diagnostic
    /// value at the tail). If the payload still cannot fit — impossible with
    /// the per-string caps, but fail-closed anyway — every rows section is
    /// replaced by an `unavailable` marker.
    pub fn enforce_budget(&mut self) -> bool {
        let mut truncated = false;
        loop {
            if self.fits_budget() {
                return truncated;
            }
            let dropped = self.pop_tail();
            if !dropped {
                break;
            }
            truncated = true;
        }
        // Defensive: the fixed diagnostics section alone is bounded well
        // below the ceiling, so this only runs if a caller injected a
        // pathological diagnostics value. Keep the section typed and never
        // return an oversized payload.
        let reason = RUNTIME_INSPECT_OVER_BUDGET.to_string();
        self.plugins = Section::Unavailable(reason.clone());
        self.settings = Section::Unavailable(reason.clone());
        self.extensions = Section::Unavailable(reason.clone());
        self.profiles = Section::Unavailable(reason);
        true
    }

    /// Drop the tail row from the first non-empty rows section.
    fn pop_tail(&mut self) -> bool {
        if let Section::Available(rows) = &mut self.settings {
            if rows.pop_tail() {
                return true;
            }
        }
        if let Section::Available(rows) = &mut self.plugins {
            if rows.pop_tail() {
                return true;
            }
        }
        if let Section::Available(rows) = &mut self.extensions {
            if rows.pop_tail() {
                return true;
            }
        }
        if let Section::Available(rows) = &mut self.profiles {
            if rows.pop_tail() {
                return true;
            }
        }
        false
    }

    /// Convert the bounded snapshot to the Lua table returned to plugins.
    ///
    /// Every string is bounded here as defense in depth; the shape matches
    /// [`Self::write_json`].
    #[must_use]
    pub fn to_lua(&self) -> LuaValue {
        LuaValue::table([
            ("diagnostics", self.diagnostics.to_lua()),
            ("plugins", self.plugins.to_lua()),
            ("settings", self.settings.to_lua()),
            ("extensions", self.extensions.to_lua()),
            ("profiles", self.profiles.to_lua()),
        ])
    }

    fn write_json(&self, out: &mut String) {
        out.push('{');
        out.push_str("\"diagnostics\":");
        self.diagnostics.write_json(out);
        out.push_str(",\"plugins\":");
        self.plugins.write_json(out);
        out.push_str(",\"settings\":");
        self.settings.write_json(out);
        out.push_str(",\"extensions\":");
        self.extensions.write_json(out);
        out.push_str(",\"profiles\":");
        self.profiles.write_json(out);
        out.push('}');
    }
}

impl<T: JsonSection> Section<T> {
    fn write_json(&self, out: &mut String) {
        match self {
            Self::Available(value) => value.write_json(out),
            Self::Unavailable(reason) => {
                out.push_str("{\"unavailable\":");
                write_json_string(
                    out,
                    &bounded_display(reason, RUNTIME_INSPECT_MAX_STRING_BYTES),
                );
                out.push('}');
            }
        }
    }
}

impl<T: LuaSection> Section<T> {
    fn to_lua(&self) -> LuaValue {
        match self {
            Self::Available(value) => value.to_lua(),
            Self::Unavailable(reason) => LuaValue::table([(
                "unavailable",
                LuaValue::String(bounded_display(reason, RUNTIME_INSPECT_MAX_STRING_BYTES)),
            )]),
        }
    }
}

/// JSON half of the payload shape (also drives the size bound).
#[doc(hidden)]
pub trait JsonSection {
    fn write_json(&self, out: &mut String);
}

impl JsonSection for RuntimeDiagnostics {
    fn write_json(&self, out: &mut String) {
        out.push('{');
        write_json_field(out, "app_version", &self.app_version, true);
        write_json_opt_path(out, "config_file", self.config_file.as_deref());
        write_json_opt_path(out, "config_dir", self.config_dir.as_deref());
        write_json_opt_path(out, "plugin_ledger", self.plugin_ledger.as_deref());
        write_json_opt_path(out, "plugin_store", self.plugin_store.as_deref());
        write_json_opt_path(
            out,
            "extension_user_root",
            self.extension_user_root.as_deref(),
        );
        write_json_opt_path(
            out,
            "extension_system_root",
            self.extension_system_root.as_deref(),
        );
        write_json_opt_path(
            out,
            "extension_state_root",
            self.extension_state_root.as_deref(),
        );
        out.push(',');
        write_json_name(out, "profile");
        match &self.profile {
            Some(profile) => write_json_string(
                out,
                &bounded_display(profile, RUNTIME_INSPECT_MAX_STRING_BYTES),
            ),
            None => out.push_str("null"),
        }
        out.push(',');
        write_json_name(out, "profile_source");
        write_json_string(out, self.profile_source.label());
        out.push(',');
        write_json_name(out, "theme");
        write_json_string(
            out,
            &bounded_display(&self.theme, RUNTIME_INSPECT_MAX_STRING_BYTES),
        );
        out.push(',');
        write_json_name(out, "theme_source");
        write_json_string(out, self.theme_source.label());
        out.push(',');
        write_json_name(out, "font_family");
        write_json_string(
            out,
            &bounded_display(&self.font_family, RUNTIME_INSPECT_MAX_STRING_BYTES),
        );
        out.push(',');
        write_json_name(out, "font_size");
        if self.font_size.is_finite() {
            out.push_str(&format!("{:.2}", self.font_size));
        } else {
            out.push_str("0.00");
        }
        out.push(',');
        write_json_name(out, "decoration");
        out.push_str(&format!(
            "{{\"gaps_in\":{},\"gaps_out\":{},\"border\":{},\"radius\":{},\"content_inset\":{}}}",
            self.decoration.gaps_in,
            self.decoration.gaps_out,
            self.decoration.border,
            self.decoration.radius,
            self.decoration.content_inset
        ));
        out.push('}');
    }
}

impl JsonSection for InspectRows<PluginInspectRow> {
    fn write_json(&self, out: &mut String) {
        out.push_str("{\"rows\":[");
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push('{');
            write_json_field(out, "id", &row.id, true);
            write_json_field(out, "name", &row.name, false);
            write_json_field(out, "version", &row.version, false);
            write_json_field(out, "source", &row.source, false);
            out.push(',');
            write_json_name(out, "enabled");
            out.push_str(if row.enabled { "true" } else { "false" });
            out.push(',');
            write_json_name(out, "recorded");
            out.push_str(if row.recorded { "true" } else { "false" });
            out.push('}');
        }
        out.push_str("],\"truncated\":");
        out.push_str(if self.truncated { "true" } else { "false" });
        out.push('}');
    }
}

impl JsonSection for InspectRows<SettingInspectRow> {
    fn write_json(&self, out: &mut String) {
        out.push_str("{\"rows\":[");
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push('{');
            write_json_field(out, "plugin_id", &row.plugin_id, true);
            write_json_field(out, "key", &row.key, false);
            write_json_field(out, "current", &row.current, false);
            write_json_field(out, "default", &row.default, false);
            write_json_field(out, "source", row.source.label(), false);
            out.push('}');
        }
        out.push_str("],\"truncated\":");
        out.push_str(if self.truncated { "true" } else { "false" });
        out.push('}');
    }
}

impl JsonSection for InspectRows<ExtensionInspectRow> {
    fn write_json(&self, out: &mut String) {
        out.push_str("{\"rows\":[");
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push('{');
            write_json_field(out, "name", &row.name, true);
            write_json_field(out, "version", &row.version, false);
            write_json_field(out, "tier", &row.tier, false);
            out.push(',');
            write_json_name(out, "abi_compatible");
            out.push_str(if row.abi_compatible { "true" } else { "false" });
            out.push(',');
            write_json_field(out, "sha256", &row.sha256, false);
            write_json_path_field(out, "executable_path", &row.executable_path, false);
            write_json_field(out, "activation", &row.activation.label(), false);
            write_json_field(out, "trust", row.trust.label(), false);
            out.push('}');
        }
        out.push_str("],\"truncated\":");
        out.push_str(if self.truncated { "true" } else { "false" });
        out.push('}');
    }
}

impl JsonSection for InspectRows<ProfileInspectRow> {
    fn write_json(&self, out: &mut String) {
        out.push_str("{\"rows\":[");
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push('{');
            write_json_field(out, "name", &row.name, true);
            out.push(',');
            write_json_name(out, "active");
            out.push_str(if row.active { "true" } else { "false" });
            out.push('}');
        }
        out.push_str("],\"truncated\":");
        out.push_str(if self.truncated { "true" } else { "false" });
        out.push('}');
    }
}

/// Lua half of the payload shape.
#[doc(hidden)]
pub trait LuaSection {
    fn to_lua(&self) -> LuaValue;
}

impl LuaSection for RuntimeDiagnostics {
    fn to_lua(&self) -> LuaValue {
        LuaValue::table([
            ("app_version", lua_string(&self.app_version)),
            ("config_file", lua_opt_path(self.config_file.as_deref())),
            ("config_dir", lua_opt_path(self.config_dir.as_deref())),
            ("plugin_ledger", lua_opt_path(self.plugin_ledger.as_deref())),
            ("plugin_store", lua_opt_path(self.plugin_store.as_deref())),
            (
                "extension_user_root",
                lua_opt_path(self.extension_user_root.as_deref()),
            ),
            (
                "extension_system_root",
                lua_opt_path(self.extension_system_root.as_deref()),
            ),
            (
                "extension_state_root",
                lua_opt_path(self.extension_state_root.as_deref()),
            ),
            ("profile", lua_opt_display(self.profile.as_deref())),
            (
                "profile_source",
                LuaValue::String(self.profile_source.label().to_string()),
            ),
            ("theme", lua_string(&self.theme)),
            (
                "theme_source",
                LuaValue::String(self.theme_source.label().to_string()),
            ),
            ("font_family", lua_string(&self.font_family)),
            (
                "font_size",
                LuaValue::Number(if self.font_size.is_finite() {
                    self.font_size
                } else {
                    0.0
                }),
            ),
            (
                "decoration",
                LuaValue::table([
                    (
                        "gaps_in",
                        LuaValue::Integer(i64::from(self.decoration.gaps_in)),
                    ),
                    (
                        "gaps_out",
                        LuaValue::Integer(i64::from(self.decoration.gaps_out)),
                    ),
                    (
                        "border",
                        LuaValue::Integer(i64::from(self.decoration.border)),
                    ),
                    (
                        "radius",
                        LuaValue::Integer(i64::from(self.decoration.radius)),
                    ),
                    (
                        "content_inset",
                        LuaValue::Integer(i64::from(self.decoration.content_inset)),
                    ),
                ]),
            ),
        ])
    }
}

impl LuaSection for InspectRows<PluginInspectRow> {
    fn to_lua(&self) -> LuaValue {
        LuaValue::table([
            (
                "rows",
                LuaValue::array(self.rows.iter().map(plugin_row_lua).collect()),
            ),
            ("truncated", LuaValue::Bool(self.truncated)),
        ])
    }
}

impl LuaSection for InspectRows<SettingInspectRow> {
    fn to_lua(&self) -> LuaValue {
        LuaValue::table([
            (
                "rows",
                LuaValue::array(self.rows.iter().map(setting_row_lua).collect()),
            ),
            ("truncated", LuaValue::Bool(self.truncated)),
        ])
    }
}

impl LuaSection for InspectRows<ExtensionInspectRow> {
    fn to_lua(&self) -> LuaValue {
        LuaValue::table([
            (
                "rows",
                LuaValue::array(self.rows.iter().map(extension_row_lua).collect()),
            ),
            ("truncated", LuaValue::Bool(self.truncated)),
        ])
    }
}

impl LuaSection for InspectRows<ProfileInspectRow> {
    fn to_lua(&self) -> LuaValue {
        LuaValue::table([
            (
                "rows",
                LuaValue::array(self.rows.iter().map(profile_row_lua).collect()),
            ),
            ("truncated", LuaValue::Bool(self.truncated)),
        ])
    }
}

fn plugin_row_lua(row: &PluginInspectRow) -> LuaValue {
    LuaValue::table([
        ("id", lua_string(&row.id)),
        ("name", lua_string(&row.name)),
        ("version", lua_string(&row.version)),
        ("source", lua_string(&row.source)),
        ("enabled", LuaValue::Bool(row.enabled)),
        ("recorded", LuaValue::Bool(row.recorded)),
    ])
}

fn setting_row_lua(row: &SettingInspectRow) -> LuaValue {
    LuaValue::table([
        ("plugin_id", lua_string(&row.plugin_id)),
        ("key", lua_string(&row.key)),
        ("current", lua_string(&row.current)),
        ("default", lua_string(&row.default)),
        ("source", LuaValue::String(row.source.label().to_string())),
    ])
}

fn extension_row_lua(row: &ExtensionInspectRow) -> LuaValue {
    LuaValue::table([
        ("name", lua_string(&row.name)),
        ("version", lua_string(&row.version)),
        ("tier", lua_string(&row.tier)),
        ("abi_compatible", LuaValue::Bool(row.abi_compatible)),
        ("sha256", lua_string(&row.sha256)),
        (
            "executable_path",
            display_string(&row.executable_path, RUNTIME_INSPECT_MAX_PATH_BYTES),
        ),
        ("activation", LuaValue::String(row.activation.label())),
        ("trust", LuaValue::String(row.trust.label().to_string())),
    ])
}

fn profile_row_lua(row: &ProfileInspectRow) -> LuaValue {
    LuaValue::table([
        ("name", lua_string(&row.name)),
        ("active", LuaValue::Bool(row.active)),
    ])
}

fn lua_string(value: &str) -> LuaValue {
    LuaValue::String(bounded_display(value, RUNTIME_INSPECT_MAX_STRING_BYTES))
}

fn lua_opt_display(value: Option<&str>) -> LuaValue {
    value.map_or(LuaValue::Nil, lua_string)
}

fn lua_opt_path(value: Option<&str>) -> LuaValue {
    value.map_or(LuaValue::Nil, |path| {
        display_string(path, RUNTIME_INSPECT_MAX_PATH_BYTES)
    })
}

fn display_string(value: &str, max_bytes: usize) -> LuaValue {
    LuaValue::String(bounded_display(value, max_bytes))
}

// ---------------------------------------------------------------------------
// Bounded display strings and canonical JSON
// ---------------------------------------------------------------------------

/// Truncate `value` to at most `max_bytes` UTF-8 bytes at a char boundary.
///
/// Longer values keep a leading prefix and end with `…` (three bytes) inside
/// the bound. The result is deterministic and valid UTF-8.
#[must_use]
pub fn bounded_display(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let ellipsis = "…";
    if max_bytes < ellipsis.len() {
        let mut end = max_bytes;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        return value[..end].to_string();
    }
    let mut end = max_bytes - ellipsis.len();
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + ellipsis.len());
    out.push_str(&value[..end]);
    out.push('…');
    out
}

fn write_json_name(out: &mut String, name: &str) {
    out.push('"');
    out.push_str(name);
    out.push('"');
    out.push(':');
}

fn write_json_field(out: &mut String, name: &str, value: &str, first: bool) {
    if !first {
        out.push(',');
    }
    write_json_name(out, name);
    write_json_string(
        out,
        &bounded_display(value, RUNTIME_INSPECT_MAX_STRING_BYTES),
    );
}

fn write_json_path_field(out: &mut String, name: &str, value: &str, first: bool) {
    if !first {
        out.push(',');
    }
    write_json_name(out, name);
    write_json_string(out, &bounded_display(value, RUNTIME_INSPECT_MAX_PATH_BYTES));
}

fn write_json_opt_path(out: &mut String, name: &str, value: Option<&str>) {
    out.push(',');
    write_json_name(out, name);
    match value {
        Some(path) => {
            write_json_string(out, &bounded_display(path, RUNTIME_INSPECT_MAX_PATH_BYTES))
        }
        None => out.push_str("null"),
    }
}

fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic_fixture() -> RuntimeDiagnostics {
        RuntimeDiagnostics {
            app_version: "0.0.26".to_string(),
            config_file: Some("/home/user/.config/bitty/init.lua".to_string()),
            config_dir: Some("/home/user/.config/bitty".to_string()),
            plugin_ledger: Some("/home/user/.config/bitty/bitty-plugins.toml".to_string()),
            plugin_store: Some("/home/user/.local/share/bitty/plugins".to_string()),
            extension_user_root: Some("/home/user/.local/share/bitty/extensions".to_string()),
            extension_system_root: Some("/usr/lib/bitty/extensions".to_string()),
            extension_state_root: Some("/home/user/.local/state/bitty/extensions".to_string()),
            profile: Some("work".to_string()),
            profile_source: ProfileSource::Cli,
            theme: "bitty-dark".to_string(),
            theme_source: ThemeSource::Default,
            font_family: "monospace".to_string(),
            font_size: 12.0,
            decoration: DecorationGeometry::default(),
        }
    }

    fn snapshot_with_rows(rows: usize, name_bytes: usize) -> RuntimeInspectSnapshot {
        RuntimeInspectSnapshot {
            diagnostics: Section::available(diagnostic_fixture()),
            plugins: Section::available(InspectRows::new(
                (0..rows)
                    .map(|index| PluginInspectRow {
                        id: format!("owner.plugin{index}"),
                        name: "n".repeat(name_bytes),
                        version: "1.0.0".to_string(),
                        source: "registry".to_string(),
                        enabled: true,
                        recorded: true,
                    })
                    .collect(),
            )),
            settings: Section::available(InspectRows::new(Vec::new())),
            extensions: Section::available(InspectRows::new(Vec::new())),
            profiles: Section::available(InspectRows::new(Vec::new())),
        }
    }

    fn snapshot_with_settings(rows: usize, field_bytes: usize) -> RuntimeInspectSnapshot {
        let mut snapshot = snapshot_with_rows(1, 8);
        snapshot.settings = Section::available(InspectRows::new(
            (0..rows)
                .map(|index| SettingInspectRow {
                    plugin_id: format!("owner.plugin{index}"),
                    key: "k".repeat(field_bytes),
                    current: "c".repeat(field_bytes),
                    default: "d".repeat(field_bytes),
                    source: SettingSource::User,
                })
                .collect(),
        ));
        snapshot
    }

    #[test]
    fn bounded_display_stays_on_char_boundaries() {
        let value = "é".repeat(200);
        let bounded = bounded_display(&value, RUNTIME_INSPECT_MAX_STRING_BYTES);
        assert!(bounded.len() <= RUNTIME_INSPECT_MAX_STRING_BYTES);
        assert!(bounded.ends_with('…'));
        assert!(bounded.is_char_boundary(bounded.len()));
        assert_eq!(bounded_display("short", 96), "short");
    }

    #[test]
    fn payload_below_the_cap_is_unchanged() {
        let snapshot = snapshot_with_rows(3, 8);
        assert!(snapshot.fits_budget());
        let mut bounded = snapshot.clone();
        assert!(!bounded.enforce_budget());
        assert_eq!(bounded, snapshot);
    }

    #[test]
    fn payload_above_the_cap_drops_tail_rows_and_flags_truncation() {
        // 256 settings rows with 96-byte fields exceed 64 KiB.
        let mut snapshot = snapshot_with_settings(RUNTIME_INSPECT_MAX_SETTING_ROWS, 96);
        assert!(
            !snapshot.fits_budget(),
            "fixture must start above the aggregate cap (len {})",
            snapshot.canonical_len()
        );
        assert!(snapshot.enforce_budget());
        assert!(snapshot.fits_budget());
        let Section::Available(rows) = &snapshot.settings else {
            panic!("settings must stay available");
        };
        assert!(rows.truncated);
        assert!(rows.rows.len() < RUNTIME_INSPECT_MAX_SETTING_ROWS);
    }

    /// Largest settings-row count that still fits the aggregate bound, plus
    /// the next count that exceeds it.
    fn boundary_snapshots() -> (RuntimeInspectSnapshot, RuntimeInspectSnapshot) {
        let mut fit = 1usize;
        while fit < RUNTIME_INSPECT_MAX_SETTING_ROWS
            && snapshot_with_settings(fit + 1, RUNTIME_INSPECT_MAX_STRING_BYTES).fits_budget()
        {
            fit += 1;
        }
        (
            snapshot_with_settings(fit, RUNTIME_INSPECT_MAX_STRING_BYTES),
            snapshot_with_settings(fit + 1, RUNTIME_INSPECT_MAX_STRING_BYTES),
        )
    }

    #[test]
    fn cap_accepts_the_last_fitting_row_and_rejects_the_next() {
        let (fits, over) = boundary_snapshots();
        assert!(
            fits.fits_budget(),
            "fixture must fit (len {})",
            fits.canonical_len()
        );
        assert!(
            !over.fits_budget(),
            "one more row must exceed the cap (len {})",
            over.canonical_len()
        );
        let mut over = over;
        assert!(over.enforce_budget());
        assert!(over.fits_budget());
        let truncated = over.settings.is_unavailable()
            || matches!(&over.settings, Section::Available(rows) if rows.truncated);
        assert!(truncated);
    }

    #[test]
    fn unavailable_sections_render_typed_markers() {
        let snapshot = RuntimeInspectSnapshot {
            diagnostics: Section::unavailable("no config root"),
            plugins: Section::unavailable("ledger unreadable"),
            settings: Section::unavailable("settings unreadable"),
            extensions: Section::unavailable("safe mode"),
            profiles: Section::unavailable("safe mode"),
        };
        let lua = snapshot.to_lua();
        for section in [
            "diagnostics",
            "plugins",
            "settings",
            "extensions",
            "profiles",
        ] {
            let value = lua.get(section).expect("section present");
            assert!(value.get("unavailable").is_some(), "{section}");
        }
        let mut bounded = snapshot;
        assert!(!bounded.enforce_budget());
        assert!(bounded.fits_budget());
    }

    #[test]
    fn trust_is_derived_from_the_recorded_transition() {
        assert_eq!(
            ExtensionActivation::Installed.trust(),
            ExtensionTrust::Unverified
        );
        assert_eq!(
            ExtensionActivation::Active.trust(),
            ExtensionTrust::Verified
        );
        assert_eq!(
            ExtensionActivation::Crashed.trust(),
            ExtensionTrust::Verified
        );
        assert_eq!(
            ExtensionActivation::Denied("component_untrusted".to_string()).trust(),
            ExtensionTrust::Untrusted
        );
        assert_eq!(
            ExtensionActivation::Denied("capability_denied".to_string()).trust(),
            ExtensionTrust::Unverified
        );
        assert_eq!(
            ExtensionActivation::Denied("component_untrusted".to_string()).label(),
            "denied:component_untrusted"
        );
    }

    #[test]
    fn snapshot_shape_carries_rows_and_truncation_marker() {
        let snapshot = snapshot_with_rows(2, 8);
        let lua = snapshot.to_lua();
        let plugins = lua.get("plugins").expect("plugins");
        let rows = plugins.get("rows").expect("rows");
        let LuaValue::Table(pairs) = rows else {
            panic!("row array");
        };
        assert_eq!(pairs.len(), 2);
        assert_eq!(plugins.get("truncated"), Some(&LuaValue::Bool(false)));
    }
}
