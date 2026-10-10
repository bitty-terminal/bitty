//! Declarative panel spawn rules (CTX-1080, issue 1756).
//!
//! Hyprland `windowrulev2`-like rules evaluated at PTY spawn and view
//! creation. Matchers read only already-available spawn metadata (program
//! plus argv joined as a command line, current title text, content kind).
//! No new process or environment authority is introduced.
//!
//! Shape (wezterm-style `init.lua`):
//! ```lua
//! return {
//!   panel_rules = {
//!     { cmd = "btop", presentation = "floating", width = 100, height = 30, centered = true },
//!     { cmd_regex = "^tail -f", workspace = 3 },
//!     { title_regex = ".*nvim.*", presentation = "tiled" },
//!   },
//! }
//! ```
//!
//! Semantics:
//! - `panel_rules` is a fully-optional top-level array; absent or empty
//!   means this layer says nothing.
//! - At most [`MAX_PANEL_RULES`] entries; more fails closed.
//! - Each rule needs at least one matcher (`cmd`, `cmd_regex`,
//!   `title_regex`, `content`) and at least one action (`presentation`,
//!   `width`, `height`, `workspace`, `centered`).
//! - Matchers combine with AND; all present matchers must match.
//! - First match wins in array order; evaluation is deterministic.
//! - `cmd` is an exact string matched against the program basename or the
//!   full command line. `cmd_regex` and `title_regex` use the bounded
//!   subset documented on [`validate_regex_pattern`]; anything outside the
//!   subset fails validation.
//! - Invalid rules fail closed at config validation with a
//!   `panel_rules[<n>].<field>` path, matching the `keymaps`/`plugins`
//!   precedent. At match time an invalid pattern never matches (rule
//!   skipped), never match-all.

#![forbid(unsafe_code)]

use crate::error::ConfigError;

/// Hard cap on `panel_rules` entries per layer.
///
/// Rejected with a validation error, never silently pruned: pruning would
/// present a partial rule set as complete.
pub const MAX_PANEL_RULES: usize = 64;

/// Maximum bytes for one exact or regex matcher string.
///
/// Bounds untrusted input; longer values fail closed.
pub const MAX_PANEL_RULE_PATTERN_BYTES: usize = 256;

/// Maximum grid dimension for rule-requested `width`/`height`.
///
/// Mirrors `bitty-term-state::MAX_GRID_DIM` by value so a rule can never
/// request a grid the terminal cannot represent.
pub const MAX_PANEL_RULE_DIM: u16 = 1000;

/// Maximum workspace label for rule-requested `workspace`.
///
/// Mirrors `MAX_WORKSPACE_LABEL` by value (`1..=16`).
pub const MAX_PANEL_RULE_WORKSPACE: u8 = 16;

/// Step budget for one regex search (ReDoS guard, threat T-01).
///
/// The matcher is cold-path only (spawn/title evaluation); exceeding the
/// budget returns no-match fail-closed.
const REGEX_STEP_BUDGET: usize = 10_000;

/// Presentation action for a matched rule.
///
/// Closed set: `tiled`, `floating`, `scratchpad`. `fullscreen` is rejected
/// fail-closed here; it has its own zoom-like path and is out of scope for
/// spawn rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelPresentation {
    /// Normal tiled leaf.
    Tiled,
    /// Floating overlay leaf.
    Floating,
    /// Hidden-capable scratchpad leaf.
    Scratchpad,
}

impl PanelPresentation {
    /// Parses a canonical lowercase name. Case-sensitive; rejects empty,
    /// whitespace-padded, and unknown inputs with no aliasing.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "tiled" => Some(Self::Tiled),
            "floating" => Some(Self::Floating),
            "scratchpad" => Some(Self::Scratchpad),
            _ => None,
        }
    }

    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tiled => "tiled",
            Self::Floating => "floating",
            Self::Scratchpad => "scratchpad",
        }
    }
}

