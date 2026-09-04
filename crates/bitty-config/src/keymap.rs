//! Keymap schema: chords and chrome actions (CTX-0153).
//!
//! Config-file keymaps drive all chrome keys (ghostty-style, read-only
//! reference: `recording/references/ghostty` plus the user's
//! `~/.config/ghostty/keybinds.conf`). The single-owner rule lives here as
//! data and in `bitty-app` as enforcement: a key event that matches a bound
//! chord is consumed by the chrome action and never reaches the PTY; an
//! unbound key (Tab, arrows, plain letters, ...) always goes to the shell.
//!
//! # Shape (wezterm-style `init.lua` return table)
//!
//! ```lua
//! return {
//!     keymaps = {
//!         { chord = "alt+h", action = "goto_split:left", context = "global" },
//!     },
//! }
//! ```
//!
//! - `chord`: `<mod>+...+<key>` with mods from
//!   `ctrl/control`, `alt/opt/option`, `shift`, `super/meta/cmd/win`
//!   (case-insensitive, any order) and one key: a named key (`tab`, `enter`,
//!   `escape`, `space`, `backspace`, `delete`, `insert`, `home`, `end`,
//!   `pageup`, `pagedown`, `up`, `down`, `left`, `right`, `f1`..`f35`) or a
//!   single ASCII character (`h`, `p`, `1`, ...). A single-character key
//!   requires at least one modifier so a binding can never silently steal
//!   shell typing; named keys (including `tab`) may be unmodified by explicit
//!   user choice.
//! - `action`: one of `goto_split:<left|right|up|down>`,
//!   `new_split:<left|right|up|down>`, `resize_split:<left|right|up|down>`,
//!   `close_view` (alias `close_surface`), `toggle_zoom` (alias
//!   `toggle_split_zoom`), `focus_next`, `focus_prev`, `focus:<1..=256>`.
//!   Anything else fails closed with the known-action list.
//! - `context`: only `"global"` is supported today; anything else fails
//!   closed so a future context cannot silently never-match.
//!
//! # Defaults
//!
//! [`DEFAULT_KEYMAPS`] ships the minimal documented map derived from the
//! ghostty reference (`alt+h/j/k/l` navigate, `alt+w` closes,
//! `shift+alt` creates, `shift+ctrl` resizes, `alt+m`/`alt+f` zoom,
//! `ctrl+alt+arrows` navigate, `ctrl+tab` cycles). Plain `Tab`, arrows,
//! letters, and digits are deliberately unbound so they reach the shell.
//! A user entry replaces the default with the same `context + chord`
//! identity (the existing merge rule); anything else appends.
//!
//! # Bounds and failure posture (threat T-01)
//!
//! Chord/action strings are length-bounded, parsing is total and
//! allocation-bounded, and every unknown token fails closed with
//! [`ConfigError`] (never a panic, never a silent ignore). Matching
//! ([`match_keymap`]) is a bounded linear scan over plain data — no I/O,
//! no `unsafe`, headless on Linux CI and Windows.

use crate::error::ConfigError;
use crate::types::{EffectiveConfig, KeymapEntry};

/// Maximum raw chord string length in bytes (fail-closed).
pub const MAX_CHORD_LEN: usize = 64;

/// Maximum raw action string length in bytes (fail-closed).
pub const MAX_ACTION_LEN: usize = 64;

/// Maximum focus id accepted by the `focus:<n>` action.
pub const MAX_FOCUS_ID: u64 = 256;

/// Only supported keymap context today. Unknown contexts fail closed.
pub const GLOBAL_CONTEXT: &str = "global";

