//! Keymap schema: chords and chrome actions (CTX-0153).
//!
//! Config-file keymaps drive all chrome keys (ghostty-style, read-only
//! reference: `recording/references/ghostty` plus the user's
//! `~/.config/ghostty/keybinds.conf`). The single-owner rule lives here as
//! data and in `bitty-terminal` as enforcement: a key event that matches a bound
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
//!   `new_split:<left|right|up|down>`, `new_panel`,
//!   `resize_split:<left|right|up|down>`,
//!   `close_view` (aliases `close_surface`, `close_panel`, `exit_panel`),
//!   `toggle_zoom` (aliases `toggle_split_zoom`, `suspend_panel`, `detach_panel`),
//!   `focus_next`, `focus_prev`, `focus:<1..=256>`,
//!   `copy_to_clipboard`, `paste_from_clipboard`,
//!   `scroll_page_up`, `scroll_page_down`, `increase_font_size` (aliases
//!   `zoom_in`, `font_zoom_in`), `decrease_font_size` (aliases `zoom_out`,
//!   `font_zoom_out`), `reset_font_size` (aliases `zoom_reset`,
//!   `font_zoom_reset`; CTX-0263 per-window font zoom), `toggle_help`
//!   (alias `show_help`; CTX-0265 help popup, defaults `alt+`` plus the
//!   `alt+?` shifted-symbol spellings), `open_composer` (Command Composer
//!   manual open, CTX-0227: suggested chord `alt+e`; never bound by default
//!   so Normal Mode stays byte-identical until the user opts in; CTX-0723 /
//!   #982: opens the live session with input routing plus PTY submit, the
//!   editor request stays a loud routing flag),
//!   `fold_toggle` (alias `toggle_fold`), `fold_expand` (alias
//!   `expand_fold`), `fold_collapse` (alias `collapse_fold`; CTX-0723 /
//!   #980: latest-command fold verbs, manual bind only, never in defaults),
//!   `workspace_new`, `workspace_close`, `workspace_prev`, `workspace_next`,
//!   `workspace_last`, `workspace_next_occupied`, `workspace_prev_occupied`,
//!   `workspace_focus:<1..=16>`, `workspace_move:<1..=16>`
//!   (CTX-0257 workspace ops entry per DEC-0034 plus CTX-0259 move, rechorded
//!   CTX-0766: `alt+n` opens a new panel, `alt+t` opens a new workspace):
//!   `alt+n` new panel, `alt+t` new, `alt+d`/`alt+q` close pane/view with confirm,
//!   `alt+w` close workspace with kill-confirm,
//!   `alt+-`/`alt+=` prev/next (`=` is the unshifted DEC `+`), `alt+tab`
//!   last-used, `alt+[`/`alt+]` next/prev occupied with wrap (CTX-1100 #1904),
//!   `alt+1..=9` jump to workspace N,
//!   `shift+alt+1..=9` move focused window to workspace N),
//!   `toggle_floating` (alias `floating_toggle`; CTX-0962 / #1695: focused
//!   panel tiled/floating toggle, default `alt+a`, `global` only),
//!   `toggle_pinned` (alias `pinned_toggle`; CTX-1083 follow-up to CTX-1077
//!   #1757: focused panel pinned/sticky-float toggle, manual bind only,
//!   never in defaults).
//!   W-144 (CTX-0937): the search/copy-mode policy actions retired with
//!   the Core policy; their namespace moves to W-138 with the plugins
//!   (CTX-0003), so the retired spellings fail closed as unknown here.
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
//! workspace entry (CTX-0257, rechorded CTX-0766): `alt+t` new workspace,
//! `alt+1..=9` jump to
//! workspace N, `alt+-`/`alt+=` prev/next, `alt+tab` last-used,
//! `alt+[`/`alt+]` next/prev occupied with wrap (CTX-1100 #1904),
//! `alt+d`/`alt+q`
//! close pane with confirm, `alt+w` close workspace with kill-confirm,
//! plus the Hyprland-style panel entry (CTX-0838 #1441): `alt+n`
//! `new_panel` (adaptive dwindle axis, new-second, focus follows), plus the
//! floating-toggle entry (CTX-0962 #1695): `alt+a` `toggle_floating`
//! (Mod-aware via the `alt` slot; fish reserves `alt+v` so `Mod+v` is out).
//! `alt+w` and `alt+1..=9` previously drove pane ops (`close_view`,
//! `focus:<n>`); those actions stay parseable and user-bindable but are
//! no longer bound by default — workspace numbers won the Alt slot per
//! the owner spec, panes navigate spatially (`goto_split`,
//! `focus_next`/`focus_prev`). The CTX-0265 help popup (009 §which-key:
//! floating overlay listing every bound shortcut, generated from the live
//! registry) toggles on `alt+`` plus the `alt+?` shifted-symbol spellings
//! (`alt+?`/`alt+shift+?`/`alt+shift+/`: shifted-symbol reporting varies by
//! platform, CTX-0263 precedent). Plain `Tab`, bare arrows, letters, and digits
//! are deliberately unbound so they reach the shell.
//!
//! # Shifted symbols (physical base key, issue #1446)
//!
//! A press carries the *modifier-applied* logical character, so a physical
//! `Mod+Shift+2` — the accepted DEC-0034 workspace-move gesture, spelled
//! `shift+alt+2` — arrives as `@` plus a held Shift and could never equal the
//! base-key chord by exact equality. Matching therefore tries the reported
//! spelling first and falls back to the physical base-key spelling of a
//! shift-held shifted symbol ([`KeyRef::unshifted_base`],
//! [`shifted_symbol_base`]) at the dispatch site (`bitty-terminal`): the gesture
//! resolves, the exact spellings keep their precedence, and the symbol no
//! longer leaks to the PTY. Ghostty's key events carry the same
//! `unshifted_codepoint` and its character keybinds match on it, the same
//! rule; the shipped pair table is the US reference layout's.
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
///
/// Sized for the longest qualified invocation (`command:` plus a
/// `owner:command` name at the host ceiling of 128 + 1 + 128 bytes, with
/// headroom): every other action stays far below this, and anything longer
/// is a malformed entry, never a silent truncation.
pub const MAX_ACTION_LEN: usize = 320;

