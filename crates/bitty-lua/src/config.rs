//! User configuration evaluation in the shared [`crate::LuaVm`] sandbox.
//!
//! # Direction (DEC-0011, owner-mandated)
//!
//! Bitty configuration is Lua, wezterm-style: `$XDG_CONFIG_HOME/bitty/init.lua`
//! (fallback `config.lua`) returns a plain-data table that maps 1:1 to
//! `bitty-config` plan fields. The chunk runs in a fresh [`crate::LuaVm`]
//! with the **same** RC-1/RC-2 instruction, wall-clock, and memory budgets as
//! a plugin — config never executes with more authority than a plugin.
//!
//! # What the chunk can touch
//!
//! The sandbox builds [`piccolo::Lua::core`]: pure-computation base library
//! (`type`, `tostring`, `pairs`, `ipairs`, `error`, `assert`, `select`,
//! `pcall`, `next`, `rawget`/`rawset`, `setmetatable`/`getmetatable`, …),
//! `coroutine`, `math`, `string`, and `table` (inventory verified
//! empirically; see `denied_globals_fail_closed`). Explicitly **absent**
//! (default deny): `print`, `tonumber`, `io`, `os`, `debug`,
//! `package`/`require`, `load`/`loadfile`/`dofile`, `_G`, and any host
//! callback — the chunk cannot read files, spawn processes, touch the
//! network, or reach host objects. A chunk that references a denied global
//! fails closed with a Lua runtime error, exactly like a misbehaving plugin.
//! Metatables can be set but are ignored: extraction reads raw table contents
//! only, so `__index` tricks cannot smuggle values past validation.
//!
//! # Shape contract
//!
//! ```lua
//! -- init.lua: wezterm-style, data only.
//! return {
//!     theme = "dark", -- alias for appearance.theme
//!     appearance = { theme = "bitty-dark" }, -- wins over the alias
//!     font = { family = "JetBrains Mono", size = 13.0 },
//!     window = { opacity = 0.95, padding = 8 },
//!     terminal = { scrollback = 10000, shell = "/bin/fish", scroll_lines_per_notch = 3, scroll_pixels_per_notch = 16 },
//!     selection = { auto_copy = true }, -- false opts out of copy-on-select (CTX-0191)
//!     layout = { gaps_in = 1, gaps_out = 2 }, -- Hyprland-like panel gaps in cells, 0 = edge-to-edge (CTX-0177)
//!     decoration = { gaps_in = 6, gaps_out = 6, border = 2, radius = 6, content_inset = 6 }, -- Core-owned workspace decoration in logical px; unified gaps + content padding (CTX-0292/CTX-0333)
//!     scrollbar = { mode = "auto", width = 8 }, -- overlay scrollback thumb: hidden|always|auto (CTX-0181)
//!     mouse = { focus_follows_mouse = true }, -- opt-in hover focus, default false = click-to-focus (CTX-0260)
//!     mod_key = "alt", -- leader/mod for the shipped chrome map: "alt" (default) or "super" (CTX-0236)
//!     keymaps = {
//!         { chord = "ctrl+p", action = "palette:toggle", context = "global" },
//!     },
//! }
//! ```
//!
//! - Empty chunk / no return / `return nil` means "no user overrides"
//!   (bare defaults win further down the layer stack).
//! - Anything else returned (number, string, function, …) is a shape error.
//! - Unknown keys at any level are collected into [`ConfigData::undeclared`];
//!   `bitty-config` turns them into `UndeclaredField` (fail-closed, never
//!   silently ignored).
//! - Extraction itself is bounded ([`MAX_CONFIG_TOP_KEYS`],
//!   [`MAX_CONFIG_STRING_BYTES`], [`MAX_CONFIG_NESTED_KEYS`],
//!   [`MAX_CONFIG_KEYMAPS`]) so a hostile table cannot hang the host outside
//!   VM fuel accounting; deeper/narrower typed bounds still live in
//!   `bitty-config` validation.
//! - Metatables are ignored: extraction reads raw table contents only, so
//!   `__index` tricks cannot smuggle values past validation.
//! - Error messages carry key names and piccolo line info only — never file
//!   contents beyond the offending line.

use piccolo::{Context, ExecutorMode, Table, Value, Variadic};

use crate::{DriveOutcome, LuaVm, SuspendReason, VmError};

/// Maximum top-level keys read from the returned table.
pub const MAX_CONFIG_TOP_KEYS: usize = 64;

/// Maximum bytes per extracted string (typed validation enforces tighter
/// per-field bounds afterwards; this is the host DoS cap).
pub const MAX_CONFIG_STRING_BYTES: usize = 2048;

/// Maximum keys read from any nested table (`appearance`/`font`/`window`/
/// `terminal`/`selection`/`layout`/`scrollbar`/`mouse`/keymap entry).
pub const MAX_CONFIG_NESTED_KEYS: usize = 32;

/// Maximum keymap entries read (mirrors `bitty-config` `MAX_KEYMAPS` so the
/// host never iterates unbounded sequences outside fuel accounting).
pub const MAX_CONFIG_KEYMAPS: usize = 1024;

/// A single key mapping, plain data mirroring `bitty-config` `KeymapEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapData {
    /// Chord string, e.g. `"ctrl+p"`.
    pub chord: String,
    /// Action/command identifier.
    pub action: String,
    /// Context, e.g. `"global"`.
    pub context: String,
}

/// Font overrides, plain data.
///
/// Each field is optional so callers can distinguish "table absent" (outer
/// `Option` on [`ConfigData::font`]) from "key absent inside a present
/// table" (inner `Option`); partial tables are rejected downstream rather
/// than silently defaulted — except `line_height`/`letter_spacing`, which
/// default to [`DEFAULT_LINE_HEIGHT`]/[`DEFAULT_LETTER_SPACING`] when a
/// `font` table is present but omits them (backwards compatible with
/// existing `{ family, size }` tables).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FontData {
    /// Font family.
    pub family: Option<String>,
    /// Point size.
    pub size: Option<f64>,
    /// Line-height multiplier.
    pub line_height: Option<f32>,
    /// Extra advance in px.
    pub letter_spacing: Option<f32>,
}

/// Window overrides, plain data (see [`FontData`] for `Option` semantics).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowData {
    /// Opacity.
    pub opacity: Option<f64>,
    /// Padding.
    pub padding: Option<i64>,
    /// Corner radius in physical px (CTX-0241 S0: parsed no-op, default 0).
    pub radius_px: Option<i64>,
}

/// Terminal overrides, plain data (see [`FontData`] for `Option` semantics).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TerminalData {
    /// Scrollback lines.
    pub scrollback: Option<i64>,
    /// Optional shell `argv[0]` (present only when the key is set).
    pub shell: Option<String>,
    /// Lines per wheel notch (present only when the key is set).
    pub scroll_lines_per_notch: Option<i64>,
    /// Smooth-scroll pixels per wheel notch (present only when the key is set).
    pub scroll_pixels_per_notch: Option<i64>,
}

/// Selection overrides, plain data (CTX-0191; see [`FontData`] for `Option`
/// semantics).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SelectionData {
    /// Auto-copy on select (present only when the key is set).
    pub auto_copy: Option<bool>,
}