/// Named key identity used by chords and by the app-side matcher.
///
/// This mirrors the terminal-relevant subset of
/// `bitty-platform` named keys without depending on that crate (this crate
/// has no workspace dependencies): the app converts its `KeyEvent` into a
/// [`KeyRef`] of plain data and matches here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyName {
    /// Tab key.
    Tab,
    /// Enter / Return key.
    Enter,
    /// Escape key.
    Escape,
    /// Space key.
    Space,
    /// Backspace key.
    Backspace,
    /// Delete key.
    Delete,
    /// Insert key.
    Insert,
    /// Home key.
    Home,
    /// End key.
    End,
    /// PageUp key.
    PageUp,
    /// PageDown key.
    PageDown,
    /// Arrow keys.
    Up,
    /// Arrow keys.
    Down,
    /// Arrow keys.
    Left,
    /// Arrow keys.
    Right,
    /// Function key `f1`..=`f35`.
    F(u8),
    /// Single ASCII character key, stored lowercase (`Char('h')`).
    Char(char),
}

impl KeyName {
    /// Canonical key spelling used by [`Chord::canonical`].
    #[must_use]
    pub fn canonical(&self) -> String {
        match self {
            Self::Tab => "tab".to_string(),
            Self::Enter => "enter".to_string(),
            Self::Escape => "escape".to_string(),
            Self::Space => "space".to_string(),
            Self::Backspace => "backspace".to_string(),
            Self::Delete => "delete".to_string(),
            Self::Insert => "insert".to_string(),
            Self::Home => "home".to_string(),
            Self::End => "end".to_string(),
            Self::PageUp => "pageup".to_string(),
            Self::PageDown => "pagedown".to_string(),
            Self::Up => "up".to_string(),
            Self::Down => "down".to_string(),
            Self::Left => "left".to_string(),
            Self::Right => "right".to_string(),
            Self::F(n) => format!("f{n}"),
            Self::Char(c) => c.to_string(),
        }
    }

    /// True for bare-modifier named keys, which can never be a chord key.
    #[must_use]
    pub fn is_modifier_name(token: &str) -> bool {
        matches!(
            token,
            "shift"
                | "ctrl"
                | "control"
                | "alt"
                | "opt"
                | "option"
                | "super"
                | "meta"
                | "cmd"
                | "command"
                | "win"
                | "windows"
                | "hyper"
                | "altgraph"
                | "alt_graph"
        )
    }
}

/// A parsed, normalized key chord: held modifiers plus one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    /// Control held.
    pub ctrl: bool,
    /// Alt (Option) held.
    pub alt: bool,
    /// Shift held.
    pub shift: bool,
    /// Super (Meta/Cmd/Win) held.
    pub super_held: bool,
    /// The key itself.
    pub key: KeyName,
}

