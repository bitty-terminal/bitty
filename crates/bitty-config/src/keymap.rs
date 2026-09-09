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
//!     mod_key = "alt", -- leader/mod for the shipped chrome map: "alt" (default) or "super"
//!     keymaps = {
//!         { chord = "alt+h", action = "goto_split:left", context = "global" },
//!     },
//! }
//! ```
//!
//! - `mod_key`: which modifier the shipped defaults are expressed against
//!   (CTX-0236). `"alt"` (default; aliases `opt`/`option`) keeps the
//!   Alt-as-Mod map byte-identical; `"super"` (aliases `meta`/`cmd`/
//!   `command`/`win`/`windows`) rebinds every `alt`-bearing default to Super
//!   (`alt+h` becomes `super+h`, `shift+alt+h` becomes `shift+super+h`,
//!   `ctrl+alt+left` becomes `ctrl+super+left`). Anything else — including
//!   `ctrl`/`shift`, which would silently steal shell typing and shadow the
//!   mod-independent fixed chords — fails closed. Explicit `keymaps` entries
//!   keep their exact spelling and overlay by the existing `context + chord`
//!   identity, so they survive a mod flip untouched.
//!
//! - `chord`: `<mod>+...+<key>` with mods from
//!   `ctrl/control`, `alt/opt/option`, `shift`, `super/meta/cmd/win`
//!   (case-insensitive, any order) and one key: a named key (`tab`, `enter`,
//!   `escape`, `space`, `backspace`, `delete` (`del`), `insert` (`ins`),
//!   `home` (`hm`), `end`, `pageup` (`pgup`/`pu`), `pagedown` (`pgdn`/`pd`),
//!   `up`, `down`, `left`, `right`, `f1`..`f35`) or a
//!   single ASCII character (`h`, `p`, `1`, ...). Short cap-label aliases
//!   (CTX-0264) canonicalize to the long names. A single-character key
//!   requires at least one modifier so a binding can never silently steal
//!   shell typing; named keys (including `tab`) may be unmodified by explicit
//!   user choice.
//! - `action`: one of `goto_split:<left|right|up|down>`,
//!   `new_split:<left|right|up|down>`, `resize_split:<left|right|up|down>`,
//!   `close_view` (alias `close_surface`), `toggle_zoom` (alias
//!   `toggle_split_zoom`), `focus_next`, `focus_prev`, `focus:<1..=256>`,
//!   `copy_to_clipboard`, `paste_from_clipboard`,
//!   `scroll_page_up`, `scroll_page_down`, `increase_font_size` (aliases
//!   `zoom_in`, `font_zoom_in`), `decrease_font_size` (aliases `zoom_out`,
//!   `font_zoom_out`), `reset_font_size` (aliases `zoom_reset`,
//!   `font_zoom_reset`; CTX-0263 per-window font zoom), `toggle_help`
//!   (alias `show_help`; CTX-0265 help popup, defaults `alt+`` plus the
//!   `alt+?` shifted-symbol spellings), `open_composer` (Command Composer
//!   manual open, CTX-0227: suggested chord `alt+e`; never bound by default
//!   so Normal Mode stays byte-identical until the user opts in),
//!   `workspace_new`, `workspace_close`, `workspace_prev`, `workspace_next`,
//!   `workspace_last`, `workspace_focus:<1..=16>`, `workspace_move:<1..=16>`
//!   (CTX-0257 workspace ops entry per DEC-0034 plus CTX-0259 move:
//!   `alt+n` new, `alt+w` close with kill-confirm,
//!   `alt+-`/`alt+=` prev/next (`=` is the unshifted DEC `+`), `alt+tab`
//!   last-used, `alt+1..=9` jump to workspace N,
//!   `shift+alt+1..=9` move focused window to workspace N).
//!   Anything else fails closed with the known-action list.
//! - `context`: only `"global"` is supported today; anything else fails
//!   closed so a future context cannot silently never-match.
//!
//! # Defaults
//!
//! [`DEFAULT_KEYMAPS`] ships the Alt-as-Mod map derived from the ghostty
//! reference (`alt+h/j/k/l` navigate plus CTX-0262 `alt+arrows` aliases,
//! `alt+u`/`alt+i` page up/down
//! less-like, `alt+z`/`alt+m`/`alt+f` zoom, `shift+alt` creates plus
//! CTX-0262 `shift+alt+arrows` aliases,
//! `shift+ctrl` resizes plus CTX-0262 `shift+ctrl+arrows` aliases plus the CTX-0258 `ctrl+shift+alt+h/j/k/l`
//! Mod-aware resize variant plus CTX-0262 `ctrl+shift+alt+arrows` aliases (pure `shift+alt` stays creation: it cannot
//! also resize under the single-owner rule), `ctrl+alt+arrows` navigate,
//! `ctrl+tab` cycles, `ctrl+shift+c/v` copy/paste) plus the DEC-0034
//! workspace entry (CTX-0257): `alt+n` new workspace, `alt+1..=9` jump to
//! workspace N, `alt+-`/`alt+=` prev/next, `alt+tab` last-used, `alt+w`
//! close with kill-confirm. `alt+w` and `alt+1..=9` previously drove pane
//! ops (`close_view`, `focus:<n>`); those actions stay parseable and
//! user-bindable but are no longer bound by default — workspace numbers won
//! the Alt slot per the owner spec, panes navigate spatially (`goto_split`,
//! `focus_next`/`focus_prev`). The CTX-0265 help popup (009 §which-key:
//! floating overlay listing every bound shortcut, generated from the live
//! registry) toggles on `alt+`` plus the `alt+?` shifted-symbol spellings
//! (`alt+?`/`alt+shift+?`/`alt+shift+/`: shifted-symbol reporting varies by
//! platform, CTX-0263 precedent). Plain `Tab`, bare arrows, letters, and digits
//! are deliberately unbound so they reach the shell.
//!
//! The table is the canonical Alt spelling (kept byte-identical for the
//! CTX-0178 wizard pin); [`default_keymaps_with_mod`] renders it against one
//! [`ModKey`] so flipping `mod_key` rebinds the chrome map without touching
//! this source. A user entry replaces
//! the default with the same `context + chord` identity (the existing merge
//! rule); anything else appends.
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

/// Maximum workspace index accepted by the `workspace_focus:<n>` action.
///
/// Parse-time bound only: the runtime enforces its live capacity
/// (`<= 16`, mirroring `bitty_runtime::registry::MAX_WORKSPACES_PER_WINDOW`)
/// fail-closed at apply, exactly like `MAX_FOCUS_ID` vs leaf counts.
pub const MAX_WORKSPACE_INDEX: u64 = 16;

/// Only supported keymap context today. Unknown contexts fail closed.
pub const GLOBAL_CONTEXT: &str = "global";

/// Maximum raw `mod_key` string length in bytes (fail-closed, before parsing).
pub const MAX_MOD_KEY_LEN: usize = 32;

/// Leader/Mod key the shipped chrome map is expressed against (CTX-0236).
///
/// [`DEFAULT_KEYMAPS`] is the canonical Alt spelling; [`resolve_keymaps`]
/// renders it through [`default_keymaps_with_mod`], so flipping one
/// `mod_key` setting rebinds every `alt`-bearing default (including the
/// CTX-0258 `ctrl+shift+alt+h/j/k/l` resize variant, which becomes
/// `ctrl+shift+super+h/j/k/l`, and the CTX-0262 `alt+arrows`,
/// `shift+alt+arrows`, `ctrl+shift+alt+arrows` aliases) while chords without `alt` (`ctrl+tab`
/// cycles, `shift+ctrl` legacy resizes including CTX-0262 `shift+ctrl+arrows`, `ctrl+shift` copy/paste) pass
/// through as mod-independent fixed chords. Explicit user
/// entries keep their exact spelling and overlay by the existing
/// `context + chord` identity, so a mod flip never rewrites user intent.
///
/// Only `alt` (default) and `super` are accepted. `ctrl`/`shift` fail closed
/// at parse time: they would silently steal shell typing (`ctrl+w`,
/// `ctrl+h`, ...) and collide with the fixed chords, shadowing defaults
/// instead of rebinding the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModKey {
    /// Alt / Option (default): the shipped Alt-as-Mod map, byte-identical.
    #[default]
    Alt,
    /// Super / Meta / Cmd / Win: the full chrome map on Super.
    Super,
}