/// Layout overrides, plain data (CTX-0177; see [`FontData`] for `Option`
/// semantics).
///
/// Hyprland-like panel gaps in cells: `gaps_in` between sibling panes,
/// `gaps_out` around the container edge (`0` = edge-to-edge tiling).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LayoutData {
    /// Inner gap in cells (present only when the key is set).
    pub gaps_in: Option<i64>,
    /// Outer gap in cells (present only when the key is set).
    pub gaps_out: Option<i64>,
}

/// Core-owned workspace decoration overrides, plain data (CTX-0292; unified
/// CTX-0333; see [`FontData`] for `Option` semantics).
///
/// Logical-pixel decoration per the workspace-compositor contract:
/// `gaps_in`/`gaps_out` between/around views (defaults unified at `6/6`),
/// `border` inside each View frame, `radius` for frame corners, and
/// `content_inset` for the inner padding between the border and the painted
/// content (CTX-0333). Range-checked downstream in `bitty-config`
/// (fail-closed).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DecorationData {
    /// Inner gap in logical px (present only when the key is set).
    pub gaps_in: Option<i64>,
    /// Outer gap in logical px (present only when the key is set).
    pub gaps_out: Option<i64>,
    /// View frame border in logical px (present only when the key is set).
    pub border: Option<i64>,
    /// View frame corner radius in logical px (present only when the key is
    /// set).
    pub radius: Option<i64>,
    /// Content inset in logical px (present only when the key is set).
    pub content_inset: Option<i64>,
}

/// Scrollbar overrides, plain data (CTX-0181; see [`FontData`] for `Option`
/// semantics).
///
/// Overlay scrollback thumb: `mode` is `"hidden"` (default,
/// geometry-neutral), `"always"`, or `"auto"` (revealed on mouse
/// proximity/hover/drag); `width` is the thumb width in logical pixels.
/// Range-checked downstream in `bitty-config` (fail-closed).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScrollbarData {
    /// Display mode string (present only when the key is set).
    pub mode: Option<String>,
    /// Thumb width in logical pixels (present only when the key is set).
    pub width: Option<i64>,
}

/// Mouse overrides, plain data (CTX-0260; see [`FontData`] for `Option`
/// semantics).
///
/// `focus_follows_mouse` opts into hover moving keyboard focus
/// (default-off preserves click-to-focus). Wrong types fail closed as
/// `ShapeError` downstream (never coerced).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MouseData {
    /// Hover-focus opt-in (present only when the key is set).
    pub focus_follows_mouse: Option<bool>,
}

/// Plain-data user configuration extracted from the Lua chunk.
///
/// Every field is optional: absent means "this layer says nothing". Unknown
/// keys are reported via `undeclared` for fail-closed diagnostics.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ConfigData {
    /// Top-level `theme` alias.
    pub theme: Option<String>,
    /// `appearance.theme` (wins over the alias when both are present).
    pub appearance_theme: Option<String>,
    /// `font` table.
    pub font: Option<FontData>,
    /// `window` table.
    pub window: Option<WindowData>,
    /// `terminal` table.
    pub terminal: Option<TerminalData>,
    /// `selection` table (CTX-0191).
    pub selection: Option<SelectionData>,
    /// `layout` table (CTX-0177 panel gaps).
    pub layout: Option<LayoutData>,
    /// `decoration` table (CTX-0292 Core-owned workspace decoration).
    pub decoration: Option<DecorationData>,
    /// `scrollbar` table (CTX-0181 overlay scrollbar).
    pub scrollbar: Option<ScrollbarData>,
    /// `mouse` table (CTX-0260 focus-follows-mouse).
    pub mouse: Option<MouseData>,
    /// Top-level `mod_key` scalar (CTX-0236 leader/mod for the shipped
    /// chrome map; raw string, parsed fail-closed downstream).
    pub mod_key: Option<String>,
    /// `keymaps` array.
    pub keymaps: Option<Vec<KeymapData>>,
    /// Dotted unknown key paths (e.g. `"plugins"`, `"keymaps[2].foo"`),
    /// sorted for deterministic messages.
    pub undeclared: Vec<String>,
}

impl ConfigData {
    /// Empty data (no overrides).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether no overrides were declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.theme.is_none()
            && self.appearance_theme.is_none()
            && self.font.is_none()
            && self.window.is_none()
            && self.terminal.is_none()
            && self.selection.is_none()
            && self.layout.is_none()
            && self.decoration.is_none()
            && self.scrollbar.is_none()
            && self.mouse.is_none()
            && self.mod_key.is_none()
            && self.keymaps.is_none()
    }
}

/// Outcome of [`LuaVm::eval_config`].
///
/// Box-free by design: config evaluates once per startup/reload (cold path),
/// so the `Completed` variant's `ConfigData` size costs nothing hot. The
/// `line_height`/`letter_spacing` fields (CTX-0157) push the enum over
/// clippy's 200-byte heuristic; boxing would churn the public API for no
/// runtime benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigOutcome {
    /// Chunk finished within budgets; data (possibly empty) extracted.
    ///
    /// Boxed: `ConfigData` grows with every config key (CTX-0185 pushed the
    /// inline variant past clippy's large-enum threshold); config evaluation
    /// is a cold startup path, so one pointer indirection costs nothing and
    /// keeps future keys from re-tripping the lint.
    Completed {
        /// Extracted plain data.
        data: Box<ConfigData>,
        /// Approx instructions consumed.
        instructions_used: u64,
        /// Wall elapsed ms.
        wall_elapsed_ms: u64,
        /// Memory used after execution.
        memory_used: usize,
        /// Whether warning threshold was hit.
        warning_triggered: bool,
    },
    /// Budget exceeded (fail-closed); VM is now suspended.
    Suspended {
        /// Reason.
        reason: SuspendReason,
        /// Instructions consumed up to suspend.
        instructions_used: u64,
        /// Wall elapsed ms.
        wall_elapsed_ms: u64,
        /// Memory used.
        memory_used: usize,
    },
    /// Chunk failed to load/compile or errored at runtime. Message carries
    /// piccolo diagnostics (including line info) but never file contents.
    LuaError {
        /// Diagnostic message.
        message: String,
    },
    /// Chunk ran but the returned value has the wrong shape (not a table,
    /// wrong field types, overlong strings, oversize sequences). Message
    /// names the offending key path only.
    ShapeError {
        /// Diagnostic message.
        message: String,
    },
}

/// Evaluate one config chunk and extract [`ConfigData`].
///
/// Implemented on [`LuaVm`] (same crate, so private budget state is shared
/// with [`crate::LuaVm::drive_chunk`]): identical fuel, wall-clock, and
/// memory enforcement as plugin execution, identical fail-closed suspension.
pub trait ConfigEval {
    /// Evaluate `code` as a config chunk.
    ///
    /// Fail-closed: an already-suspended VM refuses with
    /// `Err(VmError::Suspended)`; budget exceed suspends and yields
    /// [`ConfigOutcome::Suspended`]; load/runtime failures yield
    /// [`ConfigOutcome::LuaError`]; wrong shapes yield
    /// [`ConfigOutcome::ShapeError`]. Deterministic given identical source
    /// and budgets on a fresh VM.
    fn eval_config(&mut self, code: &str) -> Result<ConfigOutcome, VmError>;
}