impl Chord {
    /// Parse a raw chord string (`"alt+h"`, `"Ctrl+Alt+Left"`, ...).
    ///
    /// Fail-closed [`ConfigError`] on empty input, overlong input, unknown
    /// tokens, duplicate modifiers, missing/duplicate keys, bare-modifier
    /// keys, multi-character keys, or unmodified single-character keys
    /// (which would steal shell typing).
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::validation(
                "keymaps[].chord",
                "must not be empty",
            ));
        }
        if trimmed.len() > MAX_CHORD_LEN {
            return Err(ConfigError::validation(
                "keymaps[].chord",
                format!("must be <= {MAX_CHORD_LEN} bytes"),
            ));
        }
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut super_held = false;
        let mut key: Option<KeyName> = None;
        for part in trimmed.split('+') {
            let token = part.trim().to_ascii_lowercase();
            if token.is_empty() {
                return Err(ConfigError::validation(
                    "keymaps[].chord",
                    format!(
                        "chord '{trimmed}' has an empty segment (did you mean 'keymaps[].chord'? check '+' placement)"
                    ),
                ));
            }
            match token.as_str() {
                "ctrl" | "control" => {
                    if ctrl {
                        return Err(ConfigError::validation(
                            "keymaps[].chord",
                            format!("chord '{trimmed}' repeats modifier 'ctrl'"),
                        ));
                    }
                    ctrl = true;
                }
                "alt" | "opt" | "option" => {
                    if alt {
                        return Err(ConfigError::validation(
                            "keymaps[].chord",
                            format!("chord '{trimmed}' repeats modifier 'alt'"),
                        ));
                    }
                    alt = true;
                }
                "shift" => {
                    if shift {
                        return Err(ConfigError::validation(
                            "keymaps[].chord",
                            format!("chord '{trimmed}' repeats modifier 'shift'"),
                        ));
                    }
                    shift = true;
                }
                "super" | "meta" | "cmd" | "command" | "win" | "windows" => {
                    if super_held {
                        return Err(ConfigError::validation(
                            "keymaps[].chord",
                            format!("chord '{trimmed}' repeats modifier 'super'"),
                        ));
                    }
                    super_held = true;
                }
                _ => {
                    if key.is_some() {
                        return Err(ConfigError::validation(
                            "keymaps[].chord",
                            format!(
                                "chord '{trimmed}' has more than one key; use '<mod>+...+<key>'"
                            ),
                        ));
                    }
                    key = Some(parse_key_token(&token, trimmed)?);
                }
            }
        }
        let key = match key {
            Some(k) => k,
            None => {
                return Err(ConfigError::validation(
                    "keymaps[].chord",
                    format!("chord '{trimmed}' names only modifiers; add one key (e.g. 'alt+h')"),
                ));
            }
        };
        if matches!(key, KeyName::Char(_)) && !(ctrl || alt || shift || super_held) {
            return Err(ConfigError::validation(
                "keymaps[].chord",
                format!(
                    "single-character chord '{trimmed}' must include a modifier (e.g. 'ctrl+{trimmed}'); unmodified keys go to the shell"
                ),
            ));
        }
        Ok(Self {
            ctrl,
            alt,
            shift,
            super_held,
            key,
        })
    }

    /// Canonical spelling: modifiers in `ctrl+alt+shift+super` order, then
    /// the canonical key. Used for merge identity and matching.
    #[must_use]
    pub fn canonical(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("ctrl+");
        }
        if self.alt {
            out.push_str("alt+");
        }
        if self.shift {
            out.push_str("shift+");
        }
        if self.super_held {
            out.push_str("super+");
        }
        out.push_str(&self.key.canonical());
        out
    }
}

/// Parse one key token (already lowercased, non-empty, non-modifier).
fn parse_key_token(token: &str, raw_chord: &str) -> Result<KeyName, ConfigError> {
    match token {
        "tab" => Ok(KeyName::Tab),
        "enter" | "return" => Ok(KeyName::Enter),
        "escape" | "esc" => Ok(KeyName::Escape),
        "space" | "spacebar" => Ok(KeyName::Space),
        "backspace" | "bs" => Ok(KeyName::Backspace),
        "delete" | "del" => Ok(KeyName::Delete),
        "insert" | "ins" => Ok(KeyName::Insert),
        "home" => Ok(KeyName::Home),
        "end" => Ok(KeyName::End),
        "pageup" | "pgup" => Ok(KeyName::PageUp),
        "pagedown" | "pgdn" => Ok(KeyName::PageDown),
        "up" | "arrowup" | "arrow_up" | "arrow-up" => Ok(KeyName::Up),
        "down" | "arrowdown" | "arrow_down" | "arrow-down" => Ok(KeyName::Down),
        "left" | "arrowleft" | "arrow_left" | "arrow-left" => Ok(KeyName::Left),
        "right" | "arrowright" | "arrow_right" | "arrow-right" => Ok(KeyName::Right),
        _ => {
            if KeyName::is_modifier_name(token) {
                return Err(ConfigError::validation(
                    "keymaps[].chord",
                    format!("chord '{raw_chord}' names only modifiers; add one key (e.g. 'alt+h')"),
                ));
            }
            if token.len() > 1 {
                if let Some(n) = token.strip_prefix('f') {
                    if !n.is_empty() {
                        if let Ok(num) = n.parse::<u8>() {
                            if (1..=35).contains(&num) {
                                return Ok(KeyName::F(num));
                            }
                        }
                    }
                    return Err(ConfigError::validation(
                        "keymaps[].chord",
                        format!("unknown key '{token}' in chord '{raw_chord}'; {KNOWN_KEYS_HINT}"),
                    ));
                }
            }
            let mut chars = token.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    if c.is_ascii_graphic() && c != '+' {
                        Ok(KeyName::Char(c.to_ascii_lowercase()))
                    } else {
                        Err(ConfigError::validation(
                            "keymaps[].chord",
                            format!(
                                "unsupported key '{token}' in chord '{raw_chord}'; {KNOWN_KEYS_HINT}"
                            ),
                        ))
                    }
                }
                _ => Err(ConfigError::validation(
                    "keymaps[].chord",
                    format!("unknown key '{token}' in chord '{raw_chord}'; {KNOWN_KEYS_HINT}"),
                )),
            }
        }
    }
}