impl std::fmt::Display for PanelPresentation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One declarative panel spawn rule: matchers plus actions.
///
/// All fields are owned; `None` means the matcher or action is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelSpawnRule {
    /// Exact command matcher (program basename or full command line).
    pub cmd: Option<String>,
    /// Regex matcher against the full command line.
    pub cmd_regex: Option<String>,
    /// Regex matcher against the title text.
    pub title_regex: Option<String>,
    /// Content-kind matcher (`empty`, `terminal`, `rich`, `browser`).
    pub content: Option<String>,
    /// Requested presentation mode.
    pub presentation: Option<PanelPresentation>,
    /// Requested width in cells (`1..=1000`).
    ///
    /// Durable fixed-size constraint (CTX-1088): sizes the PTY and session
    /// grid at spawn and stamps the per-view flag so solver sync keeps
    /// them there until cleared (clearing returns to solver ownership).
    /// Paint stays slot-sized (centered when the flag fits, clipped to
    /// the slot window when larger). Fixed wins over pseudo-tiling on
    /// conflict; rule edits affect future spawns only. Degenerate or
    /// oversize values fail closed at validation and never stamp.
    pub width: Option<u16>,
    /// Requested height in cells (`1..=1000`).
    ///
    /// Same durable contract as [`Self::width`].
    pub height: Option<u16>,
    /// Requested workspace label (`1..=16`).
    pub workspace: Option<u8>,
    /// Whether a floating panel requests centering.
    pub centered: Option<bool>,
}

impl PanelSpawnRule {
    /// Validates one rule fail-closed with a `panel_rules[<n>]` path prefix.
    ///
    /// `index_one_based` is the Lua 1-based position used in diagnostics.
    pub fn validate(&self, index_one_based: usize) -> Result<(), ConfigError> {
        let base = format!("panel_rules[{index_one_based}]");
        let has_matcher = self.cmd.is_some()
            || self.cmd_regex.is_some()
            || self.title_regex.is_some()
            || self.content.is_some();
        if !has_matcher {
            return Err(ConfigError::validation(
                base,
                "needs at least one matcher: cmd, cmd_regex, title_regex, or content",
            ));
        }
        let has_action = self.presentation.is_some()
            || self.width.is_some()
            || self.height.is_some()
            || self.workspace.is_some()
            || self.centered.is_some();
        if !has_action {
            return Err(ConfigError::validation(
                base,
                "needs at least one action: presentation, width, height, workspace, or centered",
            ));
        }
        if let Some(cmd) = &self.cmd {
            validate_cmd_exact(&format!("{base}.cmd"), cmd)?;
        }
        if let Some(pat) = &self.cmd_regex {
            validate_regex_pattern(&format!("{base}.cmd_regex"), pat)?;
        }
        if let Some(pat) = &self.title_regex {
            validate_regex_pattern(&format!("{base}.title_regex"), pat)?;
        }
        if let Some(content) = &self.content {
            if crate::types::ViewContent::parse(content).is_none() {
                return Err(ConfigError::validation(
                    format!("{base}.content"),
                    "must be one of empty, terminal, rich, browser",
                ));
            }
        }
        if let Some(w) = self.width {
            if w == 0 || w > MAX_PANEL_RULE_DIM {
                return Err(ConfigError::validation(
                    format!("{base}.width"),
                    format!("must be within [1, {MAX_PANEL_RULE_DIM}]"),
                ));
            }
        }
        if let Some(h) = self.height {
            if h == 0 || h > MAX_PANEL_RULE_DIM {
                return Err(ConfigError::validation(
                    format!("{base}.height"),
                    format!("must be within [1, {MAX_PANEL_RULE_DIM}]"),
                ));
            }
        }
        if let Some(ws) = self.workspace {
            if ws == 0 || ws > MAX_PANEL_RULE_WORKSPACE {
                return Err(ConfigError::validation(
                    format!("{base}.workspace"),
                    format!("must be within [1, {MAX_PANEL_RULE_WORKSPACE}]"),
                ));
            }
        }
        Ok(())
    }