/// Maximum bytes of one `command:<qualified>` invocation target (CTX-1035).
///
/// Mirrors the host qualified-name ceiling (plugin id at most 128 bytes
/// plus one `:` separator plus a command id of at most 128 bytes).
pub const MAX_INVOKE_COMMAND_LEN: usize = 257;

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
    /// Validate a parsed value (total: only `alt`/`super` are representable;
    /// the exhaustive match keeps coverage explicit so a future variant
    /// forces a review of chrome-map and project-layer policy).
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Alt | Self::Super => Ok(()),
        }
    }

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
/// [`InvokeCommand`](Self::InvokeCommand) carries an owned qualified name, so
/// the enum is [`Clone`] but not [`Copy`]; call sites that inspect an action
/// and then run it clone first.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChromeAction {
    /// Move focus spatially (`goto_split:left`, ...).
    GotoSplit(SplitDir),
    /// Split the focused pane (`new_split:right`, ...).
    NewSplit(SplitDir),
    /// Open a new panel with Hyprland-dwindle semantics (`new_panel`).
    ///
    /// CTX-0838 (#1441): the Mod+N owner default. Split axis follows the
    /// focused leaf's cell allocation via the `smart_split_axis` heuristic
    /// (wide splits side-by-side, tall stacks, square ties break
    /// side-by-side), mirroring Hyprland's
    /// `splitTop = height * split_width_multiplier > width` at the default
    /// multiplier `1.0`. Placement is always new-second (right/below) and
    /// focus follows the fresh pane. Explicit `new_split:<dir>` keeps its
    /// fixed axis for directional splits; Niri-ribbon ordering is out of
    /// scope.
    NewPanel,
    /// Nudge the enclosing split ratio (`resize_split:left`, ...).
    /// The per-keypress delta is `layout.resize_step` (CTX-0963, issue
    /// #1697; default `0.05`).
    ResizeSplit(SplitDir),
    /// Close the focused pane (`close_view`, aliases `close_surface`, `close_panel`, `exit_panel`).
    ///
    /// Default bindings: `alt+d`, `alt+q` (issues #1444, #1776). Respects the `close_confirm`
    /// mode: `when_busy` (default) prompts when a foreground job runs,
    /// `always` prompts unconditionally, `never` closes immediately.
    /// The first gesture arms a confirmation; repeat to confirm, `Esc` to cancel.
    ///
    /// CTX-1039 (issue #1843): closing the last panel of one workspace
    /// never exits while any workspace still holds a panel. The emptied
    /// workspace stays selected with its tile session-less; only the last
    /// panel in the whole window is the window-close gesture (issue #1783).
    CloseView,
    /// Toggle single-pane zoom (`toggle_zoom`, aliases `toggle_split_zoom`, `suspend_panel`, `detach_panel`).
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
    /// open chord can never shadow bare shell typing. CTX-0723 / #982: the
    /// app opens the live session and routes input while open (submit
    /// writes one frame to the focused PTY); the external-editor request
    /// stays a loud routing flag (no terminal fd to lend `$EDITOR` in the
    /// GUI root).
    OpenComposer,
    /// Flip the latest command block's fold membership (CTX-0723, #980).
    ///
    /// Present-path verb over the live [`FoldState`](bitty_rich::blocks::FoldState):
    /// the app resolves the focused view's latest semantic command block
    /// and toggles it. Manual bind only, never in [`DEFAULT_KEYMAPS`]
    /// (same byte-identical discipline as [`Self::OpenComposer`]); the
    /// user opts in with e.g. `{ chord = "alt+z", action = "fold_toggle" }`.
    FoldToggle,
    /// Ensure the latest command block is unfolded, idempotent (CTX-0723,
    /// #980). Manual bind only, never in [`DEFAULT_KEYMAPS`].
    FoldExpand,
    /// Ensure the latest command block is folded, idempotent and fail-closed
    /// at the fold cap (CTX-0723, #980). Manual bind only, never in
    /// [`DEFAULT_KEYMAPS`].
    FoldCollapse,
    /// Create a fresh workspace and switch to it (`workspace_new`, CTX-0257
    /// DEC-0034 entry, default chord `alt+t` since CTX-0766 — previously
    /// `alt+n`, now the new-panel chord). The new workspace starts as
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
    /// Switch to the next occupied workspace rightward with wrap
    /// (`workspace_next_occupied`, default `alt+[` per #1904, CTX-1100).
    ///
    /// Occupied means at least one live pane session or ownership of the
    /// primary shell (see `Runtime::workspace_is_occupied`); session-less
    /// tiles are skipped.
    /// From the active index lands on the next occupied slot rightward,
    /// wrapping to the leftmost occupied at the rightmost. Fewer than two
    /// occupied workspaces is a fail-closed no-op (loud warning, never a
    /// panic, never a kill).
    WorkspaceNextOccupied,
    /// Switch to the previous occupied workspace leftward with wrap
    /// (`workspace_prev_occupied`, default `alt+]` per #1904, CTX-1100).
    ///
    /// Exact mirror of [`Self::WorkspaceNextOccupied`].
    WorkspacePrevOccupied,
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
    /// Swap the current workspace with workspace N or move to workspace N
    /// if target does not exist (`workspace_swap:<1..=16>`, defaults
    /// `ctrl+shift+alt+1..=9`, CTX-0945).
    WorkspaceSwap(u64),
    // W-144 (CTX-0937): search/copy-mode policy actions retired with the
    // Core policy; their namespace moves to W-138 with the plugins
    // (search@e65bf83, copy-mode@7410a3e), so the retired spellings fail
    // closed as unknown here.
    /// Jump the focused viewport to the previous shell prompt
    /// (`jump_to_prompt:prev`, CTX-0952 issue #1670).
    ///
    /// One prompt per gesture over the retained `OSC 133;A` zone anchors;
    /// the target lands at the viewport top. Fail-closed no-op with no
    /// resolvable prompt above (never a panic, never a mis-jump).
    /// Default `shift+alt+pageup`: the `Mod+Shift+Up/Down` shape the issue
    /// suggests is unavailable — `shift+alt+arrows` already creates splits
    /// and `shift+ctrl+arrows` already resizes under the single-owner rule
    /// (ghostty's Linux `shift+ctrl+arrows` default collides here) — so the
    /// page keys carry the gesture with the Mod slot for the Super flip.
    JumpToPromptPrev,
    /// Jump the focused viewport to the next shell prompt
    /// (`jump_to_prompt:next`, CTX-0952 issue #1670).
    ///
    /// Mirror of [`Self::JumpToPromptPrev`] toward live. Default
    /// `shift+alt+pagedown` (same arrow-collision rationale).
    JumpToPromptNext,
    /// Select exactly the last command's output (`select_command_output`,
    /// CTX-0952 issue #1670).
    ///
    /// Covers the rows between the last `OSC 133;C` mark and the next
    /// prompt (ghostty `selectOutput` shape); empty or absent marks select
    /// nothing. Default `alt+o` (mnemonic: output; carries the Mod slot).
    SelectCommandOutput,
    /// Toggle the command palette overlay (`toggle_palette`, CTX-0647 issue #1003).
    ///
    /// Manual open only: this action is never in [`DEFAULT_KEYMAPS`], so a
    /// fresh config keeps Normal Mode byte-identical and the palette stays
    /// bundled-disabled (OQ-053: palette is the independent first-party
    /// package `bitty-terminal/palette`, no PanelRegistry host in the app
    /// yet). The user opts in with
    /// `{ chord = "ctrl+shift+p", action = "toggle_palette" }` (suggested
    /// chord; `ctrl+shift+p` is free in the shipped map); the
    /// single-character schema rule already forces a modifier, so the open
    /// chord can never shadow bare shell typing. Until the panel host
    /// lands the app consumes a bound chord as inert with a loud warning
    /// (no overlay, no routing change) — the bundled-disabled decision is
    /// kept, and the entry exists as forward-compat wiring.
    TogglePalette,
    /// Toggle the focused panel between tiled and floating
    /// (`toggle_floating`, alias `floating_toggle`; CTX-0962 issue #1695).
    ///
    /// Present-path verb over the live layout: the app resolves the focused
    /// view and flips its [`PresentationMode`](bitty_ui::PresentationMode)
    /// through the `bitty.workspace:floating-toggle` primitive
    /// (`bitty_ui::presentation::toggle_floating`). Default chord `alt+a`
    /// (both `alt+a`/`alt+v` were free; fish reserves `alt+v` for `$EDITOR`
    /// so `Mod+v` is unusable there). The chord carries the Mod slot so a
    /// Super flip rebinds it to `super+a`. User-overridable via an explicit
    /// `keymaps` entry with the same `context + chord` identity.
    ToggleFloating,
    /// Toggle the focused panel's pinned (sticky-float) state
    /// (`toggle_pinned`, alias `pinned_toggle`; CTX-1083 follow-up to
    /// CTX-1077 issue #1757).
    ///
    /// Present-path verb over the window-global pinned store: the app
    /// resolves the focused view and routes it through the
    /// `bitty.workspace:pin-toggle` primitive (`Runtime::apply_pin_command`
    /// into `toggle_pinned`). Pinning detaches a tiled or floating leaf so
    /// it presents over every workspace; toggling again returns the still
    /// floating panel to the active workspace. Fail-closed with a loud
    /// warning and no state change when nothing is focused, the id is
    /// unknown, the mode is not pinnable, or the pin would strand a
    /// single-leaf layout. Manual bind only, never in [`DEFAULT_KEYMAPS`]
    /// (same byte-identical discipline as [`Self::TogglePalette`]); the user
    /// opts in with e.g. `{ chord = "alt+p", action = "toggle_pinned" }`.
    TogglePinned,
    /// Invoke one registered plugin command (`command:<owner:command>`,
    /// alias `invoke_command:<owner:command>`; CTX-1035 issue #1829).
    ///
    /// The generic host-mediated invocation path behind palette selection:
    /// the app parses the qualified name and routes it through the plugin
    /// runtime's deny-by-default dispatch (undeclared and foreign-qualified
    /// names refused, args validated against the callee schema, failures
    /// contained — never a host crash). Keybindings carry no arguments, so
    /// the invocation validates as the empty object `{}`; commands whose
    /// args schema requires properties (e.g. a mandatory `filter`) refuse
    /// the chord loudly. Manual bind only, never in [`DEFAULT_KEYMAPS`]
    /// (same byte-identical discipline as [`Self::OpenComposer`]); the user
    /// opts in with e.g.
    /// `{ chord = "ctrl+alt+p", action = "command:bitty-featured.devtools:plugins" }`.
    InvokeCommand(String),
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
            "new_panel" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::NewPanel)
            }
            "resize_split" => {
                let dir = require_dir_arg(arg, trimmed)?;
                Ok(Self::ResizeSplit(dir))
            }
            "close_view"
            | "close_surface"
            | "close_panel"
            | "exit_panel"
            | "close_focused_panel" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::CloseView)
            }
            "toggle_zoom"
            | "toggle_split_zoom"
            | "suspend_panel"
            | "detach_panel"
            | "suspend_focused_panel"
            | "detach_focused_panel" => {
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
            "fold_toggle" | "toggle_fold" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::FoldToggle)
            }
            "fold_expand" | "expand_fold" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::FoldExpand)
            }
            "fold_collapse" | "collapse_fold" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::FoldCollapse)
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
            "workspace_next_occupied"
            | "workspace_cycle_next"
            | "workspace_cycle_next_occupied" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::WorkspaceNextOccupied)
            }
            "workspace_prev_occupied"
            | "workspace_cycle_prev"
            | "workspace_cycle_prev_occupied" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::WorkspacePrevOccupied)
            }
            "workspace_focus" => {
                let n = require_workspace_index(arg, trimmed)?;
                Ok(Self::WorkspaceFocus(n))
            }
            "workspace_move" | "workspace_move_window" => {
                let n = require_workspace_index(arg, trimmed)?;
                Ok(Self::WorkspaceMove(n))
            }
            "workspace_swap" => {
                let n = require_workspace_index(arg, trimmed)?;
                Ok(Self::WorkspaceSwap(n))
            }
            "jump_to_prompt" => match arg {
                Some("prev") | Some("previous") | Some("up") => Ok(Self::JumpToPromptPrev),
                Some("next") | Some("down") => Ok(Self::JumpToPromptNext),
                _ => Err(ConfigError::validation(
                    "keymaps[].action",
                    format!("action '{trimmed}' needs a direction (e.g. 'jump_to_prompt:prev')"),
                )),
            },
            "jump_to_prompt_prev" | "prompt_prev" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::JumpToPromptPrev)
            }
            "jump_to_prompt_next" | "prompt_next" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::JumpToPromptNext)
            }
            "select_command_output" | "select_output" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::SelectCommandOutput)
            }
            "toggle_palette" | "open_palette" | "palette_toggle" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::TogglePalette)
            }
            "toggle_floating" | "floating_toggle" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::ToggleFloating)
            }
            "toggle_pinned" | "pinned_toggle" => {
                reject_arg(arg, trimmed)?;
                Ok(Self::TogglePinned)
            }
            "command" | "invoke_command" | "run_command" => {
                let target = require_invoke_command(arg, trimmed)?;
                Ok(Self::InvokeCommand(target))
            }
            _ => Err(ConfigError::validation(
                "keymaps[].action",
                format!("unknown action '{trimmed}'; {KNOWN_ACTIONS_HINT}"),
            )),
        }
    }

    /// Canonical action spelling (`goto_split:left`, `close_view`, ...).
    #[must_use]
    pub fn canonical(&self) -> String {
        match self {
            Self::GotoSplit(d) => format!("goto_split:{}", d.canonical()),
            Self::NewSplit(d) => format!("new_split:{}", d.canonical()),
            Self::NewPanel => "new_panel".to_string(),
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
            Self::FoldToggle => "fold_toggle".to_string(),
            Self::FoldExpand => "fold_expand".to_string(),
            Self::FoldCollapse => "fold_collapse".to_string(),
            Self::WorkspaceNew => "workspace_new".to_string(),
            Self::WorkspaceClose => "workspace_close".to_string(),
            Self::WorkspacePrev => "workspace_prev".to_string(),
            Self::WorkspaceNext => "workspace_next".to_string(),
            Self::WorkspaceLast => "workspace_last".to_string(),
            Self::WorkspaceNextOccupied => "workspace_next_occupied".to_string(),
            Self::WorkspacePrevOccupied => "workspace_prev_occupied".to_string(),
            Self::WorkspaceFocus(n) => format!("workspace_focus:{n}"),
            Self::WorkspaceMove(n) => format!("workspace_move:{n}"),
            Self::WorkspaceSwap(n) => format!("workspace_swap:{n}"),
            Self::JumpToPromptPrev => "jump_to_prompt:prev".to_string(),
            Self::JumpToPromptNext => "jump_to_prompt:next".to_string(),
            Self::SelectCommandOutput => "select_command_output".to_string(),
            Self::TogglePalette => "toggle_palette".to_string(),
            Self::ToggleFloating => "toggle_floating".to_string(),
            Self::TogglePinned => "toggle_pinned".to_string(),
            Self::InvokeCommand(qualified) => format!("command:{qualified}"),
        }
    }
}

/// Hint listing the accepted action vocabulary.
///
/// W-144 (CTX-0937): the search/copy-mode policy actions retired with the
/// Core policy; their namespace moves to W-138 with the plugins (CTX-0003),
/// so the retired spellings fail closed as unknown here.
const KNOWN_ACTIONS_HINT: &str = "expected one of goto_split:<left|right|up|down>, new_split:<left|right|up|down>, new_panel, resize_split:<left|right|up|down>, close_view, toggle_zoom, toggle_help, focus_next, focus_prev, focus:<1..=256>, copy_to_clipboard, paste_from_clipboard, scroll_page_up, scroll_page_down, increase_font_size, decrease_font_size, reset_font_size, open_composer, fold_toggle, fold_expand, fold_collapse, workspace_new, workspace_close, workspace_prev, workspace_next, workspace_last, workspace_next_occupied, workspace_prev_occupied, workspace_focus:<1..=16>, workspace_move:<1..=16>, workspace_swap:<1..=16>, jump_to_prompt:<prev|next>, select_command_output, toggle_palette, toggle_floating, toggle_pinned, command:<owner:command>";

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

/// Require a `command:<owner:command>` invocation target (CTX-1035).
///
/// The target after the `command:` head is itself qualified, so the raw
/// action holds two colons (e.g. `command:bitty-featured.devtools:plugins`);
/// the head/arg split already consumed the first one. Deny-by-default at
/// parse: missing, empty-sided, multi-colon, over-long, or NUL/space-bearing
/// targets fail closed with the action grammar error (ownership and liveness
/// stay runtime checks at dispatch).
fn require_invoke_command(arg: Option<&str>, raw: &str) -> Result<String, ConfigError> {
    let invalid = || {
        ConfigError::validation(
            "keymaps[].action",
            format!(
                "action '{raw}' needs a qualified command 'owner:command' (e.g. 'command:bitty-featured.devtools:plugins')"
            ),
        )
    };
    let target = match arg {
        Some(target) if !target.is_empty() => target,
        _ => return Err(invalid()),
    };
    if target.len() > MAX_INVOKE_COMMAND_LEN || target.contains('\0') || target.contains(' ') {
        return Err(invalid());
    }
    match target.split_once(':') {
        Some((owner, command))
            if !owner.is_empty() && !command.is_empty() && !command.contains(':') =>
        {
            Ok(target.to_string())
        }
        _ => Err(invalid()),
    }
}