/// Hint naming the accepted key vocabulary (kept out of [`parse_key_token`]
/// hot error paths as a shared constant).
const KNOWN_KEYS_HINT: &str = "expected a named key (tab, enter, escape, space, backspace, delete, insert, home, end, pageup, pagedown, up, down, left, right, f1..f35) or one ASCII character";

/// Split direction for pane actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SplitDir {
    /// Left.
    Left,
    /// Right.
    Right,
    /// Up.
    Up,
    /// Down.
    Down,
}

impl SplitDir {
    /// Parse a direction suffix (`left`/`right`/`up`/`down`).
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            other => Err(ConfigError::validation(
                "keymaps[].action",
                format!("unknown split direction '{other}'; expected one of left, right, up, down"),
            )),
        }
    }

    /// Canonical direction spelling.
    #[must_use]
    pub fn canonical(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// Chrome action invoked by a bound chord.
///
/// Every variant maps onto existing `Runtime`/`LayoutNode` APIs (focus moves,
/// leaf split/close, ratio nudge, zoom swap) — no new tiling primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromeAction {
    /// Move focus spatially (`goto_split:left`, ...).
    GotoSplit(SplitDir),
    /// Split the focused pane (`new_split:right`, ...).
    NewSplit(SplitDir),
    /// Nudge the enclosing split ratio (`resize_split:left`, ...).
    ResizeSplit(SplitDir),
    /// Close the focused pane (`close_view`, alias `close_surface`).
    CloseView,
    /// Toggle single-pane zoom (`toggle_zoom`, alias `toggle_split_zoom`).
    ToggleZoom,
    /// Focus next pane in depth-first order.
    FocusNext,
    /// Focus previous pane in depth-first order.
    FocusPrev,
    /// Focus numeric view id (`focus:3`, `1..=256`).
    FocusId(u64),
}

impl ChromeAction {
    /// Parse a raw action string. Fail-closed with the known-action list.
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::validation(
                "keymaps[].action",
                "must not be empty",
            ));
        }
        if trimmed.len() > MAX_ACTION_LEN {
            return Err(ConfigError::validation(
                "keymaps[].action",
                format!("must be <= {MAX_ACTION_LEN} bytes"),
            ));
        }
        let lowered = trimmed.to_ascii_lowercase();
        let (head, arg) = match lowered.split_once(':') {
            Some((h, a)) => (h.trim(), Some(a.trim())),
            None => (lowered.as_str(), None),
        };
        match head {
            "goto_split" => {
                let dir = require_dir_arg(arg, trimmed)?;
                Ok(Self::GotoSplit(dir))
            }
            "new_split" => {
                let dir = require_dir_arg(arg, trimmed)?;
                Ok(Self::NewSplit(dir))
            }
            "resize_split" => {
                let dir = require_dir_arg(arg, trimmed)?;
                Ok(Self::ResizeSplit(dir))
            }
            "close_view" | "close_surface" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::CloseView)
            }
            "toggle_zoom" | "toggle_split_zoom" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::ToggleZoom)
            }
            "focus_next" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::FocusNext)
            }
            "focus_prev" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::FocusPrev)
            }
            "focus" => {
                let n = require_focus_id(arg, trimmed)?;
                Ok(Self::FocusId(n))
            }
            _ => Err(ConfigError::validation(
                "keymaps[].action",
                format!("unknown action '{trimmed}'; {KNOWN_ACTIONS_HINT}"),
            )),
        }
    }

    /// Canonical action spelling (`goto_split:left`, `close_view`, ...).
    #[must_use]
    pub fn canonical(self) -> String {
        match self {
            Self::GotoSplit(d) => format!("goto_split:{}", d.canonical()),
            Self::NewSplit(d) => format!("new_split:{}", d.canonical()),
            Self::ResizeSplit(d) => format!("resize_split:{}", d.canonical()),
            Self::CloseView => "close_view".to_string(),
            Self::ToggleZoom => "toggle_zoom".to_string(),
            Self::FocusNext => "focus_next".to_string(),
            Self::FocusPrev => "focus_prev".to_string(),
            Self::FocusId(n) => format!("focus:{n}"),
        }
    }
}