    /// Whether this rule matches the given spawn metadata.
    ///
    /// All present matchers must match (AND). An absent matcher set never
    /// matches. An invalid regex at match time never matches (fail-closed
    /// skip), never match-all, never panics.
    #[must_use]
    pub fn matches(&self, cmd_line: &str, title: &str, content: &str) -> bool {
        if self.cmd.is_none()
            && self.cmd_regex.is_none()
            && self.title_regex.is_none()
            && self.content.is_none()
        {
            return false;
        }
        if let Some(cmd) = &self.cmd {
            if !matches_cmd_exact(cmd, cmd_line) {
                return false;
            }
        }
        if let Some(pat) = &self.cmd_regex {
            if !matches_regex(pat, cmd_line) {
                return false;
            }
        }
        if let Some(pat) = &self.title_regex {
            if !matches_regex(pat, title) {
                return false;
            }
        }
        if let Some(want) = &self.content {
            if want != content {
                return false;
            }
        }
        true
    }
}

/// Validates an exact `cmd` matcher: non-empty, bounded, no NUL.
fn validate_cmd_exact(field: &str, cmd: &str) -> Result<(), ConfigError> {
    if cmd.trim().is_empty() {
        return Err(ConfigError::validation(field, "must not be empty"));
    }
    if cmd.len() > MAX_PANEL_RULE_PATTERN_BYTES {
        return Err(ConfigError::validation(
            field,
            format!("must be <= {MAX_PANEL_RULE_PATTERN_BYTES} bytes"),
        ));
    }
    if cmd.contains('\0') {
        return Err(ConfigError::validation(field, "must not contain NUL bytes"));
    }
    Ok(())
}

/// Validates a regex matcher in the bounded subset.
///
/// Accepted subset: literals, `.` (any one char), `*` (zero or more of the
/// previous atom), `+` (one or more), `?` (zero or one), `\\` escapes (next
/// char literal), `[...]` classes with optional `^` negation and `a-z`
/// ranges, `^` only as the first char (start anchor), `$` only as the last
/// char (end anchor). `(`, `)`, `{`, `}`, `|` are unsupported and rejected.
/// A trailing `\\`, a leading or dangling quantifier, an empty or
/// unterminated class, or a misplaced anchor is invalid.
pub fn validate_regex_pattern(field: &str, pat: &str) -> Result<(), ConfigError> {
    if pat.is_empty() {
        return Err(ConfigError::validation(field, "must not be empty"));
    }
    if pat.len() > MAX_PANEL_RULE_PATTERN_BYTES {
        return Err(ConfigError::validation(
            field,
            format!("must be <= {MAX_PANEL_RULE_PATTERN_BYTES} bytes"),
        ));
    }
    if pat.contains('\0') {
        return Err(ConfigError::validation(field, "must not contain NUL bytes"));
    }
    if let Err(reason) = check_regex_syntax(pat) {
        return Err(ConfigError::validation(field, reason));
    }
    Ok(())
}