impl ModKey {
    /// Canonical setting spelling (`"alt"` / `"super"`), used by
    /// `config check` attribution and reload diffs.
    #[must_use]
    pub fn canonical(self) -> &'static str {
        match self {
            Self::Alt => "alt",
            Self::Super => "super",
        }
    }

    /// Parse a raw `mod_key` value (trimmed, case-insensitive, chord-mod
    /// aliases accepted: `opt`/`option` for Alt, `meta`/`cmd`/`command`/
    /// `win`/`windows` for Super).
    ///
    /// Fail-closed [`ConfigError`] on empty, overlong, or unknown values —
    /// including `ctrl`/`shift`, which would shadow shell input and the
    /// mod-independent fixed chords (never a panic, never a silent ignore).
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::validation(
                "mod_key",
                "must not be empty; expected one of 'alt', 'super'",
            ));
        }
        if trimmed.len() > MAX_MOD_KEY_LEN {
            return Err(ConfigError::validation(
                "mod_key",
                format!("must be <= {MAX_MOD_KEY_LEN} bytes"),
            ));
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "alt" | "opt" | "option" => Ok(Self::Alt),
            "super" | "meta" | "cmd" | "command" | "win" | "windows" => Ok(Self::Super),
            _ => Err(ConfigError::validation(
                "mod_key",
                format!(
                    "unknown mod '{trimmed}'; expected one of 'alt', 'super' ('ctrl'/'shift' are rejected: they would steal shell typing and shadow the fixed chords)"
                ),
            )),
        }
    }

    /// Rebind one parsed default chord against this mod: move the `alt` slot
    /// to the mod. Chords without `alt` are returned unchanged
    /// (mod-independent fixed chords).
    #[must_use]
    pub fn apply_to(self, chord: Chord) -> Chord {
        if !chord.alt {
            return chord;
        }
        match self {
            Self::Alt => chord,
            Self::Super => Chord {
                alt: false,
                super_held: true,
                ..chord
            },
        }
    }
}

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
        // CTX-0264: short cap-label aliases for the six editing/navigation
        // keys so `alt+hm`/`alt+pu`/`alt+pd` (and any-case variants) bind
        // exactly like the long spellings. Canonical forms stay the long
        // names (`home`/`pageup`/`pagedown`) so merge identity is stable.
        "home" | "hm" => Ok(KeyName::Home),
        "end" => Ok(KeyName::End),
        "pageup" | "pgup" | "pu" => Ok(KeyName::PageUp),
        "pagedown" | "pgdn" | "pd" => Ok(KeyName::PageDown),
        "up" | "arrowup" | "arrow_up" | "arrow-up" => Ok(KeyName::Up),
        "down" | "arrowdown" | "arrow_down" | "arrow-down" => Ok(KeyName::Down),
        "left" | "arrowleft" | "arrow_left" | "arrow-left" => Ok(KeyName::Left),
        "right" | "arrowright" | "arrow_right" | "arrow-right" => Ok(KeyName::Right),
        // CTX-0263: word spellings for keys the `+`-split chord syntax
        // cannot spell literally. `ctrl++` splits into empty segments and
        // fails, and `+` is Shift+= on US layouts (the compositor reports
        // either `=`+shift or `+`+shift depending on platform), so both
        // `equal`/`plus` spellings must resolve. Canonical forms stay the
        // single characters (`=`/`+`/`-`) so merge identity is stable.
        "plus" => Ok(KeyName::Char('+')),
        "minus" => Ok(KeyName::Char('-')),
        "equal" | "equals" | "eq" => Ok(KeyName::Char('=')),
        "underscore" => Ok(KeyName::Char('_')),
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
    ///
    /// No longer bound by default (CTX-0257: `alt+w` closes the workspace);
    /// stays parseable so users keep pane-granularity close via an explicit
    /// bind, and `ctl terminal close` is unchanged.
    CloseView,
    /// Toggle single-pane zoom (`toggle_zoom`, alias `toggle_split_zoom`).
    ToggleZoom,
    /// Toggle the help popup (`toggle_help`, alias `show_help`).
    ///
    /// CTX-0265 (009 §which-key, DEC-0035 follow-through): the floating
    /// overlay lists every bound shortcut, generated from the live
    /// registry ([`help_rows_from_keymaps`]) — never a hardcoded copy.
    /// Defaults are `alt+`` plus the `alt+?` shifted-symbol spellings
    /// (`alt+?`/`alt+shift+?`/`alt+shift+/`; one action, four bindings).
    /// Repeating the chord hides it again; `Esc` dismisses; the overlay
    /// never touches grid truth (present-layer only).
    ToggleHelp,
    /// Focus next pane in depth-first order.
    FocusNext,
    /// Focus previous pane in depth-first order.
    FocusPrev,
    /// Focus numeric view id (`focus:3`, `1..=256`).
    ///
    /// No longer bound by default (CTX-0257: `alt+1..=9` jumps workspaces);
    /// stays parseable so users keep pane-number jump via an explicit bind,
    /// and `ctl view focus v:N` is unchanged.
    FocusId(u64),
    /// Copy the current selection to the system clipboard
    /// (`copy_to_clipboard`; ghostty `copy_to_clipboard:mixed` equivalent:
    /// clipboard write best-effort syncs the primary selection on Linux).
    /// No selection warns and keeps the layout untouched.
    CopyToClipboard,
    /// Paste the system clipboard as terminal input through the
    /// suspicious-paste inspection gate (`paste_from_clipboard`; ghostty
    /// `paste_from_clipboard` equivalent). Suspicious text waits on the
    /// pending-paste confirmation path; there is no silent delivery.
    PasteFromClipboard,
    /// Scroll the focused pane up by one viewport page (less-like).
    ScrollPageUp,
    /// Scroll the focused pane down by one viewport page (less-like).
    ScrollPageDown,
    /// Increase the per-window font size one step (CTX-0263 font zoom).
    IncreaseFontSize,
    /// Decrease the per-window font size one step (CTX-0263 font zoom).
    DecreaseFontSize,
    /// Reset the per-window font size to the startup value (CTX-0263).
    ResetFontSize,
    /// Open the Command Composer (CTX-0227, 008 route P4).
    ///
    /// Manual open only: this action is never in [`DEFAULT_KEYMAPS`], so a
    /// fresh config keeps Normal Mode byte-identical (the 008 §14 boundary).
    /// The user opts in with `{ chord = "alt+e", action = "open_composer" }`;
    /// the single-character schema rule already forces a modifier, so the
    /// open chord can never shadow bare shell typing.
    OpenComposer,
    /// Create a fresh workspace and switch to it (`workspace_new`, CTX-0257
    /// DEC-0034 entry, default chord `alt+n`). The new workspace starts as
    /// a single idle leaf; no shell spawns until the user splits or types
    /// (lazy spawn is a follow-up).
    WorkspaceNew,
    /// Close the active workspace (`workspace_close`, default `alt+w`).
    ///
    /// Workspaces with live pane sessions never die silently: the first
    /// press arms a pending-confirm (loud banner), repeating the chord
    /// confirms the kill, `Esc` cancels. Idle workspaces close immediately.
    /// Closing the last workspace resets it to a fresh idle leaf so the
    /// layout never strands empty.
    WorkspaceClose,
    /// Switch to the previous workspace (`workspace_prev`, default `alt+-`).
    WorkspacePrev,
    /// Switch to the next workspace (`workspace_next`, default `alt+=` —
    /// the unshifted DEC `+` spelling, since `+` cannot be a chord key).
    WorkspaceNext,
    /// Switch to the last-used workspace (`workspace_last`, default
    /// `alt+tab`, MRU order). No-op with a single workspace.
    WorkspaceLast,
    /// Jump to workspace N (`workspace_focus:<1..=16>`, defaults
    /// `alt+1..=9`). Unknown indices warn and keep the current workspace.
    WorkspaceFocus(u64),
    /// Move the focused window to workspace N (`workspace_move:<1..=16>`,
    /// defaults `shift+alt+1..=9` per DEC-0034, CTX-0259).
    ///
    /// Reparents the focused leaf (with its pane session) into the target
    /// workspace slot. Same-workspace is a no-op; unknown indices warn and
    /// keep state untouched. Never kills (kill-confirm stays with close),
    /// never removes a workspace (last-workspace `>= 1` holds), and never
    /// touches the runtime-global primary PTY.
    WorkspaceMove(u64),
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
            "toggle_help" | "show_help" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::ToggleHelp)
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
            "copy_to_clipboard" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::CopyToClipboard)
            }
            "paste_from_clipboard" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::PasteFromClipboard)
            }
            "scroll_page_up" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::ScrollPageUp)
            }
            "scroll_page_down" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::ScrollPageDown)
            }
            "increase_font_size" | "zoom_in" | "font_zoom_in" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::IncreaseFontSize)
            }
            "decrease_font_size" | "zoom_out" | "font_zoom_out" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::DecreaseFontSize)
            }
            "reset_font_size" | "zoom_reset" | "font_zoom_reset" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::ResetFontSize)
            }
            "open_composer" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::OpenComposer)
            }
            "workspace_new" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::WorkspaceNew)
            }
            "workspace_close" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::WorkspaceClose)
            }
            "workspace_prev" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::WorkspacePrev)
            }
            "workspace_next" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::WorkspaceNext)
            }
            "workspace_last" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::WorkspaceLast)
            }
            "workspace_focus" => {
                let n = require_workspace_index(arg, trimmed)?;
                Ok(Self::WorkspaceFocus(n))
            }
            "workspace_move" | "workspace_move_window" => {
                let n = require_workspace_index(arg, trimmed)?;
                Ok(Self::WorkspaceMove(n))
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
            Self::ToggleHelp => "toggle_help".to_string(),
            Self::FocusNext => "focus_next".to_string(),
            Self::FocusPrev => "focus_prev".to_string(),
            Self::FocusId(n) => format!("focus:{n}"),
            Self::CopyToClipboard => "copy_to_clipboard".to_string(),
            Self::PasteFromClipboard => "paste_from_clipboard".to_string(),
            Self::ScrollPageUp => "scroll_page_up".to_string(),
            Self::ScrollPageDown => "scroll_page_down".to_string(),
            Self::IncreaseFontSize => "increase_font_size".to_string(),
            Self::DecreaseFontSize => "decrease_font_size".to_string(),
            Self::ResetFontSize => "reset_font_size".to_string(),
            Self::OpenComposer => "open_composer".to_string(),
            Self::WorkspaceNew => "workspace_new".to_string(),
            Self::WorkspaceClose => "workspace_close".to_string(),
            Self::WorkspacePrev => "workspace_prev".to_string(),
            Self::WorkspaceNext => "workspace_next".to_string(),
            Self::WorkspaceLast => "workspace_last".to_string(),
            Self::WorkspaceFocus(n) => format!("workspace_focus:{n}"),
            Self::WorkspaceMove(n) => format!("workspace_move:{n}"),
        }
    }
}

/// Hint listing the accepted action vocabulary.
const KNOWN_ACTIONS_HINT: &str = "expected one of goto_split:<left|right|up|down>, new_split:<left|right|up|down>, resize_split:<left|right|up|down>, close_view, toggle_zoom, toggle_help, focus_next, focus_prev, focus:<1..=256>, copy_to_clipboard, paste_from_clipboard, scroll_page_up, scroll_page_down, increase_font_size, decrease_font_size, reset_font_size, open_composer, workspace_new, workspace_close, workspace_prev, workspace_next, workspace_last, workspace_focus:<1..=16>, workspace_move:<1..=16>";

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