impl ConfigEval for LuaVm {
    fn eval_config(&mut self, code: &str) -> Result<ConfigOutcome, VmError> {
        match self.drive_chunk(code)? {
            DriveOutcome::Suspended {
                reason,
                instructions_used,
                wall_elapsed_ms,
                memory_used,
            } => Ok(ConfigOutcome::Suspended {
                reason,
                instructions_used,
                wall_elapsed_ms,
                memory_used,
            }),
            DriveOutcome::Failed { message } => Ok(ConfigOutcome::LuaError { message }),
            DriveOutcome::Ready { stashed } => {
                let mode = self.lua.enter(|ctx| ctx.fetch(&stashed).mode());
                if mode != ExecutorMode::Result {
                    // Stopped / yielded / already-done without a result:
                    // mirror `execute` leniency and treat as empty config.
                    return Ok(ConfigOutcome::Completed {
                        data: Box::new(ConfigData::empty()),
                        instructions_used: self.instructions_used,
                        wall_elapsed_ms: self.wall_elapsed_ms,
                        memory_used: self.memory_used,
                        warning_triggered: self.warning_triggered,
                    });
                }
                enum Taken {
                    Values(Vec<ValueSnapshot>),
                    LuaErr(String),
                    BadMode(String),
                }
                let taken = self.lua.enter(|ctx| {
                    let exec = ctx.fetch(&stashed);
                    match exec.take_result::<Variadic<Vec<Value>>>(ctx) {
                        Ok(Ok(Variadic(values))) => {
                            // Snapshot GC-bound values into owned data inside
                            // the same enter call.
                            let mut out = Vec::with_capacity(values.len().min(8));
                            for v in values {
                                out.push(ValueSnapshot::capture(ctx, v));
                            }
                            Taken::Values(out)
                        }
                        Ok(Err(e)) => Taken::LuaErr(format!("{e}")),
                        Err(e) => Taken::BadMode(format!("{e:?}")),
                    }
                });
                match taken {
                    Taken::LuaErr(message) => Ok(ConfigOutcome::LuaError { message }),
                    Taken::BadMode(message) => Ok(ConfigOutcome::LuaError { message }),
                    Taken::Values(values) => match ConfigData::from_returns(&values) {
                        Ok(data) => Ok(ConfigOutcome::Completed {
                            data: Box::new(data),
                            instructions_used: self.instructions_used,
                            wall_elapsed_ms: self.wall_elapsed_ms,
                            memory_used: self.memory_used,
                            warning_triggered: self.warning_triggered,
                        }),
                        Err(message) => Ok(ConfigOutcome::ShapeError { message }),
                    },
                }
            }
        }
    }
}

/// Owned snapshot of a returned Lua value.
///
/// Captured inside `enter` so extraction runs on plain Rust data with no GC
/// borrows escaping the VM.
#[derive(Debug, Clone, PartialEq)]
enum ValueSnapshot {
    /// Lua nil.
    Nil,
    /// Boolean.
    Bool(bool),
    /// Integer.
    Int(i64),
    /// Float.
    Float(f64),
    /// UTF-8 string (invalid UTF-8 snapshots as `Binary`).
    Str(String),
    /// Non-UTF-8 string bytes.
    Binary,
    /// Table: association list (string keys only matter; others recorded as
    /// `NonStringKey`) plus optional sequence portion.
    Table {
        /// String-keyed pairs in iteration order (capped).
        pairs: Vec<(String, Box<ValueSnapshot>)>,
        /// Sequence portion `1..=n` (capped); `None` when the table has no
        /// sequence head (length 0).
        seq: Vec<ValueSnapshot>,
        /// Whether iteration hit the key cap (table had more keys).
        truncated: bool,
        /// Whether non-string keys were present.
        has_non_string_keys: bool,
    },
    /// Anything else (function, thread, userdata): opaque, always a shape
    /// error when encountered in config position.
    Opaque(&'static str),
}

impl ValueSnapshot {
    /// Capture a GC-bound value into owned data (must run inside `enter`).
    fn capture<'gc>(ctx: Context<'gc>, value: Value<'gc>) -> Self {
        match value {
            Value::Nil => Self::Nil,
            Value::Boolean(b) => Self::Bool(b),
            Value::Integer(i) => Self::Int(i),
            Value::Number(f) => Self::Float(f),
            Value::String(s) => match std::str::from_utf8(s.as_bytes()) {
                Ok(text) => Self::Str(text.to_string()),
                Err(_) => Self::Binary,
            },
            Value::Table(t) => Self::capture_table(ctx, t, 0),
            Value::Function(f) => Self::Opaque(match f {
                piccolo::Function::Closure(_) => "function",
                piccolo::Function::Callback(_) => "function",
            }),
            Value::Thread(_) => Self::Opaque("thread"),
            Value::UserData(_) => Self::Opaque("userdata"),
        }
    }

    /// Capture one table level; nested tables recurse at most once more
    /// (`depth` 0 = top, 1 = nested, 2 = stop with empty children).
    fn capture_table<'gc>(ctx: Context<'gc>, table: Table<'gc>, depth: u8) -> Self {
        let cap = if depth == 0 {
            MAX_CONFIG_TOP_KEYS + 1
        } else {
            MAX_CONFIG_NESTED_KEYS + 8
        };
        let mut pairs = Vec::new();
        let mut has_non_string_keys = false;
        let mut truncated = false;
        let mut count = 0usize;
        for (key, val) in table.iter() {
            // Integer keys belong to the sequence portion; skip them here
            // (captured separately below) without spending the pair cap.
            if matches!(key, Value::Integer(_)) {
                continue;
            }
            count += 1;
            if count > cap {
                truncated = true;
                break;
            }
            match key {
                Value::String(s) => match std::str::from_utf8(s.as_bytes()) {
                    Ok(name) => {
                        let child = if depth >= 2 {
                            Self::Nil
                        } else {
                            match val {
                                Value::Table(nested) => Self::capture_table(ctx, nested, depth + 1),
                                other => Self::capture_shallow(other),
                            }
                        };
                        pairs.push((name.to_string(), Box::new(child)));
                    }
                    Err(_) => has_non_string_keys = true,
                },
                _ => has_non_string_keys = true,
            }
        }
        // Sequence portion (keymaps arrays live here).
        let mut seq = Vec::new();
        if depth <= 1 {
            let len = table.length().max(0) as usize;
            let want = len.min(MAX_CONFIG_KEYMAPS + 1);
            for i in 1..=(want as i64) {
                let v = table.get(ctx, i);
                if matches!(v, Value::Nil) {
                    break;
                }
                seq.push(match v {
                    // Depth 0: general nested table. Depth 1: keymap entry
                    // whose scalar fields are captured as leaves.
                    Value::Table(nested) if depth == 0 => {
                        Self::capture_table(ctx, nested, depth + 1)
                    }
                    Value::Table(nested) => Self::capture_entry_table(ctx, nested),
                    other => Self::capture_shallow(other),
                });
            }
            if len > want {
                truncated = true;
            }
        }
        Self::Table {
            pairs,
            seq,
            truncated,
            has_non_string_keys,
        }
    }

    /// Capture a keymap entry table: string-keyed scalar fields only, no
    /// further nesting and no sequence (entries are flat records).
    fn capture_entry_table(ctx: Context<'_>, table: Table<'_>) -> Self {
        let mut pairs = Vec::new();
        let mut has_non_string_keys = false;
        let mut truncated = false;
        let mut count = 0usize;
        for (key, val) in table.iter() {
            if matches!(key, Value::Integer(_)) {
                continue;
            }
            count += 1;
            if count > MAX_CONFIG_NESTED_KEYS {
                truncated = true;
                break;
            }
            match key {
                Value::String(s) => match std::str::from_utf8(s.as_bytes()) {
                    Ok(name) => {
                        // Entries are flat: any table value opaqued.
                        let child = Self::capture_shallow(val);
                        // Silence unused-ctx without recursion: ctx unused here
                        // by construction (leaves only).
                        let _ = ctx;
                        pairs.push((name.to_string(), Box::new(child)));
                    }
                    Err(_) => has_non_string_keys = true,
                },
                _ => has_non_string_keys = true,
            }
        }
        Self::Table {
            pairs,
            seq: Vec::new(),
            truncated,
            has_non_string_keys,
        }
    }

    /// Capture a leaf value (never recursing into tables).
    fn capture_shallow(value: Value<'_>) -> Self {
        match value {
            Value::Table(_) => Self::Opaque("table"),
            other => {
                // Reuse the leaf arms of capture without table recursion.
                match other {
                    Value::Nil => Self::Nil,
                    Value::Boolean(b) => Self::Bool(b),
                    Value::Integer(i) => Self::Int(i),
                    Value::Number(f) => Self::Float(f),
                    Value::String(s) => match std::str::from_utf8(s.as_bytes()) {
                        Ok(text) => Self::Str(text.to_string()),
                        Err(_) => Self::Binary,
                    },
                    Value::Function(f) => Self::Opaque(match f {
                        piccolo::Function::Closure(_) => "function",
                        piccolo::Function::Callback(_) => "function",
                    }),
                    Value::Thread(_) => Self::Opaque("thread"),
                    Value::UserData(_) => Self::Opaque("userdata"),
                    Value::Table(_) => Self::Opaque("table"),
                }
            }
        }
    }

    /// Human-readable type name for shape errors (never includes values).
    fn kind(&self) -> &'static str {
        match self {
            Self::Nil => "nil",
            Self::Bool(_) => "boolean",
            Self::Int(_) | Self::Float(_) => "number",
            Self::Str(_) | Self::Binary => "string",
            Self::Table { .. } => "table",
            Self::Opaque(k) => k,
        }
    }
}