/// Base key of a shift-held shifted symbol on the shipped US reference
/// layout, or `None` when `c` already is a base key.
///
/// Key events carry the *modifier-applied* logical character (winit
/// `Key::Character` has Shift and the layout already applied), so a physical
/// `Shift+2` arrives as `@` and a physical `Shift+/` as `?`. The shipped
/// defaults spell physical gestures with the base key (`shift+alt+1..=9` for
/// `Mod+Shift+Number`, DEC-0034), so matching recovers the base key through
/// this table instead of duplicating every shifted-symbol spelling.
/// [`KeyRef::unshifted_base`] applies it. Pair set: the US reference layout's
/// digit and punctuation rows (`!@#$%^&*()`, `~_+{}|:"<>?`).
///
/// Layout scope: the pairs are the shipped reference layout's; a layout that
/// reports a different symbol for the same physical key (for example a UK
/// `Shift+2` = `"`) is not covered here — the reported spelling keeps its own
/// exact-match path, so nothing regresses for those bindings.
#[must_use]
pub const fn shifted_symbol_base(c: char) -> Option<char> {
    match c {
        '!' => Some('1'),
        '@' => Some('2'),
        '#' => Some('3'),
        '$' => Some('4'),
        '%' => Some('5'),
        '^' => Some('6'),
        '&' => Some('7'),
        '*' => Some('8'),
        '(' => Some('9'),
        ')' => Some('0'),
        '~' => Some('`'),
        '_' => Some('-'),
        '+' => Some('='),
        '{' => Some('['),
        '}' => Some(']'),
        '|' => Some('\\'),
        ':' => Some(';'),
        '"' => Some('\''),
        '<' => Some(','),
        '>' => Some('.'),
        '?' => Some('/'),
        _ => None,
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

    /// Physical base-key spelling of this press, or `None` when no shift-held
    /// shifted symbol applies.
    ///
    /// Returns the same press with [`Self::key`] replaced by
    /// [`shifted_symbol_base`]'s base key when Shift is held and the reported
    /// key is a shifted symbol (`@` -> `Char('2')`), keeping every modifier
    /// bit: the physical gesture `Shift+Mod+2` reports `@` + `shift` + the
    /// Mod, and the base-key spelling `Char('2')` + `shift` + the Mod is the
    /// one the accepted `shift+alt+2` chord (DEC-0034 workspace move) can
    /// match. Exact matching stays untouched ([`Self::matches`]); callers try
    /// the reported spelling first and consult this second, so every explicit
    /// binding keeps its precedence.
    #[must_use]
    pub fn unshifted_base(&self) -> Option<Self> {
        if !self.shift {
            return None;
        }
        let KeyName::Char(c) = self.key else {
            return None;
        };
        let base = shifted_symbol_base(c)?;
        Some(Self {
            key: KeyName::Char(base),
            ..*self
        })
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
/// CTX-0962 adds the Mod-aware `alt+a` floating toggle
/// (`toggle_floating`; Super flip rebinds to `super+a`).
/// Issue #1776 adds the Mod-aware `alt+q` close view/panel
/// (`close_view`; Super flip rebinds to `super+q`).
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
    // Mod+Ctrl+Shift+Number swap workspace or move workspace to target (CTX-0945):
    // `ctrl+shift+alt+1..=9` swaps current workspace with target N or moves to N.
    ("ctrl+shift+alt+1", "workspace_swap:1"),
    ("ctrl+shift+alt+2", "workspace_swap:2"),
    ("ctrl+shift+alt+3", "workspace_swap:3"),
    ("ctrl+shift+alt+4", "workspace_swap:4"),
    ("ctrl+shift+alt+5", "workspace_swap:5"),
    ("ctrl+shift+alt+6", "workspace_swap:6"),
    ("ctrl+shift+alt+7", "workspace_swap:7"),
    ("ctrl+shift+alt+8", "workspace_swap:8"),
    ("ctrl+shift+alt+9", "workspace_swap:9"),
    ("alt+u", "scroll_page_down"),
    ("alt+i", "scroll_page_up"),
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
    ("alt+d", "close_view"),
    // Issue #1776: default Mod+q close view/panel with confirm (rebindable
    // via ModKey to super+q; user-overridable via keymaps).
    ("alt+q", "close_view"),
    ("alt+w", "workspace_close"),
    ("alt+m", "toggle_zoom"),
    ("alt+f", "toggle_zoom"),
    ("ctrl+shift+c", "copy_to_clipboard"),
    ("ctrl+shift+v", "paste_from_clipboard"),
    ("alt+z", "toggle_zoom"),
    // Owner default: `alt+n` opens a new panel with Hyprland-dwindle
    // semantics (CTX-0838 #1441: adaptive axis from the focused leaf's
    // allocation — wide splits side-by-side, tall stacks, square ties break
    // side-by-side; new panel goes right/below, focus follows it).
    // Explicit `new_split:<dir>` keeps its fixed axis for directional
    // splits. `alt+t` opens a fresh workspace (CTX-0766).
    ("alt+n", "new_panel"),
    // CTX-0962 (issue #1695): `alt+a` toggles the focused panel between
    // tiled and floating (`bitty.workspace:floating-toggle`). Both `alt+a`
    // and `alt+v` were free; fish reserves `alt+v` for `$EDITOR` so `Mod+v`
    // is unusable there. Carries the Mod slot so a Super flip rebinds to
    // `super+a`. User-overridable via the `context + chord` merge rule.
    ("alt+a", "toggle_floating"),
    ("alt+t", "workspace_new"),
    ("alt+-", "workspace_prev"),
    ("alt+=", "workspace_next"),
    ("alt+tab", "workspace_last"),
    // CTX-1100 (#1904): cycle occupied workspaces with wrap. Both chords
    // were free in the shipped map (no conflict); they carry the Mod slot
    // so a Super flip rebinds to super+[/]. Per the owner verbatim intent,
    // `[` moves rightward (next occupied) and `]` mirrors leftward (prev).
    ("alt+[", "workspace_next_occupied"),
    ("alt+]", "workspace_prev_occupied"),
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
    // W-144 (CTX-0937): the search/copy-mode policy defaults retired with
    // the Core policy (`ctrl+shift+space` entered copy mode, `ctrl+shift+f`
    // opened search). Both chords are shell input again until the plugins
    // (CTX-0003, W-138 namespace) rebind them.
    // CTX-0952 semantic prompt navigation (issue #1670): `shift+alt+pageup`
    // jumps to the previous prompt, `shift+alt+pagedown` to the next
    // (one prompt per gesture, target at viewport top), `alt+o` selects
    // exactly the last command's output. The issue's `Mod+Shift+Up/Down`
    // suggestion collides under the single-owner rule (`shift+alt+arrows`
    // create splits, `shift+ctrl+arrows` resize), so the free page keys
    // carry the gesture; every entry carries the Mod slot so a Super flip
    // rebinds the whole gesture. Bare arrows, `alt+o` unmodified, and
    // `pageup`/`pagedown` alone stay shell input.
    ("shift+alt+pageup", "jump_to_prompt:prev"),
    ("shift+alt+pagedown", "jump_to_prompt:next"),
    ("alt+o", "select_command_output"),
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
/// Prefix-sequence entries (`"leader <second>"`, CTX-1002 / issue #1650) do
/// NOT enter this single-chord table: they resolve separately via
/// [`resolve_prefix_bindings`] into [`PrefixBinding`]s dispatched from the
/// pending Leader window. Skipping them here keeps single-chord matching
/// byte-identical whether or not prefix bindings exist.
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
        if is_prefix_entry_shape(&entry.chord) {
            continue;
        }
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
            return Some(m.action.clone());
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
///
/// Prefix-sequence entries (`"leader <second>"`, CTX-1002 / issue #1650)
/// validate through [`parse_prefix_entry`]: the `leader` keyword is symbolic
/// (it names the configured Leader from `input.leader` / `leader_key`, never
/// a hardcoded chord) and the second chord follows [`parse_prefix_second`].
pub fn validate_entry(entry: &KeymapEntry) -> Result<(), ConfigError> {
    validate_context(&entry.context)?;
    if is_prefix_entry_shape(&entry.chord) {
        parse_prefix_entry(entry).expect("shape checked above, never None")?;
        ChromeAction::parse(&entry.action)?;
        return Ok(());
    }
    Chord::parse(&entry.chord)?;
    ChromeAction::parse(&entry.action)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Leader key contract (CTX-0715 / OQ-088; issues #981 + #1045 leader part)
// ---------------------------------------------------------------------------
//
// The Leader arms the hint/overlay session (`bitty-rich` `HintSession`):
// pressing the Leader chord arms, the keys pressed after it form the
// operator+label chord, and `Esc`/timeout disarms. This section is the
// binding half only (default chord, Windows fallback, override precedence,
// timeout/cancel semantics). Engine sequencing — collecting batches from
// truth, painting, routing — rides OQ-089 (#981); the app input path does
// not consume this yet.
//
// The Leader is deliberately independent of [`ModKey`] (OQ-052 Mod
// unification stays deferred, do NOT route the Leader through the mod
// flip): an explicit `leader_key` keeps its exact spelling under any mod,
// and the platform defaults below never rewrite.

/// Default Leader chord (OQ-088 adopted): `Alt+Space`.
///
/// Free in [`DEFAULT_KEYMAPS`] (no shipped default binds it), so the default
/// never shadows a shipped chrome chord.
pub const LEADER_DEFAULT_CHORD_RAW: &str = "alt+space";

/// Windows fallback Leader chords (OQ-088): `Alt+Space` is OS-reserved on
/// Windows (it opens the window system menu and cannot be intercepted
/// reliably), so the Windows platform default is this set instead.
///
/// A set rather than a single chord so future fallbacks join without
/// changing the resolution API. `ctrl+space` carries no shipped default
/// binding; on Windows the Leader wins that byte (bare `ctrl+space` is
/// shell NUL elsewhere) by explicit owner decision, and `leader_key`
/// overrides it per user.
pub const LEADER_WINDOWS_FALLBACK_CHORDS_RAW: &[&str] = &["ctrl+space"];

/// Default fail-open Leader timeout in milliseconds (OQ-088): while armed,
/// the follow-up chord must arrive within this window or the pending press
/// expires and keys route back to the shell (fail-open, never swallowed).
/// Generous enough for an operator+label follow-up, short enough that a
/// stray Leader press releases quickly.
pub const LEADER_TIMEOUT_MS_DEFAULT: u64 = 1000;

/// Minimum accepted `leader_timeout_ms` override (fail-closed below).
pub const LEADER_TIMEOUT_MS_MIN: u64 = 100;

/// Maximum accepted `leader_timeout_ms` override (fail-closed above, so a
/// stuck Leader cannot linger indefinitely).
pub const LEADER_TIMEOUT_MS_MAX: u64 = 60_000;

/// Host platform for Leader default selection.
///
/// [`LeaderPlatform::host`] reads the compile target; tests inject the
/// variant so the Windows fallback is covered on Linux CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LeaderPlatform {
    /// macOS / Linux / other non-Windows hosts: the default is
    /// [`LEADER_DEFAULT_CHORD_RAW`].
    #[default]
    Other,
    /// Windows: `Alt+Space` is OS-reserved, so
    /// [`LEADER_WINDOWS_FALLBACK_CHORDS_RAW`] applies.
    Windows,
}

impl LeaderPlatform {
    /// The running host's platform.
    #[must_use]
    pub fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// Resolved Leader binding: every chord that arms plus the armed window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLeader {
    /// Every chord that arms the Leader, canonical-sorted and deduped:
    /// exactly the user override when set, else the platform default
    /// ([`LEADER_DEFAULT_CHORD_RAW`], or
    /// [`LEADER_WINDOWS_FALLBACK_CHORDS_RAW`] on Windows).
    pub chords: Vec<Chord>,
    /// Armed-window budget in milliseconds (override or
    /// [`LEADER_TIMEOUT_MS_DEFAULT`).
    pub timeout_ms: u64,
    /// True when neither chord nor timeout was overridden (for `config
    /// check` attribution).
    pub from_default: bool,
}

impl ResolvedLeader {
    /// True when this press arms the Leader (exact chord equality, the
    /// single-owner rule: a Leader chord never fuzzy-matches).
    #[must_use]
    pub fn arms(&self, key: KeyRef) -> bool {
        self.chords.iter().any(|c| key.matches(c))
    }

    /// Canonical spelling of the primary (first) arming chord, for reload
    /// diffs and diagnostics. Never empty by construction.
    #[must_use]
    pub fn primary_canonical(&self) -> String {
        self.chords
            .first()
            .map_or_else(String::new, |c| c.canonical())
    }
}

/// Parse one internal Leader default (fail-closed only on a typo in the
/// constants above, covered by tests; user input never reaches this path).
fn parse_internal_leader(raw: &str) -> Result<Chord, ConfigError> {
    Chord::parse(raw).map_err(|e| ConfigError::InvalidInput {
        message: format!("internal default leader invalid: {e}"),
    })
}

/// Validate a Leader timeout override (fail-closed outside
/// [`LEADER_TIMEOUT_MS_MIN`]..=[`LEADER_TIMEOUT_MS_MAX`], named on the
/// `leader_timeout_ms` field).
pub fn validate_leader_timeout_ms(timeout_ms: u64) -> Result<(), ConfigError> {
    if (LEADER_TIMEOUT_MS_MIN..=LEADER_TIMEOUT_MS_MAX).contains(&timeout_ms) {
        Ok(())
    } else {
        Err(ConfigError::validation(
            "leader_timeout_ms",
            format!(
                "must be {LEADER_TIMEOUT_MS_MIN}..={LEADER_TIMEOUT_MS_MAX} ms (got {timeout_ms})"
            ),
        ))
    }
}

/// Resolve the effective Leader binding (OQ-088).
///
/// Precedence: explicit user chord wins over every platform default (and
/// keeps its exact spelling — never rewritten by [`ModKey`]); explicit
/// user timeout wins over [`LEADER_TIMEOUT_MS_DEFAULT`]. Chord and timeout
/// override independently, so a timeout-only config keeps the platform
/// chord and vice versa.
pub fn resolve_leader(
    user_chord: Option<Chord>,
    user_timeout_ms: Option<u64>,
    platform: LeaderPlatform,
) -> Result<ResolvedLeader, ConfigError> {
    // Fail-closed on an emptied Windows fallback set (CTX-0727, #1315):
    // `primary_canonical` is "never empty by construction" only while this
    // const stays non-empty, so a future edit that empties it must error
    // here rather than silently resolve a chordless Leader.
    debug_assert!(
        !LEADER_WINDOWS_FALLBACK_CHORDS_RAW.is_empty(),
        "internal Windows leader fallback must not be empty"
    );
    let mut chords: Vec<Chord> = match user_chord {
        Some(chord) => vec![chord],
        None => match platform {
            LeaderPlatform::Other => vec![parse_internal_leader(LEADER_DEFAULT_CHORD_RAW)?],
            LeaderPlatform::Windows => LEADER_WINDOWS_FALLBACK_CHORDS_RAW
                .iter()
                .map(|raw| parse_internal_leader(raw))
                .collect::<Result<Vec<Chord>, ConfigError>>()?,
        },
    };
    chords.sort_by_key(|c| c.canonical());
    chords.dedup();
    if chords.is_empty() {
        return Err(ConfigError::InvalidInput {
            message: "internal Windows leader fallback is empty".to_string(),
        });
    }
    let timeout_ms = match user_timeout_ms {
        Some(ms) => {
            validate_leader_timeout_ms(ms)?;
            ms
        }
        None => LEADER_TIMEOUT_MS_DEFAULT,
    };
    Ok(ResolvedLeader {
        chords,
        timeout_ms,
        from_default: user_chord.is_none() && user_timeout_ms.is_none(),
    })
}

/// Resolve the Leader binding from an [`EffectiveConfig`]: the
/// `leader_key` / `leader_timeout_ms` overrides when the layers declare
/// them, else the `platform` default.
pub fn resolve_leader_for(
    effective: &EffectiveConfig,
    platform: LeaderPlatform,
) -> Result<ResolvedLeader, ConfigError> {
    resolve_leader(effective.leader_key, effective.leader_timeout_ms, platform)
}

/// Effective hint session config (CTX-0735 / OQ-089 #981).
///
/// The OQ-089 label/authority/config follow-ups stay ordered after OQ-050
/// anchors; this is the minimal config surface that is anchor-independent:
/// a default-on kill switch. While disabled the Leader never arms a hint
/// session and every key keeps its normal owner (fail-open routing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HintConfig {
    /// Whether the Leader may arm a hint session (default `true`).
    pub enabled: bool,
}

impl HintConfig {
    /// Default-on hint config (no layer declared `hints_enabled`).
    #[must_use]
    pub const fn default_on() -> Self {
        Self { enabled: true }
    }
}

/// Resolve the effective hint config: the `hints_enabled` override when a
/// layer declares it, else default-on. Infallible by construction (a raw
/// boolean needs no range check); unknown shapes never reach here — the
/// Lua layer rejects non-booleans fail-closed.
#[must_use]
pub fn resolve_hint_config(effective: &EffectiveConfig) -> HintConfig {
    HintConfig {
        enabled: effective.hints_enabled.unwrap_or(true),
    }
}

/// Armed-window routing for one Leader press (CTX-0715 / OQ-088 timeout and
/// cancel semantics).
///
/// Pure and headless: the caller owns the clock (monotonic milliseconds)
/// and the key routing. Arming never touches grid truth; expiry is
/// fail-open (back to [`LeaderState::Idle`] — keys route to the shell,
/// never swallowed); `Esc` cancels via [`cancel`](Self::cancel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderState {
    /// No pending Leader; every key keeps its normal owner (shell/chrome).
    Idle,
    /// A Leader press armed the overlay window; the follow-up chord must
    /// arrive before `deadline_ms` (caller clock) or the press expires.
    Armed {
        /// Caller-clock milliseconds at which the armed window ends.
        deadline_ms: u64,
    },
}

/// Outcome of [`LeaderState::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderPoll {
    /// Idle: nothing pending; route keys normally.
    Idle,
    /// Still armed: follow-up keys route to the overlay session.
    Armed,
    /// The armed window lapsed and the state returned to
    /// [`LeaderState::Idle`]: fail-open — route keys to the shell.
    Expired,
}

impl LeaderState {
    /// True while a Leader press is pending.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        matches!(self, Self::Armed { .. })
    }

    /// Arm on a Leader press: the follow-up must arrive within `timeout_ms`
    /// of `now_ms` (both caller-clock milliseconds; saturating so a hostile
    /// clock cannot wrap the deadline below `now_ms`).
    pub fn arm(&mut self, now_ms: u64, timeout_ms: u64) {
        *self = Self::Armed {
            deadline_ms: now_ms.saturating_add(timeout_ms),
        };
    }

    /// Cancel a pending Leader (`Esc`): returns true when a press was armed
    /// (the `Esc` is consumed by the cancel); false when idle (the `Esc`
    /// keeps its normal owner).
    pub fn cancel(&mut self) -> bool {
        if self.is_armed() {
            *self = Self::Idle;
            true
        } else {
            false
        }
    }

    /// Reap an expired press: [`LeaderPoll::Expired`] (now [`Idle`](Self::Idle))
    /// once `now_ms` reaches the deadline, else the current state.
    /// Idempotent: polling an idle state stays idle.
    pub fn poll(&mut self, now_ms: u64) -> LeaderPoll {
        match *self {
            Self::Idle => LeaderPoll::Idle,
            Self::Armed { deadline_ms } => {
                if now_ms >= deadline_ms {
                    *self = Self::Idle;
                    LeaderPoll::Expired
                } else {
                    LeaderPoll::Armed
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Prefix-sequence dispatch (CTX-1002 / issue #1650, RFC OQ-056)
// ---------------------------------------------------------------------------
//
// Two-step Leader sequences (`<Leader> <second>`, tmux-prefix style) resolve
// from the SAME `keymaps` table as single chords: an entry whose chord is
// spelled `"leader <second>"` (e.g. `{ chord = "leader w", action =
// "new_split:right", context = "global" }`) binds the follow-up chord
// pressed inside the pending Leader window to a chrome action.
//
// The `leader` keyword is symbolic: it names the configured Leader binding
// (`input.leader` / `leader_key`, resolved via [`resolve_leader_for` for the
// host platform), never a hardcoded chord. The shipped defaults bind no
// prefix sequence, so a config without one keeps single-chord dispatch
// byte-identical. Multi-step sequences (`<Leader> w v`, three presses) stay
// deferred: a three-token chord fails closed as an invalid single chord.
//
// Dispatch order while the Leader window is pending (see the `bitty-terminal`
// pending-state router): timeout expiry first (fail-open fallback), `Esc`
// cancel, Leader re-press re-arms, a [`PrefixBinding`] second-chord match
// dispatches, anything else falls through with its normal owner (never
// swallowed).
//
// Time/Space: [`match_prefix`] is a bounded linear scan over the resolved
// bindings (user tables cap at [`crate::types::MAX_KEYMAPS`]); dispatch
// state itself is `O(1)` and lives with the caller.

/// Symbolic keyword naming the configured Leader in a prefix-sequence chord.
///
/// Case-insensitive, exactly the first whitespace-separated token
/// (`"leader w"`, `"Leader ctrl+b"`). It never spells a chord itself: the
/// arming chord always comes from the resolved [`ResolvedLeader`].
pub const PREFIX_KEYWORD: &str = "leader";

/// One resolved prefix-sequence binding: the follow-up chord pressed inside
/// the pending Leader window plus the chrome action it dispatches.
///
/// Resolved from a `keymaps` entry spelled `"leader <second>"`; the entry's
/// context must be [`GLOBAL_CONTEXT`] (the only v1 context) and its action
/// follows the same [`ChromeAction`] grammar as single chords. The Leader
/// half is symbolic (see [`PREFIX_KEYWORD`]), so bindings survive a Leader
/// re-chord untouched.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrefixBinding {
    /// Follow-up chord dispatched while the Leader window is pending.
    pub second: Chord,
    /// Chrome action dispatched on a second-chord match.
    pub action: ChromeAction,
    /// Activation context, always [`GLOBAL_CONTEXT`] in v1.
    pub context: String,
}

impl PrefixBinding {
    /// Merge/dispatch identity: `context + canonical second chord`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}::{}", self.context, self.second.canonical())
    }

    /// True when this press is the bound follow-up (exact chord equality,
    /// the single-owner rule: no fuzzy matching).
    #[must_use]
    pub fn matches(&self, key: KeyRef) -> bool {
        key.matches(&self.second)
    }
}

/// True when a raw chord string has the prefix-sequence shape: exactly two
/// whitespace-separated tokens with the first token [`PREFIX_KEYWORD`]
/// (case-insensitive). Pure shape check, no parsing: non-prefix chords
/// return false and keep their single-chord path untouched.
#[must_use]
pub fn is_prefix_entry_shape(raw: &str) -> bool {
    let mut tokens = raw.split_whitespace();
    match (tokens.next(), tokens.next(), tokens.next()) {
        (Some(first), Some(_), None) => first.eq_ignore_ascii_case(PREFIX_KEYWORD),
        _ => false,
    }
}

/// Parse a prefix-sequence follow-up chord (`<second>` in `"leader <second>"`).
///
/// The shared [`Chord`] grammar applies, except bare single-character keys
/// (`w`, `v`, `c`, `1`, ...) are accepted WITHOUT a modifier: the Leader
/// half already disambiguates, so a bare follow-up can never steal shell
/// typing (it only matches inside the pending window). Bare named keys
/// (`escape`, `tab`, `enter`, `f5`, ...) already parse through [`Chord`];
/// multi-character unknown tokens fail closed.
pub fn parse_prefix_second(raw: &str) -> Result<Chord, ConfigError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::validation(
            "keymaps[].chord",
            "prefix follow-up must not be empty ('leader <second>')",
        ));
    }
    if trimmed.len() > MAX_CHORD_LEN {
        return Err(ConfigError::validation(
            "keymaps[].chord",
            format!("must be <= {MAX_CHORD_LEN} bytes"),
        ));
    }
    if let Ok(chord) = Chord::parse(trimmed) {
        return Ok(chord);
    }
    let mut chars = trimmed.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(Chord {
            ctrl: false,
            alt: false,
            shift: false,
            super_held: false,
            key: KeyName::Char(c.to_ascii_lowercase()),
        }),
        _ => Err(ConfigError::validation(
            "keymaps[].chord",
            format!(
                "unknown prefix follow-up '{trimmed}'; use '<mod>+...+<key>', a named key, or a bare letter"
            ),
        )),
    }
}