/// Require a `workspace_focus:<n>` index argument.
fn require_workspace_index(arg: Option<&str>, raw: &str) -> Result<u64, ConfigError> {
    match arg {
        Some(n) if !n.is_empty() => match n.parse::<u64>() {
            Ok(id) if (1..=MAX_WORKSPACE_INDEX).contains(&id) => Ok(id),
            _ => Err(ConfigError::validation(
                "keymaps[].action",
                format!(
                    "action '{raw}' needs a workspace index 1..={MAX_WORKSPACE_INDEX} (e.g. 'workspace_focus:2')"
                ),
            )),
        },
        _ => Err(ConfigError::validation(
            "keymaps[].action",
            format!("action '{raw}' needs a workspace index (e.g. 'workspace_focus:2')"),
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

/// [`DEFAULT_KEYMAPS`] ships the Alt-as-Mod map derived from the ghostty
/// reference (`alt+h/j/k/l` navigate plus CTX-0262 `alt+arrows` aliases, `alt+u`/`alt+i` page up/down
/// less-like, `alt+z`/`alt+m`/`alt+f` zoom, `shift+alt` creates plus
/// CTX-0262 `shift+alt+arrows` aliases,
/// `shift+ctrl` resizes plus CTX-0262 `shift+ctrl+arrows` aliases plus the CTX-0258 `ctrl+shift+alt+h/j/k/l`
/// Mod-aware resize variant plus CTX-0262 `ctrl+shift+alt+arrows` aliases, `ctrl+alt+arrows` navigate, `ctrl+tab` cycles,
/// `ctrl+shift+c/v` copy/paste — ghostty `src/config/Config.zig` default
/// keybinds: `copy_to_clipboard:mixed` / `paste_from_clipboard` under
/// `ctrl+shift` on Linux) plus the DEC-0034 workspace entry (CTX-0257).
/// CTX-0259 adds `shift+alt+1..=9` move-window-to-workspace-N
/// (`workspace_move:<1..=9>`, Mod+Shift+Number per DEC-0034; carries the Mod
/// slot so a Super flip rebinds to `shift+super+1..=9`; digits stay free
/// under the single-owner rule — `shift+alt+h/j/k/l` remain creation).
/// CTX-0263 adds mod-independent `ctrl+=`/`ctrl+plus` (plus shifted
/// spellings) to grow, `ctrl+-` to shrink, and `ctrl+0` to reset the
/// per-window font size; bare `+`/`-`/`=`/`0` stay shell input.
/// Plain `Tab`, bare arrows, letters, and digits are deliberately unbound so
/// they reach the shell.
pub const DEFAULT_KEYMAPS: &[(&str, &str)] = &[
    ("alt+h", "goto_split:left"),
    ("alt+j", "goto_split:down"),
    ("alt+k", "goto_split:up"),
    ("alt+l", "goto_split:right"),
    // CTX-0262 arrow-key defaults for the HJKL focus actions (DEC-0035):
    // arrows are non-Vim aliases for the same actions (multi-bind: one
    // action supports multiple chords; each chord identity stays unique).
    ("alt+left", "goto_split:left"),
    ("alt+down", "goto_split:down"),
    ("alt+up", "goto_split:up"),
    ("alt+right", "goto_split:right"),
    ("alt+1", "workspace_focus:1"),
    ("alt+2", "workspace_focus:2"),
    ("alt+3", "workspace_focus:3"),
    ("alt+4", "workspace_focus:4"),
    ("alt+5", "workspace_focus:5"),
    ("alt+6", "workspace_focus:6"),
    ("alt+7", "workspace_focus:7"),
    ("alt+8", "workspace_focus:8"),
    ("alt+9", "workspace_focus:9"),
    // CTX-0259 Mod+Shift+Number move window across workspaces (DEC-0034):
    // `shift+alt+1..=9` reparents the focused leaf into workspace N.
    // Digits are free under single-owner (`shift+alt+h/j/k/l` stay creation).
    ("shift+alt+1", "workspace_move:1"),
    ("shift+alt+2", "workspace_move:2"),
    ("shift+alt+3", "workspace_move:3"),
    ("shift+alt+4", "workspace_move:4"),
    ("shift+alt+5", "workspace_move:5"),
    ("shift+alt+6", "workspace_move:6"),
    ("shift+alt+7", "workspace_move:7"),
    ("shift+alt+8", "workspace_move:8"),
    ("shift+alt+9", "workspace_move:9"),
    ("alt+u", "scroll_page_up"),
    ("alt+i", "scroll_page_down"),
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
    // CTX-0262 arrow-key defaults for the HJKL split actions.
    ("shift+alt+left", "new_split:left"),
    ("shift+alt+down", "new_split:down"),
    ("shift+alt+up", "new_split:up"),
    ("shift+alt+right", "new_split:right"),
    ("shift+ctrl+h", "resize_split:left"),
    ("shift+ctrl+j", "resize_split:down"),
    ("shift+ctrl+k", "resize_split:up"),
    ("shift+ctrl+l", "resize_split:right"),
    // CTX-0262 arrow-key defaults for the legacy HJKL resize actions
    // (mod-independent fixed chords: no `alt` slot, pass Super flip through).
    ("shift+ctrl+left", "resize_split:left"),
    ("shift+ctrl+down", "resize_split:down"),
    ("shift+ctrl+up", "resize_split:up"),
    ("shift+ctrl+right", "resize_split:right"),
    // CTX-0258 Mod-aware resize variant: `ctrl+shift+alt` carries the Mod
    // slot so a Super flip rebinds it to `ctrl+shift+super` (pure
    // `shift+alt` cannot be reused: it already creates via `new_split`
    // under the single-owner rule).
    ("ctrl+shift+alt+h", "resize_split:left"),
    ("ctrl+shift+alt+j", "resize_split:down"),
    ("ctrl+shift+alt+k", "resize_split:up"),
    ("ctrl+shift+alt+l", "resize_split:right"),
    // CTX-0262 arrow-key defaults for the Mod-aware HJKL resize variant
    // (carries the Mod slot so a Super flip rebinds to `ctrl+shift+super`).
    ("ctrl+shift+alt+left", "resize_split:left"),
    ("ctrl+shift+alt+down", "resize_split:down"),
    ("ctrl+shift+alt+up", "resize_split:up"),
    ("ctrl+shift+alt+right", "resize_split:right"),
    ("alt+w", "workspace_close"),
    ("alt+m", "toggle_zoom"),
    ("alt+f", "toggle_zoom"),
    ("ctrl+shift+c", "copy_to_clipboard"),
    ("ctrl+shift+v", "paste_from_clipboard"),
    ("alt+z", "toggle_zoom"),
    ("alt+n", "workspace_new"),
    ("alt+-", "workspace_prev"),
    ("alt+=", "workspace_next"),
    ("alt+tab", "workspace_last"),
    // CTX-0263 font zoom (per-window, Ctrl-held so bare typing stays
    // shell): `=` covers the unshifted `=` key, `plus` covers `+`
    // (Shift+= on US reports `+`+shift or `=`+shift depending on platform,
    // hence both the plain and shifted spellings), `-` covers minus,
    // `0` resets to the startup size (free: no shipped default uses it).
    ("ctrl+equal", "increase_font_size"),
    ("ctrl+plus", "increase_font_size"),
    ("ctrl+shift+equal", "increase_font_size"),
    ("ctrl+shift+plus", "increase_font_size"),
    ("ctrl+minus", "decrease_font_size"),
    ("ctrl+shift+minus", "decrease_font_size"),
    ("ctrl+0", "reset_font_size"),
    // CTX-0265 help popup (009 which-key, DEC-0035 follow-through; CTX-0264
    // left these chords unallocated): `alt+`` toggles the overlay, plus the
    // `alt+?` shifted-symbol spellings — `?` physically carries Shift, and
    // shifted-symbol reporting varies by platform (CTX-0263 precedent), so
    // the plain, shifted-`?`, and shifted-`/` spellings all toggle the one
    // action. Every entry carries the Mod slot so a Super flip rebinds the
    // whole gesture to `super`.
    ("alt+`", "toggle_help"),
    ("alt+?", "toggle_help"),
    ("alt+shift+?", "toggle_help"),
    ("alt+shift+/", "toggle_help"),
];

/// Build the shipped defaults against one [`ModKey`] (CTX-0236).
///
/// [`DEFAULT_KEYMAPS`] is the canonical Alt spelling (kept byte-identical
/// for the CTX-0178 wizard pin): every entry carrying `alt` is rebound
/// through [`ModKey::apply_to`], entries without `alt` pass through
/// unchanged. Fail-closed only on an internal default typo (covered by
/// `defaults_parse`; user input never reaches this path).
pub fn default_keymaps_with_mod(mod_key: ModKey) -> Result<Vec<ResolvedKeymap>, ConfigError> {
    let mut out = Vec::with_capacity(DEFAULT_KEYMAPS.len());
    for (chord_raw, action_raw) in DEFAULT_KEYMAPS {
        let chord = Chord::parse(chord_raw).map_err(|e| ConfigError::InvalidInput {
            message: format!("internal default keymap invalid: {e}"),
        })?;
        let action = ChromeAction::parse(action_raw).map_err(|e| ConfigError::InvalidInput {
            message: format!("internal default keymap invalid: {e}"),
        })?;
        out.push(ResolvedKeymap {
            chord: mod_key.apply_to(chord),
            action,
            context: GLOBAL_CONTEXT.to_string(),
            from_default: true,
        });
    }
    Ok(out)
}

/// Build the shipped defaults. Fail-closed only on an internal default typo
/// (covered by `defaults_parse`; user input never reaches this path).
pub fn default_keymaps() -> Result<Vec<ResolvedKeymap>, ConfigError> {
    default_keymaps_with_mod(ModKey::default())
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

/// Resolve the effective keymap table: shipped defaults (rendered against
/// `effective.mod_key`, so one setting rebinds the chrome map) overridden by
/// user entries with the same `context + chord` identity (the existing merge
/// rule), then sorted deterministically by identity.
///
/// Explicit user chords keep their exact spelling: under a non-default mod
/// they coexist with the rebound defaults (same identity replaces, anything
/// else appends).
///
/// Fail-closed on unknown contexts, chords, or actions, and on duplicate
/// normalized chords within the user config.
pub fn resolve_keymaps(effective: &EffectiveConfig) -> Result<Vec<ResolvedKeymap>, ConfigError> {
    let mut table = default_keymaps_with_mod(effective.mod_key)?;
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

/// Render one help-popup row per resolved binding (CTX-0265).
///
/// The popup content is generated FROM the live registry: each row is the
/// entry's canonical chord plus its canonical action
/// (`"alt+h  goto_split:left"`), in resolved-table order, so adding,
/// removing, or rebinding a chord (including a Super flip, which re-spells
/// every Mod chord) changes the popup by construction — there is no
/// hardcoded copy to drift. The app regenerates these rows from its live
/// `keymaps` table on every show. Bounded: one row per table entry (user
/// tables cap at [`crate::types::MAX_KEYMAPS`]) and each row caps at
/// `MAX_CHORD_LEN + MAX_ACTION_LEN + 2` bytes.
#[must_use]
pub fn help_rows_from_keymaps(maps: &[ResolvedKeymap]) -> Vec<String> {
    maps.iter()
        .map(|m| format!("{}  {}", m.chord.canonical(), m.action.canonical()))
        .collect()
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

    fn key_ref_super(key: KeyName, ctrl: bool, shift: bool) -> KeyRef {
        KeyRef {
            key,
            ctrl,
            alt: false,
            shift,
            super_held: true,
        }
    }

    #[test]
    fn mod_key_parse_accepts_aliases_case_insensitive() {
        for raw in ["alt", "Alt", " ALT ", "opt", "OPT", "option", "Option"] {
            assert_eq!(ModKey::parse(raw).expect("alt alias"), ModKey::Alt);
        }
        for raw in [
            "super", "Super", " SUPER ", "meta", "Meta", "cmd", "command", "win", "Win", "windows",
        ] {
            assert_eq!(ModKey::parse(raw).expect("super alias"), ModKey::Super);
        }
        assert_eq!(ModKey::default(), ModKey::Alt);
        assert_eq!(ModKey::Alt.canonical(), "alt");
        assert_eq!(ModKey::Super.canonical(), "super");
    }

    #[test]
    fn mod_key_parse_rejects_fail_closed() {
        // Empty, overlong, unknown, and ctrl/shift (shell-shadow mods) all
        // fail closed on the `mod_key` field.
        let mut bad: Vec<String> = vec![
            "".into(),
            "   ".into(),
            "ctrl".into(),
            "control".into(),
            "shift".into(),
            "hyper".into(),
            "altgraph".into(),
            "banana".into(),
            "alt+shift".into(),
        ];
        bad.push("a".repeat(MAX_MOD_KEY_LEN + 1));
        for raw in &bad {
            let err = ModKey::parse(raw).unwrap_err();
            assert!(
                err.to_string().contains("mod_key"),
                "must name field for {raw:?}: {err}"
            );
        }
    }

    #[test]
    fn default_mod_is_identity_over_shipped_map() {
        // `ModKey::Alt` renders the canonical map byte-identically, so the
        // CTX-0178 wizard pin and existing tests keep passing untouched.
        let shipped = default_keymaps().expect("defaults valid");
        let via_mod = default_keymaps_with_mod(ModKey::Alt).expect("alt mod valid");
        assert_eq!(shipped, via_mod);
        assert_eq!(shipped.len(), DEFAULT_KEYMAPS.len());
    }

    #[test]
    fn super_mod_flip_rebinds_chrome_map() {
        let maps = default_keymaps_with_mod(ModKey::Super).expect("super defaults valid");
        assert_eq!(maps.len(), DEFAULT_KEYMAPS.len());
        // No rebound default shadows another.
        let mut seen = std::collections::HashSet::new();
        for m in &maps {
            assert!(seen.insert(m.id()), "duplicate rebound id {}", m.id());
        }
        // Alt-bearing defaults moved to Super ...
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('h'), false, false)),
            Some(ChromeAction::GotoSplit(SplitDir::Left))
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('w'), false, false)),
            Some(ChromeAction::WorkspaceClose)
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('n'), false, false)),
            Some(ChromeAction::WorkspaceNew)
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('m'), false, false)),
            Some(ChromeAction::ToggleZoom)
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('1'), false, false)),
            Some(ChromeAction::WorkspaceFocus(1))
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('h'), false, true)),
            Some(ChromeAction::NewSplit(SplitDir::Left))
        );
        assert_eq!(
            match_keymap(
                &maps,
                KeyRef {
                    key: KeyName::Left,
                    ctrl: true,
                    alt: false,
                    shift: false,
                    super_held: true,
                }
            ),
            Some(ChromeAction::GotoSplit(SplitDir::Left))
        );
        // ... the old Alt chords are unbound (back to the shell) ...
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('h'), false, true, false)),
            None,
            "alt+h unbound under super mod"
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('w'), false, true, false)),
            None,
            "alt+w unbound under super mod"
        );
        // ... and the mod-independent fixed chords are untouched.
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Tab, true, false, false)),
            Some(ChromeAction::FocusNext)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('h'), true, false, true)),
            Some(ChromeAction::ResizeSplit(SplitDir::Left))
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('c'), true, false, true)),
            Some(ChromeAction::CopyToClipboard)
        );
    }

    #[test]
    fn super_mod_leaves_shell_keys_unbound() {
        let maps = default_keymaps_with_mod(ModKey::Super).expect("super defaults valid");
        for k in [
            key_ref(KeyName::Tab, false, false, false),
            key_ref(KeyName::Up, false, false, false),
            key_ref(KeyName::Char('h'), false, false, false),
            key_ref(KeyName::Char('p'), true, false, false),
            key_ref(KeyName::Char('w'), true, false, false),
            key_ref_super(KeyName::Char('p'), false, false),
        ] {
            assert_eq!(match_keymap(&maps, k), None, "shell key {k:?}");
        }
    }

    #[test]
    fn explicit_chords_survive_mod_flip_by_exact_identity() {
        // An explicit `alt+h` override replaces the default under the
        // default mod, and coexists with the rebound defaults under Super
        // (exact-match identity is preserved, never rewritten).
        let flip = EffectiveConfig {
            mod_key: ModKey::Super,
            keymaps: vec![KeymapEntry {
                chord: "alt+h".into(),
                action: "focus_next".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        let maps = resolve_keymaps(&flip).expect("resolves");
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('h'), false, true, false)),
            Some(ChromeAction::FocusNext),
            "explicit alt+h wins"
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('h'), false, false)),
            Some(ChromeAction::GotoSplit(SplitDir::Left)),
            "rebound super+h default intact"
        );
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
        assert_eq!(
            ChromeAction::parse("copy_to_clipboard").expect("copy"),
            ChromeAction::CopyToClipboard
        );
        assert_eq!(
            ChromeAction::parse("paste_from_clipboard").expect("paste"),
            ChromeAction::PasteFromClipboard
        );
        assert_eq!(
            ChromeAction::parse("scroll_page_up").expect("page up"),
            ChromeAction::ScrollPageUp
        );
        assert_eq!(
            ChromeAction::parse("scroll_page_down").expect("page down"),
            ChromeAction::ScrollPageDown
        );
        assert_eq!(
            ChromeAction::parse("workspace_new").expect("ws new"),
            ChromeAction::WorkspaceNew
        );
        assert_eq!(
            ChromeAction::parse("workspace_close").expect("ws close"),
            ChromeAction::WorkspaceClose
        );
        assert_eq!(
            ChromeAction::parse("workspace_prev").expect("ws prev"),
            ChromeAction::WorkspacePrev
        );
        assert_eq!(
            ChromeAction::parse("workspace_next").expect("ws next"),
            ChromeAction::WorkspaceNext
        );
        assert_eq!(
            ChromeAction::parse("workspace_last").expect("ws last"),
            ChromeAction::WorkspaceLast
        );
        assert_eq!(
            ChromeAction::parse("workspace_focus:3").expect("ws focus"),
            ChromeAction::WorkspaceFocus(3)
        );
        assert_eq!(
            ChromeAction::WorkspaceFocus(2).canonical(),
            "workspace_focus:2"
        );
        assert_eq!(
            ChromeAction::parse("workspace_move:3").expect("ws move"),
            ChromeAction::WorkspaceMove(3)
        );
        assert_eq!(
            ChromeAction::parse("workspace_move_window:2").expect("ws move alias"),
            ChromeAction::WorkspaceMove(2)
        );
        assert_eq!(
            ChromeAction::WorkspaceMove(2).canonical(),
            "workspace_move:2"
        );
        assert_eq!(
            ChromeAction::parse("increase_font_size").expect("zoom in"),
            ChromeAction::IncreaseFontSize
        );
        assert_eq!(
            ChromeAction::parse("zoom_in").expect("zoom_in alias"),
            ChromeAction::IncreaseFontSize
        );
        assert_eq!(
            ChromeAction::parse("decrease_font_size").expect("zoom out"),
            ChromeAction::DecreaseFontSize
        );
        assert_eq!(
            ChromeAction::parse("zoom_out").expect("zoom_out alias"),
            ChromeAction::DecreaseFontSize
        );
        assert_eq!(
            ChromeAction::parse("reset_font_size").expect("zoom reset"),
            ChromeAction::ResetFontSize
        );
        assert_eq!(
            ChromeAction::parse("zoom_reset").expect("zoom_reset alias"),
            ChromeAction::ResetFontSize
        );
        assert_eq!(
            ChromeAction::IncreaseFontSize.canonical(),
            "increase_font_size"
        );
        assert_eq!(
            ChromeAction::DecreaseFontSize.canonical(),
            "decrease_font_size"
        );
        assert_eq!(ChromeAction::ResetFontSize.canonical(), "reset_font_size");
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
            "workspace_focus",
            "workspace_focus:0",
            "workspace_focus:17",
            "workspace_focus:abc",
            "workspace_move",
            "workspace_move:0",
            "workspace_move:17",
            "workspace_move:abc",
            "workspace_new:1",
            "workspace_close:1",
            "goto_split:left:extra",
            "copy_to_clipboard:mixed",
            "paste_from_clipboard:1",
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
        // CTX-0262: bare arrows stay shell-bound (only Alt/Shift+Alt/etc.
        // combos are chrome).
        let shell_keys = [
            key_ref(KeyName::Tab, false, false, false),
            key_ref(KeyName::Up, false, false, false),
            key_ref(KeyName::Down, false, false, false),
            key_ref(KeyName::Left, false, false, false),
            key_ref(KeyName::Right, false, false, false),
            key_ref(KeyName::Char('n'), false, false, false),
            key_ref(KeyName::Char('p'), false, false, false),
            key_ref(KeyName::Char('1'), false, false, false),
            key_ref(KeyName::Char('u'), false, false, false),
            key_ref(KeyName::Char('i'), false, false, false),
            key_ref(KeyName::Char('z'), false, false, false),
            // CTX-0257: the workspace-entry keys stay shell-bound when bare
            // (only the Alt chords are chrome-owned).
            key_ref(KeyName::Char('-'), false, false, false),
            key_ref(KeyName::Char('='), false, false, false),
            // Ctrl+P is shell input unless the user binds it (CTX-0154
            // single-owner: 0x10 goes to the PTY, focus must not move).
            key_ref(KeyName::Char('p'), true, false, false),
            // Ctrl+C (no shift) is SIGINT for the shell: only the shifted
            // chord is owned by chrome (CTX-0161).
            key_ref(KeyName::Char('c'), true, false, false),
            // Ctrl+V is shell input (verbatim/paste in readline); only the
            // shifted chord is owned by chrome.
            key_ref(KeyName::Char('v'), true, false, false),
            // CTX-0263: bare `+`/`-`/`=`/`0` stay shell typing; only the
            // Ctrl-held chords zoom.
            key_ref(KeyName::Char('+'), false, false, false),
            key_ref(KeyName::Char('-'), false, false, false),
            key_ref(KeyName::Char('='), false, false, false),
            key_ref(KeyName::Char('0'), false, false, false),
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
            Some(ChromeAction::WorkspaceClose)
        );
        // CTX-0161 copy/paste chords: single-owner intercept owns the
        // shifted chords; the unshifted C0 bytes stay shell input (above).
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('c'), true, false, true)),
            Some(ChromeAction::CopyToClipboard)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('v'), true, false, true)),
            Some(ChromeAction::PasteFromClipboard)
        );
        // CTX-0263 font zoom: every Ctrl spelling resolves, bare keys
        // above stay shell.
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('='), true, false, false)),
            Some(ChromeAction::IncreaseFontSize)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('+'), true, false, false)),
            Some(ChromeAction::IncreaseFontSize)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('='), true, false, true)),
            Some(ChromeAction::IncreaseFontSize)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('+'), true, false, true)),
            Some(ChromeAction::IncreaseFontSize)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('-'), true, false, false)),
            Some(ChromeAction::DecreaseFontSize)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('-'), true, false, true)),
            Some(ChromeAction::DecreaseFontSize)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('0'), true, false, false)),
            Some(ChromeAction::ResetFontSize)
        );
    }

    #[test]
    fn open_composer_parses_but_is_never_a_default() {
        // CTX-0227 boundary: fresh configs keep Normal Mode byte-identical
        // (no Enter hijack, no auto-open). `open_composer` only exists when
        // the user binds it explicitly (suggested chord `alt+e`).
        assert_eq!(
            ChromeAction::parse("open_composer").expect("parses"),
            ChromeAction::OpenComposer
        );
        assert_eq!(ChromeAction::OpenComposer.canonical(), "open_composer");
        let maps = default_keymaps().expect("defaults valid");
        assert!(
            !maps.iter().any(|m| m.action == ChromeAction::OpenComposer),
            "defaults must not bind open_composer"
        );
        // Bare `e` can never be a chord (schema rule), so the open key
        // cannot shadow shell typing even when bound.
        assert!(Chord::parse("e").is_err());
        let open = Chord::parse("alt+e").expect("alt+e parses");
        assert_eq!(open.canonical(), "alt+e");
    }

    #[test]
    fn defaults_alt_number_jump_and_page_and_zoom() {
        // CTX-0178 Alt-as-Mod: fresh config jumps, pages, and zooms.
        // CTX-0257: alt+1..=9 jumps WORKSPACES now (DEC-0034); pane-number
        // jump (`focus:<n>`) stays parseable for explicit binds.
        let maps = default_keymaps().expect("defaults valid");
        for (digit, idx) in [
            ('1', 1),
            ('2', 2),
            ('3', 3),
            ('4', 4),
            ('5', 5),
            ('6', 6),
            ('7', 7),
            ('8', 8),
            ('9', 9),
        ] {
            assert_eq!(
                match_keymap(&maps, key_ref(KeyName::Char(digit), false, true, false)),
                Some(ChromeAction::WorkspaceFocus(idx)),
                "alt+{digit} jumps to workspace {idx}"
            );
        }
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('u'), false, true, false)),
            Some(ChromeAction::ScrollPageUp)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('i'), false, true, false)),
            Some(ChromeAction::ScrollPageDown)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('z'), false, true, false)),
            Some(ChromeAction::ToggleZoom)
        );
        // Vim hjkl moves stay bound.
        for (key, dir) in [
            ('h', SplitDir::Left),
            ('j', SplitDir::Down),
            ('k', SplitDir::Up),
            ('l', SplitDir::Right),
        ] {
            assert_eq!(
                match_keymap(&maps, key_ref(KeyName::Char(key), false, true, false)),
                Some(ChromeAction::GotoSplit(dir)),
                "alt+{key} moves"
            );
        }
    }

    #[test]
    fn defaults_mod_shift_number_moves_window() {
        // CTX-0259 DEC-0034 follow-through: Mod+Shift+Number moves the
        // focused window across workspaces (distinct from Alt+Number jump).
        let maps = default_keymaps().expect("defaults valid");
        for (digit, idx) in [
            ('1', 1),
            ('2', 2),
            ('3', 3),
            ('4', 4),
            ('5', 5),
            ('6', 6),
            ('7', 7),
            ('8', 8),
            ('9', 9),
        ] {
            assert_eq!(
                match_keymap(&maps, key_ref(KeyName::Char(digit), false, true, true)),
                Some(ChromeAction::WorkspaceMove(idx)),
                "shift+alt+{digit} moves to workspace {idx}"
            );
        }
        // Super flip carries the Mod slot (shift+super+digit).
        let super_maps = default_keymaps_with_mod(ModKey::Super).expect("super valid");
        assert_eq!(
            match_keymap(
                &super_maps,
                KeyRef {
                    key: KeyName::Char('2'),
                    ctrl: false,
                    alt: false,
                    shift: true,
                    super_held: true,
                }
            ),
            Some(ChromeAction::WorkspaceMove(2)),
            "shift+super+2 moves under Super mod"
        );
        // Old shift+alt chord is unbound under Super (back to shell).
        assert_eq!(
            match_keymap(&super_maps, key_ref(KeyName::Char('2'), false, true, true)),
            None,
            "shift+alt+2 unbound under super mod"
        );
    }

    #[test]
    fn defaults_have_unique_chord_identities() {
        // Collision audit as a test: every default chord identity is unique
        // so no default shadows another (CTX-0178, extended CTX-0258 to
        // cover the Super-rebound map as well). CTX-0257 extends the
        // audit to the DEC-0034 entry set: 35 shipped + 4 new (alt+n/-/=/tab;
        // alt+w and alt+1..=9 are rebinds, not new identities) = 39, plus
        // CTX-0258's 4 Mod-aware resize chords = 43, plus CTX-0262's 16
        // arrow-key aliases (4 focus + 4 split + 4 legacy resize + 4
        // Mod-aware resize) = 59, plus CTX-0263's 7 mod-independent font-zoom
        // chords = 66, plus CTX-0259's 9 Mod+Shift+Number move chords
        // (shift+alt+1..=9) = 75, plus CTX-0265's 4 help chords (alt+backtick
        // + 3 alt+? shifted-symbol spellings) = 79 total, and the full DEC
        // set resolves. Zoom chords carry
        // no `alt`, so they must stay unique under Alt and Super alike.
        for mod_key in [ModKey::Alt, ModKey::Super] {
            let maps = default_keymaps_with_mod(mod_key).expect("defaults valid");
            assert_eq!(
                maps.len(),
                79,
                "35 shipped + 4 workspace-entry chords + 4 resize chords + 16 arrow aliases + 7 zoom chords + 9 move chords + 4 help chords"
            );
            let mut seen = std::collections::HashSet::new();
            for m in &maps {
                assert!(
                    seen.insert(m.id()),
                    "duplicate default id {} under mod {:?}",
                    m.id(),
                    mod_key
                );
            }
        }
        let maps = default_keymaps().expect("defaults valid");
        let mut seen = std::collections::HashSet::new();
        for m in &maps {
            assert!(seen.insert(m.id()), "duplicate default id {}", m.id());
        }
        let maps = default_keymaps().expect("defaults valid");
        // The DEC-0034 entry set resolves through the shipped table.
        let dec: &[(&str, bool, bool, ChromeAction)] = &[
            ("n", false, false, ChromeAction::WorkspaceNew),
            ("w", false, false, ChromeAction::WorkspaceClose),
            ("-", false, false, ChromeAction::WorkspacePrev),
            ("=", false, false, ChromeAction::WorkspaceNext),
        ];
        for (key, ctrl, shift, want) in dec {
            let key = KeyName::Char(key.chars().next().expect("single"));
            assert_eq!(
                match_keymap(&maps, key_ref(key, *ctrl, true, *shift)),
                Some(*want),
                "DEC chord alt+{key:?}"
            );
        }
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Tab, false, true, false)),
            Some(ChromeAction::WorkspaceLast),
            "alt+tab is last-used workspace"
        );
        // All single-character defaults require a modifier (typing safety).
        for (chord, _) in DEFAULT_KEYMAPS {
            let parsed = Chord::parse(chord).expect("default parses");
            assert!(
                parsed.ctrl || parsed.alt || parsed.shift || parsed.super_held,
                "default '{chord}' must hold a modifier"
            );
        }
    }

    #[test]
    fn arrow_aliases_mirror_hjkl_both_mods() {
        // CTX-0262 (DEC-0035): arrows are non-Vim aliases for the HJKL
        // directional actions (multi-bind: same action, distinct chords).
        // Alt+arrows focus, Shift+Alt+arrows split, Shift+Ctrl+arrows
        // legacy-resize, Ctrl+Shift+Alt+arrows Mod-aware-resize. Bare
        // arrows stay shell-bound; Super flip rebinds the `alt`-bearing
        // arrow variants while legacy `shift+ctrl` passes through.
        let alt_maps = default_keymaps_with_mod(ModKey::Alt).expect("alt valid");
        let super_maps = default_keymaps_with_mod(ModKey::Super).expect("super valid");
        let dirs = [
            (KeyName::Left, SplitDir::Left),
            (KeyName::Down, SplitDir::Down),
            (KeyName::Up, SplitDir::Up),
            (KeyName::Right, SplitDir::Right),
        ];
        for (key, dir) in dirs {
            // Focus: alt+arrows like alt+h/j/k/l.
            assert_eq!(
                match_keymap(&alt_maps, key_ref(key, false, true, false)),
                Some(ChromeAction::GotoSplit(dir)),
                "alt+{key:?} focuses"
            );
            // Split: shift+alt+arrows like shift+alt+h/j/k/l.
            assert_eq!(
                match_keymap(&alt_maps, key_ref(key, false, true, true)),
                Some(ChromeAction::NewSplit(dir)),
                "shift+alt+{key:?} splits"
            );
            // Legacy resize: shift+ctrl+arrows (mod-independent fixed).
            assert_eq!(
                match_keymap(&alt_maps, key_ref(key, true, false, true)),
                Some(ChromeAction::ResizeSplit(dir)),
                "shift+ctrl+{key:?} resizes (alt map)"
            );
            assert_eq!(
                match_keymap(&super_maps, key_ref(key, true, false, true)),
                Some(ChromeAction::ResizeSplit(dir)),
                "shift+ctrl+{key:?} resizes (super map)"
            );
            // Mod-aware resize: ctrl+shift+alt+arrows under Alt ...
            assert_eq!(
                match_keymap(&alt_maps, key_ref(key, true, true, true)),
                Some(ChromeAction::ResizeSplit(dir)),
                "ctrl+shift+alt+{key:?} resizes"
            );
            // ... rebound to ctrl+shift+super+arrows under Super ...
            assert_eq!(
                match_keymap(
                    &super_maps,
                    KeyRef {
                        key,
                        ctrl: true,
                        alt: false,
                        shift: true,
                        super_held: true,
                    }
                ),
                Some(ChromeAction::ResizeSplit(dir)),
                "ctrl+shift+super+{key:?} resizes"
            );
            // ... and the old alt spelling is unbound under Super.
            assert_eq!(
                match_keymap(&super_maps, key_ref(key, true, true, true)),
                None,
                "ctrl+shift+alt+{key:?} unbound under super mod"
            );
            // Super-flipped focus/split arrows.
            assert_eq!(
                match_keymap(
                    &super_maps,
                    KeyRef {
                        key,
                        ctrl: false,
                        alt: false,
                        shift: false,
                        super_held: true,
                    }
                ),
                Some(ChromeAction::GotoSplit(dir)),
                "super+{key:?} focuses"
            );
            assert_eq!(
                match_keymap(
                    &super_maps,
                    KeyRef {
                        key,
                        ctrl: false,
                        alt: false,
                        shift: true,
                        super_held: true,
                    }
                ),
                Some(ChromeAction::NewSplit(dir)),
                "shift+super+{key:?} splits"
            );
            // Old alt arrow spellings are unbound under Super (shell).
            assert_eq!(
                match_keymap(&super_maps, key_ref(key, false, true, false)),
                None,
                "alt+{key:?} unbound under super mod"
            );
        }
        // Bare arrows stay shell-bound under both mods (single-owner:
        // only Alt/Shift+Alt/etc. combos are chrome).
        for key in [KeyName::Left, KeyName::Down, KeyName::Up, KeyName::Right] {
            assert_eq!(
                match_keymap(&alt_maps, key_ref(key, false, false, false)),
                None,
                "bare {key:?} is shell (alt map)"
            );
            assert_eq!(
                match_keymap(
                    &super_maps,
                    KeyRef {
                        key,
                        ctrl: false,
                        alt: false,
                        shift: false,
                        super_held: false,
                    }
                ),
                None,
                "bare {key:?} is shell (super map)"
            );
        }
    }

    #[test]
    fn workspace_entry_survives_super_flip() {
        // CTX-0257 Req-5: the new Alt-chords go through the same alt-slot
        // substitution (`default_keymaps_with_mod`), so the Super flip keeps
        // working for workspace ops exactly like pane ops.
        let maps = default_keymaps_with_mod(ModKey::Super).expect("super valid");
        assert_eq!(maps.len(), DEFAULT_KEYMAPS.len());
        let mut seen = std::collections::HashSet::new();
        for m in &maps {
            assert!(seen.insert(m.id()), "duplicate rebound id {}", m.id());
        }
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('n'), false, false)),
            Some(ChromeAction::WorkspaceNew)
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('-'), false, false)),
            Some(ChromeAction::WorkspacePrev)
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('='), false, false)),
            Some(ChromeAction::WorkspaceNext)
        );
        assert_eq!(
            match_keymap(
                &maps,
                KeyRef {
                    key: KeyName::Tab,
                    ctrl: false,
                    alt: false,
                    shift: false,
                    super_held: true,
                }
            ),
            Some(ChromeAction::WorkspaceLast)
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('9'), false, false)),
            Some(ChromeAction::WorkspaceFocus(9))
        );
        // Old Alt spellings are unbound (back to the shell) under Super.
        for key in [
            KeyName::Char('n'),
            KeyName::Char('w'),
            KeyName::Char('-'),
            KeyName::Char('='),
            KeyName::Tab,
            KeyName::Char('1'),
        ] {
            assert_eq!(
                match_keymap(&maps, key_ref(key, false, true, false)),
                None,
                "alt+{key:?} unbound under super mod"
            );
        }
    }

    #[test]
    fn workspace_keys_mod_m_and_hjkl_pinned_both_mods() {
        // CTX-0258: `Mod+M` zoom and `Mod+HJKL` directional focus are
        // pinned under BOTH Alt and Super (Super via CTX-0236 substitution).
        let alt_maps = default_keymaps_with_mod(ModKey::Alt).expect("alt defaults valid");
        for (key, dir) in [
            ('h', SplitDir::Left),
            ('j', SplitDir::Down),
            ('k', SplitDir::Up),
            ('l', SplitDir::Right),
        ] {
            assert_eq!(
                match_keymap(&alt_maps, key_ref(KeyName::Char(key), false, true, false)),
                Some(ChromeAction::GotoSplit(dir)),
                "alt+{key} focuses"
            );
        }
        assert_eq!(
            match_keymap(&alt_maps, key_ref(KeyName::Char('m'), false, true, false)),
            Some(ChromeAction::ToggleZoom),
            "alt+m zooms"
        );

        let super_maps = default_keymaps_with_mod(ModKey::Super).expect("super defaults valid");
        for (key, dir) in [
            ('h', SplitDir::Left),
            ('j', SplitDir::Down),
            ('k', SplitDir::Up),
            ('l', SplitDir::Right),
        ] {
            assert_eq!(
                match_keymap(&super_maps, key_ref_super(KeyName::Char(key), false, false)),
                Some(ChromeAction::GotoSplit(dir)),
                "super+{key} focuses"
            );
        }
        assert_eq!(
            match_keymap(&super_maps, key_ref_super(KeyName::Char('m'), false, false)),
            Some(ChromeAction::ToggleZoom),
            "super+m zooms"
        );
        // Old Alt chords are unbound under Super (back to the shell).
        assert_eq!(
            match_keymap(&super_maps, key_ref(KeyName::Char('m'), false, true, false)),
            None,
            "alt+m unbound under super mod"
        );
    }

    #[test]
    fn resize_has_legacy_and_mod_aware_variants_both_mods() {
        // CTX-0258: `shift+ctrl+h/j/k/l` stays as the mod-independent
        // legacy resize, and `ctrl+shift+alt+h/j/k/l` is the Mod-aware
        // variant (rebound to `ctrl+shift+super` under a Super flip).
        // Pure `shift+alt` stays `new_split` under both mods (single-owner).
        let alt_maps = default_keymaps_with_mod(ModKey::Alt).expect("alt defaults valid");
        let super_maps = default_keymaps_with_mod(ModKey::Super).expect("super defaults valid");
        for (key, dir) in [
            ('h', SplitDir::Left),
            ('j', SplitDir::Down),
            ('k', SplitDir::Up),
            ('l', SplitDir::Right),
        ] {
            // Legacy fixed chord works under both mods (no `alt` slot).
            assert_eq!(
                match_keymap(&alt_maps, key_ref(KeyName::Char(key), true, false, true)),
                Some(ChromeAction::ResizeSplit(dir)),
                "shift+ctrl+{key} resizes (alt map)"
            );
            assert_eq!(
                match_keymap(&super_maps, key_ref(KeyName::Char(key), true, false, true)),
                Some(ChromeAction::ResizeSplit(dir)),
                "shift+ctrl+{key} resizes (super map)"
            );
            // Mod-aware variant: alt spelling under Alt ...
            assert_eq!(
                match_keymap(&alt_maps, key_ref(KeyName::Char(key), true, true, true)),
                Some(ChromeAction::ResizeSplit(dir)),
                "ctrl+shift+alt+{key} resizes"
            );
            // ... rebound to super spelling under Super ...
            assert_eq!(
                match_keymap(
                    &super_maps,
                    KeyRef {
                        key: KeyName::Char(key),
                        ctrl: true,
                        alt: false,
                        shift: true,
                        super_held: true,
                    }
                ),
                Some(ChromeAction::ResizeSplit(dir)),
                "ctrl+shift+super+{key} resizes"
            );
            // ... and the old alt spelling is unbound under Super.
            assert_eq!(
                match_keymap(&super_maps, key_ref(KeyName::Char(key), true, true, true)),
                None,
                "ctrl+shift+alt+{key} unbound under super mod"
            );
            // `shift+alt` / `shift+super` still creates (no resize shadow).
            assert_eq!(
                match_keymap(&alt_maps, key_ref(KeyName::Char(key), false, true, true)),
                Some(ChromeAction::NewSplit(dir)),
                "shift+alt+{key} still splits"
            );
            assert_eq!(
                match_keymap(
                    &super_maps,
                    KeyRef {
                        key: KeyName::Char(key),
                        ctrl: false,
                        alt: false,
                        shift: true,
                        super_held: true,
                    }
                ),
                Some(ChromeAction::NewSplit(dir)),
                "shift+super+{key} still splits"
            );
        }
        // Shell keys stay unbound under both maps (bare keys, Tab,
        // arrows, and unshifted C0 bytes reach the shell; super+h/m are
        // chrome-owned per the focus/zoom pin above and excluded here).
        for k in [
            key_ref(KeyName::Tab, false, false, false),
            key_ref(KeyName::Char('h'), false, false, false),
            key_ref(KeyName::Char('m'), false, false, false),
        ] {
            assert_eq!(
                match_keymap(&alt_maps, k),
                None,
                "shell key {k:?} (alt map)"
            );
        }
        for k in [
            key_ref(KeyName::Tab, false, false, false),
            key_ref(KeyName::Char('h'), false, false, false),
            key_ref(KeyName::Char('m'), false, false, false),
            key_ref_super(KeyName::Char('p'), false, false),
        ] {
            assert_eq!(
                match_keymap(&super_maps, k),
                None,
                "shell key {k:?} (super map)"
            );
        }
    }

    #[test]
    fn zoom_chord_spellings_cover_us_layout() {
        // CTX-0263: `+` is Shift+= on US, and the `+`-split syntax cannot
        // spell a literal `+` (`ctrl++` has an empty segment), so word
        // spellings must resolve to the same single-character chords.
        assert_eq!(
            Chord::parse("ctrl+equal").expect("equal").canonical(),
            "ctrl+="
        );
        assert_eq!(Chord::parse("ctrl+=").expect("=").canonical(), "ctrl+=");
        assert_eq!(
            Chord::parse("ctrl+plus").expect("plus").canonical(),
            "ctrl++"
        );
        assert_eq!(
            Chord::parse("ctrl+minus").expect("minus").canonical(),
            "ctrl+-"
        );
        assert_eq!(Chord::parse("ctrl+-").expect("-").canonical(), "ctrl+-");
        assert_eq!(
            Chord::parse("ctrl+shift+equal")
                .expect("shifted equal")
                .canonical(),
            "ctrl+shift+="
        );
        assert_eq!(
            Chord::parse("ctrl+shift+plus")
                .expect("shifted plus")
                .canonical(),
            "ctrl+shift++"
        );
        assert_eq!(Chord::parse("ctrl+0").expect("reset").canonical(), "ctrl+0");
        // Word spellings are case-insensitive like every other chord.
        assert_eq!(
            Chord::parse("Ctrl+Plus").expect("case").canonical(),
            "ctrl++"
        );
        // Bare zoom keys still go to the shell (schema rule).
        for raw in ["+", "-", "=", "0", "plus", "minus", "equal"] {
            assert!(Chord::parse(raw).is_err(), "bare {raw:?} must stay shell");
        }
        // A literal `ctrl++` stays rejected (empty segment) — users must
        // write `ctrl+plus`; the error names the field.
        let err = Chord::parse("ctrl++").unwrap_err();
        assert!(err.to_string().contains("keymaps[].chord"));
    }

    #[test]
    fn zoom_defaults_survive_mod_flip() {
        // CTX-0263: font zoom is Ctrl-held and mod-independent, so a
        // `super` mod flip must leave every zoom chord bound identically.
        for mod_key in [ModKey::Alt, ModKey::Super] {
            let maps = default_keymaps_with_mod(mod_key).expect("defaults valid");
            assert_eq!(
                match_keymap(&maps, key_ref(KeyName::Char('='), true, false, false)),
                Some(ChromeAction::IncreaseFontSize),
                "zoom-in survives mod {:?}",
                mod_key.canonical()
            );
            assert_eq!(
                match_keymap(&maps, key_ref(KeyName::Char('-'), true, false, false)),
                Some(ChromeAction::DecreaseFontSize),
                "zoom-out survives mod {:?}",
                mod_key.canonical()
            );
            assert_eq!(
                match_keymap(&maps, key_ref(KeyName::Char('0'), true, false, false)),
                Some(ChromeAction::ResetFontSize),
                "zoom-reset survives mod {:?}",
                mod_key.canonical()
            );
        }
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
    fn fkey_special_aliases_parse_and_canonicalize() {
        // CTX-0264: F-keys plus the six editing/navigation keys
        // (INS/DEL/HM/END/PU/PD) are first-class chord segments. Short
        // cap-label aliases resolve exactly like the long spellings and
        // canonicalize to the long names so merge identity is stable.
        let specials: &[(&[&str], &str)] = &[
            (&["insert", "ins", "INS", "Ins"], "insert"),
            (&["delete", "del", "DEL"], "delete"),
            (&["home", "hm", "HM", "Hm"], "home"),
            (&["end", "END"], "end"),
            (&["pageup", "pgup", "pu", "PU", "PgUp"], "pageup"),
            (&["pagedown", "pgdn", "pd", "PD", "PgDn"], "pagedown"),
        ];
        for (spellings, canonical_key) in specials {
            for spelling in *spellings {
                let raw = format!("alt+{spelling}");
                assert_eq!(
                    Chord::parse(&raw).expect("special parses").canonical(),
                    format!("alt+{canonical_key}"),
                    "chord {raw:?}"
                );
            }
        }
        // F-keys across the practical range plus the schema boundary.
        for n in [1u8, 2, 5, 9, 10, 11, 12, 13, 24, 35] {
            let raw = format!("alt+f{n}");
            assert_eq!(
                Chord::parse(&raw).expect("f-key parses").canonical(),
                format!("alt+f{n}"),
                "chord {raw:?}"
            );
        }
        assert_eq!(Chord::parse("ALT+F5").expect("case").canonical(), "alt+f5");
        assert_eq!(
            Chord::parse("Ctrl+Shift+F12").expect("mods").canonical(),
            "ctrl+shift+f12"
        );
        assert_eq!(
            Chord::parse("super+home").expect("super").canonical(),
            "super+home"
        );
        assert_eq!(
            Chord::parse("shift+alt+pd")
                .expect("alias+mods")
                .canonical(),
            "alt+shift+pagedown"
        );
        // Out-of-range F-keys fail closed with the known-key hint
        // (`alt+f` stays valid: it is the Alt+F letter chord).
        for raw in ["alt+f0", "alt+f36", "alt+f99", "alt+fx"] {
            let err = Chord::parse(raw).unwrap_err();
            assert!(
                err.to_string().contains("keymaps[].chord"),
                "must name field for {raw:?}: {err}"
            );
        }
        // Bare named keys parse (explicit user choice may bind them) but
        // stay unbound by default (shell-safety test below pins that).
        for raw in ["f5", "insert", "hm", "pu", "pd", "home", "end"] {
            Chord::parse(raw).expect("bare named parses");
        }
    }

    #[test]
    fn fkey_special_user_binds_resolve_and_audit_both_mods() {
        // CTX-0264 bindability + uniqueness audit: explicit Mod+F-key and
        // Mod+special binds resolve under both Alt and Super, each chord
        // owns exactly one action, no identity collides (with each other or
        // with the shipped defaults), and the Super rebound keeps the
        // explicit Alt spellings intact while the Super spellings stay free.
        // This task allocates NO new shipped defaults (CTX-0259 owns
        // Mod+Shift+Number, CTX-0265 owns Mod+backtick/Mod+?), so the
        // default count stays pinned at 79 under both mods (35 shipped
        // + 4 workspace + 4 resize + 16 arrow + 9 move + 7 zoom + 4 help).
        let entries: &[(&str, &str)] = &[
            ("alt+f1", "goto_split:left"),
            ("alt+f5", "goto_split:right"),
            ("alt+f12", "toggle_zoom"),
            ("alt+ins", "workspace_new"),
            ("alt+del", "workspace_close"),
            ("alt+hm", "workspace_prev"),
            ("alt+end", "workspace_next"),
            ("alt+pu", "scroll_page_up"),
            ("alt+pd", "scroll_page_down"),
        ];
        let mk_effective = |mod_key| EffectiveConfig {
            mod_key,
            keymaps: entries
                .iter()
                .map(|(chord, action)| KeymapEntry {
                    chord: (*chord).into(),
                    action: (*action).into(),
                    context: "global".into(),
                })
                .collect(),
            ..Default::default()
        };
        for mod_key in [ModKey::Alt, ModKey::Super] {
            let defaults = default_keymaps_with_mod(mod_key).expect("defaults valid");
            assert_eq!(
                defaults.len(),
                79,
                "no new shipped defaults under mod {:?}",
                mod_key
            );
            let maps = resolve_keymaps(&mk_effective(mod_key)).expect("resolves");
            assert_eq!(
                maps.len(),
                79 + entries.len(),
                "explicit binds append, never shadow, under mod {:?}",
                mod_key
            );
            let mut seen = std::collections::HashSet::new();
            for m in &maps {
                assert!(
                    seen.insert(m.id()),
                    "duplicate id {} under mod {:?}",
                    m.id(),
                    mod_key
                );
            }
        }
        // Alt map: every explicit chord matches its action.
        let alt_maps = resolve_keymaps(&mk_effective(ModKey::Alt)).expect("alt resolves");
        let want: &[(&str, ChromeAction)] = &[
            ("alt+f1", ChromeAction::GotoSplit(SplitDir::Left)),
            ("alt+f5", ChromeAction::GotoSplit(SplitDir::Right)),
            ("alt+f12", ChromeAction::ToggleZoom),
            ("alt+insert", ChromeAction::WorkspaceNew),
            ("alt+delete", ChromeAction::WorkspaceClose),
            ("alt+home", ChromeAction::WorkspacePrev),
            ("alt+end", ChromeAction::WorkspaceNext),
            ("alt+pageup", ChromeAction::ScrollPageUp),
            ("alt+pagedown", ChromeAction::ScrollPageDown),
        ];
        for (chord, action) in want {
            let parsed = Chord::parse(chord).expect("audit chord parses");
            let keyref = KeyRef {
                key: parsed.key,
                ctrl: parsed.ctrl,
                alt: parsed.alt,
                shift: parsed.shift,
                super_held: parsed.super_held,
            };
            assert_eq!(
                match_keymap(&alt_maps, keyref),
                Some(*action),
                "chord {chord:?}"
            );
        }
        // Super map: explicit Alt spellings coexist with the rebound
        // defaults; the Super-spelled twins stay unbound (free allocation
        // space, no shadow).
        let super_maps = resolve_keymaps(&mk_effective(ModKey::Super)).expect("super resolves");
        assert_eq!(
            match_keymap(&super_maps, key_ref(KeyName::F(5), false, true, false)),
            Some(ChromeAction::GotoSplit(SplitDir::Right)),
            "explicit alt+f5 survives super flip"
        );
        assert_eq!(
            match_keymap(
                &super_maps,
                KeyRef {
                    key: KeyName::F(5),
                    ctrl: false,
                    alt: false,
                    shift: false,
                    super_held: true,
                }
            ),
            None,
            "super+f5 stays free under super mod"
        );
        assert_eq!(
            match_keymap(&super_maps, key_ref(KeyName::Home, false, true, false)),
            Some(ChromeAction::WorkspacePrev),
            "explicit alt+home survives super flip"
        );
    }

    #[test]
    fn bare_fkey_special_presses_stay_shell_both_mods() {
        // CTX-0264 shell-safety: bare F-keys and bare INS/DEL/HM/END/PU/PD
        // are never chrome-owned under either default map, so they fall
        // through the intercept to terminal encoding (platform xterm table)
        // unchanged. Binding them requires an explicit user chord.
        for mod_key in [ModKey::Alt, ModKey::Super] {
            let maps = default_keymaps_with_mod(mod_key).expect("defaults valid");
            for n in 1..=35u8 {
                let bare = KeyRef {
                    key: KeyName::F(n),
                    ctrl: false,
                    alt: false,
                    shift: false,
                    super_held: false,
                };
                assert_eq!(
                    match_keymap(&maps, bare),
                    None,
                    "bare f{n} is shell under mod {:?}",
                    mod_key
                );
            }
            for key in [
                KeyName::Insert,
                KeyName::Delete,
                KeyName::Home,
                KeyName::End,
                KeyName::PageUp,
                KeyName::PageDown,
            ] {
                let bare = KeyRef {
                    key,
                    ctrl: false,
                    alt: false,
                    shift: false,
                    super_held: false,
                };
                assert_eq!(
                    match_keymap(&maps, bare),
                    None,
                    "bare {key:?} is shell under mod {:?}",
                    mod_key
                );
            }
        }
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

    #[test]
    fn toggle_help_action_parses_and_canonicalizes() {
        // CTX-0265: `toggle_help` (alias `show_help`) is a first-class
        // chrome action; the canonical spelling is the long name so merge
        // identity and popup rows stay stable.
        assert_eq!(
            ChromeAction::parse("toggle_help").expect("parses"),
            ChromeAction::ToggleHelp
        );
        assert_eq!(
            ChromeAction::parse("SHOW_HELP").expect("alias parses"),
            ChromeAction::ToggleHelp
        );
        assert_eq!(
            ChromeAction::ToggleHelp.canonical(),
            "toggle_help".to_string()
        );
        assert!(
            KNOWN_ACTIONS_HINT.contains("toggle_help"),
            "fail-closed hint must name the action"
        );
    }

    #[test]
    fn help_chords_resolve_to_toggle_help_both_mods() {
        // CTX-0265: Mod+backtick plus the Mod+? shifted-symbol spellings
        // toggle the help popup under both Alt and Super. `?` physically
        // carries Shift and shifted-symbol reporting varies by platform
        // (CTX-0263 precedent), so the plain, shifted-`?`, and shifted-`/`
        // spellings all bind the one action; the Super flip re-spells every
        // entry through the Mod slot.
        let alt_maps = default_keymaps_with_mod(ModKey::Alt).expect("alt defaults valid");
        for chord in ["alt+`", "alt+?", "alt+shift+?", "alt+shift+/"] {
            let parsed = Chord::parse(chord).expect("help chord parses");
            let keyref = KeyRef {
                key: parsed.key,
                ctrl: parsed.ctrl,
                alt: parsed.alt,
                shift: parsed.shift,
                super_held: parsed.super_held,
            };
            assert_eq!(
                match_keymap(&alt_maps, keyref),
                Some(ChromeAction::ToggleHelp),
                "chord {chord:?}"
            );
        }
        let super_maps = default_keymaps_with_mod(ModKey::Super).expect("super defaults valid");
        for chord in ["super+`", "super+?", "super+shift+?", "super+shift+/"] {
            let parsed = Chord::parse(chord).expect("flipped chord parses");
            let keyref = KeyRef {
                key: parsed.key,
                ctrl: parsed.ctrl,
                alt: parsed.alt,
                shift: parsed.shift,
                super_held: parsed.super_held,
            };
            assert_eq!(
                match_keymap(&super_maps, keyref),
                Some(ChromeAction::ToggleHelp),
                "chord {chord:?}"
            );
        }
        // The old Alt spellings are unbound under Super (back to the shell),
        // exactly like every other Mod chord.
        assert_eq!(
            match_keymap(&super_maps, key_ref(KeyName::Char('`'), false, true, false)),
            None,
            "alt+` unbound under super mod"
        );
    }

    #[test]
    fn bare_backtick_and_question_stay_shell() {
        // CTX-0265 shell-safety: bare backtick / `?` (and their shifted
        // shells) are never chrome-owned under either default map, so shell
        // prompts, Markdown, and `help?` typing keep working.
        for mod_key in [ModKey::Alt, ModKey::Super] {
            let maps = default_keymaps_with_mod(mod_key).expect("defaults valid");
            for (key, shift) in [
                (KeyName::Char('`'), false),
                (KeyName::Char('?'), false),
                (KeyName::Char('?'), true),
                (KeyName::Char('/'), false),
            ] {
                let bare = KeyRef {
                    key,
                    ctrl: false,
                    alt: false,
                    shift,
                    super_held: false,
                };
                assert_eq!(
                    match_keymap(&maps, bare),
                    None,
                    "bare {key:?} shift={shift} is shell under mod {mod_key:?}"
                );
            }
        }
    }

    #[test]
    fn help_rows_derive_from_live_registry() {
        // CTX-0265 registry-generated content proof: rows render the live
        // resolved table (canonical chord + canonical action, table order),
        // so adding a chord appears in the popup with no second source.
        let maps = default_keymaps().expect("defaults valid");
        let rows = help_rows_from_keymaps(&maps);
        assert_eq!(rows.len(), maps.len(), "one row per binding");
        assert!(
            rows.iter().any(|r| r == "alt+h  goto_split:left"),
            "navigate row present: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r == "alt+`  toggle_help"),
            "backtick help row present: {rows:?}"
        );
        // Adding a chord to the registry adds a popup row by construction.
        let extended = EffectiveConfig {
            keymaps: vec![KeymapEntry {
                chord: "alt+e".into(),
                action: "open_composer".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        let maps2 = resolve_keymaps(&extended).expect("resolves");
        let rows2 = help_rows_from_keymaps(&maps2);
        assert_eq!(rows2.len(), maps2.len());
        assert!(
            rows2.iter().any(|r| r == "alt+e  open_composer"),
            "added chord listed: {rows2:?}"
        );
        // Super flip re-spells every Mod row (popup stays in sync with what
        // is actually bound).
        let flipped = EffectiveConfig {
            mod_key: ModKey::Super,
            ..Default::default()
        };
        let maps3 = resolve_keymaps(&flipped).expect("resolves");
        let rows3 = help_rows_from_keymaps(&maps3);
        assert!(
            rows3.iter().any(|r| r == "super+`  toggle_help"),
            "super spelling listed: {rows3:?}"
        );
        assert!(
            rows3.iter().any(|r| r == "shift+super+?  toggle_help"),
            "super ? spelling listed: {rows3:?}"
        );
        assert!(
            !rows3.iter().any(|r| r.starts_with("alt+")),
            "no Alt spellings survive the flip: {rows3:?}"
        );
    }
}