/// Checks regex syntax for the bounded subset; `Err` carries a short reason.
fn check_regex_syntax(pat: &str) -> Result<(), String> {
    let chars: Vec<char> = pat.chars().collect();
    if chars.is_empty() {
        return Err("must not be empty".to_string());
    }
    let mut idx = 0usize;
    let mut have_atom = false;
    while idx < chars.len() {
        let ch = chars[idx];
        match ch {
            '^' => {
                if idx != 0 {
                    return Err("misplaced start anchor".to_string());
                }
                idx += 1;
            }
            '$' => {
                if idx + 1 != chars.len() {
                    return Err("misplaced end anchor".to_string());
                }
                idx += 1;
            }
            '*' | '+' | '?' => {
                if !have_atom {
                    return Err("dangling quantifier".to_string());
                }
                have_atom = false;
                idx += 1;
            }
            '\\' => {
                if idx + 1 >= chars.len() {
                    return Err("trailing escape".to_string());
                }
                have_atom = true;
                idx += 2;
            }
            '[' => {
                let mut end = idx + 1;
                if end < chars.len() && chars[end] == '^' {
                    end += 1;
                }
                if end < chars.len() && chars[end] == ']' {
                    end += 1;
                }
                let mut closed = false;
                let mut j = end;
                while j < chars.len() {
                    if chars[j] == '\\' {
                        j += 2;
                        continue;
                    }
                    if chars[j] == ']' {
                        closed = true;
                        break;
                    }
                    j += 1;
                }
                if !closed {
                    return Err("unterminated character class".to_string());
                }
                if j == end {
                    return Err("empty character class".to_string());
                }
                have_atom = true;
                idx = j + 1;
            }
            ']' => return Err("unmatched class close".to_string()),
            '(' | ')' | '{' | '}' | '|' => {
                return Err("unsupported regex construct".to_string());
            }
            _ => {
                have_atom = true;
                idx += 1;
            }
        }
    }
    Ok(())
}

/// Exact `cmd` matching: equals the full command line or the program
/// basename (text after the final `/`).
fn matches_cmd_exact(pattern: &str, cmd_line: &str) -> bool {
    if pattern == cmd_line {
        return true;
    }
    let program = cmd_line.split_whitespace().next().unwrap_or(cmd_line);
    let basename = program.rsplit('/').next().unwrap_or(program);
    pattern == program || pattern == basename
}

/// Regex search with `^`/`$` anchors in the bounded subset.
///
/// Unanchored patterns match anywhere; `^`-prefixed patterns match only at
/// the start; `$`-suffixed patterns must reach the end. Invalid patterns
/// return false. Execution is bounded by [`REGEX_STEP_BUDGET`].
#[must_use]
pub fn matches_regex(pattern: &str, text: &str) -> bool {
    if check_regex_syntax(pattern).is_err() {
        return false;
    }
    let (anchored_start, anchored_end, body) = split_anchors(pattern);
    let atoms = match parse_atoms(body) {
        Some(atoms) => atoms,
        None => return false,
    };
    let text_chars: Vec<char> = text.chars().collect();
    let mut budget = REGEX_STEP_BUDGET;
    if anchored_start {
        return match_from(&atoms, &text_chars, 0, anchored_end, &mut budget);
    }
    for start in 0..=text_chars.len() {
        if match_from(&atoms, &text_chars, start, anchored_end, &mut budget) {
            return true;
        }
        if budget == 0 {
            return false;
        }
    }
    false
}

/// Splits `^`/`$` anchors; returns (start, end, body).
fn split_anchors(pattern: &str) -> (bool, bool, &str) {
    let mut body = pattern;
    let mut start = false;
    let mut end = false;
    if let Some(rest) = body.strip_prefix('^') {
        start = true;
        body = rest;
    }
    if let Some(rest) = body.strip_suffix('$') {
        if !rest.ends_with('\\') {
            end = true;
            body = rest;
        }
    }
    (start, end, body)
}

/// One parsed atom plus its quantifier.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Atom {
    Literal(char),
    Any,
    Class {
        negate: bool,
        ranges: Vec<(char, char)>,
        singles: Vec<char>,
    },
}

/// Quantifier for one atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quant {
    One,
    ZeroOrOne,
    ZeroOrMore,
    OneOrMore,
}