/// Split a raw prefix-sequence chord into its follow-up spelling.
///
/// Returns `Some(second)` when `raw` has the prefix shape (see
/// [`is_prefix_entry_shape`]), else `None` (the caller keeps the
/// single-chord path). The `leader` keyword spelling is checked
/// case-insensitively; the second token keeps its raw spelling for
/// [`parse_prefix_second`].
pub fn split_prefix_chord(raw: &str) -> Option<&str> {
    if !is_prefix_entry_shape(raw) {
        return None;
    }
    raw.split_whitespace().nth(1)
}

/// Parse one `keymaps` entry with the prefix-sequence shape into its
/// [`PrefixBinding`]. Returns `None` when the entry is a single chord (not
/// prefix-shaped); `Some(Err)` fail-closed on an unparseable follow-up
/// (never a panic, never a silent ignore).
pub fn parse_prefix_entry(entry: &KeymapEntry) -> Option<Result<PrefixBinding, ConfigError>> {
    let second_raw = split_prefix_chord(&entry.chord)?;
    Some((|| {
        let context = validate_context(&entry.context)?;
        let second = parse_prefix_second(second_raw)?;
        let action = ChromeAction::parse(&entry.action)?;
        Ok(PrefixBinding {
            second,
            action,
            context,
        })
    })())
}