impl ConfigData {
    /// Build data from the chunk's return values.
    fn from_returns(values: &[ValueSnapshot]) -> Result<Self, String> {
        if values.is_empty() {
            return Ok(Self::empty());
        }
        if values.len() > 1 {
            return Err(format!(
                "config must return a single table (returned {} values)",
                values.len()
            ));
        }
        match &values[0] {
            ValueSnapshot::Nil => Ok(Self::empty()),
            ValueSnapshot::Table { .. } => Self::from_table(&values[0], ""),
            other => Err(format!(
                "config must return a table (returned {})",
                other.kind()
            )),
        }
    }

    /// Extract known keys from a top-level table snapshot.
    fn from_table(table: &ValueSnapshot, _path: &str) -> Result<Self, String> {
        let (pairs, truncated, has_non_string_keys) = match table {
            ValueSnapshot::Table {
                pairs,
                truncated,
                has_non_string_keys,
                ..
            } => (pairs, *truncated, *has_non_string_keys),
            _ => return Err("config must return a table".to_string()),
        };
        if truncated {
            return Err(format!("config table exceeds {MAX_CONFIG_TOP_KEYS} keys"));
        }
        if has_non_string_keys {
            return Err("config keys must be strings".to_string());
        }
        let mut out = Self::empty();
        for (key, val) in pairs {
            match key.as_str() {
                "theme" => out.theme = Some(expect_string(key, val)?),
                "appearance" => {
                    let nested = expect_table(key, val)?;
                    check_nested_keys(key, nested, &["theme"])?;
                    if let Some(t) = get_field(nested, "theme") {
                        out.appearance_theme = Some(expect_string("appearance.theme", t)?);
                    }
                }
                "font" => {
                    let nested = expect_table(key, val)?;
                    check_nested_keys(
                        key,
                        nested,
                        &["family", "size", "line_height", "letter_spacing"],
                    )?;
                    let family = match get_field(nested, "family") {
                        Some(v) => Some(expect_string("font.family", v)?),
                        None => None,
                    };
                    let size = match get_field(nested, "size") {
                        Some(v) => Some(expect_number("font.size", v)?),
                        None => None,
                    };
                    let line_height = match get_field(nested, "line_height") {
                        Some(v) => Some(expect_number("font.line_height", v)? as f32),
                        None => None,
                    };
                    let letter_spacing = match get_field(nested, "letter_spacing") {
                        Some(v) => Some(expect_number("font.letter_spacing", v)? as f32),
                        None => None,
                    };
                    out.font = Some(FontData {
                        family,
                        size,
                        line_height,
                        letter_spacing,
                    });
                }
                "window" => {
                    let nested = expect_table(key, val)?;
                    check_nested_keys(key, nested, &["opacity", "padding", "radius_px"])?;
                    let opacity = match get_field(nested, "opacity") {
                        Some(v) => Some(expect_number("window.opacity", v)?),
                        None => None,
                    };
                    let padding = match get_field(nested, "padding") {
                        Some(v) => Some(expect_integer("window.padding", v)?),
                        None => None,
                    };
                    let radius_px = match get_field(nested, "radius_px") {
                        Some(v) => Some(expect_integer("window.radius_px", v)?),
                        None => None,
                    };
                    out.window = Some(WindowData {
                        opacity,
                        padding,
                        radius_px,
                    });
                }
                "terminal" => {
                    let nested = expect_table(key, val)?;
                    check_nested_keys(
                        key,
                        nested,
                        &[
                            "scrollback",
                            "shell",
                            "scroll_lines_per_notch",
                            "scroll_pixels_per_notch",
                        ],
                    )?;
                    let scrollback = match get_field(nested, "scrollback") {
                        Some(v) => Some(expect_integer("terminal.scrollback", v)?),
                        None => None,
                    };
                    let shell = match get_field(nested, "shell") {
                        Some(v) => Some(expect_string("terminal.shell", v)?),
                        None => None,
                    };
                    let scroll_lines_per_notch = match get_field(nested, "scroll_lines_per_notch") {
                        Some(v) => Some(expect_integer("terminal.scroll_lines_per_notch", v)?),
                        None => None,
                    };
                    let scroll_pixels_per_notch = match get_field(nested, "scroll_pixels_per_notch")
                    {
                        Some(v) => Some(expect_integer("terminal.scroll_pixels_per_notch", v)?),
                        None => None,
                    };
                    out.terminal = Some(TerminalData {
                        scrollback,
                        shell,
                        scroll_lines_per_notch,
                        scroll_pixels_per_notch,
                    });
                }
                "selection" => {
                    // CTX-0191: `selection = { auto_copy = false }` opts out
                    // of copy-on-select; absent table/key means "says nothing".
                    let nested = expect_table(key, val)?;
                    check_nested_keys(key, nested, &["auto_copy"])?;
                    let auto_copy = match get_field(nested, "auto_copy") {
                        Some(v) => Some(expect_bool("selection.auto_copy", v)?),
                        None => None,
                    };
                    out.selection = Some(SelectionData { auto_copy });
                }
                "layout" => {
                    // CTX-0177: `layout = { gaps_in = 1, gaps_out = 2 }`
                    // sets Hyprland-like panel gaps in cells; absent
                    // table/key means "says nothing". Integers only
                    // (floats rejected like every other cell/count key);
                    // range-checked downstream in `bitty-config`.
                    let nested = expect_table(key, val)?;
                    check_nested_keys(key, nested, &["gaps_in", "gaps_out"])?;
                    let gaps_in = match get_field(nested, "gaps_in") {
                        Some(v) => Some(expect_integer("layout.gaps_in", v)?),
                        None => None,
                    };
                    let gaps_out = match get_field(nested, "gaps_out") {
                        Some(v) => Some(expect_integer("layout.gaps_out", v)?),
                        None => None,
                    };
                    out.layout = Some(LayoutData { gaps_in, gaps_out });
                }
                "decoration" => {
                    // CTX-0292/CTX-0333: `decoration = { gaps_in = 6,
                    // gaps_out = 6, border = 2, radius = 6,
                    // content_inset = 6 }` sets the Core-owned workspace
                    // decoration in logical px; absent table/key means "says
                    // nothing". Integers only (floats rejected like every
                    // other px/count key); range-checked downstream in
                    // `bitty-config`.
                    let nested = expect_table(key, val)?;
                    check_nested_keys(
                        key,
                        nested,
                        &["gaps_in", "gaps_out", "border", "radius", "content_inset"],
                    )?;
                    let gaps_in = match get_field(nested, "gaps_in") {
                        Some(v) => Some(expect_integer("decoration.gaps_in", v)?),
                        None => None,
                    };
                    let gaps_out = match get_field(nested, "gaps_out") {
                        Some(v) => Some(expect_integer("decoration.gaps_out", v)?),
                        None => None,
                    };
                    let border = match get_field(nested, "border") {
                        Some(v) => Some(expect_integer("decoration.border", v)?),
                        None => None,
                    };
                    let radius = match get_field(nested, "radius") {
                        Some(v) => Some(expect_integer("decoration.radius", v)?),
                        None => None,
                    };
                    let content_inset = match get_field(nested, "content_inset") {
                        Some(v) => Some(expect_integer("decoration.content_inset", v)?),
                        None => None,
                    };
                    out.decoration = Some(DecorationData {
                        gaps_in,
                        gaps_out,
                        border,
                        radius,
                        content_inset,
                    });
                }
                "scrollbar" => {
                    // CTX-0181: `scrollbar = { mode = "auto", width = 8 }`
                    // sets the overlay scrollback thumb; absent table/key
                    // means "says nothing". `mode` is a free string here
                    // (exact-spelling check lives in `bitty-config`,
                    // fail-closed); `width` is an integer (floats rejected
                    // like every other count key); range-checked downstream.
                    let nested = expect_table(key, val)?;
                    check_nested_keys(key, nested, &["mode", "width"])?;
                    let mode = match get_field(nested, "mode") {
                        Some(v) => Some(expect_string("scrollbar.mode", v)?),
                        None => None,
                    };
                    let width = match get_field(nested, "width") {
                        Some(v) => Some(expect_integer("scrollbar.width", v)?),
                        None => None,
                    };
                    out.scrollbar = Some(ScrollbarData { mode, width });
                }
                "mouse" => {
                    // CTX-0260: `mouse = { focus_follows_mouse = true }`
                    // opts into hover moving keyboard focus; absent
                    // table/key means "says nothing" (default-off
                    // click-to-focus preserved). Booleans only (never
                    // coerced); validated downstream in `bitty-config`.
                    let nested = expect_table(key, val)?;
                    check_nested_keys(key, nested, &["focus_follows_mouse"])?;
                    let focus_follows_mouse = match get_field(nested, "focus_follows_mouse") {
                        Some(v) => Some(expect_bool("mouse.focus_follows_mouse", v)?),
                        None => None,
                    };
                    out.mouse = Some(MouseData {
                        focus_follows_mouse,
                    });
                }
                "keymaps" => {
                    out.keymaps = Some(extract_keymaps(val)?);
                }
                // CTX-0236: top-level `mod_key` scalar (raw string; typed
                // parsing and fail-closed validation live downstream in
                // `bitty-config`, like the `theme` alias).
                "mod_key" => out.mod_key = Some(expect_string(key, val)?),
                _ => out.undeclared.push(key.clone()),
            }
        }
        out.undeclared.sort();
        out.undeclared.dedup();
        Ok(out)
    }
}