/// Hint listing the accepted action vocabulary.
const KNOWN_ACTIONS_HINT: &str = "expected one of goto_split:<left|right|up|down>, new_split:<left|right|up|down>, resize_split:<left|right|up|down>, close_view, toggle_zoom, focus_next, focus_prev, focus:<1..=256>";

/// Require a `<head>:<dir>` argument.
fn require_dir_arg(arg: Option<&str>, raw: &str) -> Result<SplitDir, ConfigError> {
    match arg {
        Some(d) if !d.is_empty() => SplitDir::parse(d),
        _ => Err(ConfigError::validation(
            "keymaps[].action",
            format!("action '{raw}' needs a direction (e.g. '{raw}:left')"),
        )),
    }
}

/// Reject `head:arg` for actions that take none.
fn reject_arg(arg: Option<&str>, raw: &str) -> Result<(), ConfigError> {
    match arg {
        Some(a) if !a.is_empty() => Err(ConfigError::validation(
            "keymaps[].action",
            format!("action '{raw}' takes no argument"),
        )),
        _ => Ok(()),
    }
}

/// Require a `focus:<n>` id argument.
fn require_focus_id(arg: Option<&str>, raw: &str) -> Result<u64, ConfigError> {
    match arg {
        Some(n) if !n.is_empty() => match n.parse::<u64>() {
            Ok(id) if (1..=MAX_FOCUS_ID).contains(&id) => Ok(id),
            _ => Err(ConfigError::validation(
                "keymaps[].action",
                format!("action '{raw}' needs a view id 1..={MAX_FOCUS_ID} (e.g. 'focus:2')"),
            )),
        },
        _ => Err(ConfigError::validation(
            "keymaps[].action",
            format!("action '{raw}' needs a view id (e.g. 'focus:2')"),
        )),
    }
}

/// Plain-data key reference for matching: the pressed key plus the held
/// modifiers snapshot. The app builds this from its `KeyEvent` and its own
/// modifier mirror (key events carry no modifier field); matching here stays
/// pure so it is headless-testable without a display server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyRef {
    /// Pressed key (single chars already lowercased).
    pub key: KeyName,
    /// Control held.
    pub ctrl: bool,
    /// Alt held.
    pub alt: bool,
    /// Shift held.
    pub shift: bool,
    /// Super held.
    pub super_held: bool,
}

impl KeyRef {
    /// True when this reference exactly equals the bound chord (single
    /// owner: no fuzzy or prefix matching).
    #[must_use]
    pub fn matches(&self, chord: &Chord) -> bool {
        self.key == chord.key
            && self.ctrl == chord.ctrl
            && self.alt == chord.alt
            && self.shift == chord.shift
            && self.super_held == chord.super_held
    }
}

/// A validated, resolved key binding: normalized chord plus action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKeymap {
    /// Normalized chord.
    pub chord: Chord,
    /// Chrome action to invoke.
    pub action: ChromeAction,
    /// Context (always `"global"` today).
    pub context: String,
    /// True when this entry came from the shipped defaults rather than the
    /// user config (for `config check` attribution).
    pub from_default: bool,
}

impl ResolvedKeymap {
    /// Merge identity: `context::canonical-chord`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}::{}", self.context, self.chord.canonical())
    }
}