/// Parses the body (anchors stripped) into atoms; `None` on bad syntax.
fn parse_atoms(body: &str) -> Option<Vec<(Atom, Quant)>> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut idx = 0usize;
    while idx < chars.len() {
        let atom = match chars[idx] {
            '\\' => {
                if idx + 1 >= chars.len() {
                    return None;
                }
                Atom::Literal(chars[idx + 1])
            }
            '.' => Atom::Any,
            '[' => {
                let (atom, next) = parse_class(&chars, idx)?;
                idx = next;
                let quant = peek_quant(&chars, idx);
                if quant != Quant::One {
                    idx += 1;
                }
                out.push((atom, quant));
                continue;
            }
            '(' | ')' | '{' | '}' | '|' | ']' | '*' | '+' | '?' | '^' | '$' => return None,
            ch => Atom::Literal(ch),
        };
        let consumed = if chars[idx] == '\\' { 2 } else { 1 };
        idx += consumed;
        let quant = peek_quant(&chars, idx);
        if quant != Quant::One {
            idx += 1;
        }
        out.push((atom, quant));
    }
    Some(out)
}

/// Reads a quantifier at `idx` without consuming on `One`.
fn peek_quant(chars: &[char], idx: usize) -> Quant {
    if idx >= chars.len() {
        return Quant::One;
    }
    match chars[idx] {
        '*' => Quant::ZeroOrMore,
        '+' => Quant::OneOrMore,
        '?' => Quant::ZeroOrOne,
        _ => Quant::One,
    }
}

/// Parses a `[...]` class starting at `start` (which holds `[`).
fn parse_class(chars: &[char], start: usize) -> Option<(Atom, usize)> {
    let mut idx = start + 1;
    let mut negate = false;
    if idx < chars.len() && chars[idx] == '^' {
        negate = true;
        idx += 1;
    }
    let mut singles = Vec::new();
    let mut ranges = Vec::new();
    if idx < chars.len() && chars[idx] == ']' {
        singles.push(']');
        idx += 1;
    }
    let mut first = true;
    while idx < chars.len() {
        if chars[idx] == ']' && !first {
            idx += 1;
            return Some((
                Atom::Class {
                    negate,
                    ranges,
                    singles,
                },
                idx,
            ));
        }
        let lo = if chars[idx] == '\\' {
            if idx + 1 >= chars.len() {
                return None;
            }
            idx += 2;
            chars[idx - 1]
        } else {
            let ch = chars[idx];
            idx += 1;
            ch
        };
        if idx + 1 < chars.len() && chars[idx] == '-' && chars[idx + 1] != ']' {
            idx += 1;
            let hi = if chars[idx] == '\\' {
                if idx + 1 >= chars.len() {
                    return None;
                }
                idx += 2;
                chars[idx - 1]
            } else {
                let ch = chars[idx];
                idx += 1;
                ch
            };
            if lo > hi {
                return None;
            }
            ranges.push((lo, hi));
        } else {
            singles.push(lo);
        }
        first = false;
    }
    None
}

/// Whether `atom` matches `ch`.
fn atom_matches(atom: &Atom, ch: char) -> bool {
    match atom {
        Atom::Literal(want) => *want == ch,
        Atom::Any => true,
        Atom::Class {
            negate,
            ranges,
            singles,
        } => {
            let mut hit = singles.contains(&ch);
            if !hit {
                for (lo, hi) in ranges {
                    if (*lo..=*hi).contains(&ch) {
                        hit = true;
                        break;
                    }
                }
            }
            if *negate { !hit } else { hit }
        }
    }
}

/// Matches `atoms` at `pos`; when `anchored_end`, the match must reach the
/// end of `text`. Bounded by `budget`.
fn match_from(
    atoms: &[(Atom, Quant)],
    text: &[char],
    pos: usize,
    anchored_end: bool,
    budget: &mut usize,
) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match_atoms(atoms, text, pos, anchored_end, budget)
}