/// Look up a string key in a captured table's pairs.
fn get_field<'a>(
    nested: &'a [(String, Box<ValueSnapshot>)],
    key: &str,
) -> Option<&'a ValueSnapshot> {
    nested
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_ref())
}

/// Reject unknown keys inside a nested table (fail-closed, deterministic).
fn check_nested_keys(
    table_name: &str,
    nested: &[(String, Box<ValueSnapshot>)],
    allowed: &[&str],
) -> Result<(), String> {
    if nested.len() > MAX_CONFIG_NESTED_KEYS {
        return Err(format!(
            "{table_name}: exceeds {MAX_CONFIG_NESTED_KEYS} keys"
        ));
    }
    let mut bad: Vec<&str> = nested
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| !allowed.contains(k))
        .collect();
    bad.sort_unstable();
    bad.dedup();
    if let Some(first) = bad.first() {
        return Err(format!("undeclared field '{table_name}.{first}'"));
    }
    Ok(())
}

/// Expect a string value (names the key path only, never the value).
fn expect_string(path: &str, val: &ValueSnapshot) -> Result<String, String> {
    match val {
        ValueSnapshot::Str(s) => {
            if s.len() > MAX_CONFIG_STRING_BYTES {
                return Err(format!(
                    "{path}: string exceeds {MAX_CONFIG_STRING_BYTES} bytes"
                ));
            }
            Ok(s.clone())
        }
        ValueSnapshot::Nil => Err(format!("{path}: expected string (found nil)")),
        other => Err(format!("{path}: expected string (found {})", other.kind())),
    }
}

/// Expect a number value (integers and floats both accepted).
fn expect_number(path: &str, val: &ValueSnapshot) -> Result<f64, String> {
    match val {
        ValueSnapshot::Int(i) => Ok(*i as f64),
        ValueSnapshot::Float(f) => Ok(*f),
        ValueSnapshot::Nil => Err(format!("{path}: expected number (found nil)")),
        other => Err(format!("{path}: expected number (found {})", other.kind())),
    }
}

/// Expect an integer value (floats rejected, even integral ones).
fn expect_integer(path: &str, val: &ValueSnapshot) -> Result<i64, String> {
    match val {
        ValueSnapshot::Int(i) => Ok(*i),
        ValueSnapshot::Nil => Err(format!("{path}: expected integer (found nil)")),
        other => Err(format!("{path}: expected integer (found {})", other.kind())),
    }
}

/// Expect a boolean value (CTX-0191 `selection.auto_copy`; fail-closed on
/// numbers/strings/nil — never coerces, never echoes the value).
fn expect_bool(path: &str, val: &ValueSnapshot) -> Result<bool, String> {
    match val {
        ValueSnapshot::Bool(b) => Ok(*b),
        ValueSnapshot::Nil => Err(format!("{path}: expected boolean (found nil)")),
        other => Err(format!("{path}: expected boolean (found {})", other.kind())),
    }
}