/// Resolve the effective prefix-sequence table from the user `keymaps`
/// entries: every `"leader <second>"` entry becomes a [`PrefixBinding`]
/// (context plus canonical second-chord identity, sorted deterministically);
/// single-chord entries are skipped (they resolve via [`resolve_keymaps`]).
///
/// Later entries with the same identity overlay earlier ones (the existing
/// `context + chord` merge rule applied to the second chord), so a layer
/// rebind restates the same `"leader <second>"` spelling to win.
/// Fail-closed on unknown contexts, follow-ups, or actions.
pub fn resolve_prefix_bindings(entries: &[KeymapEntry]) -> Result<Vec<PrefixBinding>, ConfigError> {
    let mut table: Vec<PrefixBinding> = Vec::new();
    for entry in entries {
        let Some(parsed) = parse_prefix_entry(entry) else {
            continue;
        };
        let binding = parsed?;
        let id = binding.id();
        table.retain(|b| b.id() != id);
        table.push(binding);
    }
    table.sort_by_key(|b| b.id());
    Ok(table)
}

/// Match a pressed key against the resolved prefix table while the Leader
/// window is pending: exact second-chord equality only. Returns the bound
/// action (the single owner) or `None` for fallback routing (the press keeps
/// its normal owner and the pending window disarms).
#[must_use]
pub fn match_prefix(bindings: &[PrefixBinding], key: KeyRef) -> Option<ChromeAction> {
    for b in bindings {
        if b.matches(key) {
            return Some(b.action.clone());
        }
    }
    None
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
            Some(ChromeAction::NewPanel)
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('m'), false, false)),
            Some(ChromeAction::ToggleZoom)
        );
        // CTX-0962 (#1695): Mod+a floating toggle follows the Mod slot.
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('a'), false, false)),
            Some(ChromeAction::ToggleFloating)
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
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('a'), false, true, false)),
            None,
            "alt+a unbound under super mod"
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
            ChromeAction::parse("new_panel").expect("new panel"),
            ChromeAction::NewPanel
        );
        assert_eq!(ChromeAction::NewPanel.canonical(), "new_panel");
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
            ChromeAction::parse("workspace_next_occupied").expect("ws next occ"),
            ChromeAction::WorkspaceNextOccupied
        );
        assert_eq!(
            ChromeAction::WorkspaceNextOccupied.canonical(),
            "workspace_next_occupied"
        );
        assert_eq!(
            ChromeAction::parse("workspace_prev_occupied").expect("ws prev occ"),
            ChromeAction::WorkspacePrevOccupied
        );
        assert_eq!(
            ChromeAction::WorkspacePrevOccupied.canonical(),
            "workspace_prev_occupied"
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
    fn w144_retired_search_copy_spellings_fail_closed_as_unknown() {
        // W-144 (CTX-0937): Core search/copy-mode policy retired to the
        // search (search@e65bf83) and copy-mode (copy-mode@7410a3e) plugins.
        // Zero plugins (no-plugin baseline) and safe mode degrade safely:
        // the retired spellings parse as unknown with a clear error, never
        // panic and never strand a modal. The plugin namespace (W-138) owns
        // these verbs now; Core keeps only the host mechanisms.
        for raw in [
            "enter_copy_mode",
            "copy_mode",
            "open_search",
            "search",
            "search_next",
            "search_down",
            "search_prev",
            "search_previous",
            "search_up",
            "close_search",
            "search_close",
            "search_toggle_case",
            "toggle_search_case",
        ] {
            let err = ChromeAction::parse(raw).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("keymaps[].action") && msg.contains("unknown action"),
                "retired spelling must fail closed as unknown for {raw:?}: {msg}"
            );
            assert!(
                !msg.contains("enter_copy_mode") || msg.contains("expected one of"),
                "hint must list current vocabulary for {raw:?}"
            );
        }
        // The hint no longer advertises the retired namespace.
        assert!(!KNOWN_ACTIONS_HINT.contains("enter_copy_mode"));
        assert!(!KNOWN_ACTIONS_HINT.contains("open_search"));
        assert!(!KNOWN_ACTIONS_HINT.contains("search_next"));
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
            // CTX-0962: bare `a` stays shell input; only Mod+a toggles.
            key_ref(KeyName::Char('a'), false, false, false),
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
        // CTX-0962 (#1695): Mod+a toggles floating; alt+v stays free
        // (fish reserves it for `$EDITOR`, so Mod+v is unusable there).
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('a'), false, true, false)),
            Some(ChromeAction::ToggleFloating)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('v'), false, true, false)),
            None,
            "alt+v stays shell (fish conflict)"
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
    fn fold_verbs_parse_but_are_never_defaults() {
        // CTX-0723 (#980): fold verbs parse (with `toggle_fold` /
        // `expand_fold` / `collapse_fold` aliases) but never ship in
        // defaults, so fresh configs keep Normal Mode byte-identical.
        // Users opt in explicitly, e.g. `{ chord = "alt+z", action =
        // "fold_toggle" }`.
        for (raw, canonical) in [
            ("fold_toggle", "fold_toggle"),
            ("toggle_fold", "fold_toggle"),
            ("fold_expand", "fold_expand"),
            ("expand_fold", "fold_expand"),
            ("fold_collapse", "fold_collapse"),
            ("collapse_fold", "fold_collapse"),
        ] {
            let action = ChromeAction::parse(raw).expect("fold verb parses");
            assert_eq!(action.canonical(), canonical, "raw {raw}");
        }
        assert_eq!(
            ChromeAction::parse("fold_toggle").expect("parses"),
            ChromeAction::FoldToggle
        );
        let maps = default_keymaps().expect("defaults valid");
        for action in [
            ChromeAction::FoldToggle,
            ChromeAction::FoldExpand,
            ChromeAction::FoldCollapse,
        ] {
            assert!(
                !maps.iter().any(|m| m.action == action),
                "defaults must not bind {}",
                action.canonical()
            );
        }
        assert!(
            KNOWN_ACTIONS_HINT.contains("fold_toggle"),
            "fail-closed hint must name the fold verbs"
        );
    }

    #[test]
    fn toggle_palette_parses_but_is_never_a_default() {
        // CTX-0647 / #1003: palette user entry exists as forward-compat
        // wiring while the palette stays bundled-disabled (OQ-053: the
        // palette is the independent first-party package, no panel host in
        // the app yet). `toggle_palette` only exists when the user binds
        // it explicitly (suggested chord `ctrl+shift+p`, free in the
        // shipped map).
        for raw in ["toggle_palette", "open_palette", "palette_toggle"] {
            assert_eq!(
                ChromeAction::parse(raw).expect("parses"),
                ChromeAction::TogglePalette,
                "alias {raw:?} must parse"
            );
        }
        assert_eq!(ChromeAction::TogglePalette.canonical(), "toggle_palette");
        // Panel command grammar stays rejected here: `palette:toggle` is a
        // PanelRegistry command, not a chrome action.
        assert!(ChromeAction::parse("palette:toggle").is_err());
        assert!(ChromeAction::parse("toggle_palette:1").is_err());
        let maps = default_keymaps().expect("defaults valid");
        assert!(
            !maps.iter().any(|m| m.action == ChromeAction::TogglePalette),
            "defaults must not bind toggle_palette so Ctrl+Shift+P stays shell input"
        );
        // Bare `p` can never be a chord (schema rule), so the open key
        // cannot shadow shell typing even when bound.
        assert!(Chord::parse("p").is_err());
        let open = Chord::parse("ctrl+shift+p").expect("ctrl+shift+p parses");
        assert_eq!(open.canonical(), "ctrl+shift+p");
        // Unknown head still lists the vocabulary including the new action.
        let err = ChromeAction::parse("explode:now").unwrap_err();
        assert!(err.to_string().contains("toggle_palette"));
    }

    #[test]
    fn invoke_command_parses_qualified_targets_and_canonicalizes() {
        // CTX-1035 (issue #1829): the generic host-mediated invocation
        // path. `command:<owner:command>` (aliases `invoke_command:`,
        // `run_command:`) parses to the qualified target and canonicalizes
        // back to the `command:` spelling; ownership and liveness stay
        // runtime checks at dispatch.
        for raw in [
            "command:bitty-featured.devtools:plugins",
            "invoke_command:bitty-featured.devtools:plugins",
            "run_command:bitty-featured.devtools:plugins",
        ] {
            let action = ChromeAction::parse(raw).expect("parses");
            assert_eq!(
                action,
                ChromeAction::InvokeCommand("bitty-featured.devtools:plugins".to_string()),
                "raw {raw:?} must parse"
            );
            assert_eq!(
                action.canonical(),
                "command:bitty-featured.devtools:plugins"
            );
        }
        // The vocabulary hint names the new action.
        let err = ChromeAction::parse("explode:now").unwrap_err();
        assert!(err.to_string().contains("command:<owner:command>"));
        // Deny-by-default at parse: missing, empty-sided, multi-colon,
        // NUL/space-bearing, and over-long targets fail closed.
        for raw in [
            "command",
            "command:",
            "command::plugins",
            "command:owner:",
            "command::",
            "command:a:b:c",
            "command:has space:x",
            "command:has\0nul:x",
        ] {
            assert!(ChromeAction::parse(raw).is_err(), "raw {raw:?} must fail");
        }
        let long_owner = "o".repeat(128);
        let long_command = "c".repeat(128);
        let longest = format!("command:{long_owner}:{long_command}");
        assert_eq!(longest.len(), 8 + MAX_INVOKE_COMMAND_LEN);
        assert!(ChromeAction::parse(&longest).is_ok());
        let overlong = format!("command:{long_owner}:{long_command}x");
        assert!(ChromeAction::parse(&overlong).is_err());
        // Manual bind only, never in defaults (same byte-identical
        // discipline as `open_composer`): a fresh config keeps Normal Mode
        // byte-identical and no chord invokes plugin code unasked.
        let maps = default_keymaps().expect("defaults valid");
        assert!(
            !maps
                .iter()
                .any(|m| matches!(m.action, ChromeAction::InvokeCommand(_))),
            "defaults must not bind command: invocations"
        );
    }

    #[test]
    fn prompt_nav_actions_parse_and_ship_documented_defaults() {
        // CTX-0952 (issue #1670): prompt-jump and select-output verbs parse
        // (arg form plus explicit aliases), canonicalize to the arg form,
        // and ship the documented default chords.
        for (raw, want) in [
            ("jump_to_prompt:prev", ChromeAction::JumpToPromptPrev),
            ("jump_to_prompt:previous", ChromeAction::JumpToPromptPrev),
            ("jump_to_prompt:up", ChromeAction::JumpToPromptPrev),
            ("jump_to_prompt_prev", ChromeAction::JumpToPromptPrev),
            ("prompt_prev", ChromeAction::JumpToPromptPrev),
            ("jump_to_prompt:next", ChromeAction::JumpToPromptNext),
            ("jump_to_prompt:down", ChromeAction::JumpToPromptNext),
            ("jump_to_prompt_next", ChromeAction::JumpToPromptNext),
            ("prompt_next", ChromeAction::JumpToPromptNext),
            ("select_command_output", ChromeAction::SelectCommandOutput),
            ("select_output", ChromeAction::SelectCommandOutput),
        ] {
            assert_eq!(ChromeAction::parse(raw).expect("parses"), want, "raw {raw}");
        }
        assert_eq!(
            ChromeAction::JumpToPromptPrev.canonical(),
            "jump_to_prompt:prev"
        );
        assert_eq!(
            ChromeAction::JumpToPromptNext.canonical(),
            "jump_to_prompt:next"
        );
        assert_eq!(
            ChromeAction::SelectCommandOutput.canonical(),
            "select_command_output"
        );
        // Direction is required and rejected otherwise.
        assert!(ChromeAction::parse("jump_to_prompt").is_err());
        assert!(ChromeAction::parse("jump_to_prompt:sideways").is_err());
        assert!(ChromeAction::parse("jump_to_prompt_prev:1").is_err());
        assert!(ChromeAction::parse("select_command_output:x").is_err());
        // Shipped defaults resolve each action exactly once.
        let maps = default_keymaps().expect("defaults valid");
        let prev: Vec<_> = maps
            .iter()
            .filter(|m| m.action == ChromeAction::JumpToPromptPrev)
            .collect();
        let next: Vec<_> = maps
            .iter()
            .filter(|m| m.action == ChromeAction::JumpToPromptNext)
            .collect();
        let select: Vec<_> = maps
            .iter()
            .filter(|m| m.action == ChromeAction::SelectCommandOutput)
            .collect();
        assert_eq!(prev.len(), 1);
        assert_eq!(prev[0].chord.canonical(), "alt+shift+pageup");
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].chord.canonical(), "alt+shift+pagedown");
        assert_eq!(select.len(), 1);
        assert_eq!(select[0].chord.canonical(), "alt+o");
        // Bare arrows, bare page keys, and bare letters stay shell input:
        // no shipped default binds them (chords parse, but nothing in the
        // table claims them, so they fall through to the PTY).
        for chord in ["up", "down", "pageup", "pagedown"] {
            let bare = Chord::parse(chord).expect("bare named key parses");
            assert!(
                !maps.iter().any(|m| m.chord == bare),
                "no default may bind bare {chord}"
            );
        }
        assert!(Chord::parse("o").is_err(), "bare letters need a modifier");
        assert!(
            KNOWN_ACTIONS_HINT.contains("jump_to_prompt:<prev|next>"),
            "fail-closed hint must name the prompt verbs"
        );
        assert!(KNOWN_ACTIONS_HINT.contains("select_command_output"));
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
            Some(ChromeAction::ScrollPageDown)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('i'), false, true, false)),
            Some(ChromeAction::ScrollPageUp)
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
    fn defaults_close_shortcuts_issue_1444() {
        // Issue #1444: alt+d closes pane/view, alt+w closes workspace.
        // Both respect close_confirm mode (tested in bitty-runtime).
        let maps = default_keymaps().expect("defaults valid");
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('d'), false, true, false)),
            Some(ChromeAction::CloseView),
            "alt+d closes pane/view"
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('w'), false, true, false)),
            Some(ChromeAction::WorkspaceClose),
            "alt+w closes workspace"
        );
        // Super flip rebinds both (Mod slot carried).
        let super_maps = default_keymaps_with_mod(ModKey::Super).expect("super valid");
        assert_eq!(
            match_keymap(
                &super_maps,
                KeyRef {
                    key: KeyName::Char('d'),
                    ctrl: false,
                    alt: false,
                    shift: false,
                    super_held: true,
                }
            ),
            Some(ChromeAction::CloseView),
            "super+d closes pane under Super mod"
        );
        assert_eq!(
            match_keymap(
                &super_maps,
                KeyRef {
                    key: KeyName::Char('w'),
                    ctrl: false,
                    alt: false,
                    shift: false,
                    super_held: true,
                }
            ),
            Some(ChromeAction::WorkspaceClose),
            "super+w closes workspace under Super mod"
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
        // (shift+alt+1..=9) = 75, plus CTX-0945's 9 swap chords
        // (ctrl+shift+alt+1..=9) = 84, plus CTX-0265's 4 help chords
        // (alt+backtick + 3 alt+? shifted-symbol spellings) = 88 total
        // (W-144 retired the CTX-0384 copy-mode chord and the CTX-0383
        // search chord), plus CTX-0766's 1 new workspace chord (alt+t;
        // alt+n becomes new-panel) = 89 total, plus issue #1444's 1
        // close-view chord (alt+d) = 90 total, plus CTX-0952's 3 prompt
        // chords (shift+alt+pageup/pagedown + alt+o) = 93 total,
        // plus CTX-0962's 1 floating-toggle chord (alt+a) = 94 total,
        // plus issue #1776's 1 close-view chord (alt+q) = 95 total,
        // plus CTX-1100's 2 occupied-cycle chords (alt+[/]) = 97 total,
        // and the full DEC
        // set resolves. Zoom chords carry
        // no `alt`, so they must stay unique under Alt and Super alike.
        for mod_key in [ModKey::Alt, ModKey::Super] {
            let maps = default_keymaps_with_mod(mod_key).expect("defaults valid");
            assert_eq!(
                maps.len(),
                97,
                "35 shipped + 4 workspace-entry chords + 4 resize chords + 16 arrow aliases + 7 zoom chords + 9 move chords + 9 swap chords + 4 help chords + 1 CTX-0766 rechord + 1 issue-1444 close-view + 3 CTX-0952 prompt chords + 1 CTX-0962 floating-toggle + 1 issue-1776 close-view + 2 CTX-1100 occupied-cycle (W-144 retired copy-mode + search)"
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
        // The DEC-0034 entry set resolves through the shipped table
        // (CTX-0766: new-workspace moved alt+n -> alt+t).
        let dec: &[(&str, bool, bool, ChromeAction)] = &[
            ("t", false, false, ChromeAction::WorkspaceNew),
            ("w", false, false, ChromeAction::WorkspaceClose),
            ("-", false, false, ChromeAction::WorkspacePrev),
            ("=", false, false, ChromeAction::WorkspaceNext),
        ];
        for (key, ctrl, shift, want) in dec {
            let key = KeyName::Char(key.chars().next().expect("single"));
            assert_eq!(
                match_keymap(&maps, key_ref(key, *ctrl, true, *shift)),
                Some(want.clone()),
                "DEC chord alt+{key:?}"
            );
        }
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Tab, false, true, false)),
            Some(ChromeAction::WorkspaceLast),
            "alt+tab is last-used workspace"
        );
        // CTX-1100 (#1904): occupied-cycle defaults. Both chords were free;
        // per the owner verbatim intent `[` moves rightward (next) and `]`
        // mirrors leftward (prev).
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('['), false, true, false)),
            Some(ChromeAction::WorkspaceNextOccupied),
            "alt+[ is next occupied"
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char(']'), false, true, false)),
            Some(ChromeAction::WorkspacePrevOccupied),
            "alt+] is prev occupied"
        );
        // CTX-0838 (#1441): alt+n opens a new panel with Hyprland-dwindle
        // semantics (adaptive axis, new-second, focus follows).
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('n'), false, true, false)),
            Some(ChromeAction::NewPanel),
            "alt+n is new panel"
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
    fn shifted_symbol_base_recovers_us_reference_pairs() {
        // Issue #1446: the platform reports the modifier-applied logical
        // character, so the physical digit and punctuation rows must fold
        // back to their base key.
        for (symbol, base) in [
            ('!', '1'),
            ('@', '2'),
            ('#', '3'),
            ('$', '4'),
            ('%', '5'),
            ('^', '6'),
            ('&', '7'),
            ('*', '8'),
            ('(', '9'),
            (')', '0'),
            ('~', '`'),
            ('_', '-'),
            ('+', '='),
            ('{', '['),
            ('}', ']'),
            ('|', '\\'),
            (':', ';'),
            ('"', '\''),
            ('<', ','),
            ('>', '.'),
            ('?', '/'),
        ] {
            assert_eq!(shifted_symbol_base(symbol), Some(base), "{symbol:?} folds");
        }
        // Base keys, letters, and unpaired characters fold to nothing.
        for c in ['1', 'a', 'Z', 'ä', ' ', '\t'] {
            assert_eq!(shifted_symbol_base(c), None, "{c:?} stays itself");
        }
    }

    #[test]
    fn physical_shift_number_resolves_through_base_key_fallback() {
        // Issue #1446 regression pin at the config layer: the physical
        // gesture Mod+Shift+Number arrives as the shifted symbol plus a held
        // Shift and the Mod (winit applies both), while DEC-0034 spells the
        // move chord with the base key (`shift+alt+2`). The reported spelling
        // misses by exact equality; the base-key spelling — what the app
        // dispatch consults second — hits. Without the fold the gesture moved
        // nothing and the symbol leaked to the PTY.
        let maps = default_keymaps().expect("defaults valid");
        let digit_row = [
            ('1', '!'),
            ('2', '@'),
            ('3', '#'),
            ('4', '$'),
            ('5', '%'),
            ('6', '^'),
            ('7', '&'),
            ('8', '*'),
            ('9', '('),
        ];
        for (index, (digit, symbol)) in digit_row.iter().enumerate() {
            let one_based = (index + 1) as u64;
            let digit = KeyName::Char(*digit);
            let reported = KeyRef {
                key: KeyName::Char(*symbol),
                ctrl: false,
                alt: true,
                shift: true,
                super_held: false,
            };
            assert_eq!(
                match_keymap(&maps, reported),
                None,
                "the reported {symbol:?} is not a bound spelling"
            );
            let base = reported.unshifted_base().expect("shifted digit folds");
            assert_eq!(base.key, digit, "{symbol:?} folds to its base key");
            assert!(
                base.alt && base.shift && !base.ctrl && !base.super_held,
                "the fold keeps every modifier so Shift stays part of the chord"
            );
            assert_eq!(
                match_keymap(&maps, base),
                Some(ChromeAction::WorkspaceMove(one_based)),
                "physical Mod+Shift+{digit:?} moves the focused pane to ws:{one_based}"
            );
            // The plain Mod+digit switch spelling is unchanged and needs no
            // fold (a digit already is a base key).
            let plain = key_ref(digit, false, true, false);
            assert_eq!(
                match_keymap(&maps, plain),
                Some(ChromeAction::WorkspaceFocus(one_based)),
                "physical Mod+{digit:?} jumps to ws:{one_based}"
            );
            assert!(plain.unshifted_base().is_none());
        }
        // The Super flip re-spells the same physical gesture.
        let super_maps = default_keymaps_with_mod(ModKey::Super).expect("super valid");
        let reported = KeyRef {
            key: KeyName::Char('@'),
            ctrl: false,
            alt: false,
            shift: true,
            super_held: true,
        };
        assert_eq!(match_keymap(&super_maps, reported), None);
        assert_eq!(
            match_keymap(&super_maps, reported.unshifted_base().expect("folds")),
            Some(ChromeAction::WorkspaceMove(2))
        );
        // Named keys never fold: Shift+Mod+Tab is not Mod+Tab.
        assert!(
            key_ref(KeyName::Tab, false, true, true)
                .unshifted_base()
                .is_none()
        );
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
            Some(ChromeAction::NewPanel)
        );
        // CTX-0766: new-workspace moved super+n -> super+t.
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('t'), false, false)),
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
        // CTX-0962 (#1695): the floating toggle follows the Mod slot too.
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('a'), false, false)),
            Some(ChromeAction::ToggleFloating)
        );
        // CTX-1100 (#1904): the occupied-cycle pair follows the Mod slot.
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char('['), false, false)),
            Some(ChromeAction::WorkspaceNextOccupied)
        );
        assert_eq!(
            match_keymap(&maps, key_ref_super(KeyName::Char(']'), false, false)),
            Some(ChromeAction::WorkspacePrevOccupied)
        );
        // Old Alt spellings are unbound (back to the shell) under Super.
        for key in [
            KeyName::Char('n'),
            KeyName::Char('w'),
            KeyName::Char('a'),
            KeyName::Char('-'),
            KeyName::Char('='),
            KeyName::Char('['),
            KeyName::Char(']'),
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
    fn floating_toggle_parses_canonicalizes_and_stays_user_overridable() {
        // CTX-0962 (#1695): the floating-toggle action parses (primary plus
        // primitive-order alias), canonicalizes to the primary spelling,
        // ships on Mod+a, and stays user-overridable through the existing
        // `context + chord` merge rule (Lua `keymaps` entries).
        assert_eq!(
            ChromeAction::parse("toggle_floating").expect("parses"),
            ChromeAction::ToggleFloating
        );
        assert_eq!(
            ChromeAction::parse("floating_toggle").expect("alias parses"),
            ChromeAction::ToggleFloating
        );
        assert_eq!(ChromeAction::ToggleFloating.canonical(), "toggle_floating");
        // Shipped default.
        let maps = default_keymaps().expect("defaults valid");
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('a'), false, true, false)),
            Some(ChromeAction::ToggleFloating)
        );
        // User override replaces the default by chord identity ...
        let overridden = EffectiveConfig {
            keymaps: vec![KeymapEntry {
                chord: "alt+a".into(),
                action: "focus_next".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        let maps = resolve_keymaps(&overridden).expect("resolves");
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('a'), false, true, false)),
            Some(ChromeAction::FocusNext)
        );
        // ... and the action rebinds elsewhere by explicit chord.
        let rebound = EffectiveConfig {
            keymaps: vec![KeymapEntry {
                chord: "alt+q".into(),
                action: "toggle_floating".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        let maps = resolve_keymaps(&rebound).expect("resolves");
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('q'), false, true, false)),
            Some(ChromeAction::ToggleFloating)
        );
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('a'), false, true, false)),
            Some(ChromeAction::ToggleFloating),
            "default survives an unrelated append"
        );
    }

    #[test]
    fn pinned_toggle_parses_canonicalizes_and_stays_manual_bind_only() {
        // CTX-1083 (follow-up to CTX-1077 #1757): the pinned-toggle action
        // parses (primary plus primitive-order alias), canonicalizes to the
        // primary spelling, ships on no default chord (manual bind only, so
        // Normal Mode stays byte-identical), and binds by explicit chord.
        assert_eq!(
            ChromeAction::parse("toggle_pinned").expect("parses"),
            ChromeAction::TogglePinned
        );
        assert_eq!(
            ChromeAction::parse("pinned_toggle").expect("alias parses"),
            ChromeAction::TogglePinned
        );
        assert_eq!(ChromeAction::TogglePinned.canonical(), "toggle_pinned");
        let maps = default_keymaps().expect("defaults valid");
        assert!(
            !maps.iter().any(|m| m.action == ChromeAction::TogglePinned),
            "toggle_pinned ships unbound"
        );
        let rebound = EffectiveConfig {
            keymaps: vec![KeymapEntry {
                chord: "alt+p".into(),
                action: "toggle_pinned".into(),
                context: "global".into(),
            }],
            ..Default::default()
        };
        let maps = resolve_keymaps(&rebound).expect("resolves");
        assert_eq!(
            match_keymap(&maps, key_ref(KeyName::Char('p'), false, true, false)),
            Some(ChromeAction::TogglePinned),
            "toggle_pinned binds by explicit chord"
        );
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
        // The default count is 95 under Alt mod (35 shipped + 4 workspace
        // + 4 resize + 16 arrow + 9 move + 9 swap + 7 zoom + 4 help
        // + 1 CTX-0766 rechord + 1 issue-1444 close-view + 3 CTX-0952 prompt
        // + 1 CTX-0962 floating-toggle + 1 issue-1776 close-view; W-144 retired the copy-mode and
        // search chords).
        // Under Super mod, alt+d becomes super+d, alt+q becomes super+q (still counted).
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
                97,
                "no new shipped defaults under mod {:?} (90 + 3 CTX-0952 prompt chords + 1 CTX-0962 floating-toggle + 1 issue-1776 close-view + 2 CTX-1100 occupied-cycle)",
                mod_key
            );
            let maps = resolve_keymaps(&mk_effective(mod_key)).expect("resolves");
            assert_eq!(
                maps.len(),
                97 + entries.len(),
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
                Some(action.clone()),
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

    // -- CTX-0715 / OQ-088 Leader key contract ---------------------------

    fn alt_space() -> KeyRef {
        key_ref(KeyName::Space, false, true, false)
    }

    fn ctrl_space() -> KeyRef {
        key_ref(KeyName::Space, true, false, false)
    }

    #[test]
    fn leader_default_resolution_is_alt_space() {
        // OQ-088 default: no overrides on a non-Windows host arms on
        // `Alt+Space` with the default fail-open timeout.
        let effective = EffectiveConfig::default();
        let leader =
            resolve_leader_for(&effective, LeaderPlatform::Other).expect("default resolves");
        assert_eq!(leader.timeout_ms, LEADER_TIMEOUT_MS_DEFAULT);
        assert!(leader.from_default);
        assert!(leader.arms(alt_space()), "alt+space arms the leader");
        assert_eq!(leader.primary_canonical(), "alt+space");
        // The default never shadows a shipped chrome chord (single-owner).
        let maps = default_keymaps().expect("defaults valid");
        assert_eq!(
            match_keymap(&maps, alt_space()),
            None,
            "alt+space must stay free of chrome bindings"
        );
        assert_eq!(
            match_keymap(&maps, ctrl_space()),
            None,
            "ctrl+space must stay free of chrome bindings"
        );
    }

    #[test]
    fn leader_windows_fallback_set_applies() {
        // OQ-088 Windows fallback: `Alt+Space` is OS-reserved there, so the
        // fallback set arms instead and the default does not.
        let effective = EffectiveConfig::default();
        let leader =
            resolve_leader_for(&effective, LeaderPlatform::Windows).expect("fallback resolves");
        assert!(leader.from_default, "platform fallback is still default");
        assert_eq!(leader.timeout_ms, LEADER_TIMEOUT_MS_DEFAULT);
        assert!(
            leader.arms(ctrl_space()),
            "ctrl+space arms the leader on Windows"
        );
        assert!(
            !leader.arms(alt_space()),
            "alt+space must not arm on Windows (OS-reserved)"
        );
        for raw in LEADER_WINDOWS_FALLBACK_CHORDS_RAW {
            let chord = Chord::parse(raw).expect("fallback parses");
            assert!(
                leader.chords.contains(&chord),
                "fallback set member {raw} resolves"
            );
        }
    }

    #[test]
    fn leader_windows_fallback_set_cannot_silently_empty() {
        // CTX-0727 (#1315): `primary_canonical` is "never empty by
        // construction" only while the fallback const stays non-empty; pin
        // the invariant so a future edit that empties it fails here, and
        // resolution stays fail-closed with a non-empty primary.
        assert!(
            !LEADER_WINDOWS_FALLBACK_CHORDS_RAW.is_empty(),
            "Windows leader fallback must hold at least one chord"
        );
        let leader =
            resolve_leader(None, None, LeaderPlatform::Windows).expect("fallback resolves");
        assert!(
            !leader.chords.is_empty(),
            "resolved Windows leader must arm at least one chord"
        );
        assert!(
            !leader.primary_canonical().is_empty(),
            "resolved primary canonical must not be empty"
        );
    }

    #[test]
    fn leader_override_precedence_wins_on_both_platforms() {
        // OQ-088 override surface: an explicit `leader_key` + timeout wins
        // over every platform default, on both platforms, and chord/timeout
        // override independently.
        let overridden = EffectiveConfig {
            leader_key: Some(Chord::parse("ctrl+q").expect("override parses")),
            leader_timeout_ms: Some(2500),
            ..Default::default()
        };
        for platform in [LeaderPlatform::Other, LeaderPlatform::Windows] {
            let leader = resolve_leader_for(&overridden, platform).expect("resolves");
            assert!(!leader.from_default, "override is not default");
            assert_eq!(leader.timeout_ms, 2500);
            assert_eq!(leader.primary_canonical(), "ctrl+q");
            assert!(
                leader.arms(key_ref(KeyName::Char('q'), true, false, false)),
                "override arms on {platform:?}"
            );
            assert!(
                !leader.arms(alt_space()) && !leader.arms(ctrl_space()),
                "platform defaults disarmed by override on {platform:?}"
            );
        }
        // Timeout-only override keeps the platform chord.
        let timeout_only = EffectiveConfig {
            leader_timeout_ms: Some(2000),
            ..Default::default()
        };
        let leader = resolve_leader_for(&timeout_only, LeaderPlatform::Other).expect("resolves");
        assert!(!leader.from_default);
        assert_eq!(leader.timeout_ms, 2000);
        assert!(leader.arms(alt_space()), "platform chord kept");
        // Chord-only override keeps the default timeout.
        let chord_only = EffectiveConfig {
            leader_key: Some(Chord::parse("alt+x").expect("parses")),
            ..Default::default()
        };
        let leader = resolve_leader_for(&chord_only, LeaderPlatform::Other).expect("resolves");
        assert_eq!(leader.timeout_ms, LEADER_TIMEOUT_MS_DEFAULT);
        assert_eq!(leader.primary_canonical(), "alt+x");
    }

    #[test]
    fn hint_config_resolves_default_on_and_explicit_off() {
        // CTX-0735 (#981): no layer declaration resolves default-on; an
        // explicit `false` disables arming; `true` re-enables.
        assert_eq!(
            resolve_hint_config(&EffectiveConfig::default()),
            HintConfig::default_on(),
            "absent hints_enabled stays default-on"
        );
        let disabled = EffectiveConfig {
            hints_enabled: Some(false),
            ..Default::default()
        };
        assert_eq!(
            resolve_hint_config(&disabled),
            HintConfig { enabled: false },
            "explicit false disables"
        );
        let enabled = EffectiveConfig {
            hints_enabled: Some(true),
            ..Default::default()
        };
        assert_eq!(
            resolve_hint_config(&enabled),
            HintConfig { enabled: true },
            "explicit true enables"
        );
    }

    #[test]
    fn leader_timeout_fail_open_and_esc_cancel() {
        // OQ-088 timeout/cancel semantics: expiry returns to Idle
        // (fail-open: keys route back to the shell) and `Esc` consumes only
        // while armed.
        let mut state = LeaderState::Idle;
        assert!(!state.is_armed());
        assert_eq!(state.poll(0), LeaderPoll::Idle, "idle poll stays idle");
        assert!(!state.cancel(), "idle Esc keeps its normal owner");

        state.arm(10_000, LEADER_TIMEOUT_MS_DEFAULT);
        assert!(state.is_armed());
        assert_eq!(state.poll(10_000), LeaderPoll::Armed, "press instant armed");
        assert_eq!(
            state.poll(10_000 + LEADER_TIMEOUT_MS_DEFAULT - 1),
            LeaderPoll::Armed,
            "last ms still armed"
        );
        assert_eq!(
            state.poll(10_000 + LEADER_TIMEOUT_MS_DEFAULT),
            LeaderPoll::Expired,
            "deadline expires fail-open"
        );
        assert!(!state.is_armed(), "expiry returns to idle");
        assert_eq!(state.poll(u64::MAX), LeaderPoll::Idle, "post-expiry idle");

        // `Esc` cancel consumes while armed, then releases.
        state.arm(0, 500);
        assert!(state.cancel(), "armed Esc is consumed by the cancel");
        assert!(!state.is_armed());
        assert_eq!(state.poll(60_000), LeaderPoll::Idle);

        // Re-arm replaces the pending window (no stacked presses).
        state.arm(100, 1000);
        state.arm(200, 1000);
        assert_eq!(state.poll(1100), LeaderPoll::Armed, "second arm wins");
        assert_eq!(state.poll(1200), LeaderPoll::Expired);
    }

    #[test]
    fn leader_bad_timeout_fails_closed() {
        for bad in [
            0,
            1,
            LEADER_TIMEOUT_MS_MIN - 1,
            LEADER_TIMEOUT_MS_MAX + 1,
            u64::MAX,
        ] {
            let err = validate_leader_timeout_ms(bad).unwrap_err();
            assert!(
                err.to_string().contains("leader_timeout_ms"),
                "must name field for {bad}: {err}"
            );
            assert!(
                resolve_leader(None, Some(bad), LeaderPlatform::Other).is_err(),
                "resolution rejects {bad}"
            );
        }
        for good in [
            LEADER_TIMEOUT_MS_MIN,
            LEADER_TIMEOUT_MS_DEFAULT,
            LEADER_TIMEOUT_MS_MAX,
        ] {
            validate_leader_timeout_ms(good).expect("boundary accepted");
        }
    }

    #[test]
    fn leader_is_independent_of_mod_flip() {
        // OQ-052 Mod unification stays deferred: the Leader never routes
        // through `ModKey` — a Super chrome map keeps the Alt+Space Leader.
        let flipped = EffectiveConfig {
            mod_key: ModKey::Super,
            ..Default::default()
        };
        let leader = resolve_leader_for(&flipped, LeaderPlatform::Other).expect("resolves");
        assert!(
            leader.arms(alt_space()),
            "leader stays alt+space under super"
        );
        // And an explicit leader keeps its exact spelling under Super too.
        let explicit = EffectiveConfig {
            mod_key: ModKey::Super,
            leader_key: Some(Chord::parse("alt+space").expect("parses")),
            ..Default::default()
        };
        let leader = resolve_leader_for(&explicit, LeaderPlatform::Other).expect("resolves");
        assert!(!leader.from_default);
        assert!(leader.arms(alt_space()));
    }

    #[test]
    fn leader_platform_host_matches_target() {
        // `host()` reflects the compile target (Windows CI covers the
        // Windows arm; Linux/macOS cover Other). Note: `default()` is
        // always `Other`, so it must NOT be equated with `host()` here.
        if cfg!(windows) {
            assert_eq!(LeaderPlatform::host(), LeaderPlatform::Windows);
        } else {
            assert_eq!(LeaderPlatform::host(), LeaderPlatform::Other);
        }
    }

    #[test]
    fn prefix_shape_detection_is_exact_two_tokens() {
        // CTX-1002: the `leader` keyword is symbolic (case-insensitive) and
        // takes exactly one follow-up token; anything else keeps the
        // single-chord path untouched.
        for raw in [
            "leader w",
            "Leader ctrl+b",
            "LEADER  escape",
            "  leader\tv  ",
        ] {
            assert!(is_prefix_entry_shape(raw), "prefix shape {raw:?}");
        }
        for raw in [
            "",
            "   ",
            "leader",
            "leaderw",
            "alt+h",
            "ctrl+b",
            "leader w v",
            "leader  w  v",
            "w leader",
        ] {
            assert!(!is_prefix_entry_shape(raw), "single shape {raw:?}");
        }
        assert_eq!(split_prefix_chord("leader w"), Some("w"));
        assert_eq!(split_prefix_chord("Leader ctrl+b"), Some("ctrl+b"));
        assert_eq!(split_prefix_chord("alt+h"), None);
        assert_eq!(split_prefix_chord("leader w v"), None);
    }

    #[test]
    fn prefix_second_accepts_bare_letters_and_chords() {
        // CTX-1002: bare follow-ups need no modifier (the Leader half
        // disambiguates); everything else follows the shared chord grammar.
        let w = parse_prefix_second("w").expect("bare w");
        assert_eq!(w.key, KeyName::Char('w'));
        assert!(!(w.ctrl || w.alt || w.shift || w.super_held));
        let upper = parse_prefix_second("V").expect("bare V folds");
        assert_eq!(upper.key, KeyName::Char('v'));
        let digit = parse_prefix_second("1").expect("bare digit");
        assert_eq!(digit.key, KeyName::Char('1'));
        let modified = parse_prefix_second("ctrl+w").expect("modified follow-up");
        assert!(modified.ctrl);
        assert_eq!(modified.key, KeyName::Char('w'));
        let named = parse_prefix_second("escape").expect("bare named");
        assert_eq!(named.key, KeyName::Escape);
        let tab = parse_prefix_second("ctrl+tab").expect("modified named");
        assert_eq!(tab.key, KeyName::Tab);
        for bad in ["", "   ", "hyper+q", "ctrl", "leader"] {
            assert!(
                parse_prefix_second(bad).is_err(),
                "follow-up must reject {bad:?}"
            );
        }
    }

    #[test]
    fn prefix_entry_resolves_and_matches_second() {
        // CTX-1002: `<Leader> w` dispatches deterministically; unrelated
        // follow-ups fall through (None = normal owner, never swallowed).
        let entry = KeymapEntry {
            chord: "leader w".into(),
            action: "new_split:right".into(),
            context: "global".into(),
        };
        validate_entry(&entry).expect("prefix entry valid");
        let parsed = parse_prefix_entry(&entry)
            .expect("prefix shape")
            .expect("parses");
        assert_eq!(parsed.second.key, KeyName::Char('w'));
        assert_eq!(parsed.action, ChromeAction::NewSplit(SplitDir::Right));
        assert!(parsed.matches(key_ref(KeyName::Char('w'), false, false, false)));
        assert!(!parsed.matches(key_ref(KeyName::Char('h'), false, false, false)));
        assert!(!parsed.matches(key_ref(KeyName::Char('w'), true, false, false)));

        // Single chords never parse as prefix entries.
        let single = KeymapEntry {
            chord: "alt+h".into(),
            action: "goto_split:left".into(),
            context: "global".into(),
        };
        assert!(parse_prefix_entry(&single).is_none());
        // Three-token sequences stay deferred: not a prefix shape, and not
        // a valid single chord either (fail-closed, never silently bound).
        let multi = KeymapEntry {
            chord: "leader w v".into(),
            action: "new_split:right".into(),
            context: "global".into(),
        };
        assert!(parse_prefix_entry(&multi).is_none());
        assert!(validate_entry(&multi).is_err());
    }

    #[test]
    fn prefix_resolve_skips_singles_and_overlays_duplicates() {
        // CTX-1002: single chords stay in `resolve_keymaps`, prefix
        // bindings resolve separately with last-wins overlay on the
        // `context + second` identity.
        let effective = EffectiveConfig {
            keymaps: vec![
                KeymapEntry {
                    chord: "alt+h".into(),
                    action: "goto_split:left".into(),
                    context: "global".into(),
                },
                KeymapEntry {
                    chord: "leader w".into(),
                    action: "new_split:right".into(),
                    context: "global".into(),
                },
                KeymapEntry {
                    chord: "leader w".into(),
                    action: "workspace_new".into(),
                    context: "global".into(),
                },
                KeymapEntry {
                    chord: "leader c".into(),
                    action: "workspace_new".into(),
                    context: "global".into(),
                },
            ],
            ..Default::default()
        };
        let singles = resolve_keymaps(&effective).expect("singles resolve");
        assert!(
            singles.iter().all(|m| m.chord.canonical() != "leader w"),
            "prefix entries never enter the single table"
        );
        assert_eq!(
            match_keymap(&singles, key_ref(KeyName::Char('h'), false, true, false)),
            Some(ChromeAction::GotoSplit(SplitDir::Left))
        );
        let prefixes = resolve_prefix_bindings(&effective.keymaps).expect("prefixes resolve");
        assert_eq!(
            prefixes.len(),
            2,
            "duplicate second overlays, distinct kept"
        );
        assert_eq!(
            match_prefix(&prefixes, key_ref(KeyName::Char('w'), false, false, false)),
            Some(ChromeAction::WorkspaceNew),
            "last duplicate wins"
        );
        assert_eq!(
            match_prefix(&prefixes, key_ref(KeyName::Char('c'), false, false, false)),
            Some(ChromeAction::WorkspaceNew)
        );
        assert_eq!(
            match_prefix(&prefixes, key_ref(KeyName::Char('z'), false, false, false)),
            None,
            "unmatched follow-up falls through"
        );
    }

    #[test]
    fn prefix_validate_entry_rejects_bad_followup_action_context() {
        // CTX-1002: fail-closed with a clear error, never a silent ignore.
        let bad_followup = KeymapEntry {
            chord: "leader hyper+q".into(),
            action: "workspace_new".into(),
            context: "global".into(),
        };
        assert!(validate_entry(&bad_followup).is_err());
        let bad_action = KeymapEntry {
            chord: "leader w".into(),
            action: "nope:unknown".into(),
            context: "global".into(),
        };
        assert!(validate_entry(&bad_action).is_err());
        let bad_context = KeymapEntry {
            chord: "leader w".into(),
            action: "workspace_new".into(),
            context: "overlay".into(),
        };
        assert!(validate_entry(&bad_context).is_err());
    }

    #[test]
    fn alt_u_i_page_scroll_order_fixed() {
        // Issue #1437: alt+u/alt+i were swapped; corrected so alt+u scrolls
        // down (toward live, less-like `d` behavior) and alt+i scrolls up
        // (into history, less-like `u` behavior reversed from the key name).
        let mk_effective = |mod_key| EffectiveConfig {
            mod_key,
            ..Default::default()
        };
        let maps = resolve_keymaps(&mk_effective(ModKey::Alt)).expect("resolves");

        let u_chord = Chord::parse("alt+u").expect("alt+u parses");
        let i_chord = Chord::parse("alt+i").expect("alt+i parses");

        let u_key = KeyRef {
            key: u_chord.key,
            ctrl: u_chord.ctrl,
            alt: u_chord.alt,
            shift: u_chord.shift,
            super_held: u_chord.super_held,
        };
        let i_key = KeyRef {
            key: i_chord.key,
            ctrl: i_chord.ctrl,
            alt: i_chord.alt,
            shift: i_chord.shift,
            super_held: i_chord.super_held,
        };

        assert_eq!(
            match_keymap(&maps, u_key),
            Some(ChromeAction::ScrollPageDown),
            "alt+u scrolls down (toward live)"
        );
        assert_eq!(
            match_keymap(&maps, i_key),
            Some(ChromeAction::ScrollPageUp),
            "alt+i scrolls up (into history)"
        );
    }

    #[test]
    fn default_mod_q_and_panel_aliases_resolve_and_rebind() {
        // Issue #1776: Mod+q closes pane/view by default under both Alt and Super mods.
        let alt_maps = default_keymaps_with_mod(ModKey::Alt).expect("alt defaults valid");
        let super_maps = default_keymaps_with_mod(ModKey::Super).expect("super defaults valid");

        let q_alt = KeyRef {
            key: KeyName::Char('q'),
            ctrl: false,
            alt: true,
            shift: false,
            super_held: false,
        };
        let q_super = KeyRef {
            key: KeyName::Char('q'),
            ctrl: false,
            alt: false,
            shift: false,
            super_held: true,
        };
        assert_eq!(
            match_keymap(&alt_maps, q_alt),
            Some(ChromeAction::CloseView),
            "alt+q closes pane under Alt mod"
        );
        assert_eq!(
            match_keymap(&super_maps, q_super),
            Some(ChromeAction::CloseView),
            "super+q closes pane under Super mod"
        );

        // Mod+z toggles zoom / suspends panel under both Alt and Super mods.
        let z_alt = KeyRef {
            key: KeyName::Char('z'),
            ctrl: false,
            alt: true,
            shift: false,
            super_held: false,
        };
        let z_super = KeyRef {
            key: KeyName::Char('z'),
            ctrl: false,
            alt: false,
            shift: false,
            super_held: true,
        };
        assert_eq!(
            match_keymap(&alt_maps, z_alt),
            Some(ChromeAction::ToggleZoom),
            "alt+z toggles zoom / suspends panel under Alt mod"
        );
        assert_eq!(
            match_keymap(&super_maps, z_super),
            Some(ChromeAction::ToggleZoom),
            "super+z toggles zoom / suspends panel under Super mod"
        );

        // Action alias parsing:
        for alias in [
            "close_view",
            "close_surface",
            "close_panel",
            "exit_panel",
            "close_focused_panel",
        ] {
            assert_eq!(
                ChromeAction::parse(alias).expect("parses"),
                ChromeAction::CloseView,
                "alias {alias} maps to CloseView"
            );
        }

        for alias in [
            "toggle_zoom",
            "toggle_split_zoom",
            "suspend_panel",
            "detach_panel",
            "suspend_focused_panel",
            "detach_focused_panel",
        ] {
            assert_eq!(
                ChromeAction::parse(alias).expect("parses"),
                ChromeAction::ToggleZoom,
                "alias {alias} maps to ToggleZoom"
            );
        }

        // Custom user rebinding via EffectiveConfig keymaps:
        let user_cfg = EffectiveConfig {
            keymaps: vec![
                KeymapEntry {
                    chord: "alt+q".into(),
                    action: "toggle_help".into(),
                    context: "global".into(),
                },
                KeymapEntry {
                    chord: "alt+z".into(),
                    action: "new_panel".into(),
                    context: "global".into(),
                },
            ],
            ..Default::default()
        };
        let custom_maps = resolve_keymaps(&user_cfg).expect("resolves custom keymaps");
        assert_eq!(
            match_keymap(&custom_maps, q_alt),
            Some(ChromeAction::ToggleHelp),
            "user rebind of alt+q overrides default CloseView"
        );
        assert_eq!(
            match_keymap(&custom_maps, z_alt),
            Some(ChromeAction::NewPanel),
            "user rebind of alt+z overrides default ToggleZoom"
        );
    }
}