/// Minimal documented default map, derived from the ghostty reference
/// (`alt+h/j/k/l` navigate, `shift+alt+h/j/k/l` create, `shift+ctrl+h/j/k/l`
/// resize, `alt+w` closes, `alt+m`/`alt+f` zoom, `ctrl+alt+arrows` navigate,
/// `ctrl+tab` cycles). Plain Tab, arrows, letters, and digits stay unbound
/// so they always reach the shell.
pub const DEFAULT_KEYMAPS: &[(&str, &str)] = &[
    ("alt+h", "goto_split:left"),
    ("alt+j", "goto_split:down"),
    ("alt+k", "goto_split:up"),
    ("alt+l", "goto_split:right"),
    ("ctrl+alt+left", "goto_split:left"),
    ("ctrl+alt+right", "goto_split:right"),
    ("ctrl+alt+up", "goto_split:up"),
    ("ctrl+alt+down", "goto_split:down"),
    ("ctrl+tab", "focus_next"),
    ("ctrl+shift+tab", "focus_prev"),
    ("shift+alt+h", "new_split:left"),
    ("shift+alt+j", "new_split:down"),
    ("shift+alt+k", "new_split:up"),
    ("shift+alt+l", "new_split:right"),
    ("shift+ctrl+h", "resize_split:left"),
    ("shift+ctrl+j", "resize_split:down"),
    ("shift+ctrl+k", "resize_split:up"),
    ("shift+ctrl+l", "resize_split:right"),
    ("alt+w", "close_view"),
    ("alt+m", "toggle_zoom"),
    ("alt+f", "toggle_zoom"),
];

/// Build the shipped defaults. Fail-closed only on an internal default typo
/// (covered by `defaults_parse`; user input never reaches this path).
pub fn default_keymaps() -> Result<Vec<ResolvedKeymap>, ConfigError> {
    let mut out = Vec::with_capacity(DEFAULT_KEYMAPS.len());
    for (chord_raw, action_raw) in DEFAULT_KEYMAPS {
        let chord = Chord::parse(chord_raw).map_err(|e| ConfigError::InvalidInput {
            message: format!("internal default keymap invalid: {e}"),
        })?;
        let action = ChromeAction::parse(action_raw).map_err(|e| ConfigError::InvalidInput {
            message: format!("internal default keymap invalid: {e}"),
        })?;
        out.push(ResolvedKeymap {
            chord,
            action,
            context: GLOBAL_CONTEXT.to_string(),
            from_default: true,
        });
    }
    Ok(out)
}

/// Validate one raw entry's context (only `"global"` today).
pub fn validate_context(raw: &str) -> Result<String, ConfigError> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(ConfigError::validation(
            "keymaps[].context",
            "must not be empty",
        ));
    }
    if normalized != GLOBAL_CONTEXT {
        return Err(ConfigError::validation(
            "keymaps[].context",
            format!("unknown context '{raw}'; only 'global' is supported"),
        ));
    }
    Ok(GLOBAL_CONTEXT.to_string())
}

/// Resolve the effective keymap table: shipped defaults overridden by user
/// entries with the same `context + chord` identity (the existing merge
/// rule), then sorted deterministically by identity.
///
/// Fail-closed on unknown contexts, chords, or actions, and on duplicate
/// normalized chords within the user config.
pub fn resolve_keymaps(effective: &EffectiveConfig) -> Result<Vec<ResolvedKeymap>, ConfigError> {
    let mut table = default_keymaps()?;
    let mut seen_user: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in &effective.keymaps {
        let context = validate_context(&entry.context)?;
        let chord = Chord::parse(&entry.chord)?;
        let action = ChromeAction::parse(&entry.action)?;
        let resolved = ResolvedKeymap {
            chord,
            action,
            context,
            from_default: false,
        };
        let id = resolved.id();
        if !seen_user.insert(id.clone()) {
            return Err(ConfigError::validation(
                "keymaps",
                format!("duplicate keymap id '{id}'"),
            ));
        }
        table.retain(|k| k.id() != id);
        table.push(resolved);
    }
    table.sort_by_key(|a| a.id());
    Ok(table)
}