/// Recursive atom matcher with budget.
fn match_atoms(
    atoms: &[(Atom, Quant)],
    text: &[char],
    pos: usize,
    anchored_end: bool,
    budget: &mut usize,
) -> bool {
    if atoms.is_empty() {
        return if anchored_end {
            pos == text.len()
        } else {
            true
        };
    }
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    let (atom, quant) = &atoms[0];
    let rest = &atoms[1..];
    match quant {
        Quant::One => {
            if pos < text.len() && atom_matches(atom, text[pos]) {
                match_atoms(rest, text, pos + 1, anchored_end, budget)
            } else {
                false
            }
        }
        Quant::ZeroOrOne => {
            if match_atoms(rest, text, pos, anchored_end, budget) {
                return true;
            }
            if pos < text.len() && atom_matches(atom, text[pos]) {
                match_atoms(rest, text, pos + 1, anchored_end, budget)
            } else {
                false
            }
        }
        Quant::ZeroOrMore => {
            let mut count = 0usize;
            while pos + count < text.len() && atom_matches(atom, text[pos + count]) {
                count += 1;
                if count > text.len() + 1 {
                    break;
                }
            }
            let mut n = count;
            loop {
                if match_atoms(rest, text, pos + n, anchored_end, budget) {
                    return true;
                }
                if n == 0 {
                    return false;
                }
                n -= 1;
                if *budget == 0 {
                    return false;
                }
                *budget -= 1;
            }
        }
        Quant::OneOrMore => {
            if pos >= text.len() || !atom_matches(atom, text[pos]) {
                return false;
            }
            let mut count = 1usize;
            while pos + count < text.len() && atom_matches(atom, text[pos + count]) {
                count += 1;
            }
            let mut n = count;
            loop {
                if match_atoms(rest, text, pos + n, anchored_end, budget) {
                    return true;
                }
                if n <= 1 {
                    return false;
                }
                n -= 1;
                if *budget == 0 {
                    return false;
                }
                *budget -= 1;
            }
        }
    }
}

/// Builds the command line joined from `program` plus `args`.
///
/// No shell interpolation; verbatim join with single spaces.
#[must_use]
pub fn command_line_for(program: &str, args: &[&str]) -> String {
    let mut out = program.to_string();
    for arg in args {
        out.push(' ');
        out.push_str(arg);
    }
    out
}

/// First-match-wins lookup over `rules` in array order.
///
/// Returns the index and rule of the first rule matching the metadata, or
/// `None` when nothing matches.
#[must_use]
pub fn find_match<'a>(
    rules: &'a [PanelSpawnRule],
    cmd_line: &str,
    title: &str,
    content: &str,
) -> Option<(usize, &'a PanelSpawnRule)> {
    for (idx, rule) in rules.iter().enumerate() {
        if rule.matches(cmd_line, title, content) {
            return Some((idx, rule));
        }
    }
    None
}