/// Expect a table value and return its pairs.
fn expect_table<'a>(
    path: &str,
    val: &'a ValueSnapshot,
) -> Result<&'a [(String, Box<ValueSnapshot>)], String> {
    match val {
        ValueSnapshot::Table { pairs, .. } => Ok(pairs),
        ValueSnapshot::Nil => Err(format!("{path}: expected table (found nil)")),
        other => Err(format!("{path}: expected table (found {})", other.kind())),
    }
}

/// Extract the keymaps array (1-based Lua sequence of `{chord, action,
/// context}` tables).
fn extract_keymaps(val: &ValueSnapshot) -> Result<Vec<KeymapData>, String> {
    let (pairs, seq, truncated, has_non_string_keys) = match val {
        ValueSnapshot::Table {
            pairs,
            seq,
            truncated,
            has_non_string_keys,
        } => (pairs, seq, *truncated, *has_non_string_keys),
        ValueSnapshot::Nil => return Err("keymaps: expected array (found nil)".to_string()),
        other => {
            return Err(format!("keymaps: expected array (found {})", other.kind()));
        }
    };
    if !pairs.is_empty() || has_non_string_keys {
        return Err("keymaps: expected array (found map keys)".to_string());
    }
    if truncated {
        return Err(format!("keymaps: exceeds {MAX_CONFIG_KEYMAPS} entries"));
    }
    if seq.len() > MAX_CONFIG_KEYMAPS {
        return Err(format!("keymaps: exceeds {MAX_CONFIG_KEYMAPS} entries"));
    }
    let mut out = Vec::with_capacity(seq.len());
    for (idx, entry) in seq.iter().enumerate() {
        let path = format!("keymaps[{}]", idx + 1);
        let nested = expect_table(&path, entry)?;
        check_nested_keys(&path, nested, &["chord", "action", "context"])?;
        let chord = match get_field(nested, "chord") {
            Some(v) => expect_string(&format!("{path}.chord"), v)?,
            None => return Err(format!("{path}.chord: missing")),
        };
        let action = match get_field(nested, "action") {
            Some(v) => expect_string(&format!("{path}.action"), v)?,
            None => return Err(format!("{path}.action: missing")),
        };
        let context = match get_field(nested, "context") {
            Some(v) => expect_string(&format!("{path}.context"), v)?,
            None => return Err(format!("{path}.context: missing")),
        };
        out.push(KeymapData {
            chord,
            action,
            context,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LuaVm;

    /// Evaluate `code` on a fresh default VM; panics on suspension (tests
    /// that expect success must use cheap chunks).
    fn eval_ok(code: &str) -> ConfigData {
        let mut vm = LuaVm::new("test.config");
        match vm.eval_config(code).expect("eval must not refuse") {
            ConfigOutcome::Completed { data, .. } => *data,
            other => panic!("expected completed, got {other:?}"),
        }
    }

    #[test]
    fn empty_chunk_yields_empty_data() {
        for code in ["", "-- comment only\n", "return nil"] {
            let data = eval_ok(code);
            assert!(data.is_empty(), "code: {code:?}");
            assert!(data.undeclared.is_empty());
        }
    }

    #[test]
    fn top_theme_alias_extracts() {
        let data = eval_ok(r#"return { theme = "dark" }"#);
        assert_eq!(data.theme.as_deref(), Some("dark"));
        assert_eq!(data.appearance_theme, None);
    }

    #[test]
    fn nested_tables_extract() {
        let data = eval_ok(
            r#"return {
                appearance = { theme = "bitty-dark" },
                font = { family = "JetBrains Mono", size = 13.0 },
                window = { opacity = 0.95, padding = 8 },
                terminal = { scrollback = 10000, shell = "/bin/fish", scroll_lines_per_notch = 3, scroll_pixels_per_notch = 16 },
            }"#,
        );
        assert_eq!(data.appearance_theme.as_deref(), Some("bitty-dark"));
        let font = data.font.unwrap();
        assert_eq!(font.family.as_deref(), Some("JetBrains Mono"));
        assert!((font.size.unwrap() - 13.0).abs() < f64::EPSILON);
        let window = data.window.unwrap();
        assert!((window.opacity.unwrap() - 0.95).abs() < f64::EPSILON);
        assert_eq!(window.padding, Some(8));
        // CTX-0241 S0: absent radius key means "says nothing" (None).
        assert_eq!(window.radius_px, None);
        let term = data.terminal.unwrap();
        assert_eq!(term.scrollback, Some(10000));
        assert_eq!(term.shell.as_deref(), Some("/bin/fish"));
        assert_eq!(term.scroll_lines_per_notch, Some(3));
        assert_eq!(term.scroll_pixels_per_notch, Some(16));
    }

    #[test]
    fn window_radius_extracts_and_absent_means_no_override() {
        // CTX-0241 S0: explicit integer parses; absent key is `None` so
        // merge keeps the lower-precedence value (0 when no layer sets it).
        let data = eval_ok(r#"return { window = { opacity = 1.0, padding = 8, radius_px = 12 } }"#);
        let window = data.window.unwrap();
        assert_eq!(window.radius_px, Some(12));
        let data = eval_ok(r#"return { window = { opacity = 1.0, padding = 8 } }"#);
        assert_eq!(data.window.unwrap().radius_px, None);
    }

    #[test]
    fn terminal_scroll_keys_absent_means_no_override() {
        // Scroll keys are optional extras (like `shell`): an absent key is
        // `None` so merge keeps the lower-precedence value instead of
        // resetting to a default (fail-closed attribution).
        let data = eval_ok(r#"return { terminal = { scrollback = 10000 } }"#);
        let term = data.terminal.unwrap();
        assert_eq!(term.scrollback, Some(10000));
        assert_eq!(term.shell, None);
        assert_eq!(term.scroll_lines_per_notch, None);
        assert_eq!(term.scroll_pixels_per_notch, None);
    }

    #[test]
    fn selection_auto_copy_extracts_and_absent_means_no_override() {
        // CTX-0191: explicit bool parses; absent table/key is `None` so merge
        // keeps the lower-precedence value (default true when no layer sets it).
        let data = eval_ok(r#"return { selection = { auto_copy = false } }"#);
        assert_eq!(data.selection.unwrap().auto_copy, Some(false));
        let data = eval_ok(r#"return { selection = { auto_copy = true } }"#);
        assert_eq!(data.selection.unwrap().auto_copy, Some(true));
        let data = eval_ok(r#"return { terminal = { scrollback = 10000 } }"#);
        assert_eq!(data.selection, None);
        let data = eval_ok(r#"return { selection = {} }"#);
        assert_eq!(data.selection.unwrap().auto_copy, None);
    }

    #[test]
    fn selection_auto_copy_wrong_type_is_shape_error_without_value() {
        // CTX-0191: fail-closed on non-boolean (no coercion, no value echo).
        let mut vm = LuaVm::new("test.selection-type");
        for code in [
            r#"return { selection = { auto_copy = 1 } }"#,
            r#"return { selection = { auto_copy = "false" } }"#,
            r#"return { selection = { auto_copy = 0 } }"#,
        ] {
            match vm.eval_config(code).expect("no refuse") {
                ConfigOutcome::ShapeError { message } => {
                    assert!(
                        message.contains("selection.auto_copy"),
                        "{code:?}: {message}"
                    );
                    assert!(!message.contains("false"), "must not echo value: {message}");
                }
                other => panic!("{code:?}: expected shape error, got {other:?}"),
            }
        }
    }

    #[test]
    fn mod_key_scalar_extracts_and_absent_means_no_override() {
        // CTX-0236: top-level `mod_key` extracts as a raw string (typed
        // parsing lives downstream); absent means `None` so merge keeps the
        // lower-precedence value (Alt when no layer sets it).
        let data = eval_ok(r#"return { mod_key = "super" }"#);
        assert_eq!(data.mod_key.as_deref(), Some("super"));
        assert!(!data.is_empty());
        let data = eval_ok(r#"return { theme = "dark" }"#);
        assert_eq!(data.mod_key, None);
        // Wrong type is a shape error naming the key, without echoing values.
        let mut vm = LuaVm::new("test.modkey-type");
        match vm
            .eval_config(r#"return { mod_key = 42 }"#)
            .expect("no refuse")
        {
            ConfigOutcome::ShapeError { message } => {
                assert!(message.contains("mod_key"), "{message}");
            }
            other => panic!("expected shape error, got {other:?}"),
        }
    }

    #[test]
    fn layout_gaps_extract_and_absent_means_no_override() {
        // CTX-0177: explicit integers parse; absent table/key is `None` so
        // merge keeps the lower-precedence value (0 when no layer sets it).
        let data = eval_ok(r#"return { layout = { gaps_in = 1, gaps_out = 2 } }"#);
        let layout = data.layout.unwrap();
        assert_eq!(layout.gaps_in, Some(1));
        assert_eq!(layout.gaps_out, Some(2));
        let data = eval_ok(r#"return { layout = { gaps_in = 0 } }"#);
        let layout = data.layout.unwrap();
        assert_eq!(layout.gaps_in, Some(0));
        assert_eq!(layout.gaps_out, None);
        let data = eval_ok(r#"return { terminal = { scrollback = 10000 } }"#);
        assert_eq!(data.layout, None);
        let data = eval_ok(r#"return { layout = {} }"#);
        let layout = data.layout.unwrap();
        assert_eq!(layout.gaps_in, None);
        assert_eq!(layout.gaps_out, None);
    }

    #[test]
    fn layout_gaps_wrong_type_is_shape_error_without_value() {
        // CTX-0177: fail-closed on non-integers (floats/strings/tables never
        // coerce, values never echoed).
        let mut vm = LuaVm::new("test.layout-type");
        for code in [
            r#"return { layout = { gaps_in = 1.5 } }"#,
            r#"return { layout = { gaps_in = "1" } }"#,
            r#"return { layout = { gaps_out = true } }"#,
            r#"return { layout = "wide" }"#,
            r#"return { layout = { gaps_in = 1, bogus = 2 } }"#,
        ] {
            match vm.eval_config(code).expect("no refuse") {
                ConfigOutcome::ShapeError { message } => {
                    assert!(message.contains("layout"), "{code:?}: {message}");
                }
                other => panic!("{code:?}: expected shape error, got {other:?}"),
            }
        }
    }

    #[test]
    fn decoration_extract_and_absent_means_no_override() {
        // CTX-0292/CTX-0333: explicit integers parse; absent table/key is
        // `None` so merge keeps the lower-precedence value (the unified
        // defaults 6/6/2/6/6 when no layer sets them).
        let data = eval_ok(
            r#"return { decoration = { gaps_in = 0, gaps_out = 1, border = 1, radius = 0, content_inset = 2 } }"#,
        );
        let dec = data.decoration.unwrap();
        assert_eq!(dec.gaps_in, Some(0));
        assert_eq!(dec.gaps_out, Some(1));
        assert_eq!(dec.border, Some(1));
        assert_eq!(dec.radius, Some(0));
        assert_eq!(dec.content_inset, Some(2));
        let data = eval_ok(r#"return { decoration = { border = 3 } }"#);
        let dec = data.decoration.unwrap();
        assert_eq!(dec.gaps_in, None);
        assert_eq!(dec.gaps_out, None);
        assert_eq!(dec.border, Some(3));
        assert_eq!(dec.radius, None);
        assert_eq!(dec.content_inset, None);
        let data = eval_ok(r#"return { terminal = { scrollback = 10000 } }"#);
        assert_eq!(data.decoration, None);
        let data = eval_ok(r#"return { decoration = {} }"#);
        let dec = data.decoration.unwrap();
        assert_eq!(dec.gaps_in, None);
        assert_eq!(dec.gaps_out, None);
        assert_eq!(dec.border, None);
        assert_eq!(dec.radius, None);
        assert_eq!(dec.content_inset, None);
    }

    #[test]
    fn decoration_wrong_type_is_shape_error_without_value() {
        // CTX-0292: fail-closed on non-integers (floats/strings/tables never
        // coerce, values never echoed).
        let mut vm = LuaVm::new("test.decoration-type");
        for code in [
            r#"return { decoration = { gaps_in = 1.5 } }"#,
            r#"return { decoration = { gaps_in = "4" } }"#,
            r#"return { decoration = { gaps_out = true } }"#,
            r#"return { decoration = { border = 1.5 } }"#,
            r#"return { decoration = { radius = false } }"#,
            r#"return { decoration = { content_inset = 1.5 } }"#,
            r#"return { decoration = { content_inset = "6" } }"#,
            r#"return { decoration = "bold" }"#,
            r#"return { decoration = { gaps_in = 1, bogus = 2 } }"#,
        ] {
            match vm.eval_config(code).expect("no refuse") {
                ConfigOutcome::ShapeError { message } => {
                    assert!(message.contains("decoration"), "{code:?}: {message}");
                }
                other => panic!("{code:?}: expected shape error, got {other:?}"),
            }
        }
    }

    #[test]
    fn scrollbar_extract_and_absent_means_no_override() {
        // CTX-0181: explicit mode/width parse; absent table/key is `None`
        // so merge keeps the lower-precedence value (hidden/8 when no layer
        // sets it).
        let data = eval_ok(r#"return { scrollbar = { mode = "auto", width = 12 } }"#);
        let bar = data.scrollbar.unwrap();
        assert_eq!(bar.mode.as_deref(), Some("auto"));
        assert_eq!(bar.width, Some(12));
        let data = eval_ok(r#"return { scrollbar = { mode = "always" } }"#);
        let bar = data.scrollbar.unwrap();
        assert_eq!(bar.mode.as_deref(), Some("always"));
        assert_eq!(bar.width, None);
        let data = eval_ok(r#"return { terminal = { scrollback = 10000 } }"#);
        assert_eq!(data.scrollbar, None);
        let data = eval_ok(r#"return { scrollbar = {} }"#);
        let bar = data.scrollbar.unwrap();
        assert_eq!(bar.mode, None);
        assert_eq!(bar.width, None);
    }

    #[test]
    fn scrollbar_wrong_type_is_shape_error_without_value() {
        // CTX-0181: fail-closed on non-string mode / non-integer width
        // (never coerce, never echo the value).
        let mut vm = LuaVm::new("test.scrollbar-type");
        for code in [
            r#"return { scrollbar = { mode = true } }"#,
            r#"return { scrollbar = { mode = 1 } }"#,
            r#"return { scrollbar = { width = 8.5 } }"#,
            r#"return { scrollbar = { width = "8" } }"#,
            r#"return { scrollbar = "auto" }"#,
            r#"return { scrollbar = { mode = "auto", bogus = 1 } }"#,
        ] {
            match vm.eval_config(code).expect("no refuse") {
                ConfigOutcome::ShapeError { message } => {
                    assert!(message.contains("scrollbar"), "{code:?}: {message}");
                }
                other => panic!("{code:?}: expected shape error, got {other:?}"),
            }
        }
    }

    #[test]
    fn mouse_focus_follows_mouse_extracts_and_absent_means_no_override() {
        // CTX-0260: explicit opt-in parses; absent table/key is `None` so
        // merge keeps the lower-precedence value (false when no layer sets
        // it, preserving click-to-focus).
        let data = eval_ok(r#"return { mouse = { focus_follows_mouse = true } }"#);
        assert_eq!(data.mouse.unwrap().focus_follows_mouse, Some(true));
        let data = eval_ok(r#"return { mouse = { focus_follows_mouse = false } }"#);
        assert_eq!(data.mouse.unwrap().focus_follows_mouse, Some(false));
        let data = eval_ok(r#"return { terminal = { scrollback = 10000 } }"#);
        assert_eq!(data.mouse, None);
        let data = eval_ok(r#"return { mouse = {} }"#);
        assert_eq!(data.mouse.unwrap().focus_follows_mouse, None);
    }

    #[test]
    fn mouse_focus_follows_mouse_wrong_type_is_shape_error_without_value() {
        // CTX-0260: fail-closed on non-boolean values (never coerce, never
        // echo the value).
        let mut vm = LuaVm::new("test.mouse-type");
        for code in [
            r#"return { mouse = { focus_follows_mouse = 1 } }"#,
            r#"return { mouse = { focus_follows_mouse = "true" } }"#,
            r#"return { mouse = true }"#,
            r#"return { mouse = { focus_follows_mouse = true, bogus = 1 } }"#,
        ] {
            match vm.eval_config(code).expect("no refuse") {
                ConfigOutcome::ShapeError { message } => {
                    assert!(message.contains("mouse"), "{code:?}: {message}");
                }
                other => panic!("{code:?}: expected shape error, got {other:?}"),
            }
        }
    }

    #[test]
    fn integer_size_accepted_as_number() {
        let data = eval_ok(r#"return { font = { family = "Mono", size = 13 } }"#);
        assert!((data.font.unwrap().size.unwrap() - 13.0).abs() < f64::EPSILON);
    }

    #[test]
    fn font_spacing_keys_optional_and_parsed() {
        let data = eval_ok(r#"return { font = { family = "Mono", size = 12 } }"#);
        let font = data.font.unwrap();
        assert_eq!(font.line_height, None);
        assert_eq!(font.letter_spacing, None);
        let data = eval_ok(
            r#"return { font = { family = "Mono", size = 12, line_height = 1.2, letter_spacing = 1 } }"#,
        );
        let font = data.font.unwrap();
        assert!((font.line_height.unwrap() - 1.2).abs() < f32::EPSILON);
        assert!((font.letter_spacing.unwrap() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn keymaps_array_extracts() {
        let data = eval_ok(
            r#"return { keymaps = {
                { chord = "ctrl+p", action = "palette:toggle", context = "global" },
                { chord = "ctrl+q", action = "quit", context = "global" },
            } }"#,
        );
        let maps = data.keymaps.unwrap();
        assert_eq!(maps.len(), 2);
        assert_eq!(maps[0].chord, "ctrl+p");
        assert_eq!(maps[0].action, "palette:toggle");
        assert_eq!(maps[1].context, "global");
    }

    #[test]
    fn lua_computation_allowed_in_chunk() {
        // Pure computation (string/table/math) is available; the result is
        // still plain data.
        let data = eval_ok(
            r#"return { theme = ("da" .. "rk"), window = { opacity = 0.5 + 0.45, padding = 4 + 4 } }"#,
        );
        assert_eq!(data.theme.as_deref(), Some("dark"));
        let window = data.window.unwrap();
        assert!((window.opacity.unwrap() - 0.95).abs() < 1e-9);
        assert_eq!(window.padding, Some(8));
    }

    #[test]
    fn non_table_return_is_shape_error() {
        let mut vm = LuaVm::new("test.shape");
        for code in [r#"return 42"#, r#"return "dark""#, r#"return true"#] {
            match vm.eval_config(code).expect("no refuse") {
                ConfigOutcome::ShapeError { message } => {
                    assert!(message.contains("table"), "{code:?}: {message}")
                }
                other => panic!("{code:?}: expected shape error, got {other:?}"),
            }
        }
    }

    #[test]
    fn wrong_field_types_are_shape_errors_without_values() {
        let mut vm = LuaVm::new("test.types");
        match vm
            .eval_config(r#"return { theme = 42 }"#)
            .expect("no refuse")
        {
            ConfigOutcome::ShapeError { message } => {
                assert!(message.contains("theme"), "{message}");
                assert!(!message.contains("42"), "must not echo value: {message}");
            }
            other => panic!("expected shape error, got {other:?}"),
        }
        match vm
            .eval_config(r#"return { keymaps = { { chord = "a" } } }"#)
            .expect("no refuse")
        {
            ConfigOutcome::ShapeError { message } => assert!(message.contains("keymaps")),
            other => panic!("expected shape error, got {other:?}"),
        }
    }

    #[test]
    fn undeclared_keys_collected_not_executed() {
        let data = eval_ok(r#"return { theme = "dark", plugins = { "x" } }"#);
        assert_eq!(data.theme.as_deref(), Some("dark"));
        assert_eq!(data.undeclared, vec!["plugins".to_string()]);
    }

    #[test]
    fn syntax_error_is_lua_error_with_line() {
        let mut vm = LuaVm::new("test.syntax");
        match vm.eval_config("return { theme = }").expect("no refuse") {
            ConfigOutcome::LuaError { message } => {
                assert!(!message.is_empty());
            }
            other => panic!("expected lua error, got {other:?}"),
        }
    }

    #[test]
    fn runtime_error_is_lua_error() {
        let mut vm = LuaVm::new("test.runtime");
        match vm.eval_config(r#"error("boom")"#).expect("no refuse") {
            ConfigOutcome::LuaError { message } => assert!(message.contains("boom")),
            other => panic!("expected lua error, got {other:?}"),
        }
    }

    #[test]
    fn denied_globals_fail_closed() {
        // No io/os/debug/package ambient authority in the config sandbox.
        let mut vm = LuaVm::new("test.deny");
        for code in [
            r#"return { theme = io.popen("x") }"#,
            r#"return { theme = os.getenv("HOME") }"#,
            r#"return require("x")"#,
        ] {
            match vm.eval_config(code).expect("no refuse") {
                ConfigOutcome::LuaError { .. } => {}
                other => panic!("{code:?}: expected lua error, got {other:?}"),
            }
        }
    }

    #[test]
    fn infinite_loop_suspends_with_same_budgets() {
        let mut vm = LuaVm::new("test.budget");
        match vm.eval_config("while true do end").expect("no refuse") {
            ConfigOutcome::Suspended { .. } => {}
            other => panic!("expected suspension, got {other:?}"),
        }
        assert!(vm.is_suspended());
        // Fail-closed: further evaluation refused.
        assert!(vm.eval_config(r#"return { theme = "dark" }"#).is_err());
    }

    #[test]
    fn determinism_same_source_same_data() {
        let code = r#"return { theme = "dark", font = { family = "Mono", size = 12 } }"#;
        let a = eval_ok(code);
        let b = eval_ok(code);
        assert_eq!(a, b);
    }
}