/// Match a pressed key against the resolved table: exact chord equality only.
/// Returns the bound action (the single owner) or `None` for shell input.
#[must_use]
pub fn match_keymap(maps: &[ResolvedKeymap], key: KeyRef) -> Option<ChromeAction> {
    for m in maps {
        if key.matches(&m.chord) {
            return Some(m.action);
        }
    }
    None
}

/// Semantic validation for [`KeymapEntry`]: context, chord, and action must
/// all parse. Called by [`KeymapEntry::validate`](crate::types::KeymapEntry::validate)
/// so every pipeline stage (plan validation, merge, `config check`) fails
/// closed on unknown actions or keys with a clear error.
pub fn validate_entry(entry: &KeymapEntry) -> Result<(), ConfigError> {
    validate_context(&entry.context)?;
    Chord::parse(&entry.chord)?;
    ChromeAction::parse(&entry.action)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_ref(key: KeyName, ctrl: bool, alt: bool, shift: bool) -> KeyRef {
        KeyRef {
            key,
            ctrl,
            alt,
            shift,
            super_held: false,
        }
    }

    #[test]
    fn chord_parse_normalizes_order_and_case() {
        let c = Chord::parse("Shift+Ctrl+H").expect("parses");
        assert_eq!(c.canonical(), "ctrl+shift+h");
        let d = Chord::parse("ctrl+shift+h").expect("parses");
        assert_eq!(c, d);
    }

    #[test]
    fn chord_parse_named_keys() {
        assert_eq!(
            Chord::parse("ctrl+tab").expect("tab").canonical(),
            "ctrl+tab"
        );
        assert_eq!(
            Chord::parse("ctrl+alt+Left").expect("arrow").canonical(),
            "ctrl+alt+left"
        );
        assert_eq!(Chord::parse("alt+F5").expect("fn").canonical(), "alt+f5");
        assert_eq!(
            Chord::parse("escape").expect("bare named ok").canonical(),
            "escape"
        );
    }

    #[test]
    fn chord_rejects_unmodified_single_char() {
        // Typing safety: bare letters/digits must reach the shell.
        for raw in ["n", "p", "1", "h"] {
            assert!(Chord::parse(raw).is_err(), "must reject {raw}");
        }
    }

    #[test]
    fn chord_rejects_garbage_fail_closed() {
        for raw in [
            "",
            "   ",
            "ctrl",
            "ctrl+shift",
            "ctrl++h",
            "ctrl+h+p",
            "ctrl+hyper+h",
            "alt+f99",
            "ctrl+ ",
            "super+",
        ] {
            assert!(Chord::parse(raw).is_err(), "must reject {raw:?}");
        }
    }

    #[test]
    fn action_parse_all_known() {
        assert_eq!(
            ChromeAction::parse("goto_split:left").expect("goto"),
            ChromeAction::GotoSplit(SplitDir::Left)
        );
        assert_eq!(
            ChromeAction::parse("new_split:down").expect("new"),
            ChromeAction::NewSplit(SplitDir::Down)
        );
        assert_eq!(
            ChromeAction::parse("resize_split:up").expect("resize"),
            ChromeAction::ResizeSplit(SplitDir::Up)
        );
        assert_eq!(
            ChromeAction::parse("close_view").expect("close"),
            ChromeAction::CloseView
        );
        assert_eq!(
            ChromeAction::parse("close_surface").expect("close alias"),
            ChromeAction::CloseView
        );
        assert_eq!(
            ChromeAction::parse("toggle_split_zoom").expect("zoom alias"),
            ChromeAction::ToggleZoom
        );
        assert_eq!(
            ChromeAction::parse("focus_next").expect("next"),
            ChromeAction::FocusNext
        );
        assert_eq!(
            ChromeAction::parse("focus:3").expect("id"),
            ChromeAction::FocusId(3)
        );
    }

    #[test]
    fn action_rejects_unknown_fail_closed() {
        for raw in [
            "",
            "palette:toggle",
            "goto_split",
            "goto_split:sideways",
            "close_view:1",
            "focus:0",
            "focus:999",
            "focus:abc",
            "goto_split:left:extra",
        ] {
            let err = ChromeAction::parse(raw).unwrap_err();
            assert!(
                err.to_string().contains("keymaps[].action"),
                "must name field for {raw:?}: {err}"
            );
        }
        // Unknown head lists the known actions.
        let err = ChromeAction::parse("explode:now").unwrap_err();
        assert!(err.to_string().contains("goto_split"));
    }

    #[test]
    fn context_rejects_unknown() {
        assert!(validate_context("global").is_ok());
        assert!(validate_context("  GLOBAL ").is_ok());
        assert!(validate_context("pane").is_err());
        assert!(validate_context("").is_err());
    }

    #[test]
    fn defaults_parse_and_leave_shell_keys_unbound() {
        let maps = default_keymaps().expect("defaults valid");
        assert!(!maps.is_empty());
        // Tab, plain arrows, plain letters/digits reach the shell.
        let shell_keys = [
            key_ref(KeyName::Tab, false, false, false),
            key_ref(KeyName::Up, false, false, false),
            key_ref(KeyName::Char('n'), false, false, false),
            key_ref(KeyName::Char('p'), false, false, false),
            key_ref(KeyName::Char('1'), false, false, false),
            // Ctrl+P is shell input unless the user binds it (CTX-0154
            // single-owner: 0x10 goes to the PTY, focus must not move).
            key_ref(KeyName::Char('p'), true, false, false),
        ];
        for k in shell_keys {
            assert_eq!(match_keymap(&maps, k), None, "shell key {k:?}");
        }
        // Bound chords resolve.
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('h'), false, true, false)),
            Some(ChromeAction::GotoSplit(SplitDir::Left))
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Tab, true, false, false)),
            Some(ChromeAction::FocusNext)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('w'), false, true, false)),
            Some(ChromeAction::CloseView)
        );
    }

    #[test]
    fn resolve_user_override_replaces_default_by_chord_identity() {
        let effective = EffectiveConfig {
            keymaps: vec![KeymapEntry {
                chord: "alt+h".into(),
                action: "focus_next".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        let maps = resolve_keymaps(&effective).expect("resolves");
        let found: Vec<_> = maps
            .iter()
            .filter(|m| m.chord.canonical() == "alt+h")
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].action, ChromeAction::FocusNext);
        assert!(!found[0].from_default);
    }

    #[test]
    fn resolve_rejects_unknown_action_and_duplicate_chords() {
        let bad = EffectiveConfig {
            keymaps: vec![KeymapEntry {
                chord: "alt+h".into(),
                action: "explode:now".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        assert!(resolve_keymaps(&bad).is_err());
        let dup = EffectiveConfig {
            keymaps: vec![
                KeymapEntry {
                    chord: "alt+h".into(),
                    action: "focus_next".into(),
                    context: "global".into(),
                },
                KeymapEntry {
                    chord: "ALT+H".into(),
                    action: "focus_prev".into(),
                    context: "global".into(),
                },
            ],
            ..Default::default()
        };
        assert!(resolve_keymaps(&dup).is_err());
    }

    #[test]
    fn entry_validation_is_semantic() {
        KeymapEntry {
            chord: "alt+h".into(),
            action: "goto_split:left".into(),
            context: "global".into(),
        }
        .validate()
        .expect("valid entry");
        KeymapEntry {
            chord: "alt+h".into(),
            action: "nope".into(),
            context: "global".into(),
        }
        .validate()
        .unwrap_err();
        KeymapEntry {
            chord: "n".into(),
            action: "focus_next".into(),
            context: "global".into(),
        }
        .validate()
        .unwrap_err();
        KeymapEntry {
            chord: "alt+h".into(),
            action: "focus_next".into(),
            context: "pane".into(),
        }
        .validate()
        .unwrap_err();
    }
}