/// Validates a whole `panel_rules` array fail-closed.
pub fn validate_all(rules: &[PanelSpawnRule]) -> Result<(), ConfigError> {
    if rules.len() > MAX_PANEL_RULES {
        return Err(ConfigError::validation(
            "panel_rules",
            format!("must contain at most {MAX_PANEL_RULES} entries"),
        ));
    }
    for (idx, rule) in rules.iter().enumerate() {
        rule.validate(idx + 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule_cmd(cmd: &str) -> PanelSpawnRule {
        PanelSpawnRule {
            cmd: Some(cmd.to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: Some(PanelPresentation::Floating),
            width: None,
            height: None,
            workspace: None,
            centered: None,
        }
    }

    #[test]
    fn exact_matches_basename_and_full_line() {
        let rule = rule_cmd("btop");
        assert!(rule.matches("btop", "", "terminal"));
        assert!(rule.matches("/usr/bin/btop", "", "terminal"));
        assert!(rule.matches("btop --theme dark", "", "terminal"));
        assert!(!rule.matches("btopx", "", "terminal"));
        assert!(!rule.matches("htop", "", "terminal"));
    }

    #[test]
    fn regex_prefix_matches() {
        let rule = PanelSpawnRule {
            cmd: None,
            cmd_regex: Some("^tail -f".to_string()),
            title_regex: None,
            content: None,
            presentation: None,
            width: None,
            height: None,
            workspace: Some(3),
            centered: None,
        };
        assert!(rule.matches("tail -f /var/log/syslog", "", "terminal"));
        assert!(!rule.matches("sudo tail -f /var/log/syslog", "", "terminal"));
    }

    #[test]
    fn title_regex_matches() {
        assert!(matches_regex(".*nvim.*", "nvim - README.md"));
        assert!(!matches_regex(".*nvim.*", "htop"));
        assert!(matches_regex("^editor$", "editor"));
        assert!(!matches_regex("^editor$", "my editor "));
    }

    #[test]
    fn invalid_regex_never_matches() {
        assert!(!matches_regex("([", "(["));
        assert!(!matches_regex("a(", "a("));
        assert!(!matches_regex("", "anything"));
        let rule = PanelSpawnRule {
            cmd: None,
            cmd_regex: Some("([".to_string()),
            title_regex: None,
            content: None,
            presentation: None,
            width: None,
            height: None,
            workspace: Some(2),
            centered: None,
        };
        assert!(!rule.matches("([", "", "terminal"));
    }

    #[test]
    fn empty_matcher_never_matches() {
        let rule = PanelSpawnRule {
            cmd: None,
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: Some(PanelPresentation::Tiled),
            width: None,
            height: None,
            workspace: None,
            centered: None,
        };
        assert!(!rule.matches("btop", "btop", "terminal"));
    }

    #[test]
    fn and_semantics_require_all_matchers() {
        let rule = PanelSpawnRule {
            cmd: Some("btop".to_string()),
            cmd_regex: None,
            title_regex: Some(".*mon.*".to_string()),
            content: None,
            presentation: Some(PanelPresentation::Floating),
            width: None,
            height: None,
            workspace: None,
            centered: None,
        };
        assert!(rule.matches("btop", "system monitor", "terminal"));
        assert!(!rule.matches("btop", "editor", "terminal"));
        assert!(!rule.matches("htop", "system monitor", "terminal"));
    }

    #[test]
    fn first_match_wins() {
        let first = PanelSpawnRule {
            cmd: Some("btop".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: Some(PanelPresentation::Floating),
            width: None,
            height: None,
            workspace: None,
            centered: None,
        };
        let second = PanelSpawnRule {
            cmd: Some("btop".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: Some(PanelPresentation::Tiled),
            width: None,
            height: None,
            workspace: None,
            centered: None,
        };
        let rules = vec![first, second];
        let (idx, hit) = find_match(&rules, "btop", "", "terminal").expect("match");
        assert_eq!(idx, 0);
        assert_eq!(hit.presentation, Some(PanelPresentation::Floating));
    }

    #[test]
    fn validation_needs_matcher_and_action() {
        let no_matcher = PanelSpawnRule {
            cmd: None,
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: Some(PanelPresentation::Tiled),
            width: None,
            height: None,
            workspace: None,
            centered: None,
        };
        assert!(no_matcher.validate(1).is_err());
        let no_action = PanelSpawnRule {
            cmd: Some("btop".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: None,
            width: None,
            height: None,
            workspace: None,
            centered: None,
        };
        assert!(no_action.validate(1).is_err());
    }

    #[test]
    fn validation_rejects_bad_dims_and_workspace() {
        let mut rule = rule_cmd("btop");
        rule.width = Some(0);
        assert!(rule.validate(1).is_err());
        rule.width = Some(1001);
        assert!(rule.validate(1).is_err());
        rule.width = None;
        rule.workspace = Some(0);
        assert!(rule.validate(1).is_err());
        rule.workspace = Some(17);
        assert!(rule.validate(1).is_err());
    }

    #[test]
    fn presentation_parse_closed() {
        assert_eq!(
            PanelPresentation::parse("floating"),
            Some(PanelPresentation::Floating)
        );
        assert_eq!(PanelPresentation::parse("fullscreen"), None);
        assert_eq!(PanelPresentation::parse("Floating"), None);
        assert_eq!(PanelPresentation::parse(""), None);
    }
}
